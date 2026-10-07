package app.rowd

import org.junit.Assert.*
import org.junit.Test

/** Pure comparison coverage. This does not execute Android Uri, queries, or a SAF provider. */
class SafStructureTest {
    private val rootChild = SafStructuralEntry("photos", "content://provider/tree/root/document/dir-id", true)
    private val nestedFile = SafStructuralEntry("photos/a.jpg", "content://provider/tree/root/document/file-id", false)

    @Test fun rootSubdirectoryAndFileRetainPathUriAndType() {
        assertTrue(safStructureMatches(listOf(rootChild), listOf(rootChild.copy())))
        assertTrue(safStructureMatches(listOf(nestedFile), listOf(nestedFile.copy())))
        assertTrue(safStructureMatches(emptyList(), emptyList()))
        assertTrue(safStructureMatches(listOf(rootChild, nestedFile), listOf(nestedFile, rootChild)))
    }

    @Test fun createDeleteRenameUriRebindingRetypingAndDuplicateRowsDiffer() {
        val expected = listOf(nestedFile)
        for (actual in listOf(
            emptyList(),
            expected + nestedFile.copy(path = "photos/new.jpg", uri = "new-uri"),
            listOf(nestedFile.copy(path = "photos/renamed.jpg")),
            listOf(nestedFile.copy(uri = "content://provider/tree/other/document/file-id")),
            listOf(nestedFile.copy(uri = "content://provider/tree/root/document/replacement-id")),
            listOf(nestedFile.copy(directory = true)),
            expected + nestedFile
        )) assertFalse(safStructureMatches(expected, actual))
    }
}
