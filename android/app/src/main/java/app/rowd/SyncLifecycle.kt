package app.rowd

/** Runtime shutdown must not erase the user's persisted sync preference. */
internal class SyncLifecycle {
    private var stopping = false

    fun start(automatic: Boolean, enabled: Boolean, paired: Boolean): Boolean {
        if (!paired || (automatic && (!enabled || stopping))) return false
        stopping = false
        return true
    }

    fun stop() { stopping = true }

    fun retryDelay(enabled: Boolean, paired: Boolean): Long? =
        if (!stopping && enabled && paired) 5_000L else null
}
