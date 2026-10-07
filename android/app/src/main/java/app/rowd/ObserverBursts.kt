package app.rowd

/** Fixed window from the first directory/generic callback: notifications never postpone work. */
internal class ObserverBursts {
    companion object { const val WINDOW_MS = 100L }
    data class Burst(val provider: String, val startedAt: Long, var callbacks: Int = 0,
        val shares: MutableMap<String, Long> = linkedMapOf())
    private val pending = linkedMapOf<String, Burst>()
    @Synchronized fun add(provider: String, share: String, at: Long): Boolean {
        val schedule = pending.isEmpty()
        val burst = pending.getOrPut(provider) { Burst(provider, at) }
        burst.callbacks++
        burst.shares.putIfAbsent(share, at)
        return schedule
    }
    @Synchronized fun flush(): List<Burst> = pending.values.toList().also { pending.clear() }
}
