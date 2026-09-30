package app.rowd

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class NetworkPathTest {
    @Test fun equivalentCallbacksKeepConnection() {
        val original = NetworkPath(10, "wlan0", "192.168.1.8")
        val incomplete = NetworkPath(10, null, null)
        assertFalse(incomplete.changedFrom(original))
        assertFalse(incomplete.retainingKnown(original).changedFrom(original))
        assertFalse(original.changedFrom(original))
    }

    @Test fun realPathChangesInvalidateConnection() {
        val original = NetworkPath(10, "wlan0", "192.168.1.8")
        assertTrue(NetworkPath(11, null, null).changedFrom(original))
        assertTrue(NetworkPath(10, "wlan1", "192.168.1.8").changedFrom(original))
        assertTrue(NetworkPath(10, "wlan0", "192.168.1.9").changedFrom(original))
    }
}
