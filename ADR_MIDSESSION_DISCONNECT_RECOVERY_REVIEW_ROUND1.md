# Review Round 1: `ADR_MIDSESSION_DISCONNECT_RECOVERY.md`（opus 2体、独立並行レビュー）

opus-critic-a・opus-critic-bの2エージェントに、ADR draft(round 0)を独立に
（互いの指摘を見せずに）実コード裏取り込みでレビューさせた。両者が
**独立に同じblocking項目に収束した**（B1: Windows経路が復旧判断に到達しない、
非idempotentコマンドの再実行、`native::mux::run_with_reconnect`という
既存資産の見落とし）ことは、指摘の信頼度が高いことの強い傍証。
opus-critic-bには追加で「Windows mux機構の詳細」を深掘りさせ、round 0の
B3を大幅に更新する発見（owner側でのtransport死のExit(255)への「洗浄」）を得た。

以下、両者の指摘を統合・重複排除して記録する。個々の発言者の帰属は
末尾の出典一覧を参照。

---

## BLOCKING

### B1. Windowsではmid-session切断が`Err`にならないため、復旧判断に到達しない

`drive_connect_recovery`（`native/connect.rs:346-348`）は`Ok(exit_code)`
経路を即returnし、`claim_outcome`は`Err`のときしか呼ばない。ところが
mid-session切断は次の経路で**常に`Ok(255)`**になる:

- 単一プロセス直結: `run_shell_io_loop_inner`が`Some(ChannelMsg::Close) | None`
  で`break`（`native/connect.rs:1195`）→`Ok(NO_EXIT_STATUS_RECEIVED /* 255 */)`
  （`:1110`,`:1204`）
- mux client経由（Windowsの既定経路、後述B3参照）: owner側が`Frame::Exit(255)`
  を送ってから退場するため、client側は`ClientOutcome::Exited(255)`
  → `DispatchOutcome::Done(255)`となり、`OwnerLost`分岐（既存の復旧機構が
  ある側）には一度も到達しない

round 0のADRが述べた「両OSで理論上は既に1回だけ自動リカバリが走っている」
はUnix限定の事実であり、Windowsでは**現状ゼロ回**。`claim_outcome`されない
`ConnectOutcome`ファイルはruntime_dirに溜まり続ける。

### B2. `err.chain().any(downcast_ref)`は機能しない、既存の裸`downcast_ref`パターンで十分

`anyhow::Error::downcast_ref`は内部で`ContextError`のchainを自動的に
辿るため、`.context(...)`で包んだ後でも既存の`StaleTrustSignal`検出
（`connect.rs:486`、裸の`err.downcast_ref::<T>()`）はそのまま機能している。
round 0の§2.2が抱いていた懸念（chainを手動で辿る必要がある）は誤った
前提に基づく。新しいマーカー型も同じ裸の形で検出できる。

### B3. Windows mux機構の深掘り: owner側がtransport死を`Frame::Exit(255)`に
    「洗浄」しており、既存の`run_with_reconnect`が機構ごと無力化されている

Windowsの`isekai-ssh <host>`は**既定で常にmuxクライアント**として動く
（`main.rs:225-229`、opt-inフラグなし）。single-process直結に落ちるのは
holder起動失敗等の例外ケースのみ。

トレース:
1. holder配下の`isekai-pipe connect`が死ぬ（STUNならQUIC idle timeout 15秒）
2. owner側`owner.rs:585-588`: `channel.wait()`が`Close`/`None`→break
3. **`owner.rs:703-708`**: `let final_code = exit_code.unwrap_or(255);
   write_frame(writer, &Frame::Exit(final_code))` — transport死を
   「リモートシェルが255で正常終了した」体裁のExitフレームに変換して
   clientへ送ってしまう
4. client側`client.rs:385-388`: `Frame::Exit(255)`→`ClientOutcome::Exited(255)`
   →`DispatchOutcome::Done(255)`→`mod.rs:308`で即return

`OwnerLost`（`client.rs:397-400`）が発火するのは「ownerがExitフレームを
書き切る前に異常死した」場合（クラッシュ/kill/pipe reset）だけであり、
ネットワーク切断はowner側が行儀よくExitフレームを送ってから約1秒後
（`HANDLE_HEALTH_POLL_INTERVAL`、`owner.rs:81`）に自分で退場するため、
この経路を通らない。

**結論**: `native::mux::run_with_reconnect`（`mod.rs:248-341`。
`RECONNECT_BUDGET=24h`・ジッター付きバックオフ・`RECONNECT_STABLE_THRESHOLD`
・Ctrl-C中断・`has_remote_command`ガードまで揃った既存の復旧機構）は
**バイパスされているのではなく、判定材料がclientに届く前にowner側で
握りつぶされている**。

**推奨される最小修正（Windows）**: プロトコルにフィールドを追加せず、
`relay_loop`のbreak理由を「リモートが正しくchannelを閉じた」と
「transportが死んだ」で区別し、後者では`Frame::Exit`を送らずに
接続を落とす。すると`client.rs`の`OwnerLost`が設計通り発火し、
`run_with_reconnect`の全機能（24hバジェット、バックオフ、Ctrl-C、
非idempotentコマンドガード）がそのまま働く。プロトコル変更が要らない
ことは、常駐するholderと新しいclientのバージョン不一致問題を避けられる
という副次的な利点も持つ。**この発見により、Windowsには新しいリトライ
ループを別途実装する必要が無いかもしれない**——ADRのTier 1スコープを
プラットフォームごとに再検討する必要がある。

### B4. `recover_via_cross_family_fallback`がmid-session失敗を握りつぶし、
    かつstdinを二重消費する

`ConnectRoute::StunWithFallback`（`connect.rs:622-634`）では、
`run_stun_p2p_with_fallback`のエラー（mid-sessionの`relay_stdio`失敗を
含む）がそのまま`recover_via_cross_family_fallback`に渡され、**同一
プロセス内で**relay経由の`run_relay_resumable`をフルに実行する。
`relay_stdio`がspawnした生存側ポンプタスク（`resume_loop.rs:68,83`）は
abortされないため、新旧2つのタスクが`tokio::io::stdin()`を奪い合う。
さらにフォールバックが**成功**すると`run_connect`は`Ok(())`を返し、
`ConnectOutcome`が一切書かれない（`connect.rs:450-457`）——本ADRが
主眼とする「STUN primary + relay fallback」構成で、新設計が完全に
無反応になる。**修正**: `primary_err`がmid-sessionマーカーを持つ場合は
`recover_via_cross_family_fallback`へ入る前に即bailする。

### B5. 非idempotentなリモートコマンドの黙った再実行

`isekai-ssh host -- ./deploy.sh`やscp/rsync/gitのような一括コマンド
実行が、実行途中の切断後に頭から再実行される。**この正確に同じバグ
クラスは、このリポジトリで一度adversarial reviewにより発見・修正済み**:
`native/mux/mod.rs:311-330`はコメント付きで
`prepared.plan().remote_command().is_some()`のとき`OwnerLost`への
auto-retryを拒否している（"rerunning it could repeat a non-idempotent
action ... found by adversarial review, 2026-08"）。新しいリトライにも
`plan.remote_command().is_none()`と同等のガードが必須。

### B6. 新シグナルが依存する`ConnectOutcome`ファイル書き込みが、
    sshプロセスの終了とレースする

`give_up`（`resume_loop.rs:812-829`）は`stdout.shutdown()`を**先に**
呼んでから`Err`を返す。`ConnectOutcome`が書かれるのはずっと後、
`connect_command`（`connect.rs:455`）の中。ssh(1)は`stdout.shutdown()`
によるEOFで即終了し、wrapperの`claim_connect_outcome`（`wrapper.rs:574`）
は`child.wait()`直後に呼ばれるため、ファイルがまだ存在しない可能性が
ある→`NoRecoverableSignal`として静かに失敗する。レース窓には
`notify_os`呼び出し・エラーチェーンの`eprintln!`整形・
`write_json_atomically`のtmp+renameが挟まり、無視できない大きさ。
**この問題は既に一度Windows側で踏まれ、1秒のgraceという対症療法で
凌がれている**（`native/connect.rs:452-462`のコメント: "dropping
`child` the instant the SSH layer errors here can cut the child off
mid-write ... turning a recoverable ... failure into an unrecoverable
one"）。Unix側には同等の保護が無い。**修正**: `give_up`から
`stdout.shutdown()`を外す（またはoutcome書き込みを前倒しする）。
「outcomeを書く」が「stdoutを閉じる」より必ず先、という不変条件を
コメントで明記する。

### B7. Tier 1/Tier 2のコスト見積もりが誤っている——STUN P2Pの
    バイトレベルresumeは、実は狭い変更で済む可能性が高い

`isekai-transport/src/stun_p2p.rs:129-133`のdocコメント:

> Resume support for a connection established this way still goes
> through the plain `crate::resume::reconnect_and_resume` against a
> synthesized `RelayTarget{helper_addr: target.peer_addr, ..}` — see
> that Android transport's own module docs for why a bare redial (no
> re-STUN/re-punch) is this mode's accepted resume-capability ceiling.

**Androidの`isekai-terminal-core`は、STUN P2Pで確立した接続に対して
既にこの機構でresumeを行っている。** CLI側（`isekai-pipe`）で欠けて
いるのは狭い範囲: (1) `connect_stun_p2p_with_round`が`resume_grace`を
`0`にハードコードし`_conn`を捨てている（`stun_p2p.rs:290-292`）、
(2) `run_stun_p2p_with_fallback`（`resume_loop.rs:247-252`）が
`run_resume_loop`ではなく`relay_stdio`を呼んでいる。round 0のADR
§3.1が想定した「HELLO/proof/ACK・control stream・replay bufferを
P2P間に再実装する規模」は実コストではなく、真の resume は
Tier 1（フルre-dial）と同程度のコストで、かつUXは厳密に上（scrollback/
未確認バイトの継続）になる可能性がある。

**ただし前提条件の補正**: フルre-dial・真resumeのどちらも、効くのは
「サーバー側が安定した/公開アドレスで待ち受けており、クライアント側
からのpunchだけで足りる」構成に限られる。`our_observed_addr`は
相手に帯域外で伝わらない（module docsが明記）ため、クライアント・
サーバー双方のアドレスが同時に変わる（対称NAT越しの相互punchが
要る）構成では、フルre-dialも真resumeも効かない——この場合のみ
本当にTier 2（再ランデブー）が必要になる。

---

## SIGNIFICANT

### S1. `isekai-pipe connect`孫プロセスの孤児化

`run_ssh_once`の`child.wait()`（`wrapper.rs:855`）はssh(1)のみを待つ。
`wrapper.rs:769-772`のdocコメント「`.status()`は`ProxyCommand`孫プロセス
を含むプロセスツリー全体の終了まで блок する」は**事実に反する**。
sshが終了しても孫（`isekai-pipe connect`）は生き残り得る。resume window
既定値`DEFAULT_RESUME_GRACE_SECS`=864,000秒=**10日**（round 0のADRは
誤って「10〜15分」と記載していた）の間、孤児がresumeを試み続け、
サーバー側のセッション/fencing slotを掴んだままになりうる。次の
リトライ試行が新セッションをダイヤルすると`BUSY_OTHER_SESSION`に
当たるが、そのバックオフ窓は180秒（`BUSY_OTHER_SESSION_RETRY_WINDOW`、
`resume_loop.rs:158`）しかなく、10日オーダーの孤児には無力。
**新しいリトライループは、次の試行を始める前に前回試行の孫プロセス
（またはそのプロセスグループ）を確実に終了させる必要がある。**

### S2. STUN P2P経路は`retry_while_busy_other_session`の対象外、かつ
    サーバー側セッションスロットを消費する

`retry_while_busy_other_session`は`run_relay_resumable`/
`_with_fallback`（`resume_loop.rs:210,233`）にしか適用されておらず、
`run_stun_p2p_with_fallback`（`:247-252`）にはかかっていない。しかし
STUN P2Pも`random_session_id()`を発行しATTACH HELLO/proof/ACKを
フルに実行する（`isekai-transport/src/stun_p2p.rs:112,215,292`）ため、
relayと同じセッション機構に載っている。放棄されたセッションは
resume_grace(10日)の間parkedのまま残り、`admit_new_session`
（`isekai-pipe/src/engine/mod.rs:1066`以降）は`--max-sessions`
（既定16）到達時に最古のparkedセッションを立ち退かせるか、
無ければ`BusyOtherSession`で拒否する。具体的な害: (i) 1ホストへの
リトライ連打が他タブ/他ホストのparkedセッションを巻き添えで立ち退か
せる、(ii) 拒否時、STUN経路にはBUSYバックオフが無いため
ハンドシェイク前失敗=`Unreachable`として表面化し、フル再展開
（`RebootstrapAndRetry`）に化ける——「軽量リトライ」という設計意図の
反対の結果になる。なお`release_slot_for`は正しく呼ばれておりスロット
の**永久リーク**は起きない（Epic N-5で構造的に解消済み）。
**対応**: mid-sessionリトライの試行回数を`--max-sessions`既定16より
明確に手前（3〜5回）で打ち切り、以降はクラス遷移（`Unreachable`への
自然な降格）によるエスカレーションに委ねる。STUN connectも
`retry_while_busy_other_session`で包む。

### S3. Unix側ループの反復ごとのリソースリーク（ctl-socket forward）

`apply_ctl_socket_forward`→`spawn_ctl_listener`（`wrapper.rs:743-751`、
`ctl_forward.rs:223-233`）は毎回新しいUNIXソケットをbindしlistener
タスクをspawnするが、JoinHandleの回収もunlinkも無い。長寿命の
リトライループでは反復ごとにソケット1つ+タスク1つが漏れる。
反復ごとの明示的teardownが必要。

### S4. `ConnectOutcomeClass`への未知タグ追加は、schema version bump
    ではなく deserialize耐性で対処すべき

round 0のADRはサーバー側sha256一致チェックを根拠に「bump不要」と
結論したが、その根拠は的外れ（無関係なのはサーバー側ヘルパーの
話）。実際に問題になるのは、書き手（ローカルの`isekai-pipe`、
`--isekai-pipe-path`で差し替え可能）と読み手（ローカルの`isekai-ssh`）
が異なるビルドになりうること。`#[serde(tag = "class")]`では未知タグは
deserialize失敗=`Err`になり、`wrapper.rs:618-619`と`:574-575`の`?`が
それをinvocation全体のハードエラーに変換する——現状より悪い失敗
モード。**結論は同じ(bump不要)だが理由が違う**。対応:
`#[serde(other)] Unknown`variantを追加し、`claim_connect_outcome`の
deserialize失敗を`?`で伝播させず`Ok(None)`相当に握りつぶす。

---

## MINOR

- **M1.** §5のE2E案（`isekai-pipe connect`子を`SIGKILL`）は
  `ConnectOutcome`が一切書かれないケースを検証してしまう
  （`PanicOutcomeGuard`はunwindのみカバーしシグナルはカバーしない）。
  フォールト注入可能なソケットファクトリでストリームエラーを注入する
  か、サーバー側を落として模擬すること。
- **M2.** ライブ再接続表示は`resume_loop.rs`と同じ`stderr.is_terminal()`
  ゲートを入れないと、`--isekai-log-file`実行時にログが`\r`で汚れる。
- **M3.** `MID_SESSION_RETRY_WINDOW`による段階的降格分岐はおそらく
  dead code——2回目の反復はハンドシェイク前で失敗しどのみち
  `Unreachable`に分類され`RebootstrapAndRetry`が選ばれる。クラス遷移
  そのものがエスカレーション機構なので、window分岐と
  `RetryConnectThenRebootstrap`という命名の後半は削ってよい。
- **M4.** `relay_stdio`の片肺待ち（生存側タスクをabortしない、
  `resume_loop.rs:100-113`）は現スコープでは実害なしだが、Tier 2着手時
  には関係してくる。

---

## §7 Open Questionsへの回答（統合版）

1. **有限。既存の`RECONNECT_BUDGET`（24h）+`RECONNECT_STABLE_THRESHOLD`
   をそのまま再利用する**（`native::mux::run_with_reconnect`が既に
   確立している設計判断）。STUN P2P固有のBUSY_OTHER_SESSION対策
   （S2）として、試行回数はさらに3〜5回程度で頭打ちにする。
2. **既存の`--isekai-no-bootstrap`をそのまま「再展開のみ禁止」と
   解釈してよい。新フラグは不要**（`~.`とCtrl-Cが既に脱出口）。
   `--isekai-explain`とヘルプに明記する。
3. **schema version bumpは不要。ただし`#[serde(other)] Unknown`と
   deserialize失敗の非致命化が必須**（S4）。
4. **`isekai-pipe/src/engine/`のサーバー側コードは読むべきで、読むと
   設計が変わる**（S2）。fencing slotの永久リークは既に解消済みだが、
   parkedスロット枯渇・他タブ巻き添え立ち退きという別の害が残る。
5. **"Future Work"に1行書くだけで十分、別ADRを今すぐ起票する必要は
   ない。ただしTier 1/Tier 2のコスト見積もりを再検証すること**（B7）。
   STUN P2Pの残存ギャップ（scrollback/未確認バイトの非継続）は
   Consequences・`ISEKAI_PIPE_DESIGN.md`のEpic R要約の両方に明記する。
6. B1・B3(mux詳解)・B4・B5・B6・S1が該当。

---

## PR分割案（opus-critic-b提案、採用）

- **PR1（バグ修正のみ、リトライループなし）**: B6（give_upの順序）・
  S4（schema耐性）・B1のOk経路でのoutcome claim・B4
  （cross-family-fallbackのbail-out）。既存の`RebootstrapAndRetry`の
  信頼性を底上げするだけの、独立して価値のある修正群。
- **PR2（リトライループ本体）**: Windowsは`owner.rs:707`の
  Exit洗浄抑止が中心（B3）。Unix/native単体直結には
  `run_with_reconnect`と同形の新ループ（B5ガード・S1孫プロセス
  クリーンアップ込み）。
- **PR3（STUN P2Pの真のresume、Tier 2）**: B7の再見積もりを踏まえ、
  PR2よりも早期に着手する価値があるかを判断する。

---

## 出典（帰属）

- B1・B4・B5・S1(孫プロセス)・S2・S3・S4: opus-critic-a・opus-critic-b
  独立収束（両者が同じ結論に到達）
- B2: opus-critic-a
- B3(mux詳解): opus-critic-bへの追加深掘り依頼への回答
- B6・B7: opus-critic-b
- M1〜M4: opus-critic-a（M2はopus-critic-bも独立指摘）
- PR分割案: opus-critic-b
