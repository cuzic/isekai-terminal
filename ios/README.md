# isekai-terminal iOS版(Phase 0: 技術検証スパイク)

このディレクトリは、`rust-core`(crate `isekai-terminal-core`)を Rust コアとして共有する iOS 版アプリの
Swift Package Manager パッケージ雛形です。背景・設計方針は `PLAN.md` の「Phase Y: iOS対応
(Rust + Swift)」節を参照してください。

## Linux上でのロジック層テスト(`swift test`、macOS/Xcode不要)

`Sources/IsekaiTerminalCoreLogic`(UIKit/SwiftUI/GRDB/Keychainに依存しない純粋なビジネスロジック層)
は、macOS/Xcodeが無くてもLinux上でネイティブに`swift test`できます(詳細は`PLAN.md`
「Phase Y」節の「iOS Linux CI: IsekaiTerminalCoreLogicの切り出し」参照)。

```bash
# 初回のみ: rust-core/scripts/build-linux-swift-ffi.sh がRustコアをLinuxネイティブビルドし、
# UniFFI Swiftバインディングを生成し、Linux向けFFIリンク設定(Sources/IsekaiTerminalCoreFFILinux)を用意する。
bash rust-core/scripts/build-linux-swift-ffi.sh

cd ios
export LIBRARY_PATH="$(pwd)/Frameworks/linux:${LIBRARY_PATH:-}"
export LD_LIBRARY_PATH="$(pwd)/Frameworks/linux:${LD_LIBRARY_PATH:-}"
swift test
```

`ios/Package.swift`はホストOSが Linux の場合、マニフェスト自体から `IsekaiTerminalCore`/`IsekaiTerminalCoreTests`
(Apple専用、UIKit/GRDB/Keychainに依存)を除外するため、上記コマンドは`IsekaiTerminalCoreLogic`/
`IsekaiTerminalCoreLogicTests`だけを解決・実行します。実iOSアプリ全体のビルド・シミュレータ実行は
引き続き下記の macOS 手順(または `.github/workflows/ios-rust-core-check.yml` /
`ios-app-build-check.yml`)が必要です。CIでは `.github/workflows/ios-logic-linux-check.yml`
(`ubuntu-24.04`ランナー)がこのレーンを担当します。

## 現状(Phase 0 の到達点)

- **Swiftバインディング生成は Linux 開発機で検証済み**(`rust-core/scripts/generate-swift-bindings.sh`)。
  `ios/Sources/IsekaiTerminalCore/generated/` に生成済みのファイルをコミットしています。
  `OrchestratorCallback`(9メソッド)・`SessionCallback`・`SshError` を含む全エクスポート面が
  問題なく Swift の `protocol`/`enum: Swift.Error` として生成されることを確認済みです。
- **iOSクロスコンパイル・XCFramework化・シミュレータでの動作確認は、`.github/workflows/
  ios-rust-core-check.yml`(GitHub Actions、`macos-26`ランナー)で2026-07-04に実際にgreenを
  確認済みです**。このリポジトリは公開リポジトリのため、GitHub-hostedのmacOSランナーは
  無課金で使えます(詳細は`PLAN.md`「Phase Y」節のPhase 0-6参照)。ローカルのmacOS環境がある
  場合は、以下の手順で同じ内容を手元でも実行できます。

## 前提条件(macOS側)

- Xcode(バージョンは特にこだわりません。`-create-xcframework` は Xcode 11+、SwiftPM の
  local path binaryTarget は Xcode 12+ で動作するはずです)
- Rust ツールチェーン(`rustup`)。`rust-toolchain.toml` は本リポジトリに存在しないため、
  手元の安定版 rustc で構いません(問題が出た場合のみバージョン固定を検討してください)。

## 手順

### 1. iOS向けRustターゲットを追加

```bash
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
```

### 2. クロスコンパイル + XCFramework化

先に `isekai-pipe` の x86_64/aarch64 musl静的バイナリをビルドしておく必要があります
(`isekai-terminal-core`が`include_bytes!`で埋め込むため。iOS固有ではなく`isekai-terminal-core`をビルドする際の
一般的な前提です):

```bash
brew install zig
cargo install cargo-zigbuild --locked
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
bash rust-core/scripts/build-isekai-pipe-musl.sh
```

```bash
bash rust-core/scripts/build-ios-xcframework.sh
```

内部で以下を行います(詳細はスクリプト本体を参照):
1. 3ターゲット(`aarch64-apple-ios` / `aarch64-apple-ios-sim` / `x86_64-apple-ios`)を
   `cargo build --release -p isekai-terminal-core` でビルド(`crate-type = ["cdylib", "staticlib"]` により
   生成される `.a` のうち **staticlib のみ使用**)。
2. シミュレータ向け2アーキテクチャ(`aarch64-apple-ios-sim` + `x86_64-apple-ios`)を
   `lipo -create` でfat化(XCFrameworkは1プラットフォームバリアントにつき1ライブラリの
   制約があるため必須)。
3. `ios/Sources/IsekaiTerminalCore/generated/isekai_terminal_coreFFI.modulemap` を `module.modulemap` という
   名前でコピーしたヘッダーディレクトリを用意し(XCFrameworkの `-headers` はこの
   ファイル名を期待するため。`module` 宣言自体は `isekai_terminal_coreFFI` のまま変更しない)、
   `xcodebuild -create-xcframework` で `ios/Frameworks/IsekaiTerminalCoreFFI.xcframework` を生成。

もし `ring`/`rustls`/`noq` のビルドで SDK 解決に失敗する場合は、シミュレータ向けビルド前に
以下を試してください:

```bash
export SDKROOT="$(xcrun --sdk iphonesimulator --show-sdk-path)"
```

### 3. Swiftバインディングの再生成(Rust側を変更した場合のみ)

```bash
bash rust-core/scripts/generate-swift-bindings.sh
```

Linux/macOS どちらでも実行できます(ホストのデフォルトターゲットでビルドした cdylib から
メタデータを読むだけのため、iOSクロスコンパイル環境は不要です)。

### 4. round-trip検証(最小テスト)

```bash
cd ios
xcrun simctl list devices available   # 使えるシミュレータ名を確認
xcodebuild test -scheme IsekaiTerminalCore-Package -destination 'platform=iOS Simulator,name=iPhone 15'
```

`-scheme IsekaiTerminalCore-Package` は `Package.swift` から自動生成される、全ターゲット(`IsekaiTerminalCore`/
`IsekaiTerminalCoreLogic`とそれぞれのテストターゲット)を束ねるスキームです。`IsekaiTerminalCoreLogic`分離
(Phase Y、`IsekaiTerminalCoreLogic`分離の設計判断の節参照)以降、単体の`IsekaiTerminalCore`スキームには
テストアクションが構成されなくなったため、これを使う必要があります。CI(`macos-26`)では
そのまま動作することを確認済みです。

成功基準: `CoreVersionRoundTripTests.testCoreVersionMatchesCargoPackageVersion` が green になり、
`coreVersion()` の戻り値が `rust-core/Cargo.toml` の `[package] version` と一致すること
(CIで実際にpass済み)。

## Phase 0 でやらないこと

- 単一tokioランタイム(`LazyLock<Runtime>`、`rust-core/src/lib.rs`)とiOSバックグラウンド
  サスペンド/jetsamの実機検証(`PLAN.md` Phase 0-5 参照。実機が無いため次フェーズに送る)。
- `prepare_for_background`/`resume_from_background` 等のライフサイクルAPI実装(Phase 1以降)。
