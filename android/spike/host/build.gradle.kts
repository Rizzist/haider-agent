plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}
android {
    namespace = "ai.diffforge.haider.spike"
    compileSdk = 35
    ndkVersion = "28.2.13676358"
    defaultConfig {
        applicationId = "ai.diffforge.haider.spike"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "971-1-spike"
    }
    packaging { jniLibs { useLegacyPackaging = false } }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}
