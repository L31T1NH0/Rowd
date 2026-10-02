package app.rowd

internal enum class ObserverHint { KNOWN_FILE_URI, KNOWN_DIRECTORY_URI, PROVIDER_WIDE_URI, NULL_URI, UNKNOWN_SPECIFIC_URI, UNRELATED_URI }

internal fun observerHint(uri: String?, authority: String?, treeAuthority: String?, segments: List<String>,
    knownFile: Boolean, knownDirectory: Boolean, treeId: String?, documentId: String?): ObserverHint = when {
    uri == null -> ObserverHint.NULL_URI
    authority != treeAuthority -> ObserverHint.UNRELATED_URI
    knownFile -> ObserverHint.KNOWN_FILE_URI
    knownDirectory -> ObserverHint.KNOWN_DIRECTORY_URI
    segments.isEmpty() -> ObserverHint.PROVIDER_WIDE_URI
    // ExternalStorage document IDs encode the tree path. Other providers may use opaque IDs.
    authority == "com.android.externalstorage.documents" && treeId != null && documentId != null &&
        documentId != treeId && !documentId.startsWith("$treeId/") -> ObserverHint.UNRELATED_URI
    else -> ObserverHint.UNKNOWN_SPECIFIC_URI
}

internal data class DocumentMetadata(val uri: String, val modified: Long, val length: Long) {
    fun differsFrom(cached: DocumentMetadata?): Boolean = cached == null || uri != cached.uri ||
        modified <= 0 || modified != cached.modified || length != cached.length
}
