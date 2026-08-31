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
  - `~.`エスケープ切断（`EscapeAction::Disconnect`、`:1136`付近）、
    `channel.data(...)`書き込みエラー（`:1134`付近） →
    `ClientGone`（ユーザー自身が切断した、またはローカル入力経路が
    死んだだけ）。
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
    として分離、S10で手順を厳密化）

- **これは調査タスクであり、Task 2.8をブロックしない**（M3——
  実測が終わるまでTask 2.8の他の要素の着手を止める必要はない）。
- **手順（S10で厳密化）**: 経路によって「正解」が逆になる
  （STUN=15秒で自然に落ちるのが正常、Relay=最大10日残るのが正常）
  ため、**必ずSTUN P2P経路のホスト**（`#@isekai stun`設定済み）で
  意図的にネットワークを切断し、**20〜30秒後**（QUIC idle timeout
  15秒に余裕を見た時間）に`pgrep -f 'isekai-pipe connect'`が
  残るかを確認する（ローカル実行で良い、`cargo build`/`test`では
  ないため`prefer-gh-actions-over-local-cargo`に抵触しない）。
  Relay経路での孫の残存はこの対策の対象外——観測してはいけない。
- **残らない場合**: `ensure_process_terminated`機構は実装しない。
  `wrapper.rs:719-725`のdocコメントを「`.status()`自体は直接の子
  だけを待つが、`ssh(1)`が自分のProxyCommandを終了させるため
  結果として孫も残らない」と理由を正確にする訂正のみ行う。このタスク
  はここで完了（Task 2.8への追加実装は無し）。
- **残る場合（S3: pid単発killという選択肢は存在しないと明記）**:
  `run_ssh_once`は`Result<(ExitStatus, String /*intent_id*/)>`
  （`wrapper.rs:798-803`）を返すのみでpidを取得できず、そもそも
  孫（`isekai-pipe connect`）のpidはwrapperから原理的に取得
  不能——**取れるのは`ssh(1)`自身のpidだけ**。したがって唯一の
  実装可能な選択肢は、`ssh(1)`を独自プロセスグループで起動し
  （`setsid`/`process_group(0)`）、次の試行前にグループ全体へ
  `SIGTERM`→猶予後`SIGKILL`を送ること。この場合のみTask 2.8の
  ループに実装を追加し、対応するテスト（プロセス監視付き）も
  追加する。

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

## PR3: STUN P2Pへの真のバイトレベルresume（ADR §2.3）

**マージ条件**: PR2がベース。

### Task 3.1 — `connect_stun_p2p_with_round`のresume_grace配線
    （S4で変更範囲を訂正）

- **対象**: `rust-core/isekai-transport/src/stun_p2p.rs:290-292`。
- **変更**: `resume_grace`のハードコード`0`を、呼び出し元から渡された
  `requested_resume_grace_secs`に置き換え、`_conn`
  （`AnyMuxConnection`）を破棄せず呼び出し元へ返す。
- **範囲の訂正（S4）**: これだけでは`run_resume_loop`
  （`ResumableRelaySession { connection, data_stream, control_stream,
  session_id, effective_resume_grace_secs, network_rebinder }`を要求、
  `resume.rs:152-175`）を満たせない。追加で必要な変更:
  - `open_control_stream(&conn, &proof)`（`resume.rs:196`）を呼ぶ
    ために`proof`も呼び出し元へ返す（現状`connect_stun_p2p_with_round`
    は`_proof`として捨てている）。
  - `network_rebinder`のために`endpoint`も保持する。
  - `StunP2pConnection`（現状`our_observed_addr`/`stream`のみ）を
    拡張し、上記を含められる型にする。
  - これは公開API`connect_stun_p2p`/`connect_stun_p2p_with_fallback`
    のシグネチャ変更であり、**`isekai-transport`crateの他の
    呼び出し元（Androidの`isekai-terminal-core`側は影響を受けない
    設計にする——Android側は既に`connect_stun_p2p_on_socket`という
    別関数を使っている、`stun_p2p.rs`参照）への影響有無を確認する**。

### Task 3.2 — `run_stun_p2p_with_fallback`の`run_resume_loop`移行

- **対象**: `isekai-pipe/src/resume_loop.rs:247-252`。
- **変更**: STUN P2P確立後に`RelayTarget{helper_addr:
  target.peer_addr, server_name, cert_sha256_hex, session_secret,
  ..}`を合成し、`relay_stdio`ではなく`run_resume_loop`（既存の
  Relay向けループ）へ渡すよう変更する。

### Task 3.3 — STUN経路のマーカー付与をrun_resume_loopのgive_upへ移動
    （B3で決定を反転）

- **対象**: Task 2.4で導入したSTUN 2箇所への
  `MidSessionDisconnectSignal`付与。
- **変更**: Task 3.2により`run_stun_p2p_with_fallback`と単一
  candidate経路のSTUN P2Pは`relay_stdio`を直接呼ばなくなるため、
  Task 2.4の2箇所の付与は自動的に消滅する。**代わりに、STUN P2P用の
  `run_resume_loop`呼び出しの`give_up`に`MidSessionDisconnectSignal`
  を付与する**（ADR §2.3-4のround 4決定——relay経路の`give_up`には
  引き続き付けないが、STUN経路は非対称に付ける。理由は§1.5の
  resume上限の違い）。
- **検証**: STUN経路でresumeを使い切った（`give_up`に到達した）
  場合に`MidSessionDisconnect`として分類され、PR2の軽量リトライ
  ループが発火することを確認する統合テスト。

### Task 3.4 — スコープ境界の実装時判断（旧Task 3.4を格下げ、S11）

- §1.5の前提条件（サーバー側アドレスが安定していること）を超える
  真の再ランデブーはこのPRでは対応しない。この構成でresumeが効かず
  scrollbackが失われる場合の検知・ログ出力方法（サイレント失敗か
  明示ログか）は、Task 3.2実装時にコードレビューで決めてよい
  （独立の実装タスクではなく、Task 3.2の一部として扱う）。

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
