plugins {
    kotlin("multiplatform")
    id("org.jetbrains.compose")
    id("org.jetbrains.kotlin.plugin.compose")
    id("com.android.application")
}
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
        androidMain.dependencies {
            implementation("androidx.activity:activity-compose:1.10.1")
            implementation("androidx.lifecycle:lifecycle-runtime-compose:2.9.0")
            implementation("androidx.camera:camera-camera2:1.4.2")
            implementation("androidx.camera:camera-lifecycle:1.4.2")
            implementation("androidx.camera:camera-view:1.4.2")
            implementation("com.google.mlkit:barcode-scanning:17.3.0")
            implementation("com.squareup.okhttp3:okhttp:4.12.0")
            implementation("androidx.webkit:webkit:1.13.0")
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
        versionCode = 1
        versionName = "1.0"
    }
    sourceSets["main"].manifest.srcFile("src/androidMain/AndroidManifest.xml")
    sourceSets["main"].assets.srcDirs("src/androidMain/assets")
    sourceSets["main"].res.srcDirs("src/androidMain/res")
    compileOptions { sourceCompatibility = JavaVersion.VERSION_17; targetCompatibility = JavaVersion.VERSION_17 }
    buildFeatures { buildConfig = true }
    buildTypes {
        getByName("debug") { manifestPlaceholders["cleartextAllowed"] = "true" }
        getByName("release") { manifestPlaceholders["cleartextAllowed"] = "false"; isMinifyEnabled = false }
    }
}
