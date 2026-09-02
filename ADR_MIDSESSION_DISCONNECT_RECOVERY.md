# ADR: セッション確立後のネットワーク切断からの自動リカバリ（`isekai-ssh` Epic R）

- **Status**: **Accepted**（2026-08-31起草、round 3改訂でADR設計に
  ついてopus-critic-a・opus-critic-bの両者が収束。round 4は
  `TASKS_MIDSESSION_DISCONNECT_RECOVERY.md`への実装タスク分解の
  過程でopus-task-reviewが発見した、コード構造レベルの矛盾4件を
  反映した設計修正——ADRの意思決定そのものが変わったわけではないが、
  §2.2.1・§2.3の実装詳細を訂正している）
- **ユーザー決定事項**（2026-08-31）:
  1. **PR構成は3分割**（PR1: 独立した安全な修正のみ / PR2: リトライループ本体
     / PR3: STUN P2Pの真resume）。各PRが個別にCI green→マージ可能な単位。
  2. **STUN P2Pの真のバイトレベルresume（旧Tier 2）を今回のスコープに含める**
     （B7でコストが当初想定より小さいと判明したため。§2.1参照）。
- **対象**: `rust-core/isekai-pipe`（`connect.rs`/`resume_loop.rs`/`main.rs`）、
  `rust-core/isekai-pipe-core`（`outcome.rs`）、`rust-core/isekai-ssh`
  （`wrapper.rs`・`native/connect.rs`・`native/child_stdio.rs`）
- **入力**: 本セッションでのユーザー報告「`tssh`はすぐ死なないが`isekai-ssh`は通信断で
  すぐ死ぬ。Windows の russh 経由でも同様」の調査結果（本ドキュメント §1）
- **拘束される既存ルール**: `CLAUDE.md`、`.claude/rules/always-connects.md`（本ADRが
  対処するギャップの上位原則）、`.claude/rules/rust-ssot.md`（本ADRのスコープは
  Android/iOSと無関係な独立クレートなので直接の抵触は無いが、状態機械をRust側に
  閉じる設計思想は踏襲する）、`.claude/rules/prefer-gh-actions-over-local-cargo.md`
  （ローカル`cargo build`/`test`全面禁止、実装・検証はすべてCI経由）、
  `.claude/rules/main-branch-protection.md`（PRの5 required checksが緑になって
  初めてマージ対象）
- **UniFFIへの影響**: なし。`isekai-ssh`/`isekai-pipe`は`isekai-terminal-core`
  （Android/iOSが依存するUniFFI公開crate）とは独立したcrate群であり
  （`CLAUDE.md`のディレクトリ構成節）、本ADRのスコープはこれらの中で完結する。
  `.claude/rules/uniffi-binding-regeneration.md`の手順は不要。

---

## 0. 改訂履歴

### Round 7（2026-09-02）— Round 6の`PumpFailure`分離だけでは不十分と
    判明し、`parent_watchdog`（ブロッキング`poll()`）で再解決

Round 6実装（`PumpFailure::Local`/`Remote`分離）をPR #112として提出後、
opus 2体（opus-critic-task29-a/b、それぞれ実験による裏取りを伴う）に
複数ラウンドの敵対的レビューを依頼し、**Round 6単体では不十分**という
結論に収束した:

- **F1（blocking）**: `ssh(1)`が死んだ際の支配的なシグナルは`stdin`の
  **EOF**であってエラーではない。`pump_c2h`の`n == 0`分岐は`Ok(())`を
  返す既存の「正常終了」経路であり、`PumpFailure::Local`は実質
  発火しない。
- **F2（blocking）**: 実際に孤児が長時間残る2シナリオ——(a)
  resume-with-backoffループの最中に`ssh(1)`が殺される、(b)ネットワーク
  が既に落ちている状態で`ssh(1)`が死ぬ——はどちらも`Remote`分類に
  落ち、relay経路ではデフォルト10日のresume windowにそのまま入る。
  Round 6はこの核心部分を未解決のまま残していた。
- **F3（significant）**: relay経路（`MidSessionDisconnectSignal`を
  付与しない）で`Local`失敗が`Unreachable`として素通しされると
  `RebootstrapAndRetry`（`wrapper.rs::decide_connect_failure_recovery`）
  に到達しうるが、このアームには非冪等リモートコマンド再実行防止の
  B5ガード（`RetryConnectLightweight`側にのみ存在）が無い。
- **F4（significant）**: `Local`分岐の`quic_write.reset(0)`が、
  「二度とresumeされないセッションをサーバー側に最大10日間park
  させる」という、コメントの意図と正反対の結果を招いていた。

ユーザー指示「根本原因を考えて非連続な変更を」を受け、`isekai-ssh`が
`ProxyCommand`文字列に`exec `を前置して`isekai-pipe connect`を
`ssh(1)`の直接の子に強制し、`prctl(PR_SET_PDEATHSIG)`（Linux）/
`kqueue`+`EVFILT_PROC`（macOS）でカーネルレベルの親死亡通知を使う案を
提案したが、**両批評家が独立に実機実験でこれを否定した**:
`ssh_config(5)`が明記する通り`ssh(1)`は既に`ProxyCommand`を
`$SHELL -c "exec <cmd>"`として起動しており(「the command string ...
is executed using the user's shell 'exec' directive to avoid a
lingering shell process」)、`isekai-pipe connect`は既に`ssh(1)`の
直接の子である。二重`exec`を追加する提案通りの変更は
`exec exec <cmd>`となり、dash/bashともに`exec: exec: not found`
(rc=127)で即座に失敗し、**全てのUnix接続を壊す**ことが実証された
(この近接ミスの記録として本セクションに残す)。

その後さらに数ラウンドの往復を経て、両批評家が
「専用スレッドでのブロッキング`poll()`」という、`prctl`/`kqueue`より
単純かつ正しい方式に収束した。理由: レベルトリガーのためarm時
レース(`prctl`にはある——親が既に死んでいると通知自体が届かない
既知の問題、しかもorphanは`systemd --user`のようなchild subreaperへ
reparentされるため`getppid() == 1`という素朴なguardは静かに機能
しない、という具体的なバグを両批評家が独立に発見した)が原理上存在
しない、Linux/macOS/BSDで単一のPOSIX実装で済む、「特定のPIDが
死んだか」ではなく「stdioの向こう側に誰かまだいるか」という本質的な
述語を直接検査できる、という3点。`SIGTERM`のデフォルトアクションが
即座にプロセスを終了させグレースフルシャットダウン(FIN送信)を
スキップしてしまう(結果としてF4と同じ「サーバー側10日park」を招く)
という点も、独立にどちらの批評家からも指摘された。

**最終的に実装した設計**(`rust-core/isekai-pipe/src/parent_watchdog.rs`):

- `connect_command`の冒頭で一度だけ、専用OSスレッドが`poll(fd=0, fd=1,
  events:0)`をタイムアウト無しでブロッキング待機する
  (`POLLERR`/`POLLHUP`/`POLLNVAL`は要求した`events`に関わらず
  `revents`に常に現れる)。fd0とfd1は別々のpipe(同一
  socketpairではない、2026-09-02実機測定で確認——`fd1`側は
  `POLLHUP`ではなく`POLLERR`で倒れることも判明したため両方を
  受理する)。`tokio::io::unix::AsyncFd`は使わない
  (`O_NONBLOCK`はopen file description単位のプロパティであり、
  fd1に設定すると`pump_h2c`の実際の`stdout`書き込みまで
  non-blocking化してしまう——素のスレッドならこの問題自体が
  発生しない)。
- 検知したら、`connect_command`が`run_connect`全体を
  この watchdog と`tokio::select!`で競わせているため、負けた側の
  `run_connect`のfutureがその場でdropされる。これによりネストされた
  非同期フレーム(`data_stream`/`quic_write`含む)が通常のRust
  スコープ脱出と同じ順序でdropされ、`quicmux`(内部の`noq`/`qmux`)の
  送信ストリームのデフォルトDrop実装がclean FIN(`finish()`)を
  キューに積む——`.reset(0)`を呼ぶコードを一切書かずに、まさにF4が
  求めていた「今回は二度とresumeしないのでサーバー側をparkさせず
  即座にteardownさせる」動作を狙ったもの。**ただしこれはbest-effort
  であり保証ではない**(Round 7フォローアップ参照——`finish()`は
  キューに積むだけで、実際に配線へ流すにはconnectionのdriverタスクが
  再度pollされる必要があり、Drop自体はそれを保証しない。失敗しても
  実害は「サーバー側が15秒のidle timeoutでDataStreamDiedに倒れ
  resume-grace分parkされ、後でsweep_expired_parkedに回収される」だけで、
  PR適用前の状態(F4のバグ)より悪化はしない)。
- `resume_loop.rs`側の`PumpFailure::Local`/EOF-latch(下記)は
  廃止せず、(a) macOS以外でwatchdogが無いプラットフォーム
  (MSYS2/Cygwin版`ssh.exe`が起動するnative Windowsバイナリの
  `isekai-pipe connect`——`poll(2)`が使えない)向けのフォールバック、
  (b) Unix上でも`ssh(1)`自体は生きたままこのpipeだけ壊れる稀な
  ケース、として維持する。`PumpFailure::Local`到達時も
  `.reset(0)`を呼ばず`.shutdown()`を呼ぶよう変更(F4対応。ただし
  `.shutdown()`自体もnoqの`poll_shutdown`実装がPoll::Readyを即座に
  返すだけで、Dropと同じくキューに積むのみ——実際にflushの機会を
  与えるのは後述のgrace sleepであり、`.shutdown()`はreset/FINの
  フレーム種別を正しくするためだけの呼び出し)。
- **EOF-latch**(F2対応の一部): `run_data_pump`が
  `c2h_already_done: &mut bool`を追加引数として受け取り、
  `pump_c2h`がOkを返した瞬間に同期的に`true`を書き込むよう変更した
  (戻り値の一部としてタプルで返す設計は、`run_resume_loop`の外側
  `tokio::select!`で`reconnect_signal_rx`側が勝って`run_data_pump`の
  futureごとcancelされた際にこの情報を失う実バグがあり、opus再レビュー
  で発見・修正済み——Round 7フォローアップ参照)。この変数は
  `run_resume_loop`の外側loopの外で1度だけ宣言し、二度と`false`へ
  戻さない。`run_resume_loop`はこれが`true`の状態で`Remote`失敗を
  受け取った場合、`Local`と同じ「即座に諦める」経路へ合流させる
  (ローカル側の送出元が既に終わっている以上、resumeを試みる価値が
  無いという結論はどちらも同じ)。watchdogの無いプラットフォームで
  F2(b)の一部を狭く塞ぐ。
- **F3対応**: `ParentGoneSignal`という新しいマーカーを
  `resume_loop.rs`に新設し、watchdog起因のエラー・
  `PumpFailure::Local`起因のエラー・EOF-latch起因のエラーの
  いずれにも(route/STUN・relay問わず)付与する。
  `connect::write_connect_outcome_for_wrapper`はこのマーカーを
  他のどの分類判定よりも先にチェックし、存在すれば**outcomeファイル
  自体を一切書かない**——relay経路で`Unreachable`
  →`RebootstrapAndRetry`(B5ガード無し)に化ける経路を根本から断つ。

### Round 7フォローアップ（2026-09-02）— 実装直後のopus再レビューで
    見つかった5件のバグ、および誇張の自己訂正

Round 7の初回実装をopus 2体に再レビューさせたところ、独立に同一の
3件の実バグ（+関連指摘）が見つかった:

- **wait()のfail-open反転**: `spawn()`のスレッド起動失敗
  （`std::thread::Builder::spawn`は失敗時にクロージャ自体を
  txごと破棄し、呼び出し元へ返さない）と`watch_loop()`自身の
  `poll()`異常時の`return`は、どちらも「fail open」という
  コメント通りの意図だったが、`wait()`旧実装
  (`let _ = rx.changed().await;`)はsender全滅による`changed()`の
  `Err`を実際の発火と区別できず、逆に即座のfire(接続開始直後の
  abort、または健全なセッション中のabort)を引き起こしていた。
  `spawn()`で`tx.clone()`を1つ`mem::forget`でリークし、どちらの
  経路が`tx`を送らずdropしてもチャンネルが閉じないよう修正した。
- **`.shutdown()`はflushを保証しない**: noqの`SendStream`の
  `poll_shutdown`実装を確認したところ`Poll::Ready`を即座に返す
  だけで、`Drop`同様キューに積むのみ。`.await`してもスケジューラへ
  yieldしない。実際にflushの機会を与えるのは
  `tokio::time::sleep()`のみであり、これを「watchdog起因の
  selectアーム」ではなく「結果が`ParentGoneSignal`を持つか」で
  `connect_command`側に一元化した——`run_resume_loop`のin-loop
  give-up分岐も`run_connect`ブランチ経由で戻ってくるため、
  watchdogアームだけにsleepを置くとこちらを取りこぼす。
- **EOF-latchの世代跨ぎ**: `c2h_already_done`を`run_resume_loop`の
  外側`loop`の中で毎世代`false`初期化していたのを、loopの外で
  1回だけ宣言し二度と`false`に戻さないよう変更した（レビューでは
  「`tokio::select!`の非決定的なポーリング順序次第で理論上
  再発しうる」と指摘されたが、事後の相互検証で「実際には
  latchが立った世代は必ずその場でreturnするため次の世代には
  到達しない」ことが判明——本質的には`&mut`で借用を渡すこと自体が
  cancel耐性の効いている理由であり、宣言位置はbelt-and-braces。
  それでも「set-only-to-true」という規律自体は正しいため変更は
  維持した）。
- **UX（N5）**: watchdogは通常の`exit`でも、S→C方向がTCP往復済みの
  clean EOF経路より先に倒れることが多く、`ParentGoneSignal`は
  レアケースではなく毎回のログアウトで頻繁に発生しうる。
  `connect_command`のeprintln!(anyhowダンプ)をこのケースで
  抑制した。**ただし終了コードはEX_UNAVAILABLEのまま維持**——
  一度`ExitCode::SUCCESS`に変更したが、「`ParentGoneSignal`は
  dial中のssh(1)強制終了やssh(1)生存中の`PumpFailure::Local`
  など本当に異常なケースも含むため、終了コードを見るスクリプトが
  気づけなくなる」という指摘を受けて差し戻した。
- **テストが`watch_loop`本体を呼んでいなかった**: 手書きコピーの
  ループを検証していたのを、`watch_loop(tx, fds: &[libc::c_int])`
  というシグネチャに変更して本物を呼ぶようにし、fd1側で実際に
  発火する`POLLERR`（`POLLHUP`ではない）方向のテストケースも
  追加した（既存テストはfd0側の`POLLHUP`のみ検証していた）。

**自己訂正の記録**: 上記UX対応の初稿で、「watchdogがclean EOF
経路との競合に確実に勝つため通常ログアウトでは`ParentGoneSignal`が
例外ではなく通常経路になる」という一文を、あたかもopus-critic-task29-a
のレビュー結果であるかのようにコードコメントへ記録したが、
実際にはレビュー側は「勝つことがある、断続的に」としか述べておらず、
断定はこちらの誤った言い換えだった（このセッション中に同種の
誇張——未検証の思いつきを確定事実として記録する——が3回発生した
うちの1つとして本人が自認し訂正）。`eprintln!`抑制自体の判断は
維持しつつ、コメントの根拠を「その時点でローカル側の相手が既に
いないため報告先が無い」という無条件に正しい理由に書き換えた。

### Round 7フォローアップ2（2026-09-02）— macOS実CIが`events: 0`の
    非対応を検出、per-fd events+sleep-guardで解決（kqueue案は両批評家が
    自ら撤回）

Round 7フォローアップの実装をpushしたところ、`rust-core-test-macos`
（必須チェックではないが実在するCI）が新設した`parent_watchdog`の
2テストとも「タイムアウトで発火せず」失敗した。`events: 0`で
`poll()`した場合、LinuxではPOSIXの保証通り`POLLHUP`/`POLLERR`/
`POLLNVAL`が常に`revents`へ現れるが、Darwinでは同じ保証が成立
しないことが実測で判明した(Darwinは`poll(2)`をネイティブ実装
しておらず、`poll_nocancel()`が各`pollfd`を`kqueue`登録へ変換する
——`events`が読み取り方向を要求しなければ`EVFILT_READ`は登録
されず、その fd には何も監視するものが無くなるため`revents`が
一切生成されない、というのがopus 2体独立の見立て)。

**最初の対応(即座にrevert)**: `events: 0`を`POLLIN`へ全fd一律で
変更したが、これは「watchdogスレッド自身は一切fd0をreadしない」
という設計上の事実と衝突する重大な回帰だった——`ssh(1)`は
接続確立直後にバージョンバナーをstdinへ書き込むが、
`run_resume_loop`のpumpが実際にreadを始めるのはdial/handshake/
`retry_while_busy_other_session`が終わった後であり、その間ずっと
未読データがfd0に residual するため、level-triggeredな`poll()`が
即座に`POLLIN`を返し続け、CPU 100%でスピンする(全接続・全
プラットフォームで発生する、Darwin固有ではない一般的な性質)。
opus-critic-task29-aの指摘で即座にrevertした。

**検討したが両批評家が自ら撤回したkqueue案**: Darwin/BSD向けに
`EVFILT_READ`/`EVFILT_WRITE`+`EV_CLEAR`(edge-triggered)を使う
プラットフォーム別実装をユーザーの明示的指示で一度設計したが、
実装に着手する前に両批評家が独立に「もっと単純な代替案がある」
と撤回を申し出た。

**最終的に採用した設計**: `poll()`実装を1つのまま維持し、
(a)各fdが実際にサポートする方向のみを要求する(fd0=`POLLIN`、
fd1=`POLLOUT`、逆方向は要求しない——書き込み専用のfd1に読み取り
フィルタを要求するのは、登録失敗による即時発火という別の危険が
疑われる、実測はしていない)、(b)`watch_loop`本体に「`TERMINAL`
ビットが立たずに`poll()`が戻った場合はSPURIOUS_BACKOFF(1秒)だけ
sleepしてから再度pollする」というガードを追加する。これにより
per-fd events要求で再発するspurious wakeup(fd0の未読データ・
fd1の常時書き込み可能状態)をスピンではなく1秒間隔のポーリングに
変換する。kqueueによるedge-triggered化より単純で、新規の
プラットフォーム固有コードを一切増やさずに済む。

**この設計変更の副次的な帰結**(critic Bの指摘、記録に値する):
検知レイテンシが実質最大1秒になったことで、`parent_watchdog`は
通常のログアウト(clean EOF経路)との競合にもう確実には勝たなく
なった——これは望ましい division of labor の回復であり、退行では
ない。通常ケースは元々の`pump_c2h`のEOF経路が`Ok(())`で処理し、
`parent_watchdog`はdial/handshake/resume待機中などpumpが動いて
いない「静かな」局面のバックストップとしてのみ機能する、という
役割分担が実現された(N5で追加した`eprintln!`抑制・非ゼロ終了
コード維持は引き続き必要かつ正しいが、発火頻度は当初の想定より
低い)。

**テスト**: 「peerが生きている間は発火しない」という否定側の
チェックを全ての肯定的テストに組み込んだ(単に閉じてから起動する
既存の全テストでは「常に無条件で発火する」という壊れ方——
まさにレジストレーション失敗による誤発火のシナリオ——を検知
できないため、両批評家が独立にmerge gateとして要求した)。
さらに、production が実際に渡す`&[(0, POLLIN), (1, POLLOUT)]`
という複数fd構成そのものを検証するテストも追加した——単一fdずつの
テストでは、「片方のfdが常時spuriously-readyな状態でも、もう片方の
fdの本当のcloseを見逃さない」という production の定常状態を
検証できないため(両批評家が独立に指摘した、唯一残っていた
テストギャップ)。

**Task 2.9自体の結論**: `ensure_process_terminated`
(`wrapper.rs`側のプロセスグループ管理)は実装しない
(ジョブコントロール/`SIGHUP`/Ctrl+Cへの副作用が大きく、かつ
wrapper自身が死ぬケースを覆えないため——両批評家が独立に到達した
結論)。代わりに`isekai-pipe connect`自身が自分のstdin/stdoutの
生死を能動的に監視する`parent_watchdog`で解決した。

### Round 6（2026-09-02）— Task 2.9（孫プロセスの実測）を実施し、
    `ensure_process_terminated`より汎用的な自己終了型の修正で解決

PR1〜PR3（#108〜#110）マージ後、唯一残っていたTask 2.9（§2.3.6/
TASKS.md該当節）を実機に近い形で検証した。当初の実測手順（実際に
STUN P2Pホストのネットワークを切断する）の代わりに、**Task 2.9の
本質はOS一般の`ssh(1)`+`ProxyCommand`の性質であり、実ネットワーク
切断もSTUN固有の事情も不要**と判断し、合成`ProxyCommand`
（`sh -c 'echo $$ > pidfile; exec sleep 600'`）でローカルに再現した:

| `ssh(1)`の終了経路 | ProxyCommand子プロセス |
|---|---|
| 自身の`ConnectTimeout`満了（自発的終了） | 道連れで終了 |
| 外部からの`SIGTERM` | **生存**（initへreparent） |
| 外部からの`SIGKILL` | **生存**（initへreparent） |

`ssh(1)`は自分の`cleanup_exit()`が走る自発的終了でのみ子を終了させ、
外部から殺された場合は終了させない（`SIGKILL`はcleanup_exit自体を
実行不能にするため当然だが、`SIGTERM`でも同様だったのは実測するまで
自明ではなかった）。TASKS.mdのTask 2.9が要求していた「実際に…」
という手順とは異なる、より汎用的な検証だが、この結果は
`wrapper.rs`が`ssh(1)`を再起動する際の内部シグナル経路にも、外部
要因（OOM killer・端末強制終了等）にも等しく当てはまる、より広い
主張として扱える。

この結果を受けて**`ensure_process_terminated`（プロセスグループ管理
によるkill）は実装しなかった**。代わりに、より根本的で経路非依存の
修正を`resume_loop.rs`に実装した: `run_data_pump`の失敗を
`PumpFailure::Local`（`stdin`/`stdout`側、`ssh(1)`のパイプが壊れた
ことを示す）と`PumpFailure::Remote`（QUIC/ネットワーク側、resumeを
試す価値がある）に型で分離し、`run_resume_loop`は`Local`失敗を
即座に`Err`として返してresumeループに入らないようにした。

この修正が`ensure_process_terminated`より優れる理由: `ssh(1)`が
自分の子の**パイプfdを閉じる**のはOSカーネルがプロセス終了時に
無条件で行う後始末であり、`ssh(1)`自身の終了経路（自発的/`SIGTERM`/
`SIGKILL`のいずれか）にも、`ssh(1)`が孫プロセスを明示的にkillする
かどうかにも依存しない——`isekai-pipe connect`は自分のstdin/stdout
が壊れたことを次のread/writeで即座に検出でき、`ssh(1)`側の協力を
一切必要としない。これにより`MidSessionDisconnectSignal`のdoc
コメントが警告していた「ローカル側障害＋健全なネットワークが
`disconnected_since`のリセットにより無限に再接続ループする」という、
relay・STUN両経路が共有していた既存バグ（Epic R以前から存在）も
同時に解消される。

検証: `pump_c2h`/`pump_h2c`それぞれについて、実QUIC接続を使い
「`stdin.read()`失敗」「`stdout.write_all()`失敗」がそれぞれ
`Local`に分類されることを直接ピン留めする回帰テストを追加した
（`pump_c2h_classifies_a_stdin_read_failure_as_local`・
`pump_h2c_classifies_a_stdout_write_failure_as_local`）。

PR1〜PR3とは独立した修正のため新規PRとして分離し、TASKS.mdの
Task 2.9をこの結果で更新した。

### Round 5（2026-09-02）— PR2マージ後、PR3着手前にopus-critic-a・
    opus-critic-bへ独立並行レビューを再依頼して発見

PR1（#108）・PR2（#109）がmainへマージされた後、PR3実装に入る前に
§2.3・Task 3.1〜3.4をopus-critic-a・opus-critic-bの2エージェントへ
再度独立並行でレビューさせた（round 1と同じ形式）。**両者が完全に
独立して同じblocking項目に収束した**——PR3が実装されればADRが解決
しようとしている問題を悪化させる、という重大な設計欠陥だった。

| 変更 | 由来 |
|---|---|
| **STUN P2P専用の短い give-up 境界を新設（Task 3.4、load-bearing）**: `run_resume_loop`が使う`resume_with_backoff_until_deadline`は、確立済みセッションが本当に消えたと確定する条件が「`UnknownSession`拒否がN回連続」のみで、単純な接続タイムアウトでは早期終了しない。**サーバー側が到達不能な場合**（後述round 5フォローアップ参照——クライアント側のNAT変化自体はbare redialが正しく処理できることが判明したため、当初の「常に成功し得ない」という評価は訂正済み）、redialは接続タイムアウトで失敗し続け`UnknownSession`にもならないため、デフォルトの`resume_grace`（サーバー既定=10日）のままでは最大10日間の無音のハングになる。STUN経路には接続確立段階の失敗回数（またはごく短い秒数、目安60〜120秒）でPR2の軽量リトライへ制御を戻す独自の短いgive-up境界が必須（severityは「退行防止」ではなく「ハング防止」） | opus-critic-a N1・opus-critic-b B-1（両者独立に同一結論、round 5フォローアップで理由を訂正） |
| **round 4で確定した「STUN経路の`give_up`にのみマーカーを付ける」非対称設計の実装方法を訂正**: `give_up`は`resume_with_backoff_until_deadline`内部で2箇所から呼ばれる共有関数であり、STUN専用の`give_up`は存在しない。ルート判別フラグを`run_resume_loop`→`resume_with_backoff_until_deadline`へ貫通させる案（opus-critic-a）ではなく、**`run_resume_loop`の呼び出し元（STUN側の2箇所）で`.await.map_err(\|e\| e.context(MidSessionDisconnectSignal))`するだけで済む**（opus-critic-b）——`run_resume_loop`の`Err`は必ず「resumeを諦めた」ことを意味し（内部は1個の`?`と1個の`Ok(())`のみ）、`anyhow::Error::downcast_ref`は`.context(...)`を何重に重ねても辿れるため、relay側の呼び出し元には一切影響しない。`recover_via_cross_family_fallback`のbail-out判定もこの配置のままで正しく機能する | opus-critic-a N2・opus-critic-b S-2（opus-critic-b案を採用） |
| **単一candidate経路のSTUN P2P（`connect.rs`の`CandidateRoute::StunP2p`アーム）の移行タスクが欠落していた**ことが判明。Task 3.2は`run_stun_p2p_with_fallback`のみ言及しており、これだけを実装すると「フォールバック経路はresumeするが単一candidate経路はしない」という分裂状態になる。Task 3.2bとして独立化し、`recover_via_cross_family_fallback`のbail-out条件との整合も確認する | opus-critic-a N3 |
| **`network_rebinder`・`tethering_interface`・`experimental_network_rebind`をSTUN経路では明示的に無効化（`false`/`None`）**: これらは全て「別の物理インターフェース/ソケットへ切り替える」機構であり、relay（安定した公開アドレス）では無害だが、STUN P2Pのホールパンチ済みNATマッピングは特定のソケットに紐づいているため再パンチなしに切り替えると到達不能になる——B-1と同じ症状の別経路。`ConnectLaunch::tethering_interface`の「STUN P2Pには効果がない」というdocコメントもTask 3.2実装と同じコミットで訂正が必要 | opus-critic-a N4・opus-critic-b S-3 |
| **`RelayTarget`の`local_bind_port_range`フィールドに供給元が無いことが判明**: `StunP2pTarget`にこのフィールドは無く、値は`ConnectionIntent::local_bind_port_range`にしかないが、`run_stun_p2p_with_fallback`/単一candidate経路のどちらもこのintentを受け取っていない。デフォルトで`None`にする（ユーザーの`#@isekai local-bind-port-range`設定がSTUN経路でのみサイレントに無視される、というfirewall越しの可視な挙動差になる）か、シグネチャを拡張してintentを貫通させるかをTask 3.1/3.2実装時に明示的に決定する。またADR文中の`RelayTarget{..}`という構造体更新記法は`RelayTarget`に`Default`実装が無いため無効なRustであり、5フィールドを全て明示する | opus-critic-a N7・opus-critic-b S-4 |
| **`relay_stdio`の削除をTask 3.5として独立させる**: Task 3.2/3.2b完了後`relay_stdio`の呼び出し元はゼロになり（`resume_loop.rs`・`connect.rs`の2箇所）、放置すると`dead_code`警告で`rust-core-test-linux`が落ちる。削除に伴い`MidSessionDisconnectSignal`のdocコメント（現在「`relay_stdio`のリモートストリームI/O 2箇所のみ」とスコープを説明している）、および`isekai-pipe-core/src/outcome.rs`内の関連する説明文・テストフィクスチャ文字列（`"relay_stdio: writing to remote stream failed"`相当）も同じコミットで更新する | opus-critic-a N6・opus-critic-b S-5 |
| **Task 3.1の説明の力点を訂正**: 「`resume_grace`のハードコード`0`を置き換える」ことは主眼ではない——`0`は「resume無効」ではなく`ResumableRelaySession::effective_resume_grace_secs`のdocが明記する「サーバー既定を要求する」という意味（`resume_grace: 0`＝“no preference”）であり、この置き換え自体は`#@isekai resume-grace`を尊重するための付随的な改善に過ぎない。実際にresumeを可能にする本質的な変更は、`connect_and_handshake`が返す4値のうち`connect_stun_p2p_with_round`が捨てている`conn`・`proof`・`effective_resume_grace_secs`を呼び出し元へ返すことと、`endpoint.rebinder()`を`endpoint`がスコープを抜ける前に確保しておくこと（`connect_via_relay_resumable`の既存パターンと同じ順序制約）。Task本文の記述順もこちらを主眼にする | opus-critic-a N5 |
| 行番号を`origin/main`（PR2マージ後）に合わせて全面的に更新（例: `run_stun_p2p_with_fallback`は`resume_loop.rs:307`、`resume_grace`ハードコードは`stun_p2p.rs:289-293`、`give_up`はPR1のB6修正で`stdout`引数を既に取らない） | opus-critic-a N8・opus-critic-b（両者指摘） |

**再検証で「変更不要」と確定した項目**（念のため記録）: Task 3.1の
S4スコープ拡張自体（`conn`/`proof`/`endpoint.rebinder()`/negotiated
grace を返す必要があるという判断）は正確。Android
`isekai-terminal-core`側の`connect_stun_p2p_on_socket`は独立した
公開関数であり、`connect_stun_p2p`/`connect_stun_p2p_with_fallback`/
`StunP2pConnection`の変更の影響を受けない（`StunP2pConnection`が
`pub`であるため、変更前にワークスペース全体の他の利用者を
grepで確認すること）。round 4で確定した「STUN経路のみ`give_up`に
マーカーを付ける」非対称設計自体は必要な決定のまま——ただし
このBLOCKING修正（短いgive-up境界）を適用して初めて、ADRが
述べる「発火頻度が下がるだけで無意味化はしない」という前提が
文字通り成立する（10日の既定値のままではPR2の軽量リトライ機構は
事実上到達不能になり、この非対称設計の前提が崩れていた）。
STUN P2PがIPv4限定である点は`connect_stun_p2p_with_round`が既に
そうなっているため、PR3による新規の後退ではない。

### Round 5フォローアップ（2026-09-02）— ユーザー指示「根本原因を
    考えたうえで非連続的な変更も考えて」を受けたroot-cause rework案
    ("Option B")の検討と却下

round 5でN1/B-1として提出した「PR3は正味の退行になる」という評価に
対し、ユーザーから根本原因の再検討と非連続的な代案の検討を求められた。
検討の結果、**round 5のN1/B-1評価自体が誤りを含んでいた**ことが
opus-critic-a・opus-critic-bへの再依頼で判明した。

提出したroot-cause rework案（"Option B"）: `run_resume_loop`のredial
処理自体をSTUN専用にし、再接続のたびに再STUN問い合わせ＋再ホール
パンチをしてから（`resume.rs:768`の`resume_on_connection`は接続
確立方法と既に疎結合になっているためRESUMEハンドシェイク自体は
再利用可能）、bare redialの代わりに使う、という設計。

opus-critic-a・opus-critic-bへ再度独立並行で評価を依頼した結果、
**両者が独立に同一結論——この案は到達可能性を一切改善せず、
むしろコストだけ増やす——に達し、Option Bは却下、round 5の
Task 3.4（短いgive-up境界）がそのまま正しい修正である、という
結論に至った**。詳細な理由は本ADR§2.3.0・§1.5補足に反映済み。
要点のみ記録:

| 発見 | 由来 |
|---|---|
| **round 5のN1/B-1「bare redialはNAT変化時に原理的に成功し得ない」は誤り**。`reconnect_and_resume`が新しいwildcardソケットからdialする挙動は、クライアント側のIP/NAT変化（Wi-Fi切替等、本ADRが対象とする最も典型的なトリガー）を正しく処理する——OSが現在のデフォルト経路にルーティングし、dial自体が新しいNATマッピングを副作用として作るため。サーバー側アドレスが安定していれば（§1.5の既存前提）、PR3は当初の目的通りbare redialのままで真のresumeを達成する | opus-critic-a・opus-critic-bが独立に同一結論 |
| **Option Bが到達可能性を改善しないことが判明**: `isekai-pipe serve`のホールパンチ（`--punch-peer`）は起動時の一度きりの動作であり、再接続してきたクライアントの新アドレスを学習して再パンチする手段が無い。クライアント側だけが再STUN・再ホールパンチしても、サーバー側NATの許可フィルタは更新されない——両者が互いの新アドレスを知る帯域外シグナリング（Androidの`bootstrap_via_ssh_with_punch`のような）が無い限り無意味。CLI側にはこの帯域外経路が存在しない（`our_observed_addr`はログに出るだけで実際どこにも送信されない） | opus-critic-b（`our_observed_addr`の全消費者をgrepして実証）・opus-critic-a（サーバー側`--punch-peer`の一度きり動作を実証） |
| **Option Bの実装コストも「改善ゼロ」に見合わない**ことが判明: 再試行のたびに750ms（`PUNCH_PROBE_COUNT`×`PUNCH_PROBE_INTERVAL`）の固定遅延＋公開STUNサーバーへの追加RTT＋新しい第三者障害点が乗る。`RelayTarget`は`run_resume_loop`内の6箇所（`reestablish_control_stream`・`WarmStandby::new_bound_to_interface`・`spawn_reconnect_signal`等）に配線されており「redialだけ差し替える」抽象化にならない。何より——**再パンチの有無で結果が変わることを示す差分テストが原理的に書けない**（サーバー側が一度しかパンチしないため） | opus-critic-a・opus-critic-b（両者独立に同一結論） |
| round 5のN1/B-1のseverityを「PR3実装がADRの目的に反する退行」から「サーバー到達不能時のみのハング」に格下げ。Task 3.4自体（短いgive-up境界）は変更なし——修正すべきはADRの説明（§2.3.0）であって設計ではなかった | opus-critic-a・opus-critic-b |
| §1.5に「CLIにはAndroidの`bootstrap_via_ssh_with_punch`にあたる帯域外シグナリング経路が無く、クライアント側の再パンチは単独では無意味」という補足を追加 | opus-critic-b |

**教訓**: 「根本原因を考える」ことが常に「より大きな変更」を
正当化するとは限らない——今回はむしろ、round 5で提出した評価
（既存設計だけでは正味の退行になる、という診断）自体が根本原因の
分析不足（サーバー側パンチが一度きりであることを未確認だった）に
基づく誤りで、掘り下げた結果「診断を訂正して元の対症療法（短い
give-up境界）を正しい理由で採用する」が正解だった、という事例。

### Round 4（2026-08-31）— タスク分解時にopus-task-reviewが発見した
    コード構造レベルの矛盾への対応

`TASKS_MIDSESSION_DISCONNECT_RECOVERY.md`を実装可能な粒度へ分解する
過程で、ADR設計そのものに残っていた4件のblocking矛盾が判明した
（ADRレベルのround 1〜3レビューは設計方針の妥当性を検証したが、
実際のbreak地点の網羅性やクレート境界のような実装細部までは
掘り下げていなかった）。

| 変更 | 由来 |
|---|---|
| **`remote_reported_exit: bool`を`BreakReason`列挙型に置き換え**。`relay_loop`のbreakは`RemoteExitReported`/`ClientGone`/`TransportDead`/`CloseDeadline`の最低4系統あり、`bool`1個では「クライアント側が先に切断した」ケースを「transport死」と誤判定し、同じholderを共有する他タブを巻き添えにする。`native/connect.rs`の`~.`エスケープ切断も同じ問題を持つ | B1・B2 |
| **PR3後のマーカー付与方針を明確化**: STUN経路は`run_resume_loop`の`give_up`にマーカーを付ける方針で確定し、relay経路（付けない）とは意図的に非対称にする。理由はSTUNのresume上限がrelayより低い（§1.5）ため、resume使い切り後も「再展開」より「もう一度確立し直す」方が有効な場合がある。PR2のUnix軽量リトライ機構がPR3後も無意味化(dead code化)しないようにする | B3 |
| **`retry_while_busy_other_session`の配置を訂正**: この関数は`isekai-pipe`crateのprivate関数であり、別バイナリの`isekai-ssh`（wrapper.rs）からは呼べない。「wrapper.rsのループでSTUN connectをこれで包む」という記述を削除し、STUN試行回数の抑制は(a) `isekai-pipe`内部の`retry_while_busy_other_session`適用（Task 2.10、既存スコープ通り）と(b) wrapper.rs側の`RetryConnectLightweight`総試行回数上限、の2つの独立した層に分離する | B4 |
| `shutdown.notify_waiters()`の発火条件を「`BreakReason::TransportDead`と分類され、かつ共有handleの生存確認（既存`handle_died`相当のチェック）でhandle自体が死んでいると確認できた場合のみ」に絞る。チャネル単体の異常閉鎖（handleは生きている）で他タブを巻き添えにしない | S9 |
| **孫プロセス実測手順を具体化**: 経路によって「正解」が逆になる（STUN=15秒で自然に落ちるのが正常、Relay=最大10日残るのが正常＝resume中）ため、実測は明示的にSTUN P2P経路を選んで行い、Relay経路での残存は対策対象外と明記する | S10 |

### Round 3（2026-08-31）— opus-critic-aのround 2再レビューへの対応

opus-critic-bはround 2で収束（「収束、blocking/significantなし」）。
opus-critic-aは同じround 2 draftにsignificant 1件・minor 3件を発見
（blockingなし。詳細:
[`ADR_MIDSESSION_DISCONNECT_RECOVERY_REVIEW_ROUND2.md`]の末尾に
round 3分も追記）。

| 変更 | 由来 |
|---|---|
| **§2.2.1のWindows mux判定方法を訂正**: `exit_code.is_some()`だけでは`ChannelMsg::ExitSignal`（シグナル終了）を`exit_code = None`のtransport死と誤判定してしまう。`remote_reported_exit: bool`という別変数を判別子にし、`ExitStatus`/`ExitSignal`のどちらでも`true`にする方式へ変更 | R2-S1 |
| **`outcome_summary`（`wrapper.rs:648-653`）への新variantアーム追加をPR1・PR2双方の作業項目に明記**。`.claude/rules/always-connects.md`の既存ルール。`Unknown`には`Unreachable`用文言をそのまま流用せず専用の中立的文言を用意する | R2-M1 |
| §2.2.2の孫プロセス対応（`prev_child_pid`/`ensure_process_terminated`）を、擬似コード・§4.2・§5の3箇所すべてで「実測で孫が残った場合のみ」の条件付きに統一 | R2-M2 |
| Windows mux経路でSTUN試行回数を数える状態をどこに置くか（プロセスごとに再spawnされるため）をPR2実装時の決定事項として明記 | R2-M3 |

### Round 1（2026-08-31）— opus 2体独立並行レビューへの対応

opus-critic-a・opus-critic-bの2エージェントに、互いの指摘を見せずに
round 0 draftを実コード裏取り込みでレビューさせた（詳細:
[`ADR_MIDSESSION_DISCONNECT_RECOVERY_REVIEW_ROUND1.md`]）。**両者が独立に
同じblocking項目に収束した**ことに加え、opus-critic-bへの追加深掘り依頼で
Windows mux機構の重大な発見（owner側がtransport死を`Frame::Exit(255)`に
「洗浄」しており、既存の`native::mux::run_with_reconnect`が判定材料の
到達前に無力化されている）を得た。round 0からの主な変更:

| 変更 | 由来 |
|---|---|
| **§1.3の根本原因説明を全面書き直し**。「両OSで理論上は既に1回だけ自動リカバリが走っている」はUnix限定の事実で、Windowsではゼロ回だったと訂正。line引用の誤りを修正（`resume_loop.rs:242-247`等） | B1・S2(引用) |
| **Windows mux経路の詳解を新設**。owner側の`Frame::Exit`洗浄が根本原因であり、修正はプロトコル変更ゼロで`run_with_reconnect`を素通しさせるだけで足りる可能性が高いと判明 | B3(深掘り) |
| **`err.chain()`によるdowncastの提案を撤回**。anyhowの`downcast_ref`は既にchainを辿るため、既存の`StaleTrustSignal`と同じ裸の形で十分 | B2 |
| **`recover_via_cross_family_fallback`との衝突を新設のB4として明記**し、mid-sessionマーカー検出時は即bailする設計に変更 | B4 |
| **非idempotentなリモートコマンドの再実行防止ガードを必須化**。`native/mux/mod.rs`に既存の同種ガード（`remote_command().is_none()`）を発見、流用する | B5 |
| **`give_up`のstdout close→outcome書き込みのレース(B6)を新設**。書き込み順序を不変条件として明記 | B6 |
| **Tier 1/Tier 2のコスト見積もりを訂正(B7)**。STUN P2Pの真resumeは`crate::resume::reconnect_and_resume`の既存流用で狭く実装できると判明。ユーザー判断によりTier 2を今回のスコープ(PR3)に格上げ | B7 |
| `run_resume_loop`へのマーカー付与を撤回（10日resumeを使い切った後の失敗は現行の`RebootstrapAndRetry`が正しい） | S1(旧) |
| STUN P2P経路のサーバー側セッションスロット消費・`retry_while_busy_other_session`未適用を新設(S2)。試行回数を3〜5回に制限する設計に変更 | S2 |
| `isekai-pipe connect`孫プロセスの孤児化リスクを新設(S1)。リトライ前に前回試行のプロセスを確実に終了させる設計を追加 | S1 |
| ctl-socket forwardの反復ごとのリークを新設(S3) | S3 |
| `ConnectOutcomeClass`のschema対応方針を訂正: bump不要は変わらないが、根拠を「サーバー側sha256一致」から「ローカル異ビルド間のdeserialize耐性」に訂正し、`#[serde(other)] Unknown`の追加を必須化 | S4 |
| **PR構成を3分割に変更**（PR1: 独立した安全な修正のみ、PR2: リトライループ本体、PR3: STUN P2P真resume）。ユーザーが2026-08-31に決定 | opus-critic-b提案 + ユーザー決定 |
| §7 Open Questionsの大半をレビューで解決済みとしてクローズし、round 2向けの残課題のみ残す | 全般 |

### Round 2（2026-08-31）— opus-critic-aのround 1再レビューへの対応

opus-critic-bはround 1で収束（「収束、blocking/significantなし」）。
opus-critic-aは同じround 1 draftに新規blocking 2件・significant 5件を
発見した——round 1で導入した設計自体（PR分割・新enum・Windows修正案）
に対する二次的な指摘であり、round 0→round 1の訂正が新たに生んだ
矛盾を突いている。

| 変更 | 由来 |
|---|---|
| **`ConnectOutcomeClass::Unknown`の扱いを訂正**: `NoRecoverableSignal`（復旧を諦める）ではなく`Unreachable`相当（`should_bootstrap`に応じてRebootstrapAndRetry/AutoBootstrapDisabled）に変更。round 1案は`always-connects.md`原則に対し現状より後退していた | R1-B1 |
| **PR1/PR2の境界を再設定**: `MidSessionDisconnectSignal`マーカー自体・そのSTUN 2箇所への付与・`ConnectOutcomeClass::MidSessionDisconnect`variant・B4のbail-out・Windows `Ok`経路でのoutcome claim修正を、すべてPR1からPR2へ移動。round 1案のままPR1だけ実装すると、mid-session切断が`Unreachable`に誤分類され「通信断のたびにサイレント再展開+見覚えのない2つ目のセッション」という**現状より悪い**退行を生むと判明したため | R1-B2 |
| **Windows mux修正の判定方法を具体化**: `owner.rs`が既に持つ`exit_code: Option<u8>`（`ChannelMsg::ExitStatus`受信時のみ`Some`）をそのまま判別子に使う設計へ変更。子プロセスの終了コード/シグナルを覗く必要はない | R1のOpen Question 2への具体案 |
| **client再接続とholder退場のレース対策を追加**: `RECONNECT_BACKOFF.initial`(500ms±jitter)が`HANDLE_HEALTH_POLL_INTERVAL`(1秒)より短く、生きている死にかけholderに当たり多重化なしdirect connectへ静かに劣化しうる。`relay_loop`がtransport死でbreakする際に`shutdown.notify_waiters()`を呼ぶ（`owner.rs:413`の既存パターンと同型）よう追加 | R1-S1 |
| **STUN経路の試行回数上限(3〜5回)をWindows muxの`RECONNECT_BUDGET`(24h)側にも適用**。round 1案のままだと非対称で、Windows側でこそセッションスロット枯渇が深刻になりうる | R1-S2 |
| **`#[serde(other)]`が`#[serde(flatten)]`越しでも機能するかをPR1で実証必須に**。`ConnectOutcome`は`#[serde(flatten)] class: ConnectOutcomeClass`であり、単体enumのテストでは不十分。動かない場合`class: String`+変換関数へフォールバック | R1-S3 |
| 行番号の誤りを修正（`wrapper.rs:618-619`、`wrapper.rs:719-725`） | R1-S4 |
| **孫プロセス孤児化(S1)の扱いを「断定」から「実測してから決める」に変更**。`.status()`が孫を待たないことと孫が実際に生き残ることは別問題（`ssh(1)`自体がProxyCommandをkillする可能性がある）。実装時にまず`pgrep`で実測し、残らなければ§2.2.2の`ensure_process_terminated`機構ごと不要という判断を許容する | R1-S5 |
| PR3の受け入れ条件に「STUN経路もrelayと同じくresume使い切り後は`RebootstrapAndRetry`（マーカー無し）」を明記し、Open Questionから格上げ | M-1 |
| §4.1の文言を訂正: relay経路は元々10日resumeでカバーされ「即死」ではなかったため、本ADRが改善するのは主にSTUN P2PとWindows mux経路である | M-2 |
| 改訂履歴表とpseudocode内のコメントで「B2」/「B5」采番が食い違っていたのを統一 | M-3 |

### Round 0（2026-08-31）— 初稿

本セッションでのユーザー報告の調査結果に基づく初稿。

---

## 1. Context

### 1.1 報告された症状

`isekai-ssh <host>`で対話セッションを開いた後、Wi-Fi切断・スリープ復帰・
セルラー⇔Wi-Fi切り替えのような**セッション確立後の**ネットワーク瞬断が起きると、
セッション全体が即座に終了し、ローカルのシェルへ戻される。ユーザーは
`isekai-ssh <host>`を手動で再実行する必要がある。同じ状況で比較対象の`tssh`
（trzsz-ssh）はセッションを終了させず、黙って再接続してセッションを継続する。
Unix（`ssh(1)` ProxyCommand経由）・Windows（`russh`ネイティブ経由）の両方で
同じ症状が再現する。

### 1.2 なぜ両OSで同じ症状が起きるか

`isekai-ssh`のWindowsネイティブ経路は独自にQUIC接続を張っているわけではない。
`isekai-ssh/src/native/child_stdio.rs`冒頭のコメントが明記する通り、Unix版の
`ssh(1)` ProxyCommandが起動するのと**全く同じ`isekai-pipe connect`バイナリ**を
子プロセスとして起動し、そのstdin/stdoutを`russh_stream_session`に生ソケット
代わりに渡しているだけである:

> `isekai-pipe connect`'s own route selection, resume-on-disconnect, and
> `ConnectOutcome` bookkeeping are completely unchanged — the native path
> just runs the same binary as a child process instead of leaving that job
> to a real `ssh(1)`.

したがって本ADRが対象とする根本原因は`isekai-pipe connect`という単一の
中継プロセスの挙動であり、修正は1箇所で両OSに効く。

### 1.3 根本原因（コード上の裏付け、round 1で訂正済み）

問題は独立した複数の設計判断・実装上の見落としが重なって生じている。
round 0では「3つの設計判断」としていたが、round 1レビューでUnixと
Windowsは**別の理由で**症状が起きていることが判明したため、まずOS別に
分けて説明する。

#### (a) STUN P2P経路にはセッション確立後の再接続機構が一切ない（両OS共通）

`isekai-pipe/src/resume_loop.rs:242-247`に明記の通り（round 0はこれを
誤って`connect.rs:640-648`と引用していた）:

> STUN P2P has no resume/control-stream concept ... there is no
> `run_resume_loop` step here: the winning candidate's stream goes straight
> into `relay_stdio`, exactly like the legacy single-candidate path already
> does.

`relay_stdio`（`isekai-pipe/src/resume_loop.rs:66`）は読み書きが1回失敗したら
即座に`Err`を返すだけの単純な双方向パイプで、リトライも再接続もない。
QUICクライアント側の`max_idle_timeout`は15秒（`isekai-transport/src/system.rs:24`）
なので、通信不能になってから最大15秒でこの`Err`が発生する。呼び出し元は
`run_stun_p2p_with_fallback`（`resume_loop.rs:247`、呼び出しは`connect.rs:624`）
と、単一candidate経路の`CandidateRoute::StunP2p`アーム（`connect.rs:680`）
の2箇所。

一方、Relay経由（Tailscale等）は`run_resume_loop`
（`isekai-pipe/src/resume_loop.rs:986`）が切断検知→バックオフ付き再接続
（`reconnect_and_resume`）→replay bufferによるバイト単位の継続、という
本格的なresumeを、最大`DEFAULT_RESUME_GRACE_SECS`=864,000秒=**10日**
（`isekai-pipe-core/src/lib.rs:70`）にわたり持つ。同ループのコメント
「tssh風のライブ再接続表示」（`resume_loop.rs:1006`）が示す通り、この設計は
まさに`tssh`の体験を再現するために作られたものであり、**STUN P2Pはそこから
意図的に除外されている**（`isekai_transport::stun_p2p`のモジュールdoc:
"resume support lands in S-4a onward" — 着手されていないのは事実だが、
round 1で判明した通り、実は必要な部品の大半が既に別の形で存在している。
§1.6参照）。

#### (b) Unix（`ssh(1)` ProxyCommand経由）: 既存のリカバリ機構は理論上1回だけ
    走るが、遅延なしの再展開という誤った対応をする

`isekai-pipe-core/src/outcome.rs`の`ConnectOutcome`は、`isekai-ssh`のwrapper
プロセスが`ssh`終了後に読む唯一のサイドチャネルである。そのモジュールdocは
次のように明言する:

> a `run_connect` failure only ever happens before any SSH byte ever flows
> (this is `ssh`'s `ProxyCommand`; a remote shell command that ran and
> exited non-zero never touches this path at all)

しかし実際のコードでは、この前提は**すでに成立していない**。
`isekai-pipe/src/connect.rs:439-459`の`connect_command`は、
`run_connect(launch).await`が`Err`を返した場合、それがハンドシェイク前の
失敗かセッション確立後（pump段階）の失敗かを一切区別せず、無条件で
`write_connect_outcome_for_wrapper`を呼ぶ。`run_stun_p2p_with_fallback`は
`relay_stdio(connection.stream).await`の`Result`をそのまま`run_connect`の
戻り値として伝播させているため、**STUN P2Pのmid-session切断も
`ConnectOutcome`ファイルを書く**——`Unreachable`として分類される
（`write_connect_outcome_for_wrapper`の分岐: `StaleTrustSignal`以外は
すべて`Unreachable`）。

つまりUnixの`isekai-ssh`のwrapper（`wrapper.rs::run_ssh_with_connect_failure_recovery`）
は、ssh(1)がProxyCommandの異常終了を255で終了として伝えてくれる
（`status.success()`がfalseになる）おかげで、mid-session切断についても
一応「シグナルあり」と判定し、`ConnectFailureRecoveryAction::RebootstrapAndRetry`
（`wrapper.rs:709-717`）を選ぶ。**理論上は既に1回だけ自動リカバリが走る
——ただしUnixに限る**（Windowsについては(c)で後述する通り、これは成立しない）。
ではUnixでもなぜ「即死」に見えるのか——(d)がその理由。

#### (c) Windows: 転送層の死が正常終了として「洗浄」され、復旧判断に
    一度も到達しない

Windowsの`isekai-ssh <host>`は**既定で常に**マルチプレクスclient
として動く（`main.rs:225-229`、opt-inフラグなし。単一プロセス直結に
落ちるのはholder起動失敗など例外ケースのみ）。この経路でmid-session
切断が起きたときの実際の流れ:

1. holder配下の`isekai-pipe connect`が死ぬ（STUN P2Pなら15秒のQUIC
   idle timeout）
2. owner側`native/mux/owner.rs:585-588`: `channel.wait()`が
   `Close`/`None`を返し`break`
3. **`native/mux/owner.rs:703-708`**:
   `let final_code = exit_code.unwrap_or(255); write_frame(writer,
   &Frame::Exit(final_code))` — 転送層の死を「リモートシェルが
   exit status 255で正常終了した」体裁のExitフレームに**変換して
   clientへ送ってしまう**
4. client側`native/mux/client.rs:385-388`: `Frame::Exit(255)`受信 →
   `ClientOutcome::Exited(255)` → `DispatchOutcome::Done(255)`
5. **`native/mux/mod.rs:308`**: `DispatchOutcome::Done(code) => return
   Ok(code)` で即return
6. 単一プロセス直結側でも同型の問題がある: `run_shell_io_loop_inner`
   が接続断を`Some(ChannelMsg::Close) | None => break`
   （`native/connect.rs:1195`）として扱い、`Ok(NO_EXIT_STATUS_RECEIVED
   /* 255 */)`（`:1110`,`:1204`）を返す

`native/connect.rs:346-348`の`drive_connect_recovery`は`ops.attempt()`が
`Ok(exit_code)`を返した時点で即returnし、`claim_outcome`（＝`ConnectOutcome`
ファイルの確認）を呼ぶのは`Err`のときだけである。**上記いずれの経路でも
戻り値は`Ok(255)`であって`Err`ではない**ため、Windowsでは復旧判断の
入口にすら到達しない——(b)で述べたUnixの「理論上1回は走る」自動リカバリは
Windowsでは**実質ゼロ回**である。副作用として、`ConnectOutcome`ファイルは
claimされないままruntime_dirに溜まり続ける。

重要な事実: Windowsのmux経路には、mux holder自体が死んだ場合の復旧機構
（`native::mux::run_with_reconnect`、`native/mux/mod.rs:248-341`）が
**既に実装済み**である——24時間のリトライ予算(`RECONNECT_BUDGET`)、
ジッター付き指数バックオフ、安定接続後の予算リセット
(`RECONNECT_STABLE_THRESHOLD`)、Ctrl-Cでの中断、そして非idempotentな
リモートコマンドの再実行を防ぐガード(`has_remote_command`)まで揃って
いる。しかしこの機構は「ownerプロセス自体が異常死してExitフレームを
送れなかった場合」の`OwnerLost`だけを検知対象にしており、上記3の通り
transportの死は**行儀よくExitフレームへ変換されてから**ownerが退場する
ため、`OwnerLost`分岐には一度も到達しない。**つまりこの既存の復旧機構は
バイパスされているのではなく、判定材料が届く前に握りつぶされている。**
（§2.2で詳述する通り、Windowsの修正はこの「握りつぶし」を止めるだけで
足り、新しいリトライループを別途書く必要が無い可能性が高い。）

#### (d) 既存のリカバリは「古いデプロイの再展開」専用に設計されており、一過性の
    ネットワーク瞬断には構造的に不向き（Unixで一応走る1回のリカバリの中身）

`RebootstrapAndRetry`の実装（`wrapper.rs:628-638`）は:

1. 遅延なし・即座に`bootstrap_and_register(plan, resolution,
   TofuConfirmation::Silent)`を呼ぶ——これは対象ホストへ**別のSSH接続**
   （鍵/パスワード認証)を張り直し、`isekai-pipe serve`を**再展開**する
   重い操作である。
2. ネットワークがまだ復旧していない場合、この再展開用SSH接続自体が
   即座に失敗し、`print_bootstrap_failure_guidance`を出して
   `Err`のまま関数全体が終了する（バックオフも再試行もなし）。
3. 仮に再展開が成功しても、リトライは**1回きり**（ループではない）。
   同じ関数呼び出しの中で`run_ssh_once`をもう一度呼ぶだけなので、
   2回目の切断にはもう対処できない。
4. どの分岐でも、ユーザーが見ていた対話ターミナル（1回目の`ssh(1)`/
   `russh`セッション）は**既に終了済み**であり、リカバリが成功しても
   全く新しい対話セッションが（`isekai-ssh`プロセスの中で）改めて
   起動されるだけである。

この設計は本来「キャッシュ済みデプロイ情報が古い/死んでいる」ケース
（`.claude/rules/always-connects.md`が定義する主眼、`ISEKAI_PIPE_DESIGN.md`
§8 Epic N/N-2）向けに作られたものであり、**再展開そのものは正しい判断
だが、それを一過性のネットワーク瞬断（サーバー側の`isekai-pipe serve`は
生きたまま、クライアント側の経路が一時的に切れただけ）に対して
遅延なしで即座に適用する**ため、ネットワークがまだ死んでいる間に
飛んできたこのリトライも当然失敗し、結果として「即死」に見える。
再展開自体は本質的に不要な処理でもある——サーバー側のヘルパーは
生きているので、STUN P2Pなら同じ`peer_addr`へ、Relayなら同じ
`RelayTarget`へ、単に再ダイヤルするだけで十分なことが多い
（§2.3で詳述）。

### 1.4 「セッション確立後」であることをどう検出しているか（現状は検出していない）

`ConnectOutcomeClass`は現在`StaleTrust`/`Unreachable`の2値しかなく、
「SSHバイトが実際に流れた後の失敗か」を区別する情報がそもそも存在しない。
本ADRの核心はこの区別を導入することである（§2.2）。ただしこれは**Unix側の
入口**を直す話であり、Windows側は(c)で述べた「洗浄」そのものを止める方が
先に必要になる。

### 1.5 なぜSTUN P2Pの「フルre-dial」は多くの場合で機能すると考えられるか
    （前提条件付き、round 1で補正）

`isekai_transport::stun_p2p`のモジュールdocによれば、STUN P2P接続確立は
「毎回そのソケットで新たにSTUNへ自分の観測アドレスを問い合わせる」処理を
含む（キャッシュしない）。つまり、クライアント側のネットワークが変わっても
（Wi-Fi再接続でNATマッピングが変わった、Wi-Fi⇔セルラー切替など）、
確立処理をそのまま最初からやり直すだけで新しい観測アドレスが自動的に
反映される。

**ただし前提条件がある**（round 1で追加）: `our_observed_addr`は相手に
帯域外で伝わらない（モジュールdocが「out-of-band exchangeはこの層の
対象外」と明記）ため、再ダイヤル先の`target.peer_addr`は
`PersistentProfile`にキャッシュされた**古い**アドレスのままである。
フルre-dialが効くのは「サーバー側が安定した/公開アドレスで待ち受けて
おり、クライアント側からのpunchだけで足りる」構成に限られ、対象ホスト
（サーバー）側のアドレスも同時に変わった場合（対称NAT越しの相互punchが
要る構成）には効かない。この場合のみ、真の再ランデブー（帯域外
シグナリングのやり直し）が必要になる——後述§1.6・§2.4のPR3のスコープ外。

### 1.6 round 1での訂正: STUN P2Pの「真のバイトレベルresume」は、
    当初の見積もりより遥かに狭い変更で実現できる

round 0では、STUN P2Pへの真resumeの実装を「Relay向けのresumeプロトコル
一式をP2P間で再ランデブー込みで再現する規模」と見積もり、Tier 2として
先送りしていた。round 1レビューでこの見積もりは誤りだったと判明した。

`isekai-transport/src/stun_p2p.rs:129-133`のモジュールdocが明記する通り:

> Resume support for a connection established this way still goes
> through the plain `crate::resume::reconnect_and_resume` against a
> synthesized `RelayTarget{helper_addr: target.peer_addr, ..}` — see
> that Android transport's own module docs for why a bare redial (no
> re-STUN/re-punch) is this mode's accepted resume-capability ceiling.

**Androidの`isekai-terminal-core`は、STUN P2Pで確立した接続に対して
既にこの機構でresumeを行っている。** CLI（`isekai-pipe`）側で欠けて
いるのは次の狭い範囲だけである:

1. `connect_stun_p2p_with_round`が`resume_grace`を`0`にハードコードし
   `_conn`（`AnyMuxConnection`）を捨てている（`stun_p2p.rs:290-292`、
   コメント "No resume support on this path (module docs), so there is
   no grace period to request"）。
2. `run_stun_p2p_with_fallback`（`resume_loop.rs:247-252`）が
   `run_resume_loop`ではなく単純な`relay_stdio`を呼んでいる。

この2点を直し、STUN P2P確立後に`RelayTarget{helper_addr: target.peer_addr,
..}`を合成して`run_resume_loop`（既存のRelay向けループ）に渡せば、
Androidと同じ「バイトレベルresume（scrollback/未確認バイトの継続）」が
CLIでも実現できる可能性が高い。ユーザー判断により、これを**PR3として
今回のスコープに含める**（§2.4）。ただし§1.5の前提条件（サーバー側
アドレスが安定していること）を超える真の再ランデブーは、引き続き
本ADRのスコープ外とする。

---

## 2. Decision（提案する設計、round 1: PR1/PR2/PR3の3段構成）

ユーザー判断（2026-08-31）により、以下をそれぞれ独立にCI green→
マージ可能な3本のPRとして実装する。PR2はPR1の上に、PR3はPR2の上に
積む（依存順）。

### 2.1 PR1: 独立した安全性の修正（リトライループなし、新enum/マーカーなし）

新しいリトライ機構・新しい`ConnectOutcomeClass` variant・新しい
マーカー型を一切導入せず、既存の`RebootstrapAndRetry`機構自体の
信頼性を上げる、リスクの低い修正群に厳密に限定する。**round 2で
R1-B2により範囲を訂正**: round 1案は「B4のbail-out」「Windowsの
`Ok`経路でのoutcome claim」もPR1に含めていたが、どちらも
`MidSessionDisconnect`の存在を前提にした修正であり、そのvariantが
まだ無いPR1単独でclaim側だけ先行させると、mid-session切断が
`Unreachable`に誤分類され「通信断のたびにサイレント再展開SSH+
見覚えのない2つ目の対話セッション」という**現状より悪い**退行を
生む。両方をPR2へ移した。PR1に残るのは以下の2点のみ:

1. **B6: `give_up`の書き込み順序を修正**（`resume_loop.rs:812-829`）。
   `stdout.shutdown()`を`ConnectOutcome`ファイル書き込み完了後に
   遅らせる（またはoutcome書き込み自体を`give_up`内へ前倒しする）。
   「outcomeを書く」が「stdoutを閉じる」より必ず先、という不変条件を
   コメントで明記する。Windowsの`native/connect.rs:452-462`が同じ
   問題に対し1秒のgraceで対症療法していることも合わせて記録し、
   Unix側にも将来同種の保護が要るかを検討する。
2. **S4: `ConnectOutcomeClass`のdeserialize耐性**。`#[serde(other)]
   Unknown`variant相当を追加し、`claim_connect_outcome`が未知タグに
   対して`Err`で invocation 全体を殺さないようにする
   （`wrapper.rs:618-619`——round 1は`:574-575`と誤記していた——の
   `?`を見直す）。理由は「サーバー側sha256一致」ではなく、
   「ローカルの`isekai-pipe`（`--isekai-pipe-path`で差し替え可能）と
   `isekai-ssh`が異なるビルドになりうるため」（S4、round 0の誤った
   根拠付けを訂正）。**round 2で追加(R1-S3)**: `ConnectOutcome`は
   `#[serde(flatten)] pub class: ConnectOutcomeClass`（`outcome.rs:60-61`）
   であり、flatten越しの内部タグ付きenumは`FlatMapDeserializer`を
   経由する——単体enumで`#[serde(other)]`が動くことと、flatten越しで
   動くことは別問題。**`ConnectOutcome`構造体全体を通した「未知タグ→
   `Unknown`」ラウンドトリップの単体テストをPR1に必須で追加し、
   CIで実証する。動作しない場合は`class: String`+手動変換関数へ
   フォールバックする**（この判断はPR1実装時にCIで確定させ、ADRの
   再改訂は不要）。
3. **R2-M1: `outcome_summary`（`wrapper.rs:648-653`）への`Unknown`
   アーム追加**。`.claude/rules/always-connects.md`が「新しい
   `ConnectOutcomeClass`を追加する場合は`wrapper.rs::outcome_summary`
   にもメッセージを足すこと」と明示的に要求している既存ルール。
   `Unknown`に既存の`Unreachable`用文言（"the cached deployment
   could not be reached … run `isekai-ssh init` manually"）を
   そのまま流用すると誤誘導になるため、専用の中立的な文言
   （例: "isekai-pipe connect reported an outcome this isekai-ssh
   build doesn't recognize"）を追加する。

### 2.2 PR2: リトライループ本体

#### 2.2.1 Windows: 新ループではなく、既存`run_with_reconnect`への
    素通しを実現する（B3、round 2でR1-S1の判定方法・レース対策を追加）

Windowsのmux経路（既定）では、**新しいリトライループを書かない**。
根本原因は`native/mux/owner.rs:703-708`が転送層の死を
`Frame::Exit(255)`に「洗浄」していることであり（§1.3(c)）、
修正はここ1箇所。

**判定方法（round 2→3→4で段階的に修正）**: round 2は
`exit_code: Option<u8>`をそのまま判別子にし、round 3は
`ChannelMsg::ExitSignal`の見落とし（シグナル終了が`exit_code=None`に
落ちてしまう）を修正するため`remote_reported_exit: bool`に変えた。
**round 4でさらに訂正**: `relay_loop`のbreakは実際には最低4系統
あり、`bool`1個では区別しきれない:

- **`RemoteExitReported`**: `ChannelMsg::ExitStatus`または
  `ChannelMsg::ExitSignal`を受信——リモートが終了理由を喋った。
- **`ClientGone`**: このclient接続がリモートchannelより先に終わった
  （`Some(Ok(None)) | None`、`owner.rs:560`付近）、またはctl arm側の
  `write_frame(...)`が失敗した（`owner.rs`の該当2箇所）——**このclient
  自身が先に消えただけ**であり、holderやtransportは無関係に生きて
  いる可能性が高い。
- **`TransportDead`**: 上記いずれでもなく`Close | None`でbreak
  （`owner.rs:583`付近）——本当に転送層が死んだ可能性が高いケース。
- **`CloseDeadline`**: `shutdown_close_deadline`分岐（`owner.rs:693-698`、
  ローカルEOF後にリモートがcloseを確認しなかった＝ユーザーが意図的に
  終了した文脈）。

`BreakReason`列挙型を導入し、各break地点に正しい理由を割り当てる
（この列挙自体の導入は`native/connect.rs::run_shell_io_loop_inner`
とも共有される概念だが、break地点の集合はowner側と単一プロセス側で
異なる——単一プロセス側には`~.`エスケープ切断やstdin書き込みエラーが
別途ある。詳細は実装タスク側の先行タスクとして切り出す）。

最終的な判定:

- `RemoteExitReported` → 従来通り`Frame::Exit(code)`を送る
  （`ExitSignal`の場合はssh(1)の慣習に合わせ`exit_code`を255扱いに
  する）。
- `TransportDead` → `Frame::Exit`を送らず、後述のレース対策
  （ただし共有handleの生存確認込み、下記参照）を行った上で
  `Ok(())`を返す。
- `ClientGone`・`CloseDeadline` → 従来通り`Frame::Exit`を送る側に
  倒す（`ClientGone`をtransport死扱いにすると、このclientが単に
  先に終了しただけのケースで、同じholderを共有する**他タブまで
  巻き添え**にする。`CloseDeadline`をtransport死扱いにすると、
  正常な`exit`の後に無駄な再接続が走る）。

単一プロセスfallback側（`native/connect.rs`の`run_shell_io_loop_inner`、
`:1195`付近、`~.`エスケープ切断・stdin書き込みエラーを含む）にも
同じ`BreakReason`方式の手当てが必要。

これにより`client.rs`の既存`OwnerLost`分岐が設計通り発火し、
`run_with_reconnect`（`native/mux/mod.rs:248-341`）の
`RECONNECT_BUDGET`(24h)・`RECONNECT_BACKOFF`・
`RECONNECT_STABLE_THRESHOLD`・`wait_or_abort`（Ctrl-C）・
`has_remote_command`ガードが**すべてタダで働く**。

**レース対策（round 2で追加、round 4でS9により発火条件を絞った）**:
`RECONNECT_BACKOFF.initial`=500ms（jitter込み375〜625ms、`mod.rs:142`）
は`HANDLE_HEALTH_POLL_INTERVAL`=1秒（`owner.rs:81`）より短い。
`Frame::Exit`送出をやめるだけだと、clientが最短375msで再dispatchする
一方、holderが自身の死（共有handleの死）を検知して実際に退場するのは
最大1秒後になりうるため、その隙間で再接続したclientが「生きている
死にかけholder」に`Rejected`され、`client.rs:254`のコメント通り
多重化なしのdirect connectへ静かにフォールバックしてしまう（＝再接続
には成功するがmux機能が黙って失われる）。

対策として`shutdown.notify_waiters()`を呼ぶが、**発火条件を
`BreakReason::TransportDead`と分類されたことだけに置いてはいけない
（S9）**: チャネル1本だけが（例えばsshdのチャネル数上限や個別
ポリシーで）異常閉鎖しても共有handle自体は生きている場合があり、
この場合に`notify_waiters()`を呼ぶと、無関係な他タブまで巻き添えで
`OwnerLost`扱いにしてしまう。`owner.rs:413`の既存パターン
（channel open失敗）は「共有handleが死んでいる証拠」という明示的な
根拠を伴っていたのに対し、mid-sessionのchannel closeにはその根拠が
無い。**`BreakReason::TransportDead`と分類され、かつ既存の
`handle_died`（`owner.rs:216-225`）相当の生存確認で共有handle自体が
死んでいると確認できた場合にのみ**`shutdown.notify_waiters()`を
呼ぶ——これにより`owner.rs:228-234`のhandle死亡検知分岐が即座に
発火し、holderが健康ポーリングを待たずに退場する。handleが生きている
場合は、このチャネル単体のエラーとしてこのclientにだけ報告する
（holder全体には影響させない）。

プロトコル（`Frame`列挙体）へのフィールド追加は行わない——holderは
常駐プロセスであり、新しい`isekai-ssh`バイナリと古いholderが
共存しうるため、プロトコル変更は互換性リスクを持つ
（`mod.rs`のmodule docsが言う "a small versioned frame protocol"）。
上記の修正は`relay_loop`内のbreak理由の分岐と`shutdown.notify_waiters()`
の追加呼び出しだけで済み、`Frame`にも`client.rs`にも一切触れないため、
この互換性リスクを回避できる。

単一プロセス直結にfallbackした場合（`native/connect.rs`）は、
`run_shell_io_loop_inner`の`Some(ChannelMsg::Close) | None => break`
（`:1195`）も同様に、transport死を示す情報を`Ok(255)`に潰さず、
`connect_attempt`まで伝播させ、PR2で追加する`claim_outcome`の
`Ok`経路対応（後述）と組み合わせて、Unixと同じ
`decide_connect_failure_recovery`ベースの経路へ合流させる。

**STUN経路の試行回数上限をWindows muxにも適用（round 2で新規、R1-S2）**:
`run_with_reconnect`の`RECONNECT_BUDGET`(24h)をそのまま使うと、
STUN P2P経由のholderが繰り返し死ぬ状況では最悪8000回超の再接続が
起こりうる——`OwnerLost`のたびに新しいholder→新しい`isekai-pipe
connect`→新しい`random_session_id()`が生まれるため、後述§2.2.2の
S2（サーバー側セッションスロット枯渇・他タブへの巻き添え立ち退き）は
Windows mux経路でこそ深刻になる。§2.2.2で導入する「STUN P2Pの
connectは試行回数を3〜5回で頭打ちにし`retry_while_busy_other_session`
で包む」対策を、Windows mux経由で`isekai-pipe connect`を再spawnする
際にも同様に適用する（`run_with_reconnect`の24hバジェット自体は
そのまま——OwnerLostの再接続試行回数を制限するのではなく、その中で
`isekai-pipe connect`がSTUN P2Pをダイヤルする回数を制限する）。

**PR2実装時に決める点（round 3で追加、R2-M3）**: Windows mux経路では
`isekai-pipe connect`が`OwnerLost`のたびに**新しいプロセス**として
起動されるため、試行回数を数える状態をプロセス内カウンタに置いても
常に1から始まってしまい機能しない。`run_with_reconnect`側の
`attempt`カウンタを環境変数等で子プロセスへ渡すか、あるいは
`isekai-pipe connect`内の`retry_while_busy_other_session`
（180秒バックオフ）だけで実用上十分と割り切るかを、PR2実装時に
決定する。

#### 2.2.2 Unix・Windows単一プロセスfallback: 新しいシグナルと
    リトライループ

**round 2で範囲を明確化(R1-B2)**: 以下の新シグナル定義に加え、
§2.1で述べた通りB4（`recover_via_cross_family_fallback`のbail-out）と
Windows `native/connect.rs:346-348`の`drive_connect_recovery`の
`Ok`経路でも`ConnectOutcome`をclaimする修正は、両方とも本PR2に含む
（round 1はどちらもPR1に置いていたが、`MidSessionDisconnect`が
存在しないPR1単独では実装不能、または実装すると悪化するため移動した）。

**新シグナル**: `isekai-pipe-core/src/outcome.rs`の
`ConnectOutcomeClass`に3つ目のvariantを追加する:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "kebab-case")]
pub enum ConnectOutcomeClass {
    StaleTrust,
    Unreachable,
    MidSessionDisconnect,
    #[serde(other)]
    Unknown, // PR1のS4対応。デシリアライズ時の未知タグをここへ吸収する
}
```

**検出方法**（round 1でB2により修正——`err.chain()`は不要）:
既存の`StaleTrustSignal`マーカー型パターン（`connect.rs`の
`StaleTrustSignalSource`/`attach_stale_trust_signal`、**裸の**
`err.downcast_ref::<T>()`——anyhowの`downcast_ref`は`.context(...)`で
包まれたchainを内部で自動的に辿るため、chainを手動で辿る必要はない）
をそのまま踏襲する。新しいゼロサイズマーカー型
`pub(crate) struct MidSessionDisconnectSignal;`を定義し、以下の
**STUN P2Pの2箇所のみ**に`.map_err(|e| e.context(MidSessionDisconnectSignal))`
を挟む（round 1でS1(旧)により訂正——`run_resume_loop`には付けない。
理由は次段落）:

- `run_stun_p2p_with_fallback`（`resume_loop.rs:247`）の`relay_stdio(...)`
- 単一candidate経路の`CandidateRoute::StunP2p`アーム
  （`connect.rs:680`）の`relay_stdio(...)`

`run_relay_resumable`/`run_relay_resumable_with_fallback`が呼ぶ
`run_resume_loop`の`give_up`には**マーカーを付けない**。理由
（S1(旧)）: relay経路は`run_resume_loop`内部で既に最大10日
（`DEFAULT_RESUME_GRACE_SECS`）resumeを試みており、そこから漏れて
きたエラーは本質的に terminal（helper再起動によるUnknownSession確定
等）である——このケースでは現行の即時`RebootstrapAndRetry`（再展開）
が引き続き正しい。

`write_connect_outcome_for_wrapper`の分類ロジック:

```rust
let class = if err.downcast_ref::<isekai_transport::StaleTrustSignal>().is_some() {
    ConnectOutcomeClass::StaleTrust
} else if err.downcast_ref::<MidSessionDisconnectSignal>().is_some() {
    ConnectOutcomeClass::MidSessionDisconnect
} else {
    ConnectOutcomeClass::Unreachable
};
```

`outcome.rs`のモジュールdocコメント（「a `run_connect` failure only
ever happens before any SSH byte ever flows」という誤った前提）を、
新しい2分類（pre-handshake failure / mid-session disconnect）を
正しく説明する記述へ書き換える。

**R2-M1（PR1と同じ規律をPR2にも適用）**: `outcome_summary`
（`wrapper.rs:648-653`）に`MidSessionDisconnect`用のアームも追加する
（`.claude/rules/always-connects.md`の既存ルール）。

**リカバリの意思決定**: `decide_connect_failure_recovery`を
`Option<&ConnectOutcomeClass>`ベースへ変更する:

```rust
pub(crate) enum ConnectFailureRecoveryAction {
    NoRecoverableSignal,
    AutoBootstrapDisabled,
    RebootstrapAndRetry,
    RetryConnectLightweight, // 新設。round 1でM3により単純化
                             //（段階的降格ロジックは削除、下記参照）
}

pub(crate) fn decide_connect_failure_recovery(
    outcome_class: Option<&ConnectOutcomeClass>,
    should_bootstrap: bool,
) -> ConnectFailureRecoveryAction {
    match outcome_class {
        None => ConnectFailureRecoveryAction::NoRecoverableSignal,
        Some(ConnectOutcomeClass::MidSessionDisconnect) => ConnectFailureRecoveryAction::RetryConnectLightweight,
        // Unknown(将来isekai-pipeが追加する未知のクラス)もUnreachableと同じ
        // 扱いにする — round 2でR1-B1により訂正。「未知タグでハードエラーに
        // しない」(S4)の意図は「未知タグでも復旧を試みる」であって「未知タグ
        // では復旧を諦める」ではない。旧来のwrapper.rs:621は
        // `outcome.is_some()`だけで復旧を試みており、outcomeファイルが
        // 存在する(=何かがrun_connectを失敗させた)という事実だけで
        // 十分だった。round 1案のNoRecoverableSignalは
        // .claude/rules/always-connects.mdに対して現状より後退していた。
        Some(_) if !should_bootstrap => ConnectFailureRecoveryAction::AutoBootstrapDisabled,
        Some(_) => ConnectFailureRecoveryAction::RebootstrapAndRetry,
    }
}
```

（round 1のM3指摘により、round 0にあった「軽量リトライを何回か試して
ダメなら`RebootstrapAndRetry`に降格する」という時間ベースの分岐を
削除した。理由: 軽量リトライで再ダイヤルした2回目の試行がハンドシェイク
前に失敗すれば、それは通常の`Unreachable`として分類され、次の
`run_ssh_once`呼び出しで自然に`RebootstrapAndRetry`が選ばれる。クラス
遷移そのものが既にエスカレーション機構であり、別途window変数を持つ
必要が無い。）

**`should_bootstrap`との関係（Q2で確定）**: `MidSessionDisconnect`の
場合、`should_bootstrap`の値に関わらず軽量リトライは常に試みる
（既存の`--isekai-no-bootstrap`は「勝手にSSH再展開しない」契約であり、
「既知のtargetへ黙って再ダイヤルする」ことまでは禁止しないと解釈する
——両レビュアーが支持。新フラグは不要）。

**リトライループの骨格**（`run_ssh_with_connect_failure_recovery`・
Windows単一プロセスfallback側の対応する関数を、単発呼び出しから
ループへ拡張）:

```
let mut attempt = 0u32;
let mut lost_since: Option<Instant> = None; // native/mux/mod.rsのlost_sinceと同じ役割
// S1: 孫プロセスの孤児化対策。round 3(R2-M2)で明記した通り、これは
// 「§2.2.2のS1実測で孫が実際に残ると判明した場合のみ」必要になる
// 条件付きの機構——以下は残った場合の実装イメージ。
let mut prev_child_pid: Option<Pid> = None; // 実測で不要と判明したら丸ごと削除

loop {
    if let Some(pid) = prev_child_pid.take() {
        // S1(残る場合のみ): 前回試行のisekai-pipe connect(孫)プロセスが
        // 生きていれば確実に終了させてからでないと、新しいセッションの
        // ダイヤルがBUSY_OTHER_SESSION(180秒しか耐性が無い)に当たりうる。
        ensure_process_terminated(pid);
    }
    (status, child_pid) = run_ssh_once(plan, resolution, intent, runtime_dir)
    prev_child_pid = Some(child_pid)
    if status.success(): return Ok(0)

    // B5: 非idempotentなリモートコマンドは絶対に再実行しない
    // (native/mux/mod.rsの既存has_remote_commandガードと同じ規律)。
    let has_remote_command = plan.remote_command().is_some();

    outcome = claim_connect_outcome(...)
    action = decide_connect_failure_recovery(outcome.class, should_bootstrap)
    match action {
        NoRecoverableSignal | AutoBootstrapDisabled => return Ok(exit_code)
        RetryConnectLightweight if has_remote_command => {
            // native/mux/mod.rs:311-330と同じ理由・同じ挙動
            log("connection lost while running a remote command; not auto-retrying")
            return Ok(exit_code)
        }
        RetryConnectLightweight => {
            // native/mux/mod.rsのRECONNECT_BUDGET/RECONNECT_STABLE_THRESHOLDと
            // 同じ設計をそのまま流用する(新規定数を発明しない、B3)。
            match reconnect_backoff_or_give_up(&mut attempt, &mut lost_since) {
                Retry => {
                    // S2: STUN経路はサーバー側セッションスロットを消費する
                    // (AttachArbiter、--max-sessions既定16)。ここでの
                    // attempt上限(3〜5回)はwrapper.rsレベルの総試行回数の
                    // 話であり、isekai-pipe内部のBUSY_OTHER_SESSION対策
                    // (retry_while_busy_other_session、Task 2.10)とは別層
                    // (round 4のB4で訂正——wrapper.rsから
                    // retry_while_busy_other_sessionを直接呼ぶことはできない。
                    // それはisekai-pipe crateのprivate関数であり、
                    // isekai-sshは別バイナリとしてisekai-pipeをspawnするだけ)。
                    intent = build_connection_intent(resolution)?  // 新しいintent_id
                    continue
                }
                GiveUp(code) => return Ok(code)
            }
        }
        RebootstrapAndRetry => (既存の1回きりの再展開+再試行、変更なし)
    }
}
```

**バックオフ・上限（Q1で確定）**: `native::mux::run_with_reconnect`が
既に確立している設計（`RECONNECT_BUDGET`=24時間、`RECONNECT_BACKOFF`
=`initial: 500ms, max: 10s, jitter: 0.25`、`RECONNECT_STABLE_THRESHOLD`
=60秒で予算リセット）をそのまま流用する。新規に`MID_SESSION_RETRY_WINDOW`
を発明しない（B3、旧採番。round 4のB4も参照）。ただしSTUN P2P経路
固有の対策として（S2）、サーバー側`--max-sessions`（既定16）に
当たる前に軽量リトライの試行回数を3〜5回程度で頭打ちにし、以降は
自然な`Unreachable`へのクラス遷移によるエスカレーションに委ねる
——**これはwrapper.rsレベル（`isekai-ssh`プロセスが`run_ssh_once`を
再試行する回数）の上限であり、isekai-pipe crate内部の
`retry_while_busy_other_session`（180秒バックオフ、Task 2.10、
`isekai-pipe`のconnect.rsにのみ実装可能——`isekai-ssh`は別バイナリと
して`isekai-pipe`をspawnするだけで、その内部関数を直接呼べない）
とは異なる層の対策である（round 4のB4で訂正、既存のround 1〜3の
記述はこの2層を混同していた）**。

**孫プロセスの後始末（S1、round 2でR1-S5により「断定」を撤回し
実測ベースの方針へ変更）**: `run_ssh_once`の`child.wait()`は
`std::process::Command::status()`の性質としてssh(1)自身のみを待ち、
`ProxyCommand`孫プロセス（`isekai-pipe connect`）を待たない。ただし
——round 1では`wrapper.rs:769-772`（正しくは`:719-725`）のdoc
コメント「`.status()`はProxyCommand孫を含むプロセスツリー全体の
終了までブロックする」を「事実に反すると判明」と断定したが、これは
早計だった。`.status()`自体がそうしないことと、`ssh(1)`が自分の
`ProxyCommand`を終了時にkillするか（＝結果的に孫が残らないか）は
別問題であり、後者はOpenSSH側の経験的な挙動でありコードからは
決まらない。

**実装時にまず1回実測する**（ローカル`cargo`を使わないため
`prefer-gh-actions-over-local-cargo`に抵触しない）。**round 4で
手順を厳密化(S10)**: この実測は**経路によって「正解」が逆になる**
——STUN P2Pなら孫はQUICの`max_idle_timeout`(15秒)で自然に落ちる
のが正常動作だが、Relayなら`run_resume_loop`が最大10日
（`DEFAULT_RESUME_GRACE_SECS`）resumeを試み続けるため**孫が残るのが
正常動作（バグではない）**。経路を固定せず測ると、Relay経由の
セッションで「孫が残った」という観測から誤って
`ensure_process_terminated`機構を作り込み、それがRelay経路の10日
resumeそのものを破壊しかねない。したがって実測は必ず**明示的に
STUN P2P経路を使うホスト**（`#@isekai stun`設定済み、Relay
fallbackが働かない構成）に対して行い、切断から**20〜30秒後**
（QUIC idle timeoutの15秒に余裕を見た時間）に`pgrep -f
'isekai-pipe connect'`が残るかを確認する。Relay経由での孫の残存は
この対策の対象外であることを明記する。

- **残らない場合（STUN経路で）**: `ensure_process_terminated`機構・
  関連する§5のテスト・旧Open Question「孫プロセス終了策の実装方法」は
  丸ごと不要。`wrapper.rs:719-725`のdocコメントは「`.status()`自体は
  直接の子だけを待つが、`ssh(1)`が自分のProxyCommandを終了させるため
  結果として孫も残らない」と理由を正確にする訂正のみ行う。
- **残る場合（STUN経路で、かつ15秒のQUIC idle timeoutを超えても
  残る場合）**: pid単発の`kill`では不十分（孫がSTUN経路であっても
  何らかの理由で残り続ける可能性がある）。`ssh(1)`を独自の
  プロセスグループで起動し（`setsid`/`process_group(0)`）、
  グループ全体に`SIGTERM`→猶予後`SIGKILL`を送る設計にする。

**ctl-socket forwardの後始末（S3）**: `apply_ctl_socket_forward`/
`spawn_ctl_listener`（`wrapper.rs:743-751`）が反復ごとにbindする
UNIXソケット・spawnするlistenerタスクを、次の反復に入る前か
ループ終了時に明示的にteardownする。

#### 2.2.3 UX: ライブ再接続表示

`resume_loop.rs`が既に持つ「tssh風のライブ再接続表示」と一貫した
スタイルで、wrapperループ側にも同種の1行ステータス表示を実装する。
`resume_loop`側（セッション内のQUIC再接続）と`isekai-ssh`側
（プロセス全体の再起動）は文言で区別する（例:
「reconnecting (quic)...」 vs 「isekai-ssh: connection lost,
reconnecting...」）。M2指摘により、`resume_loop.rs`と同じ
`stderr.is_terminal()`ゲートを入れ、`--isekai-log-file`実行時に
ログが`\r`で汚れないようにする。

### 2.3 PR3: STUN P2Pへの真のバイトレベルresume（round 5で全面改訂）

§1.6で述べた通り、round 1レビューでコストの見積もりが訂正された
ため、ユーザー判断により今回のスコープに含める。**round 5レビュー
（opus-critic-a・opus-critic-bの独立並行再レビュー、両者が同一
blocking項目に収束）で、以下の設計のままでは「PR3を実装すると
このADRが解決しようとしている問題自体が悪化する」ことが判明し、
全面改訂した。**

#### 2.3.0 blocking: サーバー到達不能時のSTUN専用短いgive-up境界
    （round 5で「正味の退行」と誤って強調、round 5フォローアップの
    Option B検討で訂正）

**round 5で一度「PR3は正味の退行になる」と結論したが、これは
過大評価だった**——opus-critic-a・opus-critic-bの両者へ、より根本的な
修正案（下記コラム参照）を独立に評価させる過程で、両者が独立に
同じ理由でこの評価を訂正した。正しい理解は以下の通り。

`reconnect_and_resume`（STUN P2Pのresumeが使う再接続関数）は
新しいwildcardソケットを`bind`し、そこから`target.peer_addr`（＝
サーバー側の観測アドレス）へ直接dialする「bare redial」である
（`connect_stun_p2p_with_round`と違い、再STUN問い合わせも
再ホールパンチも行わない）。**これは実は正しい:** クライアント側の
IP/NATマッピングが変わる場合（Wi-Fi切替・スリープ復帰——本ADRが
対象とする最も典型的なトリガー）、OSは新しいソケットを現在の
デフォルト経路にルーティングし、そのdial自体がクライアント側NAT上に
新しいマッピングを副作用として作る。**サーバー側アドレスが安定して
いる限り（§1.5の既存の前提条件、この案件の実際のデプロイもこれに
該当する）、bare redialはこのケースを正しく処理し、本当にバイト
継続的なresumeを実現する。** PR3は当初の目的（クライアント側の
アドレス変化に対する真のresume）をbare redialのままで達成できる。

真に問題なのは別のケースである: **サーバー側が到達不能な場合**
（サーバーそのものがダウンしている、対称NAT越しでクライアント側の
パンチだけでは足りない構成等）、bare redialは単に接続タイムアウトで
失敗し続け、`UnknownSession`拒否にはならない。`run_resume_loop`が
使う`resume_with_backoff_until_deadline`は、確立済みセッションが
本当に失われたと確定する条件が「`UnknownSession`拒否がN回連続」の
1つしかなく、単純な接続タイムアウトでは早期終了しない
（`resume_grace`のデフォルト＝サーバー既定＝10日をそのままデッド
ラインとして使う）ため、**このケースに限り**「`run_resume_loop`の
中で最大10日間、成功し得ないredialをバックオフしながら繰り返す」
という無音のハングが起きる。

**なぜ「STUN専用の再接続時に再STUN問い合わせ＋再ホールパンチをする」
（root-cause修正案、後述コラム参照）では解決しないか**: サーバー側
（`isekai-pipe serve`）のホールパンチは起動時の`--punch-peer`引数に
よる**一度きりの動作**であり（「before listening」）、再接続してきた
クライアントの新しいアドレスを学習して再パンチする手段が存在しない
（`engine/mod.rs`）。クライアント側だけが再STUN・再パンチしても、
サーバー側のNATが新しいクライアントアドレスからの着信を許可する
ようにはならない——両者が同時に相手の新アドレスを知る帯域外の
シグナリング（Androidの`isekai_stun_p2p_transport.rs`が
`bootstrap_via_ssh_with_punch`でSSHブートストラップ channel 越しに
行っているもの）が無い限り、クライアント側だけの再パンチはQUIC
Initialパケット自体と区別のつかない無意味な動作になる。CLI
（`isekai-pipe`）側にはこの帯域外シグナリング経路が無く
（`stun_p2p.rs`のモジュールdocが明記する通りこの層の対象外）、
`connect_stun_p2p_with_round`が観測した`our_observed_addr`も
CLI側では実際どこにも送信されずログに出るだけで終わる
（Android側と違う点）。したがって「サーバー到達不能」ケースの
唯一実行可能な対応は、**サーバー側アドレスが変わった/古いことを
検知したら、フルブートストラップ（PR2の軽量リトライが実行する
再STUN・再パンチ込みの完全な再確立）に委ねる**ことであり、
`run_resume_loop`自身に再STUN・再パンチ機構を持たせることではない
——それは§1.5が最初から「このPRのスコープ外」と明記している
「真の再ランデブー」に踏み込まずに実現できるものではない。

**修正（Task 3.4として実装、load-bearing・severityは「退行防止」から
「ハング防止」に修正）**: STUN P2P経路には、`run_resume_loop`が
通常使うresumeデッドラインとは別に、独自の短いgive-up境界を
持たせる。具体的な形（接続試行回数の上限か、数十秒〜120秒程度の
短いタイムデッドラインか）は実装時に決めてよいが、**この境界に
達したら`run_resume_loop`自身が`Err`を返してPR2の軽量リトライへ
制御を戻す**（フルSTUN再確立をやり直させる）という効果は必須。
この修正を入れて初めて、下記2.3.4の非対称設計が前提とする「STUN
経路の`give_up`はPR2の軽量リトライにとって現実的に到達可能な
防衛線である」が成立する（10日の既定値のままではこの防衛線は
事実上到達不能で、非対称設計の前提が崩れていた）。この境界は
「サーバーが移動/到達不能になった」ケースを早期に見切って
フルブートストラップに委ねるためのものであり、`run_resume_loop`
自体に再ランデブー能力を持たせるものではない——`run_resume_loop`は
「自分が直せる範囲（クライアント側アドレス変化）」だけを担当し、
それ以外は速やかにPR2へ投げ返す、という責務分担が正しい設計である。

> **検討して却下した案（root-cause rework, "Option B"）**: `run_resume_loop`
> のredial自体をSTUN専用にし、再接続のたびに再STUN問い合わせ＋
> 再ホールパンチをしてから`resume_on_connection`（RESUMEハンドシェイク
> 自体は`resume.rs:768`で接続確立方法から既に疎結合になっている）を
> 呼ぶ、という設計を検討した。opus-critic-a・opus-critic-bへ独立に
> 評価させたところ、両者が独立に同じ結論——**この案は到達可能性を
> 一切改善せず、むしろ退行させる**——に達した。理由は上述の通り
> サーバー側の一度きりのホールパンチにある。加えて実務上のコストも
> 判明した: 再試行のたびに`PUNCH_PROBE_COUNT`(5)×`PUNCH_PROBE_INTERVAL`
> (150ms)＝750msの固定遅延と公開STUNサーバーへの追加RTTが乗る
> （バックオフで数時間〜数日粘る接続に対し、無関係な第三者
> インフラへの負荷を増やし続けることにもなる）、`RelayTarget`が
> `run_resume_loop`内の6箇所（`reestablish_control_stream`・
> `WarmStandby::new_bound_to_interface`・`spawn_reconnect_signal`・
> `promote_warm_standby_once`等）に配線されているため「redialだけを
> 差し替える」ような綺麗な抽象化にならない。そして何より——
> **この変更が実際に到達可能性を改善することを示す差分テストが
> 原理的に書けない**（サーバー側が起動時に一度しかパンチしない以上、
> 「再パンチ有り/無しで結果が変わる」シナリオが存在しない）。
> テストで検証できない変更は効果が無い変更である、という判断で
> 却下した。

#### §1.5補足（round 5フォローアップで追加）: CLIにはAndroidにある
    帯域外シグナリング経路が無い

上記の議論を明文化するため、§1.5に以下を補足する: Androidの
`isekai_stun_p2p_transport.rs`は`bootstrap_via_ssh_with_punch`で
自分の観測アドレスをSSHブートストラップchannel越しにサーバーへ
伝え、サーバー側がそのアドレスへパンチし返す、という帯域外
シグナリングを行っている。**CLI（`isekai-pipe`）にはこの経路が
無い**——`connect_stun_p2p_with_round`が観測する`our_observed_addr`は
テストのアサーション以外どこにも渡されずログに出力されるだけである。
したがって、CLI側でクライアントだけが再STUN・再ホールパンチしても、
サーバー側の一度きりのパンチが更新されない限りQUIC Initialパケット
自体と区別のつかない無意味な動作になる。resumeが機能するかどうかを
決めるのは「サーバー側アドレスが安定しているか」（§1.5の既存の前提
条件）であり、クライアント側の再パンチの有無ではない。

#### 2.3.1 Task 3.1: `stun_p2p.rs`の戻り値を拡張する

`connect_stun_p2p_with_round`（`isekai-transport/src/stun_p2p.rs:289-293`
付近）が`connect_and_handshake`の戻り値4つのうち`conn`・`proof`・
`effective_resume_grace_secs`を破棄している（`_conn`/`_proof`/
`_effective_resume_grace_secs`）。**主眼はここ**——`resume_grace`の
ハードコード`0`自体は「resume無効」ではなく`0`＝「サーバー既定を
要求する」という意味（`ResumableRelaySession::effective_resume_grace_secs`
のdoc参照）であり、この置き換えは`#@isekai resume-grace`を尊重する
ための付随的改善に過ぎない。

`run_resume_loop`が要求する`ResumableRelaySession`（`connection`・
`data_stream`・`control_stream`・`session_id`・
`effective_resume_grace_secs`・`network_rebinder`）を満たすため、
以下をすべて呼び出し元へ返すよう`StunP2pConnection`を拡張する:
- `conn`（`open_control_stream(&conn, &proof)`のために必要）
- `proof`
- `effective_resume_grace_secs`
- `endpoint.rebinder()`（**`endpoint`がスコープを抜ける前に確保する**
  ——`connect_via_relay_resumable`の既存パターンと同じ順序制約）

`connect_stun_p2p`/`connect_stun_p2p_with_fallback`/
`StunP2pConnection`という公開APIのシグネチャ変更になるため、
Androidの`isekai-terminal-core`側への影響を確認する——ただし
Android側は既に独立した`connect_stun_p2p_on_socket`
（`stun_p2p.rs:135`付近）という別関数を使っており、その関数は自前で
`connect_and_handshake`を呼んでいるため直接の影響は無い（`round 5`で
再検証済み）。**`StunP2pConnection`は`pub`なので、変更前にワーク
スペース全体で他に利用者がいないかgrepで確認すること**。

#### 2.3.2 Task 3.2 / 3.2b: `run_resume_loop`への移行（2経路とも）

- **Task 3.2**: `run_stun_p2p_with_fallback`（`resume_loop.rs:307`
  付近）を、確立後に`RelayTarget`を合成し`run_resume_loop`へ渡す
  よう変更する。`RelayTarget`に`Default`実装は無いため`..`構文は
  使えず、5フィールドすべてを明示する:
  - `helper_addr: target.peer_addr`
  - `server_name`
  - `cert_sha256_hex`
  - `session_secret`
  - `local_bind_port_range`: **供給元が無い**——`StunP2pTarget`に
    このフィールドは無く、値は`ConnectionIntent::local_bind_port_range`
    にしかないが、`run_stun_p2p_with_fallback`/単一candidate経路の
    どちらもこのintentを受け取っていない。デフォルトで`None`にする
    （ユーザーの`#@isekai local-bind-port-range`設定がSTUN経路のみ
    サイレントに無視される、というfirewall越しの可視な挙動差になる）か、
    シグネチャを拡張してintentを貫通させるかを実装時に明示的に決定
    する（既定は前者でよいが、コードコメントで意図的な決定として
    残すこと）。
  - `run_resume_loop`へ渡す`factory`は**`system_quic_factory()`**
    （`connect_stun_p2p_with_fallback`が既に使っているものと同一）を
    使う——`relay_endpoint_factory(RelayTransportKind::Qmux)`
    （TCPトランスポート）を誤って流用しないこと。
  - `experimental_network_rebind`は**`false`**、`tethering_interface`
    は**`None`**を明示的に渡す——`AnyMuxRebinder::rebind`・
    `WarmStandby::new_bound_to_interface`はどちらも「別の
    ソケット/物理インターフェースへ切り替える」機構であり、relay
    （安定した公開アドレス）では無害だが、STUN P2Pのホールパンチ済み
    NATマッピングは特定のソケットに紐づいているため、再パンチなしに
    切り替えると2.3.0と同じ理由で到達不能になる。同じコミットで
    `ConnectLaunch::tethering_interface`の「STUN P2Pには効果が
    ない」という現行docコメントも訂正する（Task 3.2実装後は
    誤りになるため）。
- **Task 3.2b（新設、旧設計では欠落していた）**: `connect.rs`の
  `CandidateRoute::StunP2p`アーム（単一candidate経路）も同様に
  `run_resume_loop`へ移行する。Task 3.2のみ実装すると「フォール
  バック経路はresumeするが単一candidate経路はしない」という分裂
  状態になる。この経路のエラーは`recover_via_cross_family_fallback`
  にも渡るため、下記2.3.3のマーカー配置後もそのbail-out条件が
  正しく機能することを確認する。

#### 2.3.3 Task 3.3: マーカー付与は呼び出し元での`.map_err`のみでよい

**受け入れ条件（round 2でM-1によりOpen Questionから格上げ、round 4の
B3で決定を反転、round 5で実装方法を訂正）**: Task 3.2/3.2bにより、
Task 2.1が付与していたSTUN P2Pの`relay_stdio`直接呼び出し2箇所は
消滅する。round 2案「relayと同じくgive_upにマーカーを付けない」の
ままでは、PR3後にマーカーの付与箇所がゼロになり
`ConnectOutcomeClass::MidSessionDisconnect`・
`RetryConnectLightweight`・PR2のUnixループ全体が誰にも到達されない
状態になる（opus-task-reviewが発見、B3）。

**決定（round 4のまま変更なし）**: STUN経路は**relayとは意図的に
非対称に**し、STUN P2P用の`run_resume_loop`の`give_up`には
**マーカーを付ける**。relay経路のgive_upには引き続き付けない
（S1(旧)の理由は変わらず有効）。

**実装方法（round 5で訂正）**: `give_up`自体は`resume_with_backoff_until_deadline`
内部の2箇所（relay/STUN共有）から呼ばれる関数であり、STUN専用の
`give_up`は存在しない——ルート判別フラグを`run_resume_loop`から
`resume_with_backoff_until_deadline`まで貫通させる案は過剰な変更
である。実際には**`run_resume_loop`の呼び出し元（Task 3.2/3.2bの
STUN側2箇所）で`.await`の結果を`.map_err(|e| e.context(MidSessionDisconnectSignal))`
するだけでよい**——`run_resume_loop`の`Err`は内部が1個の`?`と1個の
`Ok(())`のみであるため常に「resumeを諦めた」ことを意味し、
`anyhow::Error::downcast_ref`は`.context(...)`を何重に重ねても
辿れる（既存の`StaleTrustSignal`と同じ「source-attach, downcast at
top」パターン）ため、relay側の呼び出し元やさらに外側の
`.context("isekai-pipe connect: ... failed")`ラップに一切影響
されない。`recover_via_cross_family_fallback`のbail-out判定
（同じ裸の`downcast_ref`を使用）もこの配置のまま正しく機能する。

- **検証**: STUN経路でresumeを使い切った（2.3.0の短いgive-up境界に
  到達した）場合に`MidSessionDisconnect`として分類され、PR2の
  軽量リトライループが発火することを確認する統合テスト。relay側の
  give-upが誤ってこのマーカーを継承しないことを確認するテストも
  追加する。

この非対称の根拠は§1.5で述べた通りSTUNのresume上限がrelayより
本質的に低い（サーバー側アドレスが安定していないとそもそも
効かない）ことにあり、STUNのresume使い切りは「redeployすべき」
よりも「もう一度確立し直せば直るかもしれない」に近いケースが多い。
これによりPR2のUnix軽量リトライ機構はPR3後もSTUN経路の最終防衛線
として生き続ける——2.3.0の短いgive-up境界を入れることで、この
防衛線は「発火頻度が下がるだけ」ではなく実際に到達可能であり続ける
（10日の既定値のままでは事実上到達不能で無意味化していた）。

#### 2.3.4 Task 3.4: STUN専用give-up境界（2.3.0参照、本タスクとして実装）

2.3.0で述べた短いgive-up境界を実際に実装するタスク。境界に達した
際、`--isekai-log-file`使用時にハングのように見えないよう、
「scrollbackが失われ新しいセッションを確立する」旨を一度だけログ
出力する。

#### 2.3.5 Task 3.5（新設）: `relay_stdio`の削除

Task 3.2/3.2b完了後、`relay_stdio`（`resume_loop.rs`）の呼び出し元は
ゼロになる（`resume_loop.rs`・`connect.rs`の元の2箇所）。放置すると
`dead_code`警告で`rust-core-test-linux`が失敗する。同じコミットで:
- `relay_stdio`関数自体を削除する。
- `MidSessionDisconnectSignal`のdocコメント（現在「`relay_stdio`の
  リモートストリームI/O 2箇所のみ」とスコープを説明している）を、
  新しい付与箇所（2.3.3のTask 3.2/3.2b呼び出し元）を指すよう書き換える。
- `isekai-pipe-core/src/outcome.rs`内の`relay_stdio`に言及する説明文・
  テストフィクスチャ文字列（`"relay_stdio: writing to remote stream
  failed"`相当）を更新する。

#### 2.3.6 スコープ外（変更なし）

§1.5の前提条件（サーバー側アドレスが安定していること）を超える
真の再ランデブー（クライアント・サーバー双方のアドレスが同時に
変わるケース）は、引き続きこのPRのスコープ外とする——
`isekai_transport::stun_p2p`のモジュールdocの
"resume support lands in S-4a onward" が指す本当に難しい部分は
ここに残る。STUN P2Pが（`connect_stun_p2p_with_round`が既に
`BindSpec::any_ipv4()`を使っているため）IPv4限定である点も
PR3による新規の後退ではない。

### 2.4 3 PR共通: Unix/Windows単一プロセスfallbackの実装配置

`decide_connect_failure_recovery`・バックオフ定数・ループの骨格は
`isekai-ssh`クレート内のプラットフォーム非依存モジュールに置き、
Unix版（`wrapper.rs::run_ssh_with_connect_failure_recovery`）と
Windows単一プロセスfallback版（`native/connect.rs`の対応する関数、
`ConnectRecoveryOps`トレイト経由でテスト用モックと実装を差し替え
可能な既存設計を踏襲）の両方から呼ぶ。Windowsのmux経路（既定）は
§2.2.1の通り新ループを持たず、既存`run_with_reconnect`をそのまま
使う——ループを共有するのはUnixとWindowsの「単一プロセスfallback」
経路の間だけである。

---

## 3. 検討した代替案

### 3.1 STUN P2Pにフルバイトレベルresumeを実装する（round 1: 採用、PR3）

round 0では「Relay向けのresumeプロトコル一式をP2P間で再ランデブー
込みで再現する規模」と見積もり却下していたが、round 1レビュー(B7)で
`crate::resume::reconnect_and_resume`の既存流用（Androidが既に
使っている）により狭いコストで実現できると判明したため、§2.3(PR3)
として採用した。却下ではなく採用に転じた稀な例として記録しておく
——見積もりの誤りは、STUN P2Pの実装（`stun_p2p.rs`）ではなく
Androidのtransport層（`isekai-terminal-core`側）を読んでいなかった
ことに起因する。

### 3.2 `ProxyCommand`自体を永続プロキシ化し、`ssh(1)`からは1本のTCPに見せかける

`ssh(1)`に「切断が起きたことを気付かせない」ため、`isekai-pipe
connect`を`ssh`のライフタイムより長く生存する常駐プロセス
（例えば`isekai-pipe agent`のようなデーモン）にし、`ProxyCommand`は
そのデーモンへの単なるUNIXドメインソケット接続にする案。
デーモン側でQUIC再接続を隠蔽できれば、`ssh(1)`自身は何も知らずに
済み、真の意味で"never dies"になる。**却下理由**: (1)
既存のセッション/プロファイル/ランタイムディレクトリのモデルを
プロセスモデルごと作り直す必要があり、本ADRが対処したい
「即死」バグの修正としては過大な変更、(2) デーモン常駐は
Windows側の`child_stdio.rs`が明示的に避けている設計
（"the native path just runs the same binary as a child process
instead of leaving that job to a real `ssh(1)`" — 単純さを優先する
既存方針）と衝突する、(3) それでも`ssh(1)`自身のバイトストリームは
QUIC再接続の間止まる（デーモンが内部でRESUMEするまでバッファ
するだけ）ので、結局PR3が扱う「サーバー側アドレスが安定していない
場合の再ランデブー」という核心課題は解決しない。将来この課題に
着手する際の選択肢の1つとして記録だけしておく。

### 3.3 `ssh(1)`の`ServerAliveInterval`/`ServerAliveCountMax`だけで解決する

却下理由: 今回の根本原因は`ssh(1)`自身のTCPレベルの生存確認では
なく、その手前にいる`ProxyCommand`子プロセス（`isekai-pipe
connect`）がQUICの`max_idle_timeout`で先に死んで`ssh(1)`にEOFを
返してしまうことにある。`ssh(1)`側の設定をいくら調整しても、
`ProxyCommand`が先に落ちる限り無関係。

---

## 4. Consequences

### 4.1 良くなる点（round 2でM-2により文言を訂正）

- Unix・Windows両方で、**STUN P2P経路とWindows mux経路**における
  セッション確立後のネットワーク瞬断が「即死」ではなく「自動
  再接続」になる。（round 0/1は「STUN P2P・Relay双方」としていたが
  誤り——Relay経由は元々`run_resume_loop`が最大10日resumeを
  試みており、そもそも即死していなかった。本ADRが実際に改善する
  のはSTUN P2PとWindows mux経路である。）
- 既存の`RebootstrapAndRetry`（古いデプロイの再展開）はフォール
  バックとして温存され、退行しない。
- `outcome.rs`の実態と乖離していたドキュメントコメント、および
  `wrapper.rs:719-725`の孫プロセスに関するdocコメントが是正される。

### 4.2 リスク・要検討事項（round 1で全面更新）

- **サーバー側セッションスロットの枯渇・他タブへの巻き添え立ち退き**
  （round 0は「fencing slotの永久リーク」と表現していたが誤り——
  `release_slot_for`は正しく呼ばれておりEpic N-5で構造的にリーク
  は解消済み。round 1のS2で訂正）: STUN P2P経路は
  `retry_while_busy_other_session`の対象外であり、`--max-sessions`
  （既定16）到達時に他タブ/他ホストのparkedセッションが立ち退かされ
  うる。§2.2.2の対策（試行回数を3〜5回に制限、STUN connectも
  `retry_while_busy_other_session`で包む）で軽減する。
- **`isekai-pipe connect`孫プロセスの孤児化**（S1、round 1で新規発見、
  round 3(R2-M2)で条件付きに変更）: 実装時に実測し、孫が実際に
  残ると判明した場合のみ§2.2.2の対策（次の試行前に前回の孫プロセスを
  確実に終了）を実装する。
- **`ConnectOutcome`ファイル書き込みとssh終了のレース**（B6、
  round 1で新規発見）: PR1で修正済みの前提とする。
- **`should_bootstrap`のセマンティクス変更**: `--isekai-no-bootstrap`
  が「一切の自動リトライ禁止」ではなく「再展開のみ禁止」に意味が
  狭まる。round 1レビューで両opusが「既存の契約と整合しており新
  フラグは不要」と判定（Q2）。
- **`MidSessionDisconnect`のシグナルがネットワーク以外の原因
  （例: リモートプロセスのクラッシュ、`isekai-pipe serve`自体の
  panic）でも発生する**: 再接続を試みること自体は無害（再接続しても
  すぐ同じ理由で失敗し、24時間予算の上限に達したら諦める）が、
  メッセージは「connection lost」程度の中立的な表現に留める。
- **PR3実装後もSTUN P2Pにはscrollback/未確認バイトの継続性の限界が
  残る**（Q5、両opus指摘）: クライアント・サーバー双方のアドレスが
  同時に変わる構成（対称NAT越しの相互punchが要る場合）では、PR3の
  resumeも効かない。この限界は`ISEKAI_PIPE_DESIGN.md`のEpic R要約に
  明記する（§6）。

---

## 5. テスト戦略

`.claude/rules/prefer-gh-actions-over-local-cargo.md`によりローカル
`cargo test`は実行できない前提で設計する。

- `decide_connect_failure_recovery`: 既存の3ケースのテストに加え、
  `MidSessionDisconnect` × `should_bootstrap`の各組み合わせを
  純粋関数の単体テストとして追加（ネットワーク不要、既存パターン
  踏襲）。
- `write_connect_outcome_for_wrapper`相当の分類ロジック: pump段階の
  `Err`に`MidSessionDisconnectSignal`が正しく載ることを、実ネット
  ワークなしのユニットテストで検証（`relay_stdio`にモックの
  `AnyByteStream`を渡し、読み取りエラーを注入する形——
  `isekai-transport`の`faulty_udp_socket.rs`的なフォールト注入
  パターンを踏襲できるか確認）。
- リトライループ自体: `ConnectRecoveryOps`トレイト
  （`native/connect.rs`に既存）と同種の抽象を介して、実`ssh(1)`/
  実ネットワークなしにモックで「1回目は`MidSessionDisconnect`で
  失敗、2回目は成功」というシナリオをテストする。
- E2Eレベル（`isekai-ssh/tests/*_e2e.rs`の実sshdハーネス）: round 0の
  「クライアント側`isekai-pipe connect`を`SIGKILL`する」案は、M1
  指摘の通り`ConnectOutcome`が一切書かれないケース（`PanicOutcomeGuard`
  はunwindのみカバーしシグナルはカバーしない）を検証してしまうため
  不採用。代わりに、フォールト注入可能なソケットファクトリで
  ストリーム読み取りエラーを注入するか、**サーバー側**
  （`isekai-pipe serve`）を落とす／該当セッションを強制切断する
  ことで、クライアント側プロセスは正常に`Err`を返す経路を通した
  「mid-session切断」を模擬する。`isekai-ssh`が自動的に新しい対話
  セッションを再確立することを確認する（`isekai-ssh-e2e-test-self-containment-convention`
  のメモリ通り、この種のヘルパーは当該テストファイル内で自己完結
  させる）。
- 孫プロセスの後始末（S1、round 3で条件付きに変更）: 実装時の実測で
  孫プロセスが実際に残ると判明した場合のみ、リトライ2回目の試行が
  1回目の`isekai-pipe connect`プロセスを確実に終了させてから新
  セッションをダイヤルしていることを、プロセス監視付きのテスト
  （またはモック）で検証する。残らないと判明した場合はこのテスト
  自体が不要になる。
- 非idempotentコマンドのガード（B5）: `isekai-ssh host -- cmd`形式の
  invocationでmid-session切断が起きた場合、自動リトライされない
  ことを検証する（`native/mux/mod.rs`の既存`has_remote_command`
  ガードのテストと同型）。

---

## 6. Rollout

- **3本のPRを順に**（PR1→PR2→PR3、各PR依存順）マージする。各PRは
  独立に`.claude/rules/main-branch-protection.md`の required 5本
  （`android-unit-test`は本変更では無関係だが形式上走る、
  `rust-core-test-linux`・`android-uniffi-drift`（no-op）・
  `lockfile-drift`・`room-migration`（no-op））が緑になることを
  マージ条件とする。
- CI: `rust-core-test-linux`（required check）で新規ユニット
  テストが走る。E2Eテストは required 5本には含まれない重量級
  テストなので、既存の`isekai-ssh/tests/*_e2e.rs`群と同じ
  非required扱いで追加する。
- UniFFI再生成: 不要（本ADR冒頭に明記の通り、対象crateは
  Android/iOS非依存）。
- ドキュメント更新: `ISEKAI_PIPE_DESIGN.md`に「Epic R:
  セッション確立後の切断からの自動リカバリ」として本ADRの要約を
  PR3完了後にまとめて追記する。§4.2で述べた「PR3後もSTUN P2Pには
  scrollback継続性の限界が残る」ことを明記する。

---

## 7. Open Questions

round 1〜2のレビューで、round 0の6項目・round 1で新規発見された
指摘の大半は解決済み（詳細:
[`ADR_MIDSESSION_DISCONNECT_RECOVERY_REVIEW_ROUND1.md`]、および
本ドキュメント各所に反映済み）。round 1で残っていたOpen Question 2
（Windows mux修正の判定方法）はround 2でopus-critic-a自身の具体案
（`exit_code: Option<u8>`を判別子に使う、§2.2.1）を採用して解決した。
round 3レビューで検証してほしい残課題:

1. **§2.2.2のS1対応**（孫プロセスの後始末）は、round 2で「実装時に
   まず実測してから機構の要否を決める」という方針に変更した
   （§2.2.2参照）。この「実測してから決める」という進め方自体に
   異論がないか。
2. **PR3のスコープ境界**: §2.3の3で「対称NAT越しの相互punchが
   要る構成はPR3の対象外」としたが、この構成をどう検知して
   ユーザーに伝えるか（サイレントに「再接続してもscrollbackが
   失われる」で済ませるか、明示的なログを出すか）。
3. 他に見落としている失敗モード・設計の抜けはあるか（特に
   round 2で新規に追加した§2.2.1のレース対策・§2.2.1のWindows mux
   試行回数上限の適用方法・§2.1で narrowed したPR1スコープに、
   さらなる見落としがないか）。
