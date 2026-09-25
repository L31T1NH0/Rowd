package app.rowd

import android.content.Context
import android.os.SystemClock
import org.json.JSONObject
import java.io.BufferedWriter
import java.io.File
import java.io.FileWriter
import java.security.MessageDigest
import java.util.zip.ZipEntry
import java.util.zip.ZipOutputStream

object PerformanceTrace {
    private const val KOTLIN_FILE = "performance-trace-android-kotlin.jsonl"
    private const val RUST_FILE = "performance-trace-android-rust.jsonl"
    private var writer: BufferedWriter? = null
    private var started = 0L
    @Volatile private var active = false

    fun enabled() = active

    @Synchronized fun enable(context: Context) {
        active = false
        writer?.close()
        val kotlin = File(context.filesDir, KOTLIN_FILE)
        val rust = File(context.filesDir, RUST_FILE)
        val next = FileWriter(kotlin, false).buffered()
        try { check(NativeBridge.setTrace(rust.absolutePath)) { "Não foi possível abrir o trace Rust." } }
        catch (error: Exception) { next.close(); throw error }
        writer = next
        started = SystemClock.elapsedRealtimeNanos()
        active = true
    }

    @Synchronized fun disable() {
        active = false
        writer?.close()
        writer = null
        check(NativeBridge.setTrace("")) { "Não foi possível fechar o trace Rust." }
    }

    @Synchronized fun flush() { writer?.flush(); NativeBridge.flushTrace() }

    fun fileId(share: String, path: String): String {
        val bytes = MessageDigest.getInstance("SHA-256")
            .digest((share + "\u0000" + path).toByteArray(Charsets.UTF_8))
        return bytes.take(8).joinToString("") { "%02x".format(it.toInt() and 255) }
    }

    @Synchronized fun event(name: String, share: String?, path: String? = null, bytes: Long? = null, start: Long? = null) {
        if (!active) return
        try {
            val json = JSONObject().put("wall_ms", System.currentTimeMillis())
                .put("elapsed_us", (SystemClock.elapsedRealtimeNanos() - started) / 1000)
                .put("side", "android").put("component", "saf").put("event", name)
            if (share != null) json.put("share_id", share)
            if (share != null && path != null) json.put("file_id", fileId(share, path))
            if (bytes != null) json.put("bytes", bytes)
            if (start != null) json.put("duration_us", (SystemClock.elapsedRealtimeNanos() - start) / 1000)
            writer?.write(json.toString() + "\n")
        } catch (_: Exception) {
            active = false
            runCatching { writer?.close() }
            writer = null
            runCatching { NativeBridge.setTrace("") }
        }
    }

    fun now() = SystemClock.elapsedRealtimeNanos()

    fun export(context: Context, destination: android.net.Uri) {
        flush()
        context.contentResolver.openOutputStream(destination)?.use { output ->
            ZipOutputStream(output).use { zip ->
                for (name in listOf(KOTLIN_FILE, RUST_FILE)) {
                    zip.putNextEntry(ZipEntry(name))
                    File(context.filesDir, name).takeIf { it.exists() }?.inputStream()?.use { it.copyTo(zip) }
                    zip.closeEntry()
                }
            }
        } ?: error("Não foi possível abrir o destino do trace.")
    }
}
