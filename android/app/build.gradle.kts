plugins { id("com.android.application"); id("org.jetbrains.kotlin.android") }
android {
    namespace = "app.rowd"
    compileSdk = 35
    useLibrary("android.test.runner")
    useLibrary("android.test.mock")
    useLibrary("android.test.base")
    testBuildType = "release"
    defaultConfig {
        applicationId = "app.rowd"
        minSdk = 26
        targetSdk = 35
        versionCode = 20
        versionName = "0.8.7-alpha"
        testInstrumentationRunner = "android.test.InstrumentationTestRunner"
        ndk { abiFilters += "arm64-v8a" }
    }
    compileOptions { sourceCompatibility = JavaVersion.VERSION_17; targetCompatibility = JavaVersion.VERSION_17 }
    kotlinOptions { jvmTarget = "17" }
    buildFeatures { viewBinding = true }
    buildTypes {
        release { signingConfig = signingConfigs.getByName("debug") }
        create("diagnostic") {
            initWith(getByName("release"))
            matchingFallbacks += listOf("release")
            isDebuggable = true
            proguardFiles("trace-proguard-rules.pro")
        }
    }
}
androidComponents {
    beforeVariants(selector().withBuildType("debug")) { it.enable = false }
}
dependencies {
    testImplementation("junit:junit:4.13.2")
    implementation("com.journeyapps:zxing-android-embedded:4.3.0")
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.documentfile:documentfile:1.0.1")
}

// Digest tests exercise the exact JNI implementation shipped in the APK.
val buildHostDigest by tasks.registering(Exec::class) {
    workingDir(rootProject.projectDir.parentFile)
    commandLine("cargo", "build", "-p", "rowd-android", "--locked", "--offline")
}
tasks.withType<Test>().configureEach {
    dependsOn(buildHostDigest)
    systemProperty("java.library.path", rootProject.projectDir.resolve("../target/debug").absolutePath)
}
