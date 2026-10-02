# Diagnostic locations use explicit callsite stamps; preserve fallback metadata and JNI names too.
-keepattributes SourceFile,LineNumberTable
-keep class app.rowd.PerformanceTrace { *; }
-keep class app.rowd.FolderAccess { public *; }
-keep class app.rowd.NativeBridge { *; }
-keep class app.rowd.TraceDiagnosticsKt { *; }
