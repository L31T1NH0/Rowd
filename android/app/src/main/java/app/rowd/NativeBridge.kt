package app.rowd

object NativeBridge {
    init { System.loadLibrary("rowd_android") }
    external fun sync(invitation: String, deviceId: String, focusJson: String, access: FolderAccess): String
    external fun pollWake(): String
    external fun networkChanged()
    external fun previewInvitation(invitation: String, address: String): String
    external fun discoverPairing(): String
    external fun pollPairOffer(deviceName: String): String
    external fun beginPairing(peer: String, deviceId: String, deviceName: String): String
    external fun finishPairing(): String
    external fun cancel()
    external fun resetCancellation()
    external fun setTrace(path: String): Boolean
    external fun flushTrace()
}
