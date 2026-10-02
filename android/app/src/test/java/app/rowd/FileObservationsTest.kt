package app.rowd

import org.junit.Assert.*
import org.junit.Test

class FileObservationsTest {
    @Test fun knownRemoteInstallSurvivesSubsequentScan() {
        val state = FileObservations()
        state.remoteInstalled("file-id", "verified-hash")
        assertEquals("remote_install", state.firstSeen("file-id", "verified-hash"))
        assertNull(state.firstSeen("file-id", "verified-hash"))
    }
    @Test fun noEvidenceDoesNotGuessLocalOrigin() {
        val state = FileObservations()
        state.remoteInstalled("changed-after-install", "old-hash")
        assertEquals("unknown", state.firstSeen("changed-after-install", "different-hash"))
        assertEquals("unknown", state.firstSeen("preexisting", "hash"))
        state.clear()
        assertEquals("unknown", state.firstSeen("changed-after-install", "old-hash"))
    }
}
