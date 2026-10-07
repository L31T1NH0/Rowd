@file:Suppress("DEPRECATION")
package app.rowd

import android.database.ContentObserver
import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.test.AndroidTestCase
import android.test.mock.MockContentProvider
import android.test.mock.MockContentResolver
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicInteger

class SafDirectoryObserversTest : AndroidTestCase() {
    private val tree = DocumentsContract.buildTreeDocumentUri("app.rowd.test", "root")
    private fun children(id: String) = DocumentsContract.buildChildDocumentsUriUsingTree(tree, id)
    private class WatchCursor(columns: Array<out String>) : MatrixCursor(columns) {
        @Volatile var registered = false
        override fun registerContentObserver(observer: ContentObserver) {
            super.registerContentObserver(observer)
            registered = true
        }
        fun change() = onChange(false)
    }
    private val rows = ConcurrentHashMap<Uri, List<Array<String>>>()
    private val opened = ConcurrentHashMap<Uri, CopyOnWriteArrayList<WatchCursor>>()
    private val changes = AtomicInteger()
    private val changedDirectories = CopyOnWriteArrayList<Uri>()
    private val errors = CopyOnWriteArrayList<Exception>()
    private val resolver = MockContentResolver().apply {
        addProvider("app.rowd.test", object : MockContentProvider() {
            override fun query(uri: Uri, projection: Array<out String>?, selection: String?,
                selectionArgs: Array<out String>?, sortOrder: String?): Cursor {
                val cursor = WatchCursor(projection!!)
                rows[uri].orEmpty().forEach { cursor.addRow(it) }
                opened.getOrPut(uri) { CopyOnWriteArrayList() }.add(cursor)
                return cursor
            }
        })
    }
    private lateinit var observers: SafDirectoryObservers
    override fun setUp() {
        super.setUp()
        rows[children("root")] = listOf(arrayOf("nested", Document.MIME_TYPE_DIR))
        rows[children("nested")] = emptyList()
        observers = SafDirectoryObservers(resolver, { _, share, directory ->
            assertEquals("share", share)
            changedDirectories.add(directory)
            changes.incrementAndGet()
        }, { _, _, error -> errors.add(error) })
    }
    override fun tearDown() {
        observers.close()
        super.tearDown()
    }
    private fun eventually(check: () -> Boolean) {
        val deadline = System.nanoTime() + 3_000_000_000L
        while (!check() && System.nanoTime() < deadline) Thread.sleep(10)
        assertTrue(check())
        assertTrue(errors.toString(), errors.isEmpty())
    }
    private fun cursor(id: String) = opened[children(id)]!!.last()

    fun testRetainsNestedWatchesAndTracksNewAndRemovedDirectories() {
        observers.update(mapOf(tree to "share"))
        eventually { opened[children("nested")]?.lastOrNull()?.registered == true }
        val rootCursor = cursor("root")
        val nestedCursor = cursor("nested")
        assertFalse(rootCursor.isClosed)
        assertFalse(nestedCursor.isClosed)

        // A file edit inside a pre-existing subdirectory must wake the Share.
        nestedCursor.change()
        eventually { changes.get() == 1 && nestedCursor.isClosed }
        assertEquals(listOf(DocumentsContract.buildDocumentUriUsingTree(tree, "nested")), changedDirectories)
        assertFalse(cursor("nested").isClosed)
        assertFalse(rootCursor.isClosed)

        rows[children("root")] = rows[children("root")]!! + listOf(arrayOf("new", Document.MIME_TYPE_DIR))
        rows[children("new")] = emptyList()
        rootCursor.change()
        eventually { opened[children("new")]?.lastOrNull()?.registered == true }
        assertEquals(2, changes.get())
        val newCursor = cursor("new")
        assertFalse(newCursor.isClosed)

        rows[children("root")] = emptyList()
        cursor("root").change()
        eventually { cursor("nested").isClosed && newCursor.isClosed }
        assertEquals(3, changes.get())
        observers.update(emptyMap())
        eventually { opened.values.flatMap { it }.all { it.isClosed } }
    }

    fun testSubdirectoryBindingUsesSelectedDocumentRatherThanGrantRoot() {
        val selected = DocumentsContract.buildDocumentUriUsingTree(tree, "nested")
        val selectedChildren = DocumentsContract.buildChildDocumentsUriUsingTree(selected, "nested")
        rows[selectedChildren] = emptyList()
        observers.update(mapOf(selected to "share"))
        eventually { opened[selectedChildren]?.lastOrNull()?.registered == true }
        assertNull(opened[children("root")])
        observers.close()
        eventually { opened[selectedChildren]!!.all { it.isClosed } }
    }
}
