# レビュー: `TASKS_IOS_ADR_YR.md`（Y-R 実装タスクリスト）

- **レビュー対象**: `TASKS_IOS_ADR_YR.md`（Y-R フェーズのみの実装タスクリスト）
- **照合先**: `ADR_IOS_PARITY_IMPLEMENTATION.md`（Accepted, round-3）D-6 / §3.9.3c / §3.10.2-② /
  §3.10.4 / §4.1 / §4.2-5 / §4.4、`.claude/rules/uniffi-binding-regeneration.md`、
  `.claude/rules/main-branch-protection.md`、および現ソースツリーの実読
- **方針**: タスクファイルの主張する行番号・引用は一切信用せず、全て該当ファイルを開いて確認した。
- **判定**: **2件の blocking を修正してから着手すること。** それ以外は概ね正確で、
  行アンカーはほぼ全て現ソースと一致していた（下記「確認できた事実」参照）。

---

## Blocking

### B1. `OrchestratorCallback` の Rust 側実装が3つあるのに、タスクリストは1つしか挙げていない → `rust-core-test-linux`（required）がコンパイルできない

タスクリスト A-3 は追従先として `RecordingCallback`（`rust-core/src/orchestrator.rs:1817`）
のみを指示している。しかし現ソースには `impl OrchestratorCallback` が**3箇所**ある:

| # | 実装 | ファイル:行 | 状態 |
|---|---|---|---|
| 1 | `RecordingCallback` | `rust-core/src/orchestrator.rs:1817` | タスクリストが挙げている ✓ |
| 2 | `ForwardingOrchestratorCallback` | `rust-core/src/test_callbacks.rs:65` | **未記載** |
| 3 | `FloodTestCallback` | `rust-core/src/transport/ssh_handler.rs:1895` | **未記載** |

- #2 は `rust-core/src/lib.rs:40-41` で `#[cfg(test)] pub(crate) mod test_callbacks;` と
  宣言されており、非テストビルドには含まれない（＝`regenerate-uniffi-bindings.yml` の
  `cargo build -p isekai-terminal-core` は通る）。**しかし `cargo nextest run --workspace`
  ＝ `rust-core-test-linux` は required status check である**
  （`.claude/rules/main-branch-protection.md` の表）。トレイトにデフォルト実装を付けない
  設計（A-1 が正しく指示している）である以上、この2つを埋めないと**テストビルドが
  コンパイルすら通らず、required check が赤で止まる**。
- #2 は `rust-core/src/transport/forward.rs:416,493,554,624` と
  `rust-core/src/transport/ssh_handler.rs:1282` から `TestCallback` という別名で
  import されて使われている（`ForwardingOrchestratorCallback as TestCallback`）。
  「`TestCallback` を探しても定義が見つからない」で実装者が迷う可能性があるので、
  タスクファイルには別名の事実も書いておくこと。
- **ADR 側も同じ穴を持っている**: D-6-5 の表と §4.4 の「実装の追従は4箇所」は Swift 3 +
  Kotlin 1 しか数えておらず、Rust 側のテストダブル3つを勘定に入れていない。
  タスクリストはこれをそのまま継承してしまっている。ADR は Accepted なので本レビューでは
  改訂を求めないが、**タスクリスト側で「Rust 内3箇所 + Swift 3 + Kotlin 1 = 計7箇所」と
  明記して補正する**こと（`android-uniffi-drift` と `rust-core-test-linux` の
  2本ゲートのうち、後者を落とすのはこの見落としである）。

**要求する修正**: A-3 の冒頭を「`OrchestratorCallback` の Rust 実装3箇所すべてに
`fn on_foreground_resume(&self, _did_reconnect: bool) {}` を足す（記録が要るのは
`RecordingCallback` だけ、他2つは no-op でよい）」に書き換える。

---

### B2. `did_reconnect = reconnect_with.is_some()` は、ADR が round-3 を丸ごと費やして潰した「嘘」（NP4 / R18）を再導入する

A-2 のコードスケッチ（タスクファイル 99行目）は

```rust
let did_reconnect = reconnect_with.is_some();
```

と規定し、「これが N2b の意味論であり必須」としている。しかし `reconnect_with` は
`orchestrator.rs:1287-1298` のとおり

```rust
let was_suspended = s.background_state == BackgroundState::Suspended;
s.background_state = BackgroundState::Foreground;
if was_suspended && !s.reconnect_loop_active && s.phase != ConnPhase::Connecting {
    s.last_connect_attempt.clone()
} else {
    None
}
```

で決まる。したがって **`background_state == Suspended`（＝接続は死んでいる前提）なのに
`reconnect_with == None` になる分岐が3つある**:

1. `s.reconnect_loop_active == true`（既に別経路の自動再接続ループが回っている）
2. `s.phase == ConnPhase::Connecting`（別の接続試行が進行中）
3. `s.last_connect_attempt == None`

この3つで A-2 のスケッチは `on_foreground_resume(false)` を発火する。ADR §3.9.3c の表は
`did_reconnect: false` を **「復帰しました（接続は維持されています）」** に対応させている。
つまり**接続が切れていて今まさに再接続中のタブに「接続は維持されています」と表示する**。
これは ADR がリスク表 R18 で

> 未接続タブに「復帰しました（接続は維持されています）」（N2a）…
> S5 で追加した回帰テストは**古い嘘だけを固定していて新しい嘘を通す**

と名指しした問題そのものであり、N2a のガード（`background_state != Foreground`）を
実装しても**素通りする**種類の嘘である（`Suspended` は `Foreground` ではないので
N2a のガードは発火を止めない）。

しかもこの1・2の状態は**空想ではなく、既に明示的なユニットテストが存在する**:

- `orchestrator.rs:3095-3109` `notify_will_enter_foreground_does_not_double_trigger_when_reconnect_loop_already_active`
  （`:3102` で `reconnect_loop_active = true` を立てている）
- `orchestrator.rs:3112-3124` `notify_will_enter_foreground_does_not_trigger_while_a_connect_is_already_in_flight`
  （`:3117` で `phase = ConnPhase::Connecting`）

A-3 が指示している4テストは、この2状態を**1本もカバーしていない**。つまり
「実装したとおりにテストも書く」ので、間違った意味論がそのまま緑で固定される。

**要求する修正**:

```rust
let did_reconnect = was_suspended;   // N2b: 「接続が切れていた」＝「再接続する側」
```

に変える。ADR §3.9.3c の分岐の定義は `false` ⟺「猶予内復帰（`Quiescing`）で再接続不要」、
`true` ⟺「接続が切れており再接続を開始した」であり、`Quiescing`/`Suspended` の二値が
そのまま対応する。`was_suspended` を使えば上記1・2でも `true`（＝「再接続しています」）
となり、実際に再接続ループ/接続試行が走っている事実と一致する。
N2b の「開始した ≠ 成功した」という性質も保たれる（むしろ強まる）。

加えて **A-3 に次の2テストを必須で追加**すること:

- `notify_will_enter_foreground_fires_true_when_reconnect_loop_already_active`
  （`:3102` と同じ状態を作り、`attempt_count == 0` かつ `foreground_resumes == [true]`）
- `notify_will_enter_foreground_fires_true_when_a_connect_is_already_in_flight`
  （`:3117` と同じ状態を作り、同上）

この2本が B2 の再発防止線であり、B2 を修正しないなら**この2本は必ず `[false]` を
主張することになる**ので、レビューでどちらの意味論を採るかを実装前に確定させること。

---

## Significant

### S1. Open question 1（発火順序）への回答: **`reconnect_attempt` の呼び出し前に発火する**と確定させる

タスクファイルは「ADR が規定していないので末尾に追加でよい」としているが、順序は
意味論には効かなくても**UI の見え方には効く**。`reconnect_attempt` が同期的に失敗する経路
（`orchestrator.rs:1305-1318`）では `on_connection_state_changed(Disconnected)` が発火する。
末尾に置くと Swift/Kotlin が受け取る順序は

```
onConnectionStateChanged(.disconnected)  →  onForegroundResume(didReconnect: true)
```

となり、**既に「切断」と表示された直後に「再接続しています」バナーを重ねる**。
これは B2 と同型の嘘（実態と食い違う一過性バナー）である。前に置けば

```
onForegroundResume(didReconnect: true)  →  onConnectionStateChanged(.disconnected)
```

で「再接続を始めた → 失敗した」という一貫した物語になり、`did_reconnect` の定義
（「開始した」）とも整合する。

**要求**: A-2 のスケッチを「ロック解放直後・`reconnect_attempt` 呼び出し前」に
`on_foreground_resume` を置く形に変え、その理由（上記）を**トレイトの doc コメントに
1行書く**（Y-P3 の Swift 実装者が順序を仮定してよい根拠になる。ADR §4.2-5 の
「壊れたまま main にマージされうる」体質下では、doc に書いてない前提は守られない）。

### S2. Open question 3 への回答 + `reset_for_test()` の並行安全性の穴

**注入可能な内部ヘルパー（`try_claim_in(&mut HashMap, ...)`）は不要**。2関数とも6行の
プリミティブであり、ADR N3 が要求しているのは意味論(i)〜(iv)であって内部構造ではない。
CLAUDE.md の「タスクが要求する以上の抽象を足さない」にも反する。A-4 のこの推奨は削除してよい。

ただし、その代わりに**タスクファイルが `reset_for_test()` で解決したと考えている問題は、
実は解決していない**:

- CI（`rust-core-test-linux` = `cargo nextest run --workspace`）はテストごとにプロセスが
  分離されるので問題ない、という記述は正しい。
- しかし CLAUDE.md 自身が案内し、タスクファイル A-5 が 🟢 の根拠として挙げている
  `cargo test -p isekai-terminal-core --lib` は**1プロセス・マルチスレッド**である。
  ここで `reset_for_test()` を各テストの冒頭で呼ぶと、**リセットそのものが他テストの
  claim を消す**——干渉を防ぐつもりの仕掛けが干渉源になる。

**要求**: A-5 のグローバル static 経由テストは、`reset_for_test()` に頼らず
**テストごとに一意な `profile_identity`**（例: `"yr-review-roundtrip"` のように
テスト名を含む文字列）を使う設計にする。`reset_for_test()` は ADR N3-(iv) の
文字通りの充足のために置いてよいが、モジュール doc には「並行実行下では
これ単体でテストを独立にはできない。キーを一意にすること」と明記する。

### S3. Task Group B は不必要に直列化されている（実装者2が待たされる）

タスクファイル 313-317行は「B は A の署名に依存するので A 完了後に着手」としているが、
**追従先4箇所の Swift/Kotlin シグネチャは UniFFI の命名規則で完全に決まっており、
regen 成果物を待つ必要はない**。既存の生成物がその規則を実証している:

- `on_host_key(&self, host: String, port: u16, fingerprint: String) -> bool`
  → `nonisolated func onHostKey(host: String, port: UInt16, fingerprint: String) -> Bool`
  （`ios/Tests/.../KeyManagerTests.swift:78`）
- `on_file_preview_result(&self, request_id: String, outcome: FilePreviewOutcome)`
  → `onFilePreviewResult(requestId: String, outcome: FilePreviewOutcome)`
  （同 `:105` 直前）／Kotlin `override fun onFilePreviewResult(requestId: String, outcome: FilePreviewOutcome)`
  （`TerminalSession.kt:386`）

したがって `on_foreground_resume(&self, did_reconnect: bool)` →
Swift `onForegroundResume(didReconnect: Bool)` / Kotlin
`override fun onForegroundResume(didReconnect: Boolean)` は確定している。

**要求**: B の前置き注意書きを「A と**並行して着手してよい**（シグネチャは UniFFI の
命名規則で確定済み、上記既存例を参照）。ただし**検証**は C（regen）完了後にしかできない
——ローカルビルド禁止のため、実装者2は自分の変更を C の前に一切コンパイル確認できない
点を承知しておく」に書き換える。これで2人の実装者が実際に並行できる。

なお、E の推奨コミット順 A → C → B のままだと C 時点のツリーはコンパイル不能になる。
PR head だけが CI 対象なので実害は無いが、その旨を1行添えておくと bisect 時の混乱を防げる。

---

## Minor

- **m1**: B-1 が「`Self.logger.info(...)` パターンは既存の `onRebindStateChanged`
  （964-966行目）…と同じ」としているが、`TerminalSessionController.swift:964-966` の
  `onRebindStateChanged` はロガーを一切使わず `Task { @MainActor in ... }` だけである。
  正しい precedent は同ファイル `:768`（`ensureTmuxTabWindow` の
  `Self.logger.info(... privacy: .public)`）と `:889-893`（port forward、これは正しく
  引用されている）。引用を差し替えること。
- **m2**: B-4 が「`RemoteLogger.i(...)` は `onNoViablePath`（309-311行目）等と同じパターン」
  としているが、`TerminalSession.kt:309-311` の `onNoViablePath` は
  `RemoteLogger.w`（warning）である。`RemoteLogger.i` の precedent は同ファイル
  `:239`（`✓ connected`）や `:315-320`（`onForwardStateChanged` の Listening/Stopped）。
  レベル `i` を使うこと自体は妥当なので、引用行だけ直せばよい。
- **m3**: A-3 のテスト4が「`reconnect_attempt` を差し替えているテストヘルパーの作り方を
  既存の失敗系テストから確認する」と実装者に探索を委ねているが、探索は不要。
  `orchestrator.rs:1998` と `:3195` が `reconnect_attempt: Box::new(|shared, attempt| ...)`
  を直接構築している既存箇所であり、`:1986` の
  `orchestrator_connected_with_reconnect_policy` がその形。この2つのアンカーを
  タスクファイルに書き込んでおくこと。
- **m4**: A-3-1 の「`RecordingCallback`（1790-1875行）」は、正確には構造体定義が `1790`
  開始・`impl OrchestratorCallback for RecordingCallback` が `1817` 開始・impl の閉じ括弧が
  `1875` である（構造体と impl を合わせた範囲としては正しい）。誤解の余地があるので
  「構造体 `1790`、impl `1817`」と分けて書くこと。

---

## Open question への回答（まとめ）

| # | タスクファイルの問い | 回答 |
|---|---|---|
| 1 | `on_foreground_resume` の発火位置 | **`reconnect_attempt` の前**に固定し、トレイト doc に明記する（S1）。ADR は順序を規定していないが、末尾に置くと「Disconnected の直後に『再接続しています』」という NP4 同型の嘘になる |
| 2 | `RecordingCallback` への フィールド追加が `Default` 経由で自動初期化されるか | **される。確認済み**。`orchestrator.rs:1789` が `#[derive(Default)]`、構築箇所は `:1907` `:1989` `:3182` の3つで全て `RecordingCallback::default()`。`StdMutex<Vec<bool>>` は `Default` を満たすので**構築箇所を1つも触る必要がない** |
| 3 | 内部ヘルパーへの切り出し | **不要**。ADR N3 が要求するのは意味論のみ。ただし `reset_for_test()` だけでは `cargo test --lib` 並行実行下の独立性を担保できないので、テストごとに一意な `profile_identity` を使うこと（S2） |
| 4 | 「export するだけ」が Y-R の正しいスコープか | **正しい**。ADR D-6 の表が free function 2本を Y-R の成果物とし、§3.10.4 の #10 のファイル表が `tmux_window_claim.rs` を **依存: Y-R** として列挙、実際の呼び出し配線（`TabRestoreCoordinator` からの claim/release、N3-(ii) の RPC 失敗時 release）は §3.10.2-② / §3.10.4 の 🟠 受け入れ条件として **Y-P2 側**に置かれている。タスクファイルは Y-P2 領域へはみ出していない。`#[uniffi::export]` 付きの `pub fn` は呼び出し元が無くても dead_code 警告を出さないので、ビルド上の副作用も無い |

---

## 確認できた事実（タスクファイルの主張が現ソースと一致していた点）

行アンカーの精度は高く、以下はすべて実読で一致を確認した。実装者は下記については
タスクファイルの記述をそのまま信用してよい。

- `lib.rs`: `#[uniffi::export(callback_interface)]` が `:1439`、`pub trait OrchestratorCallback`
  が `:1440`、最後のメソッド `on_file_preview_result` が `:1504`、閉じ括弧が `:1505` ✓
- `lib.rs:1` の `uniffi::setup_scaffolding!("isekai_terminal_core");`、`:46` の
  `pub mod reattach_persistence;` ✓（`pub mod` にする必要があるという A-6 の判断も正しい。
  `#[uniffi::export]` を持つモジュールは `pub mod`、内部専用は `pub(crate) mod` という
  使い分けが `lib.rs:3-49` 全体で一貫している）
- `reattach_persistence.rs:62-66` の `#[uniffi::export] pub fn reattach_grace_window_secs()`
  ——`lib.rs` 側に再 export が無いのに UniFFI に拾われている precedent ✓
- `lib.rs:89-91` `set_terminal_theme` / `:109-111` `core_version` の free function export ✓
- `tmux_locator.rs:583-584` の
  `pub(crate) static TMUX_LOCATOR_REGISTRY: LazyLock<Mutex<TmuxLocatorRegistry>>`、
  および `:65-69` の `use std::collections::HashMap; use std::sync::LazyLock;
  use parking_lot::Mutex;` ✓。`pool.rs` に相乗りしない理由付け（`pool.rs` のモジュール doc が
  スコープを限定している）も `tmux_locator.rs:563-577` のコメントが実際に同じ論法を
  記録しており、precedent として妥当 ✓
- `orchestrator.rs:1285-1320` の `notify_will_enter_foreground`、2ブロック構造の説明 ✓
- `orchestrator.rs:3066-3132` の `notify_will_enter_foreground_*` テスト群、
  `notify_will_enter_foreground_is_noop_without_prior_backgrounding` が `:3126-3132` ✓
- ヘルパー `orchestrator_with_phase` `:1938`（`(orch, cb)` を返す）/
  `orchestrator_connected_with_reconnect_policy` `:1986`（`(orch, cb, attempt_count)` を返す）✓
- `TerminalSessionController.swift`: クラス宣言 `:112`、`private static let logger` `:114`、
  `// MARK: - OrchestratorCallback` `:712`、`onNotify(kind:)` `:990`、クラス閉じ括弧 `:991` ✓
- `SshVerticalSliceTests.swift`: `private actor SshVerticalSliceRecorder: OrchestratorCallback` `:61`、
  `onNotify` `:110`、閉じ括弧 `:111` ✓
- `KeyManagerTests.swift`: `private actor KeyManagerAuthRecorder: OrchestratorCallback` `:73`、
  `onNotify` `:105`、閉じ括弧 `:106` ✓。`IsekaiTerminalCoreLogicTests` に属し
  `ios-logic-linux-check` が直撃するという指摘も ADR §1.4 の表と一致 ✓
- `TerminalSession.kt`: `private val callback = object : OrchestratorCallback {` `:235`、
  `onFilePreviewResult` `:386-388`、匿名オブジェクトの閉じ括弧 `:389` ✓
- `FakeSshGateway.kt`（`android/src/{test,androidTest}/.../:12`）は
  `var callback: OrchestratorCallback? = null` で**参照保持のみ・実装者ではない**
  ——ADR N4 の訂正どおり ✓（Kotlin 側の実装者は `TerminalSession.kt:235` の1箇所のみ）
- Task Group C の再生成手順は `.claude/rules/uniffi-binding-regeneration.md` と
  逐語的に一致。7ファイル（Kotlin 1 + Swift 3 + `.sha256` 3）という数え方も、
  `rust-core/scripts/generate-swift-bindings.sh:37`（`sha256sum "$f" | awk ... > "$f.sha256"`）と
  `regenerate-uniffi-bindings.yml` の2つの `upload-artifact`（Kotlin ディレクトリ /
  `ios/Sources/IsekaiTerminalCoreLogic/generated/` ディレクトリ）から正しい ✓
- required check 5本の識別（`android-unit-test` / `rust-core-test-linux` /
  `android-uniffi-drift` / `lockfile-drift` / `room-migration`）は
  `.claude/rules/main-branch-protection.md` の表と一致。`.sha256` 取りこぼしによる
  2026-08-09 の drift-check 失敗の引用も正確 ✓
- スコープ: Y-P2/Y-P3 側の成果物（`BackgroundBehaviorView` / `TabRestoreStore` /
  `NotifyGenerationTracker` / claim の呼び出し配線 / Android の `tmuxClaimedProfileIds` 移行）
  に一切はみ出していない。ADR §4.2-5 の「単独で最初にマージ」も E で正しく反映 ✓
- `try_claim_tmux_window` のスケッチの冪等判定は**順序が正しい**
  （`Some(existing) if existing == &owner_id => true` が `Some(_) => false` より先）。
  単一 `Mutex` 下の check-and-insert なので claim map に TOCTOU も無い ✓

---

## 判定

**着手前に 2件（B1・B2）の修正が必須。加えて S1〜S3 の反映を強く推奨。**

- **B1**（Rust 側実装3箇所のうち2箇所が未記載）は、放置すると required check
  `rust-core-test-linux` がコンパイルエラーで赤くなる——Y-R は「required を2本ゲートする
  唯一の PR」なので、ここで1往復無駄にするのは ADR §4.1-1 が避けようとしたコストそのもの。
- **B2**（`did_reconnect` の導出式）は、放置しても CI は緑になる。だからこそ危険で、
  ADR が round-3 を費やして潰した NP4/R18 の嘘が**間違った意味論のまま緑のテストで
  固定される**。Y-P3 の実装者はそのテストを正しい仕様だと信じる。

それ以外については、行アンカー・precedent の引用・CI リスクの識別・スコープ境界の
いずれも精度が高く、上記を直せばそのまま2名の実装者に渡してよい品質である。
