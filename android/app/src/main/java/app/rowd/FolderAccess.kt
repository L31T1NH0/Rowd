package app.rowd

import android.content.Context
import android.net.Uri
import androidx.documentfile.provider.DocumentFile
import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream
import java.io.InputStream
import java.security.MessageDigest
import java.util.UUID

/** SAF boundary. All calls run on the single sync worker, never on the UI thread. */
class FolderAccess(private val context: Context, private val treeUri: Uri) {
    private val resolver = context.contentResolver
    private val recovery = File(context.filesDir, "recovery").apply { mkdirs() }
    private val root get() = DocumentFile.fromTreeUri(context, treeUri)
        ?: error("A pasta não está disponível. Confira a permissão de acesso.")

    fun tempDirectory(): String = context.cacheDir.absolutePath

    private fun digest(input: InputStream): Pair<String, Long> {
        val md = MessageDigest.getInstance("SHA-256")
        var size = 0L
        val buffer = ByteArray(64 * 1024)
        input.use { stream ->
            while (true) {
                val count = stream.read(buffer)
                if (count < 0) break
                md.update(buffer, 0, count)
                size += count
                check(size <= 8L * 1024 * 1024 * 1024) { "Arquivo maior que 8 GiB." }
            }
        }
        return md.digest().joinToString("") { "%02x".format(it.toInt() and 255) } to size
    }

    private fun hash(document: DocumentFile): String = digest(
        resolver.openInputStream(document.uri) ?: error("Não foi possível ler ${document.name}")
    ).first

    private fun parts(path: String): List<String> {
        val segments = path.split('/')
        require(path.isNotEmpty() && path.length <= 2048 && segments.all {
            it.isNotEmpty() && it != "." && it != ".." && !it.contains('\\') && !it.contains('\u0000')
        }) { "Caminho inválido." }
        return segments
    }

    private fun find(path: String): DocumentFile? {
        var doc = root
        for (part in parts(path)) doc = doc.findFile(part) ?: return null
        check(doc.isFile) { "O caminho já é uma pasta: $path" }
        return doc
    }

    private fun parent(path: String): Pair<DocumentFile, String> {
        val parts = parts(path)
        var doc = root
        for (part in parts.dropLast(1)) {
            doc = doc.findFile(part) ?: doc.createDirectory(part) ?: error("Não foi possível criar $part")
            check(doc.isDirectory) { "Um arquivo ocupa o lugar da pasta $part" }
        }
        return doc to parts.last()
    }

    private fun persist(file: File, json: JSONObject) {
        val temp = File(file.parentFile, file.name + ".tmp")
        FileOutputStream(temp).use { it.write(json.toString().toByteArray()); it.fd.sync() }
        check(temp.renameTo(file)) { "Não foi possível salvar o registro de recuperação." }
    }

    private fun copyToPrivate(document: DocumentFile, file: File) {
        val input = resolver.openInputStream(document.uri) ?: error("Não foi possível abrir ${document.name}")
        input.use { source -> FileOutputStream(file).use { out -> source.copyTo(out); out.fd.sync() } }
    }

    private fun writeDocument(file: File, document: DocumentFile) {
        val descriptor = resolver.openFileDescriptor(document.uri, "rwt")
            ?: error("O provedor não permite substituir este arquivo.")
        android.os.ParcelFileDescriptor.AutoCloseOutputStream(descriptor).use { out ->
            file.inputStream().use { it.copyTo(out) }; out.fd.sync()
        }
    }

    /** Recover only unambiguous outcomes. Never overwrite an unknown user edit on restart. */
    private fun recoverPending() {
        recovery.listFiles { f -> f.extension == "json" }?.forEach { file ->
            val journal = JSONObject(file.readText())
            if (journal.optString("tree") != treeUri.toString() || journal.optBoolean("finished")) return@forEach
            val target = find(journal.getString("path"))
            val actual = target?.let { hash(it) } ?: ""
            if (actual == journal.getString("newHash") || actual == journal.getString("oldHash")) {
                journal.put("finished", true)
                persist(file, journal)
            } else {
                error("Recuperação pendente para ${journal.getString("path")}. Exporte as cópias de recuperação e restaure a versão desejada na pasta. Os arquivos old e new estão preservados.")
            }
        }
    }

    fun scanJson(): String {
        recoverPending()
        val manifest = JSONObject()
        var count = 0
        fun walk(directory: DocumentFile, prefix: String) {
            val children = directory.listFiles()
            val names = HashSet<String>()
            children.forEach { child ->
                val name = child.name ?: error("Arquivo sem nome no provedor.")
                check(names.add(name)) { "O provedor contém nomes duplicados: $name" }
                val path = if (prefix.isEmpty()) name else "$prefix/$name"
                if (child.isDirectory) walk(child, path)
                else {
                    check(child.isFile && !child.isVirtual) { "Tipo de documento não suportado: $path" }
                    check(++count <= 50_000) { "Limite de 50 mil arquivos excedido." }
                    val (hash, size) = digest(resolver.openInputStream(child.uri) ?: error("Sem acesso: $path"))
                    manifest.put(path, JSONObject().put("hash", hash).put("size", size))
                }
            }
        }
        check(root.canRead() && root.canWrite()) { "A permissão de leitura/gravação da pasta foi revogada." }
        walk(root, "")
        return manifest.toString()
    }

    fun snapshot(path: String, expectedHash: String): String {
        val source = find(path) ?: error("STALE_SOURCE: $path")
        val staged = File.createTempFile("rowd-send-", ".part", context.cacheDir)
        try {
            copyToPrivate(source, staged)
            check(digest(staged.inputStream()).first == expectedHash) { "STALE_SOURCE: $path" }
            return staged.absolutePath
        } catch (e: Exception) { staged.delete(); throw e }
    }

    fun install(path: String, expectedHash: String, newHash: String, sourcePath: String): String {
        val source = File(sourcePath)
        check(digest(source.inputStream()).first == newHash) { "SHA-256 não confere." }
        val old = find(path)
        val actual = old?.let { hash(it) } ?: ""
        if (actual == newHash) return "ok"
        check(actual == expectedHash) { "STALE_TARGET: $path" }
        val id = UUID.randomUUID().toString()
        val incoming = File(recovery, "$id.new")
        val backup = File(recovery, "$id.old")
        val journalFile = File(recovery, "$id.json")
        FileOutputStream(incoming).use { out -> source.inputStream().use { it.copyTo(out) }; out.fd.sync() }
        if (old != null) {
            copyToPrivate(old, backup)
            check(digest(backup.inputStream()).first == expectedHash) { "STALE_TARGET: $path" }
        }
        val journal = JSONObject().put("path", path).put("tree", treeUri.toString())
            .put("oldHash", expectedHash).put("newHash", newHash).put("finished", false)
        persist(journalFile, journal)
        // SAF lacks atomic compare-and-replace. Recheck immediately and retain both snapshots.
        if ((find(path)?.let { hash(it) } ?: "") != expectedHash) {
            // No shared document was touched; this is an aborted operation, not a crash.
            journal.put("finished", true)
            persist(journalFile, journal)
            error("STALE_TARGET: $path")
        }
        val (directory, name) = parent(path)
        val target = old ?: directory.createFile("application/octet-stream", name)
            ?: error("Não foi possível criar $path")
        check(target.name == name) { "O provedor alterou o nome do arquivo; sincronização interrompida." }
        writeDocument(incoming, target)
        check(hash(target) == newHash) { "Falha na gravação. Cópias preservadas para recuperação." }
        journal.put("finished", true)
        persist(journalFile, journal)
        return "ok"
    }

    fun exportRecovery(destination: Uri) {
        val directory = DocumentFile.fromTreeUri(context, destination) ?: error("Destino inválido")
        val output = directory.createDirectory("Rowd-recovery-${System.currentTimeMillis()}") ?: error("Sem acesso ao destino")
        recovery.listFiles()?.filter { it.isFile }?.forEach { file ->
            val doc = output.createFile("application/octet-stream", file.name) ?: error("Falha ao exportar")
            resolver.openOutputStream(doc.uri)?.use { out -> file.inputStream().use { it.copyTo(out) } }
                ?: error("Falha ao exportar ${file.name}")
        }
    }
}
