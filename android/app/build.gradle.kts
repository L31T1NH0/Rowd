plugins { id("com.android.application"); id("org.jetbrains.kotlin.android") }
android {
    namespace = "app.rowd"
    compileSdk = 35
    defaultConfig {
        applicationId = "app.rowd"
        minSdk = 26
        targetSdk = 35
        versionCode = 14
        versionName = "0.8.1-alpha"
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
            isDebuggable = false
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
