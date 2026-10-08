package app.rowd

/** The same native BLAKE3 implementation used by the PC. No persistent native handles. */
internal object ContentDigest {
    init { System.loadLibrary("rowd_android") }
    external fun hashStream(input: java.io.InputStream, copy: java.io.OutputStream?, checkControl: () -> Unit): String
}
