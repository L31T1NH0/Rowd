package app.rowd

/** Session observations use verified content, not scan focus, to identify our own installs. */
internal class FileObservations {
    private val seen = mutableSetOf<String>()
    private val installed = mutableMapOf<String, String>()
    fun clear() { seen.clear(); installed.clear() }
    fun remoteInstalled(id: String, hash: String) { installed[id] = hash }
    fun firstSeen(id: String, hash: String?): String? {
        if (!seen.add(id)) return null
        return if (hash != null && installed[id] == hash) "remote_install" else "unknown"
    }
}
