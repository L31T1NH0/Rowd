package app.rowd

import android.app.*
import android.content.Intent
import android.os.IBinder
import org.json.JSONObject
import java.util.concurrent.atomic.AtomicBoolean

class SyncService : Service() {
    companion object {
        const val STOP = "app.rowd.STOP"
        val busy = AtomicBoolean(false)
        @Volatile var automatic = false
        @Volatile var status = "Vamos conectar seus Shares"
        @Volatile var detail = "Pareie o PC e escolha uma pasta Android para cada Share."
        private val changes = Object()
        private var dirty = false

        fun wake() = synchronized(changes) {
            dirty = true
            changes.notifyAll()
        }
    }
    private val active = AtomicBoolean(false)
    private var worker: Thread? = null
    private val observer = object : android.database.ContentObserver(android.os.Handler(android.os.Looper.getMainLooper())) {
        override fun onChange(selfChange: Boolean) { wake() }
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
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel("sync", "Sincronização", NotificationManager.IMPORTANCE_LOW)
        )
    }
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == STOP) {
            active.set(false); automatic = false; status = "Finalizando a rodada atual"
            worker?.interrupt()
            return START_NOT_STICKY
        }
        if (!busy.compareAndSet(false, true)) {
            if (!active.get()) stopSelf()
            return START_NOT_STICKY
        }
        startForeground(1, notification("Conectando ao PC"))
        active.set(true)
        automatic = intent?.getBooleanExtra("automatic", false) == true
        worker = Thread({
            val prefs = getSharedPreferences("rowd", MODE_PRIVATE)
            var failures = 0
            val access = FolderAccess(this)
            access.treeUris().forEach { tree ->
                try { contentResolver.registerContentObserver(tree,true,observer) } catch (_: Exception) { /* SAF polling is a supported capability. */ }
            }
            try {
                do {
                    status = "Sincronizando"
                    detail = "Comparando arquivos e verificando SHA-256."
                    notifyStatus(status)
                    try {
                        val invitation = prefs.getString("invitation", null) ?: error("Importe o convite do PC.")
                        val device = prefs.getString("deviceId", null) ?: error("Abra o Rowd novamente para criar a identidade do aparelho.")
                        val result = JSONObject(NativeBridge.sync(invitation, device, access))
                        if (result.has("error")) error(result.getString("error"))
                        failures = 0
                        val count = result.getInt("transferred")
                        val conflicts = result.getInt("conflicts")
                        val missing = org.json.JSONArray(access.unassignedShares()).length()
                        status = when {
                            missing > 0 -> "Aguardando pasta Android"
                            conflicts > 0 -> "Sincronizado com conflitos"
                            else -> "Tudo sincronizado"
                        }
                        detail = if (missing > 0) "$missing Share(s) aguardam uma pasta escolhida no Android."
                            else "$count transferências · $conflicts conflitos. Última sincronização: ${java.text.DateFormat.getTimeInstance(java.text.DateFormat.SHORT).format(java.util.Date())}"
                        if (missing == 0 && conflicts > 0) detail += " As versões estão em Rowd Conflicts."
                    } catch (error: Exception) {
                        failures++
                        status = if (automatic) "Aguardando conexão ou correção" else "Não foi possível sincronizar"
                        detail = error.message ?: "Confira a pasta e o endereço do PC."
                    }
                    notifyStatus(status)
                    prefs.edit().putString("lastStatus", status).putString("lastDetail", detail).apply()
                    if (!automatic || !active.get()) break
                    synchronized(changes) {
                        if (!dirty) changes.wait(if (failures == 0) 5_000L else minOf(60_000L, 5_000L * failures))
                        dirty = false
                    }
                    Thread.sleep(350) // Debounce provider bursts; hashes still confirm every SAF scan.
                } while (active.get())
            } catch (_: InterruptedException) {
                status = "Sincronização automática pausada"
            } finally {
                contentResolver.unregisterContentObserver(observer)
                active.set(false); automatic = false; busy.set(false)
                stopForeground(STOP_FOREGROUND_REMOVE); stopSelf()
            }
        }, "rowd-sync").apply { start() }
        return START_NOT_STICKY
    }
    override fun onTimeout(startId: Int, fgsType: Int) {
        active.set(false); automatic = false
        status = "Pausado pelo Android"
        detail = "O limite de execução em segundo plano foi atingido. Abra o Rowd para retomar."
        worker?.interrupt(); stopForeground(STOP_FOREGROUND_REMOVE); stopSelf()
    }
    override fun onDestroy() { active.set(false); worker?.interrupt(); super.onDestroy() }
    override fun onBind(intent: Intent?): IBinder? = null
}
