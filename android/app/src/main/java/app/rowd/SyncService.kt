package app.rowd

import android.app.*
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.net.ConnectivityManager
import android.net.Network
import android.net.LinkProperties
import android.os.IBinder
import android.os.Handler
import android.os.Looper
import org.json.JSONObject
import org.json.JSONArray
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.CopyOnWriteArraySet
import java.net.Inet4Address

internal data class NetworkPath(val network: Long?, val interfaceName: String?, val ipv4: String?) {
    fun changedFrom(previous: NetworkPath): Boolean =
        network != previous.network ||
            (interfaceName != null && previous.interfaceName != null && interfaceName != previous.interfaceName) ||
            (ipv4 != null && previous.ipv4 != null && ipv4 != previous.ipv4)

    fun retainingKnown(previous: NetworkPath): NetworkPath =
        if (network == previous.network) NetworkPath(network, interfaceName ?: previous.interfaceName, ipv4 ?: previous.ipv4)
        else this
}

data class RowdStatus(val kind: Kind, val title: String, val detail: String) {
    enum class Kind { Idle, Working, Ready, NeedsAttention, Error, Paused }
}

class SyncService : Service() {
    companion object {
        const val STOP = "app.rowd.STOP"
        val busy = AtomicBoolean(false)
        private val listeners = CopyOnWriteArraySet<() -> Unit>()
        private val main = Handler(Looper.getMainLooper())
        private fun changed() { main.post { listeners.forEach { it() } } }
        fun observe(listener: () -> Unit) { listeners.add(listener) }
        fun stopObserving(listener: () -> Unit) { listeners.remove(listener) }
        @Volatile var state = RowdStatus(RowdStatus.Kind.Idle, "Vamos conectar seus Shares", "Pareie o PC e escolha uma pasta Android para cada Share.")
            private set
        fun publish(kind: RowdStatus.Kind, title: String, detail: String) {
            state = RowdStatus(kind, title, detail)
            changed()
        }
        fun restore(context: Context) {
            val prefs = context.getSharedPreferences("rowd", Context.MODE_PRIVATE)
            val title = prefs.getString("lastStatus", null) ?: return
            val detail = prefs.getString("lastDetail", null) ?: return
            val kind = runCatching { RowdStatus.Kind.valueOf(prefs.getString("lastStatusKind", "Idle")!!) }
                .getOrDefault(RowdStatus.Kind.Idle)
            publish(kind, title, detail)
        }
        private val changes = Object()
        private var dirty = false
        private var generation = 0L
        fun changeGeneration(): Long = synchronized(changes) { generation }
        private var dirtyAllGeneration = 0L
        private val dirtyShares = mutableMapOf<String, Long>()
        private val detectedAt = mutableMapOf<String, Long>()

        fun wake(shareId: String? = null) = synchronized(changes) {
            generation++
            if (shareId == null) dirtyAllGeneration = generation
            else dirtyShares[shareId] = generation
            detectedAt[shareId ?: "*"] = android.os.SystemClock.elapsedRealtime()
            android.util.Log.i("RowdLatency", "change_detected_at=${detectedAt[shareId ?: "*"]} share=${shareId ?: "*"}")
            dirty = true
            changes.notifyAll()
        }
    }
    private val active = AtomicBoolean(false)
    private var worker: Thread? = null
    private var networkCallback: ConnectivityManager.NetworkCallback? = null
    private var networkPath: NetworkPath? = null
    private fun observeNetwork() {
        val manager = getSystemService(ConnectivityManager::class.java)
        fun path(): NetworkPath {
            val network = manager.activeNetwork ?: return NetworkPath(null, null, null)
            val links = manager.getLinkProperties(network)
            val ipv4 = links?.linkAddresses?.mapNotNull { it.address as? Inet4Address }
                ?.filter { !it.isLoopbackAddress && !it.isLinkLocalAddress }
                ?.map { it.hostAddress }?.sorted()?.firstOrNull()
            return NetworkPath(network.networkHandle, links?.interfaceName, ipv4)
        }
        networkPath = path()
        val callback = object : ConnectivityManager.NetworkCallback() {
            private fun changed() {
                val current = path()
                val changed = synchronized(this@SyncService) {
                    val previous = networkPath ?: current
                    networkPath = current.retainingKnown(previous)
                    current.changedFrom(previous)
                }
                if (!changed) return
                NativeBridge.networkChanged()
                wake()
            }
            override fun onAvailable(network: Network) = changed()
            override fun onLost(network: Network) = changed()
            override fun onLinkPropertiesChanged(network: Network, linkProperties: LinkProperties) = changed()
        }
        networkCallback = callback
        manager.registerDefaultNetworkCallback(callback)
    }
    private fun notifyStatus(text: String) {
        if (android.os.Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) != android.content.pm.PackageManager.PERMISSION_GRANTED) return
        getSystemService(NotificationManager::class.java).notify(1, notification(text))
    }
    private fun notification(text: String): Notification {
        val open = PendingIntent.getActivity(this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
        val stop = PendingIntent.getService(this, 1, Intent(this, SyncService::class.java).setAction(STOP), PendingIntent.FLAG_IMMUTABLE)
        return Notification.Builder(this, "sync").setSmallIcon(R.drawable.ic_sync)
            .setContentTitle("Rowd").setContentText(text).setContentIntent(open).setOngoing(true)
            .addAction(Notification.Action.Builder(null, "Parar", stop).build()).build()
    }
    override fun onCreate() {
        super.onCreate()
        observeNetwork()
        if (getSharedPreferences("rowd", MODE_PRIVATE).getBoolean("performanceTrace", false) && !PerformanceTrace.enabled()) {
            runCatching { PerformanceTrace.enable(this) }
                .onFailure { android.util.Log.e("RowdTrace", "Não foi possível iniciar o trace", it) }
        }
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel("sync", "Sincronização", NotificationManager.IMPORTANCE_LOW)
        )
    }
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == STOP) {
            active.set(false)
            publish(RowdStatus.Kind.Paused, "Finalizando a rodada atual", state.detail)
            NativeBridge.cancel()
            worker?.interrupt()
            return START_NOT_STICKY
        }
        if (!busy.compareAndSet(false, true)) {
            if (!active.get()) stopSelf()
            return START_NOT_STICKY
        }
        startForeground(1, notification("Conectando ao PC"))
        NativeBridge.resetCancellation()
        active.set(true)
        worker = Thread({
            val prefs = getSharedPreferences("rowd", MODE_PRIVATE)
            var failures = 0
            val observers = mutableMapOf<Uri, android.database.ContentObserver>()
            var observedTrees = emptyMap<Uri, String?>()
            var lastAudit = 0L
            var lastDeepAudit = 0L
            var auditDeferred = false
            var auditCursor = 0
            var deepAuditCursor = 0
            var completedAllGeneration = 0L
            try {
                val access = FolderAccess(this)
                fun refreshObservers() {
                    val current = access.observedShareTrees()
                    if (current == observedTrees) return
                    observers.values.forEach(contentResolver::unregisterContentObserver)
                    observers.clear()
                    current.forEach { (tree, shareId) ->
                        val observer = object : android.database.ContentObserver(Handler(Looper.getMainLooper())) {
                            override fun onChange(selfChange: Boolean) {
                                if (access.noteChange(shareId, null, selfChange)) wake(shareId) else wake()
                            }
                            override fun onChange(selfChange: Boolean, uri: Uri?) {
                                if (access.noteChange(shareId, uri, selfChange)) wake(shareId) else wake()
                            }
                        }
                        try {
                            contentResolver.registerContentObserver(tree, true, observer)
                            observers[tree] = observer
                        } catch (_: Exception) { /* Full audit remains the fallback. */ }
                    }
                    observedTrees = current
                }
                do {
                    try {
                        refreshObservers()
                        var invitation = prefs.getString("invitation", null) ?: error("Importe o convite do PC.")
                        if (!invitation.startsWith("rowd1:")) {
                            val preview = JSONObject(NativeBridge.previewInvitation(invitation, ""))
                            check(!preview.has("error")) { preview.optString("error", "Convite inválido.") }
                            invitation = preview.getString("invitation")
                        }
                        val device = prefs.getString("deviceId", null) ?: error("Abra o Rowd novamente para criar a identidade do aparelho.")
                        val now = android.os.SystemClock.elapsedRealtime()
                        val (full, allVersion, selected) = synchronized(changes) {
                            dirty = false
                            val selected = dirtyShares.toMap()
                            val auditDue = lastAudit == 0L || now - lastAudit >= 60_000L
                            // A known Share goes first; the overdue audit runs on the next pass.
                            val focusedFirst = auditDue && selected.isNotEmpty() && !auditDeferred &&
                                dirtyAllGeneration == completedAllGeneration
                            if (focusedFirst) auditDeferred = true
                            Triple(dirtyAllGeneration != completedAllGeneration || (auditDue && !focusedFirst),
                                dirtyAllGeneration, selected)
                        }
                        val periodic = full && dirtyAllGeneration == completedAllGeneration
                        val eligible = if (periodic) JSONArray(access.availableShares()).let { shares ->
                            (0 until shares.length()).map(shares::getString).sorted()
                        } else emptyList()
                        val auditShare = eligible.takeIf { it.isNotEmpty() }?.let { it[auditCursor % it.size] }
                        val deep = auditShare != null && eligible[deepAuditCursor % eligible.size] == auditShare &&
                            (lastDeepAudit == 0L || now - lastDeepAudit >= 15 * 60_000L)
                        if (auditShare != null) access.scheduleAudit(auditShare, deep)
                        val focus = when {
                            !full -> JSONArray(selected.keys.toList()).toString()
                            periodic -> JSONArray(listOfNotNull(auditShare)).toString()
                            else -> ""
                        }
                        access.setFocusedScan(focus.isNotEmpty())
                        access.setAuditRound(full)
                        if (!prefs.contains("invitation")) break
                        publish(RowdStatus.Kind.Working,
                            if (full) "Verificando arquivos" else "Sincronizando alterações",
                            if (full) "Auditoria periódica dos Shares." else "Verificando os Shares alterados.")
                        notifyStatus(state.title)
                        val startedAt = android.os.SystemClock.elapsedRealtime()
                        val activationMs = synchronized(changes) {
                            selected.keys.mapNotNull { detectedAt[it]?.let { time -> startedAt - time } }.maxOrNull() ?: 0L
                        }
                        android.util.Log.i("RowdLatency", "round_started_at=$startedAt focus=${if (full) "audit" else focus} activation_ms=$activationMs")
                        val result = JSONObject(NativeBridge.sync(invitation, device, focus, access))
                        if (result.has("error")) error(result.getString("error"))
                        if (!prefs.contains("invitation")) {
                            publish(RowdStatus.Kind.Idle, "Celular desvinculado", "Pareie novamente para continuar.")
                            break
                        }
                        result.optJSONObject("metrics")?.put("activation_ms", activationMs)
                        val roundDeferred = result.optBoolean("round_deferred")
                        val completedAt = android.os.SystemClock.elapsedRealtime()
                        android.util.Log.i("RowdLatency", "round_completed_at=$completedAt duration_ms=${completedAt - startedAt} transferred=${result.optInt("transferred")}")
                        result.optJSONArray("pending_wakes")?.let { pending ->
                            for (index in 0 until pending.length()) wake(pending.getString(index))
                        }
                        refreshObservers() // configureShares may have changed bindings in this round.
                        synchronized(changes) {
                            if (roundDeferred) {
                                auditDeferred = false
                            } else if (full) {
                                lastAudit = android.os.SystemClock.elapsedRealtime()
                                auditDeferred = false
                                if (auditShare != null) {
                                    auditCursor++
                                    if (deep) {
                                        deepAuditCursor++
                                        lastDeepAudit = lastAudit
                                    }
                                }
                                if (dirtyAllGeneration == allVersion) completedAllGeneration = allVersion
                            }
                            if (!roundDeferred) {
                                selected.forEach { (id, version) ->
                                    if (dirtyShares[id] == version) { dirtyShares.remove(id); detectedAt.remove(id) }
                                }
                            }
                        }
                        failures = 0
                        val count = result.getInt("transferred")
                        val conflicts = result.getInt("conflicts")
                        val missing = org.json.JSONArray(access.unassignedShares()).length()
                        val title = when {
                            roundDeferred -> "Priorizando mudança do PC"
                            missing > 0 -> "Aguardando pasta Android"
                            conflicts > 0 -> "Sincronizado com conflitos"
                            !full -> "Mudanças locais verificadas"
                            else -> "Tudo sincronizado"
                        }
                        var detail = if (roundDeferred) "Auditoria pausada entre Shares; aguardando o Share alterado."
                            else if (missing > 0) "$missing Share(s) aguardam uma pasta escolhida no Android."
                            else "$count transferências · $conflicts conflitos. Última sincronização: ${java.text.DateFormat.getTimeInstance(java.text.DateFormat.SHORT).format(java.util.Date())}"
                        if (missing == 0 && conflicts > 0) detail += " As versões estão em Rowd Conflicts."
                        publish(if (roundDeferred) RowdStatus.Kind.Working else if (missing > 0 || conflicts > 0) RowdStatus.Kind.NeedsAttention else RowdStatus.Kind.Ready, title, detail)
                    } catch (error: Exception) {
                        failures++
                        if (!prefs.contains("invitation")) publish(RowdStatus.Kind.Idle, "Celular desvinculado", "Conecte ao computador para parear novamente.")
                        else if (!active.get()) publish(RowdStatus.Kind.Paused, "Sincronização pausada", "A operação foi cancelada em um ponto seguro.")
                        else publish(RowdStatus.Kind.Error,
                            "Aguardando conexão ou correção",
                            error.message ?: "Confira a pasta e o endereço do PC.")
                    }
                    notifyStatus(state.title)
                    prefs.edit().putString("lastStatus", state.title).putString("lastDetail", state.detail)
                        .putString("lastStatusKind", state.kind.name).apply()
                    if (!active.get() || !prefs.contains("invitation")) break
                    var remoteWake = false
                    if (failures == 0) {
                        while (active.get()) {
                            if (synchronized(changes) { dirty }) break
                            val remote = NativeBridge.pollWake()
                            if (remote.isNotEmpty()) {
                                wake(if (remote == "!") null else remote)
                                remoteWake = true
                                break
                            }
                            val untilAudit = 60_000L - (android.os.SystemClock.elapsedRealtime() - lastAudit)
                            if (untilAudit <= 0) break
                        }
                    } else synchronized(changes) {
                        if (!dirty) changes.wait(minOf(60_000L, 5_000L * failures))
                    }
                    synchronized(changes) { dirty = false }
                    Thread.sleep(150) // Debounce provider bursts; full audits remain the fallback.
                    if (remoteWake && active.get()) {
                        while (true) {
                            val next = NativeBridge.pollWake()
                            if (next.isEmpty()) break
                            wake(if (next == "!") null else next)
                            if (next == "!") break
                        }
                    }
                } while (active.get() && prefs.contains("invitation"))
            } catch (_: InterruptedException) {
                if (!prefs.contains("invitation")) publish(RowdStatus.Kind.Idle, "Celular desvinculado", "Conecte ao computador para parear novamente.")
                else publish(RowdStatus.Kind.Paused, "Sincronização pausada", state.detail)
            } catch (error: Exception) {
                if (!prefs.contains("invitation")) publish(RowdStatus.Kind.Idle, "Celular desvinculado", "Conecte ao computador para parear novamente.")
                else publish(RowdStatus.Kind.Error, "Não foi possível iniciar a sincronização",
                    error.message ?: "Confira o armazenamento do aplicativo.")
            } finally {
                observers.values.forEach(contentResolver::unregisterContentObserver)
                active.set(false); busy.set(false)
                changed()
                stopForeground(STOP_FOREGROUND_REMOVE); stopSelf()
            }
        }, "rowd-sync").apply { start() }
        return START_NOT_STICKY
    }
    override fun onTimeout(startId: Int, fgsType: Int) {
        active.set(false)
        NativeBridge.cancel()
        publish(RowdStatus.Kind.Paused, "Pausado pelo Android",
            "O limite de execução em segundo plano foi atingido. Abra o Rowd para retomar.")
        worker?.interrupt(); stopForeground(STOP_FOREGROUND_REMOVE); stopSelf()
    }
    override fun onDestroy() {
        networkCallback?.let { getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(it) }
        active.set(false); NativeBridge.cancel(); worker?.interrupt(); super.onDestroy()
    }
    override fun onBind(intent: Intent?): IBinder? = null
}
