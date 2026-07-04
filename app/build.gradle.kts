plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.ksp)
    alias(libs.plugins.roborazzi)
    id("kotlin-parcelize")
}

android {
    namespace = "tools.isekai.terminal"
    compileSdk = 36

    defaultConfig {
        applicationId = "tools.isekai.terminal"
        minSdk = 28
        targetSdk = 36
        versionCode = 1
        versionName = "1.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        // Phase 9-4(物理Wi-Fi/セルラー同時マルチパス)は noq issue #738
        // (https://github.com/n0-computer/noq/issues/738、Needs Triage)により現状常に
        // no-op(黙って直接アドレスのみのマルチパスへフォールバック)。一般ユーザー向けの
        // リリースビルドでは非表示にし、開発・実機検証用のdebugビルドでのみ見せる
        // (外部レビュー指摘対応、PLAN.md Phase 10完了後の外部レビューP1参照)。
        buildConfigField("boolean", "ENABLE_EXPERIMENTAL_PHYSICAL_MULTIPATH", "true")
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
            buildConfigField("boolean", "ENABLE_EXPERIMENTAL_PHYSICAL_MULTIPATH", "false")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlin {
        compilerOptions {
            jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17)
        }
    }
    buildFeatures {
        compose = true
        buildConfig = true
    }
    testOptions {
        unitTests {
            isReturnDefaultValues = true
            isIncludeAndroidResources = true
            all {
                // Roborazzi のCompose UIスクリーンショットにハードウェアレンダリングを使う
                it.systemProperties["robolectric.pixelCopyRenderMode"] = "hardware"
            }
        }
    }
}

val rustCoreDir = rootProject.file("rust-core")

val cargoBuildRustCore = tasks.register<Exec>("cargoBuildRustCore") {
    description = "Cross-compiles the Rust tssh-core native library for arm64-v8a via cargo/NDK."
    workingDir = rustCoreDir
    commandLine("cargo", "build", "--release", "--target", "aarch64-linux-android", "-p", "tssh-core")
    inputs.dir(rustCoreDir.resolve("src"))
    inputs.file(rustCoreDir.resolve("Cargo.toml"))
    inputs.file(rustCoreDir.resolve("Cargo.lock"))
    inputs.dir(rustCoreDir.resolve(".cargo"))
    outputs.file(rustCoreDir.resolve("target/aarch64-linux-android/release/libtssh_core.so"))
}

val copyRustCoreJniLibs = tasks.register<Copy>("copyRustCoreJniLibs") {
    description = "Copies the cross-compiled tssh-core .so into jniLibs/arm64-v8a."
    dependsOn(cargoBuildRustCore)
    from(rustCoreDir.resolve("target/aarch64-linux-android/release/libtssh_core.so"))
    into("src/main/jniLibs/arm64-v8a")
}

tasks.matching { it.name == "preBuild" }.configureEach {
    dependsOn(copyRustCoreJniLibs)
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.ui)
    implementation(libs.androidx.ui.tooling.preview)
    implementation(libs.androidx.material3)
    implementation(libs.kmp.terminal.input)
    implementation("net.java.dev.jna:jna:5.14.0@aar")
    implementation(libs.room.runtime)
    implementation(libs.room.ktx)
    ksp(libs.room.compiler)
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.9.1")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.9.1")
    implementation("androidx.navigation:navigation-compose:2.9.0")
    debugImplementation(libs.androidx.ui.tooling)
    debugImplementation("androidx.compose.ui:ui-test-manifest")

    testImplementation("junit:junit:4.13.2")
    testImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.10.2")
    testImplementation("org.robolectric:robolectric:4.13")
    testImplementation("androidx.test:core:1.5.0")
    testImplementation("androidx.test.ext:junit:1.2.1")
    testImplementation("androidx.room:room-testing:2.7.1")
    testImplementation(platform(libs.androidx.compose.bom))
    testImplementation("androidx.compose.ui:ui-test-junit4")
    testImplementation("io.github.takahirom.roborazzi:roborazzi:${libs.versions.roborazzi.get()}")
    testImplementation("io.github.takahirom.roborazzi:roborazzi-compose:${libs.versions.roborazzi.get()}")

    androidTestImplementation("androidx.test.ext:junit:1.2.1")
    androidTestImplementation("androidx.test:runner:1.6.2")
    androidTestImplementation("androidx.test:rules:1.6.1")
    androidTestImplementation(platform(libs.androidx.compose.bom))
    androidTestImplementation("androidx.compose.ui:ui-test-junit4")
    androidTestImplementation("androidx.room:room-testing:2.7.1")
    androidTestImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.10.2")
}
