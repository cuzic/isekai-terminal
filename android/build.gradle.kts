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

    // リポジトリに固定debug keystoreをコミットし、ローカルビルド・CI(GitHub Actions)
    // 間で常に同じ鍵で署名させる。AGPが既定で使う`~/.android/debug.keystore`は
    // マシン/CI runnerごとに初回ビルド時に自動生成される別々の鍵であり、CIを回すたびに
    // 毎回違う署名になって`adb install -r`が`INSTALL_FAILED_UPDATE_INCOMPATIBLE`で失敗し、
    // 実機の既存インストール(保存済みプロファイル・鍵を含むアプリデータ)を毎回
    // アンインストールする羽目になっていた(2026-07-27、実機検証で発覚)。debug専用の
    // 使い捨て鍵なので秘匿する必要はない(Android公式サンプルと同じ運用方針)。
    signingConfigs {
        getByName("debug") {
            storeFile = file("keystore/debug.keystore")
            storePassword = "android"
            keyAlias = "androiddebugkey"
            keyPassword = "android"
        }
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
    description = "Cross-compiles the Rust isekai-terminal-core native library for arm64-v8a via cargo/NDK."
    workingDir = rustCoreDir
    commandLine("cargo", "build", "--release", "--target", "aarch64-linux-android", "-p", "isekai-terminal-core")
    // isekai-terminal-core(rust-core/src)は同じcargoワークスペースの他クレート
    // (isekai-protocol/isekai-transport等、rust-core直下に23クレート)にも依存する。
    // 以前は`rust-core/src`だけをinputsに宣言していたため、それら下位クレートだけを
    // 変更してもGradleがこのExecタスクをUP-TO-DATEと誤判定し、古い.soがAPKへ
    // 混入し得た(2026-07-28、書籍原稿の実地検証で発覚)。target/やcargo-mutantsの
    // 出力ディレクトリ、ワークスペース非メンバーのnoq-multipath-spikeは除外する。
    inputs.files(
        fileTree(rustCoreDir) {
            exclude("target/**", "mutants.out/**", "mutants.out.old/**", "noq-multipath-spike/**")
        }
    ).withPropertyName("rustCoreWorkspaceSources").withPathSensitivity(PathSensitivity.RELATIVE)
    // isekai_pipe_quic_transport.rsが`include_bytes!`で埋め込むmusl静的isekai-pipe
    // バイナリ(scripts/build-isekai-pipe-musl.shが別途生成、CLAUDE.md参照)。
    // target/配下だがワークスペースソースの変更では追跡できないため個別に指定する。
    // ローカルでmuslビルドを未実行の環境(このリポジトリのCI以外の開発環境等)では
    // 存在しないこともあるためoptionalにする。
    inputs.files(
        rustCoreDir.resolve("target/x86_64-unknown-linux-musl/release/isekai-pipe"),
        rustCoreDir.resolve("target/aarch64-unknown-linux-musl/release/isekai-pipe"),
    ).withPropertyName("muslIsekaiPipeBinaries").withPathSensitivity(PathSensitivity.RELATIVE).optional()
    outputs.file(rustCoreDir.resolve("target/aarch64-linux-android/release/libisekai_terminal_core.so"))
}

val copyRustCoreJniLibs = tasks.register<Copy>("copyRustCoreJniLibs") {
    description = "Copies the cross-compiled isekai-terminal-core .so into a jniLibs source dir."
    dependsOn(cargoBuildRustCore)
    from(rustCoreDir.resolve("target/aarch64-linux-android/release/libisekai_terminal_core.so"))
    into(layout.buildDirectory.dir("rustJniLibs/arm64-v8a"))
}

// `preBuild`に直接dependsOnせず、jniLibsのsourceSetとして登録する: JVMのみで完結する
// testDebugUnitTest(Robolectric)実行のたびに無関係なNDKクロスビルドが走らないように
// するため(2026-07-28、書籍原稿の実地検証で発覚した内側ループの遅さの反面教師に
// 対する修正)。
//
// ただし`jniLibs.srcDir(...)`の登録**だけ**では、AGPが実際にパッケージング系タスク
// (`merge<Variant>JniLibFolders`)からこのタスクへの依存を自動的に汲み取ってくれる
// という当初の想定(Opus・Codexのセカンドオピニオンを踏まえたもの)が誤りだった
// (2026-08、実機インストールで発覚: `mergeDebugJniLibFolders`が`copyRustCoreJniLibs`
// より一切依存せずに実行され、.soを一切含まない`android-debug.apk`が生成されて
// `UnsatisfiedLinkError: library "libisekai_terminal_core.so" not found`で
// 起動直後にクラッシュしていた——ユニットテストはネイティブライブラリを必要としない
// ため、この非同期漏れはCIのどのテストにも引っかからず、実際にAPKを実機へ
// インストールして起動するまで誰も気づかなかった)。`merge*JniLibFolders`という
// 名前のタスク(バリアントごとに動的に生成される)へ明示的に`dependsOn`することで、
// srcDir登録だけに頼らず確実に依存させる。
android.sourceSets.getByName("main").jniLibs.srcDir(copyRustCoreJniLibs)
tasks.matching { it.name.startsWith("merge") && it.name.endsWith("JniLibFolders") }.configureEach {
    dependsOn(copyRustCoreJniLibs)
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.ui)
    implementation(libs.androidx.ui.tooling.preview)
    implementation(libs.androidx.material3)
    // タスク#17(ファイルプレビュー機能): ディレクトリブラウザのフォルダ/ファイルアイコン
    // (`Folder`/`Description`)がmaterial-icons-core(既定で入っている少数の定番アイコンのみ)
    // に無いため追加。バージョンはandroidx-compose-bomが管理する。
    implementation(libs.androidx.material.icons.extended)
    implementation(libs.kmp.terminal.input)
    implementation("net.java.dev.jna:jna:5.14.0@aar")
    implementation(libs.room.runtime)
    implementation(libs.room.ktx)
    ksp(libs.room.compiler)
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.9.1")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.9.1")
    // アプリ全体(プロセス)のフォアグラウンド/バックグラウンド遷移をSessionOrchestratorへ
    // 転送するための ProcessLifecycleOwner(AndroidAppExecutor.registerLifecycleCallbacks参照)。
    implementation("androidx.lifecycle:lifecycle-process:2.9.1")
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
