# ADR: pool.rs修正(issue #120/PR #119)実装後にコードレビューで見つかった2つの残存ギャップ

- **Status**: Draft(2026-09-17起草。`ADR_ANDROID_POOL_STALE_HANDLE.md`(rev4、収束済み)の
  実装(PR #119、コミット4793ebcd・c4eb37ac)がmainへマージ前の`/code-review`で
  発見された指摘のうち、同ADRの設計範囲に直接関わる2件を切り出して検討する。
  実装済みの本体修正(try_attach_with+tombstone-in-place、初回使用時タイムアウト)を
  差し戻す話ではなく、その上に追加で必要な改善の設計判断)
- **対象**: `rust-core/src/transport/ssh_handler.rs`(`streamlocal_forward`・
  `tcpip_forward`・`cancel_streamlocal_forward`呼び出し箇所)、
  `rust-core/src/pool.rs`(`mark_dead_if_same`)、
  `rust-core/src/isekai_pipe_quic_transport.rs`・`rust-core/src/lib.rs`
  (`AcquireOutcome::Attached`・`mark_dead_if_same`呼び出し箇所)
- **入力**: PR #119の`/code-review`(2026-09-17実施、8観点並列+4バッチ検証、
  10件中2件が本ADRの対象)。`ADR_ANDROID_POOL_STALE_HANDLE.md`(収束済み、
  §3.2「限界」・§3.4項目2)
- **拘束される既存ルール**: `.claude/rules/always-connects.md`

---

## 1. 背景

`ADR_ANDROID_POOL_STALE_HANDLE.md`(以下「元ADR」)は、SSH接続プールが
死んだハンドルを再利用し続けるバグ(issue #120)を、(a)`try_attach`への
生存確認+tombstone-in-place(I1〜I4)、(b)`run_ssh_channel_loop`の
最初の`channel_open_session()`だけを`RUN_EXEC_TIMEOUT`でタイムアウトさせ
`FirstChannelOpen::{Succeeded,Failed,TimedOut}`を返す、という2本柱で修正した
(PR #119、mainへのマージ前)。この実装自体はCI(`rust-core-test-linux`含む
必須5チェック)を通過しているが、マージ前の`/code-review`が、元ADRの設計
範囲に関連する2つの未検討ギャップを見つけた。どちらも「今回のバグを再発
させる」ような致命的なものではないが、元ADRが暗黙に前提としていた範囲を
超える部分なので、対応方針を決めてから着手する。

## 2. ギャップA: `channel_open_session`以外の3箇所にタイムアウト保護が無い

### 2.1 現状

`pooled.handle`(型`Arc<tokio::sync::Mutex<client::Handle<RusshEventHandler>>>`、
`ssh_handler.rs:558`)は、`run_ssh_channel_loop`内で最初の
`channel_open_session()`だけがawait中もこのMutexを握り続ける操作として
特定され、`RUN_EXEC_TIMEOUT`(10秒、`ssh_handler.rs:679`)でタイムアウト
保護された(`ssh_handler.rs:802`)。しかし、同じ`pooled.handle`
(または`session = pooled.handle.clone()`、`ssh_handler.rs:966`)を
ロックしたままawaitする操作は他に3箇所あり、いずれもタイムアウト保護が
無い:

| 箇所 | 行 | 操作 | 用途 |
|---|---|---|---|
| A1 | `ssh_handler.rs:867` | `pooled.handle.lock().await.streamlocal_forward(path).await` | tmux迂回control-plane(Epic M、opt-in)のUDSフォワード開始 |
| A2 | `ssh_handler.rs:1043` | `session.lock().await.tcpip_forward(...).await` | `TransportCommand::AddRemoteForward`によるリモートポートフォワード開始 |
| A3 | `ssh_handler.rs:1112` | `session.lock().await.cancel_streamlocal_forward(path).await` | フォワード終了時のクリーンアップ(既に`debug!`ログのみのbest-effort扱い、`:1113`) |

これは元ADR自身が§3.2の「限界」で明記していた懸念の実例である
——項目2(初回使用時タイムアウト)を「項目1(`try_attach`の生存確認)の
前提条件」と位置づけた理由は、まさに「`run_ssh_channel_loop`が
await中もMutexを握り続けるため、ハングした保持者がいる限り
`try_lock()`ベースの生存確認が無力化される」というものだった
(元ADR §3.4項目2)。しかし実装時、この保護は`channel_open_session`
(A1相当、命名が紛らわしいが元ADRのA1とは別物)だけに適用され、
A1〜A3という**同じ性質の別の3箇所**には適用されなかった。

### 2.2 実害のシナリオ

タブXが`streamlocal_forward`(A1)を要求した直後にネットワークが
サイレントに死ぬと、`pooled.handle`のMutexはこの`.await`の間
握られたままになる(keepaliveで最終的に検出されるまで最大約4分、
元ADR §3.2と同じ理屈)。この間、同じ`pooled.handle`を共有する
別のタブYが新規に`try_attach_with`する(またはpool内の既存タブが
再接続を試みる)と、生存確認述語`|p| p.handle.try_lock()...
unwrap_or(true)`は`try_lock()`が失敗するため「生きている」に
フェイルオープンし、死にかけの(あるいは既に死んでいる)ハンドルを
そのまま渡してしまう——これはissue #120がまさに修正しようとした
症状そのものが、A1〜A3を経由する別の窓から再現する形になる。

### 2.3 対応方針の検討

**案A-1: A1〜A3にもA(channel_open_session)と同じ`tokio::time::timeout`を適用する**

- 長所: 元ADR §3.4項目2の意図(「あらゆる保持者に適用しないと
  ロック競合の窓が塞がらない」)を素直に完成させる。実装は小さい
  (3箇所に`tokio::time::timeout(RUN_EXEC_TIMEOUT, ...)`を足すだけ)。
- 短所: 下記§3で述べる通り、`channel_open_session`のケース(§3.2の
  finding、後述)と同種の「タイムアウトでキャンセルした後、サーバー
  からの遅延応答がクライアント内部にゴミを残す」リスクが、A1・A2にも
  同様に存在しうる(§2.4参照)。何も考えずにタイムアウトを足すだけでは、
  A1〜A3固有のリーク経路を新たに生む可能性がある。
- A3(`cancel_streamlocal_forward`)は既にbest-effort
  (失敗を`debug!`ログするだけで処理を続ける、`ssh_handler.rs:1112-1113`)
  なので、単にタイムアウトを足すだけなら副作用は小さい
  (「掃除に失敗した」ことがログに残るだけ)。

**案A-2: これらのフォワード系操作(A1・A2)は生存確認の対象から除外し、
専用の軽量タイムアウトのみ与える(pool側のtombstoneには繋げない)**

- Epic M(tmux迂回)・リモートポートフォワードはどちらも
  opt-inかつopportunisticな機能(`CLAUDE.md`「実験的・opt-inの機能は
  既定OFFとし、使えない環境では黙ってフォールバックする」の精神に近い)。
  これらの操作がタイムアウトしても、必ずしも「SSH接続そのものが死んだ」
  ことを意味しない(サーバー側がUDSフォワードや`tcpip-forward`要求を
  単に処理し損ねただけ、というケースもありうる)。したがって、
  タイムアウトした場合に元ADRのtombstone機構(`mark_dead_if_same`)を
  呼ぶべきかどうかは、A(channel_open_session)ほど自明ではない
  ——channel_open_session失敗はSSHセッションの基本機能が使えないことを
  意味するが、A1・A2の失敗はopt-in機能が使えないだけで、SSHセッション
  自体は継続して問題ない可能性がある。

### 2.4 `channel_open_session`のタイムアウト自体が抱える副作用(コードレビュー指摘、参考情報)

`channel_open_session`(元ADRの実装、`ssh_handler.rs:802`)を
`tokio::time::timeout`でキャンセルした場合、russh内部
(`russh-0.48.2/src/client/mod.rs:468-477`)は`unbounded_channel()`の
受信側を介してサーバーからの`CHANNEL_OPEN_CONFIRMATION`を待つ設計に
なっている。タイムアウトで我々の future がdropされると、この受信側も
dropされる。その後(タイムアウト後)にサーバーから確認応答が届くと、
`client_read_authenticated`(`russh-0.48.2/src/client/encrypted.rs:402-431`)
は`channel.send(ChannelMsg::Open{...}).unwrap_or(())`という形で
送信結果を握りつぶす(`:425-429`)——受信側が無くなっていても
エラーは無視され、`self.channels`(`local_id`をキーとする内部マップ)の
該当エントリは削除されない。つまり:

- クライアント内部の`channels`マップに、二度と使われることのない
  エントリが残り続ける(プロセス生存中、境界は「試みた
  `channel_open_session`のタイムアウト回数」で、無限ではないが
  無視できる量でもない)。
- サーバー側から見れば、このチャネルは正常に開かれたことになっている
  (`CHANNEL_OPEN_CONFIRMATION`を送った時点でサーバーはチャネルを
  確立済みとみなす)。クライアント側はこのチャネルを二度と
  `CHANNEL_CLOSE`しないため、sshdの`MaxSessions`のようなper-connection
  チャネル数上限を、タイムアウトのたびに1つずつ静かに消費し続ける。

これは元ADRが導入したタイムアウト機構自体の、新たに生まれた副作用であり、
`channel_open_session`固有の問題ではなく「オペレーション途中で
`tokio::time::timeout`によりfutureをdropする」という設計パターン全般が
持つ既知のリスクパターンである。russh側の公開APIには、
`Channel<Msg>`オブジェクトを受け取れなかった場合に「まだ生きているかも
しれない`local_id`」を明示的に閉じる手段が無い(`channel_open_session`は
成功時のみ`Channel<Msg>`を返し、失敗/タイムアウト時は`local_id`自体が
呼び出し元に渡らない)。

### 2.5 このギャップへの推奨対応

1. A3(`cancel_streamlocal_forward`)には案A-1(単純なタイムアウト追加)を
   適用する——既にbest-effort扱いなので副作用が小さい。
2. A1・A2(`streamlocal_forward`・`tcpip_forward`)については、
   タイムアウト自体は追加する(ハングしたまま`pooled.handle`を握り
   続けることの方が実害が大きいため)が、タイムアウトを
   `mark_dead_if_same`には**接続しない**(案A-2)——これらはopt-in
   opportunistic機能であり、失敗/タイムアウトがSSHセッション全体の
   死を意味するとは限らないため、元ADRのtombstone判断ロジックとは
   独立に扱う。
3. §2.4の「タイムアウトによるchannel/確認応答のリーク」は、
   現時点では**受容する**(reject)——発生条件が「既に`RUN_EXEC_TIMEOUT`
   (10秒)を超えてハングするほど不健全な接続」に限られ、かつそのような
   接続はいずれにせよ`is_closed()`が最終的に`true`になって
   tombstone化される経路に乗る(元ADR §3.2)ため、実害は「その
   数秒〜数十秒の間、sshdのチャネル数上限を1つ余分に消費する」程度に
   留まる。根本的に直すには russh 側にlocal_idの明示的キャンセルAPIを
   追加する必要があり、この修正コストに見合う実害の大きさではないと
   判断する(要検証、§4参照)。

## 3. ギャップB: `mark_dead_if_same`が「共有ハンドルへの単一タブの失敗」で
   他のタブの分まで巻き込んでtombstone化する

### 3.1 現状

`isekai_pipe_quic_transport.rs`(`:159-197`)・`lib.rs`(`run_russh_transport`)
はどちらも、`run_ssh_channel_loop`の戻り値が
`FirstChannelOpen::Failed | FirstChannelOpen::TimedOut`の場合、
無条件で`pool::mark_dead_if_same(&POOL, &key, &pooled)`を呼ぶ
(`isekai_pipe_quic_transport.rs:140,176`・`lib.rs:1711`)。

この判断は、呼び出し元が「このタブが確立を担当した新規接続
(`AttachOutcome::Establisher`)なのか、既存の共有ハンドルに便乗した
再利用(`AttachOutcome::Ready`または`AttachOutcome::Waiter`)なのか」を
一切区別できない設計になっている。`acquire_pooled_handle`
(`isekai_pipe_quic_transport.rs:707-762`)の`AcquireOutcome::Attached
(Arc<PooledSshHandle>, Option<Key>)`は、`Ready`・`Waiter`→`Ok`・
`Establisher`→`Ok`の3経路を全て同じ`Attached`にまとめてしまうため、
`mark_dead_if_same`を呼ぶ時点でこの区別の情報は既に失われている。

### 3.2 実害のシナリオ

sshdの多くはデフォルトで`MaxSessions 10`のような、単一SSH接続あたりの
同時チャネル数上限を持つ。プールされた同一ハンドルに対して既に9個の
タブがチャネルを開いている状態で、10個目のタブが
`channel_open_session()`を試み、この上限超過によりサーバーから
`SSH_OPEN_RESOURCE_SHORTAGE`相当の拒否を受けたとする。これは
**そのタブ1つの試行が失敗しただけ**であり、他の9つのタブが使っている
チャネルも、`pooled.handle`が表す下層のSSH接続自体も、何も壊れていない
——にもかかわらず、現状の実装ではこの1回の拒否だけで
`mark_dead_if_same`が呼ばれ、プールエントリ全体がtombstone化される。

tombstone化そのものは既存の9タブの動作中チャネルには影響しない
(`EntryState::Dead`への変更はプールのマップエントリだけを書き換え、
`Arc<PooledSshHandle>`実体やそれを握っている各タブのチャネルには
一切触れない)。実害は「**次に**新規タブを開こうとしたユーザーが、
まだ健全に動いている接続を再利用できず、フルの再ブートストラップ+
QUICハンドシェイク+認証をゼロからやり直すことになる」という、
プーリングの目的そのものを無に帰す非効率——元ADRが解決した
「死んだハンドルを使い続ける」問題の裏返しである「生きているハンドルを
死んだものとして扱う」問題であり、`always-connects.md`が守ろうとする
「常に接続できる」という結果自体は損なわれないが、パフォーマンス上の
後退であり、かつ元ADRのどの節にも明示的に検討・許容された記述が無い
(rev1〜rev4の4ラウンドのopus-adversarial-consultでも、この論点は
一度も俎上に載らなかった)。

### 3.3 対応方針の検討

**案B-1: `AcquireOutcome::Attached`にEstablisher/Ready/Waiterの
区別を持たせ、`Ready`/`Waiter`経由(=既に他のタブが使っている
可能性がある共有ハンドル)での初回失敗はtombstone化しない**

```rust
enum AcquireOutcome {
    Attached(Arc<PooledSshHandle>, Option<Key>, AttachOrigin),
    ...
}
enum AttachOrigin {
    FreshlyEstablished,  // Establisher経路。このタブが初めてこのハンドルを使う
    Shared,              // Ready/Waiter経路。他のタブも使っている可能性がある
}
```

呼び出し元は`AttachOrigin::FreshlyEstablished`のときだけ
`mark_dead_if_same`を呼ぶ。`Shared`のときは(タイムアウト/失敗した
という`TransportEvent::Disconnected`はこれまで通り送るが)プール
エントリはtombstone化せず、次にこのプールにアタッチしようとする
タブに(生存確認は`is_closed()`ベースなので)まだ生きている限り
再利用させる。

- 長所: プーリングの意図(短時間での再利用によるコスト削減)を、
  1回の局所的な失敗で無駄にしない。
- 短所: 「1タブの失敗が実は接続全体の死を意味していた」ケース
  (`is_closed()`ではまだ検出できないタイミングでの本当の死)を
  見逃す可能性がある——ただしこれは元ADRの§3.2が既に認めている
  「is_closed()の検出ラグ」の範囲内の話であり、次にこの死んだ
  ハンドルへ誰かがアタッチを試みた時点で(その試行の`channel_open_session`
  自体が失敗し)結局tombstone化される。「気付くのが1回遅れる」だけで、
  「永久に気付かない」わけではない。

**案B-2: refcountを見て、tombstone化するかどうかを判断する
(このタブを含むアタッチ数が1、つまり自分しか使っていない場合のみ
tombstone化)**

- `PoolEntry`の`refcount`を`mark_dead_if_same`から覗ける形にし、
  「このタブがrelease済みになった後もrefcountが0より大きい
  (=まだ他のタブが使っている)」場合はtombstone化を見送る、
  という判断も理論上ありうる。
- 却下理由(暫定): refcountは「今アタッチ中のタブの数」であって
  「その中で何個が正常に動いているか」は表さない。他の全タブも
  実は同じ理由で失敗しかけている(例: 接続自体が本当に死んでいる)
  ケースでは、この判定はtombstone化を不当に遅らせる。案B-1の
  「Establisher/Sharedの区別」の方が、判断に使う情報(このタブが
  最初の利用者かどうか)が安定しており、他タブの状態を覗く必要が
  無い分シンプル。

### 3.4 このギャップへの推奨対応(暫定、要レビュー)

案B-1を採用する方向で検討する。`AttachOutcome`(pool.rs)自体を
変更する必要は無く(`Establisher`/`Ready`/`Waiter`という区別は
既にpool.rs側にある)、`AcquireOutcome`側(呼び出し元の型)に
新しいフィールドを足すだけで実現できる小さな変更に見える
(要実装時の再検証)。

## 4. 未決事項

- §2.5の「§2.4のリークは受容する」という判断は暫定。russh
  0.48.2に、確立に失敗した`local_id`を明示的に閉じるAPIが
  本当に無いか(`Handle`の他のpublicメソッドを再確認する)を
  実装着手前に確認する。
- 案B-1の`AttachOrigin`导入が、`isekai_pipe_quic_transport.rs`と
  `lib.rs`の両方(QUIC/プレーンSSH両プール)で同じ形で適用できるか
  (`Waiter`→`Ok`経路の扱いも含め)実装時に再検証する。
- 本ADRが対象としない8件(`/code-review`の残り、orchestrator.rsの
  `pending_wake`関連3件・`resume_client.rs`のnetwork-wake関連2件・
  `rebind_driver.rs`/`orchestrator.rs`の同期ファイルI/O2件・
  `is_alive`のフェイルオープン1件)は、いずれも本PRのpool.rs修正
  そのものではなくこのブランチの先行コミット(reconnect-timeout/
  resume-budget関連の実装)に既にあったコードへの指摘であり、
  スコープが異なるため本ADRには含めない。別途対応要否を判断する。

## 5. 次のステップ

方針が固まり次第、`ADR_ANDROID_POOL_STALE_HANDLE.md`と同様に
opus-adversarial-consultでのレビューを経てから実装するかどうかを
判断する。
