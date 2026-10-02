package app.rowd

import org.junit.Assert.*
import org.junit.Test

class ObserverHintsTest {
    private val provider = "com.android.externalstorage.documents"
    private fun hint(uri: String?, segments: List<String> = emptyList(), file: Boolean = false,
        directory: Boolean = false, document: String? = null) = observerHint(uri, provider, provider,
        segments, file, directory, "primary:Rowd", document)
    @Test fun genericCallbackNeverEntersExactDocumentLookup() {
        assertEquals(ObserverHint.PROVIDER_WIDE_URI, hint("content://$provider"))
        assertEquals(ObserverHint.NULL_URI, hint(null))
    }
    @Test fun specificFileDirectoryAndOtherTreeAreDistinct() {
        assertEquals(ObserverHint.KNOWN_FILE_URI, hint("file", listOf("document"), file = true))
        assertEquals(ObserverHint.KNOWN_DIRECTORY_URI, hint("directory", listOf("document"), directory = true))
        assertEquals(ObserverHint.UNKNOWN_SPECIFIC_URI, hint("new", listOf("document"), document = "primary:Rowd/new"))
        assertEquals(ObserverHint.UNRELATED_URI, hint("other", listOf("document"), document = "primary:Other/file"))
    }
    @Test fun metadataDiffFindsChangesWithoutHashingUnchangedFiles() {
        val previous = DocumentMetadata("uri", 100, 5)
        assertFalse(previous.differsFrom(previous))
        assertTrue(previous.copy(modified = 101).differsFrom(previous))
        assertTrue(previous.copy(length = 6).differsFrom(previous))
        assertTrue(previous.copy(uri = "new-uri").differsFrom(previous))
        assertTrue(previous.copy(modified = 0).differsFrom(previous))
        assertTrue(previous.differsFrom(null))
    }
}
