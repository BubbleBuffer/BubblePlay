plugins {
    id("com.android.application")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

// Release signing material is never committed. Same source as the native app (android/): the
// keystore dir's keystore.properties; BP_KEYSTORE_DIR overrides the dir. Without it the
// release APK is built unsigned (never debug-signed); the publish host signs it.
val keystoreDir = File(
    System.getenv("BP_KEYSTORE_DIR")
        ?: File(System.getenv("LOCALAPPDATA") ?: System.getProperty("user.home"), "BubblePlay/android-keystore").path,
)
val keystoreProps = Properties().apply {
    val file = File(keystoreDir, "keystore.properties")
    if (file.isFile) file.inputStream().use { load(it) }
}

android {
    namespace = "de.bubblebuffer.bubbleplay.flutter"
    compileSdk = flutter.compileSdkVersion
    ndkVersion = flutter.ndkVersion

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        // TODO: Specify your own unique Application ID (https://developer.android.com/studio/build/application-id.html).
        applicationId = "de.bubblebuffer.bubbleplay.flutter"
        // You can update the following values to match your application needs.
        // For more information, see: https://flutter.dev/to/review-gradle-config.
        minSdk = 29
        targetSdk = flutter.targetSdkVersion
        // Uses the version code from pubspec.yaml. When using split APKs, 1000 * ABI_VERSION
        // is added automatically by Flutter. (https://developer.android.com/studio/build/configure-apk-splits#configure-APK-versions)
        // You can force using the value of versionCode by specifying the `-P force-version-code-ignoring-abi=true`
        // flag during build.
        versionCode = flutter.versionCode
        versionName = flutter.versionName
    }

    signingConfigs {
        if (keystoreProps.getProperty("storeFile") != null) {
            create("release") {
                storeFile = File(keystoreDir, keystoreProps.getProperty("storeFile"))
                storePassword = keystoreProps.getProperty("storePassword")
                keyAlias = keystoreProps.getProperty("keyAlias")
                keyPassword = keystoreProps.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            // Never the debug key: unsigned unless the release keystore is supplied. The
            // published APK is signed on linux01 (apk-publish bubbleplay-flutter).
            signingConfig = signingConfigs.findByName("release")
            // Not R8-tested with the Kotlin core (BouncyCastle, OkHttp): ship it unshrunk.
            isMinifyEnabled = false
            isShrinkResources = false
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

flutter {
    source = "../.."
}

// The Kotlin core (keys, auth, pcs, pairing, and for the stream: net, media, input, protocol) is
// compiled from the native app's source tree. A Sync task stages only the packages the app
// contract needs (the Kotlin compile task ignores include filters on shared source dirs);
// everything else there (ui, files) is left out, and so is input/Shortcuts.kt, the one file of
// those packages that needs the native app's resources (R). The staged copy is build output and
// is regenerated on every build.
val nativeCore = rootProject.file("../../android/app/src/main/java")
val stageNativeCore by tasks.registering(Sync::class) {
    from(nativeCore) {
        include(
            "de/bubblebuffer/bubbleplay/keys/**",
            "de/bubblebuffer/bubbleplay/auth/**",
            "de/bubblebuffer/bubbleplay/log/**",
            "de/bubblebuffer/bubbleplay/pcs/**",
            "de/bubblebuffer/bubbleplay/net/**",
            "de/bubblebuffer/bubbleplay/media/**",
            "de/bubblebuffer/bubbleplay/input/**",
            "de/bubblebuffer/bubbleplay/protocol/**",
        )
        exclude("de/bubblebuffer/bubbleplay/input/Shortcuts.kt")
    }
    into(layout.buildDirectory.dir("nativeCore"))
}
android.sourceSets.getByName("main").java.srcDir(layout.buildDirectory.dir("nativeCore").get().asFile)
tasks.configureEach {
    if (name.startsWith("compile") && name.contains("Kotlin")) dependsOn(stageNativeCore)
    if (name.startsWith("ksp") || name.startsWith("lint") || name.endsWith("JavaWithJavac")) dependsOn(stageNativeCore)
}

dependencies {
    implementation("com.squareup.okhttp3:okhttp:4.12.0")
    implementation("org.bouncycastle:bcprov-jdk18on:1.86")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")
}

android.testOptions.unitTests.all {
    // The JVM tests read the shared contract fixtures in <repo>/app/test/fixtures.
    it.systemProperty("bubbleplay.contract", rootProject.file("../test/fixtures/contract").absolutePath)
}
