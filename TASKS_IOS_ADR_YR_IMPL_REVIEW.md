# レビュー: Y-R 実装（`feat/yr-orchestrator-foreground-resume-tmux-claim`、3コミット）

- **レビュー対象**: `git log main..feat/yr-orchestrator-foreground-resume-tmux-claim`
  （`bc2181ab` Rust / `d54224e9` UniFFI regen / `8e2b4b2a` Swift+Kotlin 適合）。
  worktree `/home/cuzic/isekai-terminal/.claude/worktrees/yr-orchestrator-callback` で実読。
- **照合先**: `ADR_IOS_PARITY_IMPLEMENTATION.md`（Accepted, round-3）D-6 / §3.9.3c の N2a–N2c /
  §3.10.2-② の N3-i〜iv / §3.10.4、`TASKS_IOS_ADR_YR.md` A-1〜A-6・B-1〜B-5・C・D・E、
  `TASKS_IOS_ADR_YR_REVIEW.md`（B1/B2）、`.claude/rules/uniffi-binding-regeneration.md`、
  `.claude/rules/main-branch-protection.md`。
- **方針**: コミットメッセージ・タスクリストの主張は一切信用せず、全て現ソースを開いて
  再導出した。`.sha256` は `sha256sum` で自分で再計算して照合した。
  `impl OrchestratorCallback` は自分で grep し直した。
- **判定**: **blocking なし。PR を開いてよい。** ただし significant 2件（S-1 は Y-P3 の
  前提として明示的に申し送る必要がある）と minor 5件。**CI シグナルはまだ1本も無い**（下記 §CI）。

---

## 追記: 本レビュー後に適用した修正（コミット`aec3ebf2`）

以下は本レビュー確定後、PRを開く前に追加で修正した。UniFFI公開シグネチャは
変更していないためバインディング再生成は不要:

- **S-1（`did_reconnect`が`Quiescing`中の切断を見落とす穴）を修正**。
  `did_reconnect = was_suspended || s.reconnect_loop_active || s.phase !=
  ConnPhase::Connected`に変更し(`orchestrator.rs:1300-1308`付近)、
  レビューが指摘した2ケース(`reconnect_loop_active`が既に`true`/`phase`が
  `Connected`でない)それぞれに回帰テストを追加した
  (`notify_will_enter_foreground_fires_true_when_quiescing_with_reconnect_loop_active`
  /
  `notify_will_enter_foreground_fires_true_when_quiescing_with_phase_not_connected`)。
- **S-2（発火順序を固定するテストが無い）を修正**。`RecordingCallback`に
  `event_order: StdMutex<Vec<&'static str>>`を追加し、
  `notify_will_enter_foreground_fires_true_even_when_reconnect_attempt_fails_synchronously`
  に`event_order == ["foreground_resume", "connection_state_changed"]`の
  アサーションを追加した(順序を末尾へ移動させると赤くなる)。
- **M-1（重複テスト）を修正**。
  `notify_will_enter_foreground_is_noop_without_prior_backgrounding_does_not_fire_on_foreground_resume`
  を削除し、元の`notify_will_enter_foreground_is_noop_without_prior_backgrounding`
  の追加アサーションのみを残した。
- M-2/M-3/M-4/M-5は対応せず、記録のみ(いずれもレビューが「任意」と明記)。

## 追記2: `/code-review 104`（独立レビュー、PR作成後）の指摘と対応

PR作成後、別セッションが`/code-review 104`を実行し2件の指摘が来た。いずれも
コード自体の修正はせず(理由は各項目に記載)、コメントで文書化するに留めた:

1. **`on_foreground_resume`のコールバック発火は`state`ロック解放後なので、
   別スレッドが発火させる無関係なイベント(例: 別経路の
   `notify_network_path_changed`→`handle_unexpected_disconnect`)が先に
   `on_connection_state_changed`を届けうる**。トレイトdoc(`lib.rs`)が保証する
   発火順序は「この呼び出し自身が同期的に引き起こす`reconnect_attempt`/
   その失敗時の`on_connection_state_changed`」との相対順序のみで、無関係な
   別スレッド発イベントとの順序は元々保証していない——ただし実際の誤解を
   招きうる指摘なので、`orchestrator.rs`の`on_foreground_resume`呼び出し
   直前にこの限界を明記するコメントを追加した(`:1309`付近)。**Y-Rの時点では
   Swift/Kotlin側はログのみ(実UIはY-P3)なのでこの窓は無害。Y-P3で実UIを
   配線する際、この限界(複数コールバックメソッド間でグローバルな発火順序は
   保証されない)を踏まえた設計にすること**——これは`on_foreground_resume`
   固有の問題ではなく、このオーケストレータのコールバック配送方式全体が
   持つ既存の特性であり、Y-Rの2API追加だけでは解消できない(解消するなら
   全コールバック呼び出しを単一のシリアルキュー/actorへ通す設計変更が要る、
   本ADRのスコープ外)。
2. **`did_reconnect`の`s.reconnect_loop_active`の項は、現在の実装だけを見れば
   `s.phase != ConnPhase::Connected`に包含され厳密には冗長**
   （`reconnect_loop_active`をtrueにする唯一の経路は必ずその前に
   `phase = Idle`を設定済み、`on_connected`は`phase = Connected`と
   `reconnect_loop_active = false`を同一ロック内で常に対にして設定する
   ——実装時に自分で該当箇所を再読して検証済み)。**意図的に削らなかった**:
   この式はまさに「復帰時点で本当に接続が生きていないか」を保証するための
   ものであり、将来どちらかの不変条件が崩れても(例: `phase`の更新漏れ)
   もう一方の条件が独立に安全側へ倒れる二重の保険として残す方が、
   B2/S-1が示した「暗黙の不変条件への依存が実害を生んだ」教訓に合う。
   `orchestrator.rs`に理由を明記するコメントを追加した。同レビューの
   別findingが正しく指摘したとおり、`phase != Connected`の項自体は
   `Connecting`中のバックグラウンド化→復帰という別の実経路をカバーする
   非冗長な項であり、こちらは削ってはいけない。

---

## Blocking

**なし。**

前回レビューの B1・B2 は両方とも実物に正しく落ちている（自分で再導出した結果、下記）:

- **B1**: `grep -rn "OrchestratorCallback for" --include=*.rs rust-core/` の結果は
  ちょうど3件（`orchestrator.rs:1823` RecordingCallback / `ssh_handler.rs:1895`
  FloodTestCallback / `test_callbacks.rs:65` ForwardingOrchestratorCallback）で、
  **3件とも埋まっている**。4件目の見落としは無い。`RecordingCallback` は
  `#[derive(Default)]`（`:1794`）配下に `foreground_resumes: StdMutex<Vec<bool>>`
  （`:1801`）を追加し、`:1881-1883` で実際に記録している（no-op ではない）。
  他2つは仕様どおり no-op（`test_callbacks.rs:92` / `ssh_handler.rs:1921`）。
- **B2**: `orchestrator.rs:1291-1297` を実読して制御フローを追った。
  ```
  1291  let was_foreground = s.background_state == BackgroundState::Foreground;
  1292  let was_suspended  = s.background_state == BackgroundState::Suspended;
  1293  s.background_state = BackgroundState::Foreground;
  1294  let reconnect_with = if was_suspended && !s.reconnect_loop_active && s.phase != ConnPhase::Connecting {…}
  1298  (!was_foreground, was_suspended, reconnect_with)
  ```
  `did_reconnect` はタプルの2要素目 = `was_suspended` そのもので、`reconnect_with`
  とは独立している。**B2 は本物に修正されている。**

付随して S1（発火順序）とロック保持も確認した:

- 発火は `:1300-1302`、`reconnect_attempt` の呼び出しは `:1303` の `if let Some(attempt)`。
  **コールバックは確実に `reconnect_attempt` より前**。同期失敗時の
  `on_connection_state_changed(Disconnected)`（`:1313-1316`）より前でもある（S1 充足）。
- `should_notify` / `did_reconnect` / `reconnect_with` の3つとも**同一のロックガード
  `s` を握っている間**（`:1288-1298` のブロック内）に確定しており、`background_state`
  の読みと書きの間に他スレッドが割り込む余地は無い。コールバック自体はガードの
  スコープを抜けた後（`:1300`）に呼ばれるので、フォアグラウンド側が再入しても
  デッドロックしない。**両方とも正しい。**

---

## Significant

### S-1. `did_reconnect = was_suspended` は B2 が名指しした2ケースを直したが、**猶予中（`Quiescing`）に接続が死んだケースでは同じ嘘が残っている**

`handle_unexpected_disconnect` の分岐（`orchestrator.rs:776-804`）を読むと、
`background_state` を `Foreground` へ戻すのは**自動再接続ループが始まらない2経路だけ**
（`:788` の `last_connect_attempt == None` と `:803` の else）。
**`Action::StartLoop`（`:783`）と `Action::Suppress`（`:777`）は
`background_state` に触れない**——これは #20 の設計として意図的である。

その結果、次のシーケンスが成立する:

1. タブが `Connected` → アプリがバックグラウンドへ（`background_state = Quiescing`、`:1254`）
2. **バックグラウンド滞在中に接続が落ちる** → `Action::StartLoop` →
   `reconnect_loop_active = true`、`phase = Idle`、`background_state` は **`Quiescing` のまま**
3. ユーザーが前面復帰 → `was_foreground == false`（発火する）、`was_suspended == false`
4. → **`on_foreground_resume(false)`** → Y-P3 の文言は
   **「復帰しました（接続は維持されています）」**

つまり「接続が切れていて今まさに再接続中のタブに『接続は維持されています』と表示する」——
ADR が round-3 で名指しして潰した **R18/NP4 の嘘そのもの**が、B2 が塞いだ
`Suspended` 軸ではなく `Quiescing` 軸から再発する。N2a のガード
（`background_state != Foreground`）も、`Quiescing` は `Foreground` ではないので効かない。

**Android では例外ではなく唯一の経路である。**
`grep -rn "notifyBackgroundBudgetExpired\|notifyMemoryWarning" android/src/main --include=*.kt`
のヒットは**生成バインディング（`uniffi/isekai_terminal_core.kt:3148,3182,3664,3728`）だけ**で、
アプリコードからの呼び出しは**ゼロ**（`notifyDidEnterBackground` /
`notifyWillEnterForeground` は `TerminalTabsViewModel.kt:610,616` から実際に呼ばれている）。
したがって Android では `background_state` が `Suspended` に到達する経路が存在せず、
`onForegroundResume` は**常に `didReconnect=false`** になる。
iOS でも `beginBackgroundTask` の猶予中（`TerminalTabsHostView.swift:97` →
`:103` の budget expired までの間）はプロセスが生きているので、その間の切断で同じ状態になる。

**影響と扱い**: Y-R は Android がログのみ・iOS も log-only（`TerminalSessionController.swift:997`）
なので、**現時点でユーザーに嘘は出ていない**。かつ修正は**内部の導出式だけ**なので
**バインディング再生成は不要**——Y-P3 に持ち越せる。しかし B2 の教訓は
「間違った意味論が緑のテストで固定されると、Y-P3 の実装者はそれを正しい仕様だと信じる」
だった。ここでも `notify_will_enter_foreground_within_budget_fires_on_foreground_resume_false`
（`:3090-3097`）が「`Quiescing` なら常に false」を緑で固定している。

**推奨**: 同一ロック下で
```rust
let did_reconnect = was_suspended || s.reconnect_loop_active || s.phase != ConnPhase::Connected;
```
に変えるか（`did_reconnect` の意味を「復帰時点で接続が生きていなかった」に統一する）、
最低限**この穴を `TASKS_IOS_ADR_YR.md` / ADR §3.9.3c に Y-P3 の前提として明記する**。
黙って継承させないこと。

### S-2. S1（発火順序）を固定するテストが1本も無い——順序を壊しても全テストが緑のまま通る

`RecordingCallback` は `connection_states`（`:1797`）と `foreground_resumes`（`:1801`）を
**独立した2本の `Vec`** に記録するので、両者の**相対順序は観測不能**である。

`notify_will_enter_foreground_fires_true_even_when_reconnect_attempt_fails_synchronously`
（`:3296-3309`）は `foreground_resumes == [true]` と「`Disconnected` がどこかに含まれる」
（`:3306-3309` の `events.iter().any(...)`）を別々に主張しているだけなので、
**`self.shared.callback.on_foreground_resume(...)` を関数末尾（`:1319` の後）へ移動しても
このテストは緑のまま通る**。それは A-1 のトレイトdoc（`lib.rs:1513-1516`）が
「`reconnect_attempt` より前に発火する」と明文で約束し、タスクリスト S1 が
「末尾に置くと『Disconnected 直後に再接続しています』という R18 と同型の一過性の矛盾になる」
と名指しで禁じた、まさにその regression である。

**推奨**（安価な順）:
1. `RecordingCallback` に `order: StdMutex<Vec<&'static str>>` を足し、
   `on_connection_state_changed` と `on_foreground_resume` の両方から push、
   上記テストで `assert_eq!(order, ["foreground_resume", "connection_state"])` を主張する。
2. あるいは全イベントを1本の enum Vec に集約する（既存テストへの影響が大きいので非推奨）。

これは blocking ではない（現コードの順序は正しい）が、S1 は Y-P3 の Swift 実装者が
「順序を仮定してよい根拠」として明記された契約なので、無防備なまま置くべきではない。

---

## Minor

### M-1. 同じことを主張する重複テストが2本入っている

`notify_will_enter_foreground_is_noop_without_prior_backgrounding`（`:3183-3189`）に
`assert!(cb.foreground_resumes.lock().unwrap().is_empty())`（`:3188`）を**追加**したうえで、
`notify_will_enter_foreground_is_noop_without_prior_backgrounding_does_not_fire_on_foreground_resume`
（`:3191-3197`）が**同一のセットアップで同一の主張**をしている。
タスクリスト A-2 は「既存テストに1アサーション追加する形でもよいし、独立テストにしてもよい」
と**択一**で提示していた（両方ではない）。どちらか1本に寄せる。

### M-2. `reset_for_test_clears_all_claims` だけが、自分のモジュールdocが立てた S2 の不変条件を破っている

`tmux_window_claim.rs:3-5` のモジュールdocは「`reset_for_test()` 単体では並行実行下の
テスト独立性を保証しないため、テストは `profile_identity` をテストごとに一意にする」と
宣言しており、他6本はその通りに書かれている。しかし `reset_for_test_clears_all_claims`
（`:97-105`）が呼ぶ `reset_for_test()`（`:37-40`）は **map 全体を `clear()` する**ので、
`cargo test --lib`（スレッド並列・単一プロセス＝ ADR N3-iv がこのフックを用意した理由そのもの
であり、CLAUDE.md が「ビルド・テスト」に載せているコマンド）では、
例えば `try_claim_by_different_owner_fails`（`:61-68`）の claim と assert の間に割り込んで
owner-b の claim を成功させ、**偽の失敗**を出しうる。

CI は `cargo nextest`（テストごとにプロセス分離）なので**赤くならない**し、この
リポジトリはローカルビルド禁止なので実害はほぼゼロ。ただし「独立性を担保するための
フックが、独立性を壊す唯一のテストになっている」のは記録に値する。
気になるならこの1本をモジュール内 `Mutex` で他6本と直列化するか、削る。

### M-3. トレイトdocの `did_reconnect` の定義と、実際の意味論がずれている

`lib.rs:1506-1516`（「開始した」は `:1509`） は `did_reconnect` を「**再接続を開始した**」と定義している。
しかし B2 で確定した2ケース（`reconnect_loop_active == true` / `phase == Connecting`）では、
**この呼び出しは何も開始しない**——既に別経路の再接続が進行中なだけである
（`:3142-3152` / `:3170-3180` のテストが `attempt_count == 0` かつ `[true]` を主張しているとおり）。
実際の意味は「復帰時点で接続が生きておらず、再接続中である」。
コミットメッセージ（`bc2181ab`）はこれを正しく書いているが、**Y-P3 の Swift 実装者が読むのは
トレイトdocの方**であり、「開始した」を素直に読むと ADR が S5/N2b で潰したはずの
過去形（「再接続を開始しました」）へ戻る誘因になる。1文の修正で済む。
（S-1 を採るなら定義ごと書き換わるので同時に直せる。）

### M-4. 2本の独立した API が1コミットに束ねられている

タスクリスト Task Group E は
`feat: OrchestratorCallbackにon_foreground_resumeを追加（ADR D-6 / Y-R）` と
`feat: tmux_window_claimモジュールを新設し…` の**2コミット**を例示しており、
CLAUDE.md も「大きな機能はまとまった1コミットにせず、実際に組み上がった順序が追えるよう
細かく分ける」と定めている。`bc2181ab` は両方を1コミットにしている。
コミットメッセージ本文は両方を正確に記述しており（type/日本語/`（ADR D-6 / Y-R）`タグとも
規約に適合）、実害は無い。以後の分割は任意。

### M-5. `ForwardingOrchestratorCallback` が no-op なので、このコールバックは e2e ハーネスから観測できない

`test_callbacks.rs:92` は no-op で、`OrchestratorTestEvent` に `ForegroundResume`
バリアントは追加されていない。タスクリスト A-3 の指示どおりなので Y-R としては正しいが、
Y-P2/Y-P3 でこの経路の e2e を書きたくなったら**このファイルをもう一度触ることになる**
（＝ UniFFI 再生成は不要だが `rust-core-test-linux` の再走は必要）点を申し送っておく。

---

## 確認できた事実（機械的に検証したもの）

### N3(i-iv) の逐条照合（`tmux_window_claim.rs`）

| ADR | 実装 | 判定 |
|---|---|---|
| (i) 同一 owner の再 claim は冪等（`true`） | `:20` `Some(existing) if existing == &owner_id => true` が `:21` `Some(_) => false` **より先**に置かれている | ✓ |
| (ii) ensure RPC 失敗時に release | Y-R のスコープ外（呼び出し配線は Y-P2）。`release_tmux_window_claim` が公開されており可能 | ✓（対象外） |
| (iii) claim されていないプロファイルへの release は安全な no-op / wrong-owner の release は無視 | `:32-34` `if claims.get(&profile_identity) == Some(&owner_id) { remove }`。未 claim なら `None != Some(&owner)` で何もしない | ✓ |
| (iv) テスト用リセットフック + モジュールdocへの明記 | `:37-40` `#[cfg(test)] pub(crate) fn reset_for_test()`、モジュールdoc `:3-5` に記載 | ✓（M-2 の留保付き） |

- **アトミック性**: `try_claim_tmux_window`（`:17-27`）は `:18` で `TMUX_WINDOW_CLAIMS.lock()` を
  **1回だけ**取り、その**同一ガードの生存中**に
  `get`（`:19`）と `insert`（`:23`）の両方を行い、ガードは関数末尾でしか drop されない。
  途中で `drop(claims)` も再ロックも無い。**check-and-insert は原子的で TOCTOU は無い。**
  `parking_lot::Mutex` なので poisoning も無い（`parking_lot = "0.12"` は
  `rust-core/Cargo.toml:65` に既存、新規依存の追加は無し ＝ `lockfile-drift` は無影響）。
  `LazyLock` は `pool.rs:224` に既存の precedent があるので新規の MSRV 要求ではない。
- テスト7本は A-5 の列挙と1対1で対応し、いずれも一意な `profile_identity`
  （関数名そのもの）を使っている。`release_by_wrong_owner_is_ignored`（`:69-78`）が
  「解除されていないこと」を**直後の owner-b の claim が `false`** で確認しているのは、
  内部状態を覗かずに公開 API だけで主張しており正しい。

### B2 回帰テスト2本が本当に B2 を捕まえるか

- `notify_will_enter_foreground_fires_true_when_reconnect_loop_already_active`（`:3141-3152`）:
  `background(30s)` → `budget_expired`（= `Suspended`）→ `reconnect_loop_active = true` を直接立てる
  → 前面復帰。`attempt_count == 0` **かつ** `foreground_resumes == [true]`。
  `reconnect_with` は3条件（`was_suspended && !reconnect_loop_active && phase != Connecting`）を
  満たさず `None` なので、**却下された `reconnect_with.is_some()` 実装ならここは `[false]` になり
  このテストは落ちる**。→ **本物の回帰防止線である。** 同型で
  `..._when_a_connect_is_already_in_flight`（`:3169-3180`、`phase = Connecting`）も同じく有効。
- N2a 側（`:3183-3197`）も、`should_notify` を `true` 固定にすれば落ちる。有効。
- 弱いテストは見当たらなかった（唯一の穴は S-2 の順序）。

### UniFFI regen（`d54224e9`）

- `git show d54224e9 | grep "^-"` の削除行は**4行のみ**:
  Kotlin の `@Structure.FieldOrder(...)` と `UniffiVTableCallbackInterfaceOrchestratorCallback(...)`
  コンストラクタ引数列（どちらも末尾に `onForegroundResume` が足された同一行の置換）、
  および `.sha256` 2本の旧ハッシュ。**それ以外は純粋に追加のみ。無関係な churn なし。**
- **`.sha256` を自分で再計算して照合した**（`.claude/rules/uniffi-binding-regeneration.md`
  が2回踏んだと記録しているクラス）:

  | ファイル | `sha256sum` 実測 | コミット済み `.sha256` | |
  |---|---|---|---|
  | `isekai_terminal_core.swift` | `e7de5e25…20bc1` | `e7de5e25…20bc1` | ✓ |
  | `isekai_terminal_coreFFI.h` | `39036579…c0b41` | `39036579…c0b41` | ✓ |
  | `isekai_terminal_coreFFI.modulemap` | `7a111182…d5408` | `7a111182…d5408` | ✓（内容差分なし、更新不要が正しい） |

- **`ios/Sources/IsekaiTerminalCoreFFILinux/` の取りこぼしを疑って確認した**
  （`ios-logic-linux-check.yml:114` の drift-check が
  `git diff --exit-code -- …/generated …/IsekaiTerminalCoreFFILinux` で**両方**を見るため）。
  結果、`IsekaiTerminalCoreFFILinux/isekai_terminal_coreFFI.h` は
  **mode 120000 のシンボリックリンク**（`git ls-files -s` で確認、実体は
  `../IsekaiTerminalCoreLogic/generated/isekai_terminal_coreFFI.h`）なので、
  更新漏れは原理的に起こらない。**問題なし。**
- `.h` の追加分は `Method18` typedef + vtable フィールド + 2つの free function 宣言 +
  3つの checksum 関数と、期待どおり整合している。

### Swift / Kotlin 適合4箇所（`8e2b4b2a`）— 生成物との文字単位の照合

生成された宣言:
- Swift `isekai_terminal_core.swift:7604`: `func onForegroundResume(didReconnect: Bool)`
  （`public protocol OrchestratorCallback: AnyObject, Sendable`（`:7485`）の最終メンバー）
- Kotlin `isekai_terminal_core.kt:8493`: `fun onForegroundResume(didReconnect: kotlin.Boolean)`

適合側:

| # | 箇所 | 実装 | 照合 |
|---|---|---|---|
| 1 | `ios/Sources/IsekaiTerminalCore/TerminalSessionController.swift:997` | `public func onForegroundResume(didReconnect: Bool)` | ラベル・型とも一致 ✓ |
| 2 | `ios/Tests/IsekaiTerminalCoreTests/SshVerticalSliceTests.swift:111` | `nonisolated func onForegroundResume(didReconnect: Bool) {}` | 同 actor 内の他スタブと同一パターン ✓ |
| 3 | `ios/Tests/IsekaiTerminalCoreLogicTests/KeyManagerTests.swift:106` | `nonisolated func onForegroundResume(didReconnect: Bool) {}` | ✓（`ios-logic-linux-check` の対象） |
| 4 | `android/.../session/TerminalSession.kt:393` | `override fun onForegroundResume(didReconnect: Boolean)` | 匿名オブジェクト（`:235` 開始）の内側、`FakeSshGateway.kt` は N4 の訂正どおり実装者ではない ✓ |

- #1 の `Self.logger`（`TerminalSessionController.swift:114`
  `private static let logger = Logger(subsystem:…)`、`import os` は `:3`）と
  `\(didReconnect, privacy: .public)` は `:768,889` 等の既存呼び出しと同じ形。
- #4 の `RemoteLogger.i(tag, msg)` は `util/RemoteLogger.kt:20` に存在し、
  `TerminalSession.kt:9` で import 済み、同ファイル `:239` 等に同型の呼び出しがある。
- **4箇所ともコンパイル可能に見える**（ただし実際に確認できるのは CI のみ、下記）。

### スコープ

`git diff main..<branch> --stat` は14ファイル。内訳は Rust 5（`lib.rs` /
`orchestrator.rs` / `test_callbacks.rs` / `tmux_window_claim.rs` 新規 / `ssh_handler.rs`）、
生成物5、適合4。**`tmux_window_claim` の呼び出し配線はゼロ**
（`grep -rn "tryClaimTmuxWindow\|releaseTmuxWindowClaim"` のヒットは生成バインディングのみ）、
`onForegroundResume` の実 UI 利用もゼロ（4箇所ともログ or no-op）。
Y-P2/Y-P3 側の成果物（`TabRestoreStore` / `BackgroundBehaviorCopy` / バナー /
Android の `tmuxClaimedProfileIds` 移行）へのはみ出しは**無い**。
`Cargo.toml`/`Cargo.lock`/`AppDatabase.kt` に触れていないので `lockfile-drift` /
`room-migration` は自明に緑になるはず。ADR/タスクリスト等の `.md` はこのリポジトリの
慣行どおり未追跡のままで、コミットには含まれていない。

---

## CI 状況（**シグナルはまだ存在しない**）

```
$ gh run list --branch feat/yr-orchestrator-foreground-resume-tmux-claim --limit 20
completed  success  Regenerate UniFFI bindings (on-demand)  workflow_dispatch  32574867739  11m32s
```

- **これは Task Group C の regen 実行そのもの**であり、検証ではない。
- `gh pr list --head <branch>` → **PR は未作成**。
- `git ls-remote origin <branch>` → `8e2b4b2a`（ローカル HEAD と一致、push 済み）。
- **required 5本 + iOS 2本のいずれも走っていない。** ワークフローの `on:` を確認したところ、
  `android-test-check.yml` / `rust-core-test-check.yml` / `android-uniffi-drift-check.yml` /
  `lockfile-drift-check.yml` / `room-migration-check.yml` / `ios-logic-linux-check.yml` /
  `ios-rust-core-check.yml` は**すべて `push: branches: [main]` に限定**されており、
  それ以外は `pull_request` トリガーのみ。したがって feature ブランチへの push では
  何も発火しない。**PR を開くのが最初のシグナルになる。**
- 本レビューの「コンパイル可能に見える」判断は**すべて目視照合であり、コンパイラによる
  検証ではない**（このリポジトリはローカルビルド禁止）。特に Swift 3箇所は
  `ios-logic-linux-check` / `ios-rust-core-check` が走るまで未検証である。
  ADR §4.3 のとおり、この2本は required ではないが**マージ前に目視で green を確認すること**。

### コミット衛生 / bisect

- 3コミットとも `<type>: <日本語>（… Y-R）` の規約に適合し、内容と一致している
  （`d54224e9` が「modulemap は内容差分なしのため対象外」と明記しているのは、
  上で再計算したハッシュと一致する正確な記述）。
- 中間コミット `bc2181ab`〜`d54224e9` のツリーは Swift/Kotlin 側がコンパイル不能だが、
  タスクリスト E の S3 addendum が予告済みで、**実害が無いことを再確認した**:
  上記のとおり全ワークフローが `main` への push か PR head しか対象にしないため、
  中間コミットが CI にかかる経路は存在しない。リポジトリ内に per-commit ビルドを行う
  ジョブや `git bisect` 用スクリプトも見当たらない。**この期待は今も成立している。**
- ベースは `git merge-base main HEAD == 6c4d1409 == main` の HEAD。
  `parallel-worktree-agent-operations.md` の「古いベースから分岐」問題は**該当しない**。

---

## 判定

**ready to PR as-is（blocking ゼロ）。ただし PR を開く前に S-1 の申し送りだけは行うこと。**

- **S-1** は Y-R のコードとしては正しく動くが、`did_reconnect` の意味論に
  B2 と同型の穴（`Quiescing` 中に接続が死ぬ経路。Android では**唯一**の経路）が
  残っており、しかも `:3090` のテストがそれを緑で固定している。修正はバインディング
  再生成を伴わないので Y-P3 で入れられるが、**黙って継承させると B2 で潰したはずの
  NP4 の嘘が Y-P3 のバナーとして出荷される**。PR 説明か `TASKS_IOS_ADR_YR.md` に
  明示的な申し送りを1段落入れてからマージすること。
- **S-2**（S1 順序の無防備さ）は次に `notify_will_enter_foreground` を触る人への保険。
  Y-R に含めても Y-P3 に回してもよい。
- minor 5件はいずれも任意。M-1（重複テスト）と M-3（docの1文）は
  この PR のうちに直すのが安い。

前回レビューの blocking 2件は**実物で修正が確認できた**（推測ではなく制御フローの
再導出と grep による再カウントで確認）。`.sha256` 3本は再計算して一致、
`IsekaiTerminalCoreFFILinux` の取りこぼしもシンボリックリンクであることを確認済み。
スコープの逸脱は無い。**このリポジトリが過去に踏んだ regen 系の地雷はすべて回避されている。**
