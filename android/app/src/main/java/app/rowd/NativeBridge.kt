package app.rowd

object NativeBridge {
    init { System.loadLibrary("rowd_android") }
    external fun sync(invitation: String, deviceId: String, focusJson: String, access: FolderAccess): String
    external fun pollWake(auditInMs: Long): String
    external fun signalIdle(reason: Int)
    external fun networkChanged()
    external fun previewInvitation(invitation: String, address: String): String
    external fun discoverPairing(): String
    external fun pollPairOffer(deviceName: String, sessionId: String): String
    external fun revokeRemotePairing(invitation: String, deviceId: String): String
    external fun clearPersistentConnection()
    external fun beginPairing(peer: String, deviceId: String, deviceName: String): String
    external fun finishPairing(): String
    external fun cancel()
    external fun resetCancellation()
    external fun setTrace(path: String, sessionId: String = ""): Boolean
    external fun traceEvent(event: String): Boolean
    external fun traceRuntimeState(): String
    external fun stopTrace(termination: String): Boolean
    external fun flushTrace()
}
