# ADR: pool.rs修正(issue #120/PR #119)実装後にコードレビューで見つかった残存ギャップ

- **Status**: Draft(2026-09-17起草、rev3。`ADR_ANDROID_POOL_STALE_HANDLE.md`
  (rev4、収束済み)の実装(PR #119、コミット4793ebcd・c4eb37ac)がmainへ
  マージ前の`/code-review`で発見された指摘のうち、同ADRの設計範囲に
  直接関わる2件を切り出して検討する。opus-adversarial-consult round 1で
  rev1のGap Bの設計(`AttachOrigin`)が**issue #120そのものを再発させる
  regression**であること、Gap Aの調査漏れ(3箇所と書いていたが実際は
  7箇所)が判明しrev2で書き直した。round 2で、rev2の代替案
  (`RefusedByPeer`)も**同じ理由で別の症状のregression**を生むことが
  判明し(レビュアー自身が「round 1での自分の見落とし」として指摘)、
  **Gap Bは「検討した結果、却下する」という結論に確定した**。Gap Aも
  3点の欠陥(A4の扱いの内部矛盾・動的ポート時の緩和策が原理的に
  実装不能・アクセサのシグネチャがMSRV 1.75で書けない)を修正しrev3とした)
- **対象**: `rust-core/src/transport/ssh_handler.rs`・
  `rust-core/src/transport/forward.rs`・
  `rust-core/src/transport/file_preview_exec.rs`
  (`pooled.handle`のMutexをawait中握り続ける全箇所)、
  `rust-core/src/pool.rs`(`mark_dead_if_same`、変更しないことを確認)
- **入力**: PR #119の`/code-review`(2026-09-17実施、8観点並列+4バッチ検証、
  10件中2件が本ADRの対象)。`ADR_ANDROID_POOL_STALE_HANDLE.md`(収束済み、
  §2.4「削除タイマーが値に関わらず成熟しないライブロック」・§3.2「限界」・
  §3.4項目2)。opus-adversarial-consult round 1
  (`/tmp/claude-1001/-home-cuzic-isekai-terminal/2285f8d6-e7bb-4a20-83c5-de317b68d9c7/scratchpad/opus-review-pool-followup-gaps.md`)・
  round 2
  (`/tmp/claude-1001/-home-cuzic-isekai-terminal/2285f8d6-e7bb-4a20-83c5-de317b68d9c7/scratchpad/opus-review-pool-followup-gaps-round2.md`)
- **拘束される既存ルール**: `.claude/rules/always-connects.md`

---

## 1. 背景

`ADR_ANDROID_POOL_STALE_HANDLE.md`(以下「元ADR」)は、SSH接続プールが
死んだハンドルを再利用し続けるバグ(issue #120)を、(a)`try_attach`への
生存確認+tombstone-in-place(I1〜I4)、(b)`run_ssh_channel_loop`の
最初の`channel_open_session()`だけを`RUN_EXEC_TIMEOUT`でタイムアウトさせ
`FirstChannelOpen::{Succeeded,Failed,TimedOut}`を返す、という2本柱で修正した
(PR #119、mainへのマージ前)。マージ前の`/code-review`が、元ADRの設計
範囲に関連する2つの未検討ギャップ(以下Gap A・Gap B)を見つけた。

- **Gap A**(§2、修正が必要): `channel_open_session`以外にも
  `pooled.handle`のMutexをawait中握り続ける箇所が複数残っており、
  タイムアウト保護が未適用。
- **Gap B**(§3、**検討の結果、却下**): `mark_dead_if_same`が
  「このタブだけの局所的な失敗」でも共有ハンドル全体をtombstone化する
  ように見える点について、2回のopus-adversarial-consultを経て
  「現状の無条件tombstoneの方が正しい」という結論に達した。詳細は§3。

## 2. Gap A: `channel_open_session`以外にもタイムアウト保護の無い
   ロック保持箇所が**7箇所**ある

### 2.1 現状の全数調査

`pooled.handle`(型`Arc<tokio::sync::Mutex<client::Handle<RusshEventHandler>>>`、
`ssh_handler.rs:558`)は`run_ssh_channel_loop`内で
`session = pooled.handle.clone()`(`ssh_handler.rs:966`)として
`transport/forward.rs`・`transport/file_preview_exec.rs`にも渡される。
元ADR §3.4項目2は「あらゆる保持者に適用しないと生存確認の`try_lock`が
無力化される」と**正しく**要件を書いていたが、実装(元ADRのPR #119)は
`:802`(`channel_open_session`)にしかこの保護を適用しておらず、以下の
7箇所全てが「await中も同じMutexを握り続け、タイムアウト保護が無い」
状態のまま残っている:

| # | 箇所 | 操作 | 頻度・実行コンテキスト |
|---|---|---|---|
| A1 | `ssh_handler.rs:867` | `streamlocal_forward` | タブごとに1回、opt-in(`ctl_streamlocal.rs:41`の`ctl_socket_forward_enabled()`が既定OFF) |
| A2 | `ssh_handler.rs:1043` | `tcpip_forward` | ユーザーが`-R`相当のフォワードを追加するたび |
| A3 | `ssh_handler.rs:1112` | `cancel_streamlocal_forward` | タブ終了時に1回 |
| A4 | `forward.rs:74` | `cancel_tcpip_forward` | `teardown_forward`(`ssh_handler.rs:1057,1070,1107`から呼ばれる)内の`tokio::spawn`されたタスクの中 |
| A5 | `forward.rs:124` | `channel_open_direct_tcpip` | `-L`相当のフォワードでTCP接続を受け付けるたびにspawnされるタスクの中、上限無し |
| A6 | `forward.rs:199` | `channel_open_direct_tcpip` | `-D`(SOCKS)相当のフォワードで接続を受け付けるたびにspawnされるタスクの中、同上 |
| A7 | `file_preview_exec.rs:26` | `channel_open_session` | ファイルプレビュー機能を使うたびに1回 |

(`run_exec_on_handle`、`ssh_handler.rs:681-689`は既に
`tokio::time::timeout(RUN_EXEC_TIMEOUT, run_exec_on_handle_inner(..))`
[`:685`]で保護されている——元ADRが修正した`:802`以外で唯一保護済みの
箇所)

### 2.2 A1〜A7はすべて同じ対処で直せる(round 2レビューで訂正)

A4(`teardown_forward`が`tokio::spawn`する検出用タスク)は、
`run_ssh_channel_loop`が返って`pool::release`が呼ばれた後もこの
タスクが`pooled.handle`のMutexを握り続けうるという点で一見特殊に
見えるが、「タイムアウト付きのawaitを、spawnされたタスクの**中**で
行う」という対処を取れば、A4もA1〜A3・A5〜A7と全く同じ形で保護できる
——`teardown_forward`はタブのI/Oループを止めないために意図的に
`spawn`している(呼び出し元の`ssh_handler.rs:1057,1070`はタブの
`select!`ループの中、`:1107`はループ終了直後でawaitする相手が無い)
ため、「detachさせず束縛して寿命管理する」という代替策は、タブの
シェルループを止めてしまうか、あるいはキャンセル処理自体を諦めるかの
どちらかにしかならず、実質的に選べない。したがって対処は
「spawnされたタスクの中身自体に、下記§2.3のアクセサ経由で
有界時間のawaitを行わせる」の一本に統一する(A4を特別扱いしない)。

### 2.3 推奨する対処の形: 個別の`tokio::time::timeout`の羅列ではなく、
   単一のアクセサに集約する

7箇所(将来さらに増えうる)へ`tokio::time::timeout(RUN_EXEC_TIMEOUT, ...)`を
個別にコピー&ペーストしていく方式は、元ADR §3.4項目1が
「`try_attach`のデフォルト付きラッパーを`try_attach_with`と併存させない
理由」として述べた「次に新しいプールが追加されたときに同じバグを
静かに引き継ぐ」のと同じ構造の問題を生む。`PooledSshHandle`に
「タイムアウト付きでロックして操作する」単一のアクセサを用意し、
7箇所全て(既に保護済みの`:802`・`run_exec_on_handle`含む、二重の
規約が生まれないよう合わせて寄せる)をそちらへ経由させることを推奨する。

**シグネチャ(round 2レビューで確定、当初案は書けなかった)**:
`rust-core/Cargo.toml`は`rust-version = "1.75"`を宣言しており、
`AsyncFnOnce`(安定化は1.85)が使えない。したがって
`PooledSshHandle::with_handle_timeout(|h| async { ... })`という
素朴なクロージャ形は、`MutexGuard`を呼び出し境界を越えて借用する
async blockをコンパイルできない。1.75で書ける形は次の通り
(boxed futureを介したHRTB):

```rust
impl PooledSshHandle {
    pub(crate) async fn with_handle<T>(
        &self,
        timeout: Duration,
        f: impl for<'a> FnOnce(
            &'a mut client::Handle<RusshEventHandler>,
        ) -> Pin<Box<dyn Future<Output = T> + Send + 'a>>,
    ) -> Result<T, tokio::time::error::Elapsed> {
        tokio::time::timeout(timeout, async {
            let mut guard = self.handle.lock().await;
            f(&mut guard).await
        }).await
    }
}
```

呼び出し側は`pooled.with_handle(RUN_EXEC_TIMEOUT, |h|
Box::pin(async move { h.tcpip_forward(a, p).await })).await`という形に
なる。1呼び出しごとに1回ヒープ確保が発生する見た目の悪さはあるが、
`&mut self`を要求するrussh側メソッド(`tcpip_forward`・
`streamlocal_forward`)と`&self`で足りるメソッド
(`cancel_*`・`channel_open_direct_tcpip`)の両方を`DerefMut`経由で
統一的に扱え、かつロック取得自体を含めて`timeout`で包むため
§2.5.2で述べる「ロック取得中にタイムアウトした場合は何もリークしない」
という性質も保たれる。

**このアクセサ経由にする際の注意点(round 2レビューで追加)**:
A5・A6・A7(1回の転送接続・1回のファイルプレビューという、単体の
操作の失敗)がタイムアウトしても、`TransportEvent::Disconnected`を
発行してはならない——これらの失敗はセッション全体の生死とは無関係な
per-operationの失敗であり、現状も単に`warn!`して処理を続けている。
セッションの生死に関わるのは`:802`(最初の`channel_open_session`)
だけであり、この意味論を保ったままアクセサへ寄せる。

### 2.4 (rev1で誤っていた論点、rev2で削除済み)「A1・A2のタイムアウトを
   tombstoneに繋ぐべきか」という非対称性の議論は、そもそも成立しない

rev1の§2.3・§2.5項目2は、「`streamlocal_forward`/`tcpip_forward`の
タイムアウトを`mark_dead_if_same`に接続すべきかどうか」という論点を
立てていたが、round 1レビューの指摘により**この配線は現状存在せず、
rev1もそれを新設する提案をしていなかった**ことが判明した——
`mark_dead_if_same`が呼ばれるのは`run_ssh_channel_loop`の戻り値
`FirstChannelOpen`経由のみ(`isekai_pipe_quic_transport.rs:139-141,175-177`・
`lib.rs:1710-1712`)であり、A1・A2はこの戻り値と無関係な
`run_ssh_channel_loop_after_first_open`内部の処理である。この論点は
削除したままとする(§3でGap Bを却下したことにより、A5・A6についても
同種の配線を新設しない、という結論で一貫する——§3.5参照)。

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
チャネルリーク**

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
何もリークしない**(§2.3のアクセサはロック取得と操作本体の両方を
同じ`timeout`で包むため)。リークが起きるのは「ロックは取れたが、
サーバーからの確認応答を待っている間にタイムアウトした」場合に限られる。

**この受容判断(新規修正しない)は、§3でGap Bを却下したことにより
無条件に成り立つ**(rev2時点ではGap Bの結論に依存する条件付きの
判断だったが、Gap B却下によりその依存が消えた——詳細は§4.1)。

**2.5.3 `tcpip_forward`のタイムアウトは、`bind_port`が非ゼロの場合に
限り緩和可能な、サーバー側の取り残されたフォワード登録を生みうる**

`tcpip_forward`(A2)がタイムアウトした場合、
`remote_forwards.lock().insert(...)`(`ssh_handler.rs:1046`)・
`active_forwards.insert(...)`(`:1047-1051`)のどちらも実行されない。
しかし、我々の`Msg::TcpIpForward`送信自体は(ロック取得後)既に
サーバーへ届いている可能性があり、サーバー側が実際にポートを
バインドしてから応答が我々のタイムアウトに間に合わなかった、という
ケースがありうる。この場合、サーバーが実際にリスンを開始している
にもかかわらずこちら側は`active_forwards`にエントリを持たないため、
`teardown_forward`(`ssh_handler.rs:1105-1108`、
`active_forwards.drain()`を走査)はこの存在を認識できず
`cancel_tcpip_forward`が一生呼ばれない。ユーザーが同じ`-R`を再度
追加しようとすると、サーバー側は「ポートは既にバインド済み」として
拒否し続け、接続自体は健全であるにもかかわらずこの機能だけが
(このプールされたハンドル自体が破棄されるまで)使えなくなる。

**この緩和策は`bind_port != 0`の場合にしか適用できない
(round 2レビューで発見、当初案の見落とし)**: `bind_port == 0`
(ユーザーが動的ポート割当を要求した場合、`ssh_handler.rs:1043-1045`が
明示的にこのケースを扱っている)では、サーバーが実際に選んだポート
番号は、我々が受け取れなかった応答の中にしか含まれない
(`tcpip_forward`は`Ok(bound_port)`として返す、RFC 4254 §7.1・
OpenSSHの実装ともに`cancel-tcpip-forward`はポート0をワイルドカードとして
扱わない)。つまり`bind_port == 0`の場合、そもそも何番のポートを
`cancel_tcpip_forward`すべきか知る手段が無く、**このケースの
サーバー側取り残しはプロトコルレベルで回避不能**——受容する残存事項
として記録する(実害は「動的ポートで`-R`を使ったタブの1つが
タイムアウトした場合に限り、そのポートがこのプールされたハンドルの
寿命が尽きるまで塞がれたままになる」)。

`bind_port != 0`(ユーザーが固定ポートを指定した通常ケース)では、
best-effortで`cancel_tcpip_forward`を追加発火させることを推奨する
(A3・A4と同じ「失敗してもログに残すだけ」の扱いでよい)。ただし
この緩和策自体の呼び出しも、`tcpip_forward`がタイムアウトした
原因(接続そのものがハングしている)を引き継ぐ可能性があるため、
**インラインで(`select!`アーム内で直接)awaitしてはならず**、
§2.3のアクセサ経由で有界時間に収める。

`streamlocal_forward`(A1)は影響がより軽微——`pooled.ctl_forwards`への
登録(`ssh_handler.rs:866`)は呼び出し**前**に行われ、`Err`時は
`:945`で除去されるため、タイムアウト時も同様に除去すればよい。
残るリスクはリモートソケットファイルの残留のみで、パス自体が
128bitランダム(`ctl_streamlocal.rs:45-47`)なので衝突の心配は無く、
`isekai_pipe_core::sweep_stale_sockets`がプレフィックスベースで
掃除する。過剰な対応は不要。

A3・A4(両方の`cancel_*`)自体をタイムアウトさせることは、
プロトコル上安全(§2.5.1)かつ既にbest-effort扱いなので問題ない。

### 2.6 このギャップへの推奨対応

1. §2.3の単一アクセサ方式で、A1〜A7全て(および既存の`:802`・
   `run_exec_on_handle`)を統一的に保護する。A4を含め特別扱いは
   しない(§2.2)。
2. A2(`tcpip_forward`)のタイムアウト経路では、`bind_port != 0`の
   場合に限りbest-effortで`cancel_tcpip_forward`を追加発火させる
   (§2.5.3、アクセサ経由で有界時間に収める)。`bind_port == 0`の
   場合の取り残しは受容する。A1は登録済みの`ctl_forwards`エントリの
   除去だけで足りる。
3. §2.5.2のチャネルリークは受容する(新規修正不要)——§3でGap Bを
   却下したことにより、この判断はもう条件付きではない(§4.1)。
4. A5・A6の失敗を`mark_dead_if_same`に接続することは**しない**
   ——理由はGap B(§3)と同じ一般論(§3.2の定理)がここにも当てはまる。

## 3. Gap B: `mark_dead_if_same`の巻き添え範囲 —— **検討の結果、却下**

### 3.1 rev1の提案(`AttachOrigin`)はissue #120そのものを再発させるregression
   ——不採用

rev1は「`AttachOutcome::Establisher`(新規確立)か
`Ready`/`Waiter`(既存の共有ハンドルへの便乗)かを区別し、後者では
tombstone化しない」という`AttachOrigin`を提案したが、これは
誤った前提に基づいていた。`pool.rs:74-101`の`try_attach_with`実装を
読むと、`AttachOutcome::Ready`は「マップ中に`Ready`かつ`is_alive`な
エントリが見つかった」ことを意味するだけで、refcountの値とは無関係
——refcountが0→1に遷移する場合(=直前まで誰も使っていなかった)でも
`Ready`が返る。issue #120の単一タブ再現シナリオでは、`Ready`が
「毎回」返る(タブは常に自分1人だけ)ため、`Ready`を「共有中だから
tombstone化しない」扱いにすると、`always-connects.md`が禁じる
「ユーザーが辛抱強く手動操作を繰り返すほど回復しなくなる」という
状態を復活させてしまう(詳細タイムラインは元rev1のもの、§3.2の
`RefusedByPeer`案も同型の欠陥を持つため以下でまとめて示す)。

### 3.2 rev2の代替案(`RefusedByPeer`)も、動機となったシナリオ自体を
   壊すregressionだった——これも不採用

rev2は`AttachOrigin`の代わりに、`russh::Error::ChannelOpenFailure`
(サーバーが実際に応答したというプロトコル上の生存証明)を根拠にした
`FirstChannelOpen::RefusedByPeer`を提案した。これは「issue #120を
再発させないか」というチェックには通ったが(ハング・`SendError`・
`Disconnect`はattach originに関わらず全てtombstone化を維持するため)、
**Gap Bが本来解決しようとしていたシナリオ(sshdの`MaxSessions`超過)
自体を悪化させる**という、チェックされていなかった別方向の欠陥を
opus-adversarial-consult round 2で指摘された。

`orchestrator.rs:786`の自動再接続ループは
`was_connected && !user_initiated && !graceful_exit`のときのみ
armされる。**一度も`ConnPhase::Connected`に到達していない新規タブの
初回`channel_open_session`が拒否された場合、このタブは自動再試行の
対象にならない**(`orchestrator.rs:799`の`else`腕、1回だけ
`Action::NotifyDisconnected`を出して終わる)。この状態で
`RefusedByPeer`によりtombstone化を見送ると:

| | 現状(`main`、PR #119) | `RefusedByPeer`適用後 |
|---|---|---|
| 10タブ目(sshdの`MaxSessions 10`に達している状態)の初回試行 | `try_attach_with`→`Ready`→`channel_open_session`→`Err(ChannelOpenFailure)`→`FirstChannelOpen::Failed`→`mark_dead_if_same`→`EntryState::Dead` | 分類まで同じ、その後**tombstone化しない**、エントリは`Ready`のまま |
| ユーザーがタブを開き直す/再接続をタップ | `try_attach_with`→`Dead`を見て**`Establisher`**→`establish_fresh`→新しい2本目のプールされたハンドル→10タブ目は**接続できる**✅ | `try_attach_with`→エントリは依然`Ready`(`is_closed()==false`、`try_lock()`成功)→**再び`Ready`**→再び拒否される |
| 何度再試行しても | 成功する | 拒否され続ける |
| 定常状態 | タブ1〜9はハンドル#1、タブ10〜18はハンドル#2——10タブごとに1回の余分なブートストラップというコストで、正確に必要な本数の接続を維持する | **10タブ目は永久に開けない**(このプールエントリが偶然別の理由でtombstone化されるか、`ISEKAI_PIPE_QUIC_IDLE_GRACE`が成熟するまで——元ADR §2.4が示した通り、アタッチの試行が来続ける限りこの猶予は決して成熟しない) |

最後の行は、`AttachOrigin`案を却下した理由(§3.1、元ADR §2.4が
「`always-connects.md`の最も鋭い形の違反」と呼んだ状況)と全く同じ
欠陥であり、単に「再接続ループ」から「新規タブのオープン」へ場所が
移っただけである。

### 3.3 一般論: `Ready`なエントリの「出口」は2つしか無く、
   そのどちらかを塞ぐ設計は全てライブロックする

`AttachOrigin`と`RefusedByPeer`の両方が同じ種類の欠陥を持つのは
偶然ではない。以下は両方の失敗から抽出できる一般的な定理であり、
将来また似た「条件Xのときはtombstone化しない」という第3の案が
提案された際に、同じ検証をやり直さずに済むよう明記しておく:

> `EntryState::Ready`なエントリが消える経路は、tombstone化(即座に
> `Establisher`を生む)と、アイドル猶予切れによる削除
> (`pool::release`のタイマー)の2つしか無い。元ADR §2.4が示した通り、
> 後者は「アタッチの試行が来続ける限り(`try_attach_with`のたびに
> `idle_generation`が進むため、`pool.rs:83`)決して成熟しない」。
> したがって「条件Xのときはtombstone化しない」という形のルールは
> **どんなXであっても**、条件Xを満たすエントリを「呼び出し元には
> 二度と使えないと分かっているのに、プールからは永久に払い出され
> 続ける」状態にする。Xがattach origin(`AttachOrigin`)であっても
> エラーの種類(`RefusedByPeer`)であっても結論は変わらない
> ——問題は「唯一使える出口を塞いだこと」自体にある。

これが、元ADRの「無条件にtombstone化する」というルールが場当たり的な
簡略化ではなく、**唯一停止するルールだった**ことの理由である。

### 3.4 rev1の前提そのものが誤っていた: Gap Bが解決しようとしていた「害」は、
   実は害ではない

rev1の§3(旧)は「1タブの局所的な失敗が、共有ハンドル全体を
tombstone化し、プーリングの目的そのものを無に帰す」と述べていたが、
これは実際の挙動を正しく追っていなかった。`mark_dead_if_same`が
tombstone化するのは**プールの1スロット**であり、次の
`Establisher`はそのスロットの値を新しいハンドル(2本目の接続)で
置き換える(`pool.rs:117-129`の`publish_success`)。既存のタブ
1〜9は自分自身が保持する`Arc`をそのまま使い続けるため一切
影響を受けない。つまりプーリングは「壊れる」のではなく
「2本目の接続の上で再形成される」——タブ10以降が2本目のハンドルに
プールされ直すだけである。

しかも、この2本目の接続は**本質的に必須**である: 1本の接続で
同時に開けるチャネル数が(sshdの`MaxSessions`により)10個までと
決まっている以上、11個目のチャネルを開くには2本目の接続を作る
以外に方法が無い。tombstone化はこの状況への誤った反応ではなく、
唯一の正しい反応であり、`ssh(1)`の`ControlMaster`が`MaxSessions`に
達したときに人間が手動で行う操作(新しい接続を張る)と同じことを
自動でやっているに過ぎない。したがってrev1のGap Bは、そもそも
実在しない問題を解決しようとしていた。

### 3.5 結論: Gap Bは却下する。現状の無条件tombstoneが正しい

- `AttachOutcome::Ready`は「他のタブが使っている」ことを一切
  保証しない(§3.1)ため、attach originに基づく判定は使えない。
- `russh::Error::ChannelOpenFailure`に基づく判定も、動機となった
  シナリオ自体で新規タブが永久に開けなくなるという、より直接的な
  regressionを生む(§3.2)。
- §3.3の一般論により、「条件Xのときはtombstone化しない」という
  形の設計は原理的に全て同じ欠陥を持つ。
- rev1が「害」と呼んでいたもの自体が、実際には正しい・必須の
  挙動だった(§3.4)。

以上により、`mark_dead_if_same`の呼び出し条件(`FirstChannelOpen::
{Failed, TimedOut}`で無条件に呼ぶ、現状のPR #119の実装)は変更しない。

**唯一の残存事項として受容するもの**: `MaxSessions`のような上限を
持つホストでは、上限を超えるたびに1本余分なブートストラップが発生し、
かつ上限に達した瞬間のタブは自動再試行の対象にならない
(`orchestrator.rs:786`の条件のため)ので、ユーザーが1回タブを
開き直す必要がある。これが実際に問題として観測された場合の唯一正しい
修正は、1つのキーに対して**複数のハンドルを保持できるプール**
(容量を意識したper-hostコネクションプール)にすることであり、
tombstone化の抑制ではない。これは今回のADRの対象より大きな変更であり、
実際の報告が無い現時点では着手しない。

§5(旧rev2)にあった「A5・A6の失敗を`mark_dead_if_same`に接続すべきか」
という未決事項は、この§3.3の定理により**接続しない、で確定**する
——A5・A6はそもそもプールキーを持たない per-connection の失敗であり、
セッション単位の生死とは無関係でもある。

## 4. Gap AとGap Bの相互作用

### 4.1 Gap Bを却下したことで、§2.5.2の受容判断は無条件に成り立つようになった

rev2時点では、「`channel_open_session`のチャネルリークは、どのみち
そのハンドルはいずれ`is_closed()`検出でtombstone化される経路に乗るので
実害は限定的」という受容判断は、「`TimedOut`が引き続きtombstone化を
引き起こす場合に限る」という条件付きだった。Gap Bを却下し、
`TimedOut`が無条件にtombstone化を引き起こす現状の挙動を維持することが
確定したため、この条件は無条件に満たされる——rev2で発見した
「Gap AとGap Bの結論が矛盾しうる」というリスクは、Gap Bを却下した
ことで解消された(そもそも§2.5.2の受容判断を成り立たせるために
Gap Bを却下したのではなく、Gap B自体が独立してregressionだったために
却下したのだが、結果として整合性の問題も同時に消えている)。

### 4.2 元ADRのI1〜I4への影響: 無し(Gap Bを却下したため、そもそも
   `mark_dead_if_same`の呼び出し条件を変更しない)

Gap Bを却下したことで、`mark_dead_if_same`の呼び出し条件・処理内容
どちらも変更しない。したがって元ADRのI1〜I4に触れる変更は本ADRには
含まれない。

### 4.3 A4・A5・A6(spawnされたタスク)は、元ADRの暗黙の前提を破る
   ——I5として明文化する

元ADR §3.5のI1監査は「本番のアタッチ経路2関数」における
「アタッチとreleaseの1対1対応」を全数監査したが、「`release`が
呼ばれた後、誰も`pooled.handle`を握っていない」という暗黙の前提までは
検証していなかった。A4(`teardown_forward`がspawnするタスク)・
A5・A6(転送接続ごとにspawnされるタスク)はこの前提を破りうる——
`release`後もこれらのタスクが`pooled.handle`のMutexを握り続けることが
ありえる。これは元ADRのI1〜I4を無効化するものではないが
(refcountの整合性自体は保たれる)、`try_lock`ベースの生存確認
(`is_alive`述語)の実効性を損なう独立した経路であり、元ADRのI1〜I4に
並ぶ第5の不変条件として明文化する価値がある:

> **I5 — `PooledSshHandle::handle`のtokio Mutexを、await境界を
> 越えて保持するdetached/spawnされたタスクを作ってはならない。
> 保持する場合は§2.3のアクセサ経由で必ず有界時間に収めること。**

### 4.4 `is_alive`のフェイルオープン自体は本ADRの対象外だが、
   Gap Aの根本原因であることは明記しておく

`/code-review`の指摘のうち、`is_alive`述語
(`|p| p.handle.try_lock().map(|h| !h.is_closed()).unwrap_or(true)`)が
ロック競合時に「生きている」へフェイルオープンすること自体は、
元ADR §3.2が既に認めた残存レースであり、本ADRのスコープには含めない
(元ADR側で受容済みの判断を覆す新しい理由はここでは見つかっていない)。
ただし、§2(7箇所の未保護ロック、特にI5が対象とするspawnされた
保持者)が存在することによって、このフェイルオープンが実際に踏まれる
頻度・持続時間が増えるという関係にあることは記録しておく——根本原因は
やはり「一部の処理がロックを保持したまま無期限にawaitしうること」に
あり、`is_alive`のフェイルオープンはその結果を悪化させる副次的な
要因である。

## 5. 未決事項

- §2.3のアクセサ(`with_handle`)の具体的な実装は本ADRで方向性を
  固めたが、実装時に細部(エラー型・A2の`bind_port != 0`緩和策の
  正確な配線)を詰める。
- 本ADRが対象としない8件(orchestrator.rsの`pending_wake`関連3件・
  `resume_client.rs`のnetwork-wake関連2件・`rebind_driver.rs`/
  `orchestrator.rs`の同期ファイルI/O2件・`is_alive`のフェイルオープン
  1件[§4.4参照])は、いずれも本PRのpool.rs修正そのものではなく
  このブランチの先行コミットに既にあったコードへの指摘であり、
  スコープが異なるため引き続き本ADRには含めない。

## 6. 次のステップ

rev3をopus-adversarial-consultの同じレビュアーへ再送し、収束を
確認した上で実装に着手する(Gap Aのみが実装対象、Gap Bは
「検討したが却下」として記録に残す)。
