plugins {
    id("com.android.application")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
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

    buildTypes {
        release {
            // TODO: Add your own signing config for the release build.
            // Signing with the debug keys for now, so `flutter run --release` works.
            signingConfig = signingConfigs.getByName("debug")
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

// The Kotlin core (keys, auth, pcs, pairing) is compiled from the native app's source tree. A Sync
// task stages only the packages the app contract needs (the Kotlin compile task ignores include
// filters on shared source dirs); everything else there (ui, media, input, files, protocol, the
// session) is left out. The staged copy is build output and is regenerated on every build.
val nativeCore = rootProject.file("../../android/app/src/main/java")
val stageNativeCore by tasks.registering(Sync::class) {
    from(nativeCore) {
        include(
            "de/bubblebuffer/bubbleplay/keys/**",
            "de/bubblebuffer/bubbleplay/auth/**",
            "de/bubblebuffer/bubbleplay/log/**",
            "de/bubblebuffer/bubbleplay/pcs/**",
            "de/bubblebuffer/bubbleplay/net/PairingClient.kt",
            "de/bubblebuffer/bubbleplay/net/ServerConfig.kt",
        )
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
