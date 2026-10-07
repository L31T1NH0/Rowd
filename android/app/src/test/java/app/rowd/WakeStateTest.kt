package app.rowd

import org.junit.Assert.*
import org.junit.Test

class WakeStateTest {
    @Test fun otherShareAndReconnectDoNotInvalidateActiveShare() {
        val state = WakeState()
        state.wake("A", WakeSource.LOCAL_OBSERVER, 1)
        val before = state.generationFor("A")
        state.wake("B", WakeSource.LOCAL_OBSERVER, 2)
        state.pollResult("transport_invalid", null, 3)
        state.pollResult("share", "B", 4)
        assertEquals(before, state.generationFor("A"))
        state.complete(state.dirtyShares.toMap(), setOf("A"))
        assertEquals(before, state.generationFor("A"))
        state.wake("A", WakeSource.LOCAL_OBSERVER, 5)
        assertTrue(state.generationFor("A") > before)
    }
    @Test fun manualWakeInvalidatesEveryShareButLaterScopedWakeDoesNot() {
        val state = WakeState()
        state.wake("A", WakeSource.LOCAL_OBSERVER, 1)
        state.wake(null, WakeSource.MANUAL, 2)
        assertEquals(2L, state.generationFor("A"))
        assertEquals(2L, state.generationFor("B"))
        state.wake("B", WakeSource.LOCAL_OBSERVER, 3)
        assertEquals(2L, state.generationFor("A"))
        assertEquals(3L, state.generationFor("B"))
    }
    @Test fun singleShareAuditPreservesOtherPendingShareAndItsDetection() {
        val state = WakeState()
        state.wake("A", WakeSource.LOCAL_OBSERVER, 10)
        state.wake("B", WakeSource.REMOTE_WAKE, 20)
        val selected = state.dirtyShares.toMap()
        state.dirty = false // The scheduler consumed the signal at round start.
        state.complete(selected, setOf("A"), resumePending = true)
        assertTrue(state.dirty) // Do not idle until the next periodic audit.
        assertEquals(mapOf("B" to selected.getValue("B")), state.dirtyShares)
        assertEquals(mapOf("B" to 20L), state.detectedAt)
        assertEquals(mapOf("B" to WakeSource.REMOTE_WAKE), state.sources)
        state.dirty = false
        state.complete(selected, setOf("B"), resumePending = true)
        assertTrue(state.dirtyShares.isEmpty())
        assertFalse(state.dirty)
    }
    @Test fun completedRoundCannotConsumeNewerChangeOrUncompletedShare() {
        val state = WakeState()
        state.wake("A", WakeSource.LOCAL_OBSERVER, 10)
        val selected = state.dirtyShares.toMap()
        state.complete(selected, emptySet())
        assertEquals(selected, state.dirtyShares)
        state.wake("A", WakeSource.REMOTE_WAKE, 20)
        state.complete(selected, setOf("A"))
        assertEquals(2L, state.dirtyShares["A"])
        assertEquals(20L, state.detectedAt["A"])
        assertEquals(WakeSource.REMOTE_WAKE, state.sources["A"])
    }
    @Test fun transportInvalidNeverDirtiesFilesystem() {
        val state = WakeState()
        state.pollResult("none", null, 1)
        state.pollResult("transport_invalid", null, 2)
        assertTrue(state.reconnectRequested)
        assertFalse(state.dirty)
        assertEquals(0L, state.generation)
        assertEquals(0L, state.dirtyAllGeneration)
        assertTrue(state.dirtyShares.isEmpty())
        assertTrue(state.detectedAt.isEmpty())
        assertTrue(state.sources.isEmpty())
    }
    @Test fun remoteWakeRetainsShareSourceAndGeneration() {
        val state = WakeState()
        state.pollResult("share", "share-X", 42)
        assertEquals(mapOf("share-X" to 1L), state.dirtyShares)
        assertEquals(WakeSource.REMOTE_WAKE, state.sources["share-X"])
        assertEquals(42L, state.detectedAt["share-X"])
        state.wake(null, WakeSource.NETWORK_RECONNECT, 99)
        assertEquals(1L, state.generation)
        assertEquals(0L, state.dirtyAllGeneration)
        assertEquals(mapOf("share-X" to 42L), state.detectedAt)
    }
}
