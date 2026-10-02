package app.rowd

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.ContentValues
import android.provider.MediaStore
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject

/** Shell-only controls in the diagnostic APK. No command accepts a path or Share ID. */
class DiagnosticReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        try {
            when (intent.getStringExtra("command")) {
                "trace-start" -> PerformanceTrace.enable(context)
                "trace-stop" -> PerformanceTrace.disable()
                "trace-flush" -> PerformanceTrace.flush()
                "trace-location-check" -> PerformanceTrace.event("DIAGNOSTIC_SOURCE_CHECK", null,
                    component = PerformanceTrace.Component.Service, function = "onReceive", sourceFile = "DiagnosticReceiver.kt", sourceLine = 20)
                "idle-begin", "idle-end" -> PerformanceTrace.event(
                    if (intent.getStringExtra("command") == "idle-begin") "IDLE_VALIDATION_BEGIN" else "IDLE_VALIDATION_END", null,
                    component = PerformanceTrace.Component.Service, detail = JSONObject(NativeBridge.traceRuntimeState()), sourceFile = "DiagnosticReceiver.kt", sourceLine = 22)
                "sync-start" -> if (!SyncService.busy.get()) context.startForegroundService(Intent(context, SyncService::class.java))
                "sync-now" -> {
                    val shares = JSONArray(FolderAccess(context).knownShares())
                    for (index in 0 until shares.length())
                        SyncService.wake(shares.getJSONObject(index).getString("share_id"))
                    if (!SyncService.busy.get()) context.startForegroundService(Intent(context, SyncService::class.java))
                }
                "trace-export" -> {
                    val values = ContentValues().apply {
                        put(MediaStore.Downloads.DISPLAY_NAME, "rowd-performance-trace-${System.currentTimeMillis()}.zip")
                        put(MediaStore.Downloads.MIME_TYPE, "application/zip")
                        put(MediaStore.Downloads.RELATIVE_PATH, "Download/")
                    }
                    val uri = context.contentResolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                        ?: error("Could not create trace export")
                    PerformanceTrace.export(context, uri)
                    Log.i("RowdDiagnostic", "trace_export=${uri.lastPathSegment}")
                }
                else -> error("Unknown diagnostic command")
            }
            Log.i("RowdDiagnostic", "command=${intent.getStringExtra("command")} result=ok")
        } catch (error: Exception) {
            Log.e("RowdDiagnostic", "command failed", error)
        }
    }
}
