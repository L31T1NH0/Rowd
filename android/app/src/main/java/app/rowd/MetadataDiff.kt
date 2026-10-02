package app.rowd

/** Recursive discovery. File metadata can reuse a hash; it cannot prove a subtree unchanged. */
internal class MetadataDiff(private val cached: Map<String, DocumentMetadata>, val startedAt: Long) {
    var directoriesVisited = 0
        private set
    var entriesEnumerated = 0
        private set
    val changed = linkedSetOf<String>()
    private val seen = mutableSetOf<String>()
    fun directory() { directoriesVisited++ }
    fun entry() { entriesEnumerated++ }
    fun file(path: String, metadata: DocumentMetadata) {
        seen.add(path)
        if (metadata.differsFrom(cached[path])) changed.add(path)
    }
    fun finish(prefix: String) {
        changed.addAll(cached.keys.filter { (prefix.isEmpty() || it.startsWith("$prefix/")) && it !in seen })
    }
    fun pathsHashed(hashed: Set<String>): Int = changed.count { it in hashed }
}
