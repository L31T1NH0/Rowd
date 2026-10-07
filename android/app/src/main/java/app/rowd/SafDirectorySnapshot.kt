package app.rowd

import android.content.ContentResolver
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document

internal data class SafStructuralEntry(val path: String, val uri: String, val directory: Boolean)

internal fun safStructureMatches(expected: List<SafStructuralEntry>, actual: List<SafStructuralEntry>): Boolean =
    actual.size == expected.size && actual.toSet() == expected.toSet()

/** Query the exact document using the original tree grant, never a SingleDocumentFile. */
internal fun safDirectorySnapshot(
    resolver: ContentResolver, tree: Uri, directory: Uri, prefix: String,
    ignored: (String, Boolean) -> Boolean, checkControl: () -> Unit
): List<SafStructuralEntry> {
    checkControl()
    check(DocumentsContract.isTreeUri(tree) && DocumentsContract.isTreeUri(directory) &&
        tree.scheme == "content" && directory.scheme == "content" && directory.authority == tree.authority &&
        DocumentsContract.getTreeDocumentId(directory) == DocumentsContract.getTreeDocumentId(tree)) {
        "STALE_SOURCE: $prefix, tree identity changed"
    }
    val documentId = DocumentsContract.getDocumentId(directory)
    // The snapshot supplies the exact directory identity. The binding may itself
    // select a tree-scoped subdirectory; do not replace its ID with the grant root.
    check(directory == DocumentsContract.buildDocumentUriUsingTree(tree, documentId)) {
        "STALE_SOURCE: $prefix, directory identity changed"
    }
    // A vanished/retyped empty directory must not look like a successful empty listing.
    val projection = arrayOf(Document.COLUMN_DOCUMENT_ID, Document.COLUMN_DISPLAY_NAME, Document.COLUMN_MIME_TYPE)
    (resolver.query(directory, projection, null, null, null) ?: error("STALE_SOURCE: $prefix")).use { cursor ->
        checkControl()
        check(cursor.moveToFirst() && cursor.getString(0) == documentId &&
            cursor.getString(2) == Document.MIME_TYPE_DIR &&
            (prefix.isEmpty() || cursor.getString(1) == prefix.substringAfterLast('/')) && !cursor.moveToNext()) {
            "STALE_SOURCE: $prefix"
        }
    }
    val childrenUri = DocumentsContract.buildChildDocumentsUriUsingTree(tree, documentId)
    val result = mutableListOf<SafStructuralEntry>()
    (resolver.query(childrenUri, projection, null, null, null) ?: error("STALE_SOURCE: $prefix")).use { cursor ->
        checkControl()
        check(!cursor.extras.getBoolean(DocumentsContract.EXTRA_LOADING, false)) { "STALE_SOURCE: $prefix, incomplete listing" }
        while (cursor.moveToNext()) {
            checkControl()
            val id = cursor.getString(0) ?: error("Documento sem identidade no provedor.")
            val name = cursor.getString(1) ?: error("Arquivo sem nome no provedor.")
            val path = if (prefix.isEmpty()) name else "$prefix/$name"
            val isDirectory = cursor.getString(2) == Document.MIME_TYPE_DIR
            if (!ignored(path, isDirectory)) result.add(SafStructuralEntry(
                path, DocumentsContract.buildDocumentUriUsingTree(tree, id).toString(), isDirectory))
        }
    }
    checkControl()
    return result
}
