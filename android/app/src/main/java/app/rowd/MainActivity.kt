package app.rowd

import android.Manifest
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
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
    private val statusChanged: () -> Unit = { refresh() }
    private val notifications = registerForActivityResult(ActivityResultContracts.RequestPermission()) { }
    private val shareFolderPicker = registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
        if (uri != null) safely {
            requestShare(uri)
        }
    }
    private val bindFolderPicker = registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
        val id = prefs.getString("bindingShare", null)
        prefs.edit().remove("bindingShare").apply()
        if (uri != null && id != null) safely {
            FolderAccess(this).bindShare(id, uri.toString())
            SyncService.wake()
            SyncService.publish(RowdStatus.Kind.Ready, "Pasta Android vinculada", "Sincronize para concluir a configuração do Share.")
            refresh()
        }
    }
    private val invitePicker = registerForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) safely {
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
            check(!SyncService.busy.get()) { "Aguarde a operação atual terminar para trocar o vínculo." }
            val invite = previewInvitation(text)
            val fingerprint = invite.getString("fingerprint")
            MaterialAlertDialogBuilder(this).setTitle("Conectar a este PC?")
                .setMessage("${invite.getString("address")}\n\nCompare este SHA-256 com o exibido pelo PC:\n$fingerprint\n\nImporte apenas convites gerados por você.")
                .setNegativeButton("Cancelar", null).setPositiveButton("Conectar") { _, _ -> safely {
                    check(!SyncService.busy.get()) { "Aguarde a operação atual terminar para trocar o vínculo." }
                    prefs.edit().putString("invitation", invite.getString("invitation"))
                        .putString("peerAddress", invite.getString("address"))
                        .remove("unlinkRequested").apply()
                    SyncService.wake()
                    SyncService.publish(RowdStatus.Kind.Ready, "Pronto para sincronizar", "Toque em Sincronizar agora."); refresh()
                } }.show()
    }
    private fun previewInvitation(text: String, address: String = ""): JSONObject {
        val result = JSONObject(NativeBridge.previewInvitation(text, address))
        check(!result.has("error")) { result.optString("error", "Convite inválido.") }
        return result
    }
    private val recoveryPicker = registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
        if (uri != null && !SyncService.busy.get()) {
            runRecovery("Exportando cópias") { FolderAccess(this).exportRecovery(uri) }

        }
    }
    private val tracePicker = registerForActivityResult(ActivityResultContracts.CreateDocument("application/zip")) { uri ->
        if (uri != null) safely { PerformanceTrace.export(this, uri); message("Trace exportado", "Os dois arquivos de trace foram salvos no ZIP.") }
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
        if (prefs.getBoolean("performanceTrace", false) && !PerformanceTrace.enabled()) safely { PerformanceTrace.enable(this) }
        ViewCompat.setOnApplyWindowInsetsListener(ui.page) { view, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.ime())
            view.setPadding(bars.left, bars.top, bars.right, bars.bottom); insets
        }
        if (!SyncService.busy.get()) SyncService.restore(this)
        ui.scanQr.setOnClickListener { qrScanner.launch(ScanOptions().setDesiredBarcodeFormats(ScanOptions.QR_CODE).setPrompt("Escaneie o QR no PC").setBeepEnabled(false).setOrientationLocked(false)) }
        ui.importInvite.setOnClickListener { invitePicker.launch(arrayOf("application/json", "text/plain", "application/octet-stream")) }
        ui.requestShare.setOnClickListener { shareFolderPicker.launch(null) }
        ui.manageRequests.setOnClickListener { manageRequests() }
        ui.bindShare.setOnClickListener { chooseUnassignedShare() }
        ui.syncNow.setOnClickListener { startSync() }
        ui.manageRecovery.setOnClickListener {
            safely {
                val access = FolderAccess(this)
                val records = access.recoveryRecords()
                if (records.isEmpty()) { message("Recuperação", "Nenhuma versão preservada."); return@safely }
                val shares = JSONArray(access.statusShares())
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
        ui.performanceTrace.isChecked = prefs.getBoolean("performanceTrace", false)
        var updatingTrace = false
        ui.performanceTrace.setOnCheckedChangeListener { button, checked ->
            if (updatingTrace) return@setOnCheckedChangeListener
            try {
                if (checked) PerformanceTrace.enable(this) else PerformanceTrace.disable()
                prefs.edit().putBoolean("performanceTrace", checked).apply()
            } catch (error: Exception) {
                updatingTrace = true
                button.isChecked = !checked
                updatingTrace = false
                message("Trace indisponível", error.message ?: "Não foi possível alterar o trace.")
            }
        }
        ui.exportTrace.setOnClickListener { tracePicker.launch("rowd-performance-trace.zip") }
        ui.changeAddress.setOnClickListener {
            val current = prefs.getString("invitation", "")!!
            val input = EditText(this).apply { setSingleLine(); setText(previewInvitation(current).getString("address")); hint = "192.168.1.20:43821" }
            MaterialAlertDialogBuilder(this).setTitle("Endereço do PC").setView(input)
                .setNegativeButton("Cancelar", null).setPositiveButton("Salvar") { _, _ -> safely {
                    val address = input.text.toString().trim(); check(address.isNotEmpty()) { "Informe o endereço e a porta." }
                    val updated = previewInvitation(current, address)
                    prefs.edit().putString("invitation", updated.getString("invitation"))
                        .putString("peerAddress", updated.getString("address")).apply()
                    SyncService.wake(); refresh()
                } }.show()
        }
        ui.unlinkDevice.setOnClickListener {
            MaterialAlertDialogBuilder(this).setTitle("Desvincular este aparelho?")
                .setMessage("O PC revogará a credencial atual. Arquivos e versões de recovery serão preservados.")
                .setNegativeButton("Cancelar", null).setPositiveButton("Desvincular") { _, _ -> safely {
                    FolderAccess(this).requestUnlink()
                    SyncService.wake()
                    SyncService.publish(RowdStatus.Kind.Working, "Desvinculação pendente", "Conectando ao PC para revogar a credencial.")
                    if (!SyncService.busy.get()) startSync()
                    refresh()
                } }.show()
        }
        ui.resetApp.setOnClickListener { showResetOptions() }
    }
    private fun runRecovery(title: String, action: () -> Unit) {
        if (!SyncService.busy.compareAndSet(false,true)) { message("Operação em andamento","Aguarde a sincronização ou recuperação terminar."); return }
        SyncService.publish(RowdStatus.Kind.Working, title, "Preservando as versões recuperáveis."); refresh()
        Thread {
            try {
                action()
                SyncService.publish(RowdStatus.Kind.Ready, "Recuperação finalizada", "As cópias foram preservadas.")
                runOnUiThread { message("Recuperação concluída", "As cópias foram preservadas.") }
            } catch (e: Exception) {
                SyncService.publish(RowdStatus.Kind.Error, "Falha na recuperação", e.message ?: "Tente exportar as cópias.")
                runOnUiThread { message("Falha na recuperação", e.message ?: "Tente exportar as cópias.") }
            } finally { SyncService.busy.set(false); runOnUiThread { refresh() } }
        }.start()
    }
    private fun startSync() = safely {
        if (SyncService.busy.get()) { SyncService.wake(); return@safely }
        if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != android.content.pm.PackageManager.PERMISSION_GRANTED) notifications.launch(Manifest.permission.POST_NOTIFICATIONS)
        startForegroundService(Intent(this, SyncService::class.java))
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
                SyncService.wake()
                SyncService.publish(RowdStatus.Kind.Ready, "Solicitação de Share salva",
                    if (prefs.contains("invitation")) "Sincronize para enviá-la ao PC." else "Pareie o PC e sincronize para enviá-la.")
                refresh()
            } }
            .show()
    }
    private fun manageRequests() = safely {
        val requests = JSONArray(FolderAccess(this).pendingShareRequests())
        val entries = (0 until requests.length()).map { requests.getJSONObject(it) }
        check(entries.isNotEmpty()) { "Nenhuma solicitação pendente." }
        fun stateLabel(request: JSONObject): String = when (request.optString("state", "pending")) {
            "cancelled" -> "cancelada"
            else -> "pendente"
        }
        val labels = entries.map { request ->
            "${request.getString("name")} · ${stateLabel(request)}"
        }.toTypedArray()
        MaterialAlertDialogBuilder(this).setTitle("Solicitações de Share")
            .setItems(labels) { _, index ->
                val request = entries[index]
                if (request.optString("state", "pending") == "pending") {
                    MaterialAlertDialogBuilder(this).setTitle("Cancelar solicitação?")
                        .setMessage("${request.getString("name")} não será criado no PC.")
                        .setNegativeButton("Voltar", null).setPositiveButton("Cancelar solicitação") { _, _ -> safely {
                            FolderAccess(this).cancelShareRequest(request.getString("request_id"))
                            SyncService.wake()
                            SyncService.publish(RowdStatus.Kind.Ready, "Cancelamento salvo", "A próxima conexão confirmará o cancelamento no PC.")
                            refresh()
                        } }.show()
                } else {
                    message(
                        "Decisão da solicitação",
                        "${request.getString("name")} · ${stateLabel(request)}"
                    )
                }
            }.show()
    }
    private fun showResetOptions() {
        val options = arrayOf(
            "Redefinir preferências da interface",
            "Restaurar configuração inicial",
            "Apagar todos os dados internos do Rowd"
        )
        MaterialAlertDialogBuilder(this).setTitle("Opções de redefinição")
            .setItems(options) { _, index -> when (index) {
                0 -> {
                    prefs.edit().remove("lastStatus").remove("lastDetail").apply()
                    SyncService.publish(RowdStatus.Kind.Ready, "Preferências redefinidas", "Arquivos, Shares e pareamento foram preservados.")
                    refresh()
                }
                1 -> confirmReset(false)
                2 -> confirmReset(true)
            } }.show()
    }
    private fun confirmReset(eraseAll: Boolean) {
        val title = if (eraseAll) "Apagar todos os dados internos?" else "Restaurar configuração inicial?"
        val message = if (eraseAll) "Pareamento, Shares, solicitações e recovery privado serão apagados. Os arquivos nas pastas sincronizadas permanecem." else "Pareamento e Shares serão removidos deste aparelho. Arquivos e recovery serão preservados."
        MaterialAlertDialogBuilder(this).setTitle(title).setMessage(message)
            .setNegativeButton("Cancelar", null).setPositiveButton("Confirmar") { _, _ -> safely {
                check(!SyncService.busy.get()) { "Aguarde a rodada atual terminar." }
                stopService(Intent(this, SyncService::class.java).setAction(SyncService.STOP))
                FolderAccess(this).confirmUnlinked()
                if (eraseAll) filesDir.listFiles()?.forEach { it.deleteRecursively() }
                SyncService.publish(RowdStatus.Kind.Idle, "Rowd redefinido", "Pareie novamente para continuar.")
                refresh()
            } }.show()
    }
    private fun refresh() {
        val busy = SyncService.busy.get()
        val paired = prefs.contains("invitation")
        ui.status.text = SyncService.state.title; ui.detail.text = SyncService.state.detail
        ui.progress.visibility = if (busy) View.VISIBLE else View.GONE
        ui.peerLabel.text = if (paired) prefs.getString("peerAddress", null)
            ?: runCatching { previewInvitation(prefs.getString("invitation", "")!!).getString("address") }.getOrDefault("Convite inválido")
            else "Ainda não pareado"
        ui.importInvite.isEnabled = !busy
        ui.scanQr.isEnabled = !busy
        ui.manageRecovery.isEnabled = !busy
        ui.requestShare.isEnabled = true
        val local = runCatching {
            val access = FolderAccess(this)
            Triple(JSONArray(access.unassignedShares()), JSONArray(access.statusShares()), JSONArray(access.pendingShareRequests()))
        }.getOrElse { error ->
            ui.status.text = "Estado local indisponível"
            ui.detail.text = error.message ?: "Não foi possível ler os vínculos SAF."
            ui.sharesLabel.text = "Confira o armazenamento do aplicativo."
            ui.bindShare.isEnabled = false
            ui.manageRequests.isEnabled = false
            ui.syncNow.isEnabled = false
            ui.exportRecovery.isEnabled = false
            ui.resetApp.isEnabled = false
            return
        }
        val (unassigned, shares, requestEntries) = local
        val sharesText = (0 until shares.length()).joinToString("\n") { i ->
                val share = shares.getJSONObject(i)
                val folderState = if (share.getString("binding_state") != "Bound") " · escolha a pasta Android" else ""
                val enabledState = if (share.optBoolean("enabled", true)) "ativo" else "pausado no PC"
                "${share.getString("name")} · $enabledState · ${share.getString("mode")}$folderState"
            }.ifEmpty { "Nenhum Share aceito ainda." }
        val requestCount = (0 until requestEntries.length()).count {
            requestEntries.getJSONObject(it).optString("state", "pending") == "pending"
        }
        ui.sharesLabel.text = if (requestCount == 0) sharesText else "$sharesText\nSolicitações pendentes: $requestCount"
        ui.bindShare.isEnabled = unassigned.length() > 0
        ui.manageRequests.isEnabled = requestEntries.length() > 0
        ui.changeAddress.isEnabled = paired
        ui.unlinkDevice.isEnabled = paired
        ui.syncNow.isEnabled = paired
        ui.exportRecovery.isEnabled = !busy
        ui.resetApp.isEnabled = !busy
    }
    private fun message(title: String, text: String) { MaterialAlertDialogBuilder(this).setTitle(title).setMessage(text).setPositiveButton("Entendi", null).show() }
    private fun safely(action: () -> Unit) { try { action() } catch (e: Exception) { message("Não foi possível continuar", e.message ?: "Tente novamente.") } }
    override fun onResume() { super.onResume(); SyncService.observe(statusChanged); refresh() }
    override fun onPause() { SyncService.stopObserving(statusChanged); super.onPause() }
}
