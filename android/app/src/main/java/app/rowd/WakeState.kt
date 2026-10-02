package app.rowd

internal enum class WakeSource { LOCAL_OBSERVER, REMOTE_WAKE, PERIODIC_AUDIT, MANUAL, NETWORK_RECONNECT, STARTUP }

/** Guarded by SyncService.changes. Transport state never mutates filesystem generations. */
internal class WakeState {
    var dirty = false
    var generation = 0L
    var dirtyAllGeneration = 0L
    val dirtyShares = mutableMapOf<String, Long>()
    val detectedAt = mutableMapOf<String, Long>()
    val sources = mutableMapOf<String, WakeSource>()
    var reconnectRequested = false

    fun wake(shareId: String?, source: WakeSource, at: Long) {
        if (source == WakeSource.NETWORK_RECONNECT) { requestReconnect(); return }
        generation++
        if (shareId == null) dirtyAllGeneration = generation else dirtyShares[shareId] = generation
        detectedAt[shareId ?: "*"] = at
        sources[shareId ?: "*"] = source
        dirty = true
    }
    fun requestReconnect() { reconnectRequested = true }
    fun pollResult(kind: String, shareId: String?, at: Long) {
        when (kind) {
            "none", "local", "cancelled", "audit_due" -> Unit
            "share" -> wake(requireNotNull(shareId), WakeSource.REMOTE_WAKE, at)
            "transport_invalid", "network" -> requestReconnect()
            else -> error("Resultado pollWake desconhecido: $kind")
        }
    }
}

internal class NativeSyncFailure(val kind: String, message: String) : Exception(message)
