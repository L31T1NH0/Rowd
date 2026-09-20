package app.rowd

import android.Manifest
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.View
import android.widget.EditText
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import app.rowd.databinding.ActivityMainBinding
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import org.json.JSONObject
import java.security.MessageDigest
import java.util.UUID

class MainActivity : AppCompatActivity() {
    private lateinit var ui: ActivityMainBinding
    private val prefs by lazy { getSharedPreferences("rowd", MODE_PRIVATE) }
    private val handler = Handler(Looper.getMainLooper())
    private val tick = object : Runnable { override fun run() { refresh(); handler.postDelayed(this, 1000) } }
    private val notifications = registerForActivityResult(ActivityResultContracts.RequestPermission()) { }
    private val folderPicker = registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
        if (uri != null) safely {
            check(!SyncService.busy.get() && !prefs.contains("invitation")) { "A pasta já está vinculada ao PC." }
            contentResolver.takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
            val root = MessageDigest.getInstance("SHA-256").digest(UUID.randomUUID().toString().toByteArray()).joinToString("") { "%02x".format(it.toInt() and 255) }
            prefs.edit().putString("tree", uri.toString()).putString("rootId", root).apply()
            refresh()
        }
    }
    private val invitePicker = registerForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) safely {
            check(!SyncService.busy.get()) { "Aguarde a sincronização terminar." }
            val text = contentResolver.openInputStream(uri)?.use { input ->
                val bytes = ByteArray(32769)
                var count = 0
                while (count < bytes.size) { val n = input.read(bytes, count, bytes.size - count); if (n < 0) break; count += n }
                check(count <= 32768) { "Convite muito grande." }
                String(bytes, 0, count, Charsets.UTF_8)
            } ?: error("Não foi possível abrir o convite.")
            val invite = JSONObject(text)
            check(invite.getInt("version") == 1) { "Versão de convite incompatível." }
            for (key in listOf("pair_id", "folder_id", "secret")) check(invite.getString(key).matches(Regex("[0-9a-f]{64}"))) { "Convite inválido." }
            val cert = invite.getString("cert_der")
            check(cert.length <= 16384 && cert.length % 2 == 0 && cert.matches(Regex("[0-9a-f]+"))) { "Certificado inválido." }
            val certificate = cert.chunked(2).map { it.toInt(16).toByte() }.toByteArray()
            val fingerprint = MessageDigest.getInstance("SHA-256").digest(certificate).joinToString(":") { "%02x".format(it.toInt() and 255) }
            MaterialAlertDialogBuilder(this).setTitle("Conectar a este PC?")
                .setMessage("${invite.getString("address")}\n\nCompare este SHA-256 com o exibido pelo PC:\n$fingerprint\n\nImporte apenas convites gerados por você.")
                .setNegativeButton("Cancelar", null).setPositiveButton("Conectar") { _, _ ->
                    prefs.edit().putString("invitation", invite.toString()).apply()
                    SyncService.status = "Pronto para sincronizar"; SyncService.detail = "Toque em Sincronizar agora."; refresh()
                }.show()
        }
    }
    private val recoveryPicker = registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
        if (uri != null && !SyncService.busy.get()) {
            val tree = prefs.getString("tree", null) ?: return@registerForActivityResult
            Thread {
                try {
                    FolderAccess(this, Uri.parse(tree)).exportRecovery(uri)
                    runOnUiThread { message("Cópias exportadas", "Cada registro JSON identifica o caminho original e os arquivos old/new correspondentes.") }
                } catch (e: Exception) { runOnUiThread { message("Falha ao exportar", e.message ?: "Confira o destino.") } }
            }.start()
        }
    }
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        ui = ActivityMainBinding.inflate(layoutInflater); setContentView(ui.root)
        ViewCompat.setOnApplyWindowInsetsListener(ui.page) { view, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.ime())
            view.setPadding(bars.left, bars.top, bars.right, bars.bottom); insets
        }
        if (!SyncService.busy.get()) {
            SyncService.status = prefs.getString("lastStatus", SyncService.status)!!
            SyncService.detail = prefs.getString("lastDetail", SyncService.detail)!!
        }
        ui.chooseFolder.setOnClickListener { folderPicker.launch(null) }
        ui.importInvite.setOnClickListener { invitePicker.launch(arrayOf("application/json", "text/plain", "application/octet-stream")) }
        ui.syncNow.setOnClickListener { startSync(false) }
        ui.automatic.setOnClickListener {
            if (SyncService.busy.get()) startService(Intent(this, SyncService::class.java).setAction(SyncService.STOP)) else startSync(true)
        }
        ui.exportRecovery.setOnClickListener { recoveryPicker.launch(null) }
        ui.changeAddress.setOnClickListener {
            val invitation = JSONObject(prefs.getString("invitation", "{}")!!)
            val input = EditText(this).apply { setSingleLine(); setText(invitation.optString("address")); hint = "192.168.1.20:43821" }
            MaterialAlertDialogBuilder(this).setTitle("Endereço do PC").setView(input)
                .setNegativeButton("Cancelar", null).setPositiveButton("Salvar") { _, _ -> safely {
                    val address = input.text.toString().trim(); check(address.isNotEmpty()) { "Informe o endereço e a porta." }
                    invitation.put("address", address); prefs.edit().putString("invitation", invitation.toString()).apply(); refresh()
                } }.show()
        }
    }
    private fun startSync(automatic: Boolean) = safely {
        if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != android.content.pm.PackageManager.PERMISSION_GRANTED) notifications.launch(Manifest.permission.POST_NOTIFICATIONS)
        startForegroundService(Intent(this, SyncService::class.java).putExtra("automatic", automatic))
    }
    private fun refresh() {
        val busy = SyncService.busy.get()
        val paired = prefs.contains("invitation")
        val folder = prefs.getString("tree", null)
        ui.status.text = SyncService.status; ui.detail.text = SyncService.detail
        ui.progress.visibility = if (busy && SyncService.status == "Sincronizando") View.VISIBLE else View.GONE
        ui.folderLabel.text = folder?.let { Uri.decode(Uri.parse(it).lastPathSegment ?: it) } ?: "Nenhuma pasta escolhida"
        ui.peerLabel.text = if (paired) JSONObject(prefs.getString("invitation", "{}")!!).optString("address") else "Ainda não pareado"
        ui.chooseFolder.isEnabled = !busy && !paired
        ui.chooseFolder.text = if (paired) "Pasta vinculada" else "Escolher pasta"
        ui.importInvite.isEnabled = !busy && folder != null
        ui.changeAddress.isEnabled = !busy && paired
        ui.syncNow.isEnabled = !busy && paired && folder != null
        ui.automatic.isEnabled = paired && folder != null
        ui.automatic.text = if (busy) "Parar após a rodada atual" else "Iniciar sincronização automática"
        ui.exportRecovery.isEnabled = !busy && folder != null
    }
    private fun message(title: String, text: String) { MaterialAlertDialogBuilder(this).setTitle(title).setMessage(text).setPositiveButton("Entendi", null).show() }
    private fun safely(action: () -> Unit) { try { action() } catch (e: Exception) { message("Não foi possível continuar", e.message ?: "Tente novamente.") } }
    override fun onResume() { super.onResume(); handler.post(tick) }
    override fun onPause() { handler.removeCallbacks(tick); super.onPause() }
}
