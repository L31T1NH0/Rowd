package app.rowd

import org.junit.Assert.*
import org.junit.Test
import java.io.File
import java.nio.file.Files
import java.security.MessageDigest

class PhysicalHashCacheTest {
    private fun entry(hash: Char) = PhysicalHashCache.Entry("uri", 10, 4, hash.toString().repeat(64), 4, 3)
    private fun target(directory: File) = File(directory, MessageDigest.getInstance("SHA-256").digest("share".toByteArray())
        .joinToString("") { "%02x".format(it.toInt() and 255) } + ".hashes")
    private fun load(directory: File) = PhysicalHashCache(directory).also { it.select("share", "binding") }

    @Test fun preparedCheckpointIsPrivateImmutableAndPublishedOnlyByConsumer() {
        val directory = Files.createTempDirectory("rowd-cache-receipt-").toFile()
        try {
            val cache = load(directory)
            cache.remember("file", entry('a'))
            val old = cache.prepareSaveIfDue(force = true)!!
            assertFalse(target(directory).exists())
            cache.remember("file", entry('b'))
            old.publish()
            assertEquals(entry('a').hash, load(directory).lookup("file", "uri", 10, 4)?.hash)
            cache.prepareSaveIfDue(force = true)!!.publish()
            assertEquals(entry('b').hash, load(directory).lookup("file", "uri", 10, 4)?.hash)
            assertEquals(0, directory.listFiles()!!.count { it.extension == "tmp" })
            try { old.publish(); fail("Receipt published twice") } catch (_: IllegalStateException) { }
        } finally { directory.deleteRecursively() }
    }

    @Test fun supersededCheckpointAndCancelledReceiptDoNotLeakAndEvidenceCanBeRetried() {
        val directory = Files.createTempDirectory("rowd-cache-supersede-").toFile()
        try {
            val cache = load(directory)
            cache.remember("file", entry('a'))
            val old = cache.prepareSaveIfDue(force = true)!!
            cache.remember("file", entry('b'))
            val newer = cache.prepareSaveIfDue(force = true)!!
            old.discard(retryLater = false)
            newer.discard(); newer.discard()
            assertFalse(target(directory).exists())
            assertEquals(0, directory.listFiles()!!.count { it.extension == "tmp" })
            cache.prepareSaveIfDue(force = true)!!.publish()
            assertEquals(entry('b').hash, load(directory).lookup("file", "uri", 10, 4)?.hash)
        } finally { directory.deleteRecursively() }
    }

    @Test fun publicationFailureRetainsEvidenceAndPreservesFifteenSecondThrottle() {
        val directory = Files.createTempDirectory("rowd-cache-retry-").toFile()
        try {
            val cache = load(directory)
            cache.remember("file", entry('a'))
            val now = System.nanoTime() / 1_000_000 + 15_000
            val receipt = cache.prepareSaveIfDue(nowMillis = now)!!
            assertTrue(target(directory).mkdir())
            try { receipt.publish(); fail("Directory accepted as cache target") } catch (_: Exception) { }
            assertNull(cache.prepareSaveIfDue(nowMillis = now + 14_999))
            assertTrue(target(directory).delete())
            cache.prepareSaveIfDue(nowMillis = now + 15_000)!!.publish()
            assertEquals(entry('a').hash, load(directory).lookup("file", "uri", 10, 4)?.hash)
            assertEquals(0, directory.listFiles()!!.count { it.extension == "tmp" })
        } finally { directory.deleteRecursively() }
    }
}
