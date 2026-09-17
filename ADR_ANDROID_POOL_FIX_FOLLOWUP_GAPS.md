# ADR: pool.rs修正(issue #120/PR #119)実装後にコードレビューで見つかった2つの残存ギャップ

- **Status**: Draft(2026-09-17起草、rev2。`ADR_ANDROID_POOL_STALE_HANDLE.md`
  (rev4、収束済み)の実装(PR #119、コミット4793ebcd・c4eb37ac)がmainへ
  マージ前の`/code-review`で発見された指摘のうち、同ADRの設計範囲に
  直接関わる2件を切り出して検討する。opus-adversarial-consult round 1で
  rev1のGap Bの設計(`AttachOrigin`)が**issue #120そのものを再発させる
  regression**であること、Gap Aの調査漏れ(3箇所と書いていたが実際は
  7箇所)が判明し、rev2で両方を書き直した)
- **対象**: `rust-core/src/transport/ssh_handler.rs`・
  `rust-core/src/transport/forward.rs`・
  `rust-core/src/transport/file_preview_exec.rs`
  (`pooled.handle`のMutexをawait中握り続ける全箇所)、
  `rust-core/src/pool.rs`(`mark_dead_if_same`)、
  `rust-core/src/isekai_pipe_quic_transport.rs`・`rust-core/src/lib.rs`
  (`FirstChannelOpen`分岐・`mark_dead_if_same`呼び出し箇所)
- **入力**: PR #119の`/code-review`(2026-09-17実施、8観点並列+4バッチ検証、
  10件中2件が本ADRの対象)。`ADR_ANDROID_POOL_STALE_HANDLE.md`(収束済み、
  §3.2「限界」・§3.4項目2)。opus-adversarial-consult round 1
  (`/tmp/claude-1001/-home-cuzic-isekai-terminal/2285f8d6-e7bb-4a20-83c5-de317b68d9c7/scratchpad/opus-review-pool-followup-gaps.md`)
- **拘束される既存ルール**: `.claude/rules/always-connects.md`

---

## 1. 背景

`ADR_ANDROID_POOL_STALE_HANDLE.md`(以下「元ADR」)は、SSH接続プールが
死んだハンドルを再利用し続けるバグ(issue #120)を、(a)`try_attach`への
生存確認+tombstone-in-place(I1〜I4)、(b)`run_ssh_channel_loop`の
最初の`channel_open_session()`だけを`RUN_EXEC_TIMEOUT`でタイムアウトさせ
`FirstChannelOpen::{Succeeded,Failed,TimedOut}`を返す、という2本柱で修正した
(PR #119、mainへのマージ前)。マージ前の`/code-review`が、元ADRの設計
範囲に関連する2つの未検討ギャップを見つけた。

rev1(このADRの初版)は両方のギャップに対応方針を提案したが、
opus-adversarial-consult round 1が、Gap Bの提案(`AttachOrigin`による
Establisher/Shared区別)が**issue #120そのものを再発させる regression**
であることを具体的なタイムライン付きで指摘した。加えて、Gap Aの
調査自体が`ssh_handler.rs`しか見ておらず、実際には`transport/forward.rs`・
`transport/file_preview_exec.rs`にも同種の未保護箇所が存在し、合計7箇所
(rev1が把握していた3箇所ではない)であることも判明した。rev2はこれらを
全面的に書き直したものである。

## 2. Gap A: `channel_open_session`以外にもタイムアウト保護の無い
   ロック保持箇所が(3箇所ではなく)**7箇所**ある

### 2.1 現状の全数調査(rev1からの訂正)

`pooled.handle`(型`Arc<tokio::sync::Mutex<client::Handle<RusshEventHandler>>>`、
`ssh_handler.rs:558`)は`run_ssh_channel_loop`内で
`session = pooled.handle.clone()`(`ssh_handler.rs:966`)として
`transport/forward.rs`・`transport/file_preview_exec.rs`にも渡される。
rev1は`ssh_handler.rs`しか調査しておらず、以下の7箇所全てが
「await中も同じMutexを握り続け、タイムアウト保護が無い」状態にある:

| # | 箇所 | 操作 | 頻度・実行コンテキスト |
|---|---|---|---|
| A1 | `ssh_handler.rs:867` | `streamlocal_forward` | タブごとに1回、opt-in(`ctl_streamlocal.rs:41`の`ctl_socket_forward_enabled()`が既定OFF) |
| A2 | `ssh_handler.rs:1043` | `tcpip_forward` | ユーザーが`-R`相当のフォワードを追加するたび |
| A3 | `ssh_handler.rs:1112` | `cancel_streamlocal_forward` | タブ終了時に1回 |
| **A4** | **`forward.rs:74`** | **`cancel_tcpip_forward`** | **`teardown_forward`(`ssh_handler.rs:1057,1070,1107`から呼ばれる)内の、`tokio::spawn`で起動され誰にも`join`されない detached task の中** |
| **A5** | **`forward.rs:124`** | **`channel_open_direct_tcpip`** | **`-L`相当のフォワードでTCP接続を受け付けるたびにspawnされるタスクの中、上限無し** |
| **A6** | **`forward.rs:199`** | **`channel_open_direct_tcpip`** | **`-D`(SOCKS)相当のフォワードで接続を受け付けるたびにspawnされるタスクの中、同上** |
| **A7** | **`file_preview_exec.rs:26`** | **`channel_open_session`** | **ファイルプレビュー機能を使うたびに1回** |

(`run_exec_on_handle`、`ssh_handler.rs:682-689`は既に
`tokio::time::timeout(RUN_EXEC_TIMEOUT, run_exec_on_handle_inner(..))`で
保護されている——元ADRが修正した`:802`以外で唯一保護済みの箇所)

### 2.2 A4(detached task)が最も深刻——タイムアウトを足す場所そのものが無い

```rust
// rust-core/src/transport/forward.rs:62-79(要旨)
pub(super) fn teardown_forward(forward: ActiveForward, session: Arc<Mutex<Handle>>, ...) {
    match forward {
        ActiveForward::Remote { bind_addr, bound_port } => {
            tokio::spawn(async move {                              // ← join されない
                if let Err(e) = session.lock().await                // ← forward.rs:74
                    .cancel_tcpip_forward(bind_addr.clone(), bound_port as u32).await
                { warn!(...); }
            });
        }
        ...
    }
}
```

`run_ssh_channel_loop`が返り(`pool::release`が呼ばれてrefcountが0になり
削除タイマーがarmされた)後も、この`tokio::spawn`されたタスクは
`session.lock().await`で止まったままでありうる。したがって:

1. 元ADR §3.4項目2が前提としていた「`try_attach_with`の`is_alive`述語は
   `try_lock()`が失敗する状況を想定内としてフェイルオープンする
   (`.unwrap_or(true)`)」という設計は、**そもそもタイムアウトを
   仕掛けられる「所有者」が存在する**ことを暗黙に前提にしていたが、
   A4にはその前提が成り立たない——`tokio::time::timeout`で包む対象の
   futureに、包む側のスコープが無い(detachedなので)。
2. A1〜A3・A7に単純に`tokio::time::timeout`を足すだけでは、この
   A4を直せない。別の手当て(detachされたタスク自身の中で
   タイムアウトを掛ける、またはそもそもdetachせず束縛して寿命管理する)
   が要る。

A5・A6も同様に「接続を受けるたびに際限なくspawnされるタスクが同じ
Mutexを握る」という点で深刻——1回のハングした`channel_open_direct_tcpip`
が、プール全体の`try_lock`ベースの生存確認を無力化する。

### 2.3 推奨する対処の形: 個別の`tokio::time::timeout`の羅列ではなく、
   単一のアクセサに集約する

7箇所(将来さらに増えうる)へ`tokio::time::timeout(RUN_EXEC_TIMEOUT, ...)`を
個別にコピー&ペーストしていく方式は、元ADR §3.4項目1が
「`try_attach`のデフォルト付きラッパーを`try_attach_with`と併存させない
理由」として述べた「次に新しいプールが追加されたときに同じバグを
静かに引き継ぐ」のと同じ構造の問題を生む。`PooledSshHandle`に
「タイムアウト付きでロックして操作する」単一のアクセサ
(例: `PooledSshHandle::with_handle_timeout(|h| async { ... })`)を
用意し、7箇所全てをそちらへ寄せることを推奨する。A4のようなdetached
task内の呼び出しも、このアクセサを経由すれば同じ保護を受けられる
(taskの中身自体は依然detachedのままだが、無限にハングすることは
無くなる)。

### 2.4 (rev1で誤っていた論点、rev2で削除)「A1・A2のタイムアウトを
   tombstoneに繋ぐべきか」という非対称性の議論は、そもそも成立しない

rev1の§2.3・§2.5項目2は、「`streamlocal_forward`/`tcpip_forward`の
タイムアウトを`mark_dead_if_same`に接続すべきかどうか」という論点を
立てていたが、round 1レビューの指摘により**この配線は現状存在せず、
rev1もそれを新設する提案をしていなかった**ことが判明した——
`mark_dead_if_same`が呼ばれるのは`run_ssh_channel_loop`の戻り値
`FirstChannelOpen`経由のみ(`isekai_pipe_quic_transport.rs:139-141,175-177`・
`lib.rs:1710-1712`)であり、A1・A2はこの戻り値と無関係な
`run_ssh_channel_loop_after_first_open`内部の処理である。つまり
rev1のこの議論は「何も無いところに架空の配線を仮定して、それを
繋ぐべきでないと結論する」という空虚な議論だった。この論点は
rev2では削除する。

代わりに立てるべき論点は「タイムアウト自体を追加すべきか」だけであり、
§2.3で述べた通り、7箇所全てにタイムアウトを追加すべきである
(A3・A4はプロトコル上安全、§2.5参照)。

### 2.5 タイムアウトが生む副作用: サーバー側に取り残されるリソース

**2.5.1 プロトコルのdesyncは起きない(round 1レビューで確認)**

`tcpip_forward`/`streamlocal_forward`/両方の`cancel_*`は
`oneshot::channel()`の応答を待つ設計で、russh内部
(`russh-0.48.2/src/client/session.rs:310,338,365,391`)はこの
`oneshot::Sender`をFIFOキュー(`open_global_requests`)で管理し、
`REQUEST_SUCCESS`/`REQUEST_FAILURE`受信時にFIFO順で1個ずつpopして
応答する(`client/encrypted.rs:812,848`)。我々がタイムアウトで
`oneshot::Receiver`側をdropしても、`Sender`側は依然キューに残ったまま
FIFO順を維持するため、`let _ = return_channel.send(result)`が単に
無視されるだけで、**後続の他のリクエストへの応答がズレる心配は無い**。
これは読者が真っ先に心配するであろう点であり、明記しておく価値がある。

**2.5.2 `channel_open_session`(元ADRで既に保護済み)自身が抱える
チャネルリーク——rev1から引き続き、ただし影響範囲は当初より狭い**

`channel_open_session`(`ssh_handler.rs:802-803`)を
`tokio::time::timeout`でキャンセルした場合、russh内部
(`russh-0.48.2/src/client/mod.rs:468-478`)は`unbounded_channel()`の
受信側を介してサーバーからの`CHANNEL_OPEN_CONFIRMATION`を待つ設計に
なっている。タイムアウトで我々の future がdropされると、この受信側も
dropされる。その後(タイムアウト後)にサーバーから確認応答が届くと、
`client_read_authenticated`(`russh-0.48.2/src/client/encrypted.rs:402-443`)
は`channel.send(ChannelMsg::Open{...}).unwrap_or(());`
(`encrypted.rs:424-430`、`.unwrap_or(())`自体は430行目)という形で
送信結果を握りつぶす——受信側が無くなっていてもエラーは無視され、
`self.channels`(`local_id`をキーとする内部マップ、`CHANNEL_CLOSE`
[`encrypted.rs:453`]・`CHANNEL_OPEN_FAILURE`[`encrypted.rs:475`]でしか
削除されない)・`enc.channels`(プロトコルパラメータ側、
`encrypted.rs:392`)の両方に、二度と使われることのないエントリが
残り続ける。サーバー側もこのチャネルを正常に開かれたものとして扱い
続けるため、クライアント側が二度と`CHANNEL_CLOSE`しない結果、sshdの
`MaxSessions`のようなper-connectionチャネル数上限を静かに1つずつ
消費する。russhの公開APIには、確認を受け取れなかった`local_id`を
明示的に閉じる手段が無い(`Handle`の公開メソッドを確認済み)。

ただし、**タイムアウトが`lock().await`自体の完了を待っている間に
発火した場合は、`Msg::ChannelOpenSession`自体がまだ送信されておらず
何もリークしない**(`ssh_handler.rs:802-803`はロック取得と
`channel_open_session()`呼び出しの両方を同じ`timeout`で包んでいるため)。
リークが起きるのは「ロックは取れたが、サーバーからの確認応答を
待っている間にタイムアウトした」場合に限られ、これはrev1が想定して
いたよりも狭い窓である。

**2.5.3 `tcpip_forward`のタイムアウトは、サーバー側に取り残された
フォワード登録という、より重大な問題を生みうる(round 1レビューで
新規発見、rev1は未検討)**

`tcpip_forward`(A2)がタイムアウトした場合、
`remote_forwards.lock().insert(...)`(`ssh_handler.rs:1046`)・
`active_forwards.insert(...)`(`:1047-1051`)のどちらも実行されない。
しかし、我々の`Msg::TcpIpForward`送信自体は(ロック取得後)既に
サーバーへ届いている可能性があり、サーバー側が実際にポートを
バインドしてから応答が我々のタイムアウトに間に合わなかった、という
ケースがありうる。この場合:

- サーバーが実際にリスンを開始しているにもかかわらず、こちら側は
  `active_forwards`にエントリを持たないため、`teardown_forward`
  (`ssh_handler.rs:1105-1108`、`active_forwards.drain()`を走査)は
  この存在を認識できず、**`cancel_tcpip_forward`が一生呼ばれない**。
- リスナーは、このプールされたハンドルが生きている限り(=タブより
  長生きしうる)残り続ける。
- ユーザーが同じ`-R`を再度追加しようとすると、サーバー側は
  「ポートは既にバインド済み」として拒否し続け、**接続自体は
  健全であるにもかかわらず、この機能だけが永久に使えなくなる**
  (このプールされたハンドル自体が破棄されるまで回復しない)。

これは`channel_open_session`のチャネルリーク(§2.5.2、sshdの
セッション数上限を静かに消費するだけ)よりも実害が大きい
——ユーザーから見て明確に「機能が使えなくなった」状態になる。
最低限の緩和策として、A2のタイムアウト経路でも
`cancel_tcpip_forward`をbest-effortで発火させることを推奨する
(A3・A4と同じ「失敗してもログに残すだけ」の扱いでよい)。

`streamlocal_forward`(A1)は影響がより軽微——`pooled.ctl_forwards`への
登録(`ssh_handler.rs:866`)は呼び出し**前**に行われ、`Err`時は
`:945`で除去されるため、タイムアウト時も同様に除去すればよい。
残るリスクはリモートソケットファイルの残留のみで、パス自体が
128bitランダム(`ctl_streamlocal.rs:45-47`)なので衝突の心配は無く、
`isekai_pipe_core::sweep_stale_sockets`がプレフィックスベースで
掃除する。過剰な対応は不要。

A3・A4(両方の`cancel_*`)自体をタイムアウトさせることは、
プロトコル上安全(§2.5.1)かつ既にbest-effort扱いなので問題ない。

### 2.6 このギャップへの推奨対応(rev2で更新)

1. §2.3で述べた単一アクセサ方式で、A1・A2・A3・A5・A6・A7に
   タイムアウトを追加する。A4は「detached taskを束縛して寿命管理する」
   か「タスク内部でも同じアクセサ経由にする」かのどちらかで対処する
   (実装時に決定)。
2. A2(`tcpip_forward`)のタイムアウト経路では、best-effortで
   `cancel_tcpip_forward`を追加発火させる(§2.5.3)。A1は登録済みの
   `ctl_forwards`エントリの除去だけで足りる。
3. §2.5.2のチャネルリークは、**Gap B(§3)の結論次第で結論が変わる**
   ——詳細は§4(相互作用)を参照。単独では受容可能(reject、新規修正
   不要)だが、Gap Bの結論と矛盾しないことを条件とする。
4. rev1にあった「A1・A2のタイムアウトをtombstoneに繋ぐべきか」という
   論点は削除する(§2.4)。

## 3. Gap B: `mark_dead_if_same`の巻き添え範囲

### 3.1 rev1の提案(`AttachOrigin`)はissue #120そのものを再発させるregression
   ——採用しない

rev1は「`AttachOutcome::Establisher`(新規確立)か
`Ready`/`Waiter`(既存の共有ハンドルへの便乗)かを区別し、後者では
tombstone化しない」という`AttachOrigin`を提案したが、これは
**誤った前提に基づいていた**。`pool.rs:74-101`の`try_attach_with`実装を
読むと、`AttachOutcome::Ready`は「マップ中に`Ready`かつ`is_alive`な
エントリが見つかった」ことを意味するだけで、**refcountの値とは無関係**
——refcountが0→1に遷移する場合(=直前まで誰も使っていなかった)でも
`Ready`が返る。つまり`Ready`は「他のタブが使っている」ことを一切
保証しない。

**具体的な再発シナリオ**(issue #120の再現条件そのもの、単一タブ・
`TransportPreference::Auto`・公開鍵認証・ミッドセッションでの
サイレント切断):

| t | 出来事 | 現状(PR #119、Gap B未適用) | rev1のGap B(`AttachOrigin`)適用後 |
|---|---|---|---|
| 0 | セッションが死ぬ(`channel.wait()==None`→`FirstChannelOpen::Succeeded`) | tombstone化しない(正しい、正常終了に見える) | 同左 |
| 0 | `release`→refcount 0、タイマーarm | | |
| 3 | 再試行→`try_attach_with`→エントリは`Ready`(誰も保持していないので`try_lock()`成功、`is_closed()`はkeepalive未検出でまだ`false`)→**`AttachOutcome::Ready`** | | |
| 3〜13 | `channel_open_session`が**ハング**→`FirstChannelOpen::TimedOut` | `mark_dead_if_same`→`Dead`化 | `Ready`⇒`AttachOrigin::Shared`⇒**tombstone化しない** |
| 13 | 次の再試行 | `Dead`を見て`Establisher`→新規確立→**復旧**✅ | 依然`Ready`(`is_closed()`はまだ`false`)→再びハング→タイムアウト→… |
| 13〜60 | | 復旧済み | ループ、`try_attach_with`のたびに`idle_generation`も進む(`pool.rs:83`)ため元ADR §2.4のライブロックが再武装され続け、エントリは一切削除されない |
| 60 | orchestratorの再接続予算が尽きる | | **諦める** |
| 60+ | ユーザーが「再接続」を繰り返しタップ | 復旧する | タップのたびに`Ready`→10秒ハング→`idle_generation`再武装の繰り返し——**keepaliveが約4分後に検出するまで永久に回復しない** |

最後の行は、元ADR §2.4が「`always-connects.md`の最も鋭い形の違反」と
呼んだ状況そのものである(「ユーザーが辛抱強く手動操作を繰り返すほど
回復しなくなる」)。つまりrev1のGap Bは、修正の改良ではなく
**単一タブでのissue #120再現という最重要シナリオで、出荷済みの修正を
無効化してしまう**。なお非対称なのは`Failed`(即座に`SendError`が
返る場合、`is_closed()`は既に`true`なので`try_attach_with`が
`Establisher`を返し自己修復する)であり、rev1のGap Bが無力化するのは
具体的に元ADRが項目2として追加した`TimedOut`の経路である。

### 3.2 代替案: `russh::Error::ChannelOpenFailure`を根拠に
   「ピアから明示的な拒否」だけを区別する(採用)

元々のGap Bの動機(§3節、sshdの`MaxSessions`超過のような「このタブ
だけの局所的な失敗」でも共有ハンドル全体をtombstone化してしまう)自体は
妥当だが、必要なのは「Establisher/Ready/Waiterのどれか」という
プール側の情報ではなく、「**サーバーが実際に応答したか**」という
プロトコルレベルの証拠である。russhは`channel_open_session`が
サーバーから明示的な拒否(`SSH_MSG_CHANNEL_OPEN_FAILURE`)を受けた場合、
`Err(russh::Error::ChannelOpenFailure(reason))`
(`russh-0.48.2/src/client/mod.rs:450-452`、
`ChannelOpenFailure::{AdministrativelyProhibited,ConnectFailed,
UnknownChannelType,ResourceShortage,Unknown}`は`russh/src/lib.rs:481-487`)
を返す。これは「サーバーが我々のリクエストを解釈し、応答した」という
**プロトコル上の生存証明**であり、ハング・`SendError`・`Disconnect`
(いずれも「応答が無い/送信自体が失敗した」ことを示すだけで生存の
証拠にならない)とは性質が全く異なる。

```rust
pub(crate) enum FirstChannelOpen {
    Succeeded,
    /// サーバーが SSH_MSG_CHANNEL_OPEN_FAILURE で明示的に拒否した
    /// = 接続自体は生きている、tombstone化しない。
    RefusedByPeer,
    Failed,
    TimedOut,
}
```

`ssh_handler.rs:806`付近で既に`russh::Error`を保持しているため、
`matches!(e, russh::Error::ChannelOpenFailure(_))`という判定を1箇所
足し、呼び出し元3箇所(`isekai_pipe_quic_transport.rs:139,175`・
`lib.rs:1710`)の`matches!(first_open, Failed | TimedOut)`に
`RefusedByPeer`を含めない、という変更だけで実現できる。

**rev1の`AttachOrigin`案より優れている理由:**

- **issue #120を再発させない**: ハング・`SendError`・`Disconnect`は
  attach originに関わらず全てtombstone化を維持する——§3.1の
  再発シナリオの`TimedOut`はそのままtombstone化され続ける。
- **動機となったシナリオそのものにピンポイントで効く**: `MaxSessions`
  超過はまさに`ChannelOpenFailure`として現れる。
- **プールAPIの変更が一切不要**: `AttachOutcome`・`AcquireOutcome`・
  `pool.rs`のいずれにも触れない。`lib.rs`が`AcquireOutcome`を
  経由しない(§3.3参照)という問題も同時に解消される。
- **推測ではなく証拠に基づく**: `AttachOrigin`はプール側の帳簿から
  「たぶん他のタブが使っているはず」と推測するが、`RefusedByPeer`は
  ワイヤ上の応答を直接見ている。

**未検証の注記**: OpenSSHが`MaxSessions`超過時に実際にどの
`ChannelOpenFailure`理由コードを送るか(`ResourceShortage`・
`AdministrativelyProhibited`のどちらもありえ、サーバー実装によって
異なりうる)は未確認。したがって特定の理由コードにマッチさせるのでは
なく、**`ChannelOpenFailure`である、という事実だけ**でtombstone化を
見送る(理由コードによらず「サーバーが応答した」という一点だけを
根拠にする、より保守的な判定にする)。

### 3.3 `AttachOrigin`案のまま採用する場合の追加の欠陥(参考、不採用のため実装しない)

仮に`AttachOrigin`案を(§3.2ではなく)採用しようとした場合、
以下の追加の欠陥がある(不採用と決めたため実装はしないが、
将来似た設計が再提案された際の参考として記録する):

- **`lib.rs::run_russh_transport`は`AcquireOutcome`を経由しない**
  ——`pool::AttachOutcome`を直接match(`lib.rs:1666-1700`)しており、
  `isekai_pipe_quic_transport.rs`と異なる形をしている。rev1の
  §3.4「`AcquireOutcome`側に新しいフィールドを足すだけ」という
  想定は`isekai_pipe_quic_transport.rs`にしか当てはまらず、
  `lib.rs`側は4分岐の`match`から早期returnしている経路それぞれに
  別の変数を持ち回る必要があり、想定よりも変更が大きい。
- **`Waiter`の扱いの根拠が自己矛盾している**: `Waiter`→`Ok(v)`は
  「Establisherが`publish_success`した直後」を意味し、ハンドルの
  新鮮さで言えばEstablisher自身と同じ(直前に確立されたばかり)。
  rev1は`Waiter`を`Ready`と同じ`Shared`に分類していたが、これは
  「他のタブが使っているかもしれないから」という理由と「ハンドルが
  新鮮かどうか」という理由のどちらで分類しているのかが曖昧で、
  この2つの理由は`refcount`が0の`Ready`のケース(§3.1)で矛盾する。
  実際には「Establisherタブ自身も同じハンドルで`run_ssh_channel_loop`を
  実行しており、そちらが`FreshlyEstablished`としてtombstone化される」
  という別の経路がたまたまカバーしているだけで、設計として意図された
  ものではない。

### 3.4 案B-2(refcountベースの判定)の再検討

rev1は「refcountはタブの数を数えるだけで、その中の何個が正常かは
表さない」という理由でrefcountベースの判定を却下していた。この
理由は正しいが、より根本的な理由がもう一つある: `mark_dead_if_same`
(`pool.rs:157-167`)は現状refcountを一切参照しない設計であり、
これを見るように変更すると、「呼び出し元自身の直前の`release`が
まだ実行されていない状態でのrefcount」を、プールロックの下で
呼び出し元自身の分を差し引いて判断する必要が生じ、並行する
別の`try_attach_with`とのレースに対して脆くなる。§3.2の
`ChannelOpenFailure`ベースの判定はこの種の帳簿合わせを一切必要と
しないため、この却下判断は維持する。

## 4. Gap AとGap Bの相互作用(rev1が見落としていた点)

### 4.1 §2.5.2の「受容可能」という判断は、Gap Bの結論に依存する

rev1(§2.5項目3相当)は「`channel_open_session`のチャネルリークは、
どのみちそのハンドルはいずれ`is_closed()`検出でtombstone化される
経路に乗るので、実害は限定的」と論じていたが、この論拠が成り立つのは
**`TimedOut`が引き続きtombstone化を引き起こす場合に限る**。
rev1のGap B(`AttachOrigin`)を採用してしまうと、`Ready`経由の
`TimedOut`はtombstone化されなくなるため、同じ生きている(ように
見える)プールエントリが繰り返しタイムアウトに晒され、そのたびに
`self.channels`・`enc.channels`・sshdのセッションスロットを1つずつ
消費し続ける——`MaxSessions 10`ならおよそ10回のタイムアウトで
そのハンドル自体が(`is_closed()`はまだ`false`のまま)実質的に
使い物にならなくなる、という**Gap Bが対処しようとしていた
`MaxSessions`超過を、Gap B自身が引き起こす**という逆説的な結果になる。

§3.2で採用した`ChannelOpenFailure`ベースの判定ではこの問題は
起きない——`TimedOut`は引き続きtombstone化されるため、§2.5.2の
「受容可能」という判断はそのまま成り立つ。

### 4.2 元ADRのI1〜I4への影響: 無し(確認済み)

`mark_dead_if_same`の呼び出しを一部スキップする(§3.2の
`RefusedByPeer`)ことは、I1(アタッチとreleaseの1対1対応)・I2
(tombstone化はstateのみ変更)・I3(`publish_failure`の扱い)・I4
(out-of-bandなtombstoneはrefcount≥1を保持)のいずれにも抵触しない
——これらは「tombstone化をどう行うか」を規定する不変条件であり、
「tombstone化を行うかどうか」の判断基準を制約するものではないため。
呼び出し元は引き続き`mark_dead_if_same`の呼び出し直後に`release`を
呼ぶ(`lib.rs:1710-1712`・`isekai_pipe_quic_transport.rs:139-143,175-179`)
ため、I4の前提も保たれる。

### 4.3 A4(detached task)は元ADRの暗黙の前提を壊す

元ADR §3.5のI1監査は「本番のアタッチ経路2関数」を全数監査したが、
「`release`が呼ばれた後、誰も`pooled.handle`を握っていない」という
暗黙の前提までは検証していなかった。§2.2のA4(`teardown_forward`が
spawnするdetached task)はこの前提を破る——`release`後もこのタスクが
`pooled.handle`のMutexを握り続けうる。これは元ADRのI1〜I4を無効化する
ものではないが(refcountの整合性自体は保たれる)、`try_lock`ベースの
生存確認(`is_alive`述語)の実効性を損なう別の経路として、実装時に
第5の注意点として書き留めておく価値がある:「`PooledSshHandle::handle`の
Mutexを、await境界を越えて保持するdetached taskを作ってはならない」。

### 4.4 `is_alive`のフェイルオープン自体は本ADRの対象外だが、両ギャップの
   共通の根本原因であることは明記しておく

`/code-review`の指摘のうち、`is_alive`述語
(`|p| p.handle.try_lock().map(|h| !h.is_closed()).unwrap_or(true)`)が
ロック競合時に「生きている」へフェイルオープンすること自体は、
元ADR §3.2が既に認めた残存レースであり、本ADRのスコープには含めない
(元ADR側で受容済みの判断を覆す新しい理由はここでは見つかっていない)。
ただし、§2(7箇所の未保護ロック、特にA4のようなdetachedな保持者)が
存在することによって、このフェイルオープンが実際に踏まれる頻度・
持続時間が増えるという関係にあることは記録しておく——根本原因は
やはり「一部の処理がロックを保持したまま無期限にawaitしうること」に
あり、`is_alive`のフェイルオープンはその結果を悪化させる副次的な
要因である。

## 5. 未決事項

- §2.3の単一アクセサ(`with_handle_timeout`相当)の具体的なシグネチャは
  未設計。A4のようなdetached task内からも同じ経路を通せる形にする
  必要がある。
- A5・A6(`-L`/`-D`フォワードで接続を受けるたびにspawnされるタスク)
  の失敗を`mark_dead_if_same`に接続すべきかどうかは未検討——
  §3.2の「サーバーが応答したかどうか」という判定軸をここにも
  適用できるか、実装時に再検討する。
- 本ADRが対象としない8件(orchestrator.rsの`pending_wake`関連3件・
  `resume_client.rs`のnetwork-wake関連2件・`rebind_driver.rs`/
  `orchestrator.rs`の同期ファイルI/O2件・`is_alive`のフェイルオープン
  1件[§4.4参照])は、いずれも本PRのpool.rs修正そのものではなく
  このブランチの先行コミットに既にあったコードへの指摘であり、
  スコープが異なるため引き続き本ADRには含めない。

## 6. 次のステップ

rev2をopus-adversarial-consultの同じレビュアーへ再送し、収束を
確認した上で実装に着手する。
