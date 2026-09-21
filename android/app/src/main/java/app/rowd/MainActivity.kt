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
import android.widget.LinearLayout
import android.widget.RadioButton
import android.widget.RadioGroup
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import app.rowd.databinding.ActivityMainBinding
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import org.json.JSONObject
import org.json.JSONArray
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import java.security.MessageDigest
import java.util.UUID

class MainActivity : AppCompatActivity() {
    private lateinit var ui: ActivityMainBinding
    private val prefs by lazy { getSharedPreferences("rowd", MODE_PRIVATE) }
    private val handler = Handler(Looper.getMainLooper())
    private val tick = object : Runnable { override fun run() { refresh(); handler.postDelayed(this, 1000) } }
    private val notifications = registerForActivityResult(ActivityResultContracts.RequestPermission()) { }
    private val shareFolderPicker = registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
        if (uri != null) safely {
            check(!SyncService.busy.get()) { "Aguarde a operação terminar." }
            requestShare(uri)
        }
    }
    private val bindFolderPicker = registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
        val id = prefs.getString("bindingShare", null)
        prefs.edit().remove("bindingShare").apply()
        if (uri != null && id != null) safely {
            check(!SyncService.busy.get()) { "Aguarde a operação terminar." }
            FolderAccess(this).bindShare(id, uri.toString())
            SyncService.status = "Pasta Android vinculada"
            SyncService.detail = "Sincronize para concluir a configuração do Share."
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
            acceptInvitation(text)
        }
    }
    private val qrScanner = registerForActivityResult(ScanContract()) { result ->
        result.contents?.let { safely { acceptInvitation(it) } }
    }
    private fun acceptInvitation(text: String) {
        check(text.length <= 32768) { "Convite muito grande." }
        check(!SyncService.busy.get()) { "Aguarde a sincronização terminar." }
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
    private val recoveryPicker = registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
        if (uri != null && !SyncService.busy.get()) {
            runRecovery("Exportando cópias") { FolderAccess(this).exportRecovery(uri) }

        }
    }
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (!prefs.contains("deviceId")) {
            val id = prefs.getString("rootId", null) ?: MessageDigest.getInstance("SHA-256")
                .digest(UUID.randomUUID().toString().toByteArray())
                .joinToString("") { "%02x".format(it.toInt() and 255) }
            prefs.edit().putString("deviceId", id).remove("rootId").apply()
        }
        ui = ActivityMainBinding.inflate(layoutInflater); setContentView(ui.root)
        ViewCompat.setOnApplyWindowInsetsListener(ui.page) { view, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.ime())
            view.setPadding(bars.left, bars.top, bars.right, bars.bottom); insets
        }
        if (!SyncService.busy.get()) {
            SyncService.status = prefs.getString("lastStatus", SyncService.status)!!
            SyncService.detail = prefs.getString("lastDetail", SyncService.detail)!!
        }
        ui.scanQr.setOnClickListener { qrScanner.launch(ScanOptions().setDesiredBarcodeFormats(ScanOptions.QR_CODE).setPrompt("Escaneie o QR no PC").setBeepEnabled(false).setOrientationLocked(false)) }
        ui.importInvite.setOnClickListener { invitePicker.launch(arrayOf("application/json", "text/plain", "application/octet-stream")) }
        ui.requestShare.setOnClickListener { shareFolderPicker.launch(null) }
        ui.bindShare.setOnClickListener { chooseUnassignedShare() }
        ui.syncNow.setOnClickListener { startSync(false) }
        ui.automatic.setOnClickListener {
            if (SyncService.busy.get()) startService(Intent(this, SyncService::class.java).setAction(SyncService.STOP)) else startSync(true)
        }
        ui.manageRecovery.setOnClickListener {
            safely {
                val access = FolderAccess(this)
                val records = access.recoveryRecords()
                if (records.isEmpty()) { message("Recuperação", "Nenhuma versão preservada."); return@safely }
                val sharesFile = java.io.File(filesDir,"shares.json")
                val shares = if (sharesFile.exists()) JSONArray(sharesFile.readText()) else JSONArray()
                fun label(record: Pair<java.io.File,JSONObject>): String {
                    val id = record.second.optString("share_id")
                    val share = (0 until shares.length()).map { shares.getJSONObject(it) }.firstOrNull { it.getString("share_id") == id }?.getString("name") ?: "Share legado/removido"
                    val time = java.text.DateFormat.getDateTimeInstance().format(java.util.Date(record.first.lastModified()))
                    return "$share / ${record.second.getString("path")}\n$time · ${if (record.second.optBoolean("finished")) "preservado" else "pendente"}\n${record.first.nameWithoutExtension}"
                }
                MaterialAlertDialogBuilder(this).setTitle("Versões recuperáveis")
                    .setItems(records.map { label(it) }.toTypedArray()) { _, index ->
                        val record = records[index]
                        MaterialAlertDialogBuilder(this).setTitle("Escolha a ação")
                            .setItems(arrayOf("Manter atual", "Restaurar anterior", "Exportar cópias")) { _, action ->
                                if (action == 2) recoveryPicker.launch(null)
                                else MaterialAlertDialogBuilder(this).setTitle(if (action == 1) "Restaurar esta versão?" else "Manter o arquivo atual?")
                                    .setMessage(label(record) + "\n\nAs cópias de recuperação serão preservadas.")
                                    .setNegativeButton("Cancelar",null).setPositiveButton("Confirmar") { _, _ ->
                                        runRecovery("Aplicando recuperação") { access.resolveRecovery(record.first.nameWithoutExtension,action == 1) }
                                    }.show()
                            }.show()
                    }.show()
            }
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
    private fun runRecovery(title: String, action: () -> Unit) {
        if (!SyncService.busy.compareAndSet(false,true)) { message("Operação em andamento","Aguarde a sincronização ou recuperação terminar."); return }
        SyncService.status = title; refresh()
        Thread {
            try { action(); runOnUiThread { message("Recuperação concluída","As cópias foram preservadas.") } }
            catch (e: Exception) { runOnUiThread { message("Falha na recuperação",e.message ?: "Tente exportar as cópias.") } }
            finally { SyncService.busy.set(false); SyncService.status = "Recuperação finalizada"; runOnUiThread { refresh() } }
        }.start()
    }
    private fun startSync(automatic: Boolean) = safely {
        if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != android.content.pm.PackageManager.PERMISSION_GRANTED) notifications.launch(Manifest.permission.POST_NOTIFICATIONS)
        startForegroundService(Intent(this, SyncService::class.java).putExtra("automatic", automatic))
    }
    private fun chooseUnassignedShare() = safely {
        val missing = JSONArray(FolderAccess(this).unassignedShares())
        check(missing.length() > 0) { "Nenhum Share aguarda uma pasta Android." }
        val labels = (0 until missing.length()).map { missing.getJSONObject(it).getString("name") }.toTypedArray()
        MaterialAlertDialogBuilder(this).setTitle("Escolha o Share")
            .setItems(labels) { _, index ->
                prefs.edit().putString("bindingShare", missing.getJSONObject(index).getString("share_id")).apply()
                bindFolderPicker.launch(null)
            }.show()
    }
    private fun requestShare(folder: Uri) = safely {
        val name = EditText(this).apply {
            hint = "Nome do Share"
            setSingleLine()
        }
        val modes = listOf(
            "bidirectional" to "Bidirecional",
            "to_android" to "PC para Android",
            "to_pc" to "Android para PC"
        )
        val group = RadioGroup(this).apply {
            orientation = RadioGroup.VERTICAL
            modes.forEachIndexed { index, (_, label) ->
                addView(RadioButton(this@MainActivity).apply {
                    id = View.generateViewId()
                    text = label
                    isChecked = index == 0
                })
            }
        }
        val form = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(48, 0, 48, 0)
            addView(name)
            addView(group)
        }
        MaterialAlertDialogBuilder(this)
            .setTitle("Solicitar Share")
            .setMessage("Esta pasta será usada no Android. O PC escolherá a pasta local e confirmará a solicitação.")
            .setView(form)
            .setNegativeButton("Cancelar", null)
            .setPositiveButton("Enviar") { _, _ -> safely {
                val selected = group.checkedRadioButtonId
                val index = group.indexOfChild(group.findViewById(selected)).coerceIn(0, modes.lastIndex)
                FolderAccess(this).queueShareRequest(name.text.toString(), modes[index].first, folder.toString())
                SyncService.status = "Solicitação de Share salva"
                SyncService.detail = if (prefs.contains("invitation")) "Sincronize para enviá-la ao PC." else "Pareie o PC e sincronize para enviá-la."
                refresh()
            } }
            .show()
    }
    private fun refresh() {
        val busy = SyncService.busy.get()
        val paired = prefs.contains("invitation")
        val access = FolderAccess(this)
        ui.status.text = SyncService.status; ui.detail.text = SyncService.detail
        ui.progress.visibility = if (busy) View.VISIBLE else View.GONE
        ui.peerLabel.text = if (paired) JSONObject(prefs.getString("invitation", "{}")!!).optString("address") else "Ainda não pareado"
        ui.importInvite.isEnabled = !busy
        ui.scanQr.isEnabled = !busy
        ui.manageRecovery.isEnabled = !busy
        ui.requestShare.isEnabled = !busy
        val definitions = java.io.File(filesDir,"shares.json")
        val unassigned = runCatching { JSONArray(access.unassignedShares()) }.getOrDefault(JSONArray())
        val missingIds = (0 until unassigned.length()).map { unassigned.getJSONObject(it).getString("share_id") }.toSet()
        val sharesText = if (definitions.exists()) {
            val shares = JSONArray(definitions.readText())
            (0 until shares.length()).joinToString("\n") { i ->
                val share = shares.getJSONObject(i)
                val journal = java.io.File(filesDir,"shares/${share.getString("share_id")}/journal.json")
                val pending = if (journal.exists()) JSONObject(journal.readText()).optJSONObject("pending")?.length() ?: 0 else 0
                val folderState = if (share.getString("share_id") in missingIds) " · escolha a pasta Android" else ""
                "${share.getString("name")} · $pending pendências · ${share.getString("mode")}$folderState"
            }.ifEmpty { "Nenhum Share aceito ainda." }
        } else "Nenhum Share aceito ainda."
        val requestCount = runCatching { JSONArray(access.pendingShareRequests()).length() }.getOrDefault(0)
        ui.sharesLabel.text = if (requestCount == 0) sharesText else "$sharesText\nSolicitações pendentes: $requestCount"
        ui.bindShare.isEnabled = !busy && unassigned.length() > 0
        ui.changeAddress.isEnabled = !busy && paired
        ui.syncNow.isEnabled = !busy && paired
        ui.automatic.isEnabled = paired && (!busy || SyncService.automatic)
        ui.automatic.text = if (busy) "Parar após a rodada atual" else "Iniciar sincronização automática"
        ui.exportRecovery.isEnabled = !busy
    }
    private fun message(title: String, text: String) { MaterialAlertDialogBuilder(this).setTitle(title).setMessage(text).setPositiveButton("Entendi", null).show() }
    private fun safely(action: () -> Unit) { try { action() } catch (e: Exception) { message("Não foi possível continuar", e.message ?: "Tente novamente.") } }
    override fun onResume() { super.onResume(); handler.post(tick) }
    override fun onPause() { handler.removeCallbacks(tick); super.onPause() }
}
