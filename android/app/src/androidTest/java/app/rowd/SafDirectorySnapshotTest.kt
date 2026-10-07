@file:Suppress("DEPRECATION")
package app.rowd

import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.test.AndroidTestCase
import android.test.mock.MockContentProvider
import android.test.mock.MockContentResolver
import androidx.documentfile.provider.DocumentFile

/** Android APIs with a fake query provider. Real tree grants/provider races still require device validation. */
class SafDirectorySnapshotTest : AndroidTestCase() {
    private val tree = DocumentsContract.buildTreeDocumentUri("app.rowd.test", "root-id")
    private fun document(id: String) = DocumentsContract.buildDocumentUriUsingTree(tree, id)
    private fun children(id: String) = DocumentsContract.buildChildDocumentsUriUsingTree(tree, id)
    private val rows = mutableMapOf<Uri, List<Array<String>>>()
    private val queried = mutableListOf<Uri>()
    private val cursors = mutableListOf<MatrixCursor>()
    private val resolver = MockContentResolver().apply {
        addProvider("app.rowd.test", object : MockContentProvider() {
            override fun query(uri: Uri, projection: Array<out String>?, selection: String?,
                selectionArgs: Array<out String>?, sortOrder: String?): Cursor {
                queried.add(uri)
                val cursor = MatrixCursor(projection!!)
                rows[uri].orEmpty().forEach { cursor.addRow(it) }
                cursors.add(cursor)
                return cursor
            }
        })
    }

    override fun setUp() {
        super.setUp()
        rows[document("root-id")] = listOf(arrayOf("root-id", "chosen", Document.MIME_TYPE_DIR))
        rows[document("dir-id")] = listOf(arrayOf("dir-id", "photos", Document.MIME_TYPE_DIR))
        rows[children("root-id")] = listOf(arrayOf("dir-id", "photos", Document.MIME_TYPE_DIR))
        rows[children("dir-id")] = listOf(arrayOf("file-id", "a.jpg", "image/jpeg"))
    }

    private fun snapshot(id: String, prefix: String, ignored: (String, Boolean) -> Boolean = { _, _ -> false }) =
        safDirectorySnapshot(resolver, tree, document(id), prefix, ignored) {}

    fun testRootAndSubdirectoryQueryExactDocumentThroughOriginalTree() {
        // Reproduces the old API misuse even for a valid tree-scoped document URI.
        try {
            DocumentFile.fromSingleUri(context, document("dir-id"))!!.listFiles()
            fail("SingleDocumentFile unexpectedly supported enumeration")
        } catch (_: UnsupportedOperationException) { }
        assertEquals(listOf(SafStructuralEntry("photos", document("dir-id").toString(), true)), snapshot("root-id", ""))
        assertEquals(listOf(SafStructuralEntry("photos/a.jpg", document("file-id").toString(), false)), snapshot("dir-id", "photos"))
        assertEquals(listOf(document("root-id"), children("root-id"), document("dir-id"), children("dir-id")), queried)
        assertTrue(cursors.all { it.isClosed })
    }

    fun testStructuralChangesAndIgnoreRules() {
        val expected = snapshot("dir-id", "photos")
        for (changed in listOf(
            emptyList(),
            listOf(arrayOf("file-id", "renamed.jpg", "image/jpeg")),
            listOf(arrayOf("replacement-id", "a.jpg", "image/jpeg")),
            listOf(arrayOf("file-id", "a.jpg", Document.MIME_TYPE_DIR)),
            rows[children("dir-id")]!! + listOf(arrayOf("new-id", "new.jpg", "image/jpeg"))
        )) {
            rows[children("dir-id")] = changed
            assertFalse(safStructureMatches(expected, snapshot("dir-id", "photos")))
        }
        rows[children("dir-id")] = listOf(arrayOf("file-id", "a.jpg", "image/jpeg"),
            arrayOf("ignored-id", "ignored", Document.MIME_TYPE_DIR))
        assertTrue(safStructureMatches(expected, snapshot("dir-id", "photos") { path, directory ->
            path == "photos/ignored" && directory
        }))
    }

    fun testTreeScopedSubdirectoryBindingKeepsItsOwnRootIdentity() {
        assertEquals(listOf(SafStructuralEntry("a.jpg", document("file-id").toString(), false)),
            safDirectorySnapshot(resolver, document("dir-id"), document("dir-id"), "", { _, _ -> false }) {})
        assertEquals(listOf(document("dir-id"), children("dir-id")), queried)
    }

    fun testRejectsForeignTreeAuthorityAndPlainDocument() {
        for (uri in listOf(
            DocumentsContract.buildDocumentUriUsingTree(DocumentsContract.buildTreeDocumentUri("app.rowd.test", "other-root"), "dir-id"),
            DocumentsContract.buildDocumentUriUsingTree(DocumentsContract.buildTreeDocumentUri("other.provider", "root-id"), "dir-id"),
            DocumentsContract.buildDocumentUri("app.rowd.test", "dir-id")
        )) {
            try {
                safDirectorySnapshot(resolver, tree, uri, "", { _, _ -> false }) {}
                fail("accepted foreign or unscoped directory")
            } catch (_: IllegalStateException) { }
        }
        assertTrue(queried.isEmpty())
    }

    fun testMissingEmptyDirectoryOrChangedDirectoryNameCannotPass() {
        rows[children("dir-id")] = emptyList()
        for (metadata in listOf(emptyList(), listOf(arrayOf("dir-id", "renamed", Document.MIME_TYPE_DIR)),
            listOf(arrayOf("dir-id", "photos", "text/plain")))) {
            rows[document("dir-id")] = metadata
            try { snapshot("dir-id", "photos"); fail("accepted stale directory") }
            catch (_: IllegalStateException) { }
        }
        assertTrue(cursors.all { it.isClosed })
    }

    fun testBindingControlIsCheckedAgainAfterQuery() {
        var controls = 0
        try {
            safDirectorySnapshot(resolver, tree, document("root-id"), "", { _, _ -> false }) {
                check(++controls < 2) { "binding changed" }
            }
            fail("accepted changed binding")
        } catch (error: IllegalStateException) { assertEquals("binding changed", error.message) }
        assertEquals(listOf(document("root-id")), queried)
        assertTrue(cursors.all { it.isClosed })
    }
}
