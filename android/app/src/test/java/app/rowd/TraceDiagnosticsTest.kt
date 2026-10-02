package app.rowd

import org.junit.Assert.*
import org.junit.Test

class TraceDiagnosticsTest {
    @Test fun capturesActualCallsite() {
        val expectedLine = Throwable().stackTrace[0].lineNumber + 1
        val source = traceSource()
        assertEquals("TraceDiagnosticsTest.kt", source.file)
        assertEquals(expectedLine, source.line)
        assertEquals("capturesActualCallsite", source.function)
    }

    @Test fun validEventKeepsBothProducersActive() {
        val state = TraceProducerState().apply { active = true }
        val reports = mutableListOf<String>()
        state.deliver({ true }, { TraceWriterState(true, null) }, reports::add)
        assertTrue(state.active)
        assertNull(state.failure)
        assertTrue(reports.isEmpty())
    }

    @Test fun skipsProducerAndDefaultArgumentWrappers() {
        val frames = arrayOf(
            StackTraceElement("dalvik.system.VMStack", "getThreadStackTrace", "VMStack.java", -2),
            StackTraceElement("app.rowd.TraceDiagnosticsKt", "traceSource\$default", "TraceDiagnostics.kt", 6),
            StackTraceElement("app.rowd.PerformanceTrace\$event\$1", "invoke", "PerformanceTrace.kt", 100),
            StackTraceElement("app.rowd.TraceProducerState", "deliver", "TraceDiagnostics.kt", 35),
            StackTraceElement("app.rowd.PerformanceTrace", "event\$default", "PerformanceTrace.kt", 93),
            StackTraceElement("app.rowd.SyncService\$Companion", "traceEvent", "SyncService.kt", 38),
            StackTraceElement("app.rowd.SyncService\$Companion", "traceEvent\$default", "SyncService.kt", 36),
            StackTraceElement("app.rowd.SyncService", "access\$traceEvent", "SyncService.kt", 34),
            StackTraceElement("app.rowd.SyncService", "refreshObservers", "SyncService.kt", 228)
        )
        assertEquals(TraceSource("SyncService.kt", 228, "refreshObservers"), traceSource(frames))
        val fallbackFrames = arrayOf(
            StackTraceElement("app.rowd.FolderAccess", "traceFallback", "FolderAccess.kt", 53),
            StackTraceElement("app.rowd.FolderAccess", "traceFallback\$default", "FolderAccess.kt", 48),
            StackTraceElement("app.rowd.FolderAccess", "scanPathsJson", "FolderAccess.kt", 278)
        )
        assertEquals(TraceSource("FolderAccess.kt", 278, "scanPathsJson"), traceSource(fallbackFrames))
        assertEquals(0, traceSource(emptyArray()).line)
    }

    @Test fun rejectedEventKeepsNativeWriterState() {
        val state = TraceProducerState().apply { active = true }
        val reports = mutableListOf<String>()
        state.deliver({ false }, { TraceWriterState(true, null) }, reports::add)
        assertTrue(state.active)
        assertNull(state.failure)
        assertTrue(reports.single().startsWith("INGEST_ANDROID_FAILED"))
        state.deliver({ throw IllegalArgumentException("invalid event") },
            { TraceWriterState(true, null) }, reports::add)
        assertTrue(state.active)
    }

    @Test fun writerFailureDisablesProducerAndPreservesFailure() {
        val state = TraceProducerState().apply { active = true }
        val reports = mutableListOf<String>()
        state.deliver({ false }, { TraceWriterState(false, "TRACE_WRITER_FAILED: disk full") }, reports::add)
        assertFalse(state.active)
        assertEquals("TRACE_WRITER_FAILED: disk full", state.failure)
        assertTrue(reports.contains(state.failure))
    }
}
