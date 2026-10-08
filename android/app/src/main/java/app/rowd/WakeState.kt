package app.rowd

internal enum class WakeSource { LOCAL_OBSERVER, REMOTE_WAKE, PERIODIC_AUDIT, MANUAL, NETWORK_RECONNECT, STARTUP }

internal class AuditRotation {
    private var cursor = 0L

    fun select(available: Set<String>, enabled: Set<String>): String? {
        val eligible = available.intersect(enabled).sorted()
        return if (eligible.isEmpty()) null else eligible[(cursor % eligible.size).toInt()]
    }

    fun complete(share: String?, deferred: Boolean) {
        // A Share disabled or unbound during the round must not pin the rotation.
        if (share != null && !deferred) cursor++
    }
}

/** Guarded by SyncService.changes. Transport state never mutates filesystem generations. */
internal class WakeState {
    var dirty = false
    var generation = 0L
    var dirtyAllGeneration = 0L
    val dirtyShares = mutableMapOf<String, Long>()
    val detectedAt = mutableMapOf<String, Long>()
    val sources = mutableMapOf<String, WakeSource>()
    // Keep the version after consuming a wake: completion must not look like a new change.
    private val shareGenerations = mutableMapOf<String, Long>()
    var reconnectRequested = false

    fun generationFor(shareId: String): Long = maxOf(dirtyAllGeneration, shareGenerations[shareId] ?: 0L)

    fun complete(selected: Map<String, Long>, completed: Set<String>, resumePending: Boolean = false) {
        selected.forEach { (id, version) ->
            if (id in completed && dirtyShares[id] == version) {
                dirtyShares.remove(id); detectedAt.remove(id); sources.remove(id)
            }
        }
        if (resumePending && selected.keys.any { it in dirtyShares && it !in completed }) dirty = true
    }

    fun wake(shareId: String?, source: WakeSource, at: Long) {
        if (source == WakeSource.NETWORK_RECONNECT) { requestReconnect(); return }
        generation++
        if (shareId == null) dirtyAllGeneration = generation else {
            dirtyShares[shareId] = generation
            shareGenerations[shareId] = generation
        }
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
