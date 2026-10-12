package app.rowd

import org.junit.Assert.*
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

class ScanStreamWorkerTest {
    @Test fun finishCancelsFullQueueDisposesResultsAndClosesBackpressure() {
        val full = CountDownLatch(1)
        val produced = java.util.concurrent.atomic.AtomicInteger()
        val disposed = java.util.concurrent.atomic.AtomicInteger()
        val waiting = java.util.concurrent.atomic.AtomicBoolean()
        val worker = ScanStreamWorker<String>(discard = { disposed.incrementAndGet() }, backpressure = {
            waiting.set(it); if (it) full.countDown()
        })
        worker.start { send ->
            repeat(1000) { produced.incrementAndGet(); send(it.toString()) }
            "end"
        }
        assertTrue(full.await(2, TimeUnit.SECONDS))
        val cleanup = Thread { worker.finish() }
        try {
            cleanup.start(); cleanup.join(2000)
            assertFalse("finish blocked on a full queue", cleanup.isAlive)
            assertFalse(waiting.get())
            assertEquals(produced.get(), disposed.get())
            assertFalse(worker.hasTask())
        } finally {
            worker.cancel(); cleanup.join(2000)
        }
        worker.start { "end" }
        worker.finish()
        assertFalse(worker.hasTask())
    }
    @Test fun fastProducerSlowConsumerIsBoundedAndLosesNoChunks() {
        val full = CountDownLatch(1)
        val worker = ScanStreamWorker<String>(backpressure = { if (it) full.countDown() })
        worker.start { publish -> repeat(1000) { publish(it.toString()) }; "end" }
        assertTrue(full.await(2, TimeUnit.SECONDS))
        val values = mutableListOf<Int>()
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10)
        while (true) {
            check(System.nanoTime() < deadline)
            when (val value = (worker.poll() ?: "")) {
                "end" -> break
                "" -> Thread.yield()
                else -> values.add(value.toInt())
            }
        }
        worker.finish()
        assertEquals((0 until 1000).toList(), values)
        assertTrue(worker.peakChunks <= 2)
    }
    @Test fun cancellationUnblocksAFullQueueAndCanRestart() {
        val full = CountDownLatch(1)
        val worker = ScanStreamWorker<String>(backpressure = { if (it) full.countDown() })
        worker.start { publish -> repeat(1000) { publish(it.toString()) }; "end" }
        assertTrue(full.await(2, TimeUnit.SECONDS))
        worker.cancel(); worker.finish()
        worker.start { it("fresh"); "end" }
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2)
        var value = ""
        while (value.isEmpty()) { check(System.nanoTime() < deadline); value = (worker.poll() ?: "") }
        assertEquals("fresh", value)
        worker.finish()
    }
    @Test fun failedProducerNeverTurnsIntoSuccessfulEnd() {
        val worker = ScanStreamWorker<String>()
        worker.start { error("STALE_SOURCE") }
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2)
        try {
            while ((worker.poll() ?: "").isEmpty()) check(System.nanoTime() < deadline)
            fail("worker failure was hidden")
        } catch (error: java.util.concurrent.ExecutionException) {
            assertEquals("STALE_SOURCE", error.cause?.message)
        } finally { worker.finish() }
    }
    @Test fun structuralFailureAfterPublishedChunkStillPreventsSuccessfulEnd() {
        val worker = ScanStreamWorker<String>()
        val expected = listOf(SafStructuralEntry("photos/a.jpg", "tree/document/file-id", false))
        worker.start { publish ->
            publish("verified-hash-chunk")
            check(safStructureMatches(expected, emptyList())) { "AUDIT_DEFERRED" }
            "end"
        }
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2)
        val chunks = mutableListOf<String>()
        try {
            while (true) {
                check(System.nanoTime() < deadline)
                when (val value = (worker.poll() ?: "")) {
                    "" -> Thread.yield()
                    "end" -> fail("structurally incomplete stream became successful")
                    else -> chunks.add(value)
                }
            }
        } catch (error: java.util.concurrent.ExecutionException) {
            assertEquals("AUDIT_DEFERRED", error.cause?.message)
            assertEquals(listOf("verified-hash-chunk"), chunks)
        } finally { worker.finish() }
    }
}
