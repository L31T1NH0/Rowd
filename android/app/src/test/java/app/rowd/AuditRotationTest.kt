package app.rowd

import org.junit.Assert.*
import org.junit.Test

class AuditRotationTest {
    @Test fun pausedAndUnboundSharesDoNotBlockActiveShares() {
        val audits = AuditRotation()
        val available = setOf("A", "paused", "B")
        val enabled = setOf("A", "B", "unbound")
        repeat(4) { round ->
            val share = audits.select(available, enabled)
            assertEquals(if (round % 2 == 0) "A" else "B", share)
            audits.complete(share, false)
        }
        assertNull(audits.select(available, emptySet()))
        assertNull(audits.select(emptySet(), enabled))
    }

    @Test fun skippedShareAdvancesButDeferredAuditRetriesItsShare() {
        val audits = AuditRotation()
        val shares = setOf("A", "B")
        val selected = audits.select(shares, shares)
        audits.complete(selected, true)
        assertEquals(selected, audits.select(shares, shares))
        // The peer may disable A after selection, so the successful round skips it.
        audits.complete(selected, false)
        assertEquals("B", audits.select(shares, shares))
        audits.complete(null, false)
        assertEquals("B", audits.select(shares, shares))
    }
}
