package app.rowd

import org.junit.Assert.*
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

class ScanWorkerTest {
    @Test fun cancellationOfReadyResultDiscardsInsteadOfPublishing() {
        val produced = CountDownLatch(1)
        var discarded = 0
        val worker = ScanWorker<String> { discarded++ }
        worker.start { "ready".also { produced.countDown() } }
        assertTrue(produced.await(2, TimeUnit.SECONDS))
        worker.requestCancel(); worker.finish()
        assertEquals(1, discarded)
        assertFalse(worker.hasTask())
        assertEquals(ScanWorker.State.IDLE, worker.state)
    }
    @Test fun completedPollThenPreemptCleanupIsIdempotent() {
        val worker = ScanWorker<String>()
        worker.start { "manifest" }
        var result: String
        do { result = (worker.poll() ?: ""); if (result.isEmpty()) Thread.yield() } while (result.isEmpty())
        assertEquals("manifest", result)
        assertEquals(ScanWorker.State.CONSUMED, worker.state)
        worker.requestCancel() // AuditPreempt / ScanDeferred after consuming the result.
        worker.finish()
        worker.finish()
        assertEquals(ScanWorker.State.IDLE, worker.state)
        assertFalse(worker.hasTask())
        worker.start { "next Share" }
        worker.finish()
    }
    @Test fun cleanupWaitsForWorkerAndRejectsConcurrentScan() {
        val worker = ScanWorker<String>()
        val release = CountDownLatch(1)
        worker.start { release.await(); "manifest" }
        try { worker.start { "other Share" }; fail("Concurrent scan accepted") }
        catch (_: IllegalStateException) { }
        worker.requestCancel()
        assertEquals(ScanWorker.State.CANCEL_REQUESTED, worker.state)
        val cleanupEntered = CountDownLatch(1)
        val cleanupDone = CountDownLatch(1)
        val cleanup = Thread { cleanupEntered.countDown(); worker.finish(); cleanupDone.countDown() }
        cleanup.start()
        assertTrue(cleanupEntered.await(2, TimeUnit.SECONDS))
        assertFalse(cleanupDone.await(50, TimeUnit.MILLISECONDS))
        release.countDown()
        assertTrue(cleanupDone.await(2, TimeUnit.SECONDS))
        cleanup.join()
        assertEquals(ScanWorker.State.IDLE, worker.state)
        worker.start { "other Share" }
        worker.finish()
    }
    @Test fun failedConsumedTaskCanBeCleanedUpWithoutSecondError() {
        val worker = ScanWorker<String>()
        val finished = CountDownLatch(1)
        worker.start { finished.countDown(); error("provider failed") }
        assertTrue(finished.await(2, TimeUnit.SECONDS))
        try { while ((worker.poll() ?: "").isEmpty()) Thread.yield(); fail("Failure missing") }
        catch (_: java.util.concurrent.ExecutionException) { }
        worker.finish()
        assertEquals(ScanWorker.State.IDLE, worker.state)
    }
}
