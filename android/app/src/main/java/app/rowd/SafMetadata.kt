package app.rowd

import android.content.ContentResolver
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document

private val metadataColumns = arrayOf(Document.COLUMN_DOCUMENT_ID, Document.COLUMN_DISPLAY_NAME,
    Document.COLUMN_MIME_TYPE, Document.COLUMN_LAST_MODIFIED, Document.COLUMN_SIZE, Document.COLUMN_FLAGS)

private fun readSafMetadata(resolver: ContentResolver, tree: Uri, query: Uri, prefix: String,
    checkControl: () -> Unit, validateName: Boolean = true): List<SafMetadata> {
    checkControl()
    val entries = mutableListOf<SafMetadata>()
    val names = mutableSetOf<String>()
    val ids = mutableSetOf<String>()
    (resolver.query(query, metadataColumns, null, null, null) ?: error("Provider não retornou metadados.")).use { cursor ->
        checkControl()
        check(!cursor.extras.getBoolean(DocumentsContract.EXTRA_LOADING, false)) { "Listagem SAF incompleta." }
        while (cursor.moveToNext()) {
            checkControl()
            check(entries.size < 200_000) { "Limite de listagem SAF excedido." }
            val id = cursor.getString(0) ?: error("Documento sem ID.")
            val name = cursor.getString(1) ?: error("Documento sem nome.")
            val mime = cursor.getString(2) ?: error("Documento sem tipo.")
            check(mime.isNotEmpty()) { "Documento sem tipo." }
            check(ids.add(id) && names.add(name)) { "Identidade ou nome duplicado no provedor." }
            if (validateName) check(name.isNotEmpty() && name != "." && name != ".." &&
                name.none { it == '/' || it == '\\' || it == '\u0000' }) { "Nome de documento inválido." }
            val directory = mime == Document.MIME_TYPE_DIR
            val length = if (directory) 0L else cursor.getLong(4)
            check(length >= 0) { "Tamanho de documento inválido." }
            entries.add(SafMetadata(if (prefix.isEmpty()) name else "$prefix/$name",
                DocumentsContract.buildDocumentUriUsingTree(tree, id).toString(), directory,
                cursor.getInt(5) and Document.FLAG_VIRTUAL_DOCUMENT != 0, cursor.getLong(3), length))
        }
        check(!cursor.extras.getBoolean(DocumentsContract.EXTRA_LOADING, false)) { "Listagem SAF incompleta." }
    }
    checkControl()
    return entries
}

internal fun safDocumentMetadata(resolver: ContentResolver, tree: Uri, document: Uri, path: String,
    checkControl: () -> Unit = {}): SafMetadata {
    check(document.authority == tree.authority && DocumentsContract.isTreeUri(document) &&
        DocumentsContract.getTreeDocumentId(document) == DocumentsContract.getTreeDocumentId(tree)) { "Identidade de árvore SAF mudou." }
    val rows = readSafMetadata(resolver, tree, document, path.substringBeforeLast('/', ""), checkControl, validateName = false)
    val entry = rows.singleOrNull() ?: error("Documento SAF ausente ou ambíguo.")
    check(entry.uri == document.toString() && (path.isEmpty() || entry.path == path)) { "STALE_SOURCE: $path" }
    return entry
}

internal fun safDirectoryMetadata(resolver: ContentResolver, tree: Uri, directory: Uri, prefix: String,
    checkControl: () -> Unit): List<SafMetadata> {
    val parent = safDocumentMetadata(resolver, tree, directory, prefix, checkControl)
    check(parent.directory && !parent.virtual) { "Diretório SAF ausente ou reclassificado: $prefix" }
    return readSafMetadata(resolver, tree, DocumentsContract.buildChildDocumentsUriUsingTree(tree,
        DocumentsContract.getDocumentId(directory)), prefix, checkControl)
}
