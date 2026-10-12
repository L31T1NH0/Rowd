package app.rowd

import org.junit.Assert.*
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

class ScanResultTest {
    private val identity = ScanIdentity("connection-owner", 7, "share", "tree", "mode|policy|binding", 11)

    @Test fun everyLifecycleMismatchDisposesWithoutPublishing() {
        val mismatches = listOf(identity.copy(owner = "reconnect"), identity.copy(epoch = 8),
            identity.copy(share = "other"), identity.copy(tree = "remap"),
            identity.copy(binding = "different-direction-policy"), identity.copy(generation = 12))
        for (current in mismatches) {
            var published = false; var disposed = 0
            val result = PreparedScan(identity, "manifest", publishResult = { published = true }, discardResult = { disposed++ })
            try { result.consume(identity, current); fail("Stale result accepted: $current") }
            catch (error: IllegalStateException) { assertEquals("AUDIT_DEFERRED", error.message) }
            result.discard()
            assertFalse(published); assertEquals(1, disposed)
        }
    }

    @Test fun jobPathSequenceAndCancellationAreCheckedBeforePublication() {
        for (failure in 0..3) {
            var published = false; var disposed = false
            val result = PreparedScan(if (failure == 3) identity.copy(epoch = 6) else identity,
                "digest", sequence = 2, path = "photos/file", publishResult = { published = true }, discardResult = { disposed = true })
            try {
                result.consume(identity, identity, expectedSequence = if (failure == 0) 3 else 2,
                    expectedPath = if (failure == 1) "different/file" else "photos/file", cancelled = failure == 2)
                fail("Invalid result accepted")
            } catch (_: IllegalStateException) { }
            assertFalse(published); assertTrue(disposed)
        }
    }

    @Test fun unrelatedGenerationIsAllowedForFocusedWorkButSourceEvidenceStillGatesIt() {
        val current = identity.copy(generation = 12)
        val source = SafMetadata("file", "uri:file", false, false, 100, 4)
        for (samePathChanged in listOf(false, true)) {
            var files = listOf(source, source.copy(path = "other", uri = "uri:other"))
            val lookup = ScanPathLookup("root") { _, _ -> files }
            assertEquals(source, lookup.find("file"))
            var published = false; var disposed = false
            val result = PreparedScan(identity, "focused", validateResult = { lookup.validate(setOf("file")) },
                publishResult = { published = true }, discardResult = { disposed = true })
            files = files.map { if (it.path == if (samePathChanged) "file" else "other") it.copy(modified = 101) else it }
            if (samePathChanged) {
                try { result.consume(identity, current, requireGeneration = false); fail("Changed source accepted") }
                catch (error: IllegalStateException) { assertTrue(error.message!!.startsWith("STALE_SOURCE")) }
                assertFalse(published); assertTrue(disposed)
            } else {
                assertEquals("focused", result.consume(identity, current, requireGeneration = false))
                assertTrue(published); assertFalse(disposed)
            }
        }
    }

    @Test fun composedDeltaValidatesFocusedSourceBeforePublishingAnyDiscovery() {
        var source = SafMetadata("file", "uri:file", false, false, 100, 4)
        val lookup = ScanPathLookup("root") { _, _ -> listOf(source) }
        assertEquals(source, lookup.find("file"))
        var hints = false; var cache = false; var discarded = 0
        val discovery = PreparedScan(identity, "paths", publishResult = { hints = true }, discardResult = { discarded++ })
        val focused = PreparedScan(identity, "manifest", validateResult = { lookup.validate(setOf("file")) },
            publishResult = { cache = true }, discardResult = { discarded++ })
        val combined = PreparedScan(identity, focused.payload, validateResult = { discovery.validate(); focused.validate() },
            publishResult = { discovery.publish(); focused.publish() }, discardResult = { discovery.discard(); focused.discard() })
        source = source.copy(uri = "replaced-source")
        try { combined.consume(identity, identity); fail("Stale composed result accepted") }
        catch (error: IllegalStateException) { assertTrue(error.message!!.startsWith("STALE_SOURCE")) }
        assertFalse(hints); assertFalse(cache); assertEquals(2, discarded)
    }

    @Test fun workerOnlyProducesAndCoordinatorPublishesOnce() {
        val coordinator = Thread.currentThread()
        val produced = CountDownLatch(1)
        var publishedOn: Thread? = null
        var disposed = 0
        val worker = ScanWorker<PreparedScan> { it.discard() }
        worker.start {
            assertNotSame(coordinator, Thread.currentThread())
            PreparedScan(identity, "namespace", publishResult = { publishedOn = Thread.currentThread() },
                discardResult = { disposed++ }).also { produced.countDown() }
        }
        assertTrue(produced.await(2, TimeUnit.SECONDS))
        assertNull(publishedOn)
        var result: PreparedScan? = null
        while (result == null) { result = worker.poll(); Thread.yield() }
        assertEquals("namespace", result.consume(identity, identity))
        worker.finish()
        result.discard()
        assertSame(coordinator, publishedOn); assertEquals(0, disposed)
        try { result.publish(); fail("Duplicate publication accepted") } catch (_: IllegalStateException) { }
    }

    @Test fun fullQueueCancellationDisposesAllProducedResultsAndRestartIsClean() {
        val full = CountDownLatch(1)
        val produced = AtomicInteger(); val disposed = AtomicInteger(); val published = AtomicInteger()
        val worker = ScanStreamWorker<PreparedScan>(discard = { it.discard() }, backpressure = { if (it) full.countDown() })
        worker.start { send ->
            repeat(100) {
                produced.incrementAndGet()
                send(PreparedScan(identity, it.toString(), sequence = it.toLong(),
                    publishResult = { published.incrementAndGet() }, discardResult = { disposed.incrementAndGet() }))
            }
            PreparedScan(identity, "end")
        }
        assertTrue(full.await(2, TimeUnit.SECONDS))
        worker.cancel(); worker.finish()
        assertEquals(0, published.get()); assertEquals(produced.get(), disposed.get())
        assertTrue(worker.peakChunks <= 2)
        val fresh = identity.copy(epoch = 8)
        worker.start { send -> send(PreparedScan(fresh, "fresh")); PreparedScan(fresh, "end", sequence = 1) }
        var result: PreparedScan? = null
        while (result == null) { result = worker.poll(); Thread.yield() }
        assertEquals("fresh", result.consume(fresh, fresh))
        worker.finish()
    }
}
