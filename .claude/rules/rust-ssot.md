---
description: Rust(rust-core)をセッション/接続状態のSSOTとし、Kotlin側は薄いイベント転送層に留める設計原則
---

# Rust を SSOT にする

このプロジェクト(Kotlin/Compose の UI 層 + Rust/UniFFI の rust-core)では、
**セッション・接続・トランスポートに関する状態と、その状態に基づく意思決定ロジックは
必ず Rust(rust-core)側に置く**。Kotlin 側にミラー状態を作って、その上で分岐判断をしてはいけない。

## ルール

- 「今どのフェーズ/状態か」を見て挙動を変えるロジック(接続中か・切断済みか・どの
  transport 経由か、等の状態機械)は Rust 側(`rust-core/src/orchestrator.rs` の
  `SessionOrchestrator` 等)に実装する。Kotlin 側で同種の状態を複製して判断してはいけない。
- Kotlin(ViewModel / `TerminalSession` などのラッパー層)がやってよいのは次の2つだけ:
  1. OS/プラットフォームからの生イベント(ネットワーク断・復帰、ライフサイクルイベント、
     ユーザー操作など)を UniFFI 経由でそのまま Rust に転送する。
  2. Rust からのコールバック(`OrchestratorCallback` 等)を受けて UI に反映する。
- 新しいプラットフォームイベント(例: 新しい `ConnectivityManager` コールバック、新しい
  Android ライフサイクルシグナル)を追加するときは、Kotlin 側にフラグや分岐を積み上げず、
  生イベントをそのまま Rust に渡す新しい UniFFI メソッドを追加する方向で設計する。
- 既存コードに歴史的経緯でミラー状態が残っている箇所を触るときは、そのついでに判断ロジックを
  Rust 側へ寄せることを検討する(Phase 8-4d で実際にそうした)。
- 例外: スクロール位置・ダイアログの開閉・入力途中のテキスト・フォントサイズ設定など、
  **UI 表示だけに閉じた状態**は Kotlin/Compose 側に置いて構わない。この原則が対象にしている
  のはセッション/プロトコル/接続の状態であり、UI 表示状態ではない。

## 理由

Rust 側は SSH/QUIC と実際に通信し、トランスポートの実際のフェーズを JNI/UniFFI 境界越しの
遅延やコピーなしに知っている唯一のレイヤーである。Kotlin 側に判断ロジックを置くたびに、
Rust の状態と食い違い得る「もう1つの状態のコピー」が増える。これは抽象的な懸念ではなく、
実際にこのプロジェクトで一度発生し、発見されるまで気づかれなかった不具合である(下記参照)。

## 実例: `notifyNetworkLost()` の修正(Phase 8-4d)

- **問題**: `TerminalSession.kt` の docstring には元々「セッション状態の SSOT は Rust 側に持つ」
  と書いてあったにもかかわらず、`notifyNetworkLost()`(ハンドシェイク中/TCP接続中は切断、
  QUIC接続中は無視、という判断)は Kotlin 側のミラー状態(`_state`)を見て判断していた。
- **修正**: `rust-core/src/orchestrator.rs` の `SessionOrchestrator` に `ConnPhase`
  (`Idle`/`Connecting`/`Connected`)を追加し、`SessionOrchestrator::notify_network_lost()`
  として判断ロジックを Rust 側に一元化した。
- **結果**: `android/src/main/kotlin/tools/isekai/terminal/session/TerminalSession.kt` の
  `notifyNetworkLost()` は次の1行に縮小された:

  ```kotlin
  fun notifyNetworkLost() = orchestrator.notifyNetworkLost()
  ```

  判断結果は既存の `onConnectionStateChanged` コールバック経由でそのまま UI に反映される。

## 参照実装

- `rust-core/src/orchestrator.rs`: `SessionOrchestrator` / `ConnPhase`
- `android/src/main/kotlin/tools/isekai/terminal/session/TerminalSession.kt`: `notifyNetworkLost()`
- `android/src/main/kotlin/tools/isekai/terminal/TerminalViewModel.kt`: `notifyNetworkLost()` の呼び出し元
