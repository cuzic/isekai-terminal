# Y-R 実装タスクリスト: Rust API チェックポイント（UniFFI 破壊的変更バッチ）

**スコープ**: `ADR_IOS_PARITY_IMPLEMENTATION.md` D-6 / §4.1 の Y-R フェーズのみ。
`OrchestratorCallback` に `on_foreground_resume(did_reconnect: bool)` を追加し、新規
`rust-core/src/tmux_window_claim.rs` に `try_claim_tmux_window` /
`release_tmux_window_claim` を追加し、UniFFI バインディングを1回だけ再生成し、
それによって壊れる適合を追従させる（**レビューB1で補正: Rust内3箇所
（RecordingCallback/ForwardingOrchestratorCallback/FloodTestCallback）+
Swift 3箇所 + Kotlin 1箇所 = 計7箇所**、ADR D-6の表自体はSwift3+Kotlin1の
4箇所しか数えていないが本タスクリストではRust内3箇所を追加で明記する）。
**#9/#10 自体の
UI・ロジック実装（`BackgroundBehaviorView`、`TabRestoreStore`、`NotifyGenerationTracker`
等）は Y-R のスコープ外**（それぞれ Y-P3 / Y-P2）。Android 側は no-op + ログのみ
（ADR Q10）。この PR は **計画の先頭で単独マージする**（ADR §4.1-1, §4.2-5, N4/NP1）
——他フェーズの worktree が1つも在庫にないタイミングで通すこと。

---

## 0. ブランチ / worktree セットアップ

1. `main` の最新コミットから新しい worktree を作る（この時点で他フェーズの
   worktree が在庫に無いことを確認する——`git worktree list` で確認）。
   ```bash
   git worktree add .claude/worktrees/yr-orchestrator-callback -b feat/yr-orchestrator-foreground-resume-tmux-claim origin/main
   ```
2. `scripts/link-worktree-artifacts.sh <worktree-path>` を明示的に実行する
   （`.claude/rules/worktree-artifact-sharing.md`: Agent tool の worktree isolation
   経由では `post-checkout` hook が発火しないことがある）。
3. 作業開始前に `git merge-base --is-ancestor origin/main HEAD` でベースを確認する
   （`.claude/rules/parallel-worktree-agent-operations.md` 1番）。
4. **ローカルビルド/テストは一切行わない**（`prefer-gh-actions-over-local-cargo`）。
   全検証は GitHub Actions 経由。

---

## Task Group A（Rust）— 実装者1

### A-1. `OrchestratorCallback::on_foreground_resume` をトレイトへ追加

**ファイル**: `rust-core/src/lib.rs`

- トレイト定義は **`lib.rs:1439-1505`**（`#[uniffi::export(callback_interface)] pub
  trait OrchestratorCallback: Send + Sync { ... }`）。既存の最後のメソッドは
  `fn on_file_preview_result(&self, request_id: String, outcome:
  crate::file_preview::FilePreviewOutcome);`（1504行目）。この直後、`}`（1505行目）の
  前に以下を追加する:

  ```rust
  /// #9(iOS)/D-6: 前面復帰時にRustが下した「再接続を開始したか / 猶予内で
  /// 接続が生きていたか」の判断を、Swift/Kotlinが観測できるようにする
  /// (`orchestrator.rs::notify_will_enter_foreground`から発火)。
  /// `did_reconnect`は「再接続を開始した」であって「成功した」ではない
  /// (round-3 N2b、`ADR_IOS_PARITY_IMPLEMENTATION.md` §3.9.3c参照。再接続は
  /// `notify_will_enter_foreground`内で同期的に失敗しうる)。`background_state`が
  /// 既に`Foreground`だったタブでは発火しない(N2a、未接続/既切断タブへの
  /// 誤ったバナー表示を防ぐ)。呼び出し順序: `reconnect_attempt`の呼び出し
  /// (および同期失敗時の`on_connection_state_changed(Disconnected)`)より
  /// **前**に発火する(round-3 レビュー S1)——順序を逆にすると「Disconnected
  /// 直後に『再接続しています』」という矛盾した一過性表示になる。
  fn on_foreground_resume(&self, did_reconnect: bool);
  ```

  - トレイトにデフォルト実装を**付けない**こと（ADR D-6-5: 「UniFFI が生成する
    Kotlin `OrchestratorCallback` も全メソッドを abstract で宣言する」という
    破壊的変更の性質そのものが、Swift 3箇所 + Kotlin 1箇所の追従を必須にする
    設計意図）。

### A-2. `notify_will_enter_foreground` に発火ロジックを追加

**ファイル**: `rust-core/src/orchestrator.rs`

- 対象メソッドは **`orchestrator.rs:1285-1320`** の `pub fn
  notify_will_enter_foreground(&self)`。現状は次の2ブロック:
  1. `shared.state.lock()` 内で `app_foreground = true` を立て、`was_suspended =
     background_state == Suspended` を読み、`background_state = Foreground` に
     書き換え、`was_suspended` かつ他条件を満たせば `reconnect_with` に
     `last_connect_attempt.clone()` をセットする。
  2. ロック外で `reconnect_with` が `Some` なら `reconnect_attempt` を呼び、
     同期的に失敗したら `phase = Idle` に戻して `on_connection_state_changed
     (Disconnected)` を発火する。

- **変更**: ロックを取っている間に、**書き換え前**の `background_state` が
  `Foreground` だったかどうか（N2a）と、`Suspended` だったかどうか（`was_suspended`）
  の両方を保存する。**`did_reconnect` は `was_suspended` そのもので決める**
  （レビュー B2、確定）。`reconnect_with`（実際に `reconnect_attempt` へ渡す
  `Some`/`None`）とは**別物**として扱うこと——両者を混同するとN2b/R18の嘘を
  再導入する（下記「B2で確定した理由」参照）。

  実装イメージ（正確な変数名・整形は既存コードのスタイルに合わせて調整してよいが、
  以下の意味論は必須）:

  ```rust
  pub fn notify_will_enter_foreground(&self) {
      let (should_notify, did_reconnect, reconnect_with) = {
          let mut s = self.shared.state.lock();
          s.app_foreground = true;
          let was_foreground = s.background_state == BackgroundState::Foreground; // N2a
          let was_suspended = s.background_state == BackgroundState::Suspended;    // B2: did_reconnectの唯一の根拠
          s.background_state = BackgroundState::Foreground;
          let reconnect_with = if was_suspended && !s.reconnect_loop_active && s.phase != ConnPhase::Connecting {
              s.last_connect_attempt.clone()
          } else {
              None
          };
          (!was_foreground, was_suspended, reconnect_with)
      };
      // on_foreground_resumeは reconnect_attempt 呼び出し(同期失敗時の
      // on_connection_state_changed(Disconnected)発火を含む)より前に発火する(S1、確定)。
      // 末尾に置くと「Disconnected直後に『再接続しています』」という一過性の
      // 矛盾した順序になる(R18と同型)。
      if should_notify {
          self.shared.callback.on_foreground_resume(did_reconnect);
      }
      if let Some(attempt) = reconnect_with {
          match (self.shared.reconnect_attempt)(&self.shared, attempt) {
              Ok(()) => {}
              Err(e) => {
                  log::warn!("orchestrator: foreground resume reconnect failed synchronously: {e:?}");
                  let mut s = self.shared.state.lock();
                  s.phase = ConnPhase::Idle;
                  drop(s);
                  self.shared.callback.on_connection_state_changed(ConnectionPublicState::Disconnected {
                      reason: Some(format!("foreground resume reconnect failed: {e}")),
                      issue_hint: None,
                  });
              }
          }
      }
  }
  ```

  - **順序は確定（S1）**: `on_foreground_resume` は `reconnect_attempt` 呼び出しより
    **前**。A-1のトレイトdocコメントに「`reconnect_attempt`より前に発火する」を
    1行追加すること(Y-P3のSwift実装者が順序を仮定してよい根拠にする)。
  - **`should_notify`・`did_reconnect` の計算は共に、ロックを握っている間**に
    行うこと（`background_state` の読み取りと書き換えの間に別スレッドが
    割り込む余地を作らない）。

  **B2で確定した理由（`did_reconnect = was_suspended` であって
  `reconnect_with.is_some()` ではない）**: `reconnect_with` は
  `was_suspended && !s.reconnect_loop_active && s.phase != ConnPhase::Connecting`
  の3条件すべてを満たさないと `Some` にならない。つまり「接続は切れている
  （`Suspended`）が、既に別経路の再接続ループが回っている／別の接続試行が
  進行中」という状態では `was_suspended == true` なのに `reconnect_with ==
  None` になる。ADR §3.9.3c は `did_reconnect: false` を「復帰しました
  （接続は維持されています）」に対応させているため、`reconnect_with.is_some()`
  を使うと**接続が切れていて今まさに再接続中のタブに「接続は維持されています」
  と表示する**——ADRがround-3で名指しして潰したR18/NP4の嘘そのものが、
  N2aのガード（`background_state != Foreground`）をすり抜けて再発する
  （`Suspended`は`Foreground`ではないのでN2aは効かない）。`was_suspended`を
  使えば、この2状態でも`true`（＝「再接続しています」）となり実態と一致する。

### A-3. `OrchestratorCallback` の Rust 側実装3箇所すべてに追従（B1、必須）

**トレイトにデフォルト実装を付けない設計（A-1）である以上、`impl
OrchestratorCallback` は Rust 内に3箇所あり、全て埋めないと `rust-core-test-linux`
（required）がテストビルドの時点でコンパイルできない**（`cargo build`単体は
`ForwardingOrchestratorCallback`が`#[cfg(test)]`配下のため通るが、`cargo nextest
run --workspace`は通らない）:

| # | 実装 | ファイル:行 | 対応 |
|---|---|---|---|
| 1 | `RecordingCallback` | `rust-core/src/orchestrator.rs:1817`（構造体定義は`:1790`、implは`:1817`開始） | 記録用フィールド追加（下記手順1） |
| 2 | `ForwardingOrchestratorCallback` | `rust-core/src/test_callbacks.rs:65` | `fn on_foreground_resume(&self, _did_reconnect: bool) {}`（no-op） |
| 3 | `FloodTestCallback` | `rust-core/src/transport/ssh_handler.rs:1895` | `fn on_foreground_resume(&self, _did_reconnect: bool) {}`（no-op） |

- #2 は `rust-core/src/lib.rs:40-41` の `#[cfg(test)] pub(crate) mod
  test_callbacks;` 配下。`rust-core/src/transport/forward.rs:416,493,554,624` と
  `rust-core/src/transport/ssh_handler.rs:1282` から
  `ForwardingOrchestratorCallback as TestCallback` という別名で import・使用
  されている——「`TestCallback`で検索しても定義が見つからない」で迷わないこと。
- #1・#2・#3以外に実装が無いか、着手前に念のため
  `grep -rn "impl OrchestratorCallback" rust-core/src` で確認すること。

**ファイル**: `rust-core/src/orchestrator.rs`（`#[cfg(test)] mod tests`、末尾に追加）

1. **`RecordingCallback`（構造体定義 `:1790`、`impl OrchestratorCallback` `:1817`）に
   記録用フィールドとメソッドを追加**:
   ```rust
   foreground_resumes: StdMutex<Vec<bool>>,
   ```
   （他の `StdMutex<Vec<_>>` フィールドと同じスタイル）。**構造体は
   `#[derive(Default)]`（`:1789`）で、構築箇所は `:1907`/`:1989`/`:3182` の3箇所
   すべて `RecordingCallback::default()` を使っている（確認済み）ので、この
   フィールドを1行足すだけでよく、構築箇所を触る必要はない**。`impl
   OrchestratorCallback for RecordingCallback` に追加:
   ```rust
   fn on_foreground_resume(&self, did_reconnect: bool) {
       self.foreground_resumes.lock().unwrap().push(did_reconnect);
   }
   ```

2. **新規テスト**（既存の `notify_will_enter_foreground_*` 群、3067-3132行の隣に
   追加。ヘルパーは `orchestrator_connected_with_reconnect_policy(fast_test_policy())`
   （1986行目定義、`(orch, cb, attempt_count)` を返す）と `orchestrator_with_phase`
   （1938行目定義、`(orch, cb)` を返す）を再利用する）:

   - `notify_will_enter_foreground_within_budget_fires_on_foreground_resume_false`:
     `notify_did_enter_background` → `notify_will_enter_foreground` の後、
     `cb.foreground_resumes.lock().unwrap()` が `[false]` であること
     （猶予内復帰・再接続不要 = N2の1パターン目）。
   - `notify_will_enter_foreground_after_budget_expired_fires_on_foreground_resume_true`:
     `notify_did_enter_background` → `notify_background_budget_expired` →
     `notify_will_enter_foreground` の後、`cb.foreground_resumes` が `[true]`
     であること（猶予切れ・再接続開始 = N2の2パターン目）。
   - `notify_will_enter_foreground_is_noop_without_prior_backgrounding_does_not_fire_on_foreground_resume`:
     バックグラウンド化を一度も経ずに `notify_will_enter_foreground` を呼んだ場合
     （＝入口で `background_state == Foreground`）、`cb.foreground_resumes` が
     空であること（**N2a の直接的な再発防止線**）。既存の
     `notify_will_enter_foreground_is_noop_without_prior_backgrounding`
     （3126-3132行）に1アサーションを追加する形でもよいし、独立テストにしてもよい。
   - `notify_will_enter_foreground_fires_true_even_when_reconnect_attempt_fails_synchronously`:
     `reconnect_attempt` が同期的に `Err` を返すよう `OrchestratorShared` を構成する
     （`orchestrator.rs:1998` と `:3195` が `reconnect_attempt: Box::new(|shared,
     attempt| ...)` を直接構築している既存箇所、`:1986`
     `orchestrator_connected_with_reconnect_policy` がそのヘルパー形。これを
     流用/参考にする）、`on_connection_state_changed(Disconnected)` が飛ぶのと
     同時に `cb.foreground_resumes` が `[true]` であること
     （N2b: 「開始した」は同期失敗でも真であることの直接的な固定テスト）。
   - **`notify_will_enter_foreground_fires_true_when_reconnect_loop_already_active`
     （B2、必須・再発防止線）**: `notify_did_enter_background` →
     `notify_background_budget_expired` の後、`reconnect_loop_active = true` を
     直接立てる（`orchestrator.rs:3102` の既存テスト
     `notify_will_enter_foreground_does_not_double_trigger_when_reconnect_loop_already_active`
     と同じ状態を作る）→ `notify_will_enter_foreground` を呼ぶ。
     `attempt_count == 0`（再接続は実際には開始されない）かつ
     `cb.foreground_resumes == [true]`（`was_suspended`ベースなので発火する）
     であることを確認する。**この2つの条件が両立することが `did_reconnect =
     was_suspended` を選んだ理由そのもの**——`reconnect_with.is_some()`ベースだと
     このテストは`[false]`を主張してしまう（B2で却下した設計）。
   - **`notify_will_enter_foreground_fires_true_when_a_connect_is_already_in_flight`
     （B2、必須・再発防止線）**: 同様に `phase = ConnPhase::Connecting` を
     直接立てる（`orchestrator.rs:3117` の既存テスト
     `notify_will_enter_foreground_does_not_trigger_while_a_connect_is_already_in_flight`
     と同じ状態）→ `notify_will_enter_foreground` を呼ぶ。`attempt_count == 0`
     かつ `cb.foreground_resumes == [true]`。

   6テストとも 🟢 相当（`cargo test -p isekai-terminal-core --lib`、
   Linux CIで実行される Rust 側テスト）。

### A-4. `rust-core/src/tmux_window_claim.rs`（新規）

**シグネチャ**（ADR D-6 / §3.10.2-② に厳密に一致させる）:

```rust
pub fn try_claim_tmux_window(profile_identity: String, owner_id: String) -> bool;
pub fn release_tmux_window_claim(profile_identity: String, owner_id: String);
```

**スタイル precedent**: `rust-core/src/tmux_locator.rs:565-584` の
`TMUX_LOCATOR_REGISTRY`（`pub(crate) static TMUX_LOCATOR_REGISTRY:
LazyLock<Mutex<TmuxLocatorRegistry>> = LazyLock::new(...)`、`use
parking_lot::Mutex;`）が最も近い precedent（同じ「プロセス全体で共有する
`Mutex<HashMap>`」パターン、かつ同じ tmux 領域）。`pool.rs` の `SSH_POOL` も
同型だが `pool.rs` 自身のモジュール doc が「ここはSSH固有のプリミティブ専用」と
スコープを限定しているため、新しいファイルを立てる（`tmux_locator.rs` に相乗り
しない）。

**UniFFI free function precedent**: `rust-core/src/lib.rs:89-91`
（`#[uniffi::export] pub fn set_terminal_theme(...)`）と `lib.rs:108-111`
（`#[uniffi::export] pub fn core_version() -> String`）。free function を
`#[uniffi::export]` で公開するのに `lib.rs` 側の追加登録は不要
（`reattach_persistence.rs:63-76` が同じパターンの参照実装——`pub fn` に直接
`#[uniffi::export]` を付け、`lib.rs` 側は `pub mod reattach_persistence;` の
宣言だけで済んでいる）。

**内部実装**:

```rust
use std::collections::HashMap;
use std::sync::LazyLock;
use parking_lot::Mutex;

/// profile_identity → 現在の claim owner_id。
static TMUX_WINDOW_CLAIMS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[uniffi::export]
pub fn try_claim_tmux_window(profile_identity: String, owner_id: String) -> bool {
    let mut claims = TMUX_WINDOW_CLAIMS.lock();
    match claims.get(&profile_identity) {
        Some(existing) if existing == &owner_id => true, // N3-i: 同一ownerの再claimは冪等
        Some(_) => false,                                 // 別ownerが既にclaim中
        None => {
            claims.insert(profile_identity, owner_id);
            true
        }
    }
}

#[uniffi::export]
pub fn release_tmux_window_claim(profile_identity: String, owner_id: String) {
    let mut claims = TMUX_WINDOW_CLAIMS.lock();
    if claims.get(&profile_identity) == Some(&owner_id) {
        claims.remove(&profile_identity);
    }
    // owner不一致・未claimでのreleaseは無条件でno-op(N3-iii: あらゆるteardown
    // 経路からsafeに呼べる)。
}
```

**満たすべき意味論**（ADR N3、round-3で確定。実装者はこれを再導出しない）:

- **(i) 同一 owner の再 claim は冪等 → `true` を返す**。上記実装参照。
- **(ii) ensure RPC 失敗時の release は呼び出し側（Swift/Kotlin）の責務**。
  Rust 側のこの2関数自体は「呼ばれたら claim/release するだけ」のプリミティブで、
  「いつ release すべきか」の判断（RPC失敗時・teardown時等）は持たない
  （Swift 側は Y-P2 で `TabRestoreCoordinator` から呼ぶ、Y-R ではこの2関数を
  export するだけでよい）。
- **(iii) release は owner 不一致・未 claim でも安全に呼べる**（no-op）。
- **(iv) テスト用リセットフックを用意する**: `cargo nextest`（CI・required check
  `rust-core-test-linux`）はテストごとにプロセス分離されるため、以下は
  CI上は問題にならない。ADR N3-(iv) を文字通り満たすために用意する:
  ```rust
  #[cfg(test)]
  pub(crate) fn reset_for_test() {
      TMUX_WINDOW_CLAIMS.lock().clear();
  }
  ```
  **注意（S2、レビューで確定）**: 内部ヘルパー（`HashMap`を注入可能にした
  private関数）への切り出しは**不要**——2関数とも数行のプリミティブで、
  ADR N3が要求しているのは意味論(i)〜(iv)であって内部構造ではない
  (CLAUDE.md「タスクが要求する以上の抽象を足さない」)。ただし
  `reset_for_test()`は**並行実行下のテスト独立性を担保しない**——CIの
  `cargo nextest`はプロセス分離だが、CLAUDE.mdが案内する
  `cargo test -p isekai-terminal-core --lib`はローカル実行時1プロセス・
  マルチスレッドであり、各テストの冒頭で`reset_for_test()`を呼ぶと
  **リセットそのものが他テストのclaimを消して干渉源になる**。
  したがって**A-5の各テストはテストごとに一意な`profile_identity`文字列
  （例: `"try-claim-by-new-owner-succeeds"`のようにテスト名を含める）を
  使うことでテスト間の独立性を確保し、`reset_for_test()`には依存しない
  設計にする**。モジュール doc には「`reset_for_test()`単体では並行実行下の
  テスト独立性を保証しない、profile_identityをテストごとに一意にすること」
  と明記する。

**モジュール doc**: ファイル冒頭に「D-6 / §3.10.2-②の参照実装」「プロセス内
`Mutex<HashMap>`なのでプロセス再起動（jetsam/force-quit/クラッシュ）を跨いだ
stale claim は原理的に発生しない」（ADR該当箇所を要約）を記載する。

### A-5. `tmux_window_claim.rs` のユニットテスト

**S2確定事項: 全テストはグローバル static（公開API `try_claim_tmux_window`/
`release_tmux_window_claim`自体）に対して直接書く。テストごとに一意な
`profile_identity`（例: 下記関数名をそのまま文字列にする）を使い、
`reset_for_test()`には依存しない**（並行`cargo test --lib`実行下でも
テスト間で干渉しないため）。

- `try_claim_by_new_owner_succeeds`: 未 claim のプロファイルへの初回 claim が
  `true`。
- `try_claim_by_same_owner_is_idempotent`: owner A が claim 済みのプロファイルへ
  owner A が再度 claim → `true`（N3-i）。
- `try_claim_by_different_owner_fails`: owner A が claim 済みのプロファイルへ
  owner B が claim → `false`。
- `release_by_wrong_owner_is_ignored`: owner A が claim 中に owner B が release
  → claim は解除されない（owner A のままであること、または直後の owner B の
  再 claim が `false` のままであることで確認）。
- `release_by_correct_owner_frees_the_slot`: owner A が release した後、owner B
  が同じプロファイルを claim できる（`true`）。
- `release_when_not_claimed_is_a_safe_noop`: 一度も claim されていない
  プロファイルへの release がパニックしない（N3-iii）。
- （`#[cfg(test)] reset_for_test()`自体の動作確認、この1本のみ使用可）
  `reset_for_test_clears_all_claims`: 何かをclaimした後`reset_for_test()`を
  呼び、同じprofile_identityを別ownerがclaimできることを確認する（N3-iv）。

いずれも 🟢（`cargo test -p isekai-terminal-core --lib`）。

### A-6. `lib.rs` へのモジュール宣言追加

**ファイル**: `rust-core/src/lib.rs`

- `reattach_persistence.rs` の配線（46行目 `pub mod reattach_persistence;`）を
  precedent として、同じ並び（`pub(crate) mod` 群の並び、21-37行目付近）に
  `pub mod tmux_window_claim;` を追加する。`pub mod` にする理由:
  `try_claim_tmux_window`/`release_tmux_window_claim` を UniFFI から呼べるよう
  `#[uniffi::export]` を付けた関数を含むモジュールは、`reattach_persistence`
  同様 `pub mod` にする必要がある（`pub(crate) mod` の他の内部専用モジュールとは
  性質が違う）。
- `lib.rs` 側で `try_claim_tmux_window`/`release_tmux_window_claim` を
  re-export する必要は**無い**（`reattach_persistence::reattach_grace_window_secs`
  と同じく、UniFFI のスキャンはクレート内の `#[uniffi::export]` 付与箇所を
  モジュールパスに関わらず拾う。`uniffi::setup_scaffolding!()` マクロ——
  `lib.rs` 冒頭付近を確認——がクレート全体を対象にしている）。

---

## Task Group B（Swift + Kotlin 適合）— 実装者2（A と並行着手可、S3確定）

**S3で確定**: 4箇所のSwift/Kotlinシグネチャは UniFFI の命名規則で完全に決まって
おり、regen成果物を待つ必要はない。既存の生成物がその規則を実証している
（`on_host_key(&self, host: String, port: u16, fingerprint: String) -> bool`
→ Swift `onHostKey(host: String, port: UInt16, fingerprint: String) -> Bool`、
`on_file_preview_result(&self, request_id: String, outcome:
FilePreviewOutcome)` → Swift `onFilePreviewResult(requestId: String,
outcome: FilePreviewOutcome)` / Kotlin `onFilePreviewResult(requestId:
String, outcome: FilePreviewOutcome)`）。したがって
`on_foreground_resume(&self, did_reconnect: bool)` → Swift
`onForegroundResume(didReconnect: Bool)` / Kotlin `onForegroundResume(didReconnect:
Boolean)` は確定済み。**実装者2はAと並行して着手してよい**。ただし
**検証**はC（regen）完了後にしかできない——ローカルビルド禁止のため、実装者2は
自分の変更をCの前に一切コンパイル確認できない点は承知しておくこと。

### B-1. `TerminalSessionController.swift`（iOS App 側、log-only）

**ファイル**: `ios/Sources/IsekaiTerminalCore/TerminalSessionController.swift`

- クラス宣言は `public final class TerminalSessionController: OrchestratorCallback,
  @unchecked Sendable`（112行目）。適合メソッド群は `// MARK: - OrchestratorCallback`
  （712行目）以下、末尾は `onNotify(kind:)`（990行目）、クラスの閉じ括弧は
  991行目。
- 990-991行目の間（`onNotify` の直後、クラス閉じ括弧の前）に追加:

  ```swift
  // D-6(Y-R): 前面復帰時にRustが下した「再接続を開始したか/猶予内で接続が
  // 生きていたか」の判断。Y-Rではログのみ(バナー表示等の実UIはY-P3で実装、
  // `ADR_IOS_PARITY_IMPLEMENTATION.md` §3.9.3c参照)。didReconnect=trueは
  // 「開始した」であって「成功した」ではない(N2b)——結果は既存の
  // onConnectionStateChangedが伝える。
  public func onForegroundResume(didReconnect: Bool) {
      Self.logger.info("onForegroundResume: didReconnect=\(didReconnect, privacy: .public)")
  }
  ```

  - `Self.logger`（114行目で定義済みの `Logger(subsystem: "tools.isekai.terminal",
    category: "ssh")`）を使う。既存の `ensureTmuxTabWindow`（768行目、
    `Self.logger.info(..., privacy: .public)`）や port forward ログ
    （889-893行目）と同じ `Self.logger.info(...)` パターンに揃える
    （m1訂正: `onRebindStateChanged`（964-966行目）はロガーを使わず
    `Task { @MainActor in ... }` のみなので precedent として不適切）。
  - **UI 変更（バナー表示・`uiState` への反映）は入れない**。ADR Q10 相当の
    iOS 側規律（「Y-R は required check を2本ゲートする唯一の PR なので、
    機械的なバインディング変更に UX 変更を混ぜない」の精神を Swift 側にも適用
    ——Y-R の目的は「コンパイルを通す」ことであり、バナー UI は Y-P3 の
    スコープ）。

### B-2. `SshVerticalSliceTests.swift`（`SshVerticalSliceRecorder`）

**ファイル**: `ios/Tests/IsekaiTerminalCoreTests/SshVerticalSliceTests.swift`

- `private actor SshVerticalSliceRecorder: OrchestratorCallback`（61行目）。
  既存メソッド群は全て `nonisolated func on... {}` の no-op（70-110行目、
  唯一の例外は状態を実際に記録する `onData`/`onConnectionStateChanged`）。
  110行目 `nonisolated func onNotify(kind: NotifyKind) {}` の直後、閉じ括弧
  （111行目）の前に追加:

  ```swift
  nonisolated func onForegroundResume(didReconnect: Bool) {}
  ```

  他の未使用コールバック（`onScreenUpdate`/`onNoViablePath`等）と同じ
  no-op パターンに揃える。状態を記録する必要は無い（このテストスイートは
  Y-R の対象外機能を検証しない）。

### B-3. `KeyManagerTests.swift`（`KeyManagerAuthRecorder`）

**ファイル**: `ios/Tests/IsekaiTerminalCoreLogicTests/KeyManagerTests.swift`

- `private actor KeyManagerAuthRecorder: OrchestratorCallback`（73行目）。
  同じく全メソッド no-op（77-105行目）。105行目
  `nonisolated func onNotify(kind: NotifyKind) {}` の直後、閉じ括弧
  （106行目）の前に追加:

  ```swift
  nonisolated func onForegroundResume(didReconnect: Bool) {}
  ```

  **このファイルが `IsekaiTerminalCoreLogicTests` ターゲットに属し、
  `ios-logic-linux-check.yml`（Linux・`swift test`）で実行される**——ADRが
  「本ADRが第一ゲートに指定している最も安いジョブ」と呼ぶもの。この1箇所を
  見落とすと最初に赤くなる場所になる。

### B-4. `TerminalSession.kt`（Android、no-op + log のみ、ADR Q10）

**ファイル**: `android/src/main/kotlin/tools/isekai/terminal/session/TerminalSession.kt`

- 匿名オブジェクトは `private val callback = object : OrchestratorCallback { ... }`
  （235行目開始）。唯一の実装者（`FakeSshGateway.kt` は参照を保持するだけで
  実装者ではない、ADR N4 訂正済み）。既存メソッド群の末尾は
  `onFilePreviewResult`（386-388行目）、匿名オブジェクトの閉じ括弧は389行目。
- 388-389行目の間に追加:

  ```kotlin
  // D-6(Y-R): 前面復帰時のRustの判断(再接続を開始したか/猶予内で接続が
  // 生きていたか)。Q10: Y-RではAndroid側はログのみに留める(UX活用は
  // 別follow-up、ADR_IOS_PARITY_IMPLEMENTATION.md §5.1-4/D-6-5参照)。
  override fun onForegroundResume(didReconnect: Boolean) {
      RemoteLogger.i("IsekaiTerminalSSH", "onForegroundResume: didReconnect=$didReconnect")
  }
  ```

  - `RemoteLogger.i("IsekaiTerminalSSH", ...)` は同ファイル内の`:239`
    （`✓ connected`）や`:315-320`（`onForwardStateChanged`のListening/Stopped）
    と同じ`i`(info)レベルのログパターン（m2訂正:
    `onNoViablePath`（309-311行目）は`RemoteLogger.w`(warning)であり
    precedentとして不適切。`i`レベル自体を使う判断は妥当）。
  - `_state` の更新やUIへの反映は**しない**（Q10、UX変更は別 follow-up）。

### B-5. Android 側ユニットテスト（任意・軽量）

- `android/src/test` 配下に `TerminalSession` の既存テストがあれば、
  `onForegroundResume` がクラッシュせず呼べることを1行足す程度で十分
  （Q10 により UX 挙動が無いので、深いテストは書かない）。**必須ではない**
  ——`android-unit-test`（required）はコンパイルが通れば通常通りグリーンになる。

---

## Task Group C（UniFFI バインディング再生成）

**手順は `.claude/rules/uniffi-binding-regeneration.md` の通り実行する（再導出しない）**:

1. A（Rust 側変更、`OrchestratorCallback::on_foreground_resume` +
   `try_claim_tmux_window`/`release_tmux_window_claim`）がブランチにコミット
   済みであることを確認してから、以下を実行する:
   ```bash
   gh workflow run regenerate-uniffi-bindings.yml --ref <branch>
   ```
2. 完了を待ち、run ID を確認する:
   ```bash
   gh run list --workflow=regenerate-uniffi-bindings.yml --branch <branch>
   ```
3. artifact をダウンロードする（`gh run download` はリポジトリ内で実行しつつ
   `-D` で出力先を明示する）:
   ```bash
   gh run download <run-id> -D <destination-dir>
   ```
4. **コピーするのは正確に7ファイル**（本体だけコピーして `.sha256` を古いまま
   コミットすると `drift-check` が stale 判定で落ちる——2026-08-09 に実際に
   発生済み）:
   - Kotlin（1ファイル、`.sha256` サイドカーなし）:
     `android/src/main/kotlin/uniffi/isekai_terminal_core/isekai_terminal_core.kt`
   - Swift（3ファイル + 対応する `.sha256` 3ファイル、
     `ios/Sources/IsekaiTerminalCoreLogic/generated/` 配下）:
     - `isekai_terminal_core.swift` (+ `.sha256`)
     - `isekai_terminal_coreFFI.h` (+ `.sha256`)
     - `isekai_terminal_coreFFI.modulemap` (+ `.sha256`)
5. コピー後、`diff` で差分が意図した変更（`on_foreground_resume` /
   `try_claim_tmux_window` / `release_tmux_window_claim` の追加分のみ）である
   ことを確認してからコミットする。

**必須の注意点（ADR D-6 / 本ルールファイル共通）**:

- **`android-uniffi-drift` は `main` の required status check である**
  （`.claude/rules/main-branch-protection.md`）。Kotlin バインディングの
  コピー漏れ・`.sha256` の更新漏れは、この Y-R PR **自身**だけでなく、
  この PR がマージされた後に立ち上がる**無関係な並列 PR も全部止める**
  （§1.4「Android のコードが一行も関係しないのに main へのマージが全員分
  止まる」）。B（Swift/Kotlin 適合）のコミットより**前**に、この regen 手順で
  生成された生の diff を一度確認すること。
- **API ごとに regen を分けない**。`on_foreground_resume` と tmux claim 2関数を
  同じバッチで1回だけ regen する（D-6 運用ルール1番）。
- B1修正により、A（Rust側）は`on_foreground_resume`のRust内実装3箇所すべて
  （RecordingCallback/ForwardingOrchestratorCallback/FloodTestCallback）を
  含めて完了させてからCに進むこと（`rust-core-test-linux`がコンパイルできる
  状態でregenをトリガーする）。

---

## Task Group D（検証）

- **必須で green にする（required status check、`main-branch-protection.md`）**:
  - `android-unit-test`（`./gradlew :android:testDebugUnitTest`、B-4/B-5 が対象）
  - `rust-core-test-linux`（`cargo nextest run --workspace`、A-3/A-5 のテストが
    対象）
  - `android-uniffi-drift`（Task Group C の regen が正しく反映されているか）
  - `lockfile-drift`（`Cargo.lock` の整合性、新規クレート依存を増やしていない
    限り通常は無影響）
  - `room-migration`（このフェーズでは Room migration を触らないので無関係、
    念のため green を確認するだけでよい）
- **目視確認する（required ではないが ADR §4.3 の運用でマージ前に green を
  確認する）**:
  - `ios-logic-linux-check.yml`（`swift test`、Linux。B-3 の
    `KeyManagerAuthRecorder` 追従が直接ここで検証される。ADR §1.4 が
    「everything else が依存する第一ゲート」と位置づける最も安いジョブ）
  - `ios-rust-core-check.yml`（macOS、`xcodebuild test -scheme
    IsekaiTerminalCore-Package`。`IsekaiTerminalCoreTests` と
    `IsekaiTerminalCoreLogicTests` の両方——B-2/B-3 双方がここでも実行される）
  - （任意）`ios-app-build-check.yml`: B-1 の `TerminalSessionController`
    適合はここでもビルドされる。約31分かかり ADR は必須運用に含めていないが、
    Y-R は全 worktree を巻き込む破壊的変更なので走らせておくことを推奨する。
- **🔴 手動確認は無い**。Y-R は `IOS_PARITY_GAP.md` の番号付き gap 項目
  （#1〜#10）のいずれにも直接対応しない、純粋な API 追加 + 適合追従の
  「配線」フェーズである。ADR §3（項目別 Decision）にも Y-R 自身の節は無く、
  §3.9.3/§3.10.2-② が Y-R の成果物に**依存する**別フェーズ（Y-P3/Y-P2）として
  記述されている。したがって Y-R の受け入れ条件は**上記 CI green のみ**で
  完結する（Task Group D 冒頭の一覧が全て）。

---

## Task Group E（PR）

- **コミット規約**（`CLAUDE.md`）: `<type>: <日本語での説明>(該当する場合は
  「（Phase X-Y）」を付す)`。例:
  - `feat: OrchestratorCallbackにon_foreground_resumeを追加（ADR D-6 / Y-R）`
  - `feat: tmux_window_claimモジュールを新設しtry_claim/release_claimをUniFFI公開（ADR D-6 / Y-R）`
  - `chore: UniFFIバインディングを再生成（on_foreground_resume/tmux_window_claim分、Y-R）`
  - `fix: Swift/Kotlin側のOrchestratorCallback適合4箇所にonForegroundResumeを追従（Y-R）`
  - 直近の precedent: `857f6ae6 fix: ...（ADR D-1〜D-5, D-6一部）` が
    「（ADR ...）」タグ付けの実例。
  - **大きな機能はまとまった1コミットにせず、実際に組み上がった順序が追える
    よう細かく分ける**（CLAUDE.md）。推奨コミット順は A → C（regen） → B
    （S3: Bの**着手**はAと並行してよいが、コミット順としてはA→C→Bが自然
    ——A→C間のツリーは一時的にコンパイル不能な状態を経由するが、PR head
    のみがCI対象なので実害は無い。bisect時の混乱を避けるためこの順序メモを
    残す）。
- **必ず単独で、かつ最初にマージする**（ADR §4.1-1, §4.2-5, N4/NP1）。
  この PR がマージされる前に他フェーズ（Y-P0 含む）の worktree を新規に
  作らない。既に作ってしまっていた場合は、`.claude/rules/
  parallel-worktree-agent-operations.md` に従い、それらの worktree に対して
  「リベースして D-6 表の4箇所へスタブを足す」ことを一斉に指示する必要が
  あることを申し送る（ADR §4.2-5 末尾）。
- レビュー観点として、`android-uniffi-drift` の green を PR マージ前に
  必ず確認する（Task Group C 参照）。

---

## Open questions（`TASKS_IOS_ADR_YR_REVIEW.md`で決着済み）

初版の4つの未決事項は、Opusによるレビュー（`/home/cuzic/isekai-terminal/
TASKS_IOS_ADR_YR_REVIEW.md`、現ソース照合込み）で全て決着した。上記の
本文は決着後の内容に更新済み。決着内容の要約:

1. **発火位置**: `reconnect_attempt`呼び出しより**前**で確定（S1）。理由と
   docコメントはA-1/A-2に反映済み。
2. **`RecordingCallback`の`Default`経由自動初期化**: **される、確認済み**
   （構造体`:1789`が`#[derive(Default)]`、構築箇所`:1907`/`:1989`/`:3182`の
   3箇所全て`::default()`）。A-3に反映済み。
3. **内部ヘルパー切り出し**: **不要**。ただし`reset_for_test()`単体では
   `cargo test --lib`並行実行下のテスト独立性を保証しないため、テストごとに
   一意な`profile_identity`を使う設計に変更した（S2、A-4/A-5に反映済み）。
4. **Y-Rのスコープ（export のみで呼び出し配線は含まない）**: **正しい**。
   ADR §3.10.4がtmux_window_claim.rsを「依存: Y-R」として列挙し、実際の
   呼び出し配線はY-P2の🟠受け入れ条件側に置かれている。`#[uniffi::export]`
   付き`pub fn`は未使用でもdead_code警告を出さないため副作用も無い。

加えてレビューで新規に2件のblocking（B1: Rust側`OrchestratorCallback`実装
3箇所のうち2箇所が未記載でrequired check破壊、B2: `did_reconnect`の導出式が
ADR round-3が潰した嘘を再導入）が見つかり、A-2/A-3に反映済み。詳細は
レビューファイル本文を参照。
