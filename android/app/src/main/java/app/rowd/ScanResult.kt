package app.rowd

/** Local lifecycle identity; it never crosses the protocol/JNI payload boundary. */
internal data class ScanIdentity(val owner: String, val epoch: Long, val share: String,
    val tree: String, val binding: String, val generation: Long) {
    fun matches(current: ScanIdentity, requireGeneration: Boolean): Boolean =
        (if (requireGeneration) this else copy(generation = current.generation)) == current
}

/** Only the consuming coordinator publishes; abandoned private results are disposed once. */
internal class PreparedScan(val identity: ScanIdentity, val payload: String,
    val sequence: Long = 0, val path: String? = null,
    private val validateResult: () -> Unit = {},
    private val publishResult: () -> Unit = {}, private val discardResult: () -> Unit = {}) {
    private var settled = false
    fun consume(expected: ScanIdentity, current: ScanIdentity, expectedSequence: Long = 0,
        expectedPath: String? = null, requireGeneration: Boolean = true, cancelled: Boolean = false): String {
        try {
            check(!cancelled && identity == expected && sequence == expectedSequence && path == expectedPath &&
                expected.matches(current, requireGeneration)) { "AUDIT_DEFERRED" }
            validate()
            publish()
            return payload
        } finally { discard() }
    }
    fun validate() {
        check(!settled) { "Scan result already consumed." }
        validateResult()
    }
    fun publish() {
        check(!settled) { "Scan result already consumed." }
        publishResult()
        settled = true
    }
    fun discard() {
        if (!settled) {
            settled = true
            discardResult()
        }
    }
}
