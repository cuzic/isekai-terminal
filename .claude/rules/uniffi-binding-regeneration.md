# ローカルビルド禁止下でのUniFFIバインディング再生成手順

Rust側のpublic API(UniFFI公開シグネチャ)を変更した場合、Kotlin/Swiftバインディング
の再生成が必要(CLAUDE.mdの「ビルド・テスト」セクション参照)。しかしこのプロジェクト
は`prefer-gh-actions-over-local-cargo`の方針でローカルbuild/testが禁止されている
ため、通常の`cargo run -p uniffi-bindgen -- generate ...`をローカルで実行できない。

## ルール

1. `regenerate-uniffi-bindings.yml`(workflow_dispatch専用CI)をトリガーする:
   ```bash
   gh workflow run regenerate-uniffi-bindings.yml --ref <branch>
   ```
2. 完了を待ち、`gh run list --workflow=regenerate-uniffi-bindings.yml --branch <branch>`
   等でrun IDを確認する。
3. artifactをダウンロードする:
   ```bash
   gh run download <run-id> -D <destination-dir>
   ```
   (`gh run download`はgitリポジトリのコンテキストを必要とする。カレントディレクトリ
   がgitリポジトリ外だと失敗するので、`-D`で明示的な出力先を指定しつつ実行自体は
   リポジトリ内で行う。)
4. **本体ファイルだけでなく`.sha256`サイドカーファイルも必ず一緒にコピーする**:
   - Kotlin: `android/src/main/kotlin/uniffi/isekai_terminal_core/isekai_terminal_core.kt`
     (1ファイル、sha256サイドカーなし)
   - Swift: `ios/Sources/IsekaiTerminalCoreLogic/generated/`配下の
     `isekai_terminal_core.swift`・`isekai_terminal_coreFFI.h`・
     `isekai_terminal_coreFFI.modulemap`の3ファイル**と、対応する`.sha256`
     ファイル3つ**(`rust-core/scripts/generate-swift-bindings.sh`が生成物ごとに
     `.sha256`サイドカーを作る設計になっている)。
5. コピー後、`diff`で差分が意図した変更(削除/追加したメソッド分など)のみである
   ことを確認してからコミットする。

## 理由

`ios-logic-linux-check.yml`/`ios-rust-core-check.yml`のdrift-checkは、
`generate-swift-bindings.sh`(または`build-linux-swift-ffi.sh`経由でそれを呼ぶ形)
をCI上で実際に再実行し、`git diff --exit-code -- ios/Sources/IsekaiTerminalCoreLogic/generated`
(および`IsekaiTerminalCoreFFILinux`)で比較する方式。このディレクトリには`.sha256`
ファイルも含まれるため、本体ファイルだけ更新して`.sha256`を古いままコミットすると、
このdiffが非ゼロになりCIが「stale」判定で落ちる。

2026-08-09のセッションで実際にこれが原因で`build-and-test`ジョブが2つ失敗した
(本体ファイルの内容自体は正しかったにもかかわらず)。

## 参照実装

- `.github/workflows/regenerate-uniffi-bindings.yml`
- `rust-core/scripts/generate-swift-bindings.sh`(`.sha256`サイドカー生成箇所)
- `.github/workflows/ios-logic-linux-check.yml` /
  `.github/workflows/ios-rust-core-check.yml`(drift-check本体)
