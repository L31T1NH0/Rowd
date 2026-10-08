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
        private fun traceEvent(name: String, share: String?, component: PerformanceTrace.Component,
            level: String = "trace", detail: JSONObject? = null, function: String? = null, sourceLine: Int? = null) {
            PerformanceTrace.event(name, share, component = component, level = level, detail = detail,
                function = function, sourceFile = "SyncService.kt", sourceLine = sourceLine)
        }
        const val STOP = "app.rowd.STOP"
        private const val RESUME = "app.rowd.RESUME"
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
        private val wakes = WakeState()
        fun changeGeneration(shareId: String): Long = synchronized(changes) { wakes.generationFor(shareId) }

        internal fun wake(shareId: String? = null, source: WakeSource = WakeSource.MANUAL, detectedAt: Long = android.os.SystemClock.elapsedRealtime()) = synchronized(changes) {
            wakes.wake(shareId, source, detectedAt)
            traceEvent("WAKE_REQUESTED", shareId, component = PerformanceTrace.Component.Scheduler,
                detail = JSONObject().put("source", source.name).put("generation", wakes.generation)
                    .put("detected_at_ms", if (source == WakeSource.NETWORK_RECONNECT) JSONObject.NULL else wakes.detectedAt[shareId ?: "*"]), sourceLine = 69)
            changes.notifyAll()
            NativeBridge.signalIdle(if (source == WakeSource.NETWORK_RECONNECT) 2 else 1)
        }
        private fun requestReconnect() = wake(source = WakeSource.NETWORK_RECONNECT)
        private fun pollWake(auditInMs: Long): Boolean {
            val result = JSONObject(NativeBridge.pollWake(auditInMs))
            val kind = result.getString("kind")
            synchronized(changes) {
                wakes.pollResult(kind, result.optString("share_id").takeIf { it.isNotEmpty() }, android.os.SystemClock.elapsedRealtime())
                if (kind != "none") changes.notifyAll()
            }
            if (kind == "share") synchronized(changes) {
                val id = result.getString("share_id")
                traceEvent("WAKE_REQUESTED", id, component = PerformanceTrace.Component.Scheduler,
                    detail = JSONObject().put("source", WakeSource.REMOTE_WAKE.name)
                        .put("generation", wakes.dirtyShares[id]).put("detected_at_ms", wakes.detectedAt[id]), sourceLine = 85)
            }
            // A local signal may belong to a change already consumed by the previous round.
            return if (kind == "local" || kind == "none") synchronized(changes) { wakes.dirty || wakes.reconnectRequested }
                else true
        }

    }
    private val active = AtomicBoolean(false)
    private val lifecycle = SyncLifecycle()
    private var worker: Thread? = null
    private var observerBurstFlush: (() -> Unit)? = null
    private val traceLifecycle = Any()
    private var traceWorkerFinished = false
    private var traceServiceDestroyed = false
    private var networkCallback: ConnectivityManager.NetworkCallback? = null
    private var networkPath: NetworkPath? = null
    @Volatile private var observerCount = 0
    @Volatile private var traceAccess: FolderAccess? = null
    private val traceHandler = Handler(Looper.getMainLooper())
    private val retryWorker = object : Runnable {
        override fun run() {
            val prefs = getSharedPreferences("rowd", MODE_PRIVATE)
            val delay = lifecycle.retryDelay(prefs.getBoolean("syncEnabled", false), prefs.contains("invitation"))
                ?: return
            if (busy.get()) {
                traceHandler.postDelayed(this, delay)
                return
            }
            runCatching { startService(Intent(this@SyncService, SyncService::class.java).setAction(RESUME)) }
                .onFailure { error ->
                    android.util.Log.e("Rowd", "Não foi possível retomar o serviço", error)
                    lifecycle.stop()
                    stopForeground(STOP_FOREGROUND_REMOVE); stopSelf()
                }
        }
    }
    private val traceSnapshot = object : Runnable {
        override fun run() {
            if (PerformanceTrace.enabled()) {
                val snapshot = JSONObject(NativeBridge.traceRuntimeState())
                    .put("service_active", active.get()).put("worker_alive", worker?.isAlive == true)
                    .put("observer_count", observerCount).put("dirty_share_count", synchronized(changes) { wakes.dirtyShares.size })
                    .put("network_path", networkPath.toString())
                    .put("reconnect_requested", synchronized(changes) { wakes.reconnectRequested })
                traceAccess?.traceSnapshot()?.let { local -> local.keys().forEach { snapshot.put(it, local.get(it)) } }
                traceEvent("RUNTIME_STATE_SNAPSHOT", null, component = PerformanceTrace.Component.Service, level = "debug", detail = snapshot, sourceLine = 133)
            }
            traceHandler.postDelayed(this, 30_000L)
        }
    }

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
            private fun changed(callbackType: String) {
                val current = path()
                val previousPath = networkPath
                val changed = synchronized(this@SyncService) {
                    val previous = networkPath ?: current
                    networkPath = current.retainingKnown(previous)
                    current.changedFrom(previous)
                }
                traceEvent("NETWORK_CALLBACK", null, component = PerformanceTrace.Component.Network, detail = JSONObject()
                    .put("callback_type", callbackType).put("previous_network_handle", previousPath?.network ?: JSONObject.NULL)
                    .put("current_network_handle", current.network ?: JSONObject.NULL).put("previous_interface", previousPath?.interfaceName ?: JSONObject.NULL)
                    .put("current_interface", current.interfaceName ?: JSONObject.NULL).put("previous_ipv4", previousPath?.ipv4 ?: JSONObject.NULL)
                    .put("current_ipv4", current.ipv4 ?: JSONObject.NULL).put("changed", changed), sourceLine = 159)
                if (!changed) return
                traceEvent("NETWORK_PATH_CHANGED", null, component = PerformanceTrace.Component.Network, detail = JSONObject().put("operation", "NativeBridge.networkChanged"), sourceLine = 165)
                NativeBridge.networkChanged()
                requestReconnect()
            }
            override fun onAvailable(network: Network) = changed("onAvailable")
            override fun onLost(network: Network) = changed("onLost")
            override fun onLinkPropertiesChanged(network: Network, linkProperties: LinkProperties) = changed("onLinkPropertiesChanged")
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
        PerformanceTrace.serviceStartedWallMs = System.currentTimeMillis()
        observeNetwork()
        if (getSharedPreferences("rowd", MODE_PRIVATE).getBoolean("performanceTrace", false) && !PerformanceTrace.enabled()) {
            runCatching { PerformanceTrace.enable(this) }
                .onFailure { android.util.Log.e("RowdTrace", "Não foi possível iniciar o trace", it) }
        }
        traceHandler.postDelayed(traceSnapshot, 30_000L)
        traceEvent("ANDROID_SERVICE_CREATE", null, component = PerformanceTrace.Component.Service, level = "info", sourceLine = 197)
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel("sync", "Sincronização", NotificationManager.IMPORTANCE_LOW)
        )
    }
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        traceEvent("ANDROID_SERVICE_START", null, component = PerformanceTrace.Component.Service, level = "info", detail = JSONObject().put("start_id", startId).put("flags", flags).put("stop_requested", intent?.action == STOP).put("system_restart", intent == null), sourceLine = 203)
        val prefs = getSharedPreferences("rowd", MODE_PRIVATE)
        if (intent?.action == STOP) {
            prefs.edit().putBoolean("syncEnabled", false).commit()
            lifecycle.stop()
            traceHandler.removeCallbacks(retryWorker)
            active.set(false)
            publish(RowdStatus.Kind.Paused, "Finalizando a rodada atual", state.detail)
            NativeBridge.cancel()
            worker?.interrupt()
            if (worker?.isAlive != true) {
                stopForeground(STOP_FOREGROUND_REMOVE); stopSelf()
            }
            return START_NOT_STICKY
        }
        if (!lifecycle.start(intent == null || intent.action == RESUME,
                prefs.getBoolean("syncEnabled", false) && !prefs.getBoolean("syncBlockedByAndroid", false),
                prefs.contains("invitation"))) {
            lifecycle.stop()
            stopSelf()
            return START_NOT_STICKY
        }
        prefs.edit().putBoolean("syncEnabled", true).remove("syncBlockedByAndroid").commit()
        traceHandler.removeCallbacks(retryWorker)
        if (!active.get()) startForeground(1, notification("Conectando ao PC"))
        if (!busy.compareAndSet(false, true)) {
            if (!active.get()) scheduleWorkerRetry()
            else if (intent != null && intent.action != RESUME) wake()
            return START_STICKY
        }
        NativeBridge.resetCancellation()
        active.set(true)
        // Changes made while the service was stopped have no observer wake.
        wake(source = WakeSource.STARTUP)
        synchronized(traceLifecycle) { traceWorkerFinished = false }
        worker = Thread({
            traceEvent("WORKER_START", null, component = PerformanceTrace.Component.Service, sourceLine = 239)
            val prefs = getSharedPreferences("rowd", MODE_PRIVATE)
            var failures = 0
            var reconnectFailures = 0
            val observers = mutableMapOf<Uri, android.database.ContentObserver>()
            var observedTrees = emptyMap<Uri, String?>()
            var directoryObservers: SafDirectoryObservers? = null
            var lastAudit = 0L
            val lastDeepAudits = mutableMapOf<String, Long>()
            var auditDeferred = false
            val audits = AuditRotation()
            var completedAllGeneration = 0L
            try {
                val access = FolderAccess(this)
                traceAccess = access
                val bursts = ObserverBursts()
                val burstHandler = Handler(Looper.getMainLooper())
                val flushBursts = Runnable {
                    bursts.flush().forEach { burst ->
                        traceEvent("OBSERVER_BURST_COALESCED", null, component = PerformanceTrace.Component.Watcher,
                            detail = JSONObject().put("callbacks", burst.callbacks).put("shares", JSONArray(burst.shares.keys.toList()))
                                .put("window_ms", android.os.SystemClock.elapsedRealtime() - burst.startedAt)
                                .put("provider", burst.provider), sourceLine = 258)
                        synchronized(changes) {
                            burst.shares.forEach { (id, at) -> wake(id, WakeSource.LOCAL_OBSERVER, at) }
                        }
                    }
                }
                fun observerChanged(shareId: String?, uri: Uri?, tree: Uri, selfChange: Boolean) {
                    val hint = access.noteChange(shareId, uri, selfChange) ?: return
                    if (hint == ObserverHint.PROVIDER_WIDE_URI || hint == ObserverHint.NULL_URI || hint == ObserverHint.KNOWN_DIRECTORY_URI) {
                        if (bursts.add(tree.authority.orEmpty(), requireNotNull(shareId), android.os.SystemClock.elapsedRealtime())) {
                            burstHandler.postDelayed(flushBursts, ObserverBursts.WINDOW_MS)
                        }
                    } else wake(shareId, WakeSource.LOCAL_OBSERVER)
                }
                directoryObservers = SafDirectoryObservers(contentResolver,
                    changed = { tree, shareId, directory -> burstHandler.post {
                        if (active.get()) {
                            traceEvent("OBSERVER_CALLBACK", shareId, component = PerformanceTrace.Component.Watcher,
                                function = "directoryCursorChanged", detail = JSONObject()
                                    .put("callback_type", "directory_cursor").put("tree", tree.toString()), sourceLine = 278)
                            observerChanged(shareId, directory, tree, false)
                        }
                    } },
                    failed = { tree, shareId, error ->
                        traceEvent("SAF_DIRECTORY_OBSERVER_FAILED", shareId, component = PerformanceTrace.Component.Watcher,
                            level = "warn", detail = JSONObject().put("tree", tree.toString()).put("fallback", "full_audit")
                                .put("error", PerformanceTrace.error(error, "watcher", "observe_directory", false)), sourceLine = 285)
                    })
                observerBurstFlush = { burstHandler.removeCallbacks(flushBursts); flushBursts.run() }
                fun refreshObservers() {
                    traceEvent("OBSERVER_REFRESH_START", null, component = PerformanceTrace.Component.Watcher, sourceLine = 291)
                    val current = access.observedShareTrees()
                    directoryObservers?.update(current)
                    if (current == observedTrees) return
                    observers.forEach { (tree, observer) ->
                        contentResolver.unregisterContentObserver(observer)
                        traceEvent("OBSERVER_UNREGISTERED", observedTrees[tree], component = PerformanceTrace.Component.Watcher, detail = JSONObject().put("tree", tree.toString()), sourceLine = 297)
                    }
                    PerformanceTrace.observerUnregistered()
                    observers.clear()
                    observerCount = 0
                    current.forEach { (tree, shareId) ->
                        val observer = object : android.database.ContentObserver(Handler(Looper.getMainLooper())) {
                            override fun onChange(selfChange: Boolean) {
                                traceEvent("OBSERVER_CALLBACK", shareId, component = PerformanceTrace.Component.Watcher, function = "onChange", detail = JSONObject().put("callback_type", "selfChange").put("self_change", selfChange).put("tree", tree.toString()), sourceLine = 305)
                                observerChanged(shareId, null, tree, selfChange)
                            }
                            override fun onChange(selfChange: Boolean, uri: Uri?) {
                                traceEvent("OBSERVER_CALLBACK", shareId, component = PerformanceTrace.Component.Watcher, function = "onChange", detail = JSONObject().put("callback_type", "uri").put("self_change", selfChange).put("tree", tree.toString()).put("uri", uri?.toString()), sourceLine = 309)
                                observerChanged(shareId, uri, tree, selfChange)
                            }
                        }
                        try {
                            traceEvent("OBSERVER_REGISTER_START", shareId, component = PerformanceTrace.Component.Watcher, detail = JSONObject().put("tree", tree.toString()), sourceLine = 314)
                            contentResolver.registerContentObserver(tree, true, observer)
                            observers[tree] = observer
                            observerCount = observers.size
                            PerformanceTrace.observerRegistered(shareId)
                            traceEvent("OBSERVER_REGISTERED", shareId, component = PerformanceTrace.Component.Watcher, detail = JSONObject().put("tree", tree.toString()), sourceLine = 319)
                        } catch (error: Exception) {
                            traceEvent("OBSERVER_REGISTER_FAILED", shareId, component = PerformanceTrace.Component.Watcher, level = "warn", detail = JSONObject().put("tree", tree.toString()).put("fallback", "full_audit").put("error", PerformanceTrace.error(error, "watcher", "register_observer", false)), sourceLine = 321)
                        }
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
                            wakes.dirty = false
                            wakes.reconnectRequested = false
                            val selected = wakes.dirtyShares.toMap()
                            val auditDue = lastAudit == 0L || now - lastAudit >= 60_000L
                            // A known Share goes first; the overdue audit runs on the next pass.
                            val focusedFirst = auditDue && selected.isNotEmpty() && !auditDeferred &&
                                wakes.dirtyAllGeneration == completedAllGeneration
                            if (focusedFirst) auditDeferred = true
                            Triple(wakes.dirtyAllGeneration != completedAllGeneration || (auditDue && !focusedFirst),
                                wakes.dirtyAllGeneration, selected)
                        }
                        val periodic = full && allVersion == completedAllGeneration
                        val auditShare = if (periodic) {
                            val shares = JSONArray(access.knownShares())
                            val enabled = (0 until shares.length()).map(shares::getJSONObject)
                                .filter { it.optBoolean("enabled", true) }.map { it.getString("share_id") }.toSet()
                            val available = JSONArray(access.availableShares()).let { bound ->
                                (0 until bound.length()).map(bound::getString).toSet()
                            }
                            audits.select(available, enabled)
                        } else null
                        val deep = auditShare != null &&
                            (lastDeepAudits[auditShare]?.let { now - it >= 15 * 60_000L } ?: true)
                        if (auditShare != null) access.scheduleAudit(auditShare, deep)
                        val focus = when {
                            !full -> JSONArray(selected.keys.toList()).toString()
                            periodic -> JSONArray(listOfNotNull(auditShare)).toString()
                            else -> ""
                        }
                        access.setFocusedScan(focus.isNotEmpty())
                        access.setAuditRound(full)
                        access.setScanObservation(if (full && lastAudit == 0L) "startup_scan"
                        else if (periodic) "audit_scan" else "focused_scan", synchronized(changes) { selected.keys.associateWith { id ->
                            when (wakes.sources[id]) {
                                WakeSource.LOCAL_OBSERVER -> "content_observer"
                                WakeSource.MANUAL -> "manual_scan"
                                else -> "focused_scan"
                            }
                        } })
                        if (!prefs.contains("invitation")) break
                        publish(RowdStatus.Kind.Working,
                            if (full) "Verificando arquivos" else if (selected.isEmpty()) "Reconectando ao PC" else "Sincronizando alterações",
                            if (full) "Auditoria periódica dos Shares." else "Verificando os Shares alterados.")
                        notifyStatus(state.title)
                        val startedAt = android.os.SystemClock.elapsedRealtime()
                        val activationMs = synchronized(changes) {
                            selected.keys.mapNotNull { wakes.detectedAt[it]?.let { time -> startedAt - time } }.maxOrNull() ?: 0L
                        }
                        android.util.Log.i("RowdLatency", "round_started_at=$startedAt focus=${if (full) "audit" else focus} activation_ms=$activationMs")
                        val result = JSONObject(NativeBridge.sync(invitation, device, focus, access))
                        result.optJSONArray("pending_wakes")?.let { pending ->
                            for (index in 0 until pending.length()) wake(pending.getString(index), WakeSource.REMOTE_WAKE)
                        }
                        if (result.has("error")) throw NativeSyncFailure(result.optString("error_kind", "share_error"), result.getString("error"))
                        if (!prefs.contains("invitation")) {
                            publish(RowdStatus.Kind.Idle, "Celular desvinculado", "Pareie novamente para continuar.")
                            break
                        }
                        result.optJSONObject("metrics")?.put("activation_ms", activationMs)
                        val roundDeferred = result.optBoolean("round_deferred")
                        val completedShares = result.optJSONArray("completed_shares")?.let { shares ->
                            (0 until shares.length()).map(shares::getString).toSet()
                        }.orEmpty()
                        val completedAt = android.os.SystemClock.elapsedRealtime()
                        android.util.Log.i("RowdLatency", "round_completed_at=$completedAt duration_ms=${completedAt - startedAt} transferred=${result.optInt("transferred")}")
                        refreshObservers() // configureShares may have changed bindings in this round.
                        synchronized(changes) {
                            audits.complete(auditShare, roundDeferred)
                            if (roundDeferred) {
                                auditDeferred = false
                            } else if (full) {
                                lastAudit = android.os.SystemClock.elapsedRealtime()
                                auditDeferred = false
                                if (auditShare != null && auditShare in completedShares) {
                                    if (deep) {
                                        lastDeepAudits[auditShare] = lastAudit
                                    }
                                }
                                if (wakes.dirtyAllGeneration == allVersion) completedAllGeneration = allVersion
                            }
                            // A one-Share audit may leave other selected Shares pending.
                            wakes.complete(selected, completedShares, resumePending = periodic || roundDeferred)
                        }
                        failures = 0
                        reconnectFailures = 0
                        val count = result.getInt("transferred")
                        val conflicts = result.getInt("conflicts")
                        val missing = org.json.JSONArray(access.unassignedShares()).length()
                        val title = when {
                            roundDeferred -> "Priorizando mudança do PC"
                            missing > 0 -> "Aguardando pasta Android"
                            conflicts > 0 -> "Sincronizado com conflitos"
                            !full && selected.isEmpty() -> "Conexão restabelecida"
                            !full -> "Mudanças verificadas"
                            else -> "Tudo sincronizado"
                        }
                        var detail = if (roundDeferred) "Auditoria pausada entre Shares; aguardando o Share alterado."
                            else if (missing > 0) "$missing Share(s) aguardam uma pasta escolhida no Android."
                            else "$count transferências · $conflicts conflitos. Última sincronização: ${java.text.DateFormat.getTimeInstance(java.text.DateFormat.SHORT).format(java.util.Date())}"
                        if (missing == 0 && conflicts > 0) detail += " As versões estão em Rowd Conflicts."
                        publish(if (roundDeferred) RowdStatus.Kind.Working else if (missing > 0 || conflicts > 0) RowdStatus.Kind.NeedsAttention else RowdStatus.Kind.Ready, title, detail)
                    } catch (error: Exception) {
                        val kind = (error as? NativeSyncFailure)?.kind
                        val reconnect = kind in setOf("transport_reconnect", "network_generation_changed")
                        val cancelled = kind == "cancelled"
                        traceEvent("WORKER_OPERATION_FAILED", null, component = PerformanceTrace.Component.Service,
                            level = if (reconnect) "warn" else if (cancelled) "debug" else "error",
                            detail = JSONObject().put("error_kind", kind).put("error", PerformanceTrace.error(error, "android_service", "sync_round")), sourceLine = 443)
                        when {
                            reconnect -> { requestReconnect(); reconnectFailures++; failures = 0 }
                            cancelled -> { active.set(false); failures = 0 }
                            else -> { failures++; reconnectFailures = 0 }
                        }
                        if (!prefs.contains("invitation")) publish(RowdStatus.Kind.Idle, "Celular desvinculado", "Conecte ao computador para parear novamente.")
                        else if (!active.get()) publish(RowdStatus.Kind.Paused, "Sincronização pausada", "A operação foi cancelada em um ponto seguro.")
                        else if (reconnect) publish(RowdStatus.Kind.Working, "Reconectando ao PC", "Aguardando a conexão de rede.")
                        else publish(RowdStatus.Kind.Error,
                            "Aguardando conexão ou correção",
                            error.message ?: "Confira a pasta e o endereço do PC.")
                    }
                    notifyStatus(state.title)
                    prefs.edit().putString("lastStatus", state.title).putString("lastDetail", state.detail)
                        .putString("lastStatusKind", state.kind.name).apply()
                    if (!active.get() || !prefs.contains("invitation")) break
                    traceEvent("SYNC_IDLE_ENTER", null, component = PerformanceTrace.Component.Service,
                        detail = JSONObject().put("reconnect_requested", synchronized(changes) { wakes.reconnectRequested }), sourceLine = 462)
                    if (failures == 0 && reconnectFailures == 0) {
                        while (active.get()) {
                            if (synchronized(changes) { wakes.dirty || wakes.reconnectRequested }) break
                            val untilAudit = 60_000L - (android.os.SystemClock.elapsedRealtime() - lastAudit)
                            if (untilAudit <= 0) break
                            if (pollWake(untilAudit)) break
                        }
                    } else synchronized(changes) {
                        // Repeated connection failures still back off; reconnect never resets the audit clock.
                        val delay = if (failures > 0) minOf(60_000L, 5_000L * failures)
                            else minOf(60_000L, 5_000L * (reconnectFailures - 1))
                        if (!wakes.dirty && delay > 0) changes.wait(delay)
                    }
                    traceEvent("SYNC_IDLE_EXIT", null, component = PerformanceTrace.Component.Service, sourceLine = 477)

                } while (active.get() && prefs.contains("invitation"))
            } catch (_: InterruptedException) {
                traceEvent("WORKER_INTERRUPTED", null, component = PerformanceTrace.Component.Service, sourceLine = 481)
                if (!prefs.contains("invitation")) publish(RowdStatus.Kind.Idle, "Celular desvinculado", "Conecte ao computador para parear novamente.")
                else publish(RowdStatus.Kind.Paused, "Sincronização pausada", state.detail)
            } catch (error: Exception) {
                if (!prefs.contains("invitation")) publish(RowdStatus.Kind.Idle, "Celular desvinculado", "Conecte ao computador para parear novamente.")
                else publish(RowdStatus.Kind.Error, "Não foi possível iniciar a sincronização",
                    error.message ?: "Confira o armazenamento do aplicativo.")
            } finally {
                fun cleanup(action: () -> Unit) {
                    try { action() }
                    catch (error: Exception) { android.util.Log.w("Rowd", "Falha ao liberar os observadores", error) }
                }
                traceEvent("WORKER_STOP", null, component = PerformanceTrace.Component.Service, sourceLine = 493)
                PerformanceTrace.flush()
                cleanup { directoryObservers?.close() }
                observers.forEach { (tree, observer) ->
                    cleanup {
                        contentResolver.unregisterContentObserver(observer)
                        traceEvent("OBSERVER_UNREGISTERED", observedTrees[tree], component = PerformanceTrace.Component.Watcher, detail = JSONObject().put("tree", tree.toString()).put("reason", "worker_stop"), sourceLine = 499)
                    }
                }
                cleanup { observerBurstFlush?.invoke() }
                observerBurstFlush = null
                PerformanceTrace.observerUnregistered()
                observerCount = 0
                traceAccess = null
                synchronized(traceLifecycle) {
                    traceWorkerFinished = true
                    if (traceServiceDestroyed) finishServiceTrace()
                }
                active.set(false); busy.set(false)
                changed()
                main.post {
                    if (traceServiceDestroyed || active.get()) return@post
                    if (!scheduleWorkerRetry()) {
                        stopForeground(STOP_FOREGROUND_REMOVE); stopSelf()
                    }
                }
            }
        }, "rowd-sync").apply { start() }
        return START_STICKY
    }
    override fun onTimeout(startId: Int, fgsType: Int) {
        getSharedPreferences("rowd", MODE_PRIVATE).edit().putBoolean("syncBlockedByAndroid", true).commit()
        lifecycle.stop()
        traceHandler.removeCallbacks(retryWorker)
        active.set(false)
        NativeBridge.cancel()
        publish(RowdStatus.Kind.Paused, "Pausado pelo Android",
            "O limite de execução em segundo plano foi atingido. Abra o Rowd para retomar.")
        worker?.interrupt(); stopForeground(STOP_FOREGROUND_REMOVE); stopSelf()
    }
    private fun finishServiceTrace() { PerformanceTrace.flush() }
    private fun scheduleWorkerRetry(): Boolean {
        val prefs = getSharedPreferences("rowd", MODE_PRIVATE)
        val delay = lifecycle.retryDelay(prefs.getBoolean("syncEnabled", false), prefs.contains("invitation"))
            ?: return false
        traceHandler.removeCallbacks(retryWorker)
        traceHandler.postDelayed(retryWorker, delay)
        traceEvent("WORKER_RESTART_SCHEDULED", null, component = PerformanceTrace.Component.Service,
            level = "warn", detail = JSONObject().put("delay_ms", delay), sourceLine = 540)
        return true
    }
    override fun onDestroy() {
        lifecycle.stop()
        traceHandler.removeCallbacks(retryWorker)
        traceHandler.removeCallbacks(traceSnapshot)
        traceEvent("ANDROID_SERVICE_DESTROY", null, component = PerformanceTrace.Component.Service, level = "info", sourceLine = 548)
        traceEvent("ANDROID_SERVICE_STOP", null, component = PerformanceTrace.Component.Service, level = "info", sourceLine = 549)
        PerformanceTrace.flush()
        networkCallback?.let { getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(it) }
        active.set(false); NativeBridge.cancel(); worker?.interrupt()
        synchronized(traceLifecycle) {
            traceServiceDestroyed = true
            if (traceWorkerFinished || worker == null) finishServiceTrace()
        }
        super.onDestroy()
    }
    override fun onBind(intent: Intent?): IBinder? = null
}
