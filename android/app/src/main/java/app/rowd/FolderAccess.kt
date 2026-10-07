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
    private var multicastLock: android.net.wifi.WifiManager.MulticastLock? = null
    fun acquireMulticast(): String {
        releaseMulticast()
        val wifi = context.applicationContext.getSystemService(Context.WIFI_SERVICE) as android.net.wifi.WifiManager
        val lock = wifi.createMulticastLock("rowd-discovery").apply { setReferenceCounted(false); acquire() }
        multicastLock = lock
        return ""
    }
    fun releaseMulticast(): String {
        multicastLock?.release()
        multicastLock = null
        return ""
    }
    fun authenticatedAddress(address: String): String {
        context.getSharedPreferences("rowd", Context.MODE_PRIVATE).edit().putString("peerAddress", address).apply()
        return ""
    }
    private class AuditDeferred : RuntimeException("AUDIT_DEFERRED")
    companion object {
        private val stateLock = Any()
        // Updated by every FolderAccess instance after durable administrative writes.
        // The identity is deterministic, so PhysicalHashCache survives process restart.
        @Volatile private var liveBindingIdentities: Map<String, String>? = null
        private const val LOCAL_OP_TIMEOUT_MS = 30L * 60L * 1000L
    }

    private fun checkLocalDeadline(deadline: Long) {
        check(!Thread.currentThread().isInterrupted) { "Sincronização cancelada." }
        check(android.os.SystemClock.elapsedRealtime() < deadline) { "Operação SAF excedeu 30 minutos." }
    }

    private val resolver = context.contentResolver
    private fun traceFallback(name: String, share: String?, path: String?, operation: String,
        fallback: String, error: Exception, component: PerformanceTrace.Component = PerformanceTrace.Component.Scanner) {
        if (PerformanceTrace.enabled()) PerformanceTrace.event(name, share, path, component = component, level = "warn",
            detail = JSONObject().put("operation", operation).put("fallback", fallback)
                .put("error", PerformanceTrace.error(error, "filesystem", operation, false)), sourceFile = "FolderAccess.kt", sourceLine = 52)
    }
    private fun <T> traced(name: String, path: String?, fallback: String? = null, block: () -> T): T {
        if (!PerformanceTrace.enabled()) return block()
        val share = active?.optString("share_id")
        val start = PerformanceTrace.now()
        PerformanceTrace.event("${name}_start", share, path, sourceFile = "FolderAccess.kt", sourceLine = 60)
        return try { block() } catch (error: Exception) {
            PerformanceTrace.event("SAF_OPERATION_FAILED", share, path, component = PerformanceTrace.Component.SAF,
                level = if (fallback == null) "error" else "warn", detail = JSONObject()
                    .put("operation", name).put("fallback", fallback ?: JSONObject.NULL)
                    .put("error", PerformanceTrace.error(error, "filesystem", name, fallback == null)), sourceFile = "FolderAccess.kt", sourceLine = 62)
            throw error
        } finally { PerformanceTrace.event("${name}_end", share, path, start = start, sourceFile = "FolderAccess.kt", sourceLine = 67) }
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
    private val physicalHashes = PhysicalHashCache(File(context.filesDir, "physical-hashes"))
    private val scanLock = Any()
    private val scanCache = mutableMapOf<String, MutableMap<String, ScanEntry>>()
    private val uriPaths = mutableMapOf<String, MutableMap<String, String>>()
    private val directoryUris = mutableMapOf<String, Map<String, String>>()
    private val directoryPaths = mutableMapOf<String, Map<String, String>>()
    private val pendingUris = mutableMapOf<String, MutableSet<String>>()
    private val metadataDiffs = mutableMapOf<String, MetadataDiff>()
    private val dirtyDirectories = mutableMapOf<String, MutableSet<String>>()
    private val scanReady = mutableSetOf<String>()
    private val dirtyPaths = mutableMapOf<String, MutableSet<String>>()
    private val fullScanShares = mutableSetOf<String>()
    private val deepScanShares = mutableSetOf<String>()
    private val scheduledFullScanShares = mutableSetOf<String>()
    private val scheduledDeepScanShares = mutableSetOf<String>()
    private val scanAbort = java.util.concurrent.atomic.AtomicBoolean(false)
    private val scanWorker = ScanWorker()
    private val hashWorker = ScanStreamWorker { waiting ->
        PerformanceTrace.event(if (waiting) "PIPELINE_BACKPRESSURE_START" else "PIPELINE_BACKPRESSURE_END",
            active?.optString("share_id"), component = PerformanceTrace.Component.Scanner, sourceFile = "FolderAccess.kt", sourceLine = 100)
    }
    private data class NamespaceFile(val path: String, val uri: String, val modified: Long, val length: Long, val directory: Boolean)
    private data class StreamNamespace(val share: String, val tree: String, val binding: String, val generation: Long,
        val files: List<NamespaceFile>, val dirty: Set<String>, val deep: Boolean, val directories: Map<String, String>, val directoryPaths: Map<String, String>,
        val flaggedFull: Boolean, val scheduledFull: Boolean, val scheduledDeep: Boolean) {
        val byPath by lazy { files.associateBy { it.path } }
        val children by lazy { files.groupBy { it.path.substringBeforeLast('/', "") } }
    }
    private var streamNamespace: StreamNamespace? = null
    private val hashStaged = mutableMapOf<String, Pair<File, ScanEntry>>()
    private var hashStagedBinding = ""
    private var hashStagedBytes = 0L
    private var streamMetrics = JSONObject()
    private fun addStreamMetric(name: String, amount: Long = 1) = synchronized(scanLock) {
        streamMetrics.put(name, streamMetrics.optLong(name) + amount)
    }
    fun scanStreamMetricsJson(): String = synchronized(scanLock) { streamMetrics.toString() }
    private fun publishHashChunk(next: StreamNamespace, chunk: JSONObject, publish: (String) -> Unit) {
        val payload = chunk.toString()
        PerformanceTrace.event("HASH_CHUNK", next.share, component = PerformanceTrace.Component.Scanner,
            detail = JSONObject().put("entries", chunk.length()), sourceFile = "FolderAccess.kt", sourceLine = 121)
        publish(payload)
    }
    private data class PendingScan(val shareId: String, val tree: String, val binding: String,
        val cache: MutableMap<String, ScanEntry>, val uris: MutableMap<String, String>,
        val directories: Map<String, String>, val directoryPaths: Map<String, String>, val dirty: Set<String>,
        val flaggedFull: Boolean, val scheduledFull: Boolean,
        val deepAudit: Boolean, val scheduledDeep: Boolean)
    private var pendingScan: PendingScan? = null
    private var focusedScan = false
    private var roundIsAudit = false
    private var scanObservation = "unknown"
    private var scanSources = emptyMap<String, String>()
    fun setScanObservation(observedVia: String, sources: Map<String, String>) {
        scanObservation = observedVia; scanSources = sources
    }
    private fun observedVia(shareId: String) = scanSources[shareId] ?: scanObservation


    fun startScanJson(): String {
        active?.optString("share_id")?.let { traceMetadataDiff(it, emptySet(), "fallback") }
        check(!scanWorker.hasTask()) { "Scan SAF já em andamento." }
        scanAbort.set(false)
        discardScanJson()
        scanWorker.start { scanJson() }
        return "ok"
    }

    fun startDeltaScanJson(pathsJson: String): String {
        check(!scanWorker.hasTask()) { "Scan SAF já em andamento." }
        discardScanJson()
        scanAbort.set(false)
        scanWorker.start {
            val dirty = deltaPathsJson()
            if (dirty == "null") "null" else {
                val paths = linkedSetOf<String>()
                for (array in listOf(JSONArray(pathsJson), JSONArray(dirty))) {
                    for (index in 0 until array.length()) paths.add(array.getString(index))
                }
                if (paths.size > 1024) "null" else {
                    val requested = JSONArray(paths.toList())
                    val result = scanPathsJson(requested.toString())
                    if (result == "null") "null" else JSONObject(result).put("paths", requested).toString()
                }
            }
        }
        return "ok"
    }

    fun startNamespaceJson(): String {
        check(!scanWorker.hasTask()) { "Scan SAF já em andamento." }
        discardScanJson()
        scanAbort.set(false)
        // A new round owns no previous source snapshots. Clean crash orphans.
        context.cacheDir.listFiles()?.filter { it.name.startsWith("rowd-hash-") || it.name.startsWith("rowd-send-") }
            ?.forEach { it.delete() }
        scanWorker.start { namespaceJson() }
        return "ok"
    }

    private fun checkStream(next: StreamNamespace) {
        if (scanAbort.get() || active?.optString("share_id") != next.share || activeTree.toString() != next.tree ||
            bindingIdentity() != next.binding || SyncService.changeGeneration(next.share) != next.generation) throw AuditDeferred()
    }

    private fun namespaceJson(): String {
        val started = android.os.SystemClock.elapsedRealtime()
        recoverPending()
        checkSelectedBinding()
        val share = active?.getString("share_id") ?: error("Nenhum Share selecionado.")
        val binding = bindingIdentity()
        val tree = activeTree.toString()
        val generation = SyncService.changeGeneration(share)
        val rules = ignoreRules()
        val files = mutableListOf<NamespaceFile>()
        val directories = mutableMapOf<String, String>()
        val paths = mutableMapOf<String, String>()
        val next = synchronized(scanLock) {
            val dirty = dirtyPaths.remove(share)?.toSet().orEmpty()
            val full = fullScanShares.remove(share)
            val scheduledFull = scheduledFullScanShares.remove(share)
            val deep = deepScanShares.remove(share)
            val scheduledDeep = scheduledDeepScanShares.remove(share)
            StreamNamespace(share, tree, binding, generation, files, dirty, deep, directories, paths,
                full, scheduledFull, scheduledDeep).also { streamNamespace = it }
        }
        val uris = hashSetOf(root.uri.toString())
        var count = 0
        fun walk(directory: Uri, prefix: String) {
            checkStream(next)
            check(directories.put(directory.toString(), prefix) == null && paths.put(prefix, directory.toString()) == null) { "Índice de diretórios SAF ambíguo." }
            val deadline = android.os.SystemClock.elapsedRealtime() + LOCAL_OP_TIMEOUT_MS
            val children = safDirectoryMetadata(resolver, activeTree, directory, prefix) {
                checkStream(next); checkLocalDeadline(deadline)
            }
            for (child in children) {
                checkStream(next)
                if (ignored(child.path, child.directory, rules)) continue
                parts(child.path)
                check(uris.add(child.uri)) { "Associação URI/path ambígua: ${child.path}" }
                check(!child.virtual) { "Tipo de documento não suportado: ${child.path}" }
                if (!child.directory) check(++count <= 100_000) { "Limite de 100 mil arquivos excedido." }
                check(files.size < 200_000) { "Limite de namespace excedido." }
                files.add(NamespaceFile(child.path, child.uri, child.modified, if (child.directory) 0 else child.length, child.directory))
                if (child.directory) walk(Uri.parse(child.uri), child.path)
            }
        }
        try {
            PerformanceTrace.event("NAMESPACE_BEGIN", share, component = PerformanceTrace.Component.Scanner, sourceFile = "FolderAccess.kt", sourceLine = 230)
            check(root.canRead() && root.canWrite()) { "Permissão SAF revogada." }
            walk(root.uri, "")
            checkStream(next)
            val entries = JSONObject()
            files.sortedBy { it.path }.forEach { entry -> entries.put(entry.path, JSONObject()
                .put("size", entry.length).put("modified", entry.modified.coerceAtLeast(0)).put("directory", entry.directory)) }
            synchronized(scanLock) { streamNamespace = next }
            synchronized(scanLock) {
                streamMetrics = JSONObject().put("namespace_ms", android.os.SystemClock.elapsedRealtime() - started)
                    .put("hashes_reused", 0).put("files_staged_during_hash", 0).put("duplicate_reads_avoided", 0)
                    .put("saf_source_lookup_fallback_count", 0)
            }
            PerformanceTrace.event("NAMESPACE_END", share, component = PerformanceTrace.Component.Scanner, detail = streamMetrics, sourceFile = "FolderAccess.kt", sourceLine = 243)
            return entries.toString()
        } catch (error: AuditDeferred) { return JSONObject().put("deferred", true).toString() }
    }

    /** No directory traversal: validate the exact document and its complete ancestry. */
    private fun sourceDocument(next: StreamNamespace, entry: NamespaceFile, lookup: ScanPathLookup? = null): DocumentFile {
        checkStream(next)
        val uri = Uri.parse(entry.uri)
        val tree = Uri.parse(next.tree)
        val names = entry.path.split('/')
        check(DocumentsContract.isDocumentUri(context, uri) && uri.authority == tree.authority &&
            DocumentsContract.getTreeDocumentId(uri) == DocumentsContract.getTreeDocumentId(tree)) { "STALE_SOURCE: ${entry.path}" }
        val ancestry = try { DocumentsContract.findDocumentPath(resolver, uri)?.path }
            catch (_: UnsupportedOperationException) { null }
        if (ancestry == null) {
            addStreamMetric("saf_source_lookup_fallback_count")
            PerformanceTrace.event("SAF_SOURCE_LOOKUP_FALLBACK", next.share, entry.path, component = PerformanceTrace.Component.SAF,
                detail = JSONObject().put("reason", "provider_ancestry_unavailable"), sourceFile = "FolderAccess.kt", sourceLine = 260)
            val paths = lookup ?: ScanPathLookup(next.directoryPaths.getValue("")) { parent, prefix ->
                safDirectoryMetadata(resolver, tree, Uri.parse(parent), prefix) { checkStream(next) }
            }
            val found = paths.find(entry.path) ?: error("STALE_SOURCE: ${entry.path}")
            check(found.uri == entry.uri && !found.directory && !found.virtual) { "STALE_SOURCE: ${entry.path}" }
            val current = safDocumentMetadata(resolver, tree, uri, entry.path) { checkStream(next) }
            check(!current.directory && !current.virtual && current.modified == entry.modified && current.length == entry.length) { "STALE_SOURCE: ${entry.path}" }
            checkStream(next)
            return DocumentFile.fromSingleUri(context, uri) ?: error("STALE_SOURCE: ${entry.path}")
        }
        val parents = (0 until names.size).map { next.directoryPaths[names.take(it).joinToString("/")]
            ?: error("STALE_SOURCE: ${entry.path}") }
        check(ancestry.size == parents.size + 1 && ancestry.last() == DocumentsContract.getDocumentId(uri)) { "STALE_SOURCE: ${entry.path}" }
        parents.forEachIndexed { index, parent ->
            val parentUri = Uri.parse(parent)
            val parentMetadata = safDocumentMetadata(resolver, tree, parentUri, names.take(index).joinToString("/")) { checkStream(next) }
            check(ancestry[index] == DocumentsContract.getDocumentId(parentUri) && parentMetadata.directory && !parentMetadata.virtual) { "STALE_SOURCE: ${entry.path}" }
        }
        val doc = DocumentFile.fromSingleUri(context, uri) ?: error("STALE_SOURCE: ${entry.path}")
        val metadata = safDocumentMetadata(resolver, tree, uri, entry.path) { checkStream(next) }
        check(!metadata.directory && !metadata.virtual && metadata.modified == entry.modified && metadata.length == entry.length) { "STALE_SOURCE: ${entry.path}" }
        return doc
    }

    fun startHashStreamJson(hintsJson: String): String {
        val next = synchronized(scanLock) { streamNamespace } ?: error("Namespace SAF ausente.")
        checkStream(next)
        val hints = JSONArray(hintsJson).let { array -> (0 until array.length()).map { array.getString(it) }.toSet() }
        hashStagedBinding = next.binding
        hashWorker.start { publish ->
            val started = android.os.SystemClock.elapsedRealtime()
            val cache = mutableMapOf<String, ScanEntry>()
            val uris = mutableMapOf<String, String>()
            var chunk = JSONObject()
            var hashed = 0; var reused = 0; var bytes = 0L; var stagedCount = 0
            physicalHashes.select(next.share, next.binding)
            val sourceLookup = ScanPathLookup(next.directoryPaths.getValue("")) { uri, prefix ->
                val deadline = android.os.SystemClock.elapsedRealtime() + LOCAL_OP_TIMEOUT_MS
                safDirectoryMetadata(resolver, Uri.parse(next.tree), Uri.parse(uri), prefix) {
                    hashWorker.checkControl(); checkStream(next); checkLocalDeadline(deadline)
                }
            }
            try {
                PerformanceTrace.event("HASH_STREAM_BEGIN", next.share, component = PerformanceTrace.Component.Scanner, sourceFile = "FolderAccess.kt", sourceLine = 305)
                for (entry in next.files.filter { !it.directory }.sortedBy { it.path }) {
                    hashWorker.checkControl(); checkStream(next)
                    val doc = sourceDocument(next, entry, sourceLookup)
                    val cached = if (!next.deep) physicalHashes.lookup(entry.path, entry.uri, entry.modified, entry.length,
                        if (entry.path in next.dirty) next.generation else null) else null
                    var staging: File? = null
                    if (cached == null && entry.path in hints && entry.length <= 8L * 1024 * 1024) {
                        val waitStarted = android.os.SystemClock.elapsedRealtime()
                        var waiting = false
                        while (staging == null) {
                            hashWorker.checkControl(); checkStream(next)
                            staging = synchronized(scanLock) {
                                if (hashStaged.size < 4 && hashStagedBytes + entry.length <= 8L * 1024 * 1024) {
                                    File.createTempFile("rowd-hash-", ".part", context.cacheDir).also { hashStagedBytes += entry.length }
                                } else null
                            }
                            if (staging == null) {
                                if (chunk.length() > 0) { publishHashChunk(next, chunk, publish); chunk = JSONObject() }
                                if (!waiting) PerformanceTrace.event("PIPELINE_BACKPRESSURE_START", next.share, component = PerformanceTrace.Component.Scanner, sourceFile = "FolderAccess.kt", sourceLine = 324)
                                waiting = true
                                Thread.sleep(50)
                            }
                        }
                        if (waiting) {
                            addStreamMetric("queue_wait_ms", android.os.SystemClock.elapsedRealtime() - waitStarted)
                            PerformanceTrace.event("PIPELINE_BACKPRESSURE_END", next.share, component = PerformanceTrace.Component.Scanner, sourceFile = "FolderAccess.kt", sourceLine = 331)
                        }
                    }
                    val (hash, size) = try {
                        if (cached != null) { reused++; cached.hash to cached.size }
                        else {
                            physicalHashes.invalidate(entry.path)
                            val deadline = android.os.SystemClock.elapsedRealtime() + LOCAL_OP_TIMEOUT_MS
                            val input = resolver.openInputStream(doc.uri) ?: error("STALE_SOURCE: ${entry.path}")
                            val output = staging?.let(::FileOutputStream)
                            val result = try { scanDigest(input, output) { hashWorker.checkControl(); checkStream(next); checkLocalDeadline(deadline) } }
                                finally { output?.close() }
                            sourceDocument(next, entry, sourceLookup)
                            check(result.second == entry.length) { "STALE_SOURCE: ${entry.path}, inconsistent provider length" }
                            physicalHashes.remember(entry.path, PhysicalHashCache.Entry(entry.uri, entry.modified, entry.length, result.first, result.second, next.generation))
                            hashed++; bytes += result.second
                            result
                        }
                    } catch (error: Exception) {
                        staging?.delete()
                        if (staging != null) synchronized(scanLock) { hashStagedBytes -= entry.length }
                        throw error
                    }
                    val scanned = ScanEntry(next.tree, entry.uri, entry.modified, entry.length, hash, size)
                    cache[entry.path] = scanned; uris[entry.uri] = entry.path
                    staging?.let { file -> synchronized(scanLock) { hashStaged[entry.path] = file to scanned; stagedCount++ } }
                    PerformanceTrace.event(if (staging != null) "HASH_STAGED" else if (cached != null) "HASH_REUSED" else "FILE_HASH_END",
                        next.share, entry.path, size, component = PerformanceTrace.Component.Scanner, sourceFile = "FolderAccess.kt", sourceLine = 357)
                    chunk.put(entry.path, JSONObject().put("hash", hash).put("size", size))
                    if (hashed + reused == 1) synchronized(scanLock) {
                        streamMetrics.put("time_to_first_hash_ms", android.os.SystemClock.elapsedRealtime() - started)
                    }
                    savePhysicalHashes(next.share)
                    if (chunk.length() == 32) { publishHashChunk(next, chunk, publish); chunk = JSONObject() }
                }
                // Close structural evidence again. URI-based directory enumeration detects
                // unobserved create/delete/rename; cached hashes never establish existence.
                for ((uri, prefix) in next.directories) {
                    hashWorker.checkControl(); checkStream(next)
                    val deadline = android.os.SystemClock.elapsedRealtime() + LOCAL_OP_TIMEOUT_MS
                    val rules = ignoreRules()
                    val actual = safDirectorySnapshot(resolver, Uri.parse(next.tree), Uri.parse(uri), prefix,
                        ignored = { path, directory -> ignored(path, directory, rules) }) {
                        hashWorker.checkControl(); checkStream(next); checkLocalDeadline(deadline)
                    }
                    val expected = next.children[prefix].orEmpty().map { SafStructuralEntry(it.path, it.uri, it.directory) }
                    if (!safStructureMatches(expected, actual)) throw AuditDeferred()
                }
                for (entry in next.files.filter { !it.directory }) {
                    hashWorker.checkControl(); sourceDocument(next, entry, sourceLookup)
                }
                sourceLookup.validate(next.byPath.keys)
                checkStream(next)
                physicalHashes.retainPaths(cache.keys)
                synchronized(scanLock) {
                    pendingScan = PendingScan(next.share, next.tree, next.binding, cache, uris, next.directories, next.directoryPaths,
                        next.dirty, next.flaggedFull, next.scheduledFull, next.deep, next.scheduledDeep)
                }
                if (chunk.length() > 0) publishHashChunk(next, chunk, publish)
                synchronized(scanLock) {
                    streamMetrics.put("hashed", hashed).put("hashes_calculated", hashed).put("hashes_reused", reused).put("bytes_hashed", bytes)
                        .put("files_staged_during_hash", stagedCount)
                        .put("hash_stream_ms", android.os.SystemClock.elapsedRealtime() - started)
                }
            } finally { savePhysicalHashes(next.share, force = true) }
        }
        return "ok"
    }

    fun releaseHashStagingJson(pathsJson: String): String {
        val paths = JSONArray(pathsJson)
        synchronized(scanLock) {
            for (index in 0 until paths.length()) hashStaged.remove(paths.getString(index))?.let { (file, entry) ->
                file.delete(); hashStagedBytes -= entry.length
            }
        }
        return "ok"
    }

    fun pollHashStreamJson(): String = hashWorker.poll()
    fun finishHashStreamJson(): String {
        hashWorker.finish()
        val metrics = synchronized(scanLock) {
            streamMetrics.put("queue_peak_chunks", hashWorker.peakChunks)
                .put("queue_wait_ms", streamMetrics.optLong("queue_wait_ms") + hashWorker.waitMs)
            JSONObject(streamMetrics.toString())
        }
        PerformanceTrace.event("HASH_STREAM_END", active?.optString("share_id"), component = PerformanceTrace.Component.Scanner,
            detail = metrics, sourceFile = "FolderAccess.kt", sourceLine = 418)
        return metrics.toString()
    }

    fun pollScanJson(): String = scanWorker.poll()

    fun deferScanJson(): String {
        scanAbort.set(true)
        scanWorker.requestCancel()
        hashWorker.cancel()
        return "ok"
    }

    fun finishScanJson(): String { scanWorker.finish(); return "ok" }

    fun commitScanJson(): String = synchronized(scanLock) {
        val next = pendingScan ?: error("Nenhum scan SAF pronto para confirmar.")
        check(active?.optString("share_id") == next.shareId && activeTree.toString() == next.tree && bindingIdentity() == next.binding) {
            "Binding SAF mudou durante o scan."
        }
        streamNamespace?.let { checkStream(it) }
        scanCache[next.shareId] = next.cache
        uriPaths[next.shareId] = next.uris
        directoryUris[next.shareId] = next.directories
        directoryPaths[next.shareId] = next.directoryPaths
        scanReady.add(next.shareId)
        if (streamNamespace != null) {
            // Flags were consumed at namespace start; requests arriving during
            // the worker must survive for the next round.
            streamNamespace = null
        }
        pendingScan = null
        "ok"
    }

    fun discardScanJson(): String {
        hashWorker.cancel()
        hashWorker.finish()
        return synchronized(scanLock) {
        hashStaged.values.forEach { it.first.delete() }
        hashStaged.clear(); hashStagedBytes = 0; hashStagedBinding = ""
        if (pendingScan == null) streamNamespace?.let { scan ->
            dirtyPaths.getOrPut(scan.share) { mutableSetOf() }.addAll(scan.dirty)
            restoreScanFlags(scan.share, scan.flaggedFull, scan.scheduledFull, scan.deep, scan.scheduledDeep)
        }
        streamNamespace = null
        pendingScan?.let { scan ->
            dirtyPaths.getOrPut(scan.shareId) { mutableSetOf() }.addAll(scan.dirty)
            restoreScanFlags(scan.shareId, scan.flaggedFull, scan.scheduledFull, scan.deepAudit, scan.scheduledDeep)
        }
        pendingScan = null
        "ok"
        }
    }

    fun setFocusedScan(focused: Boolean) { focusedScan = focused }
    private fun restoreScanFlags(share: String, full: Boolean, scheduledFull: Boolean, deep: Boolean, scheduledDeep: Boolean) {
        if (full) fullScanShares.add(share)
        if (scheduledFull) scheduledFullScanShares.add(share)
        if (deep) deepScanShares.add(share)
        if (scheduledDeep) scheduledDeepScanShares.add(share)
    }
    fun setAuditRound(audit: Boolean) { roundIsAudit = audit }
    fun auditRound(): String = roundIsAudit.toString()

    private fun definitionIdentity(share: JSONObject, tree: String): String {
        val policy = share.optString("ignore_rules").lines().map { it.trim() }
            .filter { it.isNotEmpty() && !it.startsWith('#') }.joinToString("\n")
        return "$tree|${share.optLong("binding_revision", 0L)}|$policy|${share.optString("mode")}|${share.optBoolean("enabled", true)}"
    }
    private fun updateBindingIdentities(shares: JSONArray, bindings: JSONObject) {
        liveBindingIdentities = (0 until shares.length()).associate { index ->
            val share = shares.getJSONObject(index)
            val id = share.getString("share_id")
            id to definitionIdentity(share, boundTree(bindings, id))
        }
    }
    fun bindingIdentity(): String {
        val share = active ?: error("Nenhum Share selecionado.")
        val id = share.getString("share_id")
        return liveBindingIdentities?.get(id) ?: if (liveBindingIdentities != null) "revoked|$id"
            else definitionIdentity(share, activeTree.toString())
    }
    private fun checkSelectedBinding(kind: String = "STALE_SOURCE") {
        val share = active ?: error("Nenhum Share selecionado.")
        check(bindingIdentity() == definitionIdentity(share, activeTree.toString())) { "$kind: binding SAF mudou." }
    }

    fun forceFullScan(): String {
        synchronized(scanLock) { fullScanShares.add(active?.getString("share_id") ?: error("Nenhum Share selecionado.")) }
        return "ok"
    }
    fun forceDeepScan(): String {
        synchronized(scanLock) { deepScanShares.add(active?.getString("share_id") ?: error("Nenhum Share selecionado.")) }
        return "ok"
    }
    fun scheduleAudit(shareId: String, deep: Boolean) {
        synchronized(scanLock) {
            if (shareId !in fullScanShares) scheduledFullScanShares.add(shareId)
            fullScanShares.add(shareId)
            if (deep) {
                if (shareId !in deepScanShares) scheduledDeepScanShares.add(shareId)
                deepScanShares.add(shareId)
            }
        }
        PerformanceTrace.event(if (deep) "DEEP_AUDIT_START" else "AUDIT_SCHEDULED", shareId, component = PerformanceTrace.Component.Scanner, sourceFile = "FolderAccess.kt", sourceLine = 524)
    }

    fun deltaPathsJson(): String {
        val id = active?.getString("share_id") ?: return "null"
        val tree = activeTree
        val binding = bindingIdentity()
        val generation = SyncService.changeGeneration(id)
        fun checkControl() {
            if (scanAbort.get() || active?.optString("share_id") != id || activeTree != tree ||
                bindingIdentity() != binding || SyncService.changeGeneration(id) != generation) throw AuditDeferred()
        }
        fun unavailable(reason: String): String {
            if (PerformanceTrace.enabled()) synchronized(scanLock) {
                PerformanceTrace.event("delta_unavailable", id, component = PerformanceTrace.Component.Scanner,
                    detail = JSONObject().put("reason", reason).put("focused", focusedScan)
                        .put("fullScan", id in fullScanShares).put("deepScan", id in deepScanShares)
                        .put("scanReady", id in scanReady).put("cache_size", scanCache[id]?.size ?: 0)
                        .put("pendingUris_count", pendingUris[id]?.size ?: 0)
                        .put("dirtyDirectories_count", dirtyDirectories[id]?.size ?: 0)
                        .put("dirtyPaths_count", dirtyPaths[id]?.size ?: 0), sourceFile = "FolderAccess.kt", sourceLine = 538)
            }
            return "null"
        }
        val (sourceCache, cached, prefixes) = synchronized(scanLock) {
            checkControl()
            val cache = scanCache[id] ?: return unavailable("cache_missing")
            if (!focusedScan) return unavailable("not_focused")
            if (id in fullScanShares) return unavailable("full_scan_flagged")
            if (id in deepScanShares) return unavailable("deep_scan_flagged")
            if (id !in scanReady) return unavailable("scan_not_ready")
            if (cache.values.any { it.tree != tree.toString() }) return unavailable("tree_mismatch")
            val hinted = dirtyDirectories.remove(id).orEmpty().toMutableSet()
            // Unknown URI batches require one metadata discovery, not URI × directory lookups.
            if (!pendingUris.remove(id).isNullOrEmpty()) hinted.add("")
            Triple(cache, cache.toMap(), metadataRoots(hinted))
        }
        if (prefixes.isEmpty()) return synchronized(scanLock) { JSONArray(dirtyPaths[id].orEmpty().toList()).toString() }
        val rules = ignoreRules()
        val deadline = android.os.SystemClock.elapsedRealtime() + LOCAL_OP_TIMEOUT_MS
        fun control() { checkControl(); checkLocalDeadline(deadline) }
        val diff = MetadataDiff(cached.mapValues { (_, entry) ->
            DocumentMetadata(entry.uri, entry.modified, entry.length)
        }, android.os.SystemClock.elapsedRealtime())
        synchronized(scanLock) { metadataDiffs[id] = diff }
        val (directories, paths) = synchronized(scanLock) {
            directoryUris[id].orEmpty().toMutableMap() to directoryPaths[id].orEmpty().toMutableMap()
        }
        val seenUris = mutableSetOf<String>()
        try {
            val lookup = ScanPathLookup(root.uri.toString()) { uri, prefix ->
                safDirectoryMetadata(resolver, tree, Uri.parse(uri), prefix, ::control)
            }
            for (prefix in prefixes) {
                control()
                if (prefix.isNotEmpty()) parts(prefix)
                // Rebuild this subtree's index so vanished directories cannot survive discovery.
                paths.keys.filter { prefix.isEmpty() || it == prefix || it.startsWith("$prefix/") }.forEach { old ->
                    paths.remove(old)?.let { directories.remove(it) }
                }
                fun walk(directory: String, path: String) {
                    control()
                    check(seenUris.add(directory)) { "Índice de diretórios SAF ambíguo." }
                    check(paths.size < 100_000) { "Limite de diretórios SAF excedido." }
                    diff.directory()
                    directories[directory] = path; paths[path] = directory
                    for (entry in lookup.children(path).values) {
                        control()
                        diff.entry()
                        check(diff.entriesEnumerated <= 200_000) { "Limite de descoberta SAF excedido." }
                        if (ignored(entry.path, entry.directory, rules)) continue
                        parts(entry.path)
                        check(!entry.virtual) { "Documento virtual: ${entry.path}" }
                        if (entry.directory) walk(entry.uri, entry.path) else {
                            check(seenUris.add(entry.uri)) { "URI reutilizada: ${entry.path}" }
                            diff.file(entry.path, entry.documentMetadata())
                        }
                    }
                }
                val directory = lookup.directory(prefix)
                if (directory != null) walk(directory, prefix)
                diff.finish(prefix)
            }
            synchronized(scanLock) {
                control()
                check(scanCache[id] === sourceCache && id !in fullScanShares && id !in deepScanShares) { "Cache de scan mudou." }
                check(directories.size == paths.size && directories.all { (uri, path) -> paths[path] == uri }) { "Índice de diretórios SAF ambíguo." }
                directoryUris[id] = directories; directoryPaths[id] = paths
                dirtyPaths.getOrPut(id) { mutableSetOf() }.addAll(diff.changed)
                if (dirtyPaths[id].orEmpty().size > 1024) {
                    traceMetadataDiff(id, emptySet(), "fallback")
                    return unavailable("too_many_dirty_paths")
                }
                return JSONArray(dirtyPaths[id].orEmpty().toList()).toString()
            }
        } catch (error: AuditDeferred) {
            synchronized(scanLock) { fullScanShares.add(id) }
            throw error
        } catch (error: Exception) {
            traceMetadataDiff(id, emptySet(), "fallback")
            traceFallback("DELTA_SCAN_FAILED", id, null, "resolve_dirty_paths", "deep_scan", error)
            synchronized(scanLock) { deepScanShares.add(id) }
            return unavailable("resolution_error")
        }
    }

    private fun traceMetadataDiff(id: String, hashed: Set<String>, result: String) = synchronized(scanLock) {
        val diff = metadataDiffs.remove(id) ?: return@synchronized
        PerformanceTrace.event("PROVIDER_METADATA_DIFF", id, component = PerformanceTrace.Component.Scanner,
            detail = JSONObject().put("directories_visited", diff.directoriesVisited)
                .put("entries_enumerated", diff.entriesEnumerated).put("metadata_changed", diff.changed.size)
                .put("paths_hashed", diff.pathsHashed(hashed))
                .put("duration_ms", android.os.SystemClock.elapsedRealtime() - diff.startedAt)
                .put("strategy", "recursive_metadata_diff").put("result", result), sourceFile = "FolderAccess.kt", sourceLine = 632)
    }

    fun scanPathsJson(pathsJson: String): String {
        val id = active?.getString("share_id") ?: return "null"
        checkSelectedBinding()
        val tree = activeTree.toString()
        val binding = bindingIdentity()
        val hashedPaths = mutableSetOf<String>()
        try {
            fun needDeepScan(): String {
                synchronized(scanLock) { deepScanShares.add(id) }
                traceMetadataDiff(id, hashedPaths, "fallback")
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
            fun checkBinding() {
                if (scanAbort.get()) throw AuditDeferred()
                checkLocalDeadline(deadline)
                check(active?.optString("share_id") == id && activeTree.toString() == tree && bindingIdentity() == binding) {
                    "STALE_SOURCE: binding SAF mudou durante o scan focado."
                }
            }
            val requested = (0 until paths.length()).map { paths.getString(it) }.toSet()
            val rules = ignoreRules()
            val lookup = ScanPathLookup(root.uri.toString()) { uri, prefix ->
                safDirectoryMetadata(resolver, activeTree, Uri.parse(uri), prefix, ::checkBinding)
            }
            for (path in requested) {
                parts(path)
                checkBinding()
                if (cache[path]?.tree != null && cache[path]?.tree != activeTree.toString()) return needDeepScan()
                if (ignored(path, false, rules)) return needDeepScan()
                val document = try { lookup.find(path) } catch (error: AuditDeferred) { throw error }
                    catch (error: Exception) {
                        traceFallback("DELTA_SCAN_FAILED", id, path, "find", "deep_scan", error)
                        return needDeepScan()
                    }
                if (document == null) continue
                if (document.directory || document.virtual) return needDeepScan()
                val (metadata, result) = try {
                    val before = document.documentMetadata()
                    val result = verifiedScanDigest(resolver.openInputStream(Uri.parse(document.uri)) ?: return needDeepScan(), before,
                        after = {
                            val current = safDocumentMetadata(resolver, activeTree, Uri.parse(document.uri), path, ::checkBinding)
                            check(!current.directory && !current.virtual) { "STALE_SOURCE: $path" }
                            current.documentMetadata()
                        }, checkControl = ::checkBinding)
                    before to result
                } catch (error: AuditDeferred) {
                    throw error
                } catch (error: Exception) {
                    traceFallback("DELTA_SCAN_FAILED", id, path, "digest", "deep_scan", error)
                    return needDeepScan()
                }
                val (hash, size) = result
                hashedPaths.add(path)
                bytesHashed += size
                enumerated++
                PerformanceTrace.firstSeen(id, path, metadata.modified, observedVia(id), hash)
                updates[path] = ScanEntry(tree, metadata.uri, metadata.modified,
                    metadata.length, hash, size)
                files.put(path, JSONObject().put("hash", hash).put("size", size))
            }
            try { lookup.validate(requested) } catch (error: AuditDeferred) { throw error }
                catch (error: Exception) {
                    traceFallback("DELTA_SCAN_FAILED", id, null, "validate_paths", "deep_scan", error)
                    return needDeepScan()
                }
            synchronized(scanLock) {
                checkBinding()
                if (id in fullScanShares || id in deepScanShares || scanCache[id] !== cache) return "null"
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
            traceMetadataDiff(id, hashedPaths, "success")
            return JSONObject().put("files", files).put("enumerated", enumerated)
                .put("hashed", enumerated).put("bytes_hashed", bytesHashed).toString()
        } finally {
            traceMetadataDiff(id, hashedPaths, "fallback")
        }
    }

    /** Generic callbacks request metadata discovery, never exact-URI lookup. */
    internal fun noteChange(shareId: String?, changedUri: Uri?, selfChange: Boolean = false): ObserverHint? = synchronized(scanLock) {
        if (shareId == null) return@synchronized null // pending binding; audited by the normal clock
        val tree = boundTree(trees(), shareId).takeIf(String::isNotEmpty)?.let(Uri::parse)
            ?: return@synchronized null
        val text = changedUri?.toString()
        val path = text?.let { uriPaths[shareId]?.get(it) }
        val directory = text?.let { directoryUris[shareId]?.get(it) }
        val hint = observerHint(text, changedUri?.authority, tree.authority, changedUri?.pathSegments.orEmpty(),
            path != null, directory != null,
            runCatching { DocumentsContract.getTreeDocumentId(tree) }.getOrNull(),
            changedUri?.let { runCatching { DocumentsContract.getDocumentId(it) }.getOrNull() })
        PerformanceTrace.event("OBSERVER_CHANGE_CLASSIFIED", shareId, path, component = PerformanceTrace.Component.Watcher,
            detail = JSONObject().put("classification", hint.name.lowercase()).put("self_change", selfChange)
                .put("strategy", if (hint == ObserverHint.PROVIDER_WIDE_URI || hint == ObserverHint.NULL_URI) "metadata_diff" else "focused"), sourceFile = "FolderAccess.kt", sourceLine = 751)
        when (hint) {
            ObserverHint.UNRELATED_URI -> return@synchronized null
            ObserverHint.KNOWN_FILE_URI -> dirtyPaths.getOrPut(shareId) { mutableSetOf() }.add(path!!)
            ObserverHint.KNOWN_DIRECTORY_URI -> dirtyDirectories.getOrPut(shareId) { mutableSetOf() }.add(directory!!)
            ObserverHint.PROVIDER_WIDE_URI, ObserverHint.NULL_URI -> dirtyDirectories.getOrPut(shareId) { mutableSetOf() }.add("")
            ObserverHint.UNKNOWN_SPECIFIC_URI -> pendingUris.getOrPut(shareId) { mutableSetOf() }.add(text!!)
        }
        hint
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
        .getBoolean("unlinkRequested", false).toString()

    fun confirmUnlinked(): String = synchronized(stateLock) {
        check(context.getSharedPreferences("rowd", Context.MODE_PRIVATE).edit()
            .remove("invitation").remove("peerAddress").remove("unlinkRequested").remove("unlinkPrepared")
            .remove("lastStatus").remove("lastDetail").remove("lastStatusKind").commit()) {
            "Não foi possível concluir a desvinculação local."
        }
        active = null
        selectedTree = null
        recoveryTree = null
        synchronized(scanLock) {
            scanCache.clear(); uriPaths.clear(); directoryUris.clear(); directoryPaths.clear(); pendingUris.clear()
            metadataDiffs.clear(); dirtyDirectories.clear(); scanReady.clear(); dirtyPaths.clear()
            fullScanShares.clear(); deepScanShares.clear()
            scheduledFullScanShares.clear()
            scheduledDeepScanShares.clear()
        }
        "ok"
    }

    fun resetConfiguration(): String = synchronized(stateLock) {
        val oldUris = treeUris().map(Uri::toString).toSet()
        val shareState = File(context.filesDir, "shares")
        if (shareState.exists()) {
            val archive = File(context.filesDir, "state-archives").apply { mkdirs() }
            check(shareState.renameTo(File(archive, "shares-before-reset-${System.currentTimeMillis()}"))) {
                "Não foi possível preservar o estado anterior dos Shares."
            }
        }
        definitions.delete()
        shareBindings.delete()
        legacyTrees.delete()
        requests.delete()
        requestResults.delete()
        releaseUnusedGrants(oldUris)
        confirmUnlinked()
    }

    fun configureShares(json: String): String = synchronized(stateLock) {
        val started = android.os.SystemClock.elapsedRealtime()
        val oldUris = treeUris().map(Uri::toString).toSet()
        val previous = JSONArray(knownShares())
        val shares = JSONArray(json)
        PerformanceTrace.registerShares(shares)
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
        updateBindingIdentities(shares, selectedTrees)
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
            directoryPaths.keys.retainAll(currentIds - resetCacheIds)
            pendingUris.keys.retainAll(currentIds - resetCacheIds)
            metadataDiffs.keys.retainAll(currentIds - resetCacheIds)
            dirtyDirectories.keys.retainAll(currentIds - resetCacheIds)
            scanReady.retainAll(currentIds - resetCacheIds)
            dirtyPaths.keys.retainAll(currentIds - resetCacheIds)
            fullScanShares.retainAll(currentIds - resetCacheIds)
            scheduledFullScanShares.retainAll(currentIds - resetCacheIds)
            deepScanShares.retainAll(currentIds)
            scheduledDeepScanShares.retainAll(currentIds - resetCacheIds)
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
        updateBindingIdentities(shares, selected)
        synchronized(scanLock) {
            scanCache.remove(id); uriPaths.remove(id); directoryUris.remove(id); directoryPaths.remove(id)
            metadataDiffs.remove(id); pendingUris.remove(id); dirtyDirectories.remove(id); scanReady.remove(id)
            dirtyPaths.remove(id); deepScanShares.add(id)
            scheduledFullScanShares.remove(id)
            scheduledDeepScanShares.remove(id)
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
        PerformanceTrace.registerShares(shares)
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
        check(!scanWorker.hasTask() && !hashWorker.hasTask() && synchronized(scanLock) { streamNamespace == null }) {
            "Scan SAF deve terminar antes de selecionar outra Share."
        }
        check(id.matches(Regex("[0-9a-f]{64}"))) { "ID inválido" }
        synchronized(stateLock) {
            val shares = JSONArray(knownShares())
            val bindings = trees()
            active = (0 until shares.length()).map { shares.getJSONObject(it) }.firstOrNull { it.getString("share_id") == id }
                ?: error("Share desconhecido")
            val bound = boundTree(bindings, id)
            check(bindingState(bound) == BindingState.Bound) { "Pasta Android indisponível: ${bindingState(bound)}" }
            selectedTree = Uri.parse(bound)
            updateBindingIdentities(shares, bindings)
        }
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

    private fun scanSourceMetadata(document: DocumentFile, path: String): DocumentMetadata {
        val metadata = safDocumentMetadata(resolver, activeTree, document.uri, path)
        check(!metadata.directory && !metadata.virtual) { "STALE_SOURCE: identidade ou tipo de documento mudou: $path" }
        return metadata.documentMetadata()
    }

    private fun savePhysicalHashes(share: String, force: Boolean = false) {
        physicalHashes.saveIfDue(force)?.let { error ->
            traceFallback("HASH_CACHE_SAVE_FAILED", share, null, "hash_cache_save", "memory_cache", error)
        }
    }

    private fun digest(input: InputStream, copy: java.io.OutputStream? = null, deadline: Long? = null): Pair<String, Long> {
        return scanDigest(input, copy) {
            if (deadline != null) checkLocalDeadline(deadline)
        }
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

    private data class ResolvedTarget(
        val directory: DocumentFile, val missingDirectories: List<String>,
        val name: String, val target: DocumentFile?
    )

    private fun resolveTarget(path: String): ResolvedTarget {
        val segments = parts(path)
        var directory = root
        for ((index, part) in segments.dropLast(1).withIndex()) {
            check(directory.isDirectory) { "Um arquivo ocupa o lugar da pasta: $path" }
            val matches = directory.listFiles().filter { it.name == part }
            check(matches.size <= 1) { "O provedor contém nomes duplicados: $part" }
            val child = matches.singleOrNull()
                ?: return ResolvedTarget(directory, segments.subList(index, segments.size - 1), segments.last(), null)
            check(child.isDirectory) { "Um arquivo ocupa o lugar da pasta: $path" }
            directory = child
        }
        check(directory.isDirectory) { "Um arquivo ocupa o lugar da pasta: $path" }
        val name = segments.last()
        val matches = directory.listFiles().filter { it.name == name }
        check(matches.size <= 1) { "O provedor contém nomes duplicados: $name" }
        val target = matches.singleOrNull()
        if (target != null) check(target.isFile) { "O caminho já é uma pasta: $path" }
        return ResolvedTarget(directory, emptyList(), name, target)
    }

    private fun cachedTarget(path: String, shareId: String): DocumentFile? {
        val treeUri = activeTree
        val entry = synchronized(scanLock) {
            if (shareId !in scanReady || shareId in fullScanShares || shareId in deepScanShares) null
            else scanCache[shareId]?.get(path)?.takeIf {
                it.tree == treeUri.toString() && uriPaths[shareId]?.get(it.uri) == path
            }
        } ?: return null
        return try {
            val uri = Uri.parse(entry.uri)
            if (!DocumentsContract.isDocumentUri(context, uri) ||
                DocumentsContract.getTreeDocumentId(uri) != DocumentsContract.getTreeDocumentId(treeUri)) return null
            DocumentFile.fromSingleUri(context, uri)?.takeIf {
                it.uri == uri && it.name == path.substringAfterLast('/') && it.isFile && !it.isVirtual
            }
        } catch (error: Exception) {
            traceFallback("INSTALL_CACHE_LOOKUP_FAILED", shareId, path, "cached_target", "resolve_target", error, PerformanceTrace.Component.Filesystem)
            null
        }
    }

    private fun cachedResolvedTarget(path: String, shareId: String): ResolvedTarget? {
        val parentPath = path.substringBeforeLast('/', "")
        val parentNames = if (parentPath.isEmpty()) emptyList() else parentPath.split('/')
        val name = path.substringAfterLast('/')
        val treeUri = activeTree
        val pair = synchronized(scanLock) {
            val dirty = dirtyDirectories[shareId].orEmpty()
            if (shareId !in scanReady || shareId in fullScanShares || shareId in deepScanShares ||
                scanWorker.hasTask() || pendingScan?.shareId == shareId ||
                !pendingUris[shareId].isNullOrEmpty() ||
                dirty.any { it.isEmpty() || parentPath == it || parentPath.startsWith("$it/") }) null
            else {
                val parentUri = directoryPaths[shareId]?.get(parentPath)
                val target = scanCache[shareId]?.get(path)
                if (parentUri == null || directoryUris[shareId]?.get(parentUri) != parentPath ||
                    target == null || target.tree != treeUri.toString() ||
                    uriPaths[shareId]?.get(target.uri) != path) null
                else parentUri to target.uri
            }
        } ?: return null
        return try {
            val parentUri = Uri.parse(pair.first)
            val targetUri = Uri.parse(pair.second)
            val treeId = DocumentsContract.getTreeDocumentId(treeUri)
            if (!DocumentsContract.isDocumentUri(context, parentUri) ||
                !DocumentsContract.isDocumentUri(context, targetUri) ||
                parentUri.authority != treeUri.authority || targetUri.authority != treeUri.authority ||
                DocumentsContract.getTreeDocumentId(parentUri) != treeId ||
                DocumentsContract.getTreeDocumentId(targetUri) != treeId) return null
            val parentId = DocumentsContract.getDocumentId(parentUri)
            val targetId = DocumentsContract.getDocumentId(targetUri)
            val documentPath = DocumentsContract.findDocumentPath(resolver, targetUri)?.path ?: return null
            val ancestors = synchronized(scanLock) {
                val paths = directoryPaths[shareId].orEmpty()
                (0..parentNames.size).map { count ->
                    paths[parentNames.take(count).joinToString("/")]
                }
            }
            if (documentPath.size != ancestors.size + 1 || documentPath.last() != targetId ||
                documentPath[documentPath.size - 2] != parentId) return null
            val ancestorDocs = ancestors.mapIndexed { index, cached ->
                val uri = cached?.let(Uri::parse) ?: return null
                if (!DocumentsContract.isDocumentUri(context, uri) || uri.authority != treeUri.authority ||
                    DocumentsContract.getTreeDocumentId(uri) != treeId ||
                    DocumentsContract.getDocumentId(uri) != documentPath[index]) return null
                val doc = if (index == 0) root else DocumentFile.fromSingleUri(context, uri)
                if (doc == null || doc.uri != uri || !doc.isDirectory ||
                    (index > 0 && doc.name != parentNames[index - 1])) return null
                doc
            }
            val parent = ancestorDocs.last()
            val target = DocumentFile.fromSingleUri(context, targetUri)
            if (parent.uri != parentUri || target == null || target.uri != targetUri || target.name != name ||
                !target.isFile || target.isVirtual) null
            else ResolvedTarget(parent, emptyList(), name, target)
        } catch (error: Exception) {
            traceFallback("INSTALL_CACHE_LOOKUP_FAILED", shareId, path, "cached_resolved_target", "resolve_target", error, PerformanceTrace.Component.Filesystem)
            null
        }
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

    fun traceSnapshot(): JSONObject = synchronized(scanLock) {
        JSONObject().put("selected_share", active?.optString("share_id") ?: JSONObject.NULL)
            .put("pending_uri_count", pendingUris.values.sumOf { it.size })
    }

    fun scanJson(): String {
        val started = android.os.SystemClock.elapsedRealtime()
        val deadline = android.os.SystemClock.elapsedRealtime() + LOCAL_OP_TIMEOUT_MS
        checkLocalDeadline(deadline)
        recoverPending()
        checkLocalDeadline(deadline)
        val shareId = active?.getString("share_id") ?: error("Nenhum Share selecionado.")
        val changeGeneration = SyncService.changeGeneration(shareId)
        val tree = activeTree.toString()
        val binding = bindingIdentity()
        physicalHashes.select(shareId, binding)
        var deepAudit = false
        var flaggedFull = false
        var scheduledFull = false
        var scheduledDeep = false
        var hadTrustedCache = false
        var auditScan = false
        val (previous, dirty, fullScan) = synchronized(scanLock) {
            val flagged = fullScanShares.remove(shareId)
            flaggedFull = flagged
            scheduledFull = scheduledFullScanShares.remove(shareId)
            val full = !focusedScan || flagged
            val deep = deepScanShares.remove(shareId)
            scheduledDeep = scheduledDeepScanShares.remove(shareId)
            auditScan = !focusedScan || flagged || deep
            deepAudit = deep
            hadTrustedCache = shareId in scanReady
            val changed = dirtyPaths.remove(shareId)?.toSet().orEmpty()
            Triple(if (deep) emptyMap() else scanCache[shareId].orEmpty(), changed, full)
        }
        if (!focusedScan) PerformanceTrace.event("AUDIT_START", shareId, component = PerformanceTrace.Component.Scanner, sourceFile = "FolderAccess.kt", sourceLine = 1452)
        val nextCache = HashMap<String, ScanEntry>()
        val nextUris = HashMap<String, String>()
        val nextDirectories = HashMap<String, String>()
        val nextDirectoryPaths = HashMap<String, String>()
        val ambiguousUris = HashSet<String>()
        val manifest = JSONObject()
        val rules = ignoreRules()
        var count = 0
        var enumerated = 0
        var hashed = 0
        var bytesHashed = 0L
        var cacheHits = 0
        fun checkScanControl() {
            check(!Thread.currentThread().isInterrupted) { "Sincronização cancelada." }
            if (scanShouldAbort(scanAbort.get(), auditScan, SyncService.changeGeneration(shareId) != changeGeneration) ||
                active?.optString("share_id") != shareId || bindingIdentity() != binding) throw AuditDeferred()
        }
        fun hashForScan(document: DocumentFile, path: String, modified: Long, length: Long): Pair<String, Long> {
            val hashGeneration = SyncService.changeGeneration(shareId)
            val hashDeadline = android.os.SystemClock.elapsedRealtime() + LOCAL_OP_TIMEOUT_MS
            physicalHashes.invalidate(path)
            val before = DocumentMetadata(document.uri.toString(), modified, length)
            val result = verifiedScanDigest(resolver.openInputStream(document.uri) ?: error("Sem acesso: $path"), before,
                after = { scanSourceMetadata(document, path) },
                checkControl = { checkScanControl(); checkLocalDeadline(hashDeadline) })
            physicalHashes.remember(path, PhysicalHashCache.Entry(document.uri.toString(), modified, length, result.first, result.second, hashGeneration))
            return result
        }
        fun rememberUri(uri: String, path: String) {
            if (uri !in ambiguousUris && nextUris.put(uri, path) != null) {
                nextUris.remove(uri)
                ambiguousUris.add(uri)
            }
        }
        fun walk(directory: Uri, prefix: String) {
            checkScanControl()
            check(nextDirectories.put(directory.toString(), prefix) == null && nextDirectoryPaths.put(prefix, directory.toString()) == null) { "Índice de diretórios SAF ambíguo." }
            val enumerationDeadline = android.os.SystemClock.elapsedRealtime() + LOCAL_OP_TIMEOUT_MS
            val children = safDirectoryMetadata(resolver, activeTree, directory, prefix) {
                checkScanControl(); checkLocalDeadline(enumerationDeadline)
            }
            children.forEach { child ->
                checkScanControl()
                val path = child.path
                parts(path)
                if (ignored(path,child.directory,rules)) return@forEach
                if (child.directory) walk(Uri.parse(child.uri), path)
                else {
                    check(!child.virtual) { "Tipo de documento não suportado: $path" }
                    check(++count <= 100_000) { "Limite de 100 mil arquivos excedido." }
                    enumerated++
                    val uri = child.uri
                    val modified = child.modified
                    PerformanceTrace.event("FILE_ENUMERATED", shareId, path, component = PerformanceTrace.Component.Scanner, sourceFile = "FolderAccess.kt", sourceLine = 1506)
                    val length = child.length
                    val cached = if (!deepAudit) physicalHashes.lookup(path, uri, modified, length,
                        if (path in dirty) changeGeneration else null) else null
                    val reuse = cached != null
                    val (hash, size) = if (reuse) {
                        cacheHits++
                        cached!!.hash to cached.size
                    } else {
                        hashed++
                        PerformanceTrace.event("FILE_HASH_START", shareId, path, component = PerformanceTrace.Component.Scanner, sourceFile = "FolderAccess.kt", sourceLine = 1516)
                        hashForScan(DocumentFile.fromSingleUri(context, Uri.parse(uri)) ?: error("Sem acesso: $path"), path, modified, length)
                    }
                    PerformanceTrace.event(if (reuse) "FILE_HASH_REUSED" else "FILE_HASH_END", shareId, path, size, component = PerformanceTrace.Component.Scanner, detail = JSONObject().put("reason", if (reuse) "provider_metadata_matches" else "cache_missing_or_metadata_changed"), sourceFile = "FolderAccess.kt", sourceLine = 1519)
                    PerformanceTrace.firstSeen(shareId, path, modified, observedVia(shareId), hash)
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
            val focusedLookup = ScanPathLookup(root.uri.toString()) { uri, prefix ->
                safDirectoryMetadata(resolver, activeTree, Uri.parse(uri), prefix) {
                    checkScanControl(); checkLocalDeadline(deadline)
                }
            }
            if (usedFocused) {
                nextCache.putAll(previous)
                for (path in dirty) {
                    checkScanControl()
                    if (ignored(path, false, rules)) {
                        nextCache.remove(path)
                        continue
                    }
                    enumerated++
                    val document = try { focusedLookup.find(path) } catch (error: AuditDeferred) { throw error } catch (error: IllegalStateException) {
                        PerformanceTrace.event("DELTA_UNAVAILABLE", shareId, path, component = PerformanceTrace.Component.Scanner, level = "warn", detail = JSONObject().put("reason", "focused_lookup_failed").put("error", PerformanceTrace.error(error, "filesystem", "focused_lookup", false)), sourceFile = "FolderAccess.kt", sourceLine = 1547)
                        usedFocused = false
                        break
                    }
                    if (document == null) {
                        nextCache.remove(path)
                        continue
                    }
                    PerformanceTrace.event("FILE_HASH_START", shareId, path, component = PerformanceTrace.Component.Scanner, sourceFile = "FolderAccess.kt", sourceLine = 1555)
                    check(!document.directory && !document.virtual) { "Tipo de documento não suportado: $path" }
                    val modified = document.modified
                    val length = document.length
                    val (hash, size) = hashForScan(DocumentFile.fromSingleUri(context, Uri.parse(document.uri)) ?: error("Sem acesso: $path"), path, modified, length)
                    PerformanceTrace.event("FILE_HASH_END", shareId, path, size, component = PerformanceTrace.Component.Scanner, sourceFile = "FolderAccess.kt", sourceLine = 1560)
                    PerformanceTrace.firstSeen(shareId, path, document.modified, observedVia(shareId), hash)
                    hashed++
                    bytesHashed += size
                    nextCache[path] = ScanEntry(tree, document.uri, modified,
                        length, hash, size)
                }
            }
            if (usedFocused) {
                focusedLookup.validate(dirty)
                count = nextCache.size
                check(count <= 100_000) { "Limite de 100 mil arquivos excedido." }
                cacheHits = count - hashed
                nextCache.forEach { (path, entry) ->
                    rememberUri(entry.uri, path)
                    manifest.put(path, JSONObject().put("hash", entry.hash).put("size", entry.size))
                }
                synchronized(scanLock) {
                    nextDirectories.putAll(directoryUris[shareId].orEmpty())
                    nextDirectoryPaths.putAll(directoryPaths[shareId].orEmpty())
                }
            } else {
                PerformanceTrace.event("FULL_SCAN_FALLBACK", shareId, component = PerformanceTrace.Component.Scanner, detail = JSONObject().put("reason", if (fullScan) "full_scan_requested" else "focused_scan_unavailable").put("had_trusted_cache", hadTrustedCache).put("flagged_full", flaggedFull), sourceFile = "FolderAccess.kt", sourceLine = 1582)
                nextCache.clear()
                enumerated = 0
                hashed = 0
                bytesHashed = 0
                walk(root.uri, "")
            }
            synchronized(scanLock) {
                checkScanControl()
                check(nextDirectories.size == nextDirectoryPaths.size &&
                    nextDirectories.all { (uri, path) -> nextDirectoryPaths[path] == uri }) {
                    "Índice de diretórios SAF ambíguo."
                }
                physicalHashes.retainPaths(nextCache.keys)
                pendingScan = PendingScan(shareId, tree, binding, nextCache, nextUris, nextDirectories, nextDirectoryPaths,
                    dirty, flaggedFull, scheduledFull, deepAudit, scheduledDeep)
            }
        } catch (error: Exception) {
            synchronized(scanLock) {
                dirtyPaths.getOrPut(shareId) { mutableSetOf() }.addAll(dirty)
                if (error is AuditDeferred) {
                    restoreScanFlags(shareId, flaggedFull, scheduledFull, deepAudit, scheduledDeep)
                } else deepScanShares.add(shareId)
            }
            if (error is AuditDeferred) return JSONObject().put("deferred", true).toString()
            throw error
        } finally {
            // Completed physical hashes survive discard/reconnect; protocol state stays staged.
            savePhysicalHashes(shareId, force = true)
        }
        PerformanceTrace.event(if (deepAudit) "DEEP_AUDIT_END" else if (usedFocused) "DELTA_SCAN_END" else "AUDIT_END", shareId, component = PerformanceTrace.Component.Scanner, detail = JSONObject().put("enumerated", enumerated).put("hashed", hashed).put("cache_hits", cacheHits), sourceFile = "FolderAccess.kt", sourceLine = 1612)
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

    private fun cachedSource(path: String, shareId: String): DocumentFile? {
        val namespace = synchronized(scanLock) { streamNamespace }
        if (namespace != null) {
            check(namespace.share == shareId) { "STALE_SOURCE: $path" }
            return sourceDocument(namespace, namespace.byPath[path] ?: error("STALE_SOURCE: $path"))
        }
        // DeltaScan retains its existing source resolution. Full scans use the
        // committed URI index after the worker has closed its namespace.
        if (hashStagedBinding.isEmpty()) return null
        check(hashStagedBinding == bindingIdentity()) { "STALE_SOURCE: $path" }
        return cachedResolvedTarget(path, shareId)?.target
    }

    private fun sourceFallback(path: String): DocumentFile {
        if (hashStagedBinding.isNotEmpty()) addStreamMetric("saf_source_lookup_fallback_count")
        val source = traced("saf_find", path) { find(path) } ?: error("STALE_SOURCE: $path")
        check(source.isFile && !source.isVirtual) { "STALE_SOURCE: $path" }
        return source
    }

    fun snapshot(path: String): String {
        checkSelectedBinding()
        val started = android.os.SystemClock.elapsedRealtime()
        val traceStarted = PerformanceTrace.now()
        val share = active?.optString("share_id")
        PerformanceTrace.event("snapshot_start", share, path, sourceFile = "FolderAccess.kt", sourceLine = 1650)
        val deadline = android.os.SystemClock.elapsedRealtime() + LOCAL_OP_TIMEOUT_MS
        checkLocalDeadline(deadline)
        check(!ignored(path,false,ignoreRules())) { "Caminho ignorado: $path" }
        val prepared = synchronized(scanLock) {
            hashStaged.remove(path)?.also { hashStagedBytes -= it.second.length }
        }
        if (prepared != null) {
            try {
                check(bindingIdentity() == hashStagedBinding) { "STALE_SOURCE: $path" }
                val entry = prepared.second
                val doc = cachedSource(path, share ?: error("STALE_SOURCE: $path")) ?: sourceFallback(path)
                check(doc.uri.toString() == entry.uri && doc.lastModified() == entry.modified && doc.length() == entry.length) { "STALE_SOURCE: $path" }
                addStreamMetric("duplicate_reads_avoided")
                return JSONObject().put("path", prepared.first.absolutePath).put("hash", entry.hash).put("size", entry.size).toString()
            } catch (error: Exception) { prepared.first.delete(); throw error }
        }
        val source = share?.let { cachedSource(path, it) } ?: sourceFallback(path)
        val modified = source.lastModified()
        val length = source.length()
        val staged = File.createTempFile("rowd-send-", ".part", context.cacheDir)
        try {
            val (hash, size) = FileOutputStream(staged).use { output ->
                val input = traced("saf_open", path) { resolver.openInputStream(source.uri) ?: error("STALE_SOURCE: $path") }
                val result = traced("saf_copy", path) { digest(input, output, deadline) }
                result
            }
            checkSelectedBinding()
            check(source.lastModified() == modified && source.length() == length && size == length) { "STALE_SOURCE: $path" }
            PerformanceTrace.event("snapshot_end", share, path, size, traceStarted, sourceFile = "FolderAccess.kt", sourceLine = 1679)
            android.util.Log.i("RowdLatency", "snapshot_ms=${android.os.SystemClock.elapsedRealtime() - started} bytes=$size")
            return JSONObject().put("path", staged.absolutePath).put("hash", hash).put("size", size).toString()
        } catch (e: Exception) { staged.delete(); throw e }
    }

    fun install(path: String, expectedHash: String, newHash: String, sourcePath: String): String {
        checkSelectedBinding("STALE_TARGET")
        check(synchronized(scanLock) { streamNamespace == null }) { "Instalação SAF durante scan ativo." }
        val started = android.os.SystemClock.elapsedRealtime()
        val traceStarted = PerformanceTrace.now()
        val share = active?.optString("share_id")
        PerformanceTrace.event("install_start", share, path, sourceFile = "FolderAccess.kt", sourceLine = 1691)
        check(!ignored(path,false,ignoreRules())) { "Caminho ignorado: $path" }
        parts(path)
        active?.optString("share_id")?.takeIf(String::isNotEmpty)?.let { id ->
            synchronized(scanLock) { dirtyPaths.getOrPut(id) { mutableSetOf() }.add(path) }
        }
        val source = File(sourcePath)
        val shareId = active?.optString("share_id").orEmpty()
        val hinted = if (expectedHash.isNotEmpty()) cachedTarget(path, shareId) else null
        val hintedHash = hinted?.let {
            try { traced("target_hash", path, fallback = "resolve_target") { hash(it) } } catch (error: Exception) { null }
        }
        val old = if (expectedHash.isEmpty()) null else if (hintedHash == expectedHash) hinted
            else traced("saf_find", path) { find(path) }
        val actual = if (old === hinted && hintedHash == expectedHash) hintedHash
            else if (expectedHash.isEmpty()) ""
            else traced("target_hash", path) { old?.let { hash(it) } ?: "" }
        if (expectedHash.isNotEmpty() && actual == newHash) {
            val current = if (old === hinted) traced("saf_find", path) { find(path) } else old
            check(traced("target_hash", path) { current?.let { hash(it) } ?: "" } == newHash) { "STALE_TARGET: $path" }
            check(digest(source.inputStream()).first == newHash) { "SHA-256 não confere." }
            android.util.Log.i("RowdLatency", "install_ms=${android.os.SystemClock.elapsedRealtime() - started} replay=true")
            PerformanceTrace.remoteInstalled(shareId, path, newHash)
            PerformanceTrace.event("install_end", share, path, start = traceStarted, sourceFile = "FolderAccess.kt", sourceLine = 1714)
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
        val (resolved, recheckedHash) = traced("target_recheck", path) {
            val location = if (old === hinted && hinted != null)
                cachedResolvedTarget(path, shareId) ?: resolveTarget(path)
            else resolveTarget(path)
            location to (location.target?.let { hash(it) } ?: "")
        }
        if (expectedHash.isEmpty() && recheckedHash == newHash) {
            journal.put("finished", true)
            traced("journal_persist", path) { persist(journalFile, journal) }
            PerformanceTrace.remoteInstalled(shareId, path, newHash)
            PerformanceTrace.event("install_end", share, path, start = traceStarted, sourceFile = "FolderAccess.kt", sourceLine = 1755)
            android.util.Log.i("RowdLatency", "install_ms=${android.os.SystemClock.elapsedRealtime() - started} replay=true")
            return "ok"
        }
        if (recheckedHash != expectedHash) {
            // No shared document was touched; this is an aborted operation, not a crash.
            journal.put("finished", true)
            traced("journal_persist", path) { persist(journalFile, journal) }
            error("STALE_TARGET: $path")
        }
        val target = resolved.target ?: run {
            val directory = if (resolved.missingDirectories.isEmpty()) resolved.directory else traced("saf_parent", path) {
                var prefix = path.split('/').dropLast(1 + resolved.missingDirectories.size).joinToString("/")
                resolved.missingDirectories.fold(resolved.directory) { parent, part ->
                    val matches = parent.listFiles().filter { it.name == part }
                    check(matches.size <= 1) { "O provedor contém nomes duplicados: $part" }
                    val child = matches.singleOrNull() ?: parent.createDirectory(part) ?: error("Não foi possível criar $part")
                    check(child.isDirectory) { "Um arquivo ocupa o lugar da pasta $part" }
                    prefix = if (prefix.isEmpty()) part else "$prefix/$part"
                    synchronized(scanLock) {
                        val uri = child.uri.toString()
                        val byUri = directoryUris[shareId].orEmpty()
                        val byPath = directoryPaths[shareId].orEmpty()
                        if (byUri[uri]?.let { it != prefix } == true ||
                            byPath[prefix]?.let { it != uri } == true) deepScanShares.add(shareId)
                        else {
                            directoryUris[shareId] = byUri + (uri to prefix)
                            directoryPaths[shareId] = byPath + (prefix to uri)
                        }
                    }
                    child
                }
            }
            traced("saf_create", path) {
                directory.createFile("application/octet-stream", resolved.name)
                    ?: error("Não foi possível criar $path")
            }
        }
        traced("target_name", path) {
            check(target.name == path.substringAfterLast('/')) { "O provedor alterou o nome do arquivo; sincronização interrompida." }
        }
        traced("saf_write", path) { writeDocument(incoming, target) }
        check(traced("target_verify", path) { hash(target) } == newHash) { "Falha na gravação. Cópias preservadas para recuperação." }
        journal.put("finished", true)
        traced("journal_persist", path) { persist(journalFile, journal) }
        if (shareId.isNotEmpty()) {
            try {
                val entry = ScanEntry(activeTree.toString(), target.uri.toString(), target.lastModified(),
                    target.length(), newHash, incoming.length())
                synchronized(scanLock) {
                    val paths = uriPaths.getOrPut(shareId) { mutableMapOf() }
                    if (paths[entry.uri]?.let { it != path } == true) deepScanShares.add(shareId)
                    else {
                        val cache = scanCache.getOrPut(shareId) { mutableMapOf() }
                        cache.put(path, entry)?.let { if (paths[it.uri] == path) paths.remove(it.uri) }
                        paths[entry.uri] = path
                    }
                }
            } catch (error: Exception) {
                traceFallback("INSTALL_CACHE_UPDATE_FAILED", shareId, path, "update_scan_cache", "deep_scan", error, PerformanceTrace.Component.Filesystem)
                synchronized(scanLock) { deepScanShares.add(shareId) }
            }
        }
        PerformanceTrace.remoteInstalled(shareId, path, newHash)
        PerformanceTrace.event("install_end", share, path, start = traceStarted, sourceFile = "FolderAccess.kt", sourceLine = 1819)
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
