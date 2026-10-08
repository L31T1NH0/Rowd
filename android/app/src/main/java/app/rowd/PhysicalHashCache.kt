package app.rowd

import java.io.*
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.security.MessageDigest
import java.security.DigestOutputStream

/** Physical evidence only. Never provides paths, manifests, ACKs or base tokens. */
internal class PhysicalHashCache(private val directory: File) {
    internal data class Entry(val uri: String, val modified: Long, val length: Long,
        val hash: String, val size: Long, val generation: Long? = null)
    private var identity = ""
    private var share = ""
    private var entries = mutableMapOf<String, Entry>()
    private var file: File? = null
    private var dirty = false
    private var lastSaveAttempt = System.nanoTime() / 1_000_000

    fun select(shareId: String, binding: String) {
        if (share == shareId && identity == binding) return
        share = shareId; identity = binding; entries.clear()
        dirty = false; lastSaveAttempt = System.nanoTime() / 1_000_000
        val name = MessageDigest.getInstance("SHA-256").digest(shareId.toByteArray())
            .joinToString("") { "%02x".format(it.toInt() and 255) }
        file = File(directory, "$name.hashes")
        try {
            val loaded = mutableMapOf<String, Entry>()
            check(file!!.length() in 32..64L * 1024 * 1024)
            val bytes = file!!.readBytes()
            val payload = bytes.copyOfRange(0, bytes.size - 32)
            check(MessageDigest.isEqual(MessageDigest.getInstance("SHA-256").digest(payload), bytes.copyOfRange(bytes.size - 32, bytes.size)))
            DataInputStream(ByteArrayInputStream(payload)).use { input ->
                check(input.readInt() == 2 && input.readUTF() == share && input.readUTF() == identity)
                val count = input.readInt(); check(count in 0..100_000)
                repeat(count) {
                    val path = input.readUTF()
                    val entry = Entry(input.readUTF(), input.readLong(), input.readLong(), input.readUTF(), input.readLong())
                    check(valid(entry) && loaded.put(path, entry) == null)
                }
                check(input.read() == -1)
            }
            entries = loaded
        } catch (_: Exception) { entries.clear() } // Missing, stale or corrupt cache => hash again.
    }
    private fun valid(entry: Entry) = entry.modified > 0 && entry.length >= 0 &&
        entry.length == entry.size && entry.size <= 8L * 1024 * 1024 * 1024 &&
        entry.hash.matches(Regex("[0-9a-f]{64}"))

    fun lookup(path: String, uri: String, modified: Long, length: Long, dirtyGeneration: Long? = null): Entry? =
        entries[path]?.takeIf { valid(it) && it.uri == uri && it.modified == modified && it.length == length &&
            (dirtyGeneration == null || it.generation == dirtyGeneration) }

    fun remember(path: String, entry: Entry) {
        if (valid(entry)) {
            if (path !in entries && entries.size >= 100_000) entries.remove(entries.keys.first())
            if (entries.put(path, entry) != entry) dirty = true
        } else invalidate(path)
    }
    fun retainPaths(paths: Set<String>) { if (entries.keys.retainAll(paths)) dirty = true }
    fun invalidate(path: String) { if (entries.remove(path) != null) dirty = true }

    /** Failed persistence is optional evidence; keep dirty memory and throttle retries. */
    fun saveIfDue(force: Boolean = false, nowMillis: Long = System.nanoTime() / 1_000_000): Exception? {
        if (!dirty || (!force && nowMillis - lastSaveAttempt < 15_000)) return null
        lastSaveAttempt = nowMillis
        return try { save(); null } catch (error: Exception) { error }
    }

    fun save() {
        val target = file ?: return
        directory.mkdirs()
        val temp = File.createTempFile("hash-cache-", ".tmp", directory)
        try {
            FileOutputStream(temp).use { raw ->
                val digest = MessageDigest.getInstance("SHA-256")
                val output = DataOutputStream(BufferedOutputStream(DigestOutputStream(raw, digest)))
                output.writeInt(2); output.writeUTF(share); output.writeUTF(identity)
                output.writeInt(entries.size)
                entries.toSortedMap().forEach { (path, entry) ->
                    output.writeUTF(path); output.writeUTF(entry.uri); output.writeLong(entry.modified)
                    output.writeLong(entry.length); output.writeUTF(entry.hash); output.writeLong(entry.size)
                }
                output.flush(); raw.write(digest.digest()); raw.fd.sync()
            }
            Files.move(temp.toPath(), target.toPath(), StandardCopyOption.ATOMIC_MOVE, StandardCopyOption.REPLACE_EXISTING)
            dirty = false
        } finally { temp.delete() }
    }
}
