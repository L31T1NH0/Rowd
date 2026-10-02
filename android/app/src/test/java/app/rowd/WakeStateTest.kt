package app.rowd

import org.junit.Assert.*
import org.junit.Test

class WakeStateTest {
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
