package app.rowd

import org.junit.Assert.*
import org.junit.Test

class ObserverBurstsTest {
    @Test fun genericCallbacksKeepAllSharesAndEarliestDetectionWithoutExtendingWindow() {
        val bursts = ObserverBursts()
        assertTrue(bursts.add("provider", "A", 1000))
        assertFalse(bursts.add("provider", "B", 1010))
        assertFalse(bursts.add("provider", "C", 1020))
        assertFalse(bursts.add("provider", "A", 1030))
        assertFalse(bursts.add("other", "D", 1040))
        val flushed = bursts.flush()
        assertEquals(2, flushed.size)
        assertEquals(4, flushed[0].callbacks)
        assertEquals(mapOf("A" to 1000L, "B" to 1010L, "C" to 1020L), flushed[0].shares)
        assertEquals(1000L, flushed[0].startedAt)
        assertEquals(setOf("D"), flushed[1].shares.keys)
        assertTrue(bursts.flush().isEmpty())
        assertTrue(bursts.add("provider", "A", 2000))
    }
}
