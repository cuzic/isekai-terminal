# ADR: STUN P2Pの真の再ランデブー時にもresume連続性を保てないか

- **Status**: **Approved（rev4、2026-09-13）**。2026-09-13に
  `opus-adversarial-consult`round 1（`ADR_STUN_REESTABLISH_CONTINUITY_REVIEW.md`）・
  round 2（`ADR_STUN_REESTABLISH_CONTINUITY_REVIEW_ROUND2.md`）・
  round 3（`ADR_STUN_REESTABLISH_CONTINUITY_REVIEW_ROUND3.md`）の3ラウンドの
  批判的レビューを経て全面改訂し、round 3のMUST-FIX 4件・SHOULD-FIX 5件を
  rev4に反映した上で同レビュアーによる最終照合を受け「Approvedで確定してよい」
  との結論を得た。**残る未確認事項はR3-C5（§4.2が`always-connects.md`に
  抵触しないことの確認、`wrapper.rs`の再デプロイエスカレーション経路の実在
  確認）のみ——ADRレビュー事項ではなく実装着手時のコード確認チェックリスト
  項目として扱う（§4.2参照）**。
- **対象**: `rust-core/isekai-pipe`の`connect.rs`（cross-family targetの配線・
  fallback判定）・`resume_loop.rs`（切替トリガー・有界リトライ・パラメータ束の
  切替）。**`rust-core/isekai-pipe/src/engine/`（サーバー側）は変更不要**——
  サーバーは既に`SessionId`のみをキーに任意アドレスからのRESUMEを受理する設計
  になっている（`engine/mod.rs:962`）。**入力キューの置き場所は本ADR§7.1で
  決定するが、実装自体は`ADR_INPUT_RESUME_SYMMETRY.md`の担当**（本ADRの
  変更範囲には含めない）。**Android（`rust-core/src/isekai_*_transport.rs`、
  `isekai-pipe connect`プロセスを起動しない経路）は構造上到達しないため対象外**
  ——§5参照。
- **入力**: ユーザーとの「mosh的な、切断中の入力バッファリング/画面操作継続」
  検討セッション（2026-09-07）。`ADR_MIDSESSION_DISCONNECT_RECOVERY.md`
  （Epic R、2026-09-02完了）で明示的にスコープ外とされた残課題の掘り下げ
- **拘束される既存ルール**: `.claude/rules/always-connects.md`、
  `ADR_MIDSESSION_DISCONNECT_RECOVERY.md`（本ADRの前提となる既存設計。特に
  §2.4の経路分岐、§2.2.2 S1の「実測してから機構の要否を決める」前例）、
  `ADR_INPUT_RESUME_SYMMETRY.md:36-37`「isekai-pipeは薄いトランスポート
  中継のままにしたい」

---

## 1. 背景

`ADR_MIDSESSION_DISCONNECT_RECOVERY.md`（Epic R、PR1〜3）により、
確立後のネットワーク切断からの自動復旧が実装済み:

- **relay経路**: 完全バイトレベルresume（`OutputBuffer`ベース、既定
  resume window 10日）。
- **STUN P2P経路**: サーバー側アドレスが安定していれば bare redial
  （再STUN無し）で復旧できるが、これはあくまで「サーバー側アドレスが
  安定していること」という前提の範囲内の話。この経路が諦める境界は
  **`STUN_RESUME_GIVE_UP_WINDOW = 120秒`**
  （`rust-core/isekai-pipe/src/resume_loop.rs:68`）——本ADRの対象ケースを
  実際に支配する数値はこちら（relayの10日ではない）。

明示的にスコープ外とされているのは、**クライアント・サーバー双方の
アドレスが同時に変わる、対称NAT越しの真の再ランデブーが必要な
ケース**（`ADR_MIDSESSION_DISCONNECT_RECOVERY.md` §2.3.6、
1431〜1439行）。この場合フルSTUN再確立が走り、**新セッション扱いと
なりresume連続性（scrollback/未確認バイトの継続）が失われる**
（同ADR §4.2、1539〜1543行）。

なお「STUNのresume試行を諦めて新セッションを始める」境界
（give-up boundary）自体については、Task 3.4（同ADR §2.3.4、
1411〜1416行）で「一度だけログ出力する」対応が**既に実装済み**。
本ADRが扱うのはそれとは別の、**そもそも真の再ランデブーのケースで
連続性そのものを保てないか**という、同ADR §7 Open Question 2
（1620〜1629行）で「round 3で検証してほしい残課題」として明示的に
残されている論点。

### 1.1 round 1レビューで訂正された前提

round 1起草時点では「PR #116（`0df74233`）で追加された
`log_rendezvous_outcome`計装により、真の再ランデブーの検知は
実装レベルで既に土台がある」と考えていたが、**これは誤りだった**。

`log_rendezvous_outcome`の呼び出し元4箇所
（`isekai-transport/src/resume.rs:408-414 / 486-492 / 515-521 / 592`）は
すべて`connect_via_relay_resumable_with_fallback`内部にあり、**relay
逐次フォールバック専用**（identity が`resume.rs:429-434`で
`kind: "relay"`にハードコード）。STUN P2P経路
（`resume_loop.rs:335 run_stun_p2p_resumable`、`resume_loop.rs:383
run_stun_p2p_with_fallback`）はこの計装を一度も呼んでいない。

さらに`"abandoned"`クラスは「このラウンドで一度もattachに成功
しなかった＝接続失敗」を意味し、`"fresh-rendezvous"`はrelay-fallback
connectの冒頭で無条件に（初回接続でも）出る
（`previous_session_id`は`telemetry.rs:212-221`が明示する通り常に
`None`）。**したがって現状、真の再ランデブーのケースは検知すら
されていない**（「新セッション扱いになるだけで検知は可能」ではない）。

## 2. 方針

**「発生頻度を下げる」と「連続性を保つ」は別々の施策として比較する
のではなく、1本の変更（cross-family resume-preserving fallback）に
統合する。** 詳細は§3。「通知の改善」（検知計装）はこの変更の
副産物として同じPRで入れる（§3.2タスク5）。

「フルSTUN再確立後にOutputBufferそのものを引き継ぐ」という文字通りの
連続性維持は、非目標として明示的に閉じる（§4）。

## 3. 実装方針: cross-family resume-preserving fallback

### 3.1 何が可能で、なぜ今まで塞がれていたか

STUN P2P primaryとcross-family relay fallbackは、**同じhelperプロセス・
同じ`session_secret`**を指している:

- `isekai-ssh/src/wrapper.rs:1305-1316 select_transport`は、primaryを
  `IntentTransport::StunP2p { session_secret_b64: legacy.session_secret_b64, ... }`
  として作り、fallbackとして`profile.to_legacy_relay_transport()`を返す。
- `isekai-pipe-core/src/profile.rs:168-177 to_legacy_relay_transport`は
  `IntentTransport::Relay { helper_addr: legacy.helper_addr,
  session_secret_b64: legacy.session_secret_b64, ... }`を作る——
  primaryとまったく同じ`session_secret_b64`。
- `isekai-pipe/src/connect.rs:862-864`で、fallbackもprimaryと同じ
  `intent.expected_server_identity.cert_sha256_hex`に対して検証される。
- サーバー側は`SessionId`のみをキーとする`HashMap`
  （`isekai-pipe/src/engine/resume.rs:22`の`SessionTable`）で管理して
  おり、`engine/mod.rs:962`の`FRAME_RESUME`分岐は**送信元アドレスによる
  絞り込みを一切していない**。RESUME認証は新接続のTLS exporter上の
  `HMAC(session_secret, exporter || session_id)`
  （`isekai-transport/src/resume.rs:812`）。

つまり**STUN P2Pで確立したセッションは、同じsession_id・同じoffsetsの
まま、relay（サーバー直アドレス）側へ`reconnect_and_resume`できる**。
新プロトコル不要・新規シグナリング不要・再punch不要・**`ssh(1)`プロセス
も再起動せず生存したまま**、バイトレベル連続性が完全に保たれる——
ただし、この`reconnect_and_resume`は**毎回まっさらなephemeralソケット
から`dial`する**（`isekai-transport/src/resume.rs:733`
`BindSpec::any_ipv4().with_port_range(...)`、穴あけ済みソケットの再利用
はしない）ため、STUN経路のresumeは元々「サーバー側NATマッピングが
寛容であること」に賭ける構造になっている点に注意（§3.3で敷衍）。

`isekai-pipe/src/connect.rs:835-843`には、cross-family fallbackへの
遷移を防ぐガードが既にある:

```rust
if primary_err.downcast_ref::<crate::resume_loop::MidSessionDisconnectSignal>().is_some() {
    return Err(primary_err).with_context(|| format!("isekai-pipe connect: {context_label} failed"));
}
```

このガードは**Epic R PR2 Task 2.7で追加されたもので、2つの理由**を
挙げている: (a) 別トランスポートで黙って新セッションを始めてしまう
こと、(b) `always-connects.md`が要求する復旧経路
（`isekai-ssh`側の`wrapper.rs`/`native/connect.rs`の
`RetryConnectLightweight`、`MidSessionDisconnect`をそのまま見て発火する）
を迂回してしまうこと。**本ADRのcross-family resumeが理由(a)を
解消するのは、タスク1の位置（`run_resume_loop`内部、ポンプ生存中に
RESUMEする場合）に限られる。**`connect.rs:835-843`のガードの地点で
呼ばれるのは依然`run_relay_resumable`（新規ATTACH）であり、そこでは
**理由(a)も理由(b)も成立したまま**である——resumeが失敗してこの
ガードの地点まで来たということは連続性が既に失われた後であり、
そこで新規ATTACHを呼ぶと生きている`ssh(1)`に別セッションの
ハンドシェイクバイトが流れ込む（§4.1が非目標とした破壊と同型）。
**したがって、このガード自体は残す**（§3.2タスク3）。

### 3.2 タスク

1. **切替トリガー**: cross-family resumeへの切替は、STUNの
   `STUN_RESUME_GIVE_UP_WINDOW`（120秒）を待ち切ってから行うのを
   デフォルトにしない。§3.3で述べる「救うべきケース（クライアント側
   アドレスだけが変わった場合）」ではSTUNのbare redialは最初から
   絶望的であり、120秒待つとユーザーは「反応しない端末を2分間見る」
   ことになる。切替条件は次の**いずれか**が成立した時点とする
   （論理積にしない）:
   - ネットワーク変化シグナルを受け取った（前倒しの主経路）、**または**
   - シグナルの有無にかかわらず、STUN bare redialがN回失敗した
     （Nは120秒よりはるかに手前で切り替わる値を選ぶ）。

   論理積にすると、netmonが無言の環境（プラットフォーム実装の欠落、
   `next_change()`が`None`を返して永久停止した場合——
   `wait_backoff_or_network_change`のdocが明示的に想定しているケース
   ——、あるいはOSのインターフェース変化を伴わないアドレス変化）で、
   このタスクが禁じている「120秒待ち」に静かに退行してしまう。

   **なお`experimental_network_rebind = false`のSTUN経路でも、
   ネットワーク変化シグナル自体は生きている**（無効化されるのは
   「rebindを試みること」だけで「変化を検知すること」ではない）:
   (1) `spawn_reconnect_signal`は`run_resume_loop`のループ冒頭で
   無条件に呼ばれる（`resume_loop.rs:1340-1346`）。
   (2) `experimental_network_rebind == false`のとき、
   `spawn_reconnect_signal`の`_ =>`アーム（`resume_loop.rs:556-560`）が
   ネットワーク変化をそのまま再接続シグナルとして転送する。
   (3) `resume_with_backoff_until_deadline`も`network_monitor`を
   既に引数で受け取っている（`resume_loop.rs:1084`）。
   実装者が`experimental_network_rebind = false`だけを見て
   「この経路では使えない」と誤判断しないよう、この3点を明記しておく。
2. **配線（plumbing）**: `run_resume_loop`はcross-family relay
   アドレスを知らない（STUN経路の`RelayTarget`はSTUN観測ピアアドレス
   であり、`run_stun_p2p_with_fallback`は`ConnectionIntent`自体を
   受け取らない——`resume_loop.rs:428-433`が同種の欠落を既に記録して
   いる）。`connect.rs`が持つ`intent.cross_family_fallback`
   （`isekai-pipe-core/src/lib.rs:160`）由来のrelayアドレスを、
   新しい引数（例: `cross_family_target: Option<RelayTarget>`）として
   `connect.rs`から`run_resume_loop`**だけでなく、実際に
   `reconnect_and_resume`を呼ぶ`resume_with_backoff_until_deadline`
   （`resume_loop.rs:1077`）まで**通す配線を追加する。target選択
   （STUN targetかcross-family targetか）の判断は、この関数の
   バックオフループの中に置く。
   - バリデーション（`decode_secret`・`validate_endpoint_identity`、
     `connect.rs:857-864`）は`connect.rs`側で接続開始前に済ませ、
     検証済みの`RelayTarget`を渡す。
   - `intent.local_bind_port_range`をcross-family用`RelayTarget`にも
     引き継ぐ。
   - `intent.cross_family_fallback`が`None`（`legacy_relay_transport`を
     持たないプロファイル）の場合は、cross-family resumeを単に
     スキップし従来通りgive upする。
   - **トリガー判定のためのAPI変更**: 現状、切替トリガーに必要な情報が
     取り出せない。(a) `wait_backoff_or_network_change`
     （`resume_loop.rs:816`）はどちらのbranch（バックオフタイムアウト
     かネットワーク変化か）が勝ったかを`log::info!`に出すだけで
     戻り値（`()`）に載せておらず、呼び出し元へ伝わらない——
     戻り値で返すよう変更する。(b) episodeの開始原因（ネットワーク
     変化でポンプを打ち切ったのか、ポンプ自身が失敗したのか）も、
     現状`run_resume_loop`の`select!`が生成する
     `PumpFailure::Remote(anyhow!("network change detected,
     reconnecting"))`という**エラー文字列としてのみ**エンコードされて
     いる。既存の`MidSessionDisconnectSignal`/`StaleTrustSignal`と
     同じ「型付きマーカーを`anyhow::context`で載せ`downcast_ref`で
     拾う」パターンに揃え、文字列マッチはしない。
3. **`connect.rs:835-843`のbail-outは残す（変更しない）**。
   cross-family resumeは**タスク1の位置（`run_resume_loop`内部、
   `isekai-pipe connect`のデータポンプが生きている間）でのみ**行う。
   そこで失敗した場合は、従来通り`MidSessionDisconnect`を素通しして
   wrapperの`RetryConnectLightweight`（`ssh(1)`を作り直して新しい
   セッションを始める）に委ねる。§3.1の通り、`connect.rs`層で
   `run_relay_resumable`（新規ATTACH）を呼ぶことは§4.1が非目標とした
   破壊そのものであり、`always-connects.md`が要求する復旧経路も
   迂回してしまうため、**このガードを置き換えたり削除したりしては
   いけない**。`connect.rs`側で行う変更はタスク2の配線のみ。
4. **有界リトライ**: cross-family resumeは単発試行にしない。
   既存の`UnknownSession` streak判定（`is_unknown_session_rejection`
   `resume_loop.rs:999`、`update_unknown_session_streak`
   `resume_loop.rs:1016`）は`streak >= UNKNOWN_SESSION_CONFIRM_THRESHOLD`
   を要求する設計であり、単発試行では`streak`が常に1にしかならず
   この判定が永久に発火しない（`UnknownSession`がサーバー側の3状況
   ——本当に消滅／まだparkされていない／`AttachArbiter`のlease不一致
   ——を1値に潰しているため、1回では区別できない、というのが元々の
   設計理由）。cross-family resumeは**短い有界リトライ（目安: 最大
   `UNKNOWN_SESSION_CONFIRM_THRESHOLD`回、合計10〜30秒程度）**にし、
   既存のstreakロジックがそのまま意味を持つようにする。

   **ただし回数条件だけでは足りない**: `update_unknown_session_streak`
   のgive-up条件（`resume_loop.rs:1021`）は
   `streak >= UNKNOWN_SESSION_CONFIRM_THRESHOLD`（3回）**かつ**
   `elapsed_since_disconnect >= UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`
   （切断から30秒、`resume_loop.rs:978`）の**両方**を要求する。
   タスク1の前倒し切替（切断から数秒〜十数秒で開始しうる）と組み合わ
   せると、回数条件だけ満たして時間条件を満たさず判定が永久に発火
   しない、という別経路の不発が起こる。**有界リトライ窓は、回数だけ
   でなく`disconnected_at + UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`
   （切断から30秒）に到達するまでは終わらせないこと。**
   「前倒しで切り替えること」と「切断から30秒は確定判定を出さない
   こと」は両立する（早くrelayを試し始めるが、確定を宣言するのは
   30秒後）。

   **さらに、cross-familyへ切り替えたら同じepisode内でSTUN targetへの
   試行には戻らない（一方向の切替）こと。** `update_unknown_session_streak`
   は`UnknownSession`以外のエラーでstreakを0に戻す
   （`resume_loop.rs:1017-1019`）ため、2つのtargetへ交互に試行すると
   streakが毎回リセットされ、閾値に到達しなくなる。
   `state.consecutive_unknown_session`（`resume_loop.rs:872`）を
   セッション単位で持つこと自体は正しい（`UnknownSession`はsession_id
   についての主張であってアドレスについての主張ではないため）——直す
   べきは試行順序のほう。**この「一方向の切替」は、タスク8が実測待ち
   に格下げした「preempt ping-pongラッチ」とは別の要請であり、実測を
   待たずに実装する。**
5. **検知計装**: 既存の`log_rendezvous_outcome`は relay逐次
   フォールバック専用であり、**新規に似た関数を作るのではなく
   再利用する**（`telemetry.rs:212-221`のコメント「将来そういう
   呼び出し元ができたら第2の似た関数を作らずに済むよう
   `previous_session_id`引数を残してある」の趣旨通り）。新しい
   `class`値を追加する:
   - `"cross-family-resumed"`（`previous_session_id = Some(旧)`,
     `new_session_id = Some(同じ値)` — 連続性が保たれたことを表す）
   - `"continuity-lost"`（`previous_session_id = Some(旧)`,
     `new_session_id = None` — cross-family resumeも失敗し新セッションへ
     落ちる）

   失敗時はさらに理由を「セッション消滅（`UnknownSession`、タスク4の
   streakが閾値到達）」と「relay到達不能（ネットワーク/muxエラー）」に
   **分けて記録する**——これは§3.3で述べる「`cached_relay_addr`が
   新ネットワークから到達可能」という未検証の仮定を、運用データから
   検証するために必要（分けずに一括りの「失敗」とだけ記録すると、
   仮定が崩れていても気づけない）。`telemetry.rs:200-228`のdoc
   コメント（現状relay-fallback producerのみに限定した記述）も、
   新しいproducerを含むよう更新すること——**これを直さないと、
   round 1で起きた「relay専用の計装を汎用だと誤解する」事故が
   そのまま再発する。** なお`telemetry.rs:212-221`は現状
   「`previous_session_id == new_session_id`となるケース（bare
   redial）は意図的にスコープ外」と明記しているが、
   `"cross-family-resumed"`はまさに`previous == new`のケースなので、
   doc更新時にこの1文を明示的に撤回すること。
6. **サーバー側park生死との整合**: タスク4の有界リトライにより、
   `is_unknown_session_rejection`/`update_unknown_session_streak`の
   判定がcross-family経路でも意味を持つようになる。streakが閾値に
   達したら「連続性喪失が確定」として新セッションへフォールバックする
   （タスク5の`"continuity-lost"`を記録）。
   （なお`BUSY_OTHER_SESSION`はATTACHのリジェクト理由
   `AttachRejectReason::BusyOtherSession`であり、RESUMEのリジェクト
   理由は`Auth`/`UnknownSession`/`OffsetGone`の3値のみ
   （`resume.rs:693-699` `map_reject_reason`）——RESUME経由のcross-family
   resumeでは`BusyOtherSession`はワイヤ上に存在せず、対処不要。）
7. **パラメータ束の切替**: cross-family切替は「STUN向けパラメータ束」
   から「relay向けパラメータ束」への切替であると捉えて、まとめて
   実装する（個別に列挙すると1つ落とす危険がある）:
   - `max_resume_window`をSTUN経路の`Some(STUN_RESUME_GIVE_UP_WINDOW)`
     （`resume_loop.rs:370`）から**relay相当（`None`）へ切り替える**。
     切り替えないと、cross-family resumeが成功した後も以後ずっと
     120秒でgive upし続け、relay本来の耐性（サーバー付与
     `resume_grace`、既定10日）を静かに失う。
   - `notify_on_give_up = max_resume_window.is_none()`
     （`resume_loop.rs:1087`）が連動するため、give-up時のOS通知挙動の
     変化も確認する。
   - **実装形について**: `max_resume_window`は`resume_with_backoff_
     until_deadline`（`resume_loop.rs:1077`）へ値渡しされる
     `ResumeDeadlinePolicy`の一部であり、`resume_window`/`deadline`は
     そこから`effective_resume_window`（`resume_loop.rs:681`）で
     導出済みの値。つまり`max_resume_window`フィールドだけを後から
     差し替えても`deadline`は古いまま残る。**実装形は「`ResumeDeadlinePolicy`
     をrelay向けに再計算し、その新しいpolicyでバックオフループを
     再入する」**になる（`notify_on_give_up`もこのpolicyから関数内で
     導出されるため、再計算すれば自動的に追随する）。
   - `effective_resume_grace_secs`（ATTACH_HELLOのACKでサーバーが
     返した値、`resume.rs:157-164`）を持ち回す。bare RESUME/RESUME_ACK
     では再ネゴシエーションが起きない（`resume.rs:634-648`
     `finish_via_resume`のコメントが「`0`を入れると次の切断で即give up
     する」という既知の罠として言及）。
   - `experimental_network_rebind`/warm-standbyはSTUN P2Pでは意図的に
     無効（`resume_loop.rs:363-370`）。cross-familyでrelayへ移った後、
     relay経路本来の設定（`run_relay_resumable`が受け取る値）に
     切り替えるかを実装時に決める（`run_resume_loop`は現状これらを
     引数で1回受け取るだけなので更新経路が無い点に注意）。
8. **preemptの扱い（実測してから決める）**: round 1では「ゾンビ化した
   旧STUN接続と新relay接続がping-pongする」懸念からラッチ機構を
   提案したが、round 2で再検討した結果、cross-family resumeは
   `run_resume_loop`内の単一の逐次ループ（タスク1・4）で行われ、
   同一プロセス内で2つの再接続駆動主体が同時に走る構造にはならない
   （resumeループに入った時点でクライアント側の旧STUN接続は既に
   死んでいる）。**ラッチを先んじて実装せず、まず実測して2駆動主体が
   実在するかを確認してから機構の要否を決める**
   （`ADR_MIDSESSION_DISCONNECT_RECOVERY.md` §2.2.2 S1と同じ進め方）。
   ただし**サーバー側のpreempt待ちタイムアウト**
   （`engine/mod.rs:72`）はcross-family resumeのレイテンシに直接乗る
   ため、タスク1のトリガー/タイムアウト設計ではこれを見込むこと。
9. **`isekai-ssh doctor`の表示**: 直近コミット（`172d2c25`等）で
   holderログ所在の一覧表示が追加済み。cross-family resumeが入ると
   「どの経路で今つながっているか」がセッション途中で変わりうる。
   doctor/ステータス表示が「確立時の経路」を静的に見せている場合は
   実態とずれるため要確認・必要なら追従（§6の集計手段としても
   利用できる余地がある）。

### 3.3 何を救い、何を救わないか

**救うケース（想定上の多数）: クライアント側アドレスだけが変わった
場合**（Wi-Fi↔セルラー切替など）。このときサーバー側の
restricted-cone NATは新しいクライアントアドレスからのパケットを
落とすためbare redialは失敗するが、**サーバー自身のアドレスは
変わっていないのでrelay経路は生きている**。現状のコードはこの
一番よくあるケースで`ssh(1)`ごと殺しており、体験としては最悪の
復旧の仕方をしている。

**前提（未検証、計装で確認する）**: この価値は「`cached_relay_addr`
（`profile.rs:149-152`由来、ブートストラップ時にサーバーが自己申告
した到達先アドレス）が、クライアントの*新しい*ネットワークからも
到達可能である」ことに依存する。このプロジェクトはTailscaleを前提の
1つに置いており、`cached_relay_addr`がtailnetアドレスやLANアドレスの
場合、クライアントのネットワークが変わった瞬間にrelayも同時に
到達不能になりうる（この場合「アドレスが変わった」というトリガ自体が
「relayも同時に使えなくなった」ことを意味する）。**§3.2タスク5の
計装で失敗理由を「セッション消滅」と「relay到達不能」に分けて記録し、
この仮定の成立度合いを運用データから検証する。**（なおプロファイルには
`relay_endpoints`（複数）・`link_endpoints`・`rendezvous`というより
新しいフィールドも存在する（`profile.rs:141-145`）。`legacy_relay_transport`
は移行ブリッジであり、将来これらへ移るとcross-family fallbackの
候補が複数になりうる。）

**救わないケース: クライアント・サーバー双方のアドレスが同時に変わる
真の再ランデブー**。キャッシュ済みrelayアドレスも同時に陳腐化する
ため、この手では救えない（§4.2で非目標として扱う）。

## 4. 非目標

### 4.1 フルSTUN再確立後の`OutputBuffer`引き継ぎ（文字通りの形）

**恒久的に対象外**。理由: `OutputBuffer`が保持しているのは旧`ssh(1)`
↔`sshd`のSSHトランスポート生バイト（暗号鍵・シーケンス番号・
チャネルIDに束縛されている）。フルSTUN再確立は、Unix経路の
`isekai-ssh/src/wrapper.rs:661 run_ssh_with_connect_failure_recovery`
（doc: `wrapper.rs:628-633`）であれ、Windows mux経路の
`native::mux::run_with_reconnect`であれ、**どの経路であってもSSH
セッションそのもの（`ssh(1)`プロセスまたは同等のクライアント状態）が
作り直される**点は共通する。旧バッファを作り直したセッションに
流し込むのは連続性の復元ではなく、あるコネクションの暗号文を別の
デクリプタに食わせる行為であり、即座に`Corrupted MAC on input`で
死ぬ。さらにC2H方向では、resumeが`helper_committed_offset`
（`engine/resume.rs:51`）に基づきサーバー側からも parked TCP へ再送
するため、**生きている`sshd`側のストリームまで巻き添えで壊しうる**。

（`PLAN.md:982`「対象外: SSHセッションそのものの再生成・代理応答・
端末状態同期（mosh的なstate syncはやらない）」と同じ位置づけ。）

### 4.2 クライアント・サーバー双方が同時にアドレスを変える真の再ランデブー

§3.3の通り、cross-family resumeでは救えない。これを救うには
in-processの再bootstrapシグナリングが要るが、**`isekai-pipe`は
`isekai-bootstrap`/russh に依存していない**
（`rust-core/isekai-pipe/Cargo.toml`と`rust-core/isekai-ssh/Cargo.toml`
の依存リストを比較すると、`isekai-bootstrap`・`russh`・`russh-keys`を
持つのは後者のみ）。この依存を足すことは`ADR_INPUT_RESUME_SYMMETRY.md:36-37`
が記録している「isekai-pipeは薄いトランスポート中継のままにしたい
（ユーザー判断）」に真正面から反する。**これが「両者同時変化は
スコープ外」の本当の理由**であり、§3.2タスク5の計装で
`"continuity-lost"`（cross-family resumeも失敗し連続性を喪失した
ケース）の実頻度が判明してから再評価する（§9）。

**なお「救えない」のは連続性のみであり、接続そのものは自動的に
復旧する見込みで、`.claude/rules/always-connects.md`には抵触しない**:
cross-family resumeが失敗して`MidSessionDisconnect`がwrapperに届くと、
`decide_connect_failure_recovery`（`isekai-ssh/src/wrapper.rs:1007-1010`）
は`should_bootstrap`の値にかかわらず`RetryConnectLightweight`を返す
（この挙動は`wrapper.rs:2644-2651`のテストで固定されている）。
lightweight retryは再デプロイをしないため、本ケース（キャッシュ済み
アドレスが両方とも陳腐化している）では古いアドレスへの再試行を
繰り返すだけになりうるが、`run_ssh_with_connect_failure_recovery`
（`wrapper.rs:661-680`）には`lightweight_retries`カウンタと
`redeploy_gate`（`reconnect_backoff::RedeployGate`）があり、一定回数
失敗後に再デプロイへエスカレーションする経路が存在する見込みである。
**ただしこのエスカレーションの実在・発火条件は本ADRのレビューでは
未確認——実装着手前に`wrapper.rs`の該当経路を実際に確認すること。
もし存在しなければ、本節は「非目標」ではなく「別途対処が必要な
バグ」に格上げされる。**

## 5. Androidとの関係（対象外）

本ADRの変更対象（`connect.rs`/`resume_loop.rs`）は`isekai-pipe connect`
というバイナリ・コードパスであり、これは**Unix・Windowsいずれの
プラットフォームでも同じ**——経路は3つある
（`ADR_MIDSESSION_DISCONNECT_RECOVERY.md` §2.4）:

- **Unix**: `wrapper.rs::run_ssh_with_connect_failure_recovery`が
  `ssh(1)`をspawnし、`isekai-pipe connect`をProxyCommandとして起動
- **Windows単一プロセスfallback**: `native/connect.rs`の対応関数
- **Windows mux経路（既定）**: `native::mux::run_with_reconnect`

3経路とも`isekai-pipe connect`を子プロセスとして起動するため、
**本ADRの変更は3経路すべてに等しく効く**（`isekai-ssh/src/main.rs`の
`mod wrapper;`は`#[cfg]`ゲートされておらず、`wrapper.rs`自体は
クロスプラットフォームにビルドされる）。

一方Androidはrussh を **in-process** で持ち、`ReattachableStream`越しに
下のトランスポートを繋ぎ替える設計（`isekai-transport/src/resume.rs:16-24`
が「この型はAndroid専用で、`isekai-ssh`にはrusshがループに居ないので
移植していない」と明記）。つまりSSHクライアント状態が再ランデブーを
生き延びるため、§4.1の決定的障壁（SSHセッションが作り直される）は
Androidには存在しない。SSH bootstrapチャネルも in-process
（`rust-core/src/isekai_stun_p2p_transport.rs:213
bootstrap_via_ssh_with_punch`）にあり、Android のSTUN経路は既に
`reconnect_and_resume`ベースのreattachを持っている
（`isekai_stun_p2p_transport.rs:253-275`付近、ただし再STUN・再punchは
せず`peer_addr`へ直接張り直すだけというPhase 10の既知制約付き）。

ただしAndroidにも別の壁が1つある: `--punch-peer`は`serve`起動時の
ワンショット（`isekai-pipe/src/engine/mod.rs:646-670`）で、稼働中の
`isekai-pipe serve`に「新しいクライアントアドレスへ再punchせよ」と
伝える制御コマンドが存在しない。新しいserveを立ち上げればsession_secret
も証明書も変わり、旧セッションには原理的に到達できない。

**本ADRは`isekai-pipe connect`を経由するすべての経路（Unix/Windows
mux/Windows単一プロセスfallback）を対象とする。Androidは
`isekai-pipe connect`プロセスを一切起動せず`isekai-transport`を直接
in-processで使う構造のため、本ADRの変更は構造上Androidに何の影響も
与えない（「方針として除外した」のではなく「そもそも到達しない」）。**
Androidで同種の改善をするなら、serve側の再punch制御コマンド
（`isekai-pipe ctl`相当の追加）が前提タスクとして別途必要になる——
これは本ADRの範囲外の独立した設計課題として切り出す。

## 6. 成功基準

- **分母**: 「give-up境界に到達した回数」（＝§3.2タスク1の条件が
  成立し、cross-family resumeが実際に試みられた回数）。bare redialで
  120秒以内に復旧した切断（cross-family resumeが発火しないケース）は
  分母に含めない——含めるとSTUNが元気な環境で分母が膨らみ割合が
  意味をなさなくなる。
- **目標値**: この割合の上限は「`cached_relay_addr`がクライアントの
  新しいネットワークから到達可能である」という§3.3の未検証の仮定に
  支配される——仮定が崩れていれば実装が完璧でも達成率は頭打ちになり、
  「目標未達」が実装の失敗ではなく仮定の不成立を意味してしまう。
  したがって**最初の観測期間には合否判定を置かない**。§3.2タスク5の
  「セッション消滅」/「relay到達不能」の2分類の実測比率が出てから、
  **「relay到達可能だったケースのうち何%で連続性を保てたか」**という、
  実装の良否だけを測る分母に対して数値目標（初期目安80%）を設定する。
  「give-up境界到達回数」を分母にしたままだと、実装の良否と環境の
  性質（relayの到達可能性）が混ざってしまう。
- **読み出し手段**: 当面は`log::info!`（`--isekai-log-file`）の手動
  ログ検分による評価とし、専用の集計パイプラインは作らない。
  §3.2タスク9で触れた`isekai-ssh doctor`への件数表示は、将来の
  簡易集計手段の候補として検討する。

## 7. `ADR_INPUT_RESUME_SYMMETRY.md`との関係

両ADRのスコープ重複は「実装時に整理する」では不十分——衝突点は
**入力キューのキー設計そのもの**であり、今決めないと手戻りになる
（round 1レビューで特定済み）。以下は両ADRに共通で反映する結論。

### 7.1 構造上の結論（今すぐ確定する）

§3を採用すると、入力キューのflush境界は「同一トランスポートファミリ
内の再接続」から**「同一session_idでの再接続、トランスポートファミリ
不問」**に変わる。素直に実装すると（＝キューをconnectionやtransport
オブジェクトに紐づけると）、まさに連続性が保たれるようになった瞬間に
溜めた入力を黙って捨てる実装になる——最悪の失敗の仕方で、しかも
cross-family切替が起きる環境でしかテストで検出できない。

1. **スコープ**: 入力キューは`run_resume_loop`が保持する`session_id`
   （`isekai-pipe/src/resume_loop.rs:1260`）にスコープする。connection
   にもtransportにも紐づけない。
2. **置き場所**: `isekai-ssh`側のsession層でも`isekai-transport`でもなく
   **`isekai-pipe/src/resume_loop.rs`内、`C2hReplayBuffer`の隣**。
   理由: resumeのoffsets管理（`C2hReplayBuffer`/
   `helper_committed_offset`）が既にここにあり、session_idもここが
   唯一の保持者。`isekai-transport`に置くとAndroid経路
   （`ReattachableStream`経由、別のresume駆動構造）と無理に共有する
   ことになる。
3. **flush地点**: `isekai-pipe/src/resume_loop.rs:594
   replay_and_advance`と同じ場所。RESUME_ACKの
   `helper_committed_offset`を見て未ACKバイトを再送する既存処理の
   **直後**に、「そもそもオフラインで送信すらしていない新規入力」を
   継ぐ（先に未ACK再送、次に未送信flush、という順序を固定すれば
   既存の`ReplayBuffer`/`ClientResumeState`とはレイヤーが自然に
   分かれる）。

これは`ADR_INPUT_RESUME_SYMMETRY.md`§3の論点1（キューの置き場所）と
論点4（既存ReplayBufferとの役割分離）への直接の回答でもある
（**この2論点は本レビューを経て決着済み**——同ADR側にも同じ結論を
反映済み）。

### 7.2 安全要件への影響（決定順序が逆だとやり直しになる）

`ADR_INPUT_RESUME_SYMMETRY.md`§3論点3（切断中に打った危険なコマンドが
無警告で実行される事故の防止）は、本ADRの§3導入によって前提が変わる:
**サイレントに再接続が成功する窓が、STUNの120秒
（`STUN_RESUME_GIVE_UP_WINDOW`）からrelayのresume grace（既定10日）へ
伸びる。** つまり本ADRの§3採否が、入力ADRのリスク予算そのものを
変える。「スコープが重なる」ではなく**「片方の決定が他方の安全要件の
前提を変える」**が正確な関係。

**順序: 本ADR§3を先に決めてから、`ADR_INPUT_RESUME_SYMMETRY.md`の
上限サイズ・TTL・可視化ポリシーを決める**（逆順だと決め直しになる。
同ADRの論点2・3はこの決定待ちのため未決のまま）。

## 8. その他、実装前に確認が必要な項目

- **セキュリティ**: サーバーは既に任意送信元アドレスからのRESUMEを
  受理し、認証は新接続のTLS exporterに束縛されているため、本ADRの
  変更で新たな攻撃面は生まれない（必要なのは依然session_secretで
  あり、それを持つ攻撃者は新規ATTACHも同様にできる）。ただし
  `session_secret`自体のrotation機構は無く、連続性の窓を伸ばすほど
  同じ秘密がより長寿命のセッションを守ることになる——個人relayの
  脅威モデルでは問題にならない前提だが、意識的な選択として記録して
  おく。
- `ADR_ISEKAI_SSH_LOCAL_SCROLLBACK.md`（同時にdraft中）との価値の
  相互依存: ローカルスクロールバックがクライアント側で画面履歴を
  持つなら、本ADRが救えないケース（§4.2）の体験上の損失はその分
  小さくなる。3つのdraft ADR（本ADR・入力対称化・ローカル
  スクロールバック）は互いに価値が依存する関係にある。

## 9. 次のステップ

round 3レビューの総評は「MUST-FIX 4件を反映すればApprovedに進んでよい、
round 4は不要」——本rev4でそのMUST-FIX 4件（§3.2タスク1・2・4に
またがる、切替トリガーと有界リトライの相互作用の不備）とSHOULD-FIX
5件をすべて反映した。round 3が「round 4不要」と判断した根拠
（各指摘の修正文案が具体的で、反映結果は文面照合で判断できる）に
従い、**Statusを実質Approvedとして扱ってよい**。実装着手前の唯一の
残作業はR3-C5（§4.2、`wrapper.rs:661-680`の再デプロイエスカレーション
経路の実在確認）——これはADRレビューではなくコード確認事項であり、
実装着手時のチェックリスト項目として扱う。

§4.2（双方同時変化）は§3.2タスク5の計装導入後、`"continuity-lost"`の
実頻度が判明した時点で再評価する。
