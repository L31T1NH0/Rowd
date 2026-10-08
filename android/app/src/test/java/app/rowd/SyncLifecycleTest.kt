package app.rowd

import org.junit.Assert.*
import org.junit.Test

class SyncLifecycleTest {
    @Test fun systemRestartRestoresEnabledSyncButRespectsPauseAndUnlink() {
        assertTrue(SyncLifecycle().start(automatic = true, enabled = true, paired = true))
        assertFalse(SyncLifecycle().start(automatic = true, enabled = false, paired = true))
        for (automatic in listOf(false, true)) {
            assertFalse(SyncLifecycle().start(automatic, enabled = true, paired = false))
        }
    }

    @Test fun unexpectedWorkerExitRetriesWithoutChangingTheSavedPreference() {
        val lifecycle = SyncLifecycle()
        assertTrue(lifecycle.start(automatic = false, enabled = false, paired = true))
        assertEquals(5_000L, lifecycle.retryDelay(enabled = true, paired = true))
        assertNull(lifecycle.retryDelay(enabled = false, paired = true))
        assertNull(lifecycle.retryDelay(enabled = true, paired = false))
    }

    @Test fun pauseDestroyAndAndroidTimeoutCancelRetryUntilAnExplicitStart() {
        val lifecycle = SyncLifecycle()
        assertTrue(lifecycle.start(automatic = true, enabled = true, paired = true))
        lifecycle.stop()
        assertNull(lifecycle.retryDelay(enabled = true, paired = true))
        assertFalse(lifecycle.start(automatic = true, enabled = true, paired = true))
        assertFalse(lifecycle.start(automatic = true, enabled = false, paired = true))
        assertNull(lifecycle.retryDelay(enabled = true, paired = true))
        assertTrue(lifecycle.start(automatic = false, enabled = false, paired = true))
        assertEquals(5_000L, lifecycle.retryDelay(enabled = true, paired = true))
    }
}
