package app.rowd

internal fun metadataRoots(prefixes: Set<String>): List<String> {
    if ("" in prefixes) return listOf("")
    val roots = linkedSetOf<String>()
    for (prefix in prefixes.sortedBy { it.length }) {
        if (prefix.indices.none { prefix[it] == '/' && prefix.substring(0, it) in roots }) roots.add(prefix)
    }
    return roots.toList()
}

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
