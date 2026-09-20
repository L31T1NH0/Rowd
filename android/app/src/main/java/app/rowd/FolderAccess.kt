package app.rowd

import android.content.Context
import android.net.Uri
import androidx.documentfile.provider.DocumentFile
import org.json.JSONObject
import org.json.JSONArray
import java.io.File
import java.io.FileOutputStream
import java.io.InputStream
import java.security.MessageDigest
import java.util.UUID

/** SAF boundary. All calls run on the single sync worker, never on the UI thread. */
class FolderAccess(private val context: Context, private val treeUri: Uri) {
    private val resolver = context.contentResolver
    private val recovery = File(context.filesDir, "recovery").apply { mkdirs() }
    private val definitions = File(context.filesDir, "shares.json")
    private val requests = File(context.filesDir, "share-requests.json")
    private var active: JSONObject? = null
    private val base get() = DocumentFile.fromTreeUri(context, treeUri)
        ?: error("A raiz Rowd não está disponível. Confira a permissão de acesso.")
    private val root: DocumentFile get() {
        var doc = base
        val path = active?.optString("android_path") ?: ""
        for (part in path.split('/').filter { it.isNotEmpty() }) {
            doc = doc.findFile(part) ?: doc.createDirectory(part) ?: error("Não foi possível criar $part")
            check(doc.isDirectory && doc.name == part) { "Destino Android inválido: $path" }
        }
        return doc
    }
    fun knownShares(): String = if (definitions.exists()) definitions.readText() else "[]"
    fun pendingShareRequests(): String =
        if (requests.exists()) requests.readText() else "[]"

    fun queueShareRequest(name: String, mode: String): String {
        val cleanName = name.trim()
        check(cleanName.isNotEmpty() && cleanName.length <= 120 && cleanName.none { it.isISOControl() }) {
            "Informe um nome válido para o Share."
        }
        check(mode in setOf("bidirectional", "to_android", "to_pc")) { "Modo inválido." }
        val shares = JSONArray(knownShares())
        check((0 until shares.length()).none { shares.getJSONObject(it).getString("name") == cleanName }) {
            "Já existe um Share com esse nome."
        }
        val pending = JSONArray(pendingShareRequests())
        check((0 until pending.length()).none { pending.getJSONObject(it).getString("name") == cleanName }) {
            "Já existe uma solicitação com esse nome."
        }
        val requestId = MessageDigest.getInstance("SHA-256")
            .digest(UUID.randomUUID().toString().toByteArray())
            .joinToString("") { "%02x".format(it.toInt() and 255) }
        pending.put(JSONObject().put("request_id", requestId).put("name", cleanName).put("mode", mode))
        persistText(requests, pending.toString())
        return requestId
    }

    fun acknowledgeShareRequests(acceptedJson: String): String {
        val accepted = JSONArray(acceptedJson)
        if (accepted.length() == 0) return "ok"
        val pending = JSONArray(pendingShareRequests())
        val remaining = JSONArray()
        for (i in 0 until pending.length()) {
            val request = pending.getJSONObject(i)
            val id = request.getString("request_id")
            if ((0 until accepted.length()).none { accepted.getString(it) == id }) remaining.put(request)
        }
        persistText(requests, remaining.toString())
        return "ok"
    }

    fun configureShares(json: String, removed: String): String {
        val previous = JSONArray(knownShares())
        val shares = JSONArray(json)
        for (i in 0 until shares.length()) {
            val share = shares.getJSONObject(i)
            for (j in 0 until previous.length()) {
                val old = previous.getJSONObject(j)
                if (old.getString("share_id") == share.getString("share_id")) {
                    check(old.getString("android_path") == share.getString("android_path")) { "Destino do Share mudou." }
                }
            }
        }
        val hasLegacy = (0 until shares.length()).any { shares.getJSONObject(it).getString("android_path").isEmpty() }
        if (hasLegacy) for (i in 0 until shares.length()) {
            val share = shares.getJSONObject(i)
            val path = share.getString("android_path")
            val known = (0 until previous.length()).any { previous.getJSONObject(it).getString("share_id") == share.getString("share_id") }
            if (path.isNotEmpty() && !known) {
                var existing: DocumentFile? = base
                for (part in path.split('/')) existing = existing?.findFile(part)
                check(existing == null) { "Destino coincide com conteúdo legado: $path. Escolha outro destino pelo PC." }
            }
        }
        // Removal only unlinks the definition; files and recovery stay available.
        JSONArray(removed)
        persistText(definitions, shares.toString())
        for (i in 0 until shares.length()) { active = shares.getJSONObject(i); check(root.canWrite()) { "Sem acesso ao Share" } }
        active = null
        return "ok"
    }
    fun selectShare(id: String): String {
        val shares = JSONArray(knownShares())
        active = (0 until shares.length()).map { shares.getJSONObject(it) }.firstOrNull { it.getString("share_id") == id }
            ?: error("Share desconhecido")
        check(id.matches(Regex("[0-9a-f]{64}"))) { "ID inválido" }
        return File(context.filesDir,"shares/$id/journal.json").absolutePath
    }
    private fun persistText(file: File, text: String) {
        file.parentFile?.mkdirs()
        val temp = File(file.parentFile, file.name + ".tmp")
        FileOutputStream(temp).use { it.write(text.toByteArray()); it.fd.sync() }
        check(temp.renameTo(file)) { "Não foi possível salvar o estado." }
    }
    private fun ignored(path: String, directory: Boolean, rules: List<String>): Boolean {
        if (path.split('/').any { it == ".rowd" } || path == ".rowdignore") return true
        val segments = path.split('/')
        return rules.any { rule ->
            val pattern = rule.trim('/'); val directoryOnly = rule.endsWith('/')
            val regex = Regex(pattern.split('*').joinToString(".*") { Regex.escape(it) })
            (1..segments.size).any { n ->
                !(directoryOnly && n == segments.size && !directory) &&
                    regex.matches(if (pattern.contains('/')) segments.take(n).joinToString("/") else segments[n-1])
            }
        }
    }
    fun ignoreText(): String = ignoreRules().joinToString("\n")
    private fun ignoreRules(): List<String> {
        val local = root.findFile(".rowdignore")?.let { d -> resolver.openInputStream(d.uri)?.bufferedReader()?.use { it.readText() } } ?: ""
        val shares = JSONArray(knownShares())
        val managed = if (active?.optString("android_path") == "") (0 until shares.length()).map { shares.getJSONObject(it).getString("android_path") }.filter { it.isNotEmpty() }.joinToString("\n") { "$it/" } else ""
        return (local + "\n" + managed + "\n" + (active?.optString("ignore") ?: "")).lines().map { it.trim() }.filter { it.isNotEmpty() && !it.startsWith('#') }
    }

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
            val id = active?.optString("share_id") ?: ""
            if (journal.optString("share_id") != id && !(journal.optString("share_id").isEmpty() && active?.optString("android_path").isNullOrEmpty())) return@forEach
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
        val rules = ignoreRules()
        var count = 0
        fun walk(directory: DocumentFile, prefix: String) {
            val children = directory.listFiles()
            val names = HashSet<String>()
            children.forEach { child ->
                val name = child.name ?: error("Arquivo sem nome no provedor.")
                check(names.add(name)) { "O provedor contém nomes duplicados: $name" }
                val path = if (prefix.isEmpty()) name else "$prefix/$name"
                if (ignored(path,child.isDirectory,rules)) return@forEach
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
        check(!ignored(path,false,ignoreRules())) { "Caminho ignorado: $path" }
        val source = find(path) ?: error("STALE_SOURCE: $path")
        val staged = File.createTempFile("rowd-send-", ".part", context.cacheDir)
        try {
            copyToPrivate(source, staged)
            check(digest(staged.inputStream()).first == expectedHash) { "STALE_SOURCE: $path" }
            return staged.absolutePath
        } catch (e: Exception) { staged.delete(); throw e }
    }

    fun install(path: String, expectedHash: String, newHash: String, sourcePath: String): String {
        check(!ignored(path,false,ignoreRules())) { "Caminho ignorado: $path" }
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
            .put("share_id", active?.optString("share_id") ?: "").put("android_path", active?.optString("android_path") ?: "").put("oldHash", expectedHash).put("newHash", newHash).put("finished", false)
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

    fun recoveryRecords(): List<Pair<File, JSONObject>> = recovery.listFiles { f -> f.extension == "json" }
        ?.map { it to JSONObject(it.readText()) } ?: emptyList()

    fun resolveRecovery(id: String, restore: Boolean) {
        check(id.matches(Regex("[a-zA-Z0-9-]+"))) { "ID inválido" }
        val file = File(recovery,"$id.json")
        val journal = JSONObject(file.readText())
        val shareId = journal.optString("share_id")
        if (shareId.isNotEmpty()) {
            try { selectShare(shareId) } catch (e: IllegalStateException) {
                check(journal.has("android_path")) { "Share removido; exporte as cópias." }
                active = JSONObject().put("share_id",shareId).put("android_path",journal.getString("android_path"))
            }
        } else active = null
        check(journal.getString("tree") == treeUri.toString()) { "Outra raiz Android" }
        val path = journal.getString("path")
        if (restore) {
            val backup = File(recovery,"$id.old")
            check(backup.exists()) { "Não há versão anterior; exporte a cópia new." }
            val expected = find(path)?.let { hash(it) } ?: ""
            install(path,expected,digest(backup.inputStream()).first,backup.absolutePath)
        }
        journal.put("finished",true); persist(file,journal)
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
