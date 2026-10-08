package app.rowd

import org.junit.Assert.*
import org.junit.Test

class ScanPathLookupTest {
    private fun file(path: String, uri: String = "uri:$path") = SafMetadata(path, uri, false, false, 100, 2)
    private fun directory(path: String) = SafMetadata(path, "uri:$path", true, false, 100, 0)
    private fun stale(block: () -> Unit) {
        try { block(); fail("accepted stale path") }
        catch (error: IllegalStateException) { assertTrue(error.message!!.contains("STALE_SOURCE")) }
    }
    @Test fun twoThousandSiblingLookupsUseOneListingAndOneRevalidation() {
        val files = (0 until 2000).map { file("$it.jpg") }
        var reads = 0
        val lookup = ScanPathLookup("root") { _, _ -> reads++; files }
        files.forEach { assertEquals(it, lookup.find(it.path)) }
        assertEquals(1, reads)
        lookup.validate(files.map { it.path }.toSet())
        assertEquals(2, reads)
    }
    @Test fun replacementWithSameSizeAndDateCannotPassAndNextScanGetsFreshListing() {
        var files = listOf(file("a"))
        val lookup = ScanPathLookup("root") { _, _ -> files }
        val before = lookup.find("a")!!
        files = listOf(before.copy(uri = "replacement"))
        stale { lookup.validate(setOf("a")) }
        assertEquals("replacement", ScanPathLookup("root") { _, _ -> files }.find("a")!!.uri)
    }
    @Test fun movedOrReplacedAncestorCannotPassEvenIfLeafUriStillExists() {
        for (replacement in listOf(emptyList(), listOf(directory("dir").copy(uri = "new-dir")))) {
            var roots = listOf(directory("dir"))
            val lookup = ScanPathLookup("root") { _, prefix -> if (prefix.isEmpty()) roots else listOf(file("dir/a")) }
            assertNotNull(lookup.find("dir/a"))
            roots = replacement
            stale { lookup.validate(setOf("dir/a")) }
        }
    }
    @Test fun missingFileAndMissingParentAreRecheckedBeforeReportingAbsence() {
        for (path in listOf("new", "dir/new")) {
            var roots = emptyList<SafMetadata>()
            val lookup = ScanPathLookup("root") { _, prefix -> if (prefix.isEmpty()) roots else listOf(file("dir/new")) }
            assertNull(lookup.find(path))
            roots = if (path.contains('/')) listOf(directory("dir")) else listOf(file(path))
            stale { lookup.validate(setOf(path)) }
        }
    }
    @Test fun unrelatedSiblingChangesAndDirectoryTimestampDoNotInvalidateSelectedFile() {
        var roots = listOf(directory("dir"))
        var files = listOf(file("dir/a"), file("dir/b"))
        val lookup = ScanPathLookup("root") { _, prefix -> if (prefix.isEmpty()) roots else files }
        assertNotNull(lookup.find("dir/a"))
        roots = listOf(directory("dir").copy(modified = 101))
        files = listOf(file("dir/a"), file("dir/b").copy(modified = 101), file("dir/new"))
        lookup.validate(setOf("dir/a"))
    }
    @Test fun cancellationDuringRevalidationPropagates() {
        var cancelled = false
        val lookup = ScanPathLookup("root") { _, _ -> check(!cancelled) { "cancelled" }; listOf(file("a")) }
        assertNotNull(lookup.find("a"))
        cancelled = true
        try { lookup.validate(setOf("a")); fail() }
        catch (error: IllegalStateException) { assertEquals("cancelled", error.message) }
    }
    @Test fun repeatedSnapshotResolutionListsSiblingsOnceButRechecksAncestors() {
        val files = (0 until 2000).map { file("dir/$it.jpg") }
        var listings = 0
        var exactReads = 0
        val lookup = ScanPathLookup("uri:root") { _, prefix ->
            listings++
            if (prefix.isEmpty()) listOf(directory("dir")) else files
        }
        files.take(6).forEach { entry ->
            assertEquals(entry, lookup.find(entry.path))
            lookup.validateAncestors(entry.path) { uri, prefix ->
                exactReads++
                directory(prefix).copy(uri = uri)
            }
        }
        assertEquals(2, listings) // Root and dir, regardless of the number of snapshots.
        assertEquals(12, exactReads)
        for (changed in listOf(directory("renamed"), directory("dir").copy(uri = "replacement"),
            directory("dir").copy(directory = false), directory("dir").copy(virtual = true))) {
            stale { lookup.validateAncestors("dir/0.jpg") { uri, prefix ->
                if (prefix.isEmpty()) directory(prefix).copy(uri = uri) else changed
            } }
        }
    }
    @Test fun batchedCacheValidationRejectsChangedFileMetadata() {
        val original = file("a")
        for (changed in listOf(original.copy(modified = 101), original.copy(length = 3),
            original.copy(directory = true), original.copy(virtual = true))) {
            var files = listOf(original)
            val lookup = ScanPathLookup("root") { _, _ -> files }
            assertEquals(original, lookup.find("a"))
            files = listOf(changed)
            stale { lookup.validate(setOf("a")) }
        }
    }
}
