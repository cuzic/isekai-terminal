---
name: android-ci-deploy
description: android/のAPKをGitHub Actions(build-android.yml)でビルドし、成果物をダウンロードして、android-adb-remoteスキル経由でWindows PC越しの実機にインストール・起動する。既定はGitHub-hosted runner、より重い/速いビルドが要るときはcargo-ci-gcp-spot-instanceのGCP Spot VMをephemeral self-hosted runnerとして使う選択肢もある。ローカルサンドボックスでビルドせず「トリガー→待機→ダウンロード→インストール」を一気通貫で行う。
argument-hint: "[build|deploy|build-and-deploy]"
keywords:
  - android
  - github actions
  - apk
  - デプロイ
  - 実機インストール
  - workflow_dispatch
triggers:
  - androidをビルドしてデプロイ
  - 実機で試したい
  - APKを実機に入れて
  - build-android
allowed-tools: Bash
---

# GitHub Actions ビルド → 実機デプロイ

## 背景

`android/`のビルド(Rust NDKクロスビルド込み)をこのサンドボックスでローカル実行すると、
他の並行エージェントとのCPU競合で実用速度が出ないことが多い(2026-07-24に自前のGCP
Spot VM+Tailscaleで一度解決を試みたが、Opusレビューと調査の結果「過剰設計」と判断し
撤回した——GCP Spot VMは`cargo-ci-gcp-spot-instance`スキルとして残っているが、
cargo-mutantsの全走査のような真に重いジョブ専用のbreak-glassに位置づけを変えた)。

代わりに、iOSが既存の`macos-26`ランナーを使っているのと同じ発想で、Android側も
GitHub-hosted runner(public repoなので無料・Android SDK/NDKプリインストール済み)へ
ビルドを丸投げする。「ビルド待ち」の面倒さを解消するため、トリガー・待機・ダウンロード・
実機インストールまでをこのスキル1つで完結させる。

## 前提

- `gh` CLIが認証済み(`gh auth status`)。
- 実機デプロイまで行う場合は`android-adb-remote`スキルの前提(Windows PC経由の
  リモートadb)が整っていること。ビルドだけならこの前提は不要。

## 手順

### 1. ビルドをトリガーする

```bash
gh workflow run build-android.yml
```

単体テストも一緒に回したい場合(通常は不要、実機確認が目的なら省略してよい):

```bash
gh workflow run build-android.yml -f build_type=true
```

**既定はGitHub-hosted runner(無料・無制限)。** より速い/より重いビルドが必要な場合のみ、
事前に`cargo-ci-gcp-spot-instance`スキルでGCP Spot VMをephemeral self-hosted runnerとして
登録した上で(そのスキルのステップ9参照)、以下で明示的に指定する:

```bash
gh workflow run build-android.yml -f runner_type=self-hosted-gcp-spot
```

runnerが未登録の状態でこれを実行すると、ジョブは`self-hosted`ラベルのrunnerが
現れるまで無期限にキュー待ちになる(失敗はしない)ので注意。まずrunnerを登録してから
ワークフローを起動すること。

### 2. 完了を待つ

`gh workflow run`は実行IDを返さないので、直後の一覧から自分の実行を拾う:

```bash
sleep 5   # runが一覧に現れるまでの猶予
RUN_ID=$(gh run list --workflow=build-android.yml --limit 1 --json databaseId --jq '.[0].databaseId')
gh run watch "$RUN_ID" --exit-status
```

`--exit-status`を付けることで、ビルド失敗時はこのコマンド自体が非ゼロ終了するので
そのまま検知できる。実行時間の目安は5分程度(2026-07-24にGCP Spot VM実測で
5分23秒、GitHub-hosted runnerでもキャッシュヒット後は同程度かそれ以下を想定)。

### 3. APKをダウンロードする

```bash
gh run download "$RUN_ID" --name android-debug-apk --dir /tmp/android-ci-deploy
APK_PATH=$(find /tmp/android-ci-deploy -name "*.apk" | head -1)
echo "$APK_PATH"
```

### 4. 実機にインストール・起動する

ここから先は`android-adb-remote`スキルの手順(手順2〜5)をそのまま使う。
`ADB_SERVER_SOCKET`の値(WindowsのTailscale IP)は`tailscale status`で確認済みの前提:

```bash
timeout 60 env ADB_SERVER_SOCKET=tcp:<WindowsのTailscale IP>:5037 adb install -r "$APK_PATH"
timeout 15 env ADB_SERVER_SOCKET=tcp:<WindowsのTailscale IP>:5037 adb shell am start -n tools.isekai.terminal/.MainActivity
timeout 15 env ADB_SERVER_SOCKET=tcp:<WindowsのTailscale IP>:5037 adb logcat -d -v time "AndroidRuntime:E" "*:S" | tail -40
```

`installDebug`(Gradle経由のインストール)ではなく`adb install`を直接使う理由は
`android-adb-remote`スキルの「4. Gradleでビルドする場合の注意」と同じ
(AGPの端末検出が`ADB_SERVER_SOCKET`を拾わないため)。

## ビルド失敗時

`gh run view "$RUN_ID" --log-failed`でログを取得する。よくある失敗要因:

- musl版`isekai-pipe`の埋め込みバイナリ不足(`couldn't read .../isekai-pipe: No such
  file or directory`) → ワークフロー内の`build-isekai-pipe-musl.sh`ステップが
  失敗していないか確認(zig/cargo-zigbuildのインストールが絡む)。
- NDKバージョン不一致 → `rust-core/scripts/ndk-common.sh`は`$ANDROID_HOME/ndk`配下の
  最新版を自動選択するので、通常は気にしなくてよい。

## 参照

- ビルド本体: `.github/workflows/build-android.yml`
- 実機デプロイの詳細手順: `android-adb-remote`スキル(`~/.claude/skills/android-adb-remote/`)
- 真に重いジョブ(cargo-mutants全走査等)向けのbreak-glass: `cargo-ci-gcp-spot-instance`スキル
