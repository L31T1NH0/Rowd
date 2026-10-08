package app.rowd

import org.junit.Assert.*
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.File
import java.io.InputStream
import java.nio.file.Files
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

class ScanResilienceTest {
    @Test fun explicitCancellationIgnoresAuditModeAndGenerationRemainsAuditOnly() {
        for (audit in listOf(false, true)) assertTrue(scanShouldAbort(true, audit, false))
        assertFalse(scanShouldAbort(false, false, true))
        assertTrue(scanShouldAbort(false, true, true))
        assertFalse(scanShouldAbort(false, true, false))
    }
    @Test fun fullScanCancellationJoinsWorkerAndAllowsNextShare() {
        val worker = ScanWorker()
        val started = CountDownLatch(1)
        val cancel = AtomicBoolean(false)
        val stopped = CountDownLatch(1)
        worker.start {
            started.countDown()
            while (!scanShouldAbort(cancel.get(), false, false)) Thread.yield()
            stopped.countDown(); "deferred"
        }
        assertTrue(started.await(2, TimeUnit.SECONDS))
        cancel.set(true); worker.requestCancel()
        assertTrue(stopped.await(2, TimeUnit.SECONDS))
        worker.finish(); assertEquals(ScanWorker.State.IDLE, worker.state)
        worker.start { "next Share" }; worker.finish()
    }
    @Test fun cancellationDuringDigestClosesInputAndNeverReturnsPartialHash() {
        var closed = false
        var reads = 0
        val stream = object : InputStream() {
            override fun read() = error("bulk only")
            override fun read(b: ByteArray, off: Int, len: Int): Int { reads++; return len }
            override fun close() { closed = true }
        }
        try {
            scanDigest(stream) { check(reads < 3) { "cancelled" } }
            fail("Partial hash returned")
        } catch (e: IllegalStateException) { assertEquals("cancelled", e.message) }
        assertTrue(closed); assertEquals(3, reads)
    }
    @Test fun digestWithoutControlPreservesSnapshotCopyAndHash() {
        val output = java.io.ByteArrayOutputStream()
        val (hash, size) = scanDigest(ByteArrayInputStream("abc".toByteArray()), output)
        assertEquals("6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85", hash)
        assertEquals(3L, size); assertEquals("abc", output.toString())
    }
    @Test fun nativeDigestHandlesShortReadsAcrossBlocksWithoutHashingBufferPadding() {
        val bytes = ByteArray(131073) { (it % 251).toByte() }
        val expected = scanDigest(ByteArrayInputStream(bytes))
        val input = object : ByteArrayInputStream(bytes) {
            override fun read(buffer: ByteArray, offset: Int, length: Int) = super.read(buffer, offset, minOf(length, 8191))
        }
        val output = java.io.ByteArrayOutputStream()
        assertEquals(expected, scanDigest(input, output))
        assertEquals(bytes.size.toLong(), expected.second)
        assertArrayEquals(bytes, output.toByteArray())
    }
    @Test fun legacyJournalsCanStillUseSha256() {
        val result = scanDigest(ByteArrayInputStream("abc".toByteArray()), legacy = true)
        assertEquals("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", result.first)
        assertEquals(3L, result.second)
    }
    @Test fun nativeDigestPreservesReadAndWriteExceptionsAndClosesInput() {
        for (writing in listOf(false, true)) {
            var closed = false
            val original = java.io.IOException("I/O failure")
            val input = object : ByteArrayInputStream("abc".toByteArray()) {
                override fun read(buffer: ByteArray, offset: Int, length: Int): Int {
                    if (!writing) throw original
                    return super.read(buffer, offset, length)
                }
                override fun close() { closed = true }
            }
            val output = object : java.io.OutputStream() { override fun write(value: Int) { throw original } }
            try { scanDigest(input, output); fail("Accepted failed I/O") }
            catch (error: java.io.IOException) { assertSame(original, error) }
            assertTrue(closed)
        }
    }
    private val entry = PhysicalHashCache.Entry("uri", 123, 3, "a".repeat(64), 3)
    @Test fun checkpointsAreThrottledAndUnchangedCacheIsNotRewritten() {
        val directory = Files.createTempDirectory("rowd-hashes").toFile()
        try {
            val cache = PhysicalHashCache(directory); cache.select("share", "tree")
            cache.remember("a", entry)
            assertNull(cache.saveIfDue(force = true, nowMillis = 0))
            val file = directory.listFiles()!!.single()
            assertTrue(file.setLastModified(1000))
            cache.remember("a", entry)
            assertNull(cache.saveIfDue(force = true, nowMillis = 1))
            assertEquals(1000L, file.lastModified())
            cache.remember("b", entry)
            assertNull(cache.saveIfDue(nowMillis = 14999))
            val before = PhysicalHashCache(directory); before.select("share", "tree")
            assertNull(before.lookup("b", "uri", 123, 3))
            assertNull(cache.saveIfDue(nowMillis = 15000))
            val after = PhysicalHashCache(directory); after.select("share", "tree")
            assertEquals(entry, after.lookup("b", "uri", 123, 3))
        } finally { directory.deleteRecursively() }
    }
    @Test fun checkpointFailurePreservesMemoryAndOriginalErrorAndCanRetry() {
        val directory = Files.createTempDirectory("rowd-hashes").toFile()
        try {
            val blocked = File(directory, "blocked").also { it.writeText("file") }
            val cache = PhysicalHashCache(blocked); cache.select("share", "tree")
            cache.remember("a", entry)
            val original = IllegalStateException("source failed")
            try {
                try { throw original } finally { assertNotNull(cache.saveIfDue(force = true, nowMillis = 0)) }
            } catch (error: IllegalStateException) { assertSame(original, error) }
            assertEquals(entry, cache.lookup("a", "uri", 123, 3))
            assertNull(cache.saveIfDue(nowMillis = 1000))
            assertTrue(blocked.delete()); assertTrue(blocked.mkdir())
            assertNull(cache.saveIfDue(force = true, nowMillis = 1001))
            val restarted = PhysicalHashCache(blocked); restarted.select("share", "tree")
            assertEquals(entry, restarted.lookup("a", "uri", 123, 3))
        } finally { directory.deleteRecursively() }
    }
    @Test fun completedHashesSurviveFailedRoundAndRestartWithoutProtocolCommit() {
        val directory = Files.createTempDirectory("rowd-hashes").toFile()
        try {
            val cache = PhysicalHashCache(directory); cache.select("share", "tree|1")
            repeat(1033) { cache.remember("$it.png", entry) }
            // No manifest/ACK/commit API is present in the physical cache.
            cache.save()
            val restarted = PhysicalHashCache(directory); restarted.select("share", "tree|1")
            repeat(1033) { assertEquals(entry, restarted.lookup("$it.png", "uri", 123, 3)) }
        } finally { directory.deleteRecursively() }
    }
    @Test fun metadataUriPathShareAndBindingChangesInvalidate() {
        val directory = Files.createTempDirectory("rowd-hashes").toFile()
        try {
            val cache = PhysicalHashCache(directory); cache.select("share", "tree|1")
            cache.remember("a", entry); cache.save()
            assertNull(cache.lookup("a", "uri", 123, 4))
            assertNull(cache.lookup("a", "uri", 124, 3))
            assertNull(cache.lookup("a", "new-uri", 123, 3))
            assertNull(cache.lookup("new-path", "uri", 123, 3))
            cache.select("share", "tree|2"); assertNull(cache.lookup("a", "uri", 123, 3))
            cache.select("other", "tree|1"); assertNull(cache.lookup("a", "uri", 123, 3))
            cache.select("share", "other-tree|1"); assertNull(cache.lookup("a", "uri", 123, 3))
        } finally { directory.deleteRecursively() }
    }
    @Test fun unreliableMetadataDirtyPathsAndPartialHashesAreNeverReused() {
        val directory = Files.createTempDirectory("rowd-hashes").toFile()
        try {
            val cache = PhysicalHashCache(directory); cache.select("share", "tree")
            cache.remember("a", entry.copy(modified = 0)); assertNull(cache.lookup("a", "uri", 0, 3))
            cache.remember("a", entry.copy(size = 2)); assertNull(cache.lookup("a", "uri", 123, 3))
            cache.remember("a", entry); cache.invalidate("a"); assertNull(cache.lookup("a", "uri", 123, 3))
            cache.remember("a", entry)
            try { scanDigest(ByteArrayInputStream(ByteArray(100))) { error("cancelled") }; fail() }
            catch (_: IllegalStateException) { }
            // Actual scanner invalidates before hashing, so a failed rehash cannot reuse the old entry.
            cache.invalidate("a"); cache.save()
            val restarted = PhysicalHashCache(directory); restarted.select("share", "tree")
            assertNull(restarted.lookup("a", "uri", 123, 3))
        } finally { directory.deleteRecursively() }
    }
    @Test fun restoredDirtyPathsReuseHashesButNewEventsInvalidateEvenWithSameMetadata() {
        val directory = Files.createTempDirectory("rowd-hashes").toFile()
        try {
            val cache = PhysicalHashCache(directory); cache.select("share", "tree")
            val verified = entry.copy(generation = 7)
            cache.remember("a", verified)
            assertEquals(verified, cache.lookup("a", "uri", 123, 3, dirtyGeneration = 7))
            assertNull(cache.lookup("a", "uri", 123, 3, dirtyGeneration = 8))
            cache.save()
            val restarted = PhysicalHashCache(directory); restarted.select("share", "tree")
            assertNotNull(restarted.lookup("a", "uri", 123, 3))
            // Generation numbers are process-local and cannot validate new dirty events after restart.
            assertNull(restarted.lookup("a", "uri", 123, 3, dirtyGeneration = 7))
        } finally { directory.deleteRecursively() }
    }
    @Test fun corruptTruncatedAndUnknownSchemaCacheFallBackToHashing() {
        val directory = Files.createTempDirectory("rowd-hashes").toFile()
        try {
            for (mode in 0..3) {
                val cache = PhysicalHashCache(directory); cache.select("share", "tree")
                cache.remember("a", entry); cache.save()
                val file = directory.listFiles()!!.single()
                val bytes = file.readBytes()
                when (mode) {
                    0 -> { bytes[bytes.size - 33] = (bytes[bytes.size - 33].toInt() xor 1).toByte(); file.writeBytes(bytes) }
                    1 -> file.writeBytes(bytes.copyOf(12))
                    else -> {
                        bytes[3] = 1
                        val payload = bytes.copyOfRange(0, bytes.size - 32)
                        file.writeBytes(payload + java.security.MessageDigest.getInstance("SHA-256").digest(payload))
                    }
                }
                val restarted = PhysicalHashCache(directory); restarted.select("share", "tree")
                assertNull(restarted.lookup("a", "uri", 123, 3))
            }
        } finally { directory.deleteRecursively() }
    }
}
