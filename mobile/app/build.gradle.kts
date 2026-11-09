import java.util.Properties

plugins {
    kotlin("multiplatform")
    id("org.jetbrains.compose")
    id("org.jetbrains.kotlin.plugin.compose")
    id("com.android.application")
}

// Release signing comes from the environment in CI, mobile/keystore.properties,
// or the ignored PKCS#12 key in the repository root. On macOS the local key's
// password stays in Keychain rather than in this repository.
val keystoreProperties = Properties().apply {
    val file = rootProject.file("keystore.properties")
    if (file.exists()) file.inputStream().use { load(it) }
}
fun signingValue(env: String, property: String): String? =
    System.getenv(env)?.takeIf { it.isNotBlank() }
        ?: keystoreProperties.getProperty(property)?.takeIf { it.isNotBlank() }
fun mobileReleaseKeychainPassword(): String? = runCatching {
    if (!System.getProperty("os.name").startsWith("Mac", ignoreCase = true)) return@runCatching null
    val process = ProcessBuilder(
        "/usr/bin/security", "find-generic-password",
        "-s", "tty7-mobile-release-key", "-a", "io.tty7.mobile", "-w",
    ).redirectError(ProcessBuilder.Redirect.DISCARD).start()
    val password = process.inputStream.bufferedReader().use { it.readText() }.trim()
    password.takeIf { process.waitFor() == 0 && it.isNotBlank() }
}.getOrNull()

val repositoryReleaseStore = rootProject.projectDir.parentFile.resolve("tty7-mobile-release.p12").takeIf { it.isFile }
val localKeychainPassword = repositoryReleaseStore?.let { mobileReleaseKeychainPassword() }
val releaseStoreFile = signingValue("ANDROID_KEYSTORE_PATH", "storeFile") ?: repositoryReleaseStore?.absolutePath
val releaseStorePassword = signingValue("ANDROID_KEYSTORE_PASSWORD", "storePassword") ?: localKeychainPassword
val releaseKeyAlias = signingValue("ANDROID_KEY_ALIAS", "keyAlias")
    ?: repositoryReleaseStore?.let { "tty7-mobile-release" }
val releaseKeyPassword = signingValue("ANDROID_KEY_PASSWORD", "keyPassword") ?: localKeychainPassword
val hasReleaseSigning = listOf(
    releaseStoreFile, releaseStorePassword, releaseKeyAlias, releaseKeyPassword,
).all { it != null }
kotlin {
    androidTarget { compilerOptions { jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17) } }
    sourceSets {
        commonMain.dependencies {
            implementation(compose.runtime)
            implementation(compose.foundation)
            implementation(compose.material3)
            implementation(compose.ui)
            implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2")
            implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.8.1")
        }
        commonTest.dependencies {
            implementation(kotlin("test"))
            implementation("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.10.2")
        }
        androidUnitTest.dependencies {
            implementation(kotlin("test-junit"))
            implementation("junit:junit:4.13.2")
        }
        androidInstrumentedTest.dependencies {
            implementation(kotlin("test-junit"))
            implementation("androidx.test:runner:1.6.2")
        }
        androidMain.dependencies {
            implementation("androidx.activity:activity-compose:1.10.1")
            implementation("androidx.lifecycle:lifecycle-runtime-compose:2.9.0")
            implementation("androidx.camera:camera-camera2:1.4.2")
            implementation("androidx.camera:camera-lifecycle:1.4.2")
            implementation("androidx.camera:camera-view:1.4.2")
            implementation("com.google.mlkit:barcode-scanning:17.3.0")
            implementation("com.squareup.okhttp3:okhttp:4.12.0")
        }
    }
}
android {
    namespace = "io.tty7.mobile"
    compileSdk = 35
    defaultConfig {
        applicationId = "io.tty7.mobile"
        minSdk = 26
        targetSdk = 35
        versionCode = 3
        versionName = "1.0.2"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }
    sourceSets["main"].manifest.srcFile("src/androidMain/AndroidManifest.xml")
    sourceSets["main"].assets.srcDirs("src/androidMain/assets")
    sourceSets["main"].res.srcDirs("src/androidMain/res")
    compileOptions { sourceCompatibility = JavaVersion.VERSION_17; targetCompatibility = JavaVersion.VERSION_17 }
    buildFeatures { buildConfig = true }
    signingConfigs {
        if (hasReleaseSigning) {
            create("release") {
                val signingStoreFile = releaseStoreFile!!
                storeFile = rootProject.file(signingStoreFile)
                storePassword = releaseStorePassword
                keyAlias = releaseKeyAlias
                keyPassword = releaseKeyPassword
                if (signingStoreFile.endsWith(".p12", ignoreCase = true) ||
                    signingStoreFile.endsWith(".pfx", ignoreCase = true)) storeType = "PKCS12"
            }
        }
    }
    buildTypes {
        getByName("debug") { manifestPlaceholders["cleartextAllowed"] = "true" }
        getByName("release") {
            manifestPlaceholders["cleartextAllowed"] = "false"
            isMinifyEnabled = false
            if (hasReleaseSigning) signingConfig = signingConfigs.getByName("release")
        }
    }
}
