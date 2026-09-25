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
        private const val LOCAL_OP_TIMEOUT_MS = 30L * 60L * 1000L
    }

    private fun checkLocalDeadline(deadline: Long) {
        check(!Thread.currentThread().isInterrupted) { "Sincronização cancelada." }
        check(android.os.SystemClock.elapsedRealtime() < deadline) { "Operação SAF excedeu 30 minutos." }
    }

    private val resolver = context.contentResolver
    private fun <T> traced(name: String, path: String?, block: () -> T): T {
        if (!PerformanceTrace.enabled()) return block()
        val share = active?.optString("share_id")
        val start = PerformanceTrace.now()
        PerformanceTrace.event("${name}_start", share, path)
        return try { block() } finally { PerformanceTrace.event("${name}_end", share, path, start = start) }
    }
    private val recovery = File(context.filesDir, "recovery").apply { mkdirs() }
    private val definitions = File(context.filesDir, "shares.json")
    private val requests = File(context.filesDir, "share-requests.json")
    private val requestResults = File(context.filesDir, "share-request-results.json")
    private val legacyTrees = File(context.filesDir, "share-trees.json")
    private val shareBindings = File(context.filesDir, "share-bindings.json")
    private var active: JSONObject? = null
    private var selectedTree: Uri? = null
    private var recoveryTree: Uri? = null
    private data class ScanEntry(
        val tree: String, val uri: String, val modified: Long, val length: Long,
        val hash: String, val size: Long
    )
    private val scanLock = Any()
    private val scanCache = mutableMapOf<String, MutableMap<String, ScanEntry>>()
    private val uriPaths = mutableMapOf<String, MutableMap<String, String>>()
    private val directoryUris = mutableMapOf<String, Map<String, String>>()
    private val pendingUris = mutableMapOf<String, MutableSet<String>>()
    private val dirtyDirectories = mutableMapOf<String, MutableSet<String>>()
    private val scanReady = mutableSetOf<String>()
    private val dirtyPaths = mutableMapOf<String, MutableSet<String>>()
    private val fullScanShares = mutableSetOf<String>()
    private val deepScanShares = mutableSetOf<String>()
    private var focusedScan = false

    fun setFocusedScan(focused: Boolean) { focusedScan = focused }

    fun bindingIdentity(): String = "${activeTree}|${active?.optLong("binding_revision", 0L)}|${ignoreText()}"

    fun forceFullScan(): String {
        synchronized(scanLock) { fullScanShares.add(active?.getString("share_id") ?: error("Nenhum Share selecionado.")) }
        return "ok"
    }
    fun forceDeepScan(): String {
        synchronized(scanLock) { deepScanShares.add(active?.getString("share_id") ?: error("Nenhum Share selecionado.")) }
        return "ok"
    }
    fun scheduleDeepAudit() {
        val shares = JSONArray(knownShares())
        synchronized(scanLock) {
            for (index in 0 until shares.length()) deepScanShares.add(shares.getJSONObject(index).getString("share_id"))
        }
    }

    fun deltaPathsJson(): String = synchronized(scanLock) {
        val id = active?.getString("share_id") ?: return@synchronized "null"
        val cache = scanCache[id] ?: return@synchronized "null"
        if (!focusedScan || id in fullScanShares || id in deepScanShares || id !in scanReady ||
            (cache.isNotEmpty() && cache.values.first().tree != activeTree.toString()))
            return@synchronized "null"
        try {
        val unresolved = pendingUris.remove(id).orEmpty()
        for (uri in unresolved) {
            var found: String? = null
            val rootUri = root.uri.toString()
            val directories = sequenceOf(rootUri to "") + directoryUris[id].orEmpty().asSequence()
                .filter { it.key != rootUri }.take(127).map { it.key to it.value }
            for ((directoryUri, prefix) in directories) {
                val directory = if (prefix.isEmpty()) root else findDirectory(prefix) ?: continue
                if (directory.uri.toString() != directoryUri) continue
                val child = directory.listFiles().firstOrNull { it.uri.toString() == uri }
                if (child != null) {
                    val name = child.name ?: return@synchronized "null"
                    if (!child.isFile || child.isVirtual) return@synchronized "null"
                    found = if (prefix.isEmpty()) name else "$prefix/$name"
                    break
                }
            }
            if (found == null) { fullScanShares.add(id); return@synchronized "null" }
            dirtyPaths.getOrPut(id) { mutableSetOf() }.add(found)
        }
        for (prefix in dirtyDirectories.remove(id).orEmpty()) {
            val directory = if (prefix.isEmpty()) root else findDirectory(prefix) ?: return@synchronized "null"
            val found = mutableSetOf<String>()
            val seen = mutableSetOf<String>()
            fun walk(directory: DocumentFile, prefix: String) {
                val names = mutableSetOf<String>()
                for (child in directory.listFiles()) {
                    val name = child.name ?: error("Arquivo sem nome no provedor.")
                    check(names.add(name)) { "O provedor contém nomes duplicados: $name" }
                    val path = if (prefix.isEmpty()) name else "$prefix/$name"
                    if (child.isDirectory) walk(child, path)
                    else if (child.isFile && !child.isVirtual) {
                        seen.add(path)
                        val cached = cache[path]
                        if (uriPaths[id]?.get(child.uri.toString())?.let { it != path } == true) {
                            error("URI reutilizada em outro caminho: $path")
                        }
                        if (cached == null || cached.uri != child.uri.toString() ||
                            cached.modified <= 0 || cached.modified != child.lastModified() ||
                            cached.length != child.length()) found.add(path)
                    }
                    else error("Tipo de documento não suportado: $path")
                }
            }
            walk(directory, prefix)
            found.addAll(cache.keys.filter { (prefix.isEmpty() || it.startsWith("$prefix/")) && it !in seen })
            dirtyPaths.getOrPut(id) { mutableSetOf() }.addAll(found)
        }
        } catch (_: Exception) {
            deepScanShares.add(id)
            return@synchronized "null"
        }
        if (dirtyPaths[id].orEmpty().size > 1024) return@synchronized "null"
        JSONArray(dirtyPaths[id].orEmpty().toList()).toString()
    }

    fun scanPathsJson(pathsJson: String): String {
        val id = active?.getString("share_id") ?: return "null"
        fun needDeepScan(): String {
            synchronized(scanLock) { deepScanShares.add(id) }
            return "null"
        }
        val paths = JSONArray(pathsJson)
        if (paths.length() > 1024) return "null"
        val deadline = android.os.SystemClock.elapsedRealtime() + LOCAL_OP_TIMEOUT_MS
        val cache = synchronized(scanLock) { if (id in scanReady) scanCache[id] else null } ?: return "null"
        val files = JSONObject()
        var enumerated = 0
        var bytesHashed = 0L
        val updates = mutableMapOf<String, ScanEntry>()
        for (index in 0 until paths.length()) {
            checkLocalDeadline(deadline)
            val path = paths.getString(index)
            if (cache[path]?.tree != null && cache[path]?.tree != activeTree.toString()) return needDeepScan()
            if (ignored(path, false, ignoreRules())) return needDeepScan()
            val document = try { find(path) } catch (_: Exception) { return needDeepScan() }
            if (document == null) continue
            if (!document.isFile || document.isVirtual) return needDeepScan()
            val (hash, size) = try {
                digest(resolver.openInputStream(document.uri) ?: return needDeepScan(), deadline = deadline)
            } catch (_: Exception) { return needDeepScan() }
            bytesHashed += size
            enumerated++
            val modified = try { document.lastModified() } catch (_: Exception) { return needDeepScan() }
            val length = try { document.length() } catch (_: Exception) { return needDeepScan() }
            updates[path] = ScanEntry(activeTree.toString(), document.uri.toString(), modified,
                length, hash, size)
            files.put(path, JSONObject().put("hash", hash).put("size", size))
        }
        synchronized(scanLock) {
            if (id in fullScanShares || id in deepScanShares || scanCache[id] !== cache) return "null"
            val requested = (0 until paths.length()).map { paths.getString(it) }.toSet()
            val uris = uriPaths[id] ?: return "null"
            val seen = HashSet<String>()
            for ((path, entry) in updates) {
                val prior = uris[entry.uri]
                if (!seen.add(entry.uri) || (prior != null && prior != path && prior !in requested)) {
                    deepScanShares.add(id)
                    return "null"
                }
            }
            for (path in requested) {
                cache.remove(path)?.let { if (uris[it.uri] == path) uris.remove(it.uri) }
            }
            cache.putAll(updates)
            for ((path, entry) in updates) uris[entry.uri] = path
        }
        return JSONObject().put("files", files).put("enumerated", enumerated)
            .put("hashed", enumerated).put("bytes_hashed", bytesHashed).toString()
    }

    fun invalidateScans(shareIds: Set<String>) = synchronized(scanLock) {
        deepScanShares.addAll(shareIds)
    }

    /** A provider-wide notification invalidates only its Share; the periodic audit still covers all Shares. */
    fun noteChange(shareId: String?, changedUri: Uri?): Boolean = synchronized(scanLock) {
        if (shareId == null) return@synchronized false
        val path = changedUri?.let { uriPaths[shareId]?.get(it.toString()) }
        if (path == null) {
            if (changedUri == null) fullScanShares.add(shareId)
            else if (directoryUris[shareId]?.containsKey(changedUri.toString()) == true)
                dirtyDirectories.getOrPut(shareId) { mutableSetOf() }.add(directoryUris[shareId]!![changedUri.toString()]!!)
            else pendingUris.getOrPut(shareId) { mutableSetOf() }.add(changedUri.toString())
        }
        else dirtyPaths.getOrPut(shareId) { mutableSetOf() }.add(path)
        true
    }
    init { synchronized(stateLock) {
        if (requestResults.exists()) check(requestResults.delete()) { "Não foi possível remover o histórico antigo de solicitações." }
        if (!shareBindings.exists() && legacyTrees.exists()) {
            val old = JSONObject(legacyTrees.readText())
            val migrated = JSONObject()
            val keys = old.names() ?: JSONArray()
            for (index in 0 until keys.length()) {
                val id = keys.getString(index)
                migrated.put(id, JSONObject().put("tree_uri", old.getString(id)).put("revision", 1))
            }
            persistText(shareBindings, migrated.toString())
            check(legacyTrees.delete()) { "Não foi possível finalizar a migração dos vínculos SAF." }
        }
    } }
    private fun trees(): JSONObject = if (shareBindings.exists()) JSONObject(shareBindings.readText()) else JSONObject()
    private fun boundTree(selected: JSONObject, id: String): String =
        selected.optJSONObject(id)?.optString("tree_uri") ?: ""
    private fun putTree(selected: JSONObject, id: String, uri: String) {
        val revision = selected.optJSONObject(id)?.optLong("revision", 0L) ?: 0L
        selected.put(id, JSONObject().put("tree_uri", uri).put("revision", revision + 1))
    }
    private enum class BindingState { Bound, Unassigned, PermissionLost, ProviderUnavailable, Invalid }
    private fun bindingState(uri: String): BindingState {
        if (uri.isEmpty()) return BindingState.Unassigned
        val parsed = Uri.parse(uri)
        if (treeParts(parsed) == null) return BindingState.Invalid
        val grant = resolver.persistedUriPermissions.firstOrNull { it.uri == parsed }
        if (grant == null || !grant.isReadPermission || !grant.isWritePermission) return BindingState.PermissionLost
        return try {
            val directory = DocumentFile.fromTreeUri(context, parsed)
            if (directory != null && directory.isDirectory && directory.canRead() && directory.canWrite())
                BindingState.Bound else BindingState.ProviderUnavailable
        } catch (_: Exception) { BindingState.ProviderUnavailable }
    }
    private val activeTree get() = selectedTree ?: recoveryTree
        ?: error("Escolha a pasta Android deste Share.")
    private val base get() = DocumentFile.fromTreeUri(context, activeTree)
        ?: error("A pasta Android do Share não está disponível. Confira a permissão de acesso.")
    private val root: DocumentFile get() = base
    fun knownShares(): String = if (definitions.exists()) definitions.readText() else "[]"
    fun statusShares(): String {
        val shares = JSONArray(knownShares())
        val selected = trees()
        val status = JSONArray()
        for (index in 0 until shares.length()) {
            val share = shares.getJSONObject(index)
            val id = share.getString("share_id")
            status.put(JSONObject()
                .put("share_id", id)
                .put("name", share.getString("name"))
                .put("mode", share.getString("mode"))
                .put("enabled", share.optBoolean("enabled", true))
                .put("binding_state", bindingState(boundTree(selected, id)).name))
        }
        return status.toString()
    }
    fun pendingShareRequests(): String =
        if (requests.exists()) requests.readText() else "[]"

    private fun treeParts(uri: Uri): Pair<String, List<String>>? { return try {
        if (uri.scheme != "content" || uri.authority.isNullOrEmpty()) return null
        val id = DocumentsContract.getTreeDocumentId(uri)
        if (id.isEmpty()) return null
        val volume = if (id.contains(':')) id.substringBefore(':') else ""
        val relative = if (id.contains(':')) id.substringAfter(':') else id
        "${uri.authority}:$volume" to relative.split('/').filter { it.isNotEmpty() }
    } catch (_: Exception) { null } }

    private fun overlaps(first: String, second: String): Boolean {
        if (first == second) return true
        val a = treeParts(Uri.parse(first)) ?: return true
        val b = treeParts(Uri.parse(second)) ?: return true
        if (a.first != b.first) return false
        return a.second.size <= b.second.size && b.second.take(a.second.size) == a.second ||
            b.second.size <= a.second.size && a.second.take(b.second.size) == b.second
    }

    private fun validateTree(tree: String, selected: JSONObject, except: String? = null) {
        val uri = Uri.parse(tree)
        check(treeParts(uri) != null) { "Não foi possível comparar esta pasta com os outros Shares." }
        val directory = DocumentFile.fromTreeUri(context, uri)
            ?: error("Pasta do Share não está disponível.")
        check(directory.isDirectory && directory.canRead() && directory.canWrite()) {
            "Sem acesso de leitura e gravação à pasta escolhida para o Share."
        }
        val keys = selected.names() ?: JSONArray()
        check((0 until keys.length()).none {
            val id = keys.getString(it)
            id != except && overlaps(boundTree(selected, id), tree)
        }) { "A pasta escolhida coincide ou está dentro de outro Share." }
    }

    fun queueShareRequest(name: String, mode: String, tree: String): String = synchronized(stateLock) {
        val oldUris = treeUris().map(Uri::toString).toSet()
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
        try { persistText(requests, pending.toString()) }
        catch (error: Exception) {
            releaseUnusedGrants(oldUris + tree)
            throw error
        }
        requestId
    }

    fun cancelShareRequest(id: String): String = synchronized(stateLock) {
        val oldUris = treeUris().map(Uri::toString).toSet()
        check(id.matches(Regex("[0-9a-f]{64}"))) { "ID inválido" }
        val pending = JSONArray(pendingShareRequests())
        var found = false
        for (i in 0 until pending.length()) {
            val request = pending.getJSONObject(i)
            if (request.getString("request_id") == id) {
                request.put("state", "cancelled")
                request.remove("tree")
                found = true
            }
        }
        check(found) { "Solicitação não encontrada" }
        persistText(requests, pending.toString())
        releaseUnusedGrants(oldUris)
        "ok"
    }

    fun acknowledgeShareRequests(acceptedJson: String, rejectedJson: String, cancelledJson: String): String = synchronized(stateLock) {
        val oldUris = treeUris().map(Uri::toString).toSet()
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
        for (i in 0 until pending.length()) {
            val request = pending.getJSONObject(i)
            val id = request.getString("request_id")
            if (id !in terminal) {
                remaining.put(request)
            }
        }
        persistText(requests, remaining.toString())
        releaseUnusedGrants(oldUris)
        "ok"
    }

    fun requestUnlink(): String {
        check(context.getSharedPreferences("rowd", Context.MODE_PRIVATE).edit().putBoolean("unlinkRequested", true).commit()) {
            "Não foi possível salvar a intenção de desvincular."
        }
        return "ok"
    }

    fun unlinkRequested(): String = context.getSharedPreferences("rowd", Context.MODE_PRIVATE)
        .let { (it.getBoolean("unlinkRequested", false) || it.getBoolean("unlinkPrepared", false)).toString() }

    fun prepareUnlink(): String = synchronized(stateLock) {
        val shareState = File(context.filesDir, "shares")
        if (shareState.exists()) {
            val archive = File(context.filesDir, "state-archives").apply { mkdirs() }
            check(shareState.renameTo(File(archive, "shares-before-unlink-${System.currentTimeMillis()}"))) {
                "Não foi possível preservar o estado anterior dos Shares."
            }
        }
        check(context.getSharedPreferences("rowd", Context.MODE_PRIVATE).edit()
            .putBoolean("unlinkPrepared", true).commit()) { "Não foi possível registrar a preparação da desvinculação." }
        "ok"
    }

    fun confirmUnlinked(): String = synchronized(stateLock) {
        val oldUris = treeUris().map(Uri::toString).toSet()
        val shareState = File(context.filesDir, "shares")
        if (shareState.exists()) {
            val archive = File(context.filesDir, "state-archives").apply { mkdirs() }
            check(shareState.renameTo(File(archive, "shares-before-unlink-${System.currentTimeMillis()}"))) {
                "Não foi possível preservar o estado anterior dos Shares."
            }
        }
        definitions.delete()
        shareBindings.delete()
        legacyTrees.delete()
        requests.delete()
        requestResults.delete()
        releaseUnusedGrants(oldUris)
        check(context.getSharedPreferences("rowd", Context.MODE_PRIVATE).edit()
            .remove("invitation").remove("peerAddress").remove("unlinkRequested").remove("unlinkPrepared").commit()) {
            "Não foi possível concluir a desvinculação local."
        }
        active = null
        selectedTree = null
        recoveryTree = null
        synchronized(scanLock) {
            scanCache.clear(); uriPaths.clear(); directoryUris.clear(); pendingUris.clear()
            dirtyDirectories.clear(); scanReady.clear(); dirtyPaths.clear()
            fullScanShares.clear(); deepScanShares.clear()
        }
        "ok"
    }

    fun configureShares(json: String): String = synchronized(stateLock) {
        val started = android.os.SystemClock.elapsedRealtime()
        val oldUris = treeUris().map(Uri::toString).toSet()
        val previous = JSONArray(knownShares())
        val shares = JSONArray(json)
        val pending = JSONArray(pendingShareRequests())
        val selectedTrees = trees()
        val currentIds = (0 until shares.length()).map { shares.getJSONObject(it).getString("share_id") }.toSet()
        val resetCacheIds = mutableSetOf<String>()
        val oldKeys = selectedTrees.names() ?: JSONArray()
        for (i in 0 until oldKeys.length()) {
            val id = oldKeys.getString(i)
            if (id !in currentIds) selectedTrees.remove(id)
        }
        for (i in 0 until shares.length()) {
            val share = shares.getJSONObject(i)
            val old = (0 until previous.length()).map { previous.getJSONObject(it) }
                .firstOrNull { it.getString("share_id") == share.getString("share_id") }
            if (old != null && old.optLong("binding_revision", 0) != share.optLong("binding_revision", 0)) {
                selectedTrees.remove(share.getString("share_id"))
                resetCacheIds.add(share.getString("share_id"))
            }
            if (old != null && old.optString("ignore_rules") != share.optString("ignore_rules"))
                resetCacheIds.add(share.getString("share_id"))
        }
        for (i in 0 until shares.length()) {
            val share = shares.getJSONObject(i)
            val requestId = share.optString("request_id")
            val request = (0 until pending.length()).map { pending.getJSONObject(it) }
                .firstOrNull { it.getString("request_id") == requestId }
            val tree = request?.optString("tree")?.takeIf { it.isNotEmpty() } ?: continue
            if (!selectedTrees.has(share.getString("share_id"))) {
                validateTree(tree, selectedTrees)
                putTree(selectedTrees, share.getString("share_id"), tree)
            }
        }
        val legacyTree = context.getSharedPreferences("rowd", Context.MODE_PRIVATE).getString("tree", null)
        var legacyTreeMapped = false
        if (legacyTree != null) for (i in 0 until shares.length()) {
            val share = shares.getJSONObject(i)
            if (i == 0) {
                if (!selectedTrees.has(share.getString("share_id"))) {
                    validateTree(legacyTree, selectedTrees)
                    putTree(selectedTrees, share.getString("share_id"), legacyTree)
                }
                legacyTreeMapped = true
            }
        }
        val bindingsText = selectedTrees.toString()
        if (!shareBindings.exists() || shareBindings.readText() != bindingsText)
            persistText(shareBindings, bindingsText)
        val definitionsText = shares.toString()
        if (!definitions.exists() || definitions.readText() != definitionsText)
            persistText(definitions, definitionsText)
        releaseUnusedGrants(oldUris)
        if (legacyTreeMapped) context.getSharedPreferences("rowd", Context.MODE_PRIVATE)
            .edit().remove("tree").apply()
        active = null
        selectedTree = null
        recoveryTree = null
        synchronized(scanLock) {
            scanCache.keys.retainAll(currentIds - resetCacheIds)
            uriPaths.keys.retainAll(currentIds - resetCacheIds)
            directoryUris.keys.retainAll(currentIds - resetCacheIds)
            pendingUris.keys.retainAll(currentIds - resetCacheIds)
            dirtyDirectories.keys.retainAll(currentIds - resetCacheIds)
            scanReady.retainAll(currentIds - resetCacheIds)
            dirtyPaths.keys.retainAll(currentIds - resetCacheIds)
            fullScanShares.retainAll(currentIds - resetCacheIds)
            deepScanShares.retainAll(currentIds)
            deepScanShares.addAll(resetCacheIds)
        }
        android.util.Log.i("RowdLatency", "configure_ms=${android.os.SystemClock.elapsedRealtime() - started}")
        "ok"
    }
    fun bindShare(id: String, tree: String): String = synchronized(stateLock) {
        val oldUris = treeUris().map(Uri::toString).toSet()
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
        putTree(selected, id, tree)
        try { persistText(shareBindings, selected.toString()) }
        catch (error: Exception) {
            releaseUnusedGrants(oldUris + tree)
            throw error
        }
        synchronized(scanLock) {
            scanCache.remove(id); uriPaths.remove(id); directoryUris.remove(id)
            pendingUris.remove(id); dirtyDirectories.remove(id); scanReady.remove(id)
            dirtyPaths.remove(id); deepScanShares.add(id)
        }
        releaseUnusedGrants(oldUris)
        "ok"
    }
    private fun releaseUnusedGrants(previous: Set<String>) {
        val stillUsed = treeUris().map(Uri::toString).toSet()
        resolver.persistedUriPermissions.forEach { permission ->
            val uri = permission.uri.toString()
            if (uri in previous && uri !in stillUsed) {
                val flags = (if (permission.isReadPermission) Intent.FLAG_GRANT_READ_URI_PERMISSION else 0) or
                    (if (permission.isWritePermission) Intent.FLAG_GRANT_WRITE_URI_PERMISSION else 0)
                if (flags != 0) resolver.releasePersistableUriPermission(permission.uri, flags)
            }
        }
    }
    fun availableShares(): String {
        val selected = trees()
        val shares = JSONArray(knownShares())
        val available = JSONArray()
        for (i in 0 until shares.length()) {
            val id = shares.getJSONObject(i).getString("share_id")
            if (bindingState(boundTree(selected, id)) == BindingState.Bound) available.put(id)
        }
        return available.toString()
    }
    fun availableSharesForSync(focusJson: String): String {
        val focus = JSONArray(focusJson).let { array ->
            (0 until array.length()).map(array::getString).toSet()
        }
        val selected = trees()
        val shares = JSONArray(knownShares())
        val available = JSONArray()
        for (i in 0 until shares.length()) {
            val id = shares.getJSONObject(i).getString("share_id")
            val uri = boundTree(selected, id)
            if (uri.isNotEmpty() && (id !in focus || bindingState(uri) == BindingState.Bound))
                available.put(id)
        }
        // Non-focused bindings are only hints; selectShare revalidates the selected SAF tree.
        return available.toString()
    }
    fun unassignedShares(): String {
        val available = JSONArray(availableShares())
        val ids = (0 until available.length()).map { available.getString(it) }.toSet()
        val shares = JSONArray(knownShares())
        val selected = trees()
        val missing = JSONArray()
        for (i in 0 until shares.length()) {
            val share = shares.getJSONObject(i)
            if (share.getString("share_id") !in ids) missing.put(
                JSONObject().put("share_id", share.getString("share_id"))
                    .put("name", share.getString("name"))
                    .put("binding_state", bindingState(boundTree(selected, share.getString("share_id"))).name)
            )
        }
        return missing.toString()
    }
    fun treeUris(): List<Uri> {
        val values = mutableSetOf<String>()
        val selected = trees()
        val keys = selected.names() ?: JSONArray()
        for (i in 0 until keys.length()) boundTree(selected, keys.getString(i))
            .takeIf(String::isNotEmpty)?.let(values::add)
        val pending = JSONArray(pendingShareRequests())
        for (i in 0 until pending.length()) pending.getJSONObject(i).optString("tree")
            .takeIf(String::isNotEmpty)?.let(values::add)
        return values.map(Uri::parse)
    }
    fun observedShareTrees(): Map<Uri, String?> {
        val result = linkedMapOf<Uri, String?>()
        val selected = trees()
        val shares = JSONArray(knownShares())
        for (i in 0 until shares.length()) {
            val id = shares.getJSONObject(i).getString("share_id")
            boundTree(selected, id).takeIf(String::isNotEmpty)?.let { result[Uri.parse(it)] = id }
        }
        val pending = JSONArray(pendingShareRequests())
        for (i in 0 until pending.length()) {
            pending.getJSONObject(i).optString("tree").takeIf(String::isNotEmpty)
                ?.let { result.putIfAbsent(Uri.parse(it), null) }
        }
        return result
    }
    fun selectShare(id: String): String {
        check(id.matches(Regex("[0-9a-f]{64}"))) { "ID inválido" }
        val shares = JSONArray(knownShares())
        active = (0 until shares.length()).map { shares.getJSONObject(it) }.firstOrNull { it.getString("share_id") == id }
            ?: error("Share desconhecido")
        val bound = boundTree(trees(), id)
        check(bindingState(bound) == BindingState.Bound) { "Pasta Android indisponível: ${bindingState(bound)}" }
        selectedTree = Uri.parse(bound)
        val journal = File(context.filesDir, "shares/$id/journal.json")
        if (journal.exists()) check(journal.renameTo(File(journal.parentFile, "journal-retired-${System.currentTimeMillis()}.json"))) {
            "Não foi possível arquivar o journal antigo do Share."
        }
        return "ok"
    }
    private fun persistText(file: File, text: String) {
        file.parentFile?.mkdirs()
        val temp = File(file.parentFile, file.name + ".tmp")
        FileOutputStream(temp).use { it.write(text.toByteArray()); it.fd.sync() }
        check(temp.renameTo(file)) { "Não foi possível salvar o estado." }
    }
    private fun ignored(path: String, directory: Boolean, rules: List<String>): Boolean {
        if (path == "Rowd Conflicts" || path.startsWith("Rowd Conflicts/")) return false
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
        return (active?.optString("ignore_rules") ?: "").lines().map { it.trim() }.filter { it.isNotEmpty() && !it.startsWith('#') }
    }

    fun tempDirectory(): String = context.cacheDir.absolutePath

    private fun digest(input: InputStream, copy: java.io.OutputStream? = null, deadline: Long? = null): Pair<String, Long> {
        val md = MessageDigest.getInstance("SHA-256")
        var size = 0L
        val buffer = ByteArray(64 * 1024)
        input.use { stream ->
            while (true) {
                if (deadline != null) checkLocalDeadline(deadline)
                val count = stream.read(buffer)
                if (count < 0) break
                copy?.write(buffer, 0, count)
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
        for (part in parts(path)) {
            check(doc.isDirectory) { "Um arquivo ocupa o lugar da pasta: $path" }
            val matches = doc.listFiles().filter { it.name == part }
            check(matches.size <= 1) { "O provedor contém nomes duplicados: $part" }
            doc = matches.singleOrNull() ?: return null
        }
        check(doc.isFile) { "O caminho já é uma pasta: $path" }
        return doc
    }

    private fun findDirectory(path: String): DocumentFile? {
        var doc = root
        for (part in parts(path)) {
            check(doc.isDirectory) { "Um arquivo ocupa o lugar da pasta: $path" }
            val matches = doc.listFiles().filter { it.name == part }
            check(matches.size <= 1) { "O provedor contém nomes duplicados: $part" }
            doc = matches.singleOrNull() ?: return null
            check(doc.isDirectory) { "O caminho já é um arquivo: $path" }
        }
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

    private fun copyToPrivate(document: DocumentFile, file: File, deadline: Long? = null) {
        val input = traced("saf_open", null) { resolver.openInputStream(document.uri) ?: error("Não foi possível abrir ${document.name}") }
        input.use { source -> FileOutputStream(file).use { out ->
            val buffer = ByteArray(64 * 1024)
            traced("saf_copy", null) {
                while (true) {
                    if (deadline != null) checkLocalDeadline(deadline)
                    val count = source.read(buffer)
                    if (count < 0) break
                    out.write(buffer, 0, count)
                }
            }
            traced("snapshot_fsync", null) { out.fd.sync() }
        } }
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
            if (journal.optString("share_id") != id && journal.optString("share_id").isNotEmpty()) return@forEach
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
        val started = android.os.SystemClock.elapsedRealtime()
        val deadline = android.os.SystemClock.elapsedRealtime() + LOCAL_OP_TIMEOUT_MS
        checkLocalDeadline(deadline)
        recoverPending()
        checkLocalDeadline(deadline)
        val shareId = active?.getString("share_id") ?: error("Nenhum Share selecionado.")
        val tree = activeTree.toString()
        var deepAudit = false
        var hadTrustedCache = false
        val (previous, dirty, fullScan) = synchronized(scanLock) {
            val flagged = fullScanShares.remove(shareId)
            val full = !focusedScan || flagged
            val deep = deepScanShares.remove(shareId)
            deepAudit = deep
            hadTrustedCache = shareId in scanReady
            val changed = dirtyPaths.remove(shareId)?.toSet().orEmpty()
            Triple(if (deep) emptyMap() else scanCache[shareId].orEmpty(), changed, full)
        }
        val nextCache = HashMap<String, ScanEntry>()
        val nextUris = HashMap<String, String>()
        val nextDirectories = HashMap<String, String>()
        val ambiguousUris = HashSet<String>()
        val manifest = JSONObject()
        val rules = ignoreRules()
        var count = 0
        var enumerated = 0
        var hashed = 0
        var bytesHashed = 0L
        var cacheHits = 0
        fun rememberUri(uri: String, path: String) {
            if (uri !in ambiguousUris && nextUris.put(uri, path) != null) {
                nextUris.remove(uri)
                ambiguousUris.add(uri)
            }
        }
        fun walk(directory: DocumentFile, prefix: String) {
            checkLocalDeadline(deadline)
            nextDirectories[directory.uri.toString()] = prefix
            val children = directory.listFiles()
            val names = HashSet<String>()
            children.forEach { child ->
                checkLocalDeadline(deadline)
                val name = child.name ?: error("Arquivo sem nome no provedor.")
                check(names.add(name)) { "O provedor contém nomes duplicados: $name" }
                val path = if (prefix.isEmpty()) name else "$prefix/$name"
                if (ignored(path,child.isDirectory,rules)) return@forEach
                if (child.isDirectory) walk(child, path)
                else {
                    check(child.isFile && !child.isVirtual) { "Tipo de documento não suportado: $path" }
                    check(++count <= 100_000) { "Limite de 100 mil arquivos excedido." }
                    enumerated++
                    val uri = child.uri.toString()
                    val modified = child.lastModified()
                    val length = child.length()
                    val cached = previous[path]
                    val reuse = modified > 0 && path !in dirty && cached != null &&
                        cached.tree == tree && cached.uri == uri && cached.modified == modified &&
                        cached.length == length && cached.size == length
                    val (hash, size) = if (reuse) {
                        cacheHits++
                        cached!!.hash to cached.size
                    } else {
                        hashed++
                        digest(resolver.openInputStream(child.uri) ?: error("Sem acesso: $path"), deadline = deadline)
                    }
                    if (!reuse) bytesHashed += size
                    nextCache[path] = ScanEntry(tree, uri, modified, length, hash, size)
                    rememberUri(uri, path)
                    manifest.put(path, JSONObject().put("hash", hash).put("size", size))
                }
            }
        }
        // A namespace audit enumerates names but reuses verified hashes when provider metadata matches.
        var usedFocused = !fullScan && previous.isNotEmpty() && previous.values.first().tree == tree
        try {
            check(root.canRead() && root.canWrite()) { "A permissão de leitura/gravação da pasta foi revogada." }
            if (usedFocused) {
                nextCache.putAll(previous)
                for (path in dirty) {
                    checkLocalDeadline(deadline)
                    if (ignored(path, false, rules)) {
                        nextCache.remove(path)
                        continue
                    }
                    enumerated++
                    val document = try { find(path) } catch (_: IllegalStateException) {
                        usedFocused = false
                        break
                    }
                    if (document == null) {
                        nextCache.remove(path)
                        continue
                    }
                    check(!document.isVirtual) { "Tipo de documento não suportado: $path" }
                    val (hash, size) = digest(
                        resolver.openInputStream(document.uri) ?: error("Sem acesso: $path"),
                        deadline = deadline
                    )
                    hashed++
                    bytesHashed += size
                    nextCache[path] = ScanEntry(tree, document.uri.toString(), document.lastModified(),
                        document.length(), hash, size)
                }
            }
            if (usedFocused) {
                count = nextCache.size
                check(count <= 100_000) { "Limite de 100 mil arquivos excedido." }
                cacheHits = count - hashed
                nextCache.forEach { (path, entry) ->
                    rememberUri(entry.uri, path)
                    manifest.put(path, JSONObject().put("hash", entry.hash).put("size", entry.size))
                }
                nextDirectories.putAll(directoryUris[shareId].orEmpty())
            } else {
                nextCache.clear()
                enumerated = 0
                hashed = 0
                bytesHashed = 0
                walk(root, "")
            }
            synchronized(scanLock) {
                scanCache[shareId] = nextCache
                uriPaths[shareId] = nextUris
                directoryUris[shareId] = nextDirectories
                scanReady.add(shareId)
            }
        } catch (error: Exception) {
            synchronized(scanLock) {
                dirtyPaths.getOrPut(shareId) { mutableSetOf() }.addAll(dirty)
                deepScanShares.add(shareId)
            }
            throw error
        }
        val auditMode = if (deepAudit || !hadTrustedCache) "deep" else if (usedFocused) "focused" else "namespace"
        android.util.Log.i("Rowd", "SAF scan: mode=$auditMode files=$count enumerated=$enumerated hashed=$hashed cache_hits=$cacheHits ms=${android.os.SystemClock.elapsedRealtime() - started}")
        return JSONObject()
            .put("files", manifest)
            .put("enumerated", enumerated)
            .put("hashed", hashed)
            .put("bytes_hashed", bytesHashed)
            .put("full", !usedFocused)
            .put("audit_mode", auditMode)
            .toString()
    }

    fun snapshot(path: String): String {
        val started = android.os.SystemClock.elapsedRealtime()
        val traceStarted = PerformanceTrace.now()
        val share = active?.optString("share_id")
        PerformanceTrace.event("snapshot_start", share, path)
        val deadline = android.os.SystemClock.elapsedRealtime() + LOCAL_OP_TIMEOUT_MS
        checkLocalDeadline(deadline)
        check(!ignored(path,false,ignoreRules())) { "Caminho ignorado: $path" }
        val source = traced("saf_find", path) { find(path) } ?: error("STALE_SOURCE: $path")
        val staged = File.createTempFile("rowd-send-", ".part", context.cacheDir)
        try {
            val (hash, size) = FileOutputStream(staged).use { output ->
                val input = traced("saf_open", path) { resolver.openInputStream(source.uri) ?: error("STALE_SOURCE: $path") }
                val result = traced("saf_copy", path) { digest(input, output, deadline) }
                traced("snapshot_fsync", path) { output.fd.sync() }
                result
            }
            PerformanceTrace.event("snapshot_end", share, path, size, traceStarted)
            android.util.Log.i("RowdLatency", "snapshot_ms=${android.os.SystemClock.elapsedRealtime() - started} bytes=$size")
            return JSONObject().put("path", staged.absolutePath).put("hash", hash).put("size", size).toString()
        } catch (e: Exception) { staged.delete(); throw e }
    }

    fun install(path: String, expectedHash: String, newHash: String, sourcePath: String): String {
        val started = android.os.SystemClock.elapsedRealtime()
        val traceStarted = PerformanceTrace.now()
        val share = active?.optString("share_id")
        PerformanceTrace.event("install_start", share, path)
        check(!ignored(path,false,ignoreRules())) { "Caminho ignorado: $path" }
        active?.optString("share_id")?.takeIf(String::isNotEmpty)?.let { id ->
            synchronized(scanLock) { dirtyPaths.getOrPut(id) { mutableSetOf() }.add(path) }
        }
        val source = File(sourcePath)
        val old = traced("saf_find", path) { find(path) }
        val actual = traced("target_hash", path) { old?.let { hash(it) } ?: "" }
        if (actual == newHash) {
            check(digest(source.inputStream()).first == newHash) { "SHA-256 não confere." }
            android.util.Log.i("RowdLatency", "install_ms=${android.os.SystemClock.elapsedRealtime() - started} replay=true")
            PerformanceTrace.event("install_end", share, path, start = traceStarted)
            return "ok"
        }
        check(actual == expectedHash) { "STALE_TARGET: $path" }
        val id = UUID.randomUUID().toString()
        val incoming = File(recovery, "$id.new")
        val backup = File(recovery, "$id.old")
        val journalFile = File(recovery, "$id.json")
        try {
            FileOutputStream(incoming).use { out ->
                val (hash, _) = traced("incoming_copy", path) { digest(source.inputStream(), out) }
                check(hash == newHash) { "SHA-256 não confere." }
                traced("incoming_fsync", path) { out.fd.sync() }
            }
        } catch (error: Exception) {
            incoming.delete()
            throw error
        }
        if (old != null) {
            val backupHash = FileOutputStream(backup).use { output ->
                val result = traced("backup_copy", path) { digest(resolver.openInputStream(old.uri) ?: error("STALE_TARGET: $path"), output) }
                traced("backup_fsync", path) { output.fd.sync() }
                result.first
            }
            check(backupHash == expectedHash) { "STALE_TARGET: $path" }
        }
        val preparedMs = android.os.SystemClock.elapsedRealtime() - started
        val journal = JSONObject().put("path", path).put("tree", activeTree.toString())
            .put("share_id", active?.optString("share_id") ?: "").put("oldHash", expectedHash).put("newHash", newHash).put("finished", false)
        traced("journal_persist", path) { persist(journalFile, journal) }
        // SAF lacks atomic compare-and-replace. Recheck immediately and retain both snapshots.
        if (traced("target_recheck", path) { find(path)?.let { hash(it) } ?: "" } != expectedHash) {
            // No shared document was touched; this is an aborted operation, not a crash.
            journal.put("finished", true)
            traced("journal_persist", path) { persist(journalFile, journal) }
            error("STALE_TARGET: $path")
        }
        val (directory, name) = parent(path)
        val target = old ?: directory.createFile("application/octet-stream", name)
            ?: error("Não foi possível criar $path")
        check(target.name == name) { "O provedor alterou o nome do arquivo; sincronização interrompida." }
        traced("saf_write", path) { writeDocument(incoming, target) }
        check(traced("target_verify", path) { hash(target) } == newHash) { "Falha na gravação. Cópias preservadas para recuperação." }
        journal.put("finished", true)
        traced("journal_persist", path) { persist(journalFile, journal) }
        PerformanceTrace.event("install_end", share, path, start = traceStarted)
        android.util.Log.i("RowdLatency", "install_ms=${android.os.SystemClock.elapsedRealtime() - started} prepare_ms=$preparedMs")
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
                active = JSONObject().put("share_id",shareId)
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
