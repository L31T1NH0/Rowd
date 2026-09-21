plugins { id("com.android.application"); id("org.jetbrains.kotlin.android") }
android {
    namespace = "app.rowd"
    compileSdk = 35
    defaultConfig {
        applicationId = "app.rowd"
        minSdk = 26
        targetSdk = 35
        versionCode = 4
        versionName = "0.3.0"
        ndk { abiFilters += "arm64-v8a" }
    }
    compileOptions { sourceCompatibility = JavaVersion.VERSION_17; targetCompatibility = JavaVersion.VERSION_17 }
    kotlinOptions { jvmTarget = "17" }
    buildFeatures { viewBinding = true }
}
dependencies {
    implementation("com.journeyapps:zxing-android-embedded:4.3.0")
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.documentfile:documentfile:1.0.1")
}
