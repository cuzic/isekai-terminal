# Tasks: `ADR_MIDSESSION_DISCONNECT_RECOVERY.md`実装タスク一覧

- **入力**: `ADR_MIDSESSION_DISCONNECT_RECOVERY.md`（Accepted、round 4）
- **前提ルール**: `.claude/rules/prefer-gh-actions-over-local-cargo.md`
  （ローカル`cargo build`/`test`禁止、GitHub Actions経由で検証）、
  `.claude/rules/main-branch-protection.md`（各PRはrequired 5本green
  でマージ）、`.claude/rules/parallel-worktree-agent-operations.md`
  （worktree使用時のベースブランチ確認）
- **コミット規約**: `<type>: <日本語説明>（Epic R-N）`形式。
  Epic番号はPR1=R-1、PR2=R-2、PR3=R-3のprefixを使う。
- **改訂**: opus-task-reviewによるレビューでblocking 4件・
  significant 11件・minor 4件が発見され、本版で全件反映済み
  （タスク番号は一部振り直し）。

各タスクは独立にコミット可能な粒度を目安にしているが、コンパイルを
壊さない範囲でまとめて構わない。

---

## PR1: 独立した安全性の修正（ADR §2.1）

**マージ条件**: 新しいリトライ機構・新enum variant（`MidSessionDisconnect`）
・新マーカー型を一切含まない。`rust-core-test-linux`が green。

### Task 1.1 — `give_up`が`stdout`を明示的に閉じるのをやめる（B6、
    opus-task-reviewのS6/S7で実装案を訂正）

- **対象**: `rust-core/isekai-pipe/src/resume_loop.rs`の`give_up`関数
  （現状`:812-829`付近）。
- **変更**: `give_up`から`stdout.shutdown().await`の呼び出しを
  **削除する**（当初案だった「書き込み順序の入れ替え」は
  `give_up`が具象`&mut tokio::io::Stdout`を受け取り、
  `connect_command`側はそのハンドルを持たないため配管コストが
  高く、かつ`give_up`内でoutcome書き込みを行う代替案は
  `connect_command`の`Err`経路と同一`intent_id`への二重書き込みに
  なる——不採用）。プロセス終了時の自然なfd closeに任せることで、
  `connect_command`のoutcome書き込み（プロセス終了より必ず前に
  実行される）との順序が自動的に保証される。
- **doc更新**: `give_up`のdocコメント（「closes stdout so `ssh`
  treats this as a lost connection」）を「プロセス終了によって
  自然にstdoutが閉じるため明示的なshutdownは不要、かつoutcome
  書き込みより先に閉じてはならない」という趣旨に書き換える。
- **検証**: `give_up`が`stdout`を明示的に操作していないことを
  静的に保証する（`give_up`のシグネチャから`&mut Stdout`パラメータ
  自体を削除できるなら削除し、コンパイラに保証させる）。タイミング
  依存のテストは書かない。

### Task 1.2 — `ConnectOutcomeClass`のdeserialize耐性（S4、R1-S3）

- **対象**: `rust-core/isekai-pipe-core/src/outcome.rs`。
- **変更**:
  1. `ConnectOutcomeClass`に`#[serde(other)] Unknown`variantを追加。
  2. `wrapper.rs:618-619`の`claim_connect_outcome`呼び出し周辺で、
     deserialize失敗を`?`で invocation 全体へ伝播させず、
     ログを出して`Ok(None)`相当（＝`NoRecoverableSignal`と同じ
     結果になる）へ変換する。
- **検証（必須）**: `ConnectOutcome`は
  `#[serde(flatten)] pub class: ConnectOutcomeClass`（`outcome.rs:60-61`）
  なので、**`ConnectOutcomeClass`単体ではなく`ConnectOutcome`構造体
  全体を通したラウンドトリップテスト**を書く:
  1. 未知のタグ（例: `"class": "future-variant"`）を含むJSONを
     手で構築し、`ConnectOutcome`としてdeserializeできて
     `class == ConnectOutcomeClass::Unknown`になることを確認する。
  2. 動作しない場合は`class: String`フィールド+手動変換関数へ
     フォールバックする（この判断はCI結果で確定させ、ADRの
     再改訂は不要）。
- **副作業（R2-M1）**: `wrapper.rs:648-653`の`outcome_summary`
  （`ConnectOutcomeClass`の網羅match）に`Unknown`用のアームを追加。
  `Unreachable`用文言をそのまま使わず、専用の中立的文言
  （例: "isekai-pipe connect reported an outcome this isekai-ssh
  build doesn't recognize"）にする。

**PR1の受け入れ条件**: 上記2タスクのコミット後、既存の
`isekai-ssh/tests/*_e2e.rs`が引き続きpassすること
（新規テスト追加ではなくCIの既存ジョブでの確認——これ自体は
別タスクとして切り出さない、opus-task-reviewのS11）。

---

## PR2: リトライループ本体（ADR §2.2）

**マージ条件**: PR1がベース。`rust-core-test-linux`が green
（`rust-core-test-windows`はnon-requiredだが目視確認推奨）。

### Task 2.1 — `BreakReason`列挙型の導入（先行タスク、opus-task-review
    のM4で切り出し。B1・B2を同時に解消する最小単位）

- **対象**: `rust-core/isekai-ssh/src/native/mux/owner.rs`の
  `relay_loop`、`rust-core/isekai-ssh/src/native/connect.rs`の
  `run_shell_io_loop_inner`。両者から使われる共通概念だが、
  break地点の集合は異なるため、共通の列挙型定義＋各ファイルでの
  個別のマッピングという形にする。
- **変更**: 次の列挙型を定義する（置き場所は
  `isekai-ssh`クレート内の共有モジュール、または各ファイルに
  ローカル定義してもよい——実装時に決める）:

  ```rust
  enum BreakReason {
      RemoteExitReported, // ChannelMsg::ExitStatus または ExitSignal を受信
      ClientGone,         // ローカル側(このクライアント/このチャネル)が先に終わった
      TransportDead,      // 上記以外でChannel/Handleが閉じた
      CloseDeadline,      // ローカルEOF送信後、リモートがcloseを確認しなかった
  }
  ```

- **`owner.rs::relay_loop`側のマッピング**:
  - `ChannelMsg::ExitStatus`/`ChannelMsg::ExitSignal`受信 →
    `RemoteExitReported`（`ExitSignal`の場合は`exit_code`をssh(1)
    慣習に合わせ255扱いにする）。
  - `Some(Ok(None)) | None`（クライアント接続側が先に終わった、
    `:560`付近） → `ClientGone`。
  - ctl arm側の`write_frame(...)`失敗（2箇所） → `ClientGone`
    （このclientとの通信が切れただけで、holder全体は無関係）。
  - `Some(ChannelMsg::Close) | None`（チャネル側、`:583`付近） →
    `TransportDead`。
  - `shutdown_close_deadline`分岐（`:693-698`） → `CloseDeadline`。
- **`native/connect.rs::run_shell_io_loop_inner`側のマッピング**:
  - `ChannelMsg::ExitStatus`/`ExitSignal`受信 → `RemoteExitReported`。
  - `~.`エスケープ切断（`EscapeAction::Disconnect`、`:1136`付近） →
    `ClientGone`（ユーザー自身が切断しただけ）。
  - `channel.data(...)`書き込みエラー（`:1134`付近） →
    **`TransportDead`**（実装時に訂正、opus-code-review-pr2の指摘#8で
    ドキュメントとの不一致が発覚。単一プロセス経路ではローカル標準入力の
    読み取り自体は既に成功しており、`channel.data()`が書き込む先は
    *リモート*チャネルなので、その失敗はリモート側の異常であって
    `ClientGone`が指す「ローカル入力経路が死んだ」ケースではない——
    `owner.rs`の同種の書き込み失敗が`TransportDead`に分類されるのと
    一貫させた）。
  - それ以外の`Some(ChannelMsg::Close) | None`（`:1195`付近） →
    `TransportDead`。
- **検証**: 各break地点について、意図した`BreakReason`が実際に
  割り当てられることを確認する単体テスト（既存の
  `spawn_server`/`EofThenExitStatusHandler`等のテストハーネスに、
  `~.`切断・ctl write失敗・純粋なtransport切断のケースを追加）。

### Task 2.2 — Windows mux `owner.rs`の`Frame::Exit`抑制とレース対策
    （B3・R1-S1・S9、Task 2.1に依存）

- **対象**: `native/mux/owner.rs`。
- **変更**:
  1. Task 2.1の`BreakReason`を使い、`RemoteExitReported`の場合のみ
     `Frame::Exit(code)`を送る。`ClientGone`・`CloseDeadline`は
     従来通り`Frame::Exit`を送る（ADR §2.2.1参照）。`TransportDead`
     の場合のみ`Frame::Exit`を送らない。
  2. `TransportDead`の場合、`shutdown.notify_waiters()`を呼ぶ前に
     **共有handleの生存確認**（既存`handle_died`相当のチェック、
     `owner.rs:216-225`参照）を行い、handleが実際に死んでいる
     場合のみ`notify_waiters()`を呼ぶ（S9——チャネル1本の異常閉鎖
     だけでは他タブを巻き添えにしない）。handleが生きている場合は
     このclientにだけエラーを報告する。
- **検証**:
  - `ChannelMsg::ExitStatus`/`ExitSignal`受信時: 従来通り
    `Frame::Exit`が送られることを確認する既存テストのpass継続。
  - 純粋なtransport死（handle自体が死ぬケース）:
    `Frame::Exit`が送られず`notify_waiters()`が呼ばれ、
    `client.rs`の`OwnerLost`が実際に発火することを確認する統合
    テスト。
  - **チャネル1本だけの異常閉鎖（handleは生存）**: `notify_waiters()`
    が呼ばれず、他クライアントのチャネルに影響しないことを確認する
    テスト（S9の回帰防止、新規）。
  - `~.`エスケープ切断: `Frame::Exit`が送られ、自動再接続が
    **発生しない**ことを確認するテスト（B2の回帰防止）。
  - プロトコル（`Frame`列挙体）に変更が無いこと（コンパイル時に
    自明）。

### Task 2.3 — Windows単一プロセスfallbackの`Ok`経路対応（B1・S1・S2、
    Task 2.1に依存）

- **対象**: `native/connect.rs`。
- **変更**:
  1. Task 2.1の`BreakReason`を使い、`run_shell_io_loop_inner`が
     `TransportDead`の場合は`Ok(255)`へ握りつぶさず、
     `connect_attempt`まで伝播する`Err`として扱う（`RemoteExitReported`
     ・`ClientGone`は従来通り該当する終了コードで`Ok`を返す）。
  2. `drive_connect_recovery`（`:346-348`）は**この`Err`経路のみ**
     `claim_outcome`を呼ぶ（既存の1秒grace付き`Err`経路、
     `:447-462`）——「`Ok`経路でも`claim_outcome`を呼ぶ」という
     旧案は不要かつ誤り（opus-task-reviewのS1: 1で`Err`化すれば
     既存の`Err`経路がそのまま動くため、2は不要。1秒graceが効くのは
     `is_err()`のときだけ）。
- **検証**: `TransportDead`分類時に`claim_outcome`が呼ばれ
  `Err`経路の1秒grace内でoutcomeファイルを読めることを確認する
  テスト。`RemoteExitReported`/`ClientGone`分類時は従来通り
  `Ok`のまま`claim_outcome`が呼ばれないことも確認する。

### Task 2.4 — `MidSessionDisconnectSignal`マーカーとSTUN 2箇所への付与

- **対象**: `rust-core/isekai-pipe/src/connect.rs`
  （または`resume_loop.rs`——既存の`StaleTrustSignal`パターンに
  倣い置き場所を決める）。
- **変更**:
  1. `pub(crate) struct MidSessionDisconnectSignal;`（ゼロサイズ
     マーカー型）を定義。
  2. 以下の2箇所の`relay_stdio(...)`呼び出しに
     `.map_err(|e| e.context(MidSessionDisconnectSignal))`を挟む
     （**PR3実装後にこの2箇所は消滅し、STUN用`run_resume_loop`の
     `give_up`へ付け替えになる**——ADR §2.3-4・Task 3.3参照）:
     - `run_stun_p2p_with_fallback`（`resume_loop.rs:247`）
     - 単一candidate経路の`CandidateRoute::StunP2p`アーム
       （`connect.rs:680`）——**M1（opus-task-review）: この経路は
       確立成功後の`relay_stdio`直呼びであり、
       `recover_via_cross_family_fallback`は通らない
       （そちらを通るのはconnect自体が失敗した`Err`経路のみ）**。
  3. `run_relay_resumable`/`run_relay_resumable_with_fallback`が呼ぶ
     `run_resume_loop`の`give_up`には**付与しない**（relay経路は
     resume失敗＝terminalとして扱う、S1(旧)）。
- **検証**: `err.downcast_ref::<isekai_transport::StaleTrustSignal>()`
  と同じ**裸の**downcastパターンで検出できることを確認する単体
  テスト（B2、`err.chain()`は不要）。

### Task 2.5 — `ConnectOutcomeClass::MidSessionDisconnect`と分類ロジック

- **対象**: `outcome.rs`（Task 1.2のUnknown追加と同じ場所）、
  `connect.rs`の`write_connect_outcome_for_wrapper`。
- **変更**:
  1. `ConnectOutcomeClass`に`MidSessionDisconnect`variantを追加。
  2. `write_connect_outcome_for_wrapper`の分類を次の優先順位に:
     `StaleTrustSignal` → `MidSessionDisconnectSignal` → `Unreachable`。
  3. `outcome.rs`のモジュールdocコメントを新しい2分類
     （pre-handshake failure / mid-session disconnect）を正しく
     説明する記述へ書き換える。
  4. `wrapper.rs:648-653`の`outcome_summary`に
     `MidSessionDisconnect`用のアームも追加（R2-M1）。

### Task 2.6 — `decide_connect_failure_recovery`の拡張

- **対象**: `rust-core/isekai-ssh/src/wrapper.rs`と
  `rust-core/isekai-ssh/src/native/connect.rs`
  （**S5（opus-task-review）: シグネチャ変更は`native/connect.rs:355`
  周辺とそのテスト`:2209-2318`も直接壊すため、両ファイルを対象に
  含める**）。
- **変更**: ADR §2.2.2のコード例通りに変更する。**特に注意
  （R1-B1）**: `Some(ConnectOutcomeClass::Unknown)`は
  `NoRecoverableSignal`ではなく、`Some(_)`の通常アーム（
  `should_bootstrap`に応じて`RebootstrapAndRetry`/
  `AutoBootstrapDisabled`）に自然に落ちるようmatchを書く
  （`None`だけが`NoRecoverableSignal`）。
- **検証**: 既存の3ケースのテストに、`MidSessionDisconnect`×
  `should_bootstrap`の組み合わせ、`Unknown`×`should_bootstrap`の
  組み合わせを追加。`native/connect.rs`側の既存テストが
  シグネチャ変更後もコンパイル・passすることを確認。

### Task 2.7 — `recover_via_cross_family_fallback`のbail-out（B4旧、
    M1でスコープを訂正）

- **対象**: `connect.rs:622-634`付近の`ConnectRoute::StunWithFallback`
  アームのみ（**単一candidate経路にはこの問題は無い——M1参照。
  `recover_via_cross_family_fallback`を通るのは接続確立自体が
  失敗した`Err`経路だけであり、mid-session切断はこの1箇所からしか
  流れ込まない**）。
- **変更**: `recover_via_cross_family_fallback`に入る前に、
  `primary_err`が`MidSessionDisconnectSignal`を持つ場合は即座に
  元のエラーを返す。
- **検証**: 「STUN primary + relay fallback」構成でSTUNの
  mid-session失敗が発生した場合に、`recover_via_cross_family_fallback`
  へ進まず、かつ`ConnectOutcome`が`MidSessionDisconnect`として
  正しく書かれることを確認する単体テスト。

### Task 2.8 — Unixリトライループ: 共通モジュール抽出+ループ本体
    （M2・M3で責務分割）

- **対象**: `wrapper.rs::run_ssh_with_connect_failure_recovery`、
  新規共通モジュール（`native/mux/mod.rs`から`RECONNECT_BUDGET`/
  `RECONNECT_BACKOFF`/`RECONNECT_STABLE_THRESHOLD`を抽出——
  **ただしこの定数群は「`isekai-transport`に依存したくない」という
  明示的な理由でmux側にローカル実装された経緯がある
  （`mod.rs`のコメント参照）ため、抽出先が新規の依存を持ち込まない
  ことを確認する**）。
- **変更**: ADR §2.2.2の擬似コードに従い、単発呼び出しをループへ
  拡張する。このタスクに含めるのは:
  - 定数の共通モジュール抽出。
  - ループ本体（`Task 2.6`で拡張した`decide_connect_failure_recovery`
    を呼ぶ）。
  - `has_remote_command`（`plan.remote_command().is_some()`）で
    非idempotentコマンドの自動リトライを止める（B5）。
  - STUN経路の試行回数を3〜5回でキャップする**wrapper.rsレベルの
    総試行回数上限**（B4新——`retry_while_busy_other_session`は
    別クレートのprivate関数でありwrapper.rsから呼べないため、
    ここでは含めない。Task 2.11参照）。
- **このタスクに含めない**（M3で分離）: 孫プロセス対応（Task 2.9）・
  ctl-socket teardown（Task 2.10）・ライブ再接続表示（Task 2.12）。
- **検証**: 「1回目`MidSessionDisconnect`で失敗、2回目成功」の
  シナリオを`ConnectRecoveryOps`同種の抽象でモックテスト。
  `isekai-ssh host -- cmd`形式でmid-session切断が起きた場合に
  自動リトライされないことを確認するテスト（B5）。

### Task 2.9 — 孫プロセスの実測と条件付き対応（S1、独立した調査タスク
    として分離、S10で手順を厳密化、**2026-09-02解決済み、ADR Round 7参照**）

- **実測結果**: `ssh(1)`が**自発的に**終了する場合（`ConnectTimeout`
  満了等）は`ProxyCommand`子も道連れになるが、**外部から
  `SIGTERM`/`SIGKILL`された場合は子が生存し続ける**（initへ
  reparentされる）。すなわち「残る場合」が確認された。
- **Round 6での最初の対応（`PumpFailure::Local`/`Remote`型分離）は
  opus 2体の敵対的レビューで不十分と判明した**: `ssh(1)`死亡の
  支配的シグナルは`stdin`のEOFであってエラーではないため
  `PumpFailure::Local`は実質発火せず、resume-with-backoffループ
  最中やネットワーク既断状態での`ssh(1)`死亡はどちらも`Remote`
  分類のまま最大10日のresume windowに入っていた（詳細はADR Round 7）。
- **最終的に採用した対応**: `ensure_process_terminated`
  （`wrapper.rs`側のプロセスグループ管理、ジョブコントロール/
  `SIGHUP`/Ctrl+Cへの副作用と、wrapper自身が死ぬケースを覆えない
  という2つの理由で両批評家が独立に却下）ではなく、
  `isekai-pipe/src/parent_watchdog.rs`を新設した:
  `connect_command`冒頭で専用OSスレッドが自分のfd0/fd1を
  `poll(events:0)`でブロッキング監視し、`ssh(1)`が**どんな理由で
  終了しても**(自発的終了・外部kill問わず)`POLLERR`/`POLLHUP`で
  即座に検知する。検知時は`run_connect`全体とのレースに負けさせて
  futureを自然dropさせ、QUIC送信ストリームの既定Drop実装
  （`finish()`＝clean FIN）に任せることで、`.reset(0)`を書かずに
  「二度とresumeしないので サーバー側を即座にteardownさせる」を
  実現している。`prctl(PR_SET_PDEATHSIG)`/`kqueue`案（根本原因を
  問い直した末に一度提案）は実機実験で「`ssh(1)`は既に自前で
  `exec`しておりPart 1の変更は全接続を壊す」と判明し却下、代わりに
  ブロッキング`poll()`という単一POSIX実装に収束した（詳細な経緯・
  却下理由はADR Round 7、`parent_watchdog.rs`のモジュールdoc）。
  `PumpFailure::Local`/`Remote`分離とEOF-latch
  （`run_data_pump`が`(Result<(), PumpFailure>, bool)`を返し、
  `pump_c2h`が既にEOFに達した後の`Remote`失敗も同じ「即座に諦める」
  経路へ合流させる）は、watchdogの無いプラットフォーム
  （MSYS2/Cygwin版`ssh.exe`が起動するnative Windows版
  `isekai-pipe connect`）向けのフォールバックとして維持する。
  新設した`resume_loop::ParentGoneSignal`マーカーを
  watchdog起因・`Local`起因・EOF-latch起因のいずれのエラーにも
  付与し、`connect::write_connect_outcome_for_wrapper`がこれを
  他のどの分類より先にチェックしてoutcomeファイル自体を書かない
  ようにした——relay経路で`Unreachable`→`RebootstrapAndRetry`
  （B5ガード無し）に化ける経路を根本から断つ。
- **検証**: `parent_watchdog.rs`の
  `watch_loop_fires_when_the_peer_closes_its_end`（実pipeペアで
  watchdogの核心的な主張——読み書きが起きていなくても相手側の
  クローズを検知できること——を直接ピン留め）に加え、
  `pump_c2h_classifies_a_stdin_read_failure_as_local`・
  `pump_h2c_classifies_a_stdout_write_failure_as_local`
  （フォールバック経路の分類が正しいことのピン留め、`resume_loop.rs`）。
- **`wrapper.rs::apply_ctl_socket_forward`のdocコメント**を
  実測結果に合わせて訂正済み（`.status()`は直接の子だけを待つ、
  `ssh(1)`は自発的終了時のみ道連れにする、実際の解決は
  `parent_watchdog`である旨）。`proxy_command()`にも
  「単一の単純コマンドのまま維持すること」という不変条件のdocを
  追加した（`exec`前置の却下理由の記録も兼ねる）。

### Task 2.10 — ctl-socket forwardの反復ごとteardown（S3旧、独立タスク）

- **対象**: `apply_ctl_socket_forward`/`spawn_ctl_listener`
  （`wrapper.rs:743-751`）。
- **変更**: 反復ごとにbindするUNIXソケット・spawnするlistenerタスク
  を、次の反復に入る前かループ終了時に明示的にteardownする。
- **検証**: 複数回リトライした際にソケットfd/タスクの数が増加し
  続けないことを確認するテスト。

### Task 2.11 — STUN P2P connectの`retry_while_busy_other_session`対応
    （`isekai-pipe`crate内、Task 2.8とは別クレート）

- **対象**: `isekai-pipe/src/connect.rs`のSTUN P2P connect呼び出し
  箇所（`run_stun_p2p_with_fallback`・単一candidate経路）。
- **変更**: 現状Relay経路にしか適用されていない
  `retry_while_busy_other_session`（`resume_loop.rs:172`、
  `isekai-pipe`crateのprivate fn）を、STUN P2Pのconnectにも適用
  する。**このタスクは`isekai-pipe`crate内で完結し、`isekai-ssh`
  （Task 2.8）からは呼べない別層の対策であることを実装者に明示する
  （B4）**。
- **検証**: `BUSY_OTHER_SESSION`応答時にSTUN経路でもバックオフ
  リトライが効き、即座に`Unreachable`＝フル再展開に化けないことを
  確認するテスト。

### Task 2.12 — Windows mux側のSTUN試行回数上限（R1-S2、S8で
    実装方式を確定）

- **対象**: `native/mux/mod.rs`（`run_with_reconnect`周辺）。
- **決定（S8で確定、環境変数案は不採用）**: `run_with_reconnect`の
  `attempt`カウンタは**client プロセス**内で回るが、`isekai-pipe
  connect`をspawnするのは**owner/holderプロセス**
  （`spawn_isekai_pipe_connect`、`child_stdio.rs:49-70`）であり、
  しかも既存holderが再利用されるケース（他タブが先に立てたholder）
  ではclientからholderへ環境変数を渡す機会自体が無い。
  **したがって環境変数経由でカウンタを渡す案は採用しない**——
  `isekai-pipe connect`内の`retry_while_busy_other_session`
  （180秒バックオフ、Task 2.11）だけで実用上十分と割り切る。
  Windows mux経路には`wrapper.rs`相当の総試行回数キャップ
  （Task 2.8のB4新項目）は存在しないことを、既知の非対称として
  コードコメントに残す。

### Task 2.13 — UX: ライブ再接続表示

- `resume_loop.rs`が既に持つ「tssh風のライブ再接続表示」と一貫した
  スタイルで、wrapperループ側にも同種の1行ステータス表示を実装する。
  `resume_loop`側（セッション内のQUIC再接続）と`isekai-ssh`側
  （プロセス全体の再起動）は文言で区別する。`stderr.is_terminal()`
  ゲートを入れ、`--isekai-log-file`実行時にログが`\r`で汚れない
  ようにする（M2）。

### Task 2.14 — Windows単一プロセスfallbackの配線（M2で責務を明確化）

- **対象**: `native/connect.rs`の対応する関数（`ConnectRecoveryOps`
  トレイト経由）。
- **変更**: Task 2.8〜2.13で実装したロジック（共通モジュールに
  抽出済みの部分）を、Windows単一プロセスfallback経路にも配線する
  ——**再実装ではなく配線**（M2: Task 2.8が「共通モジュール抽出+
  Unix側配線」、本タスクは「Windows単一プロセスfallback側の配線
  のみ」）。
- **検証**: Task 2.8と同型のテストをWindows向けに追加（モック経由で
  Linux CIでも検証可能な形にする）。

---

## PR3: STUN P2Pへの真のバイトレベルresume（ADR §2.3、round 5＋
    round 5フォローアップ＋opus-task-review実装可能性レビューで
    全面改訂）

**マージ条件**: PR2がベース（マージ済み、`origin/main`＝`9fe13e6c`）。

**re-scoping全体像**: 旧Task 3.1〜3.4（4タスク）を、opus-critic-a・
opus-critic-bの独立並行レビュー（round 5）とそのフォローアップ
（root-cause rework案"Option B"の検討・却下）を経て6タスクへ
再分割した。さらにopus-task-reviewによる実装可能性レビューで
blocking 2件（Task 3.1とTask 3.4が同じ`resume_window`について
矛盾した指示を出していた／Task 3.4の「`Err`を返す」効果を実現する
配線先が現行シグネチャに存在しなかった）・significant 8件・
minor 3件が発見され、それらを反映して再度改訂した。Task 3.1〜3.3は
元の設計のまま実装詳細を訂正、Task 3.4は「スコープ境界の判断」から
「STUN専用の短いgive-up境界の実装」へ役割が変わった（load-bearing）、
Task 3.2b・3.5は新設。

### 実装前チェック（opus-task-reviewが「崩れているとPR3の設計自体が
    成立しない」と指摘、team-leadが実コードで検証済み・解消）

opus-task-reviewから「`factory.wrap_bound_socket(...)`（STUN初回確立、
自前でbind済みのソケットを渡す）で作った接続と、
`factory.create_endpoint(...)`（`reconnect_and_resume`のresume時、
新規bindする）で作った接続とで、RESUMEプロトコルに互換性があるか
未検証」という懸念が出た。`rust-core/quicmux/src/mux.rs:54-75`を
確認した結果、**両者は同じ`AnyMuxEndpoint`型を返し、どちらも同じ
`AnyMuxEndpoint::connect(remote)`を経由して同じ`AnyMuxConnection`型に
なる**（`qmux_backend.rs:146`/`noq_backend.rs:343`のバックエンド実装
レベルでも`connect`は`Endpoint`のメソッドであり、そのEndpointが
`create_endpoint`由来か`wrap_bound_socket`由来かを区別しない）。
RESUMEプロトコル自体（`session_id`・proof・`quicmux::request_resume`）は
サーバーとの間の接続上位層のやり取りであり、ローカルソケットの
生成方法とは完全に無関係。**この懸念は解消済み——PR3の設計は
この点で成立する。**

opus-task-reviewが挙げた残り2点（M3: `--punch-peer`確立接続の
server-side park共有／`probe.rs:370`の`AnyMuxConnection`保持位置）は
未解決のままでよい——それぞれTask 3.2・Task 3.1に実装時の確認事項
として既に明記済みであり、実装（Codexへの委譲を含む）と並行して
確認する。

### Task 3.1 — `stun_p2p.rs`の戻り値を拡張する
    （S4で変更範囲を訂正、round 5のN5で説明の力点を訂正）

- **対象**: `rust-core/isekai-transport/src/stun_p2p.rs:289-293`付近
  （`connect_stun_p2p_with_round`）。
- **主眼はここではない**: `resume_grace`のハードコード`0`は
  「resume無効」ではなく「サーバー既定を要求する」という意味
  （`ResumableRelaySession::effective_resume_grace_secs`のdoc参照、
  `resume_grace: 0`＝"no preference"）。`requested_resume_grace_secs`
  への置き換えは`#@isekai resume-grace`を尊重するための付随的改善に
  過ぎない。
- **本質的な変更**: `run_resume_loop`が要求する`ResumableRelaySession`
  （`connection`・`data_stream`・`control_stream`・`session_id`・
  `effective_resume_grace_secs`・`network_rebinder`、`resume.rs:152-176`）
  を満たすため、`connect_and_handshake`の戻り値4つのうち
  `connect_stun_p2p_with_round`が捨てている以下をすべて呼び出し元へ
  返すよう`StunP2pConnection`（現状`our_observed_addr`/`stream`のみ）
  を拡張する:
  - `conn`（`open_control_stream(&conn, &proof)`、`resume.rs:196`、の
    ために必要）
  - `proof`
  - `effective_resume_grace_secs`
  - `endpoint.rebinder()`（**`endpoint`がスコープを抜ける前に確保
    する**——`connect_via_relay_resumable`の既存パターンと同じ順序
    制約）
- **実装順序**: 上記4点（S4の本質的な変更）を先に配線し、
  `requested_resume_grace_secs`の置き換えは最後でよい——逆順で
  進めると「grace preferenceだけ配線したのにcontrol streamも
  session_idも無い」状態を実装者が発見することになる。
- **公開API変更の影響確認**: `connect_stun_p2p`/
  `connect_stun_p2p_with_fallback`/`StunP2pConnection`のシグネチャ
  変更になる。Androidの`isekai-terminal-core`側は既に独立した
  `connect_stun_p2p_on_socket`（`stun_p2p.rs:135`付近、自前で
  `connect_and_handshake`を呼ぶ）を使っており影響を受けない
  （round 5で再検証済み）。**`StunP2pConnection`は`pub`であり、
  実際に他に以下の利用者がいることをopus-task-reviewが確認済み
  （S4）——「grepで確認せよ」だけでなく、少なくとも以下は確認・
  更新すること**:
  - `isekai-pipe/src/probe.rs:370`（**プロダクションコード**、
    `isekai-ssh doctor`/probe経路）。`StunP2pConnection`に
    `AnyMuxConnection`（`conn`）が増えることでprobeが接続ハンドルを
    保持するようになるため、drop位置（probe完了後すぐ切断されるか）
    を確認する。
  - `isekai-transport/tests/stun_p2p_e2e.rs:56,102,134`・
    `stun_p2p_fallback_e2e.rs:44,85,114,134`
  - `isekai-pipe/tests/connect_stun_fallback_e2e.rs`
- **重要（B1、opus-task-review発見）: この`requested_resume_grace_secs`
  はTask 3.4の短いgive-up境界とは独立した別のノブである**——
  混同すると実装が壊れる。本タスクが配線するのは「クライアントが
  サーバーに対して要求し、サーバーが（`--resume-window`上限内で）
  実際に許可する、parked sessionの保持期間」（`effective_resume_grace_secs`、
  ユーザーの`#@isekai resume-grace`設定を尊重するためのもの）。
  Task 3.4が短くするのは「このクライアントプロセス自身が
  `resume_with_backoff_until_deadline`でbare redialを試み続ける
  時間」であり、**サーバーへ要求するgraceの値そのものは変更しない**
  （サーバー側のparked session保持期間は従来通り長くてよい——PR2の
  軽量リトライが後で同じセッションに戻ってこられる可能性を潰さない
  ため）。本タスクの実装時にTask 3.4の変更を先取りして
  `effective_resume_grace_secs`自体を短縮する実装をしないこと
  （具体的な理由と正しい配線先はTask 3.4参照）。

### Task 3.2 — `run_stun_p2p_with_fallback`の`run_resume_loop`移行

- **対象**: `isekai-pipe/src/resume_loop.rs:307`付近
  （`run_stun_p2p_with_fallback`）。
- **変更**: STUN P2P確立後に`RelayTarget`を合成し、`relay_stdio`では
  なく`run_resume_loop`（既存のRelay向けループ）へ渡すよう変更する。
  `RelayTarget`に`Default`実装は無いため`..`構文は使えず、5フィールド
  すべてを明示する:
  - `helper_addr: target.peer_addr`
  - `server_name`
  - `cert_sha256_hex`
  - `session_secret`
  - `local_bind_port_range`: **この関数に限り供給元が無い**
    （S-4、opus-task-reviewがTask 3.2bとの違いを確認済み——
    Task 3.2bの単一candidate経路には`intent`がスコープ内にあり
    問題にならない、下記参照）。`StunP2pTarget`にこのフィールドは
    無く、値は`ConnectionIntent::local_bind_port_range`にしかないが、
    `run_stun_p2p_with_fallback(target, candidates)`はこのintentを
    受け取っていない。デフォルト`None`にする（ユーザーの`#@isekai
    local-bind-port-range`設定がこのSTUN経路のみサイレントに
    無視される、という挙動差になる——コードコメントで意図的な決定
    として残す）か、シグネチャを拡張してintentを貫通させるかを
    実装時に決定する。
- **`factory`は`system_quic_factory()`を使う**（S-1）——
  `connect_stun_p2p_with_fallback`が既に使っているものと同一。
  `relay_endpoint_factory(RelayTransportKind::Qmux)`（TCP
  トランスポート）を誤って流用しないこと。
- **`experimental_network_rebind: false`・`tethering_interface: None`
  を明示的に渡す**（round 5のN4・S-3）——`AnyMuxRebinder::rebind`・
  `WarmStandby::new_bound_to_interface`はどちらも「別のソケット/
  物理インターフェースへ切り替える」機構であり、relay（安定した
  公開アドレス）では無害だが、STUN P2Pのホールパンチ済みNAT
  マッピングは特定のソケットに紐づいているため、再パンチなしに
  切り替えると到達不能になる。
- **doc修正（同一コミットで、S6でwordingを訂正）**: `ConnectLaunch::
  tethering_interface`の現行doc（`connect.rs:71-80`付近）の
  「`--mode stun`では効果が無い」という**結論はPR3後も正しいまま**
  （上記の通り明示的に`None`を渡すため）——訂正が必要なのは
  「STUN P2Pにはresume/control-stream概念が無いから」という
  **理由の部分のみ**。「ホールパンチ済みソケットとwarm standbyの
  socket rebindは両立しないため意図的に無効化している」という
  正しい理由に差し替えること。「効果が無い、という記述自体が
  誤りになる」と読んで「STUNでもtetheringが効く」ように実装と逆の
  docへ書き換えないこと。
  同様に`run_stun_p2p_with_fallback`自身のdocコメント（`resume_loop.rs:299-306`
  付近、「STUN P2P has no resume/control-stream concept … there is
  no `run_resume_loop` step here」）もPR3で全面的に誤りになるため
  同じコミットで書き換える（M2）。
- **確認事項（M3、実装コストは低いがPR3の効果が実地で成立する前提）**:
  `isekai-pipe serve --punch-peer`が確立した接続でも、通常の
  `--relay`確立と同じ`AttachArbiter`のaccept/attach/park機構が
  使われ、セッションが正しくparkされてRESUMEを受け付けることを
  `engine/mod.rs`で確認する（`punch_peer`は起動時のprobe送出にしか
  関与しないはずだが、ここが崩れているとクライアント側の単体
  テストでは検知できないままPR3が実地で無効になる）。

### Task 3.2b — 単一candidate経路のSTUN P2Pも同様に移行する（新設）

- **対象**: `isekai-pipe/src/connect.rs`の`CandidateRoute::StunP2p`
  アーム（`connect.rs:675-720`付近）。
- **変更**: Task 3.2と同じ内容をこの経路にも適用する。**Task 3.2の
  みでは「フォールバック経路はresumeするが単一candidate経路は
  しない」という分裂状態になる**（round 5のN3）。
- **実際の変更箇所を明示（S3、opus-task-review発見）**:
  `connect.rs:697-699`付近の
  `retry_while_busy_other_session(...).await.map(|conn| conn.stream)`
  が、Task 3.1が新たに返すようになる`conn`・`proof`・
  `effective_resume_grace_secs`・`endpoint.rebinder()`を**すべて
  握り潰している最大の書き換え箇所**——「Task 3.2と同じ内容を適用」
  だけでは伝わらないので明記する。この`.map(...)`をやめて、
  `RelayTarget`合成→`run_resume_loop`呼び出しに置き換える。
- **`local_bind_port_range`はTask 3.2と違い供給元がある（S2、
  opus-task-review発見）**: Task 3.2のADR/TASKS記述にあった
  「単一candidate経路も`intent`を受け取っていない」は**事実誤認**——
  同じ`match &candidate.route`式のrelayアーム（`connect.rs:663`
  付近）が既に`local_bind_port_range: intent.local_bind_port_range`
  を使っており、`intent`はこの`StunP2p`アームでも**スコープ内**。
  したがってこのTask 3.2bでは`intent.local_bind_port_range`を
  素直に渡す（Task 3.2のように`None`固定にする必要はない）。
  これを取り違えると、2つのSTUN経路で`#@isekai
  local-bind-port-range`の扱いが割れる（片方は尊重、片方は無視）。
- **確認事項**: このアームのエラーは`recover_via_cross_family_fallback`
  にも渡る。Task 3.3のマーカー配置後もそのbail-out条件
  （`connect.rs:759`付近）が正しく機能することを確認する。

### Task 3.3 — マーカー付与は呼び出し元での`.map_err`のみでよい
    （B3で決定を反転、round 5で実装方法を訂正）

- **対象**: PR2で導入したSTUN P2P向け`MidSessionDisconnectSignal`
  付与。**現在の実際の付与位置を訂正（S1、opus-task-review発見）**:
  「Task 2.4で導入したSTUN 2箇所への付与」ではなく、現状は
  **`relay_stdio`関数の内部**（その2つの remote-stream I/O 失敗
  箇所）に付いている——`resume_loop.rs:84-97`のdocコメント
  "Attached at the source, inside [`relay_stdio`] itself, to only
  its two *remote*-stream I/O failures" 参照。`connect.rs:715`や
  `resume_loop.rs:328`（呼び出し元）に`.map_err(...)`を探しに
  行っても何も無いので注意。
- **変更**: Task 3.2/3.2bにより`relay_stdio`の呼び出し元はゼロに
  なる（関数自体の削除はTask 3.5）。**Task 3.2/3.2bで新設する
  `run_resume_loop`呼び出しそれぞれで
  `.await.map_err(|e| e.context(MidSessionDisconnectSignal))`
  するだけでよい**（ルート判別フラグを`resume_with_backoff_until_deadline`
  まで貫通させる必要は無い）——`run_resume_loop`の`Err`は内部が
  1個の`?`と1個の`Ok(())`のみであるため常に「resumeを諦めた」ことを
  意味し、`anyhow::Error::downcast_ref`は`.context(...)`を何重に
  重ねても辿れるため、relay側の呼び出し元には一切影響しない。
  `recover_via_cross_family_fallback`のbail-out判定もこの配置の
  ままで正しく機能する（ADR §2.3の非対称設計自体は変更なし——
  relay経路の`give_up`には引き続き付けない）。
- **実装順序の注意（S1続き）**: Task 3.3をTask 3.5より先に実装すると、
  一時的に「呼ばれない`relay_stdio`内に旧マーカー付与コード＋新しい
  呼び出し元にも新マーカー付与」という中間状態になる。実害は無いが
  紛らわしいため、Task 3.3と3.5は同一コミット（または連続する
  コミット）で実施することを推奨する。
- **検証（S8で既存の足場を明記）**: relay側のgive-upが誤ってこの
  マーカーを継承しないことを確認する単体テストを追加する。「STUN
  経路でresumeを使い切った場合にPR2の軽量リトライループが発火する」
  ことを確認する統合テストも追加したいが、これは`isekai-pipe`
  （子プロセス）と`isekai-ssh`（wrapperプロセス）をまたぐため
  単体テストでは書けず、`isekai-ssh/tests/*_e2e.rs`に置くことになる
  ——**このテストはrequired checkの対象外**（`rust-core-test-linux`
  では機械的に検証されない）。Task 3.4側の同種の検証
  （`resume_with_backoff_until_deadline`が`Err`を返す確認、こちらは
  `rust-core-test-linux`で検証される）との非対称性を認識しておくこと
  ——「両方ともCIで機械的に守られている」と誤解しないこと。

### Task 3.4 — STUN専用の短いgive-up境界（load-bearing、旧「スコープ
    境界の判断」から役割変更、opus-task-reviewのB1/B2で配線方法を
    確定）

- **背景（round 5フォローアップで根本原因を再検討済み、ADR §2.3.0
  参照）**: STUN P2Pのresume再接続（`reconnect_and_resume`）は
  bare redial（再STUN・再パンチ無し）だが、クライアント側の
  IP/NAT変化（Wi-Fi切替等）はこれで正しく処理できる——サーバー側
  アドレスが安定している限り、これは正味の退行ではなく正しい設計。
  問題になるのは**サーバー側が到達不能**な場合のみ:
  `resume_with_backoff_until_deadline`の早期give-upは
  `UnknownSession`拒否の連続でしか発火せず、単純な接続タイムアウトの
  繰り返しでは発火しないため、既定の`resume_grace`（サーバー既定＝
  10日）のままだと最大10日間の無音のハングになる。
- **却下した代案**: 「STUN側の再接続時に再STUN問い合わせ＋再パンチを
  行う」という根本修正案（"Option B"）は、opus-critic-a・
  opus-critic-bの独立レビューにより却下された——`isekai-pipe serve`
  のホールパンチが起動時一度きりの動作であり、クライアント側だけの
  再パンチはサーバー側NATの許可フィルタを更新しないため到達可能性を
  一切改善しない（ADR §2.3.0参照）。
- **B1（opus-task-review発見）: Task 3.1が配線する`resume_grace`と
  本タスクの短い境界は独立したノブであり、混同しないこと**。
  Task 3.1は「サーバーに要求し、サーバーが許可する、parked session
  保持期間」を配線する（`effective_resume_grace_secs`）。本タスクが
  短くするのは「このクライアントプロセス自身がbare redialを試み
  続ける時間」であり、**`effective_resume_grace_secs`の値そのものは
  変更しない**（サーバー側のparked session保持期間は長いままにして
  おく——PR2の軽量リトライが後で同じセッションへ戻ってこられる
  余地を残すため）。
- **B2（opus-task-review発見）: 配線方法を確定**——
  `resume_window`は`run_resume_loop`内部で
  `resume_window_for(established.effective_resume_grace_secs)`
  （`resume_loop.rs:1085`、`resume_window_for`は`:572`）として
  導出され、呼び出し元からは触れない。以下3案を検討した結果、
  **(i)を採用する**:
  - **(i) 採用: `run_resume_loop`（および内部で呼ぶ
    `resume_with_backoff_until_deadline`）に、`effective_resume_grace_secs`
    とは別の新しい引数（例: `max_resume_window: Option<Duration>`）を
    追加し、`Some`のときは`resume_window_for(...)`が返す通常の
    デッドラインをさらにこの値で上書き/クランプする**。relay側の
    2つの呼び出し元（`run_relay_resumable`/`run_relay_resumable_with_fallback`
    相当）は`None`を渡し既存動作を一切変えない。STUN側
    （Task 3.2/3.2b）は`Some(STUN_GIVE_UP_WINDOW)`
    （目安60〜120秒、実装時に定数として定義）を渡す。
  - (ii) 却下: `effective_resume_grace_secs`自体をSTUN用に小さい値へ
    上書きしてから`ResumableRelaySession`を組み立てる案は、
    **`resume_window_for(0) == DEFAULT_RESUME_WINDOW`
    （`resume_loop.rs:573-576`、テストも`:1371`にある）という
    落とし穴がある**——上書き先が`0`になった場合、意図と逆に
    デフォルトの10日デッドラインへ戻ってしまう。値を`0`にしない
    よう慎重に実装すれば動作はするが、B1が言う「別ノブ」の原則にも
    反する（`effective_resume_grace_secs`は他の用途にも使われる
    値であり、STUNの都合で書き換えるのは筋が悪い）。
  - (iii) 却下: `resume_with_backoff_until_deadline`が試行回数上限を
    直接持つ案は、`give_up`がrelay/STUN共有（`:946`と`:1038`の
    2箇所）であるため、結局ADR §2.3.3が「過剰な変更」として明示的に
    却下したルート判別フラグの貫通が必要になり、Task 3.3の設計
    （呼び出し元での単純な`.map_err`のみ）と矛盾する。
- **効果**: この境界に達したら`run_resume_loop`自身が`Err`を返し、
  Task 3.3の`.map_err`で`MidSessionDisconnect`に分類され、PR2の
  軽量リトライへ制御が戻る（フルSTUN再確立をやり直す）。
- **ログ・OS通知（S7、opus-task-review発見）**: デッドライン超過分岐
  （`resume_loop.rs:936-960`付近）は既に`give_up(...)`でログ出力し、
  続けて`notify_os(...)`でデスクトップ通知を発火している。本タスクで
  STUNのデッドラインが10日→60〜120秒になることで、**このOS通知が
  最悪数分おきに飛ぶようになる**（PR2の軽量リトライが再確立→また
  切断、を繰り返す状況で通知スパムになる）。STUN経路では
  `notify_os`を抑制する（またはセッションごとに初回のみに絞る）
  判断を本タスクに含めること。ログメッセージ自体は既存の
  `give_up`の文言（session_id・resume window超過量を含む）で
  十分——新規に「scrollbackが失われ新しいセッションを確立する」旨を
  別途足す場合は、この既存メッセージと重複・矛盾しない形にする。
- **検証（S8で既存の足場を明記）**: `resume_loop.rs:1791`の
  `resume_with_backoff_until_deadline_returns_err_once_the_resume_window_is_exceeded`
  テストが、`spawn_control_hello_listener()`＋`system_quic_factory()`＋
  `reestablish_control_stream()`＋到達済みdeadlineという構成で、
  give-up経路が`Err`を返すことを実ネットワーク・実sshd無しに直接
  検証している。本タスクの検証（サーバー到達不能→短いgive-up境界→
  `Err`）は**このテストのほぼクローン**（deadlineを新しい
  `max_resume_window`由来にするだけ）として`rust-core-test-linux`
  （required check）で機械的に検証できる——実sshd/実ネットワークの
  E2Eを新規に書く必要はない。

### Task 3.5 — `relay_stdio`の削除（新設）

- **対象**: `isekai-pipe/src/resume_loop.rs`の`relay_stdio`関数。
- **背景（S5で理由を訂正、opus-task-review発見）**: 「放置すると
  `dead_code`警告で`rust-core-test-linux`が失敗する」という当初の
  理由は**誤り**——このワークスペースには`-D warnings`等の設定は
  どこにも無く（ワークフロー・`Cargo.toml`・`.cargo/config.toml`
  いずれにも無し）、`relay_stdio`を放置しても警告が出るだけでCIは
  緑のまま通る。**正しい理由**: Task 3.2/3.2b完了後、`relay_stdio`の
  呼び出し元はゼロになる（元の2箇所: `resume_loop.rs`・
  `connect.rs`）。放置すると、呼び出し元が存在しないのに
  `MidSessionDisconnectSignal`の付与コードと
  「only STUN P2P call sites may call this」という不変条件doc
  （`resume_loop.rs:111-118`）だけが生き残り、(a) マーカー付与箇所が
  `relay_stdio`内部（死にコード）と`run_resume_loop`呼び出し元
  （実際に使われる箇所）の二重管理になる、(b) 将来の誰かが
  「STUN経路なら使ってよい関数」として誤って復活させる罠になる、
  という保守上のリスクがCIのgreen/redとは無関係に残るため。
- **変更**（同一コミットで）:
  - `relay_stdio`関数自体を削除する。
  - `isekai-pipe/src/connect.rs:28`付近の`relay_stdio`の`use`文
    （未使用importになる、M1）を削除する。
    `isekai-pipe/tests/stdout_purity.rs:7`付近のコメント言及は
    コンパイルに影響しないが、あわせて更新するとなお良い。
  - `MidSessionDisconnectSignal`のdocコメント（現在「`relay_stdio`の
    リモートストリームI/O 2箇所のみ」とスコープを説明している）を、
    新しい付与箇所（Task 3.2/3.2bの`run_resume_loop`呼び出し元）を
    指すよう書き換える。
  - `isekai-pipe-core/src/outcome.rs`内の`relay_stdio`に言及する
    説明文・テストフィクスチャ文字列（`"relay_stdio: writing to
    remote stream failed"`相当、`outcome.rs:71`/`:235`付近）を更新
    する。

### スコープ外（変更なし）

§1.5の前提条件（サーバー側アドレスが安定していること）を超える
真の再ランデブー（クライアント・サーバー双方のアドレスが同時に
変わるケース）は、引き続きこのPRのスコープ外とする。これには
サーバー側の再STUN・新アドレスの帯域外シグナリングという
プロトコル変更が必要で、クライアント側の再接続プリミティブでは
原理的に対応できない（round 5フォローアップで検証済み）。この
限界は`ISEKAI_PIPE_DESIGN.md`のEpic R要約に明記する（ADR §6）。

---

## 横断: テスト戦略・Rollout

`ADR_MIDSESSION_DISCONNECT_RECOVERY.md` §5・§6を参照。要点:

- 全PR共通でローカル`cargo build`/`test`は使わない
  （`.claude/rules/prefer-gh-actions-over-local-cargo.md`。
  Task 2.9の`pgrep`によるプロセス観察はcargo操作ではないため対象外）。
  GitHub Actions上の`rust-core-test-linux`（required）で検証する。
- E2Eテスト（`isekai-ssh/tests/*_e2e.rs`）は non-required だが、
  mid-session切断の模擬は「サーバー側を落とす／該当セッションを
  強制切断する」形にする（クライアント側`isekai-pipe connect`の
  `SIGKILL`は`ConnectOutcome`が書かれないケースを検証してしまうため
  不採用、M1旧）。
- 各PRマージ後、`ISEKAI_PIPE_DESIGN.md`に「Epic R: セッション確立後の
  切断からの自動リカバリ」の要約をPR3完了後にまとめて追記する。
  STUN P2Pの残存限界（対称NAT越し構成では効かない）を明記する。
