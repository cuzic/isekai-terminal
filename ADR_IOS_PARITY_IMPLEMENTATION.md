# ADR: Android→iOS 機能パリティの実装方針

- **Status**: **Accepted**（2026-08-21起草、round-3 改訂で収束。レビュアーが
  「no blocking issues remain / fold N1–N4 in and close the loop」と判定）
- **対象**: `ios/`（`IsekaiTerminalCore` アプリ本体 + `IsekaiTerminalCoreLogic` パッケージ）と
  `rust-core/`
- **入力ドキュメント**: `IOS_PARITY_GAP.md`（2026-08-21、現ソースツリーを実読して検証した
  gap分析。`PLAN.md`「Phase Y: iOS対応」節のgap記述を置き換えるもの）
- **拘束される既存ルール**: `CLAUDE.md`、`.claude/rules/rust-ssot.md`、
  `.claude/rules/uniffi-binding-regeneration.md`、`.claude/rules/prefer-gh-actions-over-local-cargo`
  （ローカルビルド/テスト全面禁止）、`.claude/rules/parallel-worktree-agent-operations.md`、
  `.claude/rules/main-branch-protection.md`

---

## 0. 改訂履歴

### Round 3（2026-08-21）— `ADR_REVIEW_ROUND2.md` への対応（最終）

round-2 レビューは **blocking ゼロ・B1〜B4 すべて resolved** と判定し、
S2 の反論も「アーキテクト側が正しい、こちらの事実誤認だった」と明示的に撤回された。
残る N1〜N4（いずれも significant、新設計の**仕様の空白**であって決定への異議ではない）と
minor 3件・Q9〜Q11 への回答を反映して収束させたのが本改訂である。

| 変更 | 由来 |
|---|---|
| `NotifyGenerationTracker` の**リセット意味論**を仕様化。(1) `@MainActor` 同期文脈でのリセット（非同期だと既存の bell-generation が文書化している競合を再導入する）、(2) 手動 `reconnect()` だけでなく **`Connected` 遷移時**にリセット（#10 により自動再接続が常態になるため）、(3) 世代の**減少**も防御的にリセット扱い | N1 / NP2 |
| `on_foreground_resume` の3つの未定義意味論を確定。(a) `background_state == Foreground` のときは**発火しない**、(b) `did_reconnect` は「開始した」であって「成功した」ではない → 文言を「再接続しました」→**「再接続しています」**へ、(c) タブ単位バナーとアプリ単位バナーの区別を明記 | N2 / NP4 |
| tmux claim の3つの未定義意味論を確定。(i) **同一 owner の再 claim は冪等（true を返す）**、(ii) **ensure RPC 失敗時に release する**（Android が既に踏んで直した箇所）、(iii) タブ close 以外の**あらゆる teardown 経路**で release、(iv) テスト用リセットフック。加えて「プロセス内 map なのでクラッシュを跨いだ stale claim は原理的に発生しない」を明記 | N3 / NP3 |
| **Y-R の破壊範囲を訂正**: `OrchestratorCallback` へのメソッド追加は Swift 側の**3つの適合**も壊す（うち1つは `IsekaiTerminalCoreLogicTests` = 最も安い第一ゲート）。D-6 の `FakeSshGateway.kt` 記述も誤りだったので訂正（参照を保持するだけで実装していない。Kotlin の唯一の実装者は `TerminalSession.kt:235` の匿名オブジェクト） | N4 |
| **Y-R を先頭へ移動**（Y-P0 の前または同時）。round-2 は §4.1 図が「Y-P1 と並行可」、§4.1-3/§4.2-5 が「Y-P2 の前に完了」「並列化しない」と**自己矛盾**していた。Y-P1 と並行させると、Y-R マージの瞬間に在庫の全 worktree がコンパイル不能になる | N4 / NP1 |
| **Y-P2 を3分割**: Y-P2（#10 単独）→ Y-P2b（#4b 経路B、スキーマ変更ゼロ・価値/工数比が最大）→ Y-P2c（#4a 経路A + GRDB v7）。**#4 を Y-P1 に混ぜない**（Y-P1 は「Rust変更ゼロ・スキーマ変更ゼロ」であることが並列化可能性の根拠） | Q11 |
| §3.6.1 を「レビュアーの CI 配線に関する事実主張が誤りだった」と「決定論的ゲートを選んだのは本リポジトリの負荷 flaky 履歴のため」の**2つの独立した記述**に分離（round-2 は後者をレビュー由来と読める書き方をしており、レビュアーから「それは私の意図ではない」と指摘された） | round-2 §0 |
| D-1 に「Swift 側 gate は名指しした2つで**打ち止め**、3つ目を足すには本ADRの改訂が要る」を追加 | m1 |
| #6 のスパイク帰結ラベルを `B-1/B-2/B-3` → **`Outcome-1/2/3`** へ改称（#1 のリリース段階ラベルと衝突していた。並列エージェントへタスク指示を出すときに「B-1 だけ実装する」が曖昧になる） | m2 |
| Q9（Android の claim を Rust へ寄せない）を**トリガー付き follow-up** として記録、Q10（Y-R では Android は no-op + log のまま）、Q1・Q2 は現状維持で確定 | Q9/Q10/Q1/Q2 |
| リスク表に NP1〜NP4 由来の4件を追加 | round-2 §8 |

### Round 2（2026-08-21）— `ADR_REVIEW_ROUND1.md` への対応

レビュアーによる敵対的レビュー（`ADR_REVIEW_ROUND1.md`、blocking 4件・significant 10件・
minor 3件・premortem 5シナリオ）を受けての全面改訂。**round-1 の最大の誤りは、
「Rust側の public API 追加は1つも必要ない」という headline conclusion が
自分の設計自身によって破られていたこと**（B3）である。round-2 ではこれを撤回し、
**新規 Rust API は正確に2本、1回のバッチで再生成する**という計画に置き換えた（D-6）。

主要な変更点:

| 変更 | 由来 |
|---|---|
| #4 を **4a（tmux hook経路）/ 4b（ctl `Notify`経路）** に分割。4b は `notify_generation` の世代差分を消費し、送出側の `title`/`body` を固定文言より優先する | B1 |
| D-1 を再定義。「セッション状態由来の抑制」だけが Rust 専管であり、**プロファイル単位の opt-in と OS 権限確認は Swift 側で正しい** | B2 |
| 復帰バナーのために Rust へ `on_foreground_resume(did_reconnect: bool)` を1本追加すると明記。「Rust変更ゼロ」の撤回 | B3 |
| #10 に **プロファイル単位の tmux ウィンドウ claim ガード**を追加。実装場所を **Rust** と決定（新規 Rust API 2本目） | B4 |
| Rust API は Y-R チェックポイントで**1回だけ**再生成。Kotlin バインディングと `android-uniffi-drift`（required check）への波及を明記 | S1, P2 |
| GRDB マイグレーション予約レジストリを Y-P0 で新設し、D-4 の直列化制約を撤廃 | S10-Q5 |
| `SettingsView` の抽出を Y-P0 の前提作業に格上げ | S7 |
| 実装順序を変更: **#10 と #4 を前倒し**（Y-P2）、#9 を Y-P3 へ | P5, P1 |
| 各項目に「どの CI ジョブが実際に実行するか / 手動確認か」を明記 | S2, S3, S4, P4 |
| §3.9.3 ブロック④ の文言が §3.10.2-③ と矛盾していた（force-quit でも復元される）ため差し替え | S5 |
| reattach レコードのタイムスタンプを Connected 遷移時にも更新 | S6 |
| #1 に requestId in-flight レジストリ・キャンセル・チャンク再結合を追加 | S8 |
| `PLAN.md` Phase Y のどの記述が既に古いかを明記し、Phase Y の更新を成果物に含めた | S9 |
| Q3（プロファイル単位を維持）・Q5（レジストリを作る）に回答済みとして反映 | S10 |
| #6 スパイクに `keyboardLANG1`/`keyboardInternational1` による**反応的検出**という第3の帰結を追加 | M2 |
| `TabRestoreRecord` に最終復元時刻/件数を追加 | M3 |

**レビュー指摘のうち1件（S2）には反論する**（§3.6.1）。`IsekaiTerminalCoreTests` は
`ios-rust-core-check.yml` の `xcodebuild test -scheme IsekaiTerminalCore-Package` で
実際に実行されている。ただし S2 が指し示していた実質的な懸念（性能ゲートが
共有 macOS ランナー上で不安定になること）は妥当なので、そちらは受け入れた。

---

## 1. Context

### 1.1 何を決めようとしているか

`IOS_PARITY_GAP.md` は、Android版で実装済みかつiOS版に未実装な機能を10項目に整理した。
本ADRは、その10項目を**どういう順序で・どこに（Rust/Swift/どのターゲット）・どういう形で**
実装するかを確定する。特に、gap分析が「単純移植不可・要設計」と明記した2項目
（#9 OEMバッテリー最適化案内UI、#10 マルチタブのバックグラウンド維持）については、
iOS向けの設計そのものを本ADRで提案する。

実装は行わない。本ADRは計画文書である。

### 1.2 実ソースで検証した事実（gap分析への補正を含む）

以下は**gap分析だけを読んでいると誤った計画を立てる**点である。★印は round-2 で
レビュー指摘により追加されたもの。

| # | gap分析（または round-1 ADR）の記述 | 実ソースで確認した事実 | 影響 |
|---|---|---|---|
| #2 | 「Rust/データ層の変更は不要」 | **正しい**。フォーム送信（`TerminalSession.kt:212-218`）は `org.json.JSONObject(values).toString()` + `send()` だけの Kotlin 純粋処理。`ai_panel.rs` は `pub(crate)` のみで何も export していない | 最短で価値が出る項目。Rust変更ゼロ |
| #4 | 「`UNUserNotificationCenter`への橋渡しを1箇所追加するだけ」 | **不正確**。iOSの `ConnectionProfile` に `enableTabNotifications` 相当の列が無い。`TmuxTabWindowCoordinator.swift:107-111` の `enableNotifications: Bool = false` は既定値のままで、唯一の呼び出し元（`TerminalSessionController.swift:761`）が何も渡していない | 通知UIを足すだけでは Rust がリモート tmux へフックを仕込まない |
| ★#4 | （round-1 が見落とし）**通知経路は2本ある** | **経路A（tmux hook）**: `OrchestratorCallback::on_notify(kind)`、Rust側で `notify_focus_change` による抑制と `(tmux_tag, seq)` 重複排除済み。**経路B（ctl `Notify`）**: `notify_generation` / `notify_kind` / `notify_title` / `notify_body`（`rust-core/src/lib.rs:1196-1206`）。Android は `TerminalTabsViewModel.kt:286-296` で世代差分を取り、`TabAlertNotifier.notify(..., message = title to body)` を呼ぶ。iOS 側は `TerminalScrollback.swift:47-50` が既に4フィールドをスナップショットへ写しているが**誰も消費していない** | **経路B は `isekai-pipe ctl notify` / claude-hookd が駆動する、このリポジトリで日常的に使われている経路**。round-1 の #4 設計は経路Aしか見ておらず、実装すると「AI通知が一切来ない」か「固定文言が出て送出側の title/body が握り潰される」のどちらかになる（§3.5b） |
| #4 | （言及なし） | `notifyFocusChange` は**iOS側で既に配線済み**（`TerminalView.swift:185,193` → `TerminalSessionController.swift:536`） | 経路Aの抑制はRust側で既に効く。Swift側に抑制を書かない |
| ★#4 | round-1 D-1 の「Swift は一切抑制しない」 | **Android 自身がそうしていない**。`TabAlertNotifier.kt:26-31` のdocが明示: 「純粋なUI opt-in設定・OS権限確認はKotlin側でよい」。`notify()` の冒頭は `if (!enabled) return; if (!hasPermission(context)) return` | プロファイル opt-in は経路Bでは Rust に到達しない（経路Aの tmux フック設置可否を決めるだけ）。D-1 を字義通り実装すると **opt-out したプロファイルからAI通知が飛ぶ**（§2 D-1 で再定義） |
| #7 | 「GRDBに1テーブル追加」 | **不適切**。Android の `HostKeySettings` は `SharedPreferences("isekai_terminal_ui")` のグローバル設定。iOS の対称物は `AppSettingsKeys`（`UserDefaults.standard`、現在4キー） | #7 は GRDB 不要。`AppSettingsKeys` に1キー追加 |
| #9 | 「Rust側 `background_reliability_policy.rs`」 | `decideBatteryGuidance` / `BackgroundKillFacts` は Swift バインディング生成済み（generated `isekai_terminal_core.swift:1976`, `9173`）。ただし `is_ignoring_battery_optimizations` という Android 固有の免除概念と「kill 2回以上で nag」という Android 前提のポリシーに固定されている | 「呼べるから呼ぶ」は誤り。§3.9 で「呼ばない」と決める |
| ★#9 | round-1 §3.9.3(c) の3分岐バナー「判断はRust側が既に持っている」 | **半分しか正しくない**。`BackgroundState` は private で、`orchestrator.rs:180-183` のdocが「UniFFIへは公開しない」と明示。かつ「猶予内復帰・接続生存」の場合、`notify_will_enter_foreground`（同 1285-1310）は `was_suspended == false` 分岐を通り**コールバックを1つも発火しない** | Rust は判断を持つが、どちらの分岐を通ったかを観測する手段を公開していない。Swift 側で `ConnectionState` 遷移から推測するのはミラー状態機械であり D-1 違反。**「Rust変更ゼロ」の撤回**（§3.9.3c, D-6） |
| #10 | 「iOSにFGS相当が無い」 | 加えて **Android の `ReattachStateStore.kt` に相当するストアが iOS にゼロ**（`grep -i reattach ios/Sources` がヒットなし）。一方 Rust の `reattach_record_is_fresh` / `reattach_grace_window_secs`（`AUTO_REATTACH_GRACE_SECS = 30*60`）は**プラットフォーム中立な純関数**で export 済み | #10 の主作業は「Swift にストアを新設し、既にある Rust ポリシーを使う」 |
| ★#10 | round-1 §3.10.2-①「tmuxの紐付けは既存の `TmuxTabLocator` が持っているので重複して持たない」 | **同一プロファイル複数タブで破綻する**。`tmux_tab_locators` は `profileId` が主キー（`ProfileDatabase.swift:509-513` のコメント自身が「1プロファイルにつき高々1タグ」と明記）。Android はこれを `tmuxClaimedProfileIds`（`ConcurrentHashMap.newKeySet<Long>()`、コルーチン起動**前**に `putIfAbsent` で同期予約、`TerminalTabsViewModel.kt:418,1046-1065,666-670`）で防いでいる。そのdocは実際の事故を記録している: 同一プロファイル2タブがほぼ同時に `connected` へ遷移すると「`@isekai_ctl_sock`が永久に正しいウィンドウへ届かなくなる二次被害」。`grep -rn "claimed" ios/Sources` は**ヒットゼロ** | round-1 の #10 は、**Android が本番で踏んで修正済みのバグを、コールドスタート時のN多重で再導入する**設計だった（§3.10.2-② で claim ガードを追加） |
| ★#3 | round-1 の性能ゲート「`TerminalFrameRendererTests` はCIで実行されない」（レビュー S2 の主張） | **レビュー指摘は誤り**。`ios-rust-core-check.yml:142` は `xcodebuild test -scheme IsekaiTerminalCore-Package`（SwiftPM 自動生成の集約スキーム）を実行しており、`Package.swift:81-82` で宣言された `IsekaiTerminalCoreTests` は**その中で実行されている**。レビュアーはアプリ側スキームと vertical-slice ワークフローだけを確認していた | §3.6.1 で反論。ただし S2 の実質的懸念（共有 macOS ランナー上の性能アサーションは不安定）は受け入れる |
| 全体 | — | ライフサイクル転送（`notifyDidEnterBackground(budgetMs:)` 他）は `TerminalTabsHostView.swift:63-125` で**既に全タブへファンアウト済み**。Rust の `BackgroundState` FSM も実装済み | #10 はゼロからではなく既存土台の上の作業 |

**round-2 の headline conclusion（round-1 から変更）**:
**10項目全体で必要な Rust public API 追加は正確に2本**である
（`on_foreground_resume(did_reconnect: bool)` と tmux ウィンドウ claim の2本、D-6）。
両方とも**設計時点で判明している**ので、実装中に発見するのではなく、
Y-R チェックポイントで**1回だけ**バインディング再生成を回す。

### 1.3 `PLAN.md` Phase Y との関係（どこが有効で、どこが既に古いか）

**有効（本ADRが土台にする、2026-07-04 の外部レビュー結論）**:

1. **「無期限にソケットを生かす」ことを iOS 版の仕様として約束しない**。正（SSOT）は
   「iOS上のQUIC connectionの生存」ではなく「isekai-pipe serve 側の論理セッション」。
2. `beginBackgroundTask` は短時間の後始末用。`BGAppRefreshTask`/`BGProcessingTask` は
   常時接続に使えない。**Live Activities は v1 必須ではなく実験機能枠**。
   `NEAppPushProvider`/PushKit はプライバシー説明・審査リスクの観点で非推奨。
3. Phase 0-5: `LazyLock<Runtime>` のワーカースレッドはサスペンド中に停止するが
   壁時計時間は経過するため、QUIC の idle timeout/keepalive は復帰時に stale 化しうる。
4. 物理 Wi-Fi/セルラー同時マルチパスは v1 スコープ外。

**既に古い（本ADRが明示的に置き換える、S9）**:

`PLAN.md` Phase 1C の #24 節（2796-2868行）は、`rust-core/src/session_supervisor.rs` に
`SessionState` × `ExecutionMode` の8状態 FSM を UniFFI Object として公開する設計を
「現行」として記述し、`SessionOrchestrator` との統合は「未実施のまま次フェーズ以降へ
持ち越す」と書いている。**このファイルは既に存在しない。** commit `710aecf2` が
削除し、`orchestrator.rs` の private な3状態 `BackgroundState`
（`Foreground`/`Quiescing`/`Suspended`）へ統合済みである。`orchestrator.rs:176-186` が
その縮小の理由を記録している（`Closing`/`Closed` は Swift/Kotlin の `disconnect()` と
アプリ終了で足りる、`Connecting`/`Resuming` は既存の `ConnPhase` で表現済み、
**UniFFI へは意図的に公開しない**）。

本ADRは現状（`BackgroundState`）に基づく。**`PLAN.md` Phase Y の当該記述の更新を、
本計画の最終フェーズ（Y-P5）の明示的な成果物とする**——`IOS_PARITY_GAP.md` の前文が
「Phase Y節はもはや現状を反映していない」と嘆いている状態を、これ以上深くしない。

### 1.4 検証環境の制約（設計判断に直結する）

- **ローカルビルド/テストは全面禁止**（`prefer-gh-actions-over-local-cargo`）。全検証はCI。
- iOS関連CIジョブと**実際に実行されるテストターゲット**（round-2 で実測確認）:

| ワークフロー | 実行内容 | 実行されるテスト |
|---|---|---|
| `ios-logic-linux-check.yml` | Linux で `swift test` | `IsekaiTerminalCoreLogicTests` のみ（`Package.swift` が `#if os(Linux)` で Apple 専用ターゲットを除外）。**最も安く速い** |
| `ios-rust-core-check.yml` | macOS、`xcodebuild test -scheme IsekaiTerminalCore-Package` | **`IsekaiTerminalCoreTests` と `IsekaiTerminalCoreLogicTests` の両方**（SwiftPM 集約スキームは全 testTarget を含む） |
| `ios-app-build-check.yml`（約31分） | macOS、`xcodebuild test -project IsekaiTerminalApp.xcodeproj -scheme IsekaiTerminalApp` | `IsekaiTerminalAppTests` / `IsekaiTerminalAppUITests`（スキームの test blueprint はこの2つのみ） |
| `ios-ssh-vertical-slice-check.yml`（約27分） | fixture 付き、`-only-testing:` で2クラスのみ | `SshVerticalSliceTests`、`KeyManagerTests/testGeneratedKeyAuthenticatesAgainstRealSshd` |

- **iOS 系のワークフローはどれも branch protection の required check ではない**
  （required は `android-unit-test` / `rust-core-test-linux` / `android-uniffi-drift` /
  `lockfile-drift` / `room-migration` の5本のみ）。したがって iOS 側のテストは
  「PRマージ前に green を目視確認する」運用でしか守られない。**この事実が D-2 と
  §4.3 の設計理由である。**
- 逆に `android-uniffi-drift` は **required** である。iOS のために Rust API を足して
  Kotlin バインディングの更新を忘れると、**Android のコードが一行も関係しないのに
  main へのマージが全員分止まる**（D-6, P2）。

---

## 2. Decision（全体方針）

### D-1. Rust は SSOT のまま。ただし「何が Rust 専管か」を正確に定義する

**（round-2 で再定義。round-1 の「Swift は一切抑制しない」は誤りだった — B2）**

`rust-ssot.md` が対象にしているのは**セッション/接続/トランスポートの状態と、
その状態に基づく意思決定**である。この境界を、本ADRの範囲で運用可能な形に落とす:

**Rust 専管（Swift に書いたら違反）**:
- セッション状態に由来する抑制・分岐。例: 「このタブが今フォアグラウンドで表示中だから
  tmux 通知を出さない」「同じ `(tmux_tag, seq)` は重複だから捨てる」「猶予内復帰だから
  再接続不要」。
- ポリシー閾値。Swift に「30分」「2回」を書かない → `reattachGraceWindowSecs()` を呼ぶ。
- `BackgroundState` / `ConnPhase` のミラー enum を Swift に作ること。
- **どのセッションがこのプロファイルの tmux ウィンドウを所有しているか**（§3.10.2-②、B4）。

**Swift 側で正しい（違反ではない）**:
- **プロファイル単位の UI opt-in 設定**（`enableTabNotifications` 等）による gating。
- **OS 権限の確認**（通知許可、ファイルアクセス等）による gating。
- UI 表示に閉じた状態（スクロール位置、シートの開閉、タブの表示順）。

後者2つが違反でないことは、**Android 自身が明文化している**
（`TabAlertNotifier.kt:26-31`: 「`.claude/rules/rust-ssot.md`: セッション状態に基づく判断は
Rust側、純粋なUI opt-in設定・OS権限確認はKotlin側でよい」）。iOS 側もこの前例に従う。
将来のレビュアーがこれを違反と誤認しないよう、実装時のソースコメントに
`TabAlertNotifier.kt` の当該docを参照として残すこと。

**Swift 側 gate はこの2つで打ち止めとする（m1）**。`willPresent` や `TabAlertNotifier` に
3つ目の条件を足すこと——たとえば「アプリが最前面のときは出さない」——は、
**本ADRの改訂を要する変更**として扱う。「OS 権限の確認」は放っておくと
「…かつ最前面でないとき」のような節を静かに生やすカテゴリであり、それはまさに
Rust が既に持っている抑制判断の二重化である。

**なぜこの区別が実害に直結するか**: 経路B（ctl `Notify`）のプロファイル opt-in フラグは
**Rust に到達しない**（Rust に渡るのは経路Aの tmux フック設置可否だけ）。
「Swift は一切抑制しない」を字義通り実装すると、通知を OFF にしたプロファイルから
AI 通知が飛ぶ。

### D-2. 新規ロジックは `IsekaiTerminalCoreLogic`（Linux CI）に置き、View は薄くする

§1.4 のとおり、iOS 系 CI はどれも required check ではなく、macOS ジョブは高価である。
したがって**各項目の「決定を下す部分」は純粋な Swift 型として
`IsekaiTerminalCoreLogic` に置き、View はその結果を描画するだけにする**。
既存の `TerminalScrollback.swift` / `SnippetCommands.swift` /
`TmuxTabWindowCoordinator.swift` / `SshHostTrustStore.swift` がこの分割の前例である。

### D-3. Android の UI を移植するのではなく、Android の**意図**を iOS の語彙で満たす

#1〜#8 は Android と機能的に同等になることを目指す（見た目まで同一にする必要はない）。
#9/#10 は、Android の文言・UI・トリガー条件をそのまま持ち込むと**iOS では端的に
誤情報になる**（§3.9.2）。この2項目は「Androidと同じ機能を提供する」ではなく
「Androidでその機能が解決していたユーザーの困りごとを、iOSで解決する」を要件とする。

### D-4.（撤回）GRDB マイグレーションの直列化 → **予約レジストリの新設**に置き換え

**round-1 の D-4「本ADR期間中に GRDB を触るのは1項目だけ」は撤回する（S10-Q5）。**

撤回理由（レビュアーの指摘に同意）: round-1 の D-4 自身が
「他の項目が GRDB スキーマ変更を必要とすると判明したら本ADRを改訂して直列化する」
という条件付きであり、それは**実装中に再計画する約束**にすぎない。#3 がプロファイル単位の
フォント設定を、#1 がプレビュー履歴テーブルを欲しがる可能性は現実的にあり、
D-4 が計画期間を生き延びる見込みは高くない。

**代替決定**: **Y-P0 で GRDB マイグレーション予約レジストリを新設する**。
Room 側で実証済みのパターン（`scripts/reserve-room-migration.sh` +
`android/migration_registry.toml` + `room-migration-check.yml`）を GRDB へ移植する:

- `scripts/reserve-grdb-migration.sh <owner-slug>` — 次の版数を予約して登録する。
- `ios/migration_registry.toml` — 予約台帳。
- `grdb-migration-check.yml` — `ProfileDatabase.migrator` の登録名の連番と台帳の整合を検証。
  **required check にはしない**（iOS 系を required に上げる判断は本ADRのスコープ外）。

これにより、GRDB を触る項目の並列化制約が計画全体から消える。

### D-5. 「常に接続できる」原則の iOS 版解釈

`.claude/rules/always-connects.md` の精神——「ユーザーの手動操作を要求する復旧不能状態を
作らない」——を iOS にも適用する。iOS 版におけるその具体化は:

> **アプリを前面に戻したら、ユーザーが何もしなくても元の作業に戻れている。**

§3.10 の設計はこの一文を満たすためのものであり、「バックグラウンドで接続を維持する」ことは
その手段の一つにすぎない（iOS では実現不能なので、tmux + 自動復元という別の手段で
同じ結果を出す）。

### D-6.（新設）Rust API 追加は正確に2本。Y-R チェックポイントで1回だけ再生成する

**（round-1 の「Rust変更ゼロ」を撤回 — B3, B4, S1, P2）**

本計画で追加する Rust public API は、設計時点で判明している次の2本のみである:

| API | 用途 | 由来 |
|---|---|---|
| `SessionOrchestrator::on_foreground_resume(did_reconnect: bool)`（`OrchestratorCallback` への1メソッド追加） | 前面復帰時にRustが下した「再接続したか / 猶予内で接続が生きていたか」の判断を、Swift/Kotlin が観測できるようにする | B3 / §3.9.3c |
| `try_claim_tmux_window(profile_identity: String, owner_id: String) -> bool` / `release_tmux_window_claim(profile_identity: String, owner_id: String)`（新規 `rust-core/src/tmux_window_claim.rs` の free function 2本） | 同一プロファイル複数タブが tmux ウィンドウの所有権を奪い合うのを、プロセス全体で排他する | B4 / §3.10.2-② |

**運用ルール**:

1. **この2本は Y-R フェーズ（§4.1）でまとめて実装し、`regenerate-uniffi-bindings.yml` を
   1回だけ回す。** API ごとに1回ずつ回さない。実装中に3本目が必要になった場合は、
   まず「Swift に状態機械を作りかけていないか」を疑い、それでも必要なら Y-R を
   もう一度開くのではなく、そのAPIを必要とする項目自体を再設計する。
2. **再生成後にコピーするファイルは7つ**（`uniffi-binding-regeneration.md`）:
   - Kotlin: `android/src/main/kotlin/uniffi/isekai_terminal_core/isekai_terminal_core.kt`
     （1ファイル、`.sha256` サイドカーなし）
   - Swift: `ios/Sources/IsekaiTerminalCoreLogic/generated/` 配下の
     `isekai_terminal_core.swift` / `isekai_terminal_coreFFI.h` /
     `isekai_terminal_coreFFI.modulemap` **と、対応する `.sha256` 3ファイル**
3. **Kotlin バインディングを忘れると全員のマージが止まる**。`android-uniffi-drift` は
   `main` の required status check である（§1.4）。iOS のための API 追加であっても、
   Kotlin 側の生成物が古いままだと required check が赤になり、**その変更と無関係な
   並列PRがすべてブロックされる**。並列worktree運用の最中にこれが起きると、
   復旧にもう1往復のCIが必要になる（P2）。
4. `.sha256` サイドカーの取りこぼしは 2026-08-09 に実際に発生している。本体ファイルだけ
   正しくても drift-check は stale 判定で落ちる。
5. **`OrchestratorCallback` にメソッドを足すと壊れる適合の完全な一覧
   （round-3 で訂正・拡張 — N4）**。UniFFI が生成するプロトコル/インターフェースの
   メンバーは Swift・Kotlin どちらも**必須**である（Rust trait 側にデフォルト実装が無く、
   生成された Kotlin `OrchestratorCallback` も全メソッドを abstract で宣言する）。
   したがって **Y-R の作業単位には次の4箇所の追従を必ず含める**:

   | 適合 | ファイル | ターゲット | 落ちるCIジョブ |
   |---|---|---|---|
   | `TerminalSessionController` | `ios/Sources/IsekaiTerminalCore/TerminalSessionController.swift:112` | `IsekaiTerminalCore` | `ios-rust-core-check` / `ios-app-build-check` |
   | `SshVerticalSliceRecorder` | `ios/Tests/IsekaiTerminalCoreTests/SshVerticalSliceTests.swift:61` | `IsekaiTerminalCoreTests` | `ios-rust-core-check` / `ios-ssh-vertical-slice-check` |
   | `KeyManagerAuthRecorder` | `ios/Tests/IsekaiTerminalCoreLogicTests/KeyManagerTests.swift:73` | **`IsekaiTerminalCoreLogicTests`** | **`ios-logic-linux-check`** — 本ADRが第一ゲートに指定している最も安いジョブ |
   | Kotlin 実装（匿名オブジェクト） | `android/.../session/TerminalSession.kt:235` | `android` | **`android-unit-test`（required）** |

   - **round-2 の「`FakeSshGateway.kt` 2箇所」という記述は誤りだった**（N4）。
     `android/src/{test,androidTest}/.../FakeSshGateway.kt:12` は
     `var callback: OrchestratorCallback? = null` として**参照を保持しているだけ**で、
     インターフェースを実装していない。Kotlin 側の唯一の実装者は上表の匿名オブジェクトである。
     誤った案内は実装者を存在しない作業へ誘導するので訂正する。
   - **Swift 側3箇所を見落とすと、`ios-logic-linux-check`——本ADRが「everything else が
     依存する第一ゲート」と位置づけたジョブ——が赤くなる**。round-2 は Kotlin 側しか
     数えていなかった。
   - Android 側の `on_foreground_resume` は **no-op + ログのみ**にとどめる（Q10）。
     Y-R は required check を2本ゲートする唯一のPRなので、機械的なバインディング変更に
     UX 変更を混ぜない。Android での活用は別 follow-up とする。
   - tmux claim API の Android 側移行（`tmuxClaimedProfileIds` を Rust の claim へ寄せる）は
     **トリガー付き follow-up**（Q9、§5.1-4）。Y-R では Rust 側に API を足すだけに留める。
6. **Y-R は Y-P0 より前（または同時）にマージする**（§4.1、N4/NP1）。他フェーズの
   worktree が在庫にある状態でこの署名変更をマージすると、変更に無関係な全ブランチが
   自分のテスト結果を見る前にリベース＋スタブ追加を強いられる。しかも iOS 系ジョブは
   required check ではないので、**壊れたまま main にマージされて放置されうる**。

---

## 3. Decision（項目別）

各項目に、**新規/変更ファイル**・**Logic側に置くもの**・**Rust API の要否**・
**依存**・**受け入れ条件と、それを実行するCIジョブ（または手動確認）** を記す。

**検証欄の凡例**（S4/P4 対応）:
- 🟢 `ios-logic-linux-check`（Linux、安価、毎PR）
- 🟡 `ios-rust-core-check`（macOS、`IsekaiTerminalCore(-Logic)Tests` を実行）
- 🟠 `ios-app-build-check`（macOS 約31分、`IsekaiTerminalApp(UI)Tests` を実行）
- 🔴 **手動確認（実機またはSimulator）**。CIでは検出できない。

### 3.1 #5 OSC 133 セマンティックプロンプト ナビゲーションUI【低工数・Rust変更なし】

- **Rust側**: 変更なし。`on_prompt_jump` / `on_prompt_output_copy_ready`（`lib.rs:1497-1500`）、
  `copy_last_command_output`（`orchestrator.rs:1420`）は既にあり、Swift 側は現状 no-op
  （`TerminalSessionController.swift:974,976`）。
- **変更**: `TerminalSessionController.swift`、`TerminalView.swift`（アクセサリバーに
  「前のプロンプト」「次のプロンプト」「直前の出力をコピー」の3ボタン）。
- **Logic側**: ジャンプ対象が `nil`（該当プロンプトなし）のときの表示文言決定を純関数に。
- **依存**: なし。
- **受け入れ条件と検証**:
  - 🟢 該当なし時の文言決定が正しい。
  - 🟠 `TerminalSessionControllerTests` に、`onPromptJump` 受信でスクロール指示が
    発行されることのテストを追加。
  - 🔴 実機/Simulator で、実際にプロンプト間をジャンプでき、直前出力のみがコピーされる。

### 3.2 #2 AI/リッチパネルUI【中工数・Rust変更なし】

- **Rust側**: 変更なし（§1.2 検証済み）。
- **新規**: `ios/Sources/IsekaiTerminalCore/AiPanelSheet.swift`。
- **新規（Logic）**: `ios/Sources/IsekaiTerminalCoreLogic/AiPanelFormSubmission.swift` —
  フォーム入力値から送信バイト列（1行JSON + 改行）を組み立てる純関数。
  - **M1 対応**: この純関数のテストは「キー順序が**決定的である**こと」を検証するが、
    **Android と同一の順序になることは検証しない**。Android の
    `org.json.JSONObject(map).toString()` の順序は渡した Map のイテレーション順に従う
    仕様外の挙動であり、パリティ・プロパティではない。テストの意図をコメントに書く。
  - テスト対象: 決定性、エスケープ（`"`/`\`/改行）、空フォーム、非ASCII値。
- **変更**: `TerminalSessionController.swift`（`submitAiPanelForm(values:)` — 上記純関数の
  結果を `send()` するだけの薄い委譲 + dismiss）、`TerminalView.swift`（シート提示）。
- **信頼境界（Android の設計を必ず踏襲）**: パネル内容はリモートの任意プロセスが偽造できる
  PTY 上の in-band データ。**表示専用テキストとしてしか扱わない**——自動実行・
  シェルコマンド化・クリップボード自動書き込みをしない。冒頭に「リモートから受信した
  パネル」を常に明示。Markdown は MVP スコープどおり構文解釈せず生テキスト表示
  （SwiftUI が `LocalizedStringKey` 経路で Markdown を解釈しないよう
  `Text(verbatim:)` を使う）。
- **依存**: なし。
- **受け入れ条件と検証**:
  - 🟢 送信バイト列組み立ての全境界ケース。
  - 🟠 `TerminalSessionControllerTests` に「`submitAiPanelForm` が期待バイト列を `send` し、
    パネルを閉じる」テスト（`send` をフェイクで観測）。
  - 🔴 **手動**: `presentDocument` / `presentForm` の両方が実際に表示されること、
    「リモートから受信」表示があること、フォーム送信結果がPTYへ届くこと。
    Simulator + fixture sshd から APC シーケンスを流して確認する。

### 3.3 #8 Snippet テンプレートギャラリー【低工数・Rust変更なし】

- **Rust側**: 変更なし。
- **新規（Logic）**: `ios/Sources/IsekaiTerminalCoreLogic/SnippetTemplates.swift` —
  バンドル済みテンプレートの静的配列。
- **変更**: `SnippetListView.swift`（「テンプレートから追加」導線）。
- **設計判断**: テンプレート定義を Rust 共通層に置く案は**採らない**。これは接続/セッションの
  状態でも意思決定でもなく単なる UI 初期データであり、Rust に置くと UniFFI regen サイクルを
  増やすだけで対称性の実利がない（D-1 の「UI表示に閉じた状態」に該当）。
- **依存**: なし。
- **受け入れ条件と検証**: 🟢 テンプレート定義の妥当性（重複ID・空コマンドなし）。
  🟠 `SnippetListModelTests` に「テンプレートから作成したスニペットが通常のスニペットとして
  永続化される」テスト。

### 3.4 #7 新規ホスト鍵の自動信頼トグル【低工数・Rust変更なし】

- **Rust側**: 変更なし。
- **変更**: `AppSettingsKeys.swift`（`autoTrustNewHostKeys`、既定 `false`）、
  `SettingsView.swift`（Y-P0 で抽出、§3.11）にトグル、
  `TerminalSessionController.swift:804` 付近のハードコードを `UserDefaults` 読み出しへ。
- **GRDB マイグレーション不要**（§1.2 補正）。
- **セキュリティ注記**: 既定 OFF を維持。説明文に
  「オンにすると初回接続時のfingerprint確認を省略します。信頼できるネットワークでのみ
  使用してください」を明記。**既知ホストの fingerprint 不一致は、このトグルの状態に
  かかわらず常に拒否する**（`always-connects.md`: MITM と正当な再デプロイを機械的に
  区別できないための意図的な設計。`SshHostTrustStore.swift` の既存挙動を変更しない）。
- **依存**: Y-P0 の `SettingsView` 抽出。
- **受け入れ条件と検証**:
  - 🟡 `SshHostTrustStoreTests` に「トグル ON でも既知ホストの fingerprint 不一致は拒否」を
    追加（**この項目で最も回帰させてはいけない性質**なのでテストで固定する）。
  - 🟠 `TerminalSessionControllerTests` に「トグル ON で未知ホストが確認なしに通る」。

### 3.5 #4 tmux/AI 注目通知【中工数・Rust変更なし・GRDB v7】

**（round-2 で経路A/経路Bに分割 — B1）**

通知経路は独立した2本ある（§1.2 ★#4）。両方を実装しないと、**このリポジトリで
日常的に使われている通知（claude-hookd → `isekai-pipe ctl notify`）は届かない**。

#### 3.5.0 共通の土台（4a/4b 双方が使う）

1. **GRDB マイグレーション `v7_add_enable_tab_notifications`**: `ConnectionProfile` に
   `enableTabNotifications: Bool`（既定 `false`）を追加。
   - **Q3 への回答（S10）: プロファイル単位を維持する。** 理由は3つ:
     (a) 粒度として正しい（ビルドサーバーのプロファイルからは通知したいが、
     使い捨ての検証ボックスからは要らない）、(b) Android と対称、(c) **経路Aでは
     Rust が `ensureWindow` 時にこの値を必要とする**（フック設置の可否判断）ので、
     グローバル設定にすると結局プロファイルごとの値を作り直すことになる。
   - D-4 撤回により、この migration は Y-P0 の予約レジストリ経由で版数を取る。
2. **`SettingsView`/`ProfileEditView.swift`**: プロファイル単位の通知 opt-in トグル。
3. **`UNUserNotificationCenter` 権限**: 初回 opt-in 時に `requestAuthorization`。
   Info.plist の追加も entitlement も不要。権限が拒否された場合、**トグルは ON のまま**
   「システム設定で通知が無効です」を表示し `openSettingsURLString` への導線を出す
   （ユーザーの意思表明を勝手に OFF に戻さない）。
4. **新規 `ios/Sources/IsekaiTerminalCore/TabAlertNotifier.swift`**: 実際の通知配信。
   Android の `TabAlertNotifier.notify` と同じく、冒頭で
   **(a) プロファイル opt-in、(b) OS 通知権限**の2つを確認して早期 return する
   （D-1 のとおりこれは違反ではない。ソースコメントに `TabAlertNotifier.kt:26-31` を
   参照として書く）。
5. **新規（Logic）`ios/Sources/IsekaiTerminalCoreLogic/TabAlertCopy.swift`**:
   Android の `titleAndTextFor` + `notify` の文言決定部分を移植した純関数。
   **シグネチャは Android に合わせる**:

   ```swift
   public func tabAlertCopy(
       kind: NotifyKind,
       profileLabel: String,
       message: (title: String, body: String)?   // 経路B の送出側指定。nil なら経路A
   ) -> (title: String, body: String)
   ```

   挙動（`TabAlertNotifier.kt:116-118` と同一）:
   - `message?.title` が空でなければ `"\(profileLabel): \(title)"` を使う。
     空/`nil` なら `kind` ごとの固定文言にフォールバック。
   - `message?.body` が空でなければそれを使う。空/`nil` なら固定文言。
   - `kind` の `switch` に `default:` を書かない（将来ケース追加でコンパイルエラーにする）。

   **`message` を無視して常に固定文言を使う実装は不可**。`TabAlertNotifier.kt:99-104` が
   その理由を明記している:「固定文言で上書きしてしまうと送出側の意図が失われるため」。

#### 3.5a 経路A（tmux hook: `bell` / `activity` / `silence` / `jobDone`）

- **配線**: `TerminalSessionController.swift:990` の `onNotify(kind:)` no-op を実装で置換 →
  `TabAlertCopy(kind:profileLabel:message: nil)` → `TabAlertNotifier`。
- **`ensureWindow` の実配線（これをやらないと何も飛んでこない）**:
  `TmuxTabWindowCoordinator.ensureWindow(enableNotifications:)` に、ハードコードの
  `false` ではなくプロファイルの値を渡す。同ファイル 107-111 行のdocコメントも更新する。
- **抑制は書かない**: フォアグラウンド/タブフォーカス抑制と `(tmux_tag, seq)` 重複排除は
  Rust 側で完結しており、`notifyFocusChange` は iOS で既に配線済み（§1.2）。
  `UNUserNotificationCenterDelegate.willPresent` では **Rust が送ってきた経路A の通知は
  常に表示する**（抑制済みのものはそもそも届かない）。

#### 3.5b 経路B（ctl `Notify`: `waiting` / `done` / `info`）

- **新規（Logic）`ios/Sources/IsekaiTerminalCoreLogic/NotifyGenerationTracker.swift`**:
  `notifyGeneration` の前回値を保持し、進んでいれば
  `(kind, title, body)` を1回だけ吐き出す純粋な差分器。Android が
  `TerminalTabsViewModel.kt:286-296` で行っている世代差分に相当する。

  **リセット意味論（round-3 で仕様化 — N1）。** ここは「テストケースとして列挙する」だけでは
  足りず、**要求される挙動として**書く必要がある。素朴に
  「`generation > last` なら発火」とだけ実装すると、**新しい論理セッションが作られた瞬間から、
  カウンタが旧セッションの最高値を追い越すまで AI 通知が全部黙って落ちる**。
  これは P1（claude-hookd 通知が来ない）が別の扉から再来するもので、しかも
  「新しいセッションでは動くが再接続後に死ぬ」という間欠的な形なので、元のP1より診断が難しい。

  同じファイルに、**この問題について異例に詳しく文書化された先例**がある
  （`TerminalSessionController.swift:70-83` の `lastFiredBellGeneration` のdoc、
  および `:552-570` のリセット実装）。そこから2点を継承する:

  1. **リセットは `@MainActor` の同期文脈で行う**。既存実装が
     `uiState.latestScreenUpdate = nil` と**同じ同期ブロック**で
     `uiState.lastFiredBellGeneration = 0` を書いているのは意図的で、doc が理由を明記している
     ——`Task` 経由の非同期リセットにすると、新セッション最初の `onScreenUpdate`
     コールバックを処理する Task の方が先に MainActor キューで実行され、
     最初の通知を取りこぼす競合が生まれる（Codexレビュー指摘）。
     `NotifyGenerationTracker` のリセットも同じ同期文脈に置く。
  2. **リセットは手動 `reconnect()` だけでなく `Connected` への遷移で行う**。
     既存の bell リセットは `reconnect()` の `.disconnected, .failed` ケースにしか無い
     （`:558`）。しかし Rust は Swift が `reconnect()` を通らない経路でも新しい Terminal を
     作る——`spawn_reconnect_loop`、および `notify_will_enter_foreground` の自動再接続
     （`orchestrator.rs:1291-1300`）。bell 側にこの穴が今あるならそれは既存の潜在バグだが、
     いずれにせよ **#10 は自動再接続を「稀な経路」から「常態」に変える**
     （前面復帰・コールドスタート復元）ので、新しいトラッカーがこの穴を継承してはならない。
     `Connected` 遷移でリセットすれば手動・自動を一様にカバーできる。
  3. **防御的に、世代の *減少* もリセットとして扱う**（比較1回分のコスト）。

  テスト対象（🟢、**上記を挙動として検証する**）: 初回（前回値なし）、据え置き、1つ進む、
  複数飛ばし（conflated チャネルでの取りこぼし）、**世代の巻き戻り＝発火する**、
  **`Connected` 遷移でリセットされ、その後の gen 1 が発火する**。
- **配線**: `TerminalScrollback.swift` が既にスナップショットへ写している
  `notifyGeneration`/`notifyKind`/`notifyTitle`/`notifyBody`（`TerminalScrollback.swift:47-50`）を
  `TerminalSessionController` の画面更新経路で `NotifyGenerationTracker` に通し、
  進んでいれば `TabAlertCopy(kind:profileLabel:message: (title, body))` → `TabAlertNotifier`。
- **既知の非対称（Android から輸入する）**: 経路B には Rust 側のフォアグラウンド/
  タブフォーカス抑制が**無い**（`AI_INTEGRATION_DESIGN.md` §11.1.4、Android 側の既知差異）。
  したがって当該タブを見ている最中でも通知が出うる。**iOS だけ先に直さない**——
  直すなら Rust 側を直して両OSに効かせる（そうしないと D-1 が形骸化する）。
  本ADRのスコープ外の follow-up として記録する。
- **信頼境界**: `notifyTitle`/`notifyBody` はリモートが偽造できる表示専用データ。
  通知本文に入れるだけで、リンク化・実行・パースをしない。

- **依存**: Y-P0（GRDB 予約レジストリ、`SettingsView`）。
- **受け入れ条件と検証**:
  - 🟢 `TabAlertCopy` の**両分岐**（`message` あり / なし）を全 kind でテスト。
    **`message` あり分岐のテストを省略しない**（これが B1 の再発防止線）。
  - 🟢 `NotifyGenerationTracker` の6ケース。
  - 🟡 opt-out プロファイルで `TabAlertNotifier` が早期 return する。
  - 🔴 **手動**: 実機で `isekai-pipe ctl notify --title X --body Y` を打ち、
    **タイトルが `プロファイル名: X`、本文が `Y`** で届くこと（固定文言でないこと）。
    tmux の `bell` で固定文言の通知が届くこと。opt-out プロファイルで何も届かないこと。

### 3.6 #3 カスタム端末フォント読み込み + グリフフォールバック【中工数・Rust変更なし】

- **Rust側**: 変更なし。
- **新規**: `ios/Sources/IsekaiTerminalCore/TerminalFontImport.swift`
  （`UIDocumentPickerViewController` → Application Support へコピー →
  `CTFontManagerRegisterFontsForURL` で登録 → PostScript 名を `UserDefaults` に永続化 →
  起動時に再登録）。
- **変更**: `TerminalFrameRenderer.swift`（登録済みカスタムフォント優先、
  未設定/登録失敗時はシステム等幅へ黙ってフォールバック）、
  `SettingsView.swift`（フォントインポートの入口）。
- **グリフフォールバック**: `CTFontGetGlyphsForCharacters` でグリフ有無を判定し、
  無ければ `CTFontCreateForString` でシステムのフォールバックを解決。
  **コードポイント → 使用フォントの解決結果を LRU キャッシュに載せる**（キャッシュは
  カスタムフォント変更時に全消去）。
- **Logic側**: `ios/Sources/IsekaiTerminalCoreLogic/GlyphFallbackCache.swift` —
  LRU キャッシュと、解決関数を注入可能にしたポリシー層。Core Text 呼び出し自体は
  注入されるクロージャに逃がす。

#### 3.6.1 レビュー指摘 S2 への反論と、受け入れた部分

**反論**: レビューは「`TerminalFrameRendererTests` は `IsekaiTerminalCoreTests` に属し、
CI のどのジョブも実行していない」と述べたが、これは誤りである。
`ios-rust-core-check.yml:142` は

```
xcodebuild test -scheme IsekaiTerminalCore-Package -destination '...'
```

を実行している。`IsekaiTerminalCore-Package` は SwiftPM が自動生成する集約スキームであり、
`Package.swift:81-82` で宣言されている `IsekaiTerminalCoreTests` を含む**全 testTarget** を
実行する。レビュアーはアプリ側スキーム（`IsekaiTerminalApp.xcscheme`）と
`ios-ssh-vertical-slice-check.yml` の `-only-testing:` だけを確認しており、
`ios-rust-core-check.yml` を見落としている。したがって
「S2: 実行されないスイートにゲートを置いている」「R1 の緩和策は fictional」は成立しない。

この反論は round-2 レビューで**レビュアー自身が全面的に認めた**
（「My round-1 S2 checked `IsekaiTerminalApp.xcscheme` and the `-only-testing:` filter in
`ios-ssh-vertical-slice-check.yml` and never opened `ios-rust-core-check.yml`」）。
§1.4 の CI/テストターゲット対応表は、この種の誤りが再発しないようにするための恒久措置である。

**それとは独立に、性能ゲートの形は変更する**（round-3 で記述を分離 — round-2 レビュー §0）。
round-2 のこの節は「S2 が実質的に指していた懸念は flaky だった」と書いていたが、
レビュアーから「それは私の意図ではない。私は CI 配線について事実誤認をしただけで、
flaky については何も言っていない」と指摘された。正確には**独立した2つの記述**である:

1. レビュアーの CI 配線に関する事実主張は誤りだった（上記）。
2. **それとは別に、共有 macOS ランナー上で「フレーム時間が N ms 未満」という
   閾値アサーションを置くのは避ける**——このリポジトリは Robolectric や QUIC テストで
   負荷起因の flaky に繰り返し悩まされた実績があり、他エージェントとの CPU 競合で
   壁時計ベースの判定は信頼できない。

2 の理由から、性能ゲートを**閾値アサーションではなく、以下の2段構えにする**:

- 🟢 **Logic 側で決定論的に検証する**（これが主たるゲート）:
  `GlyphFallbackCache` について「N セルの描画で Core Text 解決関数が呼ばれる回数が
  ユニークコードポイント数を超えないこと」を、注入したフェイク解決関数の呼び出し回数で
  検証する。**「速いこと」ではなく「重い呼び出しをキャッシュしていること」を測る**ので、
  マシン負荷から独立している。
- 🟡 `TerminalFrameRendererTests` には**計測値の記録のみ**を置き、失敗させる閾値は
  設けない（回帰の目視用）。

- **依存**: Y-P0（`SettingsView`）。
- **受け入れ条件と検証**:
  - 🟢 キャッシュのヒット/ミス・容量超過時の追い出し・無効化・解決関数の呼び出し回数上限。
  - 🟡 レンダラがカスタムフォント未設定時にシステム等幅へフォールバックする。
  - 🔴 **手動**: 任意の等幅フォントをインポートして適用でき、カスタムフォントに無い
    絵文字/CJK/記号がセル単位で正しくフォールバック描画される。スクロールが体感で
    引っかからない。

### 3.7 #6 外部（Bluetooth）キーボードの JIS/US 配列自動判定【要スパイク先行】

- **Phase A: 調査スパイク**。iOS で外部キーボードの物理配列を判定できる手段があるかを確定する。
  - **falsifiable な問い（M2 対応、round-2 で具体化）**:
    > `UIKeyboardHIDUsage` のうち JIS 配列固有のもの——`keyboardLANG1`/`keyboardLANG2`
    > （かな/英数）、`keyboardInternational1`（ろ）/`keyboardInternational3`（¥）——は、
    > **実物の JIS Bluetooth キーボードから `pressesBegan(_:with:)` / `GCKeyboard` 経由で
    > 実際に届くか？**
  - この問いに「届く」なら、上申時の配列判定APIが無くても**反応的検出**が可能になる
    （既定は US 扱い、JIS 固有キーの初回入力で JIS へ切り替え、以後そのキーボードを
    記憶する）。これは下記 Outcome-3（縮退）より明確に良い帰結であり、設計も別物になる。
  - **このサンドボックスからは検証できない**（実物の JIS Bluetooth キーボードが要る）。
    だからこそ「文献調査で結論した気になる」のではなく、**実機で1回叩いて確かめる
    質問**としてスパイクを定義する。
- **Phase B の3分岐（round-3 でラベルを改称 — m2。旧 `B-1/B-2/B-3` は #1 の
  リリース段階ラベルと衝突し、並列エージェントへの task brief で「B-1 だけ実装する」が
  曖昧になるため）**:
  - **Outcome-1（上申時に配列が分かる場合）**: Android の `KeyboardLayoutDetector` +
    `KeyboardLayoutMode` と同等の構成を移植。
  - **Outcome-2（反応的検出が可能な場合、M2 が示す中間ケース）**: 既定 US +
    JIS 固有 HID usage 初回検出で JIS へ切り替え + 手動オーバーライド。
  - **Outcome-3（どちらも不可能な場合の縮退）**: **手動オーバーライドのみ**。
    設定に「外部キーボードの配列: 自動（US扱い）/ JIS / US」の3択、既定は現状維持。
    機能的には Android に劣るが「JIS 配列で記号が打てない」という実害は解消する。
    gap 分析の #6 はこの縮退での完了を許容する。
- **Logic側**: `ios/Sources/IsekaiTerminalCoreLogic/KeyboardLayoutMode.swift` —
  配列モード enum と「キーイベント → 送出バイト列」のマッピング純関数。
  既存の `TerminalHardwareKeyMapper.swift` の配列差分部分を Logic 側へ切り出す。
- **依存**: スパイク結果に Phase B の形が依存。スパイクは他項目と並行に走らせる。
- **受け入れ条件と検証**: 🟢 各配列モードでの記号キーのマッピング。
  🔴 **手動（必須・CI不能）**: JIS 配列の外部キーボードで
  `@`/`[`/`]`/`:`/`_`/`\` が正しく入力できる。

### 3.8 #1 trzsz 転送ファイルのプレビューUI【高工数・Rust変更なし】

- **Rust側**: 変更なし。`file_preview.rs` と `filePreviewRequest` / `onFilePreviewResult` は
  既にあり、Swift 側は現状 no-op（`TerminalSessionController.swift:983`）。
- **段階分割**（一度に全部作ると途中で止まるため）:
  - **B-1（最小・これだけでリリース可能）**: 非同期リクエスト基盤 +
    ディレクトリブラウザ + プレーンテキストビューア。
  - **B-2**: 画像ビューア + CSV テーブルビューア。
  - **B-3**: Markdown レンダリング + シンタックスハイライト。
- **新規**: `ios/Sources/IsekaiTerminalCore/FilePreview/` 配下の SwiftUI View 群。
- **Logic側**（round-2 で S8 を反映して拡張）:
  `ios/Sources/IsekaiTerminalCoreLogic/FilePreview/` に以下を置く。
  1. **`FilePreviewRequestRegistry`（S8、B-1 の中核・最も壊れやすい部分）**:
     `filePreviewRequest` は**キューイングして即座に返り**、結果は後から
     `onFilePreviewResult(requestId:outcome:)` で非同期に届く。複数の `ls`/`cat` が
     同時に in-flight になりうる（Android は
     `ConcurrentHashMap<String, CompletableDeferred<FilePreviewOutcome>>` で対応、
     `TerminalSession.kt:232`）。iOS 側も `requestId` をキーにした in-flight レジストリを
     Logic 側に置き、**トランスポートを注入可能にして Linux CI でテストする**。
     カバーすべき挙動:
     - 同時 in-flight 複数件がそれぞれ独立に解決される。
     - タブを閉じた / シートを dismiss した時点で in-flight 要求がキャンセルされ、
       継続（continuation）がリークしない。
     - **セッションが in-flight 要求を抱えたまま切断した場合、全要求が失敗として
       解決される**（`orchestrator.rs:3408` が扱うのは「要求時点で未接続」の経路であり、
       **要求中の切断は別ケース**。Swift 側で待ち続けると UI が永久にスピナーのまま固まる）。
     - 同じ `requestId` が二重に返ってきた場合に2度目を無視する。
  2. `cat` のチャンク再結合（部分結果の順序と欠落検出）。
  3. ファイル種別判定（拡張子 + マジックバイト）、CSV パース、
     **リモートパスの正規化と結合**（`..` を含むリモート由来文字列。
     ディレクトリブラウザのナビゲーション破綻に直結するので境界ケースを固める）。
  4. 表示モデル導出。
- **信頼境界**: プレビュー対象はリモートの任意ファイル。表示専用として扱い、
  実行・自動オープン・`UIActivityViewController` への引き渡しを既定にしない。
- **依存**: なし。
- **受け入れ条件と検証**:
  - 🟢 上記 Logic の全項目。**特に「切断時に N 件の in-flight が全部解決される」を
    B-1 の受け入れ条件に含める**。
  - 🟠 `TerminalSessionControllerTests` に「`onFilePreviewResult` が正しい待ち手へ届く」。
  - 🔴 **手動（B-1/B-2/B-3 それぞれ）**: 実 sshd に対してディレクトリを辿り、
    各形式が表示できる。大きなディレクトリ・大きなファイルで固まらない。

### 3.9 #9 バックグラウンド信頼性の案内【要設計・Rust API 1本】

#### 3.9.1 Android で何を解決していたか

Android の案内 UI は次の4段の上に成り立っている:

1. **回避可能な**接続断要因がある（OEM 独自のバックグラウンドプロセス kill）。
2. その要因は**ユーザーの設定変更で実際に軽減できる**（バッテリー最適化の対象外）。
3. 発生を**観測できる**（clean-shutdown マーカー × 新鮮な reattach レコード）。
4. よって「2回以上観測されたら、14日クールダウンで設定変更を促す」nag が成立する。

#### 3.9.2 iOS ではこの4段すべてが崩れる

1. **回避可能な要因が存在しない**。iOS はバックグラウンド移行後、数十秒で**必ず**
   アプリをサスペンドする。OEM の逸脱ではなく OS の設計であり、どんな設定でも変わらない。
2. **設定変更で軽減できない**。特に **Background App Refresh を有効にするよう案内するのは
   端的に誤誘導である**——これが与えるのは `BGAppRefreshTask` による散発的な
   ウェイクアップ（数時間に1回、実行数十秒）であり、SSH セッションの維持には使えない。
   「設定したのに切れる」というより悪い信頼失墜を生む。
3. **観測が Android ほど当てにならない**。`applicationWillTerminate` は、サスペンド済みの
   アプリが jetsam や force-quit で終了する際には**呼ばれない**。かつ jetsam と force-quit を
   機械的に区別する信頼できる手段が無い。
4. 以上より、**nag するトリガーも、nag して促すべき行動も存在しない**。

#### 3.9.3 決定: nag しない。「説明」「実際に効く1つの助言」「復帰の可視化」に置き換える

**(a) `decideBatteryGuidance` を iOS から呼ばない。Rust 側の当該モジュールも変更しない。**
`BackgroundKillFacts.is_ignoring_battery_optimizations` に iOS の何かを写像するのは、
意味論の異なる値を同じフィールドに詰め込む行為であり、`rust-ssot.md` が禁じる
「食い違い得る状態のコピー」の一形態である。`background_reliability_policy.rs` は
Android 専用モジュールとして据え置く。

**(b) 新規 `ios/Sources/IsekaiTerminalCore/BackgroundBehaviorView.swift`（常設・nagなし）。**
Android 版が持っていた2経路（自動ポップアップ / メニューからの恒常入口）のうち、
**恒常入口だけを実装し、自動ポップアップは実装しない**。4ブロック構成:

| ブロック | 内容 |
|---|---|
| ① 何が起きるかの率直な説明 | 「iOS ではアプリがバックグラウンドにある間、SSH 接続を維持し続けることはできません（OSの制約であり、設定で変更できません）。代わりに、アプリに戻った時点で自動的に再接続します」 |
| ② **実際に効く助言（本画面の中心）** | 「リモート側で tmux を使うと、接続が切れてもシェルの状態はサーバー側に残ります。isekai-terminal は復帰時に同じ tmux ウィンドウへ自動的に戻ります」+ tmux 連携設定への導線 |
| ③ 現在の環境状態（事実の提示のみ） | 低電力モード: ON/OFF（`ProcessInfo.processInfo.isLowPowerModeEnabled`、`NSProcessInfoPowerStateDidChange` で更新）／通知許可: 許可済み/未許可（#4 と連動）／直近の復帰: 「12分前に3タブを復元しました」（§3.10.2-① の永続フィールドから、M3） |
| ④ 強制終了したときに何が違うか（**round-2 で差し替え、S5**） | 「アプリスイッチャーから上スワイプで終了すると、その時点で接続は即座に切れます。次回起動時にはタブ自体は復元されますが、必ずコールドスタートからの再接続になります（通常のバックグラウンド移行のように、短時間で戻ったときに接続が生きたままということはありません）」 |

- **round-1 のブロック④ は誤りだった（S5）**。「上スワイプで終了すると次回起動時の
  自動復元がされません」と書いていたが、§3.10.2-③ の設計では **force-quit でも
  レコードは残り、復元は発火する**（3つの終了要因を意図的に区別しない）。
  「Background App Refresh を勧めないのは誤誘導だから」という画面そのものに
  嘘を1文載せるのは自己矛盾なので、実際に起きること（即時切断 + 必ずコールドスタート）
  に差し替えた。
- **Background App Refresh に言及しないことを、この画面の明示的な設計制約とする**
  （将来この画面を編集する者への申し送りとしてソースコメントに残す）。

**(c) 復帰の可視化（legibility）— Rust API を1本追加する（B3）。**

前面復帰時に何が起きたかを、ターミナル上部の一過性バナーで明示する。3パターン:

| バナー | 単位 | 発火元 |
|---|---|---|
| 「復帰しました（接続は維持されています）」— 猶予内復帰で再接続不要だった | **タブ単位** | `on_foreground_resume(did_reconnect: false)` |
| **「再接続しています」**— 接続が切れており再接続を開始した | **タブ単位** | `on_foreground_resume(did_reconnect: true)` |
| 「前回のセッションを復元しました（3タブ）」— コールドスタートからの自動復元 | **アプリ単位（1回のみ）** | `TabRestoreCoordinator`（コールバックのファンアウト経由ではない） |

**round-1 はここで「判断は Rust が既に持っているので Swift は描画するだけ」と書いたが、
これは実装不能だった**（B3）。`BackgroundState` は private かつ「UniFFI へ公開しない」と
docに明記されており、さらに「猶予内復帰・接続生存」の分岐では
`notify_will_enter_foreground` は**コールバックを1つも発火しない**。
Swift 側で `ConnectionState` 遷移から推測するのは、ユーザー起動の再接続や
ネットワーク瞬断による再接続と競合するミラー状態機械であり、D-1 が禁じるものそのものである。

**決定**: `OrchestratorCallback` に **`on_foreground_resume(did_reconnect: bool)` を1本追加する**
（D-6 の1本目）。`notify_will_enter_foreground` の両分岐から、判断結果だけを1回発火する。
これにより:
- 判断は Rust に留まる（Swift は `did_reconnect` を描画に写すだけ）。
- 「再接続した」がユーザー起動なのか復帰起動なのかを Swift が推測せずに済む。
- 3パターン目（コールドスタート復元）は `TabRestoreCoordinator` が知っているので、
  この1本で3分岐すべてが賄える。

**代替案（採らなかった）**: 「猶予内復帰・接続生存」のバナーを諦めて2分岐にする案。
Rust 変更ゼロで済むが、(a) 「何も起きていない」ことを知る手段が無いので
再接続バナーを出すべきか判断できず、結局 `ConnectionState` の推測に戻る、
(b) B4 で結局 Rust API を1本足すことになっており、バッチに1本足す限界費用が小さい、
の2点から採らなかった。

**このコールバックの3つの意味論（round-3 で確定 — N2）。**
決めずに実装すると、「iOS では嘘をつかない」という §3.9 の唯一の存在理由を、
§3.9 自身が新設した文字列で破ることになる（NP4）。

**(a) 追跡対象でないタブでは発火しない。**
`notify_will_enter_foreground` は `TerminalTabsHostView.handleWillEnterForeground`
（`:108-113`）が**全タブに無条件でファンアウト**する。一度も接続していないタブや
既に切断済みのタブは `BackgroundState::Foreground` のままであり、
`orchestrator.rs:187-189` はこれを「バックグラウンド遷移がそもそも意味を持たない
（未接続・既に切断済み等）」と定義している。ここで `did_reconnect: false` を発火させると、
**切断されたターミナルの上に「復帰しました（接続は維持されています）」を描く**ことになる。
→ **入口で `background_state != Foreground` の場合のみ発火する。**

**(b) `did_reconnect: true` は「開始した」であって「成功した」ではない。**
再接続分岐は `(self.shared.reconnect_attempt)(...)` を呼ぶが、これは**同期的に失敗しうる**
（`orchestrator.rs:1305-1316` がまさにその場合を扱い、`phase` を `Idle` へ戻して
`Disconnected` を発火する）。したがって発火順序がどうであれ `true` の意味は
「再接続を開始した」に限られる。
→ **フラグの定義を「開始した」とし、文言を「再接続しました」（過去形・成功）から
「再接続しています」へ変更する。** 結果は既存の接続状態UIが表示する。
これは round-2 で新設した文字列が持っていた、S5 で潰したのと同型の嘘である。

**(c) N タブ → N 回のコールバック。**
ファンアウトにより1回の前面復帰で `on_foreground_resume` はタブ数だけ発火する。
タブ単位のバナーはそれでよいが、コールドスタート復元の「3タブを復元しました」は
**アプリ単位**であり、`TabRestoreCoordinator` が**1回だけ**発行する
（上表の「単位」列を参照）。コールバックのファンアウトから N 回出さない。

**(d) Logic側**: `ios/Sources/IsekaiTerminalCoreLogic/BackgroundBehaviorCopy.swift` —
「低電力モード ON/OFF × 通知許可の有無 × 直近復帰の有無」から ③④ の文言と
「設定を開く」ボタンの出し分けを決める純関数。`ProcessInfo` /
`UNUserNotificationCenter` の実問い合わせは `IsekaiTerminalCore` 側。

- **依存**: **#10（`TabRestoreCoordinator`、③の「直近の復帰」と (c) の3パターン目）** と
  **Y-R（`on_foreground_resume`）**。
- **受け入れ条件と検証**:
  - 🟢 文言決定の全組み合わせ。**Background App Refresh という文字列がどの分岐にも
    出現しないことをテストで固定する**（将来の編集で紛れ込むのを防ぐ）。
  - 🟢 ブロック④ の文言が「復元されません」と述べていないこと（S5 の再発防止）。
  - 🟢 **復帰バナーの文言も同じ 🟢 テストで固定する（round-3 追加 — NP4）**。
    S5 で追加した回帰テストは `BackgroundBehaviorView` のブロック文言だけを押さえており、
    round-2 で新設した**バナー**の文字列は無防備だった。少なくとも
    「`did_reconnect: true` の文言が過去形の成功（『再接続しました』）でないこと」を固定する。
  - 🟡（Rust側）`on_foreground_resume` が `background_state == Foreground` のときに
    発火しないこと（N2a）。`orchestrator.rs` のユニットテストで押さえる。
  - 🟠 `on_foreground_resume(did_reconnect:)` の両値でバナー文言が切り替わる。
    コールドスタート復元バナーが**タブ数に関係なく1回だけ**出る（N2c）。
  - 🔴 **手動**: 低電力モードの ON/OFF が画面上でライブに反映される。
    バックグラウンド→即復帰、バックグラウンド長時間→復帰、コールドスタート復元、
    の3シナリオで正しいバナーが出る。**未接続のタブを混ぜた状態で前面復帰し、
    そのタブに「接続は維持されています」が出ないこと**（N2a）。

#### 3.9.4 検討した代替案（#9）

| 案 | 内容 | 判断 |
|---|---|---|
| A | `decideBatteryGuidance` を iOS からも呼び、`is_ignoring_battery_optimizations` に `!isLowPowerModeEnabled` を写像 | **却下**。低電力モードは kill 要因ではない。意味の異なる値を同一フィールドに写像すると、Rust 側ポリシー変更時に iOS が静かに壊れる |
| B | iOS 専用の `decide_ios_background_guidance` を Rust に新設し nag を一元化 | **却下**。nag しないので判断すべきことが無い。※将来 nag を導入するなら必ず Rust 側に置く |
| C | 何も作らず gap を閉じる | **却下**。「Android なら繋がりっぱなしなのに iOS だと切れる」を必ず不審に思う。**説明の不在それ自体が「壊れている」という誤解とバグ報告を生む。説明することが機能である** |
| D | Live Activity で「バックグラウンドで維持中」と表示して安心させる | **却下（有害）**。実際には維持していないので嘘になる（§3.10.3） |
| **採用** | C の変種: nag をやめ「率直な説明 + tmux という実際に効く助言 + 復帰の可視化」に価値を寄せる | Android の**文言**ではなく**意図**を iOS の語彙で満たす（D-3） |

### 3.10 #10 マルチタブ時のバックグラウンド維持【要設計・Rust API 1本】

#### 3.10.1 目標の再定義

Android の `TerminalSessionService`（単一 FGS が全タブを束ね「Nセッション接続中」の
永続通知を出しながら無期限に生かす）と同じものは iOS に作れない。
**iOS 版の目標を「維持」ではなく「無損失な復帰」と定義する**（D-5）:

> アプリを前面に戻したとき、ユーザーが何もしなくても、閉じた覚えのないタブがすべて
> 元の並びで存在し、それぞれが元のシェル（tmux 利用時は元のウィンドウ）に戻っている。
> プロセスが jetsam で殺されていた場合も同じ。

#### 3.10.2 設計（6要素）

**① `TabRestoreStore`（新規、Swift、Logic + Core 分割）**

Android の `ReattachStateStore.kt` に相当するものが iOS に存在しない（§1.2）ため新設する。

- **保存先**: Application Support 配下の JSON ファイル（atomic write）。既存の
  `SshHostTrustStore.swift` が JSON ファイルストアの前例。GRDB を使わないのは、
  タブ集合という揮発的データにスキーマ管理が要らないため。
- **レコード**: `tabId` / `profileId` / `savedAtUnixSecs` / `isActive`。
- **ストア全体のメタデータ（M3 対応、round-2 で追加）**: `lastRestoreAtUnixSecs` /
  `lastRestoredTabCount`。§3.9.3 ブロック③ の「12分前に3タブを復元しました」に使う。
  **#9 は #10 より後に実装するため、この2フィールドを #10 の時点でスキーマに入れておく**
  （後から足すと、既に出荷したストア形式を開け直すことになる）。
- **書くタイミング（S6 対応、round-2 で追加）**:
  - タブの open / close 時（即時）。
  - **接続が `Connected` へ遷移するたび**（Android と同じ。
    `TerminalTabsViewModel.kt:650,852` の2箇所が `persistReattachRecord` を呼ぶ）。
  - `didEnterBackground` 時（猶予トークン内）。
  - **round-1 は Connected 遷移時の更新を欠いていた**。その欠落の帰結:
    2時間開きっぱなしで使っていたタブがフォアグラウンドのままクラッシュ/OOM すると、
    タイムスタンプが2時間前のままなので `reattachRecordIsFresh` が false を返し、
    **最も使い込んだセッションのあとで何も復元されない**。
- **消すタイミング**: ユーザーが明示的にタブを閉じたとき（そのレコードのみ）、
  全タブを閉じたとき（全消去）。

**② tmux ウィンドウの claim ガード（round-2 で新設 — B4。Rust API の2本目）**

`tmux_tab_locators` は **`profileId` が主キー**（1プロファイルにつき高々1タグ、
`ProfileDatabase.swift:509-513`）。したがって「tmux の紐付けは既存テーブルが持っているので
重複して持たない」だけでは、**同一プロファイルに N タブという #10 が存在する理由そのものの
ケース**で破綻する。

Android はこれを `tmuxClaimedProfileIds`（`ConcurrentHashMap.newKeySet<Long>()`、
コルーチン起動**前**に同期的に `add`）で排他している。その導入コメントは実際の事故を
記録している（`TerminalTabsViewModel.kt:1043-1047`）: `tmuxWindowLabel` は非同期 RPC 完了まで
書かれないので、同一プロファイルの2タブがほぼ同時に `connected` へ遷移すると
既存タブの有無チェックを両方すり抜け、`@isekai_ctl_sock` が
「永久に正しいウィンドウへ届かなくなる二次被害」が出た（実機検証 2026-07-27）。

iOS 側には claim に相当するものが**一つもない**（`grep -rn "claimed" ios/Sources` がゼロ）。
`maybeEnsureTmuxTabWindow()`（`TerminalSessionController.swift:758-773`）は
コントローラごとに無防備に発火する。§3.10.2-④ の逐次復元は、Android が踏んだ2タブ競合を
**コールドスタート時の N タブ競合**に拡大する。ユーザーから見ると症状は
「クリップボード同期が壊れた」であり、原因（復元）と結びつかない。

**決定: claim を Rust に置く。**

```rust
// rust-core/src/tmux_window_claim.rs（新規）
pub fn try_claim_tmux_window(profile_identity: String, owner_id: String) -> bool;
pub fn release_tmux_window_claim(profile_identity: String, owner_id: String);
```

プロセス全体で共有する `Mutex<HashMap<String, String>>`（profile_identity → owner_id）で
実装する。`release` は owner 一致時のみ解放する（遅れて来た非所有者の release が
他タブの claim を壊さないため）。

- **なぜ Swift ではなく Rust か（Q1 への部分回答）**: 「このプロファイルの tmux ウィンドウを
  どのセッションが所有しているか」は**セッション状態そのもの**であって表示順ではない。
  そして、Kotlin 側がこれを所有していたことが、まさに 2026-07-27 の事故の形である
  （Android は今も Kotlin 側に持っており、それは動いているので触らないが、
  iOS で同じ場所に置き直す理由にはならない）。D-1 の「Rust 専管」に追加した4項目目。
- **Android より単純にできる点**: owner_id を `tabId` にすれば、claim を持っているのは
  常にちょうど1タブなので、Android の「最後の1枚が閉じたときだけ解放する」refcount
  （`TerminalTabsViewModel.kt:666-670`）が要らない。
- **プロセス再起動をまたぐ stale claim は原理的に発生しない**（round-3 で明記）。
  レジストリはプロセス内の `Mutex<HashMap>` であり、jetsam・force-quit・クラッシュの
  いずれでもプロセスの死とともに丸ごと消える。#10 の「3つの終了要因を区別しない」
  という性質はここには影響しない——3つとも等しく map を空にするからである。

**claim の意味論（round-3 で確定 — N3）。** 以下4点は、決めずに実装すると
「破損を防ぐために足したガードが、回復を妨げるものになる」（NP3）。
(i)(ii) は **Android が既に踏んで修正済みの挙動**なので、再導出しないこと。

- **(i) 同一 owner による再 claim は冪等——`true` を返す。**
  `profile → その owner 自身` が既に入っている状態で `false` を返すと、
  再接続して `maybeEnsureTmuxTabWindow()` を再実行したタブが**自分自身の claim に
  ブロックされる**。しかも呼び出し側は日和見的で失敗を握り潰す
  （`TerminalSessionController.swift:769-771` は警告ログを出して return するだけ）ため、
  そのタブの tmux バインドはプロセスの残りの生涯にわたって黙って復活しない。
- **(ii) ensure RPC が失敗したら claim を解放する。**
  Android はこれを明示的に行っており、理由もコメントに書いてある
  （`TerminalTabsViewModel.kt:414-416`:「RPCが失敗した場合のみ解放して別タブに再挑戦の
  機会を残す」。実装は `:1080-1083` の `catch` 内の `tmuxClaimedProfileIds.remove`）。
  (i) と組み合わさると、**一度の一過性 RPC 失敗が、そのプロファイルの tmux バインドを
  プロセス生涯にわたって恒久的に無効化する**——しかも経路全体が日和見的なので何も表面化しない。
- **(iii) release はユーザー操作による close だけでなく、あらゆる teardown 経路で行う。**
  コントローラの `deinit`、中断された復元、close フローを通らずにタブを落とすエラー経路は
  すべて claim をリークさせる。**release をコントローラの teardown
  （`deinit` または明示的な `invalidate()`）に紐づけ**、ユーザー close はその呼び出し元の
  ひとつという位置づけにする。
- **(iv) テスト用のリセットフックを用意する。** プロセスグローバルな `Mutex<HashMap>` は、
  `cargo test --lib`（1プロセス共有）で §3.10.4 の 🟡 claim テスト同士が干渉する。
  CI は `cargo nextest`（テストごとにプロセス分離）なのでほぼ問題にならないが、
  ローカル/将来の実行形態のためにリセット手段を用意し、その旨をモジュールdocに書く。

- **解放後の引き継ぎ**: claim を持つタブが teardown したら、同じプロファイルの残タブが
  あればそのうち1つに対して `maybeEnsureTmuxTabWindow` を再実行する。

**③ コールドスタート検知の iOS 版（Android のマーカー方式の簡略化）**

Android は「新鮮なレコードあり **&&** clean-shutdown マーカー無し」で判定する。
iOS では `applicationWillTerminate` が信頼できない（§3.9.2-3）ので、**マーカー側を落とす**:

> **レコードが残っていること自体を「前回は明示的に終わらせていない」の証拠とする。**

明示的な終了（ユーザーがタブを閉じた/全タブを閉じた）ではレコードを消すので、
起動時にレコードが残っていれば jetsam・force-quit・クラッシュのいずれかである。
**この3つを区別しない**——区別しても取れる行動が同じ（新鮮なら自動復元する）だからである。
Android では区別が nag のトリガーだったが、iOS では nag しないと決めた（§3.9.3-a）ので
区別する動機自体が消える。**#9 と #10 の決定はここで1本に繋がる。**

- **Q7（round-1 の未解決論点）への回答**: 「アプリを普通に使っている最中に
  再起動しただけ」のケースとも区別が付かないが、その場合も復元が発火するだけで実害がない
  （ユーザーが望んでいたタブが開くだけ）。区別しないことを設計として引き受ける。

**④ 復元/再接続の順序（優先度付け）**

> **アクティブタブを最初に接続し、残りは接続完了（または失敗確定）ごとに逐次接続する。**

非アクティブタブは接続開始まで「復元待ち」プレースホルダで表示し、ユーザーがタップしたら
そのタブを列の先頭へ繰り上げる。

- **これを Swift に置く（Q1 の残り半分への回答）**: ②で「所有権」を Rust に移したことで、
  Swift に残るのは純粋に**提示順**——「どのタブから画面に出すか」——だけになった。
  これは `rust-ssot.md` の「UI表示に閉じた状態」に該当する。
  境界線を明記しておく: 将来「どのタブを諦めるか」「どのタブの再接続を打ち切るか」のような
  **セッション状態に依存する判断**が必要になったら、その時点で Rust に移す。
- **`always-connects.md` への配慮**: 逐次再接続とし、サーバー側に残った古い park セッションの
  立ち退きは既存の `hello_with_parked_preemption`（`ISEKAI_PIPE_DESIGN.md` §8 Epic N-4）に
  委ねる。iOS 側で明示的に何かを解放しようとしない。

**⑤ 猶予（`beginBackgroundTask`）の用途を限定する**

現状の実装（`TerminalTabsHostView.swift:81-99`）は維持しつつ、**猶予の用途を
「①の永続化 I/O のみ」に限定する**。接続の延命に使わない（30秒延ばしても体験は変わらず
電池を消費するだけ）。副次的な利点: 将来 iOS が猶予をさらに短縮しても、
数ミリ秒の I/O しか載せていないので影響を受けない。

**⑥ 復帰バナーとの接続**

`TabRestoreCoordinator` が「今回の起動がコールドスタート復元だったか / 何タブ復元したか」を
知っているので、§3.9.3(c) の3パターン目をそれが供給する。①のメタデータ
（`lastRestoreAtUnixSecs` / `lastRestoredTabCount`）もここで書く。

#### 3.10.3 「N件のセッション」永続アフォーダンスの扱い

**決定: v1 コアパリティには含めない。Live Activity を opt-in の拡張として後段に置く。**
`PLAN.md` Phase Y の既存判断（「Live Activities は v1 必須ではなく実験機能枠」）と整合する。
実装する場合の文言制約:

> Live Activity は「アプリを生かす」機能ではない。表示だけである。
> したがって「3セッション接続中」は**嘘**になる。表示するなら
> 「3タブ・復帰待機中 — タップで戻る」のように、実態に即し、かつタップで即座に戻れる
> という実利がある文言に限る。

#### 3.10.4 Logic / Core の分割

| 置き場所 | もの |
|---|---|
| `IsekaiTerminalCoreLogic/TabRestoreRecord.swift` | レコード+メタデータの型と JSON エンコード/デコード（`lastRestoreAtUnixSecs` / `lastRestoredTabCount` を含む、M3） |
| `IsekaiTerminalCoreLogic/TabRestorePlan.swift` | 「レコード配列 + 現在時刻 + Rust の鮮度判定結果」から「復元するタブの順序付きリスト」を組み立てる純関数。逐次接続順（アクティブ優先）とプレースホルダ表示の決定 |
| `IsekaiTerminalCore/TabRestoreStore.swift` | ファイル I/O（Application Support、atomic write） |
| `IsekaiTerminalCore/TabRestoreCoordinator.swift` | 起動時の復元駆動、claim の取得/解放、`TerminalTabsModel` との接続 |
| `IsekaiTerminalCore/TerminalTabsHostView.swift`（変更） | `handleDidEnterBackground` に永続化を追加。Connected 遷移でのタイムスタンプ更新（S6） |
| `rust-core/src/tmux_window_claim.rs`（新規） | プロセス全体の claim レジストリ（②、D-6 の2本目） |

- **依存**: **Y-R（`try_claim_tmux_window` / `release_tmux_window_claim`）**。
- **受け入れ条件と検証**（round-2 でターゲットを修正 — S3）:
  - 🟢 `TabRestorePlan` の境界ケース: 空、全期限切れ、一部期限切れ、アクティブ不明、
    プロファイル削除済み、`tabId` 重複。
  - 🟢 `TabRestoreRecord` の JSON ラウンドトリップ（メタデータ含む）。
  - 🟡（Rust側）`cargo test` で claim の排他: 同一 profile を2 owner が claim すると
    2人目が false、**同一 owner の再 claim は true（N3-i）**、owner 不一致の release が
    無視される、release 後に別 owner が claim できる。
  - 🟠 **`IsekaiTerminalAppUITests`**（`IsekaiTerminalAppTests` **ではない**）:
    `XCUIApplication.terminate()` → `.launch()` による復元シナリオ。
    3タブ開く → バックグラウンドへ → terminate → 再起動 → 3タブが元の並びで復元され、
    アクティブタブが最初に接続される。
    - **round-1 は `IsekaiTerminalAppTests` に置いていたが、それは in-process の
      ホストアプリ単体テストターゲットで、自分自身を terminate/relaunch できない**（S3）。
    - **限界の明記（S3 後半）**: `XCUIApplication.terminate()` は**graceful kill** であり、
      jetsam を再現しない。したがってこのテストが担保するのは
      「レコードが生き残り復元が発火する」半分だけである。**jetsam 経路は自動テストで
      カバーされない**——`PLAN.md` Phase Y の既存判断（メモリ圧迫は再現性が低いので
      `simctl terminate` / cold launch で代替）を踏襲し、この穴を認めたうえで進む。
  - 🟠 同一プロファイル2タブの復元で、claim を取るのはちょうど1つ。claim 保持タブを
    閉じるともう一方が引き継ぐ（B4 の再発防止線）。
  - 🟠 **ensure RPC を失敗させたあと、同じプロファイルで再試行が成功する**
    （N3-ii/NP3 の再発防止線。フェイクの resolver で RPC を1回失敗させる）。
  - 🔴 **手動**: 実機で3タブ（うち2つ同一プロファイル）を開き、バックグラウンド放置 →
    復帰。tmux ウィンドウの対応が壊れず、クリップボード同期（ctl-socket 経路）が
    引き続き動くこと。

#### 3.10.5 検討した代替案（#10）

| 案 | 内容 | 判断 |
|---|---|---|
| A | `BGAppRefreshTask` / `BGProcessingTask` で定期的に接続を維持 | **却下**。OS はスケジュールを保証せず、常時接続には原理的に使えない |
| B | `NEAppPushProvider`（Network Extension）で常駐 | **却下**。entitlement が MDM/キャリア用途向けで一般配布アプリに下りない。Phase Y で既に非推奨判断済み |
| C | Live Activity を v1 のコアパリティ要件にする | **却下**。アプリを生かさないので「維持」の代替にならない。文言を誤ると積極的に嘘をつく |
| D | サーバー側に「クライアント不在中も PTY を生かす」機能を新設 | **却下（既に存在するため）**。それは tmux そのもので、両OSで既に連携済み。**新規実装ゼロで最大の効果**なので、実装せず §3.9 の案内の中心に据える |
| E | タブごとに `beginBackgroundTask` を取って猶予を増やす | **却下**。猶予はプロセス単位の予算でトークンを増やしても総量は増えない。かつ ⑤ のとおり延命自体に価値がない |
| F | claim ガードを Swift 側（`TerminalTabsModel` の `Set<Int64>`）に置く（Android の直移植） | **却下**。動きはするが、これは 2026-07-27 の事故と同じ配置である。Rust に置く限界費用は関数2本で、Y-R のバッチに載る（②） |
| **採用** | ①永続化ストア（Connected 時も更新）+ ②Rust の claim ガード + ③レコード存在＝異常終了 + ④逐次・アクティブ優先 + ⑤猶予は永続化のみ + ⑥復帰バナーへの供給 | iOS の構造的制約を受け入れたうえで「戻った瞬間に元の作業に戻れている」を最大化する |

### 3.11 Y-P0 の前提作業（round-2 で新設 — S7, S10-Q5）

10項目のどれでもないが、複数項目が依存するため先に片付ける2件。

**(a) `SettingsView` の抽出（S7）**

現状、アプリ全体の設定は `ProfileListView.swift:55-58` に `@AppStorage` トグルとして
直接インラインされており、**独立した設定画面が存在しない**
（`ios/Sources/IsekaiTerminalCore/` に `SettingsView.swift` は無い）。
一方、本計画では次の4箇所がそこに着地する必要がある:

- #7 の自動信頼トグル（Y-P1）
- #3 のフォントインポート入口（Y-P4）
- #6-Outcome-3 の配列ピッカー（Y-P4）
- #9 の `BackgroundBehaviorView` 入口（Y-P3）

加えて §3.9.3(b) の「設定メニューから常時開ける」は、存在しない画面を前提にしていた。

**決定**: Y-P0 で `ios/Sources/IsekaiTerminalCore/SettingsView.swift` を抽出する。
既存4トグルの移設のみで、新機能は足さない（レビュー可能な小さな変更に留める）。
今やれば小さく、4項目が後から accrete するより安い。

**(b) GRDB マイグレーション予約レジストリ（S10-Q5、D-4 の代替）**

§D-4 のとおり `scripts/reserve-grdb-migration.sh` + `ios/migration_registry.toml` +
`grdb-migration-check.yml` を新設する。

- **依存**: なし。両方とも Y-P1 の前提。
- **受け入れ条件と検証**: 🟠 既存の設定トグルが移設後も動く（`ProfileListModelTests` /
  新規 `SettingsViewTests`）。レジストリ側は `grdb-migration-check.yml` 自体が、
  意図的に壊した版数で赤くなることを1回確認する。

---

## 4. 実装順序（Phasing）

### 4.1 順序と根拠（round-3 で確定 — N4, Q11。round-2 の P1/P5 反映を含む）

```
┌─ Y-R: Rust API チェックポイント【最初に単独でマージする】
│   on_foreground_resume(did_reconnect:)  … #9 が使う（B3）
│   try_claim_tmux_window / release_…     … #10 が使う（B4）
│   + Swift 3適合 + Kotlin 1実装の追従（D-6 表を参照。Android は no-op + log）
│   + バインディング7ファイルのコピー（Kotlin 1 + Swift 3 + sha256 3）
│   ※ regen はこの1回きり。他フェーズの worktree が在庫にない今のうちに通す
│
├─ Y-P0: 前提作業（小さい・全体を楽にする）※ Y-R と同時でも可
│   (a) SettingsView 抽出（S7）
│   (b) GRDB マイグレーション予約レジストリ（S10-Q5）
│
├─ Y-P1: 配線のみ（Rust変更ゼロ・スキーマ変更ゼロ＝最も自由に並列化できる）
│   #5 OSC133 プロンプトジャンプ ┐ 同一ファイルを触るので1作業単位
│   #2 AIパネルUI              ┘
│   #7 ホスト鍵自動信頼トグル
│   #8 Snippet テンプレートギャラリー
│
├─ Y-P2:  #10 マルチタブ復元（TabRestoreStore + claim + 逐次復元 + Coordinator）
├─ Y-P2b: #4b 経路B（NotifyGenerationTracker + TabAlertCopy + TabAlertNotifier）
│         ※ スキーマ変更ゼロ。計画中で価値/工数比が最大の単位
├─ Y-P2c: #4a 経路A（GRDB v7 + ensureWindow 実配線 + ProfileEditView トグル）
│
├─ Y-P3: #9 バックグラウンド信頼性の案内（#10 と Y-R に依存）
│
├─ Y-P4: 大きいが独立（並列可）
│   #3 カスタムフォント + グリフフォールバック
│   #1 trzsz プレビューUI（B-1 → B-2 → B-3）
│   #6 Phase B（スパイク結果に応じた Outcome-1/2/3 のいずれか）
│
├─ Y-P5: 仕上げ
│   PLAN.md Phase Y の更新（S9）
│   （任意）Live Activity opt-in
│
└─ 並行トラック（Y-R と同時に開始）
    #6 Phase A: 外部キーボード配列判定スパイク（実機質問、§3.7）
```

**round-2 からの順序変更と、その根拠（round-3）**:

1. **Y-R を先頭へ移した（N4 / NP1）**。round-2 は §4.1 の図が「Y-P1 と並行実行可」と
   注記する一方、§4.1-3 が「Y-P2 の前に完了させ」、§4.2-5 が「Y-R は並列化しない」と
   書いており、**自己矛盾していた**。しかも重要なのは「Y-R 自身を並列化しないこと」では
   なく「**Y-R が他の worktree を壊すこと**」である——`OrchestratorCallback` に
   メソッドを足した瞬間、在庫の全ブランチで Swift 3適合 + Kotlin 1実装が
   コンパイル不能になり（D-6 表）、そのなかには `ios-logic-linux-check` を落とすものが
   含まれる。何も変更していないブランチが、自分のテスト結果を見る前にリベースを
   強いられる。Y-P0 は `SettingsView` 抽出とマイグレーションレジストリだけで
   新APIに依存しないので、**Y-R を先に通しても失うものが無い**。
2. **Y-P2 を3分割した（Q11）**。round-2 は #10 と #4 を同じ Y-P2 に置いたが、
   両方が `TerminalSessionController.swift` を触る（round-2 の Q11 で自ら提起した問題）。
   - **#4 を Y-P1 へ混ぜる案は採らない**。Y-P1 が自由に並列化できるのは
     「Rust変更ゼロ・スキーマ変更ゼロ」だからであり、そこへ GRDB マイグレーションと
     `ProfileEditView` 変更と通知権限フローを持ち込むと、小さい競合問題を
     大きい競合問題に取り替えることになる。
   - **4b を 4a より先に置く**理由が3つある: (a) 4b は**スキーマ変更を一切必要としない**
     ——既に届いているデータの上の純粋な Swift 配線であり、マイグレーションの後ろに
     並ばせる理由が無い、(b) 4b が触るのは `TerminalSessionController.onScreenUpdate`
     周辺で、#10 が触る `TabRestoreCoordinator`/`TerminalTabsHostView` との競合は
     4a と #10 の競合より小さい、(c) claude-hookd 通知の 🔴 手動確認を早い段階で回せる
     ——配線の前提が間違っていた場合に立て直す予算が残る。
3. （round-2 で導入、維持）**#10 と #4 を前倒しした（P5 / P1）**。#9/#10 は
   「Android なら繋がったままなのに iOS は切れる」という、ユーザーを実際に離脱させる
   唯一の不満に応える項目であり、他の8項目にそれを後回しにする理由（依存もファイル競合も）
   が1つも無い。経路B（ctl `Notify`）は claude-hookd 経由で日常的に使われている通知で、
   現状 iOS では**データが届いていながら誰も読んでいない**。
4. （round-2 で導入、維持）**Y-P0 の新設**（S7, S10-Q5）。

### 4.2 並列 worktree 運用時の遵守事項

`parallel-worktree-agent-operations.md` に従う。本ADR固有の追加事項:

1. 各エージェントに「作業開始前に `git merge-base --is-ancestor <intended-base> HEAD` で
   ベースを確認せよ」を必ず指示する。
2. **競合ファイル（round-2 で2つ目を追加 — S7）**:
   - **`ios/Sources/IsekaiTerminalCore/TerminalSessionController.swift`** — #5/#2/#4/#1 が
     すべて触る（no-op コールバックの実装置換がここに集中している）。
     Y-P1 では #5+#2 を1作業単位にまとめる。Y-P2 / Y-P2b / Y-P2c を分けたのは、
     このファイルの競合を段階に散らすためでもある（Q11）。
   - **`ios/Sources/IsekaiTerminalCore/ProfileListView.swift` / `SettingsView.swift`** —
     #7/#3/#6/#9 の設定入口がすべて着地する。Y-P0 で `SettingsView` を抽出することで
     `ProfileListView` 側の競合は解消するが、`SettingsView` が新たな競合点になるので、
     同フェーズ内で複数項目が同時にトグルを足すことを避ける。
3. GRDB マイグレーションは Y-P0 の予約レジストリ経由で版数を取る（D-4 撤回により
   直列化は不要になったが、**予約は必須**）。
4. `scripts/link-worktree-artifacts.sh` を各worktreeに明示的に適用する
   （Agent tool の worktree isolation では post-checkout hook が発火しないことがある）。
5. **Y-R は単独で、かつ最初にマージする**（round-3 で修正 — N4/NP1）。単一worktreeで
   2本の API を足し、regen し、Swift 3適合 + Kotlin 1実装の追従まで含めて1PRでマージする。
   **「Y-R 自身を並列化しない」だけでは不十分**である——本当の危険は Y-R が
   *他の* worktree を壊すことなので、他フェーズの worktree が1つも在庫にない
   タイミング（＝計画の最初）に通す。
   - 万一 Y-R を後から回す羽目になった場合は、在庫の全 worktree に対して
     「リベースして D-6 表の4箇所へスタブを足す」ことを一斉に指示する必要がある。
     この作業をエージェントに任せると、iOS 系が required check でないために
     **壊れたまま main にマージされうる**点に注意する。

### 4.3 検証の全体方針（round-2 で強化 — S2, S3, S4, P4）

- **ローカルビルド/テストは行わない**。全検証はCI。
- **各項目の受け入れ条件は、それを実行するCIジョブ（🟢🟡🟠）または「手動」（🔴）を
  必ず明記する**（§3 の各項目で実施済み）。「受け入れ条件を書いたから検証されている」
  という錯覚（P4）を作らない。
- **🔴 の手動確認は成果物である**。「エージェントが動くと言った」を完了条件にしない
  ——このリポジトリには、エージェントがテスト通過を報告したがそれが事実でなかった
  記録がある。#1/#2/#6/#9/#10 は 🔴 を受け入れ条件から外さない。
- iOS 系ワークフローはどれも required check ではないため、**PRマージ前に
  `ios-logic-linux-check` / `ios-rust-core-check` / `ios-app-build-check` の green を
  目視確認する**運用を守る。
- **jetsam 経路は自動テストでカバーされない**（§3.10.4）。これは埋めない穴として
  引き受ける。

### 4.4 UniFFI 再生成の運用

D-6 に全文を記載した。要点の再掲:

- **追加する Rust API は2本のみ**（`on_foreground_resume`、tmux claim 2関数）。
- **Y-R で1回だけ regen する**。API ごとに回さない。
- **コピーするのは7ファイル**（Kotlin 1 + Swift 3 + `.sha256` 3）。
  Kotlin バインディングを忘れると `android-uniffi-drift`（required）が赤くなり、
  無関係な並列PRが全部止まる。
- **実装の追従は4箇所**（D-6 の表）: Swift 3適合
  （`TerminalSessionController` / `SshVerticalSliceRecorder` / `KeyManagerAuthRecorder`）+
  Kotlin 1実装（`TerminalSession.kt:235` の匿名オブジェクト）。
  **`FakeSshGateway.kt` は実装者ではない**（round-2 の記述を round-3 で訂正）。
  Swift 側の3つ目は `IsekaiTerminalCoreLogicTests` にあり、落とすと本ADRが第一ゲートに
  指定した `ios-logic-linux-check` が赤くなる。
- **Y-R は計画の先頭で単独マージする**（§4.1、N4/NP1）。
- Y-R 以降に3本目の API が必要になった場合は、まず「Swift に状態機械を作りかけていないか」を
  疑う。

---

## 5. Consequences

### 5.1 このADRがコミットするもの

1. **#1〜#8 は Android と機能的に同等になる**（見た目の一致は要求しない）。
   ただし #6 は、実機スパイクの結果次第で**手動オーバーライドのみへの縮退で「完了」とする**
   （§3.7 Outcome-3）。
2. **#9/#10 は Android の UI/文言を移植しない**。iOS の制約に即した別設計で、
   Android がその機能で解決していた困りごとを解決する。
3. **Rust API の追加は正確に2本**（`on_foreground_resume`、tmux claim）。
   3本目が必要になったら、それは Swift に状態機械を作りかけている兆候として扱う。
4. **「どのセッションがこのプロファイルの tmux ウィンドウを所有するか」は Rust が持つ**。
   これは Android 側（Kotlin 所有）との意図的な非対称であり、Android の 2026-07-27 の
   事故を iOS で繰り返さないための判断である。
   **Android 側の移行は「トリガー付き follow-up」として記録する（Q9 の確定回答）**:
   > **トリガー**: Kotlin 側の `tmuxClaimedProfileIds` が別のルーティング事故に
   > 関与したら、その時点で Rust の claim へ移行する。
   >
   > それまでは移行しない。claim レジストリはプロセスローカルで、両プラットフォームとも
   > 単一プロセスなので、2つの実装が相互作用することも食い違うこともない——コストは
   > 「同じ推論を2箇所で保つ」ことであって正しさではない。動いている Android コードを
   > 対称性のためだけに触るのは `parallel-worktree-agent-operations.md` が警告する
   > 種類の不要なリスクである。**期限のない TODO としてではなく、トリガー付きで書く**
   > ことで、これが恒久的な負い目として残らないようにする。
5. **プロファイル単位の UI opt-in と OS 権限確認は Swift 側に置いてよい**（D-1、
   Android の明文化された前例に従う）。**この2つで打ち止め**であり、3つ目の Swift 側
   gate を足すには本ADRの改訂を要する（m1）。
6. **新規ロジックは `IsekaiTerminalCoreLogic` に置き、Linux CI でテストする**。
7. **GRDB マイグレーションは予約レジストリ経由**（Y-P0 で新設）。
8. **各受け入れ条件は実行主体（CIジョブ名 or 手動）を明記する**。手動確認は成果物。
9. **`PLAN.md` Phase Y の更新を Y-P5 の成果物とする**（S9）。
10. **段階リリース可能な切り方を守る**（#1 は B-1 だけでリリース可能、#6 は縮退版で可）。
11. **Y-R を計画の先頭で単独マージする**（N4）。バインディング再生成はこの1回きりで、
    D-6 の表に挙げた Swift 3適合 + Kotlin 1実装の追従までを同一PRに含める。
12. **ユーザーに見せる文言で、実態より良く見せる方向の不正確さを許さない**。
    「Background App Refresh を勧めない」（§3.9.2）、「force-quit でも復元される」
    （§3.9.3 ブロック④）、「`did_reconnect` は開始であって成功ではない」（§3.9.3c）は
    すべて同じ1つの約束の帰結であり、🟢 の文言テストで固定する。

### 5.2 このADRが明示的にコミットしないもの

1. **Android と同等の無期限バックグラウンド接続維持**。iOS では実現不能。
2. **Background App Refresh / `BGAppRefreshTask` / `BGProcessingTask` /
   Network Extension による常時接続**。技術的に不可能または配布不能。
   **加えて、Background App Refresh をユーザーに勧めることもしない**（誤誘導になるため）。
3. **Live Activity を v1 のパリティ要件にすること**。opt-in の拡張として後段。
4. **物理 Wi-Fi/セルラー同時マルチパス**。既存の対象外判断を維持。
5. **jetsam / force-quit / クラッシュの区別**（§3.10.2-③）。区別しても取れる行動が同じ。
6. **jetsam 経路の自動テスト**（§3.10.4）。`XCUIApplication.terminate()` は graceful kill。
7. **AI 系 `NotifyKind`（経路B）のフォアグラウンド抑制**。Android と共通の既知差異として
   引き継ぐ。直すなら Rust 側で両OS同時に。
8. **Android 側の `tmuxClaimedProfileIds` の Rust への移行**。動いているものは触らない
   （トリガー付き follow-up、§5.1-4）。
9. **iOS 系 CI を required status check に昇格させること**。本ADRのスコープ外。
10. **Kitty graphics、alt-screen での wheel→矢印変換等**、両OSで既に won't-do のもの。
11. **UIの見た目を Android と一致させること**。iOS のヒューマンインターフェース慣行を優先。

### 5.3 リスク（round-3 更新）

| # | リスク | 影響 | 緩和 | 状態 |
|---|---|---|---|---|
| R1 | #3 のグリフフォールバックが毎フレーム Core Text を叩いて描画が破綻する | 端末描画が実用速度を割る | LRU キャッシュを設計に組み込み済み。**性能ゲートは閾値アサーションではなく「解決関数の呼び出し回数がユニークコードポイント数を超えない」という Logic 側の決定論的テスト**にする（§3.6.1、負荷起因の flaky を避ける） | round-2 で緩和策を差し替え |
| R2 | #1 が大きすぎて未完のまま放置される | 中途半端な `FilePreview/` が長期化 | B-1/B-2/B-3 の3段階、B-1 単体でリリース可能 | 継続 |
| R3 | #4 の経路B が実装されず、claude-hookd 通知が届かないまま「#4 完了」になる | **P1 の中核**。一部の通知（tmux）は届くので「抑制バグ」に見え、診断が遅れる | #4 を 4a/4b に分割。4b の受け入れ条件に「🔴 実機で `ctl notify --title X` が `プロファイル名: X` で届く」を含め、固定文言でないことを目視条件にする | round-2 で新設 |
| R4 | #4 の経路B が実装されたが `TabAlertCopy` が `message` を捨てて固定文言を出す | **沈黙より悪い**（動いているように見える） | `TabAlertCopy` のシグネチャを Android と同型にし、**`message` あり分岐のテストを 🟢 の受け入れ条件に明記** | round-2 で新設 |
| R5 | #10 の復元が同一プロファイル N タブで tmux ウィンドウ所有権を奪い合い、ctl-socket ルーティングが壊れる | **P3**。症状は「クリップボード同期が壊れた」に見え、原因（復元）と結びつかない。しかも corrupted routing は再起動を跨いで残る | Rust 側 claim ガード（§3.10.2-②）。受け入れ条件に「2タブ中ちょうど1つが claim / 閉じたら引き継ぐ」 | round-2 で新設 |
| R6 | Y-R を忘れる、または API ごとに regen して Kotlin 側を落とす | **P2**。`android-uniffi-drift`（required）が赤くなり、無関係な並列PRが全部止まる。復旧にもう1往復のCI | Y-R を独立フェーズとして Y-P2 の前に置く。7ファイル + Kotlin 実装追従を D-6 に明記 | round-2 で新設 |
| R7 | #10 の復元で全タブが一斉再接続し、サーバー側 fencing slot を食う | `always-connects.md` が警告する状態リークの新形態 | 逐次・アクティブ優先。park 立ち退きは `hello_with_parked_preemption` に委ねる | 継続 |
| R8 | 長時間フォアグラウンドで使ったタブが、クラッシュ後に「古すぎる」と判定されて復元されない | **最も使い込んだセッションの後で何も戻らない**という最悪のタイミングで失敗する | Connected 遷移時にもタイムスタンプを更新（§3.10.2-①、Android と同じ） | round-2 で新設 |
| R9 | #9 の案内画面に嘘が載る | 「誤誘導しない」ことが唯一の存在理由の画面で自己矛盾 | ブロック④ を差し替え済み（S5）。「Background App Refresh」という文字列が出ないことと、④が「復元されません」と言わないことを 🟢 のテストで固定 | round-2 で新設 |
| R10 | `TerminalSessionController.swift` / `SettingsView.swift` に変更が集中して並列作業が渋滞する | マージコンフリクトで並列化の利得が消える | §4.2 に2ファイルとも明記。同一フェーズ内で同ファイルを触る項目を同時に走らせない | round-2 で2つ目を追加 |
| R11 | 「受け入れ条件をすべて満たした」が、実際には誰も実行していないコードに対して宣言される | **P4**。まとめて後から実機で発覚する | 各条件に 🟢🟡🟠🔴 を付与済み。🔴 は成果物として扱う | round-2 で新設 |
| R12 | #6 のスパイクが「不可能」で終わる | gap #6 が未解決 | 3分岐（Outcome-1/2/3）を先に定義。縮退版でも実害（JIS で記号が打てない）は解消 | 継続・round-2 で中間ケース追加 |
| R13 | 復元テストが Simulator と実機で挙動が異なる／jetsam を再現できない | CI green でも実機で復元されない | graceful kill しかカバーできないことを明記（§3.10.4）。#10 完了時に実機確認を1回ユーザーへ依頼 | round-2 で限界を明示 |
| R14 | `PLAN.md` Phase Y がさらに古くなる | 次の読者が3つの文書を突き合わせて真実を再導出する羽目になる | §1.3 で supersede を明記。Y-P5 の成果物に Phase Y 更新を含める | round-2 で新設 |
| R15 | **Y-R がマージされた瞬間に他の全 worktree が赤くなる** | **NP1**。何も変更していないブランチが `ios-logic-linux-check`（最も安い第一ゲート）すら通せなくなり、各自リベース+スタブ追加を強いられる。iOS 系は required check でないので**壊れたまま main にマージされて放置されうる** | Y-R を計画の先頭で単独マージ（§4.1-1、§4.2-5）。壊れる4箇所を D-6 の表に列挙 | round-3 で新設 |
| R16 | **AI通知が動いていたのに、最初の再接続以降ずっと落ちる** | **NP2**。`NotifyGenerationTracker` を素朴な単調比較で実装すると、新しい論理セッションでカウンタが 0 に戻り、旧セッションの最高値を追い越すまで全通知が黙って落ちる。Logic テストも 🔴 手動確認も**新規セッションで実行するため両方 pass する**。間欠的で、P1 の中で最も診断が難しい形。しかも #10 が引き金（自動再接続）を常態化させる | `Connected` 遷移での同期リセットを**挙動として**仕様化（§3.5b、N1）。世代の巻き戻りも防御的にリセット扱い。受け入れ条件に「`Connected` 後の gen 1 が発火する」を明記 | round-3 で新設 |
| R17 | **claim ガードが、破損の防止装置から回復の妨害装置に変わる** | **NP3**。一過性の ensure RPC 失敗で claim が「ウィンドウを持たないタブ」に残り、同一 owner の再 claim が false・失敗時 release も無いと、**そのプロファイルの tmux バインドがプロセス生涯にわたって復活しない**。呼び出し側は日和見的で警告ログのみなので何も表面化せず、ユーザーには「クリップボード同期がいつのまにか壊れた」としか見えない | 同一 owner 再 claim は冪等、RPC 失敗時に release、teardown 全経路で release（§3.10.2-② の N3-i/ii/iii）。Android の `catch` ブロックが参照実装。受け入れ条件に「RPC を1回失敗させたあと再試行が成功する」 | round-3 で新設 |
| R18 | **「嘘をつかない画面」が round-2 で新しい嘘を2つ持ち込む** | **NP4**。未接続タブに「復帰しました（接続は維持されています）」（N2a）、同期的に失敗する直前に「再接続しました」（N2b）。S5 で追加した回帰テストは**古い嘘だけを固定していて新しい嘘を通す** | `background_state != Foreground` でのみ発火、フラグは「開始した」の意味・文言は「再接続しています」（§3.9.3c）。🟢 の文言テストをバナー文字列にも広げる | round-3 で新設 |

---

## 6. Open Questions（round-3 で決着済みのものと、意図的に残すもの）

### 6.1 決着したもの

| Q | 決着 | 反映先 |
|---|---|---|
| Q1 | **所有権 = Rust / 提示順 = Swift の切り分けで確定**。所有権を Rust に移したあとで Swift に残るのは本当に提示順だけになった。「タブ N を諦める」のようなセッション状態依存の判断が出たら Rust に移す、という境界も明記済み | §3.10.2-②/④ |
| Q2 | **nag しないで確定**。提案されていた弱い nag（低電力モード ON かつ復帰失敗継続）は因果的に無関係な2信号を結合するもので、`!isLowPowerModeEnabled` を `is_ignoring_battery_optimizations` に写像する案（§3.9.4-A で却下済み）と同じ欠陥になる。復帰失敗の繰り返し自体を表面化する価値があるなら、それ単独の理由で、判断は Rust に置いて行う | §3.9.3, §3.9.4 |
| Q3 | **プロファイル単位を維持**（GRDB v7 を受け入れる） | §3.5.0-1 |
| Q5 | **GRDB 予約レジストリを作る**（D-4 の直列化を撤回） | D-4, §3.11(b) |
| Q7 | **区別しないことを引き受ける**（復元して困ることが無い） | §3.10.2-③ |
| Q8 | **#10 を前倒しする**（round-2 で Y-P2 へ、round-3 で Y-P2 単独に） | §4.1 |
| Q9 | **Android の claim は移行しない。トリガー付き follow-up として記録**（「Kotlin の `Set` が別のルーティング事故に関与したら移行する」） | §5.1-4 |
| Q10 | **Y-R では Android は no-op + log のまま**。Y-R は required check を2本ゲートする唯一のPRなので外科的に保つ。Android 側での活用は別 follow-up | D-6-5 |
| Q11 | **Y-P2 を3分割**（#10 / #4b / #4a）。**#4 を Y-P1 に混ぜない**——Y-P1 が自由に並列化できる根拠は「Rust変更ゼロ・スキーマ変更ゼロ」であり、そこへマイグレーションを持ち込むと小さい競合を大きい競合に取り替えることになる | §4.1-2 |

### 6.2 意図的に残す（実装を止めない）

- **Q4**: Live Activity（§3.10.3）を本ADRのスコープに含めるか、別ADRに切るか。
  v1 スコープ外である点は確定しており、実際に着手する段になってから判断すればよい。
- **Q6**: §3.8 の #1 の3段階の切り方は、B-1 単体で本当に価値が出る切り方か。
  B-1 に着手して実物を見るまで確定できない性質の問い。B-1 完了時に見直す。

---

## 7. 参照

- `IOS_PARITY_GAP.md`（本ADRの入力となった gap 分析）
- `ADR_REVIEW_ROUND1.md` / `ADR_REVIEW_ROUND2.md`（敵対的レビュー2ラウンド。
  round-2 は blocking ゼロで収束と判定し、N1〜N4 の反映をもって完了とした）
- `PLAN.md` 「Phase Y: iOS対応」節（**Phase 1C #24 の `session_supervisor.rs` 記述は
  既に古い**、§1.3 参照）
- `.claude/rules/rust-ssot.md` / `always-connects.md` / `uniffi-binding-regeneration.md` /
  `parallel-worktree-agent-operations.md` / `worktree-artifact-sharing.md` /
  `main-branch-protection.md`
- `rust-core/src/background_reliability_policy.rs`（§3.9 で「使わない」と決めた Android 専用）
- `rust-core/src/reattach_persistence.rs`（§3.10 で再利用する中立な純関数）
- `rust-core/src/orchestrator.rs`（`BackgroundState` FSM [176-186]、
  `notify_will_enter_foreground` [1285-1310]、`notify_focus_change`、`on_notify` の抑制）
- `rust-core/src/lib.rs:1196-1206`（経路B の `notify_generation` / `notify_kind` /
  `notify_title` / `notify_body`）
- `android/src/main/kotlin/tools/isekai/terminal/TabAlertNotifier.kt`
  （D-1 の前例 [26-31]、`titleAndTextFor` と `message` 上書き [99-118]）
- `android/src/main/kotlin/tools/isekai/terminal/TerminalTabsViewModel.kt`
  （経路B の世代差分 [286-296]、`tmuxClaimedProfileIds` [418, 666-670, 1043-1065]、
  `persistReattachRecord` [570, 650, 852]）
- `android/src/main/kotlin/tools/isekai/terminal/session/ReattachStateStore.kt`
  （§3.10 の `TabRestoreStore` が対応する Android 実装）
- `ios/Sources/IsekaiTerminalCore/TerminalTabsHostView.swift`（既存のライフサイクル配線）
- `ios/Sources/IsekaiTerminalCoreLogic/TerminalScrollback.swift:47-50`
  （経路B のデータが既に到達しているが未消費）
- `ios/Sources/IsekaiTerminalCore/ProfileDatabase.swift:505-513`
  （`tmux_tab_locators` の `profileId` 主キー）
- `AI_INTEGRATION_DESIGN.md` §6.1 / §6.2 / §11.1.4（経路B の仕様、#2 の信頼境界、既知差異）
