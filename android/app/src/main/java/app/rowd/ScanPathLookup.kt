package app.rowd

internal data class SafMetadata(val path: String, val uri: String, val directory: Boolean,
    val virtual: Boolean, val modified: Long, val length: Long) {
    fun documentMetadata() = DocumentMetadata(uri, modified, length)
}

/** One fresh listing per directory in this scan; never a persistent path authority. */
internal class ScanPathLookup(root: String, private val read: (String, String) -> List<SafMetadata>) {
    private val directories = mutableMapOf<String, String?>("" to root)
    private val listings = linkedMapOf<String, Map<String, SafMetadata>>()
    fun directory(prefix: String): String? {
        if (directories.containsKey(prefix)) return directories[prefix]
        val entry = children(prefix.substringBeforeLast('/', ""))[prefix]
        check(entry == null || entry.directory) { "Um arquivo ocupa o lugar da pasta: $prefix" }
        return entry?.uri.also { directories[prefix] = it }
    }
    fun children(prefix: String): Map<String, SafMetadata> {
        listings[prefix]?.let { return it }
        val uri = directory(prefix) ?: return emptyMap()
        val entries = read(uri, prefix).associateBy { it.path }
        listings[prefix] = entries
        return entries
    }
    fun find(path: String): SafMetadata? = children(path.substringBeforeLast('/', ""))[path]

    /** Cached listings resolve names; fresh exact queries still validate snapshot ancestors. */
    fun validateAncestors(path: String, read: (String, String) -> SafMetadata) {
        val names = path.split('/')
        for (index in names.indices) {
            val prefix = names.take(index).joinToString("/")
            val uri = directory(prefix) ?: error("STALE_SOURCE: $path")
            val current = read(uri, prefix)
            check(current.uri == uri && current.directory && !current.virtual &&
                (prefix.isEmpty() || current.path == prefix)) { "STALE_SOURCE: $path" }
        }
    }

    /** Recheck selected names and every traversed ancestor, including previously absent names. */
    fun validate(paths: Set<String>) {
        val relevant = (paths + directories.keys).groupBy { it.substringBeforeLast('/', "") }
        for ((prefix, before) in listings) {
            val after = read(directories.getValue(prefix)!!, prefix).associateBy { it.path }
            for (path in relevant[prefix].orEmpty()) {
                val old = before[path]
                val current = after[path]
                val same = if (old?.directory == true && current?.directory == true)
                    old.uri == current.uri && old.virtual == current.virtual else old == current
                check(same) { "STALE_SOURCE: associação de caminho mudou: $path" }
            }
        }
    }
}
