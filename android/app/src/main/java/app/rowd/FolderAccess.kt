package app.rowd

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import androidx.documentfile.provider.DocumentFile
import org.json.JSONObject
import org.json.JSONArray
import java.io.File
import java.io.FileOutputStream
import java.io.InputStream
import java.security.MessageDigest
import java.util.UUID

/** SAF boundary. Sync I/O uses one worker; administrative file updates share [stateLock]. */
class FolderAccess(private val context: Context) {
    companion object {
        private val stateLock = Any()
    }

    private val resolver = context.contentResolver
    private val recovery = File(context.filesDir, "recovery").apply { mkdirs() }
    private val definitions = File(context.filesDir, "shares.json")
    private val requests = File(context.filesDir, "share-requests.json")
    private val requestResults = File(context.filesDir, "share-request-results.json")
    private val shareTrees = File(context.filesDir, "share-trees.json")
    private var active: JSONObject? = null
    private var selectedTree: Uri? = null
    private var recoveryTree: Uri? = null
    private fun trees(): JSONObject = if (shareTrees.exists()) JSONObject(shareTrees.readText()) else JSONObject()
    private val activeTree get() = selectedTree ?: recoveryTree
        ?: error("Escolha a pasta Android deste Share.")
    private val base get() = DocumentFile.fromTreeUri(context, activeTree)
        ?: error("A pasta Android do Share não está disponível. Confira a permissão de acesso.")
    private val root: DocumentFile get() = base
    fun knownShares(): String = if (definitions.exists()) definitions.readText() else "[]"
    fun pendingShareRequests(): String =
        if (requests.exists()) requests.readText() else "[]"
    fun shareRequestResults(): String =
        if (requestResults.exists()) requestResults.readText() else "[]"

    private fun treeParts(uri: Uri): Pair<String, List<String>>? = try {
        val id = DocumentsContract.getTreeDocumentId(uri)
        val volume = if (id.contains(':')) id.substringBefore(':') else ""
        val relative = if (id.contains(':')) id.substringAfter(':') else id
        "${uri.authority}:$volume" to relative.split('/').filter { it.isNotEmpty() }
    } catch (_: Exception) { null }

    private fun overlaps(first: String, second: String): Boolean {
        if (first == second) return true
        val a = treeParts(Uri.parse(first)) ?: return false
        val b = treeParts(Uri.parse(second)) ?: return false
        if (a.first != b.first) return false
        return a.second.size <= b.second.size && b.second.take(a.second.size) == a.second ||
            b.second.size <= a.second.size && a.second.take(b.second.size) == b.second
    }

    private fun validateTree(tree: String, selected: JSONObject, except: String? = null) {
        val uri = Uri.parse(tree)
        val directory = DocumentFile.fromTreeUri(context, uri)
            ?: error("Pasta do Share não está disponível.")
        check(directory.isDirectory && directory.canRead() && directory.canWrite()) {
            "Sem acesso de leitura e gravação à pasta escolhida para o Share."
        }
        val keys = selected.names() ?: JSONArray()
        check((0 until keys.length()).none {
            val id = keys.getString(it)
            id != except && overlaps(selected.getString(id), tree)
        }) { "A pasta escolhida coincide ou está dentro de outro Share." }
    }

    fun queueShareRequest(name: String, mode: String, tree: String): String = synchronized(stateLock) {
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
        validateTree(tree, trees())
        check((0 until pending.length()).none {
            pending.getJSONObject(it).optString("tree").takeIf(String::isNotEmpty)
                ?.let { other -> overlaps(other, tree) } == true
        }) { "Esta pasta já pertence a outra solicitação de Share." }
        resolver.takePersistableUriPermission(
            Uri.parse(tree),
            Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
        )
        val requestId = MessageDigest.getInstance("SHA-256")
            .digest(UUID.randomUUID().toString().toByteArray())
            .joinToString("") { "%02x".format(it.toInt() and 255) }
        pending.put(JSONObject().put("request_id", requestId).put("name", cleanName).put("mode", mode).put("state", "pending").put("tree", tree))
        persistText(requests, pending.toString())
        requestId
    }

    fun cancelShareRequest(id: String): String = synchronized(stateLock) {
        check(id.matches(Regex("[0-9a-f]{64}"))) { "ID inválido" }
        val pending = JSONArray(pendingShareRequests())
        var found = false
        for (i in 0 until pending.length()) {
            val request = pending.getJSONObject(i)
            if (request.getString("request_id") == id) {
                request.put("state", "cancelled")
                found = true
            }
        }
        check(found) { "Solicitação não encontrada" }
        persistText(requests, pending.toString())
        "ok"
    }

    fun acknowledgeShareRequests(acceptedJson: String, rejectedJson: String, cancelledJson: String): String = synchronized(stateLock) {
        fun ids(json: String) = JSONArray(json).let { array ->
            (0 until array.length()).map { array.getString(it) }.toSet()
        }
        val accepted = ids(acceptedJson)
        val rejected = ids(rejectedJson)
        val cancelled = ids(cancelledJson)
        val terminal = accepted + rejected + cancelled
        if (terminal.isEmpty()) return@synchronized "ok"
        val pending = JSONArray(pendingShareRequests())
        val remaining = JSONArray()
        val oldResults = JSONArray(shareRequestResults())
        val newResults = mutableListOf<JSONObject>()
        for (i in 0 until oldResults.length()) newResults.add(oldResults.getJSONObject(i))
        for (i in 0 until pending.length()) {
            val request = pending.getJSONObject(i)
            val id = request.getString("request_id")
            if (id !in terminal) {
                remaining.put(request)
            } else {
                val state = when (id) {
                    in accepted -> "accepted"
                    in rejected -> "rejected"
                    else -> "cancelled"
                }
                newResults.add(
                    JSONObject()
                        .put("request_id", id)
                        .put("name", request.getString("name"))
                        .put("state", state)
                        .put("at", System.currentTimeMillis())
                )
            }
        }
        persistText(requests, remaining.toString())
        val retained = JSONArray()
        newResults.takeLast(50).forEach { retained.put(it) }
        persistText(requestResults, retained.toString())
        "ok"
    }

    fun requestUnlink(): String {
        context.getSharedPreferences("rowd", Context.MODE_PRIVATE).edit().putBoolean("unlinkRequested", true).apply()
        return "ok"
    }

    fun unlinkRequested(): String = context.getSharedPreferences("rowd", Context.MODE_PRIVATE)
        .getBoolean("unlinkRequested", false).toString()

    fun confirmUnlinked(): String = synchronized(stateLock) {
        context.getSharedPreferences("rowd", Context.MODE_PRIVATE).edit()
            .remove("invitation").remove("unlinkRequested").apply()
        val shareState = File(context.filesDir, "shares")
        if (shareState.exists()) {
            val archive = File(context.filesDir, "state-archives").apply { mkdirs() }
            check(shareState.renameTo(File(archive, "shares-before-unlink-${System.currentTimeMillis()}"))) {
                "Não foi possível preservar o estado anterior dos Shares."
            }
        }
        definitions.delete()
        shareTrees.delete()
        requests.delete()
        requestResults.delete()
        active = null
        selectedTree = null
        recoveryTree = null
        "ok"
    }

    fun configureShares(json: String, removed: String): String = synchronized(stateLock) {
        val previous = JSONArray(knownShares())
        val shares = JSONArray(json)
        val pending = JSONArray(pendingShareRequests())
        val selectedTrees = trees()
        val currentIds = (0 until shares.length()).map { shares.getJSONObject(it).getString("share_id") }.toSet()
        val oldKeys = selectedTrees.names() ?: JSONArray()
        for (i in 0 until oldKeys.length()) {
            val id = oldKeys.getString(i)
            if (id !in currentIds) selectedTrees.remove(id)
        }
        for (i in 0 until shares.length()) {
            val share = shares.getJSONObject(i)
            val requestId = share.optString("request_id")
            val request = (0 until pending.length()).map { pending.getJSONObject(it) }
                .firstOrNull { it.getString("request_id") == requestId }
            val tree = request?.optString("tree")?.takeIf { it.isNotEmpty() } ?: continue
            if (!selectedTrees.has(share.getString("share_id"))) {
                validateTree(tree, selectedTrees)
                selectedTrees.put(share.getString("share_id"), tree)
            }
        }
        for (i in 0 until shares.length()) {
            val share = shares.getJSONObject(i)
            for (j in 0 until previous.length()) {
                val old = previous.getJSONObject(j)
                if (old.getString("share_id") == share.getString("share_id")) {
                    if (old.getString("android_path") != share.getString("android_path")) {
                        selectedTrees.remove(share.getString("share_id"))
                    }
                }
            }
        }
        val legacyTree = context.getSharedPreferences("rowd", Context.MODE_PRIVATE).getString("tree", null)
        var legacyTreeMapped = false
        if (legacyTree != null) for (i in 0 until shares.length()) {
            val share = shares.getJSONObject(i)
            if (share.getString("android_path").isEmpty()) {
                if (!selectedTrees.has(share.getString("share_id"))) {
                    validateTree(legacyTree, selectedTrees)
                    selectedTrees.put(share.getString("share_id"), legacyTree)
                }
                legacyTreeMapped = true
            }
        }
        JSONArray(removed) // Removal unlinks only the mapping; files and recovery stay available.
        persistText(shareTrees, selectedTrees.toString())
        persistText(definitions, shares.toString())
        if (legacyTreeMapped) context.getSharedPreferences("rowd", Context.MODE_PRIVATE)
            .edit().remove("tree").apply()
        active = null
        selectedTree = null
        recoveryTree = null
        "ok"
    }
    fun bindShare(id: String, tree: String): String = synchronized(stateLock) {
        check(id.matches(Regex("[0-9a-f]{64}"))) { "ID inválido" }
        val shares = JSONArray(knownShares())
        check((0 until shares.length()).any { shares.getJSONObject(it).getString("share_id") == id }) {
            "Share desconhecido"
        }
        val selected = trees()
        validateTree(tree, selected, id)
        resolver.takePersistableUriPermission(
            Uri.parse(tree),
            Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
        )
        selected.put(id, tree)
        persistText(shareTrees, selected.toString())
        "ok"
    }
    fun availableShares(): String {
        val selected = trees()
        val shares = JSONArray(knownShares())
        val available = JSONArray()
        for (i in 0 until shares.length()) {
            val id = shares.getJSONObject(i).getString("share_id")
            val tree = selected.optString(id)
            if (tree.isNotEmpty() && runCatching {
                val directory = DocumentFile.fromTreeUri(context, Uri.parse(tree))
                directory != null && directory.isDirectory && directory.canRead() && directory.canWrite()
            }.getOrDefault(false)) available.put(id)
        }
        return available.toString()
    }
    fun unassignedShares(): String {
        val available = JSONArray(availableShares())
        val ids = (0 until available.length()).map { available.getString(it) }.toSet()
        val shares = JSONArray(knownShares())
        val missing = JSONArray()
        for (i in 0 until shares.length()) {
            val share = shares.getJSONObject(i)
            if (share.getString("share_id") !in ids) missing.put(
                JSONObject().put("share_id", share.getString("share_id")).put("name", share.getString("name"))
            )
        }
        return missing.toString()
    }
    fun treeUris(): List<Uri> {
        val values = mutableSetOf<String>()
        val selected = trees()
        val keys = selected.names() ?: JSONArray()
        for (i in 0 until keys.length()) values.add(selected.getString(keys.getString(i)))
        val pending = JSONArray(pendingShareRequests())
        for (i in 0 until pending.length()) pending.getJSONObject(i).optString("tree")
            .takeIf(String::isNotEmpty)?.let(values::add)
        return values.map(Uri::parse)
    }
    fun selectShare(id: String): String {
        check(id.matches(Regex("[0-9a-f]{64}"))) { "ID inválido" }
        val shares = JSONArray(knownShares())
        active = (0 until shares.length()).map { shares.getJSONObject(it) }.firstOrNull { it.getString("share_id") == id }
            ?: error("Share desconhecido")
        selectedTree = trees().optString(id).takeIf { it.isNotEmpty() }?.let(Uri::parse)
            ?: error("Escolha a pasta Android deste Share.")
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
            if (journal.optString("tree") != activeTree.toString() || journal.optBoolean("finished")) return@forEach
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
        val journal = JSONObject().put("path", path).put("tree", activeTree.toString())
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
        selectedTree = null
        recoveryTree = Uri.parse(journal.getString("tree"))
        val shareId = journal.optString("share_id")
        if (shareId.isNotEmpty()) {
            try { selectShare(shareId) } catch (e: IllegalStateException) {
                check(journal.has("android_path")) { "Share removido; exporte as cópias." }
                active = JSONObject().put("share_id",shareId).put("android_path",journal.getString("android_path"))
            }
        } else active = null
        check(journal.getString("tree") == activeTree.toString()) { "Outra raiz Android" }
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
