package app.rowd

import org.junit.Assert.*
import org.junit.Test
import java.util.concurrent.atomic.AtomicInteger

class SafReadWorkersTest {
    @Test fun privateResultsKeepOrderAndJoinBeforeFailure() {
        assertEquals(listOf(4, 9), SafReadWorkers.read("test-provider", listOf(2, 3), {}) { it * it })
        val completed = AtomicInteger()
        try {
            SafReadWorkers.read("test-provider", listOf(1, 2), {}) {
                try { if (it == 1) error("read failure") else Thread.sleep(20) }
                finally { completed.incrementAndGet() }
            }
            fail("failed read must propagate")
        } catch (expected: IllegalStateException) { assertEquals("read failure", expected.message) }
        assertEquals(2, completed.get())
    }
    @Test fun cancellationRejectsReadBeforeOpeningIt() {
        val opened = AtomicInteger()
        try {
            SafReadWorkers.read("cancel-test-provider", listOf(1, 2), { error("cancelled") }) { opened.incrementAndGet() }
            fail("cancelled reads must fail")
        } catch (expected: IllegalStateException) { assertEquals("cancelled", expected.message) }
        assertEquals(0, opened.get())
    }
}
