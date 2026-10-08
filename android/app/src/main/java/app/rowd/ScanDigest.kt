package app.rowd

import java.io.InputStream
import java.io.OutputStream
import java.security.MessageDigest

/** Control is optional: snapshot/recovery callers retain their own cancellation semantics. */
internal fun scanDigest(input: InputStream, copy: OutputStream? = null,
    legacy: Boolean = false, checkControl: () -> Unit = {}): Pair<String, Long> {
    if (!legacy) return input.use { stream ->
        val result = ContentDigest.hashStream(stream, copy, checkControl).split(':')
        result[0] to result[1].toLong()
    }
    val md = MessageDigest.getInstance("SHA-256")
    var size = 0L
    val buffer = ByteArray(64 * 1024)
    input.use { stream ->
        while (true) {
            checkControl()
            val count = stream.read(buffer)
            checkControl()
            if (count < 0) break
            copy?.write(buffer, 0, count)
            md.update(buffer, 0, count)
            size += count
            check(size <= 8L * 1024 * 1024 * 1024) { "Arquivo maior que 8 GiB." }
        }
    }
    return md.digest().joinToString("") { "%02x".format(it.toInt() and 255) } to size
}

/** Publish a digest only when it belongs to the same document snapshot. */
internal fun verifiedScanDigest(input: InputStream, before: DocumentMetadata,
    after: () -> DocumentMetadata, checkControl: () -> Unit = {}): Pair<String, Long> {
    val result = scanDigest(input, checkControl = checkControl)
    checkControl()
    val current = after()
    checkControl()
    check(current == before && result.second == before.length) {
        "STALE_SOURCE: documento mudou durante o hash ou tamanho inconsistente."
    }
    return result
}

internal fun scanShouldAbort(explicit: Boolean, audit: Boolean, generationChanged: Boolean) =
    explicit || (audit && generationChanged)
