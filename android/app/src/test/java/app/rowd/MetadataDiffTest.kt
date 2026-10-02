package app.rowd

import org.junit.Assert.*
import org.junit.Test

class MetadataDiffTest {
    private fun metadata(path: String, modified: Long = 100) = DocumentMetadata("uri:$path", modified, 5)
    @Test fun providerWideUnchangedTreeDoesNotHash() {
        val cached = mapOf("dir/a" to metadata("dir/a"), "dir/b" to metadata("dir/b"))
        val diff = MetadataDiff(cached, 0)
        repeat(2) { diff.directory() }
        cached.forEach { (path, value) -> diff.entry(); diff.file(path, value) }
        diff.finish("")
        assertTrue(diff.changed.isEmpty())
        assertEquals(0, diff.pathsHashed(emptySet()))
        assertEquals(2, diff.directoriesVisited)
        assertEquals(2, diff.entriesEnumerated)
    }
    @Test fun providerWideNewModifiedRemovedAndUnreliableMetadataAreDiscovered() {
        val cached = mapOf("a" to metadata("a"), "removed" to metadata("removed"), "same" to metadata("same"))
        val diff = MetadataDiff(cached, 0)
        listOf("a" to metadata("a", 101), "new" to metadata("new"), "same" to metadata("same")).forEach {
            diff.entry(); diff.file(it.first, it.second)
        }
        diff.finish("")
        assertEquals(setOf("a", "new", "removed"), diff.changed)
        assertEquals(2, diff.pathsHashed(setOf("a", "new")))
        assertTrue(metadata("same", 0).differsFrom(cached["same"]))
    }
    @Test fun largeTreeEnumeratesMetadataButHashesOnlyChangedPaths() {
        val cached = (0 until 100).flatMap { dir -> (0 until 100).map { file -> "d$dir/f$file" } }.associateWith { metadata(it) }
        val diff = MetadataDiff(cached, 0)
        diff.directory() // root
        repeat(100) { diff.directory(); diff.entry() }
        cached.forEach { (path, value) ->
            if (path != "d99/f99") { diff.entry(); diff.file(path, if (path == "d0/f0") value.copy(modified = 101) else value) }
        }
        diff.entry(); diff.file("d99/new", metadata("d99/new"))
        diff.finish("")
        assertEquals(101, diff.directoriesVisited)
        assertEquals(10100, diff.entriesEnumerated)
        assertEquals(setOf("d0/f0", "d99/f99", "d99/new"), diff.changed)
        assertEquals(2, diff.pathsHashed(setOf("d0/f0", "d99/new")))
    }
}
