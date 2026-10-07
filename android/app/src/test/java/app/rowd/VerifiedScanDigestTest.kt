package app.rowd

import org.junit.Assert.*
import org.junit.Test
import java.io.ByteArrayInputStream

class VerifiedScanDigestTest {
    private val before = DocumentMetadata("document/a", 123, 3)

    @Test fun stableSourceReturnsVerifiedDigestAndClosesInput() {
        var closed = false
        val input = object : ByteArrayInputStream("abc".toByteArray()) {
            override fun close() { closed = true; super.close() }
        }
        val result = verifiedScanDigest(input, before, after = { before })
        assertEquals("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", result.first)
        assertEquals(3L, result.second)
        assertTrue(closed)
    }

    @Test fun sourceMutationDuringReadNeverReturnsADigest() {
        for (changed in listOf(before.copy(uri = "document/replacement"),
            before.copy(modified = 124), before.copy(length = 4))) {
            var current = before
            var closed = false
            val input = object : ByteArrayInputStream("abc".toByteArray()) {
                override fun read(buffer: ByteArray, offset: Int, length: Int): Int {
                    val count = super.read(buffer, offset, length)
                    if (count > 0) current = changed
                    return count
                }
                override fun close() { closed = true; super.close() }
            }
            try {
                verifiedScanDigest(input, before, after = { current })
                fail("Published a digest after the source changed: $changed")
            } catch (error: IllegalStateException) {
                assertTrue(error.message!!.startsWith("STALE_SOURCE:"))
            }
            assertTrue(closed)
        }
    }

    @Test fun stableMetadataDoesNotAcceptTruncatedOrOversizedContent() {
        for (content in listOf("ab", "abcd")) {
            try {
                verifiedScanDigest(ByteArrayInputStream(content.toByteArray()), before, after = { before })
                fail("Accepted a stream whose size differs from provider metadata")
            } catch (error: IllegalStateException) {
                assertTrue(error.message!!.startsWith("STALE_SOURCE:"))
            }
        }
    }

    @Test fun failedSourceResolutionAfterReadNeverReturnsADigest() {
        var closed = false
        val input = object : ByteArrayInputStream("abc".toByteArray()) {
            override fun close() { closed = true; super.close() }
        }
        try {
            verifiedScanDigest(input, before, after = { error("STALE_SOURCE: removed or retyped") })
            fail("Published a digest without resolving the source again")
        } catch (error: IllegalStateException) {
            assertEquals("STALE_SOURCE: removed or retyped", error.message)
        }
        assertTrue(closed)
    }
}
