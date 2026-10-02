package app.rowd

internal data class TraceSource(val file: String?, val line: Int, val function: String?)

/** Inspect only enough frames to identify the producer; no stack is retained in normal events. */
internal fun traceSource(frames: Array<StackTraceElement> = Thread.currentThread().stackTrace): TraceSource {
    val frame = frames.firstOrNull {
        it.fileName != "TraceDiagnostics.kt" && it.className != "java.lang.Thread" &&
            it.className != "dalvik.system.VMStack" &&
            !it.className.startsWith("app.rowd.PerformanceTrace\$event\$") &&
            !(it.className == "app.rowd.PerformanceTrace" &&
                (it.methodName.startsWith("event") || it.methodName == "firstSeen")) &&
            !(it.fileName == "SyncService.kt" && it.methodName.contains("traceEvent")) &&
            !(it.fileName == "FolderAccess.kt" && it.methodName.startsWith("traceFallback"))
    }
    return TraceSource(frame?.fileName, frame?.lineNumber?.coerceAtLeast(0) ?: 0, frame?.methodName)
}

internal data class TraceWriterState(val active: Boolean, val error: String?)

/** An event rejection and a writer failure are distinct; native state is authoritative. */
internal class TraceProducerState {
    var active = false
    var failure: String? = null

    fun reconcile(writer: TraceWriterState) {
        val wasActive = active
        active = writer.active
        if (!active && (wasActive || writer.error != null)) {
            failure = writer.error ?: "TRACE_WRITER_DISABLED"
        }
    }

    fun deliver(send: () -> Boolean, runtime: () -> TraceWriterState, report: (String) -> Unit) {
        try {
            if (!send()) report("INGEST_ANDROID_FAILED: event rejected; checking writer state")
        } catch (error: Exception) {
            report("INGEST_ANDROID_FAILED: ${error.javaClass.name}: ${error.message}")
        }
        try {
            reconcile(runtime())
            if (!active) report(failure ?: "TRACE_WRITER_DISABLED")
        } catch (error: Exception) {
            report("TRACE_STATE_UNAVAILABLE: ${error.javaClass.name}: ${error.message}")
        }
    }
}
