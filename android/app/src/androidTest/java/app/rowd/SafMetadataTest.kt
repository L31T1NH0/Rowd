@file:Suppress("DEPRECATION")
package app.rowd

import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import android.os.Bundle
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.test.AndroidTestCase
import android.test.mock.MockContentProvider
import android.test.mock.MockContentResolver

class SafMetadataTest : AndroidTestCase() {
    private val tree = DocumentsContract.buildTreeDocumentUri("app.rowd.test", "root")
    private fun document(id: String) = DocumentsContract.buildDocumentUriUsingTree(tree, id)
    private val children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, "root")
    private val rows = mutableMapOf<Uri, List<Array<Any>>>()
    private val queried = mutableListOf<Uri>()
    private val cursors = mutableListOf<MatrixCursor>()
    private var loading = false
    private val resolver = MockContentResolver().apply {
        addProvider("app.rowd.test", object : MockContentProvider() {
            override fun query(uri: Uri, projection: Array<out String>?, selection: String?,
                selectionArgs: Array<out String>?, sortOrder: String?): Cursor {
                queried.add(uri)
                val cursor = object : MatrixCursor(projection!!) {
                    override fun getExtras() = Bundle().apply { putBoolean(DocumentsContract.EXTRA_LOADING, loading && uri == children) }
                }
                rows[uri].orEmpty().forEach { cursor.addRow(it) }
                cursors.add(cursor)
                return cursor
            }
        })
    }
    private fun file(id: String, name: String = id): Array<Any> = arrayOf(id, name, "text/plain", 100L, 2L, 0)
    override fun setUp() {
        super.setUp()
        // The selected root display name is not a child path component.
        rows[document("root")] = listOf(arrayOf("root", "/", Document.MIME_TYPE_DIR, 100L, 0L, 0))
        rows[children] = (0 until 2000).map { file("$it.jpg") }
    }
    private fun listing(control: () -> Unit = {}) = safDirectoryMetadata(resolver, tree, document("root"), "", control)
    private fun rejected(block: () -> Unit) {
        try { block(); fail("accepted invalid provider result") } catch (_: IllegalStateException) { }
        assertTrue(cursors.all { it.isClosed })
    }
    fun testTwoThousandFilesUseTwoQueriesIncludingExactDirectoryValidation() {
        val result = listing()
        assertEquals(2000, result.size)
        assertEquals(listOf(document("root"), children), queried)
        assertEquals(SafMetadata("0.jpg", document("0.jpg").toString(), false, false, 100, 2), result.first())
        assertTrue(cursors.all { it.isClosed })
    }
    fun testIncompleteDuplicateIdentityAndUnsafeNameCannotBecomeValidListing() {
        loading = true; rejected { listing() }
        loading = false
        for (invalid in listOf(listOf(file("id"), file("id", "other")),
            listOf(file("id", "same"), file("other", "same")), listOf(file("id", "../file")))) {
            rows[children] = invalid
            rejected { listing() }
        }
    }
    fun testMissingDirectoryCannotLookLikeEmptyListingAndExactFileUsesOneQuery() {
        rows[document("root")] = emptyList(); rows[children] = emptyList()
        rejected { listing() }
        rows[document("id")] = listOf(file("id", "a"))
        queried.clear()
        assertEquals(file("id", "a")[1], safDocumentMetadata(resolver, tree, document("id"), "a").path)
        assertEquals(listOf(document("id")), queried)
    }
    fun testCancellationDuringCursorReadClosesBothCursors() {
        var checks = 0
        rejected { listing { check(++checks < 8) { "cancelled" } } }
    }
    fun testSelectedSubdirectoryKeepsItsOwnIdentityAndVirtualFlagIsPreserved() {
        val selected = document("dir")
        val selectedChildren = DocumentsContract.buildChildDocumentsUriUsingTree(selected, "dir")
        rows[selected] = listOf(arrayOf("dir", "photos", Document.MIME_TYPE_DIR, 100L, -1L, 0))
        rows[selectedChildren] = listOf(file("a").also { it[5] = Document.FLAG_VIRTUAL_DOCUMENT })
        val result = safDirectoryMetadata(resolver, selected, selected, "") {}
        assertEquals(listOf(selected, selectedChildren), queried)
        assertEquals("a", result.single().path)
        assertTrue(result.single().virtual)
        assertTrue(cursors.all { it.isClosed })
    }
}
