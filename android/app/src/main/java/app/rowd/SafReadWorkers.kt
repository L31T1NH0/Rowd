package app.rowd

import java.util.concurrent.FutureTask
import java.util.concurrent.Semaphore
import java.util.concurrent.TimeUnit

/** Private read results only. A provider shares two fair credits across all shares. */
internal object SafReadWorkers {
    private val providers = mutableMapOf<String, Semaphore>()
    fun <T, R> read(provider: String, jobs: List<T>, checkControl: () -> Unit, read: (T) -> R): List<R> {
        require(jobs.size <= 2)
        val credits = synchronized(providers) { providers.getOrPut(provider) { Semaphore(2, true) } }
        val tasks = jobs.map { job ->
            FutureTask<R> {
                while (!credits.tryAcquire(50, TimeUnit.MILLISECONDS)) checkControl()
                try { checkControl(); read(job) } finally { credits.release() }
            }.also { Thread(it, "Rowd SAF private read").start() }
        }
        var failure: Throwable? = null
        var interrupted = false
        val results = mutableListOf<R>()
        for (task in tasks) {
            while (true) {
                try { results.add(task.get()); break }
                catch (_: InterruptedException) { interrupted = true }
                catch (error: java.util.concurrent.ExecutionException) { if (failure == null) failure = error.cause ?: error; break }
            }
        }
        if (interrupted) Thread.currentThread().interrupt()
        failure?.let { throw it }
        return results
    }
}
