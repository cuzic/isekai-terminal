# 設計案: mux ownerにおける「輸送層の生死」の扱いをゼロベースで再設計する

- **位置づけ**: `docs/adr/0010-isekai-ssh-reconnect-latency.md` §3.1((d)/(b′)/
  `HANG_PRESUMED_AFTER`/`established_clients`)の**代替設計案**。
  コーディネーターが本ADRへ統合する前提の素案。コードは一切変更していない。
- **作成日**: 2026-09-15
- **入力**: 現行ADR全文、`docs/adr/reviews/0010-isekai-ssh-reconnect-latency-review-v1.md`(Round 7)、
  `.claude/rules/always-connects.md`、`.claude/rules/rust-ssot.md`、
  および `rust-core/isekai-ssh/src/native/` / `rust-core/isekai-pipe/src/` /
  `russh-0.48.2` の実コード

---

## 0. 要旨

現行ADR §3.1が7ラウンドかけて積み上げた会計機構(ロック取得デッドライン、
`HANG_PRESUMED_AFTER`、`established_clients`/`pending_clients`、
`newest_pending_start`、RAIIガード、述語化された`wait_for_idle_exit`)は、
**3つの独立した施策に置き換えられ、いずれも不要になる**と結論する。

| 施策 | 内容 | 新しい情報を要するか |
|---|---|---|
| **A** | `SharedHandle`の共有粒度を`Mutex`→`RwLock`に直し、`streamlocal_forward`だけを排他にする | **不要**(russh 0.48.2のシグネチャだけで導出できる) |
| **B** | `isekai-pipe connect`が自分の輸送層状態を`ConnectOutcome`と同じ機構で公表する | 本タスクの仮説そのもの |
| **C** | `relay_client`のハンドシェイク入口に**入場ゲート**を1つ置き、holderの死を`handle_died`のポーリングではなく子プロセスのexitに委ねる | Bに依存 |

**最も重要な発見は施策Aで、これは仮説(B)とは独立に成立する**: 現行コードで
2番目のクライアントが30秒待たされる原因(現行ADR §3.1の(d)が救おうとしている
もの)は、russhの制約ではなく**このリポジトリ側が`client::Handle`を
`Mutex`で包んでいることだけ**に由来する。`channel_open_session`は
`&self`しか要求しない(§1.2)。したがって(d)のロック取得デッドラインは
**そもそも守るべきロックが存在しないので不要**である。

施策Bは仮説通り成立するが、**push(イベント通知)ではなくpull(必要な瞬間に
読む)で足りる**。これが本案の中核的な単純化で、名前付きパイプも追加の
env varも追加のwatchdogタスクも不要になる(§3)。

---

## 1. ゼロベースで読み直して新たに判明した事実

現行ADRとRound 7レビューは、いずれも以下の5点に触れていない。うちF1・F2は
**現行ADRの前提そのものを覆す**。

### 1.1 F1(Blocking): `handle_died`は同じMutexで飢餓する——現行の300秒検知も実は働かない

`handle_died`(`owner.rs:249-256`)は

```rust
if handle.lock().await.is_closed() { return; }
```

と書かれている。一方、ハングする`channel_open_session`は
`owner.rs:366`の`let guard = handle.lock().await;`配下で実行される
(`owner.rs:361-383`)。つまり**クライアントが`channel_open_session`で
詰まっている間、`handle_died`は`is_closed()`を一度も評価できない**
(同じ`tokio::sync::Mutex`の取得待ちで止まる)。

現行ADR §3.1末尾は「(b′)を採らない場合、本当に死んだ場合の検知は
`handle_died`の`is_closed()`ポーリング(実効300秒)に委ねられたままになる」と
書いているが、これは**楽観的すぎる**: ハングしている限り`handle_died`は
300秒後にも1000秒後にも発火しない。`owner.rs:845`の`relay_loop`内の
`is_closed()`チェック(確立済みクライアントが使う高速化パス)も同じMutexを
取るため、**確立済みクライアントのセッション終了処理までハング中の
クライアントに巻き込まれる**。

現行ADR §3.1(R5-2の「ゾンビ化」の記述)はこの巻き添えの範囲を過小評価して
いる。実際には「新規タブが使えなくなる」だけでなく、「このholderは
`IDLE_GRACE`でも`handle_died`でも`shutdown`でも二度と終了しない」
——`kill`されるか、OSが`isekai-pipe connect`子プロセスごと片付けるまで残る。

### 1.2 F2(Blocking): `channel_open_session`は`&self`しか要求しない——Mutexは過剰

russh 0.48.2の実シグネチャ:

| メソッド | シグネチャ | 出典 |
|---|---|---|
| `is_closed` | `fn is_closed(&self) -> bool` | `client/mod.rs:236` |
| `channel_open_session` | `async fn channel_open_session(&self)` | `client/mod.rs:468` |
| `streamlocal_forward` | `async fn streamlocal_forward(&mut self, ..)` | `client/mod.rs:610` |
| `cancel_streamlocal_forward` | `&mut self` | 同上近傍 |

`russh_stream_session::open_channel`(`russh-stream-session/src/lib.rs:632-635`)
も`ctl_forward::open_login_shell`(`ctl_forward.rs:101-103`)も、引数は
`handle: &client::Handle<H>`——**共有参照**である。`owner.rs:366`が
`Mutex`の排他ロックを取っているのは、`SharedHandle`(`connect.rs:95`)が
`Arc<tokio::sync::Mutex<NativeHandle>>`だからというだけで、
**russhがそれを要求しているからではない**。

この結果、現行ADRのR5-1(「productionの`channel_open_session`呼び出しは
どちらも`handle.lock()`ガード配下にあるので、長時間pendingは高々1件」)は
**設計上の必然ではなく、直すべき欠陥だった**。(b′)の単一スロット簡約
(Round 6)も、それを「最も新しいpending」に定義し直す議論(Round 7 R7-4)も、
この欠陥を所与としたうえでの議論であり、欠陥を直せば議論ごと消える。

### 1.3 F3: ownerと`isekai-pipe connect`子プロセスは**同一プロセス対**である

`isekai-ssh`のholderプロセスは、

1. `mux/mod.rs:788-797`で`OwnerHook`を作り、
2. `connect::run_prepared` → `connect_attempt`(`connect.rs:638-684`)で
   `spawn_isekai_pipe_connect`(`connect.rs:657`)により子を起こし、
3. `run_authenticated_session`の`connect.rs:859-862`でそのフックを発火させて
   `owner::serve_clients`を起動する。

つまり**子を起こしたプロセスと、ownerを走らせるプロセスは同一**で、しかも
子の`ConnectionIntent`(`intent.intent_id`)と`runtime_dir`は
`connect_attempt`のスコープにそのまま存在する。「親子プロセス間の状態共有」を
新規に設計する必要は薄く、**既にある結線に1本足すだけ**で済む(§3)。

### 1.4 F4: 子プロセスへの「ファイルパスによる指示」は既に3本通っている

`spawn_isekai_pipe_connect`(`child_stdio.rs:62-112`)は既に

- `ISEKAI_INTENT_ID`(`:74`)
- `ISEKAI_PIPE_RUNTIME_DIR`(`:75`)
- `ISEKAI_PIPE_LOG_FILE`(`:76-80`)

を子へ渡している。そして子側は`isekai-pipe/src/connect.rs:568-592`
(`write_connect_outcome_for_wrapper`)で、`ISEKAI_INTENT_ID`と
`default_runtime_dir()`から**親が読む場所を自力で導出して
`ConnectOutcome`を書いている**。

**したがって輸送層状態の公表に必要な新しいenv varは1つもない**——子は
既に「自分のintent_id」と「runtime_dir」を知っており、親も同じ2つを知って
いる。この対称性が、§3で「ファイル方式」を推す最大の根拠である。

### 1.5 F5: クライアントが待ちを諦めた後の後始末は、既に正しく書かれている

`HELLO_ACK_TIMEOUT`(30秒、`client.rs:66`)でクライアントが去った場合、
ownerの`relay_client`は`channel_open_session`を待ち続けるが、
**リンク回復後にオープンが成功すると`owner.rs:418`の`HelloAck`書き込みが
失敗し、`owner.rs:425`の`channel.close()`が実行される**。

つまり「クライアントが先に諦めた」ケースでは、Round 4が問題にした
**回収不能チャネルのリークは発生しない**。リークが起きるのは
「ownerが自分から`channel_open_session`のfutureをdropしたとき」だけ
であり、本案はそれを一度も行わない(§4.3)。

---

## 2. 施策A: `SharedHandle`の共有粒度を直す(新情報ゼロで効く)

### 2.1 変更内容

```
connect.rs:95
-  pub(crate) type SharedHandle = Arc<tokio::sync::Mutex<NativeHandle>>;
+  pub(crate) type SharedHandle = Arc<tokio::sync::RwLock<NativeHandle>>;
```

呼び出し側の対応(全5箇所、grepで網羅済み):

| 箇所 | 現在 | 変更後 | 理由 |
|---|---|---|---|
| `owner.rs:251`(`handle_died`) | `lock()` | `read()` | `is_closed(&self)` |
| `owner.rs:366`(チャネルオープン) | `lock()` | `read()` | `channel_open_session(&self)` |
| `owner.rs:845`(`relay_loop`の死亡確認) | `lock()` | `read()` | 同上 |
| `ctl_forward.rs:72`(`streamlocal_forward`) | `lock()` | `write()` | `&mut self` |
| `ctl_forward.rs:87`(`cancel_streamlocal_forward`) | `lock()` | `write()` | `&mut self` |
| `connect.rs:899`(非mux経路のオープン) | `lock()` | `read()` | `channel_open_session(&self)` |

### 2.2 これだけで消える問題

- **F1の飢餓が消える**: `handle_died`は`read()`なので、他のクライアントが
  `channel_open_session`で何時間詰まっていても1秒周期で`is_closed()`を
  評価し続けられる。`relay_loop`の確立済みクライアントも巻き添えにならない。
- **実インシデントの「2回目の30秒待ち」が消える**: 08:45:16のクライアントが
  `channel_open_session`で詰まっていても、08:46:01のクライアントは
  `read()`を即座に取れるので、自分の`channel_open_session`に進める
  (そこでどうなるかは施策Cが決める)。**現行ADRの(d)(ロック取得デッドライン)は
  この時点で存在理由を失う。**
- **`HANDSHAKE_DEADLINE`という定数が不要になる**——したがって
  「§3.3のログで正常系の実測値を得てから決める」という、現行ADR §3.5が
  実装順序を縛っていた依存関係も1つ消える。

### 2.3 施策Aだけでは足りない点(正直に書く)

`tokio::sync::RwLock`は**公平(FIFO)**で、待機中のwriterが後続のreaderを
ブロックする。したがって`#@isekai ctl-socket`が有効な場合、
`ctl_forward::request`(`ctl_forward.rs:65-81`、write lock)が沈黙した
リンクでハングすると、後続クライアントの`read()`も結局待たされる。

対策は現行ADR §3.1の「具体的な変更点1・2」をそのまま採用してよい:

- `ctl_forward::request`の`streamlocal_forward`に短い予算(数秒)を付け、
  超過したら`routes.unregister` + best-effortの
  `cancel_streamlocal_forward`(独立予算)を通して**`None`へ縮退**する
  (`ctl_forward.rs:76-80`の既存の失敗経路と同じ着地)。
- `ctl_forward::cancel`の`cancel_streamlocal_forward`にも独立の短い予算を付ける。

`streamlocal_forward`は**回収可能**(`cancel_streamlocal_forward`が存在する)
なので、現行ADRの危険度分類でもタイムアウト可の側であり、この2点は論点が
決着済みである。**`channel_open_session`にタイムアウトを付けない**という
結論も維持する(F5・§4.3)。

---

## 3. 施策B: 輸送層状態の公表(伝達手段の選定)

### 3.1 何を公表するか(SSOTはどこか)

`isekai-pipe connect`の`run_resume_loop`(`resume_loop.rs:1251-1469`)は、
輸送層の状態遷移を**すでに正確な単一地点で**持っている:

| イベント | 実コード上の地点 |
|---|---|
| 切断を検知した瞬間 | `resume_loop.rs:1429` `let disconnected_at = *disconnected_since.get_or_insert_with(Instant::now);` |
| resume試行中 | 同`:1436` `print_reconnect_status(..)` / `:1452` `resume_with_backoff_until_deadline` |
| 再接続成功 | 同`:1465-1467` `data_stream = new_stream; disconnected_since = None;` |
| 諦めた(resume window満了) | 同`:1461`の`?`でErr伝播 → `connect.rs:528` `write_connect_outcome_for_wrapper` → プロセス終了 |

現状、この情報の**唯一の出口は人間向けstderr文字列**
(`format_reconnect_status`、`resume_loop.rs:709-719`)であり、
holder経由では`holder.rs:117`の`Stdio::null()`により消える。
ownerはこれを一切知らず、`owner.rs:366`以降のSSH操作の挙動から
間接的に推測している——`rust-ssot.md`が禁じている
「下流でstateを覗き見する」構図そのものである。

**公表すべき最小の状態は2つだけ**:

```
Up
Down { since: <UNIX epoch ms>, resume_deadline: <UNIX epoch ms> }
```

`GaveUp`は**要らない**。諦めた瞬間に子プロセスは終了し、
`ChildStdio`(`child_stdio.rs:161-179`)がEOFになり、russhのセッションタスクが
畳まれて`is_closed()`が`true`になるので、施策A後の`handle_died`が
1秒以内に発火する。**「終了」という事象はプロセスのexitで既に完全に
表現されている**ので、ファイルに書く必要がない(書くと二重表現になり、
どちらを信じるかという新しい問題を作る)。

### 3.2 伝達手段の比較

| 案 | 実現方法 | 評価 |
|---|---|---|
| **(1) 追加の名前付きパイプ** | ownerが`local-ipc-mux`でチャネルを立て、名前をenv varで子へ渡し、子がJSONフレームをpushする | **不採用**。`local-ipc-mux`はWindows専用(`local-ipc-mux/src/lib.rs:12-19`が明記)で、Linux CIでは子プロセス側の実物を動かせない(`InMemoryChannel`は同一プロセス用のテストダブル)。owner側に受信タスクと再接続処理が要り、「伝播経路自体が壊れたらどうするか」という新しい失敗系統が増える。pushが要る用途(§4.5)が現時点で存在しない |
| **(2) 追加の継承ハンドル** | Win32 `STARTUPINFOEX` + `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`で4本目のハンドルを継承させる | **不採用**。このリポジトリに前例が無く、Windows専用で、`holder.rs:65-98`の`NonInheritableStdHandlesGuard`が示す通りこのコードベースはハンドル継承で既に一度実害を踏んでいる。得られるものに対してリスクが大きすぎる |
| **(3) 子のstderrをholder時だけpipeして解析** | `child_stdio.rs:90`の`Stdio::inherit()`をholder時だけ`piped()`にし、行を解析 | **不採用**。人間向け表示(`format_reconnect_status`が`\r`+ANSIを出す)と機械可読プロトコルを同じストリームに混ぜることになり、`ISEKAI_PIPE_LOG_FILE`の有無で内容が変わる既存分岐(`resume_loop.rs:699-703`)とも噛み合わない。ログ解析はSSOTの反対概念 |
| **(4) 既存のctl-socket機構への相乗り** | `isekai-pipe ctl`/`ISEKAI_CTL_SOCK`経由 | **不採用**。あれは**リモート側**の制御プレーン(Epic M)であり、SSHチャネルの上に載っている。SSHが使えない状況を伝えるのに、SSHの上のチャネルを使うことになり循環する |
| **(5) `runtime_dir`配下の状態ファイル** | `ConnectOutcome`と同じ機構(`isekai-pipe-core`)で、子が書き、親が必要な瞬間に読む | **採用**。理由を以下に述べる |

### 3.3 採用案(5)の具体形

**新しいenv varもIPCも不要**(F4)。`isekai-pipe-core`に以下を足すだけ:

```rust
// isekai-pipe-core/src/transport_status.rs (新規、~60行)
pub enum TransportStatus { Up, Down { since_unix_ms: u64, resume_deadline_unix_ms: u64 } }

/// `<runtime_dir>/transport-status/<intent_id>.jsonl` へ1行 append する。
/// 失敗は無視してよい(読み手は「不明」として今日と同じ挙動に落ちる)。
pub fn append_transport_status(runtime_dir: &Path, intent_id: &str, s: &TransportStatus);

/// 最後の完全な1行を読む。ファイル無し/壊れた行/I/Oエラーはすべて `None`。
pub fn read_transport_status(runtime_dir: &Path, intent_id: &str) -> Option<TransportStatus>;
```

- **書き手(子)**: `resume_loop.rs:1429`直後に`Down`、`:1465`直後に`Up`。
  呼び出しは2箇所だけ。`intent_id`/`runtime_dir`の解決は
  `isekai-pipe/src/connect.rs:568-570`が`ConnectOutcome`向けに
  やっているのと全く同じコードで足りる。
- **読み手(親)**: `connect_attempt`(`connect.rs:657`)が
  `(runtime_dir, intent.intent_id)`から状態パスを作り、
  `run_authenticated_session`経由で`OwnerHook`へ渡す
  (`connect.rs:101`の`OwnerHook`型に引数を1つ足す)。
  **`intent_id`を再計算せず、子を起こしたのと同じスコープから
  引き回すこと(MUST)**——`drive_connect_recovery`は再bootstrap時に
  **新しいintentを作る**(`connect.rs:619-627`)ため、
  古い`intent_id`を掴んだままだと「死んだ子の最後の`Down`」を
  永久に読み続ける(=全クライアントを永久に拒否する)という、
  本案で唯一の致命的な誤用になりうる。

**なぜrename方式(`ConnectOutcome`と同じ、`isekai-pipe-core/src/lib.rs:376-397`)
ではなくappend方式か**:

1. **Windowsのrename競合を避けられる**。`std::fs::rename`はWindowsでは
   `MOVEFILE_REPLACE_EXISTING`だが、読み手が`FILE_SHARE_DELETE`なしで
   その瞬間ファイルを開いていると失敗しうる。`ConnectOutcome`は
   「一度だけ書いて一度だけ読む」ので踏まないが、状態ファイルは
   フラッピングするリンクで何度も書き換わるため踏みうる。
2. **切断/再接続の履歴がそのまま診断ログになる**。現行ADR §3.3が
   §1.3の72秒の原因究明のために欲しがっている観測性の一部が、
   別実装を足さずに手に入る。
3. 1回の書き込みが100バイト程度・イベントは切断/復帰の**エッジでのみ**
   発生するので、成長は実用上問題にならない。念のため
   `isekai-pipe-core/src/rotating_log.rs`の既存機構で上限を掛けてもよい。

### 3.4 なぜpushではなくpullで足りるのか

ownerがこの状態を必要とする瞬間は**1つしかない**: 新しいクライアントの
ハンドシェイクを始めるかどうかを決めるとき(§4)。それ以外の用途は:

- 「輸送層が諦めた」→ 子のexitで表現済み(§3.1)。
- 「輸送層が復帰した」→ ownerは何もする必要がない
  (詰まっていた`channel_open_session`が自然に完了する)。
- 「確立済みクライアントへの通知」→ 不要(むしろ通知して何かすると
  `always-connects.md`違反になる、§5.2)。

したがって**ファイルを必要な瞬間に1回読むだけ**で足り、監視タスクも
`Notify`も周期チェックブランチも増えない。Round 7が要求した
「起床源の再規定」(R7-5)も、そもそも起床すべき時間イベントが無いので
発生しない。

---

## 4. 施策C: 入場ゲートと、holder死のイベント化

### 4.1 入場ゲート(1箇所、述語なし、閾値なし)

`relay_client`(`owner.rs:303-450`)の、`Hello`検証が終わった直後・
`ctl_forward::request`(`:356-359`)より**前**に1つ置く:

```
relay_client(...)
  Hello読み取り(HELLO_READ_TIMEOUTで既に有界、:325-329)
  バージョン/トークン検証(:331-345)
+ // 入場ゲート
+ if let Some(TransportStatus::Down { since, resume_deadline }) = read_transport_status(..) {
+     write_frame(&mut writer, &Frame::Rejected {
+         reason: format!("the shared connection's transport is reconnecting \
+                          ({}s elapsed, resume window ends in {}s)", .., ..),
+     }).await;
+     return Ok(());      // ← Err ではない。異常ではなく既知の縮退
+ }
  ctl_forward::request(...)      (:356-359)
  handle.read() → channel_open_session (:361-383)
```

ポイント:

- **`shutdown.notify_waiters()`を呼ばない**。これは「真のチャネルオープン
  失敗」(`owner.rs:406-413`)専用のままにする。輸送層のresumeは異常ではない。
- **holderを畳まない**。確立済みクライアントのリレーは走り続ける
  ——これが`always-connects.md`とresume window(既定10日)を守る核心。
- クライアント側は`Frame::Rejected`を受けて`ClientOutcome::Rejected`
  (`client.rs:68-84`)→ `run_as_client_over`の`Rejected`アーム
  (`mux/mod.rs:860-863`)→ **直接接続へフォールバック**。
  この経路は自前の`isekai-pipe connect`を新たに起こすので、
  `drive_connect_recovery`のサイレント再bootstrap(実インシデントで
  08:46:47に実際に発火したもの)にちゃんと乗る。
- **ユーザーに見える文言が改善する**: 現在の
  「the mux holder rejected this connection (no response from the owner
  within 30s)」が、「輸送層が43秒前から再接続中(resume windowはあとN秒)」に
  変わる。原因が初めて画面に出る。

### 4.2 holderの死を`is_closed()`のポーリングからイベントへ

施策Aで`handle_died`の飢餓が消えるので、以下が成立する:

1. 輸送層がresume windowを使い切って諦める → 子が
   `write_connect_outcome_for_wrapper`(`isekai-pipe/src/connect.rs:528`)を
   書いて終了 →
2. `ChildStdio`(親の`connect.rs:658`が保持)がEOF → russhのセッションタスクが
   終了 → `Handle::is_closed()`が`true` →
3. `handle_died`(`owner.rs:249-256`、`HANDLE_HEALTH_POLL_INTERVAL`=1秒)が
   発火 → `owner.rs:216-222`でholderが即座に終了 → 次のタブが
   `ConnectError::NotFound`を見て新しいholderを起こす。

現行ADRが「ゾンビ状態」と呼んでいたもの(=holderが二度と終了しない)は、
**F1(飢餓)が原因であってカウンタの定義が原因ではなかった**。
施策Aでそれが消える以上、`established_clients`も`pending_clients`も
`newest_pending_start`も`HANG_PRESUMED_AFTER`も述語化された
`wait_for_idle_exit`も**すべて不要**である。
`active_clients`/`wait_for_idle_exit`(`owner.rs:146,266-277`)は
**現状のまま一切変更しない**——したがってRound 7のR7-2(acceptが
idleタイマーをリセットしなくなる)・R7-3(二重減算)・R7-6(既存ユニット
テスト3本の書き直し)も**発生しない**。

### 4.3 `channel_open_session`を一度もキャンセルしない

Round 4が特定したrusshの構造的制約(`client/mod.rs:468-479`で
`channel_ref`を先にセッションタスクへ移譲するため、確認応答待ちを
打ち切ると回収手段が無くなる)は、本案でも**そのまま残る既知の制約**である。
本案はこれに触れない:

- ゲートに引っかかったクライアントは、**そもそも
  `channel_open_session`を呼ばない**ので状態を作らない。
- ゲートを通ってから(=輸送層がUpだった瞬間の直後に)リンクが落ちて
  詰まったクライアントは、**待ち続ける**。ownerは何もしない。
  - 輸送層がresumeすれば、オープンは自然に完了する(正常系)。
  - クライアント自身が`HELLO_ACK_TIMEOUT`(30秒)で先に諦めた場合は、
    F5の通り`owner.rs:418`のHelloAck書き込み失敗 → `owner.rs:425`の
    `channel.close()`で**既存コードが正しく後始末する**。
  - 輸送層が諦めた場合は、§4.2で接続ごと畳まれる(SSH接続ごと畳むのは
    安全、という現行ADRの整理がそのまま当てはまる)。

つまり**「回収不能なリモートチャネル」を作る経路が1本も存在しない**。
現行ADRの`(c)`(russh側の修正/fork)は、依然として正しい将来課題だが、
**もはやクリティカルパス上にない**。

---

## 5. 批判的検討: これは本当に根本解決か、新しい層に同じ問題を持ち込むだけか

### 5.1 「情報が古い」問題は両方向とも安全側に倒れる

ゲートは`Down`という**レベル**を読む。読んだ瞬間に古くなりうるので、
2方向の誤りを個別に評価する:

| 誤り | 実際の帰結 | 評価 |
|---|---|---|
| ファイルは`Down`だが実際はもう`Up` | そのクライアントだけ多重化を失い、直接接続で繋がる | **許容**。`always-connects.md`が禁じるのは「手動操作なしに復旧しない接続失敗」であって、多重化の機会損失ではない |
| ファイルは`Up`だが実際は`Down` | 今日と全く同じ(詰まって、クライアントが30秒で諦める) | **退行しない**。本案は既存挙動の狭義の改善であり、新しい失敗を作らない |
| 書き込みが失敗し続ける(ディスク満杯等) | 読み手は常に`None`=不明 → 今日と同じ挙動 | **退行しない** |
| 誤った`intent_id`を掴む | **全クライアントを永久拒否**(多重化が恒久的に失われる。接続自体は直接接続で成立する) | **唯一の要注意点**。§3.3のMUST(子を起こしたスコープから引き回す)で構造的に防ぐ。加えて§7の検証項目(v)で直接テストする |

「時間の長さから生死を推測する」ことが原理的に不可能だ、というRound 1以来の
結論は本案でも維持される——**本案はどこにも時間閾値を持ち込まない**。
持ち込まれているのは「輸送層自身が自分の状態遷移を宣言する」という
事実の転送だけであり、`HANG_PRESUMED_AFTER`のような
「沈黙の長さから死を推定する定数」は1つも増えない。

### 5.2 `always-connects.md`との整合

同ルールが守る資産は「輸送層の再接続猶予(既定10日)の間、確立済みの
シェルが失われないこと」である。本案の不変条件:

> **ゲートは「まだ`HelloAck`を書いていないクライアント」にしか作用しない。
> 確立済みクライアントのリレーループには一切触れない。**

これはコード上も自明に保たれる——ゲートは`relay_client`の
`owner.rs:356`より前にしか存在せず、`relay_loop`(`owner.rs:498-`)は
無改変だからである。現行ADRの(b′)が抱えていた
「`established_clients`が0かどうかで畳んでよいか判定する」という
**間違えると確立済みのタブを殺しうる**分岐は、本案には存在しない。

さらに、同ルールが記録する「サーバー側の状態リークはクライアントの
再試行では回復できない」という教訓については、§4.3の通り**リークを
作る経路自体が無い**。

### 5.3 「新しい層に同種の問題を持ち込まないか」への直接の回答

持ち込みうるものを3つ挙げ、それぞれ評価する:

1. **伝播経路自体の故障**: ファイルI/Oの失敗は「不明」に落ち、
   今日の挙動に縮退する(§5.1)。push方式(名前付きパイプ)を採ると
   「接続が切れた」「再接続する」という**新しい状態機械が増える**——
   これがpushを採らない最大の理由である。
2. **二重の真実源**: ownerは今後`is_closed()`(russhのセッションタスクの
   生死)と状態ファイル(リモートリンクの疎通)の2つを見ることになる。
   これはミラー状態ではない——**答える問いが違う**
   (前者は「このプロセス内のSSHセッションが畳まれたか」、後者は
   「今バイトが流れる状態か」)。ただし`rust-ssot.md`の精神に沿い、
   **両者を組み合わせた判断は1つの関数に閉じる**こと(MUST):
   ゲートの述語を`owner.rs`内の単一関数(引数で両方を受け取る純粋関数)
   として切り出し、`wait_for_idle_exit`が独立関数にされているのと同じ
   理由でユニットテスト可能にする。
3. **状態の粒度不足**: `Up`/`Down`の2値では、「STUN候補を試している
   最中」「relayへフォールバック中」等を区別できない。今回は区別する
   必要が無い(ゲートの判断は2値で足りる)が、将来
   `docs/adr/0006-stun-reestablish-continuity.md`の実装で粒度が欲しくなったとき、
   JSONLの1行に**フィールドを足すだけ**で拡張できる形にしておく
   (読み手は未知フィールドを無視する`serde`の既定挙動でよい)。

### 5.4 本案が解決**しない**もの(明示)

- **§1.3の認証フェーズ72秒の無音**: 本案の対象外。現行ADR §3.2の結論
  (タイムアウトを実装せず、フェーズ境界ログのみ)をそのまま維持する。
- **リンクは生きているがsshdが応答しないケース**: 状態ファイルは`Up`の
  ままで、ownerは今日と同じく詰まる。これはSSHレベルの問題であり、
  輸送層の状態伝播では原理的に解決できない(区別する手段は
  `keepalive`しかなく、§1.4がそれを却下した理由は変わらない)。
- **russhの回収不能チャネル問題そのもの**: 残る(§4.3)。ただし
  踏まない設計になる。

---

## 6. 代案

### 6.1 代案α: push(名前付きパイプ)で状態を流す

§3.2の(1)。**将来pushが本当に必要になる条件**を明記しておく:
「ゲートを通った後に切断されたクライアントの30秒待ちを、1秒に縮めたい」
という要求が実測で裏付けられたとき。その場合でも、
`channel_open_session`のfutureをキャンセルしてはならない(§4.3)ので、
実装は「別タスクが`writer`経由で`Frame::Rejected`を先に書き、
`relay_client`はオープンを待ち続けて完了後に`close()`する」という形に
なり、`writer`の所有権を分割する追加の複雑さが要る。
**費用対効果が合うのは、実測で「ゲート通過後の切断」が頻発すると
分かってからでよい。**

### 6.2 代案β: Windowsでもmuxをやめる

`ControlPersist`相当を捨て、タブごとに独立接続する。本ADRが扱う問題は
全部消えるが、認証/TOFU/QUICハンドシェイクをタブごとに払うことになり、
再接続レイテンシは**むしろ悪化する**(これを避けるためにmuxを入れた)。
**却下。**

### 6.3 代案γ: SSHセッション自体を`isekai-pipe`側へ移す

プロセス境界をまたぐから状態が見えないのであって、そもそも
`isekai-pipe connect`の中でrusshを走らせれば境界が消える。
**これが本質的には最も正しい形**だが、`isekai-ssh`/`isekai-pipe`の
責務分割(`ISEKAI_PIPE_DESIGN.md`)を根本から引き直す規模で、
Unix経路(`ssh(1)` + ProxyCommand)との対称性も失う。
**記録に留め、今回は不採用。**

### 6.4 代案δ: ownerのリトライ戦略を変える

「`channel_open_session`が失敗したら別のハンドルで再試行する」——
holderは共有ハンドルを1本しか持たないので成立しない。**却下。**

### 6.5 現行ADRの代案B′(keepalive `interval`短縮 + `max`拡大)

現行ADR §4は「holder自身の気づきを速くする将来の代案候補」として
記録している。**本案の施策A+§4.2により、その目的(死の検知)は
1秒以内のイベント駆動で達成される**ため、B′は不要になる。
記録から「将来の候補」として外してよい。

---

## 7. 現行ADR §3.1との対応表(何を捨て、何を残すか)

| 現行ADRの要素 | 本案での扱い |
|---|---|
| (d) `handle.lock()`のデッドライン有界化 | **不要**(F2: 排他ロック自体が不要) |
| `HANDSHAKE_DEADLINE`定数 | **不要** |
| (b′) `established_clients`ベースの計上修正 | **不要**(F1の飢餓が原因だった) |
| `HANG_PRESUMED_AFTER`定数と不変条件 | **不要** |
| `pending_clients`/`newest_pending_start`/述語化(Round 7 R7-1〜R7-5) | **不要**(`wait_for_idle_exit`は無改変) |
| RAIIガードによる増減の対 | **不要** |
| `ever_served_a_client`の扱い | **無改変**(論点自体が消える) |
| `serve_clients`の周期チェックブランチ | **不要** |
| `ctl_forward::request`/`cancel`への独立予算 | **採用**(§2.3。RwLockの公平性ゆえ依然必要) |
| 「タイムアウトと真のエラーを分岐し、`shutdown.notify_waiters()`を呼ばない」 | **採用**(§4.1) |
| 後始末は常にデッドラインの管轄外(独立予算) | **採用**(`Frame::Rejected`書き込みを含む) |
| (c) russh側の本質的修正 | **将来課題として維持**。ただしクリティカルパスから外れる |
| §3.2(認証フェーズ、ログのみ) | **無改変で維持** |
| §3.3(フェーズ境界ログ) | **維持**。ただし「§3.1の値決めのために先行実装が必須」という依存は消える(決めるべき定数が`ctl_forward`の予算だけになるため)。診断価値のために引き続き入れる |
| §3.4(Win32進捗表示) | **維持**。加えて§4.1の`Frame::Rejected`の理由文字列で、進捗表示が無くても原因が読めるようになる |
| §4「代案B(輸送層状態のholderへの伝播)は本ADRのスコープ外の大きな設計」 | **訂正**。本案の実装規模は施策A〜Cの合計で~200行程度と見積もられ、現行§3.1の会計機構より**小さい** |

---

## 8. 実装順序と検証計画

### 8.1 順序

1. **施策A(RwLock化 + `ctl_forward`の予算)**。輸送層の情報を一切
   必要とせず、単独で実インシデントの「2回目の30秒待ち」とF1の飢餓を
   解消する。ここだけで一度実機(clipwire)に出して効果を測る価値がある。
2. **§3.3のフェーズ境界ログ**(現行ADR通り。施策Bの実装前に入れておくと、
   状態ファイルの内容と突き合わせられる)。
3. **施策B(`isekai-pipe-core`の状態公表 + 子側2箇所)**。
   この時点ではownerは読まない——**まず観測だけする**。
   実インシデント相当の状況で`Down`が本当に書かれるかを実測で確認する。
4. **施策C(入場ゲート)**。3の実測で「ownerがハングしている区間と
   `Down`区間が実際に重なっている」ことを確認してから入れる。
5. §3.4(進捗表示)。

この順序なら、各段階が独立に価値を持ち、かつ**仮説(輸送層のDownと
ownerのハングが対応している)を、ゲートを入れる前に実測で反証できる**。
現行ADR §3.5の「§3.3の実測でタイムアウト値を決める」という依存より
健全な形になる。

### 8.2 検証(反証テスト)

現行ADR §5の(a)(b)(e)(f)(h)は**そのまま維持**する。(c)(g)(b1)(b2)は
対象機構が消えるので差し替える:

- **(i, MUST)** 沈黙するモックsshdに対し、クライアントAを
  `channel_open_session`で詰まらせた状態でクライアントBを接続し、
  **Bが`handle.read()`を即座に取得して自分のオープンに進める**ことを
  確認する(施策Aの直接検証。現行ADRの(h)の置き換え)。
- **(ii, MUST)** 同じ状況で、**`handle_died`が1秒周期で評価され続ける**
  ことを確認する(F1のリグレッション防止。Aを将来`Mutex`に戻す
  リファクタが起きたら落ちるテスト)。
- **(iii, MUST)** 状態ファイルが`Down`のとき、新規クライアントが
  **`channel_open_session`を一度も呼ばずに**`Frame::Rejected`を受け、
  かつ**確立済みクライアントのリレーが継続する**ことを確認する
  (§5.2の不変条件の直接検証。現行ADRの(b2)と同じ性質を、
  より単純な形で押さえる)。
- **(iv, MUST)** 状態ファイルが無い/壊れている/読めない場合に、
  ownerが今日と同じ挙動(ゲートを素通し)になることを確認する。
- **(v, MUST)** 再bootstrap(`connect.rs:619-627`)で`intent_id`が
  変わった後、ownerが**新しい**intentの状態ファイルを読むことを確認する
  (§5.1の唯一の致命的誤用のリグレッション防止)。
- **(vi)** 輸送層が諦めて子が終了した場合、holderが1秒程度で
  `owner.rs:216-222`経由で終了することを確認する(§4.2)。

いずれもin-processのモックsshd(`owner.rs:1012,1024`の既存テストと
同じ枠組み)+ `tempfile`の状態ファイルで書け、実際のWindows名前付き
パイプも実ネットワークも要らない——**`prefer-gh-actions-over-local-cargo`
の制約下でLinux CIで全部回る**。これは§3.2で名前付きパイプ方式を
却下した理由と表裏一体である。

---

## 9. 残る限界(正直な記載)

1. **ゲート通過直後に切断されたクライアントは、依然30秒待つ**
   (`HELLO_ACK_TIMEOUT`)。実インシデントの2回のうち少なくとも1回は
   これに該当しうる。実測で頻度を見てから代案α(§6.1)を検討する。
2. **リンクは生きているがsshdが無応答**のケースは改善しない(§5.4)。
3. **russhの回収不能チャネル**という構造的制約は残る。踏まない設計に
   したが、将来誰かが「`channel_open_session`にタイムアウトを付ければ
   もっと速いのでは」と考えたときに再導入されうる——
   §4.3の理由をコード上のコメントとして`relay_client`に残すこと(MUST)。
4. **状態ファイルは`isekai-pipe connect`が生きている間しか更新されない**。
   子が`SIGKILL`相当で即死した場合、最後の値が残る。読み手側は
   「`Down`なら拒否」という安全側の解釈しかしないので実害は無いが、
   `Up`のまま残った場合は今日と同じ挙動になる(=改善しないだけ)。
