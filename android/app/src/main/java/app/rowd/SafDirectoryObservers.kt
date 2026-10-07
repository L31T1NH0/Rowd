package app.rowd

import android.content.ContentResolver
import android.database.ContentObserver
import android.database.Cursor
import android.net.Uri
import android.os.Handler
import android.os.HandlerThread
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import java.util.concurrent.atomic.AtomicBoolean

/** ExternalStorageProvider keeps its FileObservers alive only while directory cursors are open. */
internal class SafDirectoryObservers(
    private val resolver: ContentResolver,
    private val changed: (Uri, String?, Uri) -> Unit,
    private val failed: (Uri, String?, Exception) -> Unit
) : AutoCloseable {
    private data class Directory(val tree: Uri, val id: String)
    private data class Watch(val cursor: Cursor, val children: List<Directory>)
    private val thread = HandlerThread("rowd-directory-observers").apply { start() }
    private val handler = Handler(thread.looper)
    private val closed = AtomicBoolean(false)
    // All cursor ownership and queries stay on this thread, never on the UI/sync worker.
    private var trees = emptyMap<Uri, String?>()
    private val watches = mutableMapOf<Directory, Watch>()

    fun update(current: Map<Uri, String?>) {
        val snapshot = current.toMap()
        handler.post {
            if (!closed.get()) {
                trees = snapshot
                refresh()
            }
        }
    }

    private fun root(tree: Uri): Directory = Directory(tree,
        if (tree.pathSegments.contains("document")) DocumentsContract.getDocumentId(tree)
        else DocumentsContract.getTreeDocumentId(tree))

    private fun refresh(changedDirectory: Directory? = null) {
        val pending = java.util.ArrayDeque<Directory>()
        val seen = mutableSetOf<Directory>()
        trees.keys.forEach { tree ->
            try { pending.add(root(tree)) }
            catch (error: Exception) { failed(tree, trees[tree], error) }
        }
        while (pending.isNotEmpty() && !closed.get()) {
            val directory = pending.removeFirst()
            if (!seen.add(directory)) continue
            if (directory !in watches || directory == changedDirectory) {
                try { watch(directory) }
                catch (error: Exception) { failed(directory.tree, trees[directory.tree], error) }
            }
            watches[directory]?.children?.forEach(pending::addLast)
        }
        watches.keys.filter { it !in seen }.forEach { watches.remove(it)?.cursor?.close() }
    }

    private fun watch(directory: Directory) {
        val childrenUri = DocumentsContract.buildChildDocumentsUriUsingTree(directory.tree, directory.id)
        val cursor = resolver.query(childrenUri,
            arrayOf(Document.COLUMN_DOCUMENT_ID, Document.COLUMN_MIME_TYPE), null, null, null)
            ?: error("Provider não retornou a listagem para observação.")
        try {
            val children = mutableListOf<Directory>()
            while (cursor.moveToNext()) {
                if (cursor.getString(1) == Document.MIME_TYPE_DIR) {
                    children.add(Directory(directory.tree, cursor.getString(0) ?: error("Diretório sem ID.")))
                }
            }
            cursor.registerContentObserver(object : ContentObserver(handler) {
                override fun onChange(selfChange: Boolean) {
                    if (closed.get() || watches[directory]?.cursor !== cursor) return
                    changed(directory.tree, trees[directory.tree], DocumentsContract.buildDocumentUriUsingTree(directory.tree, directory.id))
                    // Discover new subdirectories and release removed ones without a sync round.
                    refresh(directory)
                }
            })
            if (closed.get()) { cursor.close(); return }
            // Subscribe before closing the old cursor so the provider never stops watching.
            watches.put(directory, Watch(cursor, children))?.cursor?.close()
        } catch (error: Exception) {
            cursor.close()
            throw error
        }
    }

    override fun close() {
        if (!closed.compareAndSet(false, true)) return
        handler.post {
            watches.values.forEach { it.cursor.close() }
            watches.clear()
            trees = emptyMap()
        }
        thread.quitSafely()
    }
}
