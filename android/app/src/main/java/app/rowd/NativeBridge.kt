package app.rowd

object NativeBridge {
    init { System.loadLibrary("rowd_android") }
    external fun sync(invitation: String, deviceId: String, access: FolderAccess): String
}
