package app.rowd

object NativeBridge {
    init { System.loadLibrary("rowd_android") }
    external fun sync(invitation: String, rootId: String, access: FolderAccess): String
}
