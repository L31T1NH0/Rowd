package app.rowd

import java.util.concurrent.FutureTask

/** Result consumption and worker termination are separate operations. */
internal class ScanWorker {
    enum class State { IDLE, RUNNING, COMPLETED, CONSUMED, CANCEL_REQUESTED }
    @Volatile private var task: FutureTask<String>? = null
    var state = State.IDLE
        private set
    fun hasTask() = task != null

    @Synchronized fun start(scan: () -> String) {
        check(task == null) { "Scan SAF já em andamento." }
        val next = FutureTask<String> { scan() }
        task = next
        state = State.RUNNING
        Thread(next, "Rowd SAF scan").start()
    }

    @Synchronized fun poll(): String {
        val current = task ?: error("Nenhum scan SAF em andamento.")
        if (!current.isDone) return ""
        state = State.COMPLETED
        return try { current.get() } finally {
            task = null
            state = State.CONSUMED
        }
    }

    @Synchronized fun requestCancel() {
        if (task != null) state = State.CANCEL_REQUESTED
    }

    /** Join without polling/consuming a second time. Safe after poll and on idle. */
    @Synchronized fun finish() {
        val current = task
        // Do not release the task on thread interruption before the worker ends.
        var interrupted = false
        try {
            if (current != null) {
                while (true) {
                    try { current.get(); break }
                    catch (_: InterruptedException) { interrupted = true }
                }
            }
        } finally {
            task = null
            state = State.IDLE
            if (interrupted) Thread.currentThread().interrupt()
        }
    }
}
