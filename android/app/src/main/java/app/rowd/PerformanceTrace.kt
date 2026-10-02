package app.rowd

import android.content.Context
import android.os.SystemClock
import org.json.JSONObject
import java.io.File
import java.security.MessageDigest
import java.util.UUID
import java.util.zip.ZipEntry
import java.util.zip.ZipOutputStream

/** Kotlin and native events use one persistent Rust writer, schema and sequence. */
object PerformanceTrace {
    enum class Component(val label: String) {
        Service("Android-Service"), Watcher("Watcher"), Scanner("Scanner"), Scheduler("Scheduler"),
        Round("Round"), Network("Network"), Filesystem("Filesystem"), SAF("SAF"), Terminal("Terminal-output")
    }
    private var started = 0L
    private var root: File? = null
    private val processStartedWallMs = System.currentTimeMillis() - (SystemClock.elapsedRealtime() - android.os.Process.getStartElapsedRealtime())
    var serviceStartedWallMs: Long? = null
    private val observers = mutableMapOf<String, Long>()
    private val seen = mutableSetOf<String>()
    private val shareNames = mutableMapOf<String, String>()
    private val producer = TraceProducerState()
    private var active: Boolean
        get() = producer.active
        set(value) { producer.active = value }
    var failure: String?
        @Synchronized get() = producer.failure
        private set(value) { producer.failure = value }
    private fun writerState(): TraceWriterState {
        val state = JSONObject(NativeBridge.traceRuntimeState())
        return TraceWriterState(state.getBoolean("trace_active"),
            if (state.isNull("trace_error")) null else state.optString("trace_error"))
    }
    @Synchronized fun enabled(): Boolean {
        val wasActive = active
        val previousFailure = failure
        try {
            producer.reconcile(writerState())
        } catch (error: Exception) {
            android.util.Log.e("RowdTrace", "TRACE_STATE_UNAVAILABLE", error)
        }
        if (!active && failure != null && (wasActive || previousFailure != failure))
            android.util.Log.e("RowdTrace", failure!!)
        return active
    }

    @Synchronized fun enable(context: Context) {
        check(!enabled()) { "Trace já ativo" }
        val directory = File(context.filesDir, "diagnostic-trace")
        check(NativeBridge.setTrace(directory.absolutePath, UUID.randomUUID().toString())) { "Não foi possível iniciar o trace Rust." }
        root = directory
        started = SystemClock.elapsedRealtimeNanos()
        seen.clear(); failure = null; active = true
        event("PROCESS_START", null, component = Component.Service, level = "info", sourceFile = "PerformanceTrace.kt", function = "enable", detail = JSONObject()
            .put("observed_via_trace_activation", true).put("process_started_wall_ms", processStartedWallMs)
            .put("android_version", android.os.Build.VERSION.RELEASE).put("device_model", android.os.Build.MODEL)
            .put("app_version", context.packageManager.getPackageInfo(context.packageName, 0).versionName))
    }
    @Synchronized fun disable(termination: String = "trace_stop") {
        if (!enabled()) return
        event("TRACE_PRODUCER_STOP", null, component = Component.Service, level = "info", sourceFile = "PerformanceTrace.kt", function = "disable", detail = JSONObject().put("termination", termination))
        val stopped = NativeBridge.stopTrace(termination)
        producer.reconcile(writerState())
        if (stopped) failure = null
        check(stopped) { failure ?: "Não foi possível finalizar o trace Rust." }
    }
    @Synchronized fun flush() {
        NativeBridge.flushTrace()
        enabled()
    }
    @Synchronized fun observerRegistered(share: String?) { if (share != null) observers[share] = System.currentTimeMillis() }
    @Synchronized fun observerUnregistered() { observers.clear() }
    @Synchronized fun registerShares(shares: org.json.JSONArray) {
        shareNames.clear()
        for (index in 0 until shares.length()) {
            val share = shares.getJSONObject(index)
            shareNames[share.getString("share_id")] = share.optString("name", share.getString("share_id"))
        }
    }
    fun fileId(share: String, path: String): String {
        val bytes = MessageDigest.getInstance("SHA-256").digest((share + "\u0000" + path).toByteArray(Charsets.UTF_8))
        return bytes.take(8).joinToString("") { "%02x".format(it.toInt() and 255) }
    }
    @JvmStatic fun describeError(error: Throwable): String = error(error, "filesystem", "saf_call").toString()
    fun error(error: Throwable, kind: String, operation: String, stack: Boolean = true): JSONObject {
        val chain = org.json.JSONArray()
        val visited = mutableSetOf<Throwable>()
        var cause: Throwable? = error
        while (cause != null && visited.add(cause)) { chain.put("${cause.javaClass.name}: ${cause.message}"); cause = cause.cause }
        return JSONObject().put("kind", kind).put("operation", operation).put("class", error.javaClass.name)
            .put("message", error.message).put("chain", chain).apply { if (stack) put("stack", android.util.Log.getStackTraceString(error)) }
    }
    @Synchronized fun event(name: String, share: String?, path: String? = null, bytes: Long? = null, start: Long? = null,
        detail: JSONObject? = null, component: Component = Component.SAF, level: String = "trace", function: String? = null, sourceFile: String = "FolderAccess.kt") {
        if (!enabled()) return
        producer.deliver(send = {
            val source = traceSource()
            val context = JSONObject()
            if (share != null) {context.put("share_id", share);shareNames[share]?.let {context.put("share_name", it)}}
            if (share != null && path != null) context.put("file_id", fileId(share, path))
            val fields = detail ?: JSONObject()
            if (path != null) fields.put("relative_path", path)
            if (bytes != null) fields.put("bytes", bytes)
            if (start != null) fields.put("duration_us", (now() - start) / 1000)
            val json = JSONObject().put("event", name.uppercase()).put("component", component.label).put("level", level)
                .put("context", context).put("fields", fields)
                .put("source", JSONObject().put("file", source.file ?: sourceFile).put("line", source.line)
                    .put("function", function ?: source.function ?: name).put("thread", Thread.currentThread().name))
            NativeBridge.traceEvent(json.toString())
        }, runtime = ::writerState, report = { android.util.Log.e("RowdTrace", it) })
    }
    @Synchronized fun firstSeen(share: String, path: String, modified: Long?, source: String) {
        if (!enabled() || !seen.add(fileId(share, path))) return
        val wall = System.currentTimeMillis()
        val registered = observers[share]
        event("FILE_FIRST_SEEN", share, path, component = Component.Filesystem, detail = JSONObject()
            .put("source", source).put("first_observed_wall_ms", wall).put("metadata_created_ms", JSONObject.NULL)
            .put("metadata_modified_ms", JSONObject.NULL).put("provider_last_modified_ms", modified ?: JSONObject.NULL)
            .put("file_age_at_first_observation_ms", modified?.takeIf { it > 0 }?.let { wall - it } ?: JSONObject.NULL)
            .put("process_started_wall_ms", processStartedWallMs).put("service_started_wall_ms", serviceStartedWallMs ?: JSONObject.NULL)
            .put("observer_registered_wall_ms", registered ?: JSONObject.NULL).put("observer_active", registered != null)
            .put("process_age_ms", wall - processStartedWallMs).put("observer_age_ms", registered?.let { wall - it } ?: JSONObject.NULL))
    }
    fun now() = SystemClock.elapsedRealtimeNanos()
    fun export(context: Context, destination: android.net.Uri) {
        flush()
        val latest = File(root ?: File(context.filesDir, "diagnostic-trace"), "Latest-trace")
        context.contentResolver.openOutputStream(destination)?.use { output ->
            ZipOutputStream(output).use { zip ->
                latest.walkTopDown().filter { it.isFile }.forEach { file ->
                    zip.putNextEntry(ZipEntry(file.relativeTo(latest).path)); file.inputStream().use { it.copyTo(zip) }; zip.closeEntry()
                }
            }
        } ?: error("Não foi possível abrir o destino do trace.")
    }
}
