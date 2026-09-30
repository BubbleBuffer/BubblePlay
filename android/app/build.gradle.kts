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
        versionCode = 1
        versionName = "0.1.0"
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
        unitTests.all {
            // The JVM tests read the shared contracts in <repo>/fixtures.
            it.systemProperty("bubbleplay.fixtures", rootProject.file("../fixtures").absolutePath)
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
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")
}
