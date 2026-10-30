import java.util.Properties
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// Release signing material is never committed. It lives in
// %LOCALAPPDATA%\BubblePlay\android-keystore\ (see android/README.md); without
// it the release APK is built unsigned.
val keystoreDir = File(
    System.getenv("LOCALAPPDATA") ?: System.getProperty("user.home"),
    "BubblePlay/android-keystore",
)
val keystoreProps = Properties().apply {
    val file = File(keystoreDir, "keystore.properties")
    if (file.isFile) file.inputStream().use { load(it) }
}

android {
    namespace = "de.bubblebuffer.bubbleplay"
    compileSdk = 35

    defaultConfig {
        applicationId = "de.bubblebuffer.bubbleplay"
        minSdk = 29
        targetSdk = 35
        // Every build must outrank the installed one so update stores (F-Droid repo,
        // Obtainium) offer it: minutes since the Unix epoch unless -PversionCode is given.
        val buildMinute = System.currentTimeMillis() / 60_000
        versionCode = (findProperty("versionCode") as String?)?.toInt() ?: buildMinute.toInt()
        versionName = "0.1.0-$buildMinute"
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
            isMinifyEnabled = false
            signingConfig = signingConfigs.findByName("release")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    lint {
        abortOnError = true
        checkDependencies = false
    }

    testOptions {
        // Robolectric (ReadmeScreenshotTest) renders the real layouts, so the JVM tests need the app's resources.
        unitTests.isIncludeAndroidResources = true
        unitTests.all {
            // The JVM tests read the shared contracts in <repo>/fixtures.
            it.systemProperty("bubbleplay.fixtures", rootProject.file("../fixtures").absolutePath)
            // Where ReadmeScreenshotTest writes its PNGs; CI uploads this folder as an artifact.
            it.systemProperty(
                "bubbleplay.screenshots",
                layout.buildDirectory.dir("screenshots").get().asFile.absolutePath,
            )
            // Relay e2e tests (src/test/.../e2e): BP_RELAY_SERVER_BIN names the bp-relay binary, and
            // BP_E2E_REQUIRED=1 (CI) turns a missing binary into a failure instead of a skip.
            it.inputs.property("bpRelayServerBin", System.getenv("BP_RELAY_SERVER_BIN") ?: "")
            it.inputs.property("bpE2eRequired", System.getenv("BP_E2E_REQUIRED") ?: "")
            it.testLogging {
                events("passed", "skipped", "failed")
                exceptionFormat = org.gradle.api.tasks.testing.logging.TestExceptionFormat.FULL
            }
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_17)
    }
}

dependencies {
    implementation("com.squareup.okhttp3:okhttp:4.12.0")
    implementation("org.bouncycastle:bcprov-jdk18on:1.86")
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")
    // Released 2026-09-10; 4.17 is the newest 4.x and supports compileSdk 35.
    testImplementation("org.robolectric:robolectric:4.17")
}
