package app.rowd

import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.FutureTask
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/** Two chunks of metadata, never unbounded production. Cancellation unblocks put. */
internal class ScanStreamWorker<T>(private val discard: (T) -> Unit = {}, private val backpressure: (Boolean) -> Unit = {}) {
    private val queue = ArrayBlockingQueue<T>(2)
    private val cancelled = AtomicBoolean(false)
    private var task: FutureTask<T>? = null
    @Volatile var peakChunks = 0; private set
    @Volatile var waitMs = 0L; private set
    @Synchronized fun hasTask() = task != null

    fun checkControl() { check(!cancelled.get()) { "AUDIT_DEFERRED" } }
    @Synchronized fun start(produce: ((T) -> Unit) -> T) {
        check(task == null)
        queue.clear(); cancelled.set(false); peakChunks = 0; waitMs = 0
        val next = FutureTask<T> {
            produce { chunk ->
                try {
                    checkControl()
                    val started = System.nanoTime()
                    if (!queue.offer(chunk)) {
                        backpressure(true)
                        try {
                            while (!queue.offer(chunk, 50, TimeUnit.MILLISECONDS)) checkControl()
                        } finally {
                            waitMs += (System.nanoTime() - started) / 1_000_000
                            backpressure(false)
                        }
                    }
                    peakChunks = maxOf(peakChunks, queue.size)
                } catch (error: Exception) {
                    discard(chunk)
                    throw error
                }
            }
        }
        task = next
        Thread(next, "Rowd SAF hash stream").start()
    }
    @Synchronized fun poll(): T? {
        val current = task ?: error("Nenhum stream SAF em andamento.")
        queue.poll()?.let { return it }
        if (!current.isDone) return null
        val terminal = current.get() // Propagate a worker failure, never publish partial hashes.
        // The producer may publish between the first poll and isDone.
        return queue.poll() ?: terminal
    }
    fun cancel() { cancelled.set(true) }
    fun finish() {
        cancel() // A producer blocked on a full queue must exit before joining it.
        val current = synchronized(this) { task }
        var interrupted = false
        try {
            while (current != null) {
                    try { discard(current.get()); break }
                catch (_: InterruptedException) { interrupted = true }
                catch (_: java.util.concurrent.ExecutionException) { break }
            }
        } finally {
            synchronized(this) {
                task = null
                while (true) discard(queue.poll() ?: break)
            }
            if (interrupted) Thread.currentThread().interrupt()
        }
    }
}
