# isekai-terminal

Android 単体で完結する SSH クライアント。Kotlin(Jetpack Compose)の UI 層と、
Rust(UniFFI 経由)の `rust-core`(crate 名 `isekai-terminal-core`)からなる。

日本語 IME 完全対応・trzsz ファイル転送・自作ヘルパー(`isekai-pipe serve`)経由の QUIC
接続耐性(ローミング / 完全切断からの resume / Tailscale⇔直接アドレスのマルチパス)が
差別化ポイント。詳細な設計判断・実装フェーズごとの経緯は `PLAN.md` に全て記録されている
(実装前に必ず目を通すこと。特に「対象外」と明記された過去の判断を覆す変更をする場合は
その経緯を踏まえること)。

## ディレクトリ構成

- `android/` — Kotlin/Compose UI 層(`tools.isekai.terminal` パッケージ)。
  `android/src/main` 本体、`android/src/debug` は debug ビルドのみ含まれるデバッグ専用コード
  (実機フォルト注入レシーバー等)、`android/src/test`(Robolectric/JVM)、
  `android/src/androidTest`(実機/エミュレータ)。
- `rust-core/` — Cargo workspace。
  - `src/`(crate `isekai-terminal-core`, cdylib名 `isekai_terminal_core`): SSH(russh)・VT100/VTEパーサー・
    trzsz転送FSM・QUIC transport・resume/multipath ロジック。UniFFI で Kotlin に公開。
  - `isekai-pipe/`: データプレーン CLI バイナリ。`connect`(client)/`serve`(server)を同一
    バイナリで提供する。`src/engine/`(旧 `isekai-helper` crate、2026-07-07 統合)がQUIC↔TCP
    中継の実体。`serve`はmusl staticビルド(`scripts/build-isekai-pipe-musl.sh`)されて
    `isekai-terminal-core`に埋め込まれ、Android側のリモートブートストラップでも配布・起動される。
  - `isekai-ssh/`: `ssh(1)` の `ProxyCommand` に差し込む単体 CLI バイナリ。`isekai-terminal-core`とは独立
    (`isekai-protocol`/`isekai-trust`/`isekai-auth`/`isekai-transport`/`isekai-bootstrap`/
    `isekai-bootstrap-plan`から構成)で、`isekai-pipe`経由のQUIC接続耐性を
    Androidアプリ以外の`ssh`からも使えるようにする。`isekai-bootstrap-plan`はroute種別
    (direct/STUN/relay)×hop構成(0/1/multi-hop)を横断するI/OなしのBootstrapPlan/BootstrapFailure
    共通層(`ISEKAI_PIPE_DESIGN.md` §8 Epic A)。リリース成果物の署名検証は実装したうえで
    恒久的に不要と判断し撤去済み(sha256サイドカー照合のみ、§8 Epic D)。
    利用者向けガイドは`rust-core/isekai-ssh/README.md`、設計は`ISEKAI_PIPE_DESIGN.md`参照。
  - `uniffi-bindgen/`: Kotlin バインディング(`android/src/main/kotlin/uniffi/isekai_terminal_core/isekai_terminal_core.kt`)
    生成用。
  - `noq-multipath-spike/`: `noq`(quinn の multipath フォーク)の実機検証用の使い捨てコード。
- `PLAN.md` — 実装計画と各 Phase(0〜9)の設計・実機検証結果の記録。最新の設計判断のSSOT。
- `DESIGN.md` — 初期スコープ定義(一部の「やらないこと」は後の Phase で方針転換済みなので
  `PLAN.md` の該当 Phase と食い違う場合は `PLAN.md` を優先する)。
- `ISEKAI_PIPE_DESIGN.md` — `isekai-ssh`/`isekai-pipe`(CLI)の現行設計(2026-07-07更新)。
  実装前に必ず目を通すこと。
- `archive/` — `isekai-ssh`/`isekai-pipe`の過去の設計検討過程(`chatgpt.md`・
  `ISEKAI_SSH_DESIGN.md`・`HELPER_PROTOCOL.md`・`ISEKAI_PIPE_MIGRATION.md`)。現行の設計は
  `ISEKAI_PIPE_DESIGN.md`を参照し、これらは経緯記録としてのみ参照する。
- `TESTING.md` — 実機での手動動作確認手順。
- `SSH3_PROTOCOL_NOTES.md` — SSH3/HTTP3 化の調査記録(不採用、記録として保持)。

## ビルド・テスト

```bash
# Kotlin/Android
./gradlew testDebugUnitTest       # JVM/Robolectric ユニットテスト
./gradlew installDebug            # 実機/エミュレータへインストール

# Rust
cd rust-core
cargo test -p isekai-terminal-core --lib     # コア(SSH/VTE/trzsz/resume/multipath)のユニット・e2eテスト
cargo test -p isekai-pipe         # isekai-pipe(connect/serve)のユニット・e2eテスト
cargo run -p uniffi-bindgen -- generate --library target/debug/libisekai_terminal_core.so --language kotlin
                                   # Rust の public API を変更したら Kotlin バインディング再生成が必須
```

## 設計原則

- **Rust を SSOT にする**: セッション/接続/トランスポートの状態と、それに基づく意思決定は
  Rust(`rust-core`)側に置く。Kotlin 側にミラー状態を作って分岐判断しない。
  詳細と実例は `.claude/rules/rust-ssot.md` を参照(このルールはセッション開始時に自動読込される)。
- 実験的・opt-in の機能(マルチパス、物理 Wi-Fi/セルラー同時利用など)は既定 OFF とし、
  使えない環境では黙ってフォールバックする「日和見的(opportunistic)」設計にする
  (`PLAN.md` Phase 7-7/9 の設計判断を参照)。
- **Room migration(`AppDatabase.kt`)は勝手に次の番号を使わず、先に予約する**: 複数の並行
  worktree/エージェントが同時に新しいマイグレーションを追加すると番号を奪い合い、後から
  再採番する fixup コミットが必要になる(実際に複数回発生済み)。新しい migration を書く前に
  必ず `scripts/reserve-room-migration.sh <owner-slug>` を実行してバージョン番号を予約すること
  (詳細は `android/migration_registry.toml` 参照)。CI(`room-migration-check.yml`)が
  `AppDatabase.kt` の版数と migration チェーンの整合性を検証する。

## コミット規約

`git log --oneline` に従う: `<type>: <日本語での説明>(該当する場合は「（Phase X-Y）」を付す)`。
例: `feat: TransportPreferenceとHelperQuicSessionをActiveSessionに統合（Phase 7-4）`。
大きな機能はまとまった1コミットにせず、実際に組み上がった順序が追えるよう細かく分ける。
