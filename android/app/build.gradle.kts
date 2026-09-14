import ai.diffforge.haider.build.HaiderVersion
import ai.diffforge.haider.build.HaiderNative
import ai.diffforge.haider.build.HaiderJniLibs
import ai.diffforge.haider.build.VerifyHaiderNative

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

val workspaceVersion = providers.gradleProperty("haiderVersion")
    .orNull
    ?.let(HaiderVersion::parse)
    ?: HaiderVersion.fromWorkspaceManifest(rootProject.projectDir.parentFile.resolve("Cargo.toml").readText())

val releaseSigningValues = mapOf(
    "path" to System.getenv("ANDROID_KEYSTORE_PATH"),
    "storePassword" to System.getenv("ANDROID_KEYSTORE_PASSWORD"),
    "keyAlias" to System.getenv("ANDROID_KEY_ALIAS"),
    "keyPassword" to System.getenv("ANDROID_KEY_PASSWORD"),
)
val releaseKeystore = releaseSigningValues.getValue("path")?.let(::file)
val releaseSigningAvailable = releaseSigningValues.values.all { !it.isNullOrBlank() } &&
    releaseKeystore?.isFile == true

if (!releaseSigningAvailable) {
    logger.lifecycle("signing skipped: secrets absent or release keystore unavailable")
}

android {
    namespace = "ai.diffforge.haider"
    compileSdk = 35
    ndkVersion = "28.2.13676358"

    defaultConfig {
        applicationId = "ai.diffforge.haider"
        minSdk = 26
        targetSdk = 35
        versionCode = workspaceVersion.code
        versionName = workspaceVersion.name
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        buildConfigField("boolean", "LEGACY_TERMUX_TRANSPORT", "false")
    }

    flavorDimensions += "device"
    productFlavors {
        create("phone") {
            dimension = "device"
            ndk { abiFilters += "arm64-v8a" }
        }
        create("emulator") {
            dimension = "device"
            ndk { abiFilters += "x86_64" }
        }
    }

    // AGP 8.7.3 defaults to uncompressed, directly mapped JNI libraries for minSdk 26.
    // Keep native packaging defaults; no extraction workaround.
    sourceSets.getByName("test").resources.srcDir("../../crates/haider-rpc/tests/fixtures")
    sourceSets.getByName("androidTest").assets.srcDir("../../crates/haider-rpc/tests/fixtures")

    signingConfigs {
        if (releaseSigningAvailable) {
            create("release") {
                storeFile = releaseKeystore
                storePassword = releaseSigningValues.getValue("storePassword")
                keyAlias = releaseSigningValues.getValue("keyAlias")
                keyPassword = releaseSigningValues.getValue("keyPassword")
                enableV1Signing = true
                enableV2Signing = true
                enableV3Signing = true
            }
        }
    }

    buildTypes {
        release {
            if (releaseSigningAvailable) {
                signingConfig = signingConfigs.getByName("release")
            }
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
    buildFeatures {
        aidl = true
        compose = true
        buildConfig = true
    }

    // Compose UI tests run on the JVM under Robolectric, not on an emulator:
    // the release job runs :app:testReleaseUnitTest on a runner with no AVD
    // (.github/workflows/android-apk.yml), and a connectedAndroidTest would need
    // a new emulator job that the release would then be gated on (UI-SPEC 6.4).
    testOptions {
        unitTests {
            isIncludeAndroidResources = true
            all {
                it.systemProperty("robolectric.graphicsMode", "NATIVE")
                it.systemProperty(
                    "roborazzi.test.record",
                    (project.findProperty("roborazzi.test.record") ?: "false").toString(),
                )
                it.systemProperty(
                    "roborazzi.test.verify",
                    (project.findProperty("roborazzi.test.verify") ?: "false").toString(),
                )
                it.systemProperty(
                    "roborazzi.test.compare",
                    (project.findProperty("roborazzi.test.compare") ?: "false").toString(),
                )
                it.maxHeapSize = "2g"
            }
        }
    }
}

val usePrebuiltNative = providers.gradleProperty("haiderNativePrebuilt").map(String::toBoolean).orElse(false).get()
val nativeOutput = if (usePrebuiltNative) {
    // Separate from producer outputs, including history from local native builds.
    rootProject.layout.projectDirectory.dir("../dist-android-native")
} else {
    layout.buildDirectory.dir("generated/haiderNative").get()
}
val nativeBuild = if (usePrebuiltNative) {
    tasks.register<VerifyHaiderNative>("buildHaiderNative") {
        repository.set(rootProject.layout.projectDirectory.dir(".."))
        version.set(workspaceVersion.name)
        nativeDirectory.set(nativeOutput)
    }
} else {
    tasks.register<HaiderNative>("buildHaiderNative") {
        repository.set(rootProject.layout.projectDirectory.dir(".."))
        version.set(workspaceVersion.name)
        sources.from(fileTree(rootProject.projectDir.parentFile) {
            include("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml", "crates/**",
                "scripts/android/**", "android/buildSrc/**", ".github/workflows/android-apk.yml", "customprov.bundle")
            exclude("**/target/**", "**/__pycache__/**", "android/buildSrc/build/**",
                "android/buildSrc/.gradle/**", "android/buildSrc/.kotlin/**")
        })
        outputDirectory.set(nativeOutput)
    }
}
androidComponents {
    beforeVariants(selector().withBuildType("release")) { variant ->
        if (variant.productFlavors.any { it.second == "emulator" }) variant.enable = false
    }
    onVariants { variant ->
        val jni = tasks.register<HaiderJniLibs>("prepare${variant.name.replaceFirstChar { it.uppercaseChar() }}HaiderJniLibs") {
            dependsOn(nativeBuild)
            nativeDirectory.set(nativeOutput)
            abi.set(if (variant.productFlavors.any { it.second == "emulator" }) "x86_64" else "arm64-v8a")
            outputDirectory.set(layout.buildDirectory.dir("generated/haiderJniLibs/${variant.name}"))
        }
        variant.sources.jniLibs?.addGeneratedSourceDirectory(jni, HaiderJniLibs::outputDirectory)
    }
}

// Keep the separately owned xplat compile entrypoint working after introducing ABI flavors.
tasks.register("compileReleaseKotlin") { dependsOn("compilePhoneReleaseKotlin") }
tasks.register("testDebugUnitTest") { dependsOn("testPhoneDebugUnitTest", "testEmulatorDebugUnitTest") }
tasks.register("testReleaseUnitTest") { dependsOn("testPhoneReleaseUnitTest") }

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2024.12.01")
    implementation(composeBom)

    implementation("androidx.browser:browser:1.8.0")
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.7")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")
    implementation("androidx.work:work-runtime-ktx:2.10.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")

    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-extended")

    debugImplementation("androidx.compose.ui:ui-tooling")

    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.7.3")
    testImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.9.0")
    androidTestImplementation("androidx.test:runner:1.6.2")
    androidTestImplementation("androidx.test.ext:junit:1.2.1")
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")
    testImplementation("org.robolectric:robolectric:4.14.1")
    testImplementation("androidx.test.ext:junit:1.2.1")
    testImplementation(composeBom)
    testImplementation("androidx.compose.ui:ui-test-junit4")
    testImplementation("io.github.takahirom.roborazzi:roborazzi:1.32.2")
    testImplementation("io.github.takahirom.roborazzi:roborazzi-compose:1.32.2")
    debugImplementation("androidx.compose.ui:ui-test-manifest")
}
