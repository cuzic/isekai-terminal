# Review Round 2: `ADR_MIDSESSION_DISCONNECT_RECOVERY.md`（opus-critic-a、単独）

opus-critic-bはround 1 draftで収束（「収束、blocking/significantなし」）。
opus-critic-aは同じround 1 draftに新規blocking 2件・significant 5件を
発見した——round 0→round 1の訂正自体が生んだ新しい矛盾が中心。

## BLOCKING

- **R1-B1**: `decide_connect_failure_recovery`が`Some(Unknown) =>
  NoRecoverableSignal`としていたのは`.claude/rules/always-connects.md`
  に対する後退。既存の`wrapper.rs:621`は`outcome.is_some()`だけで
  復旧を試みる。`Unknown`は`Unreachable`と同じ扱いにすべき。→ round 2で
  `decide_connect_failure_recovery`を修正（ADR本文§2.2.2）。
- **R1-B2**: PR1がPR2の成果物（`MidSessionDisconnectSignal`）に
  依存しており、PR1単独では実装不能、かつWindows `Ok`経路のclaim
  修正だけを先行させると「サイレント再展開+見覚えのない2つ目の
  セッション」という現状より悪い退行を生む。→ round 2でB4のbail-out・
  Windowsの`Ok`経路claimを両方PR2へ移動（ADR本文§2.1・§2.2.2）。

## SIGNIFICANT

- **R1-S1**: `RECONNECT_BACKOFF.initial`(500ms)が
  `HANDLE_HEALTH_POLL_INTERVAL`(1秒)より短く、client再接続とholder
  退場の間にレースが生じ、mux機能が静かに失われうる。→ round 2で
  `shutdown.notify_waiters()`呼び出しを追加（ADR本文§2.2.1）。
- **R1-S2**: STUN経路の試行回数上限（3〜5回）がUnix側にしか適用
  されておらず、Windows muxの24hバジェットと非対称。→ round 2で
  Windows mux経由の`isekai-pipe connect`再spawnにも同じ上限を
  適用する方針を追加（ADR本文§2.2.1）。
- **R1-S3**: `ConnectOutcome`は`#[serde(flatten)]`越しのenumであり、
  `#[serde(other)]`が単体enumで動くことと flatten越しで動くことは
  別問題。→ round 2でPR1に「`ConnectOutcome`構造体全体を通した
  ラウンドトリップテストを必須化、動作しなければ`String`+変換関数へ
  フォールバック」を明記（ADR本文§2.1）。
- **R1-S4**: 行番号の誤り2箇所（`wrapper.rs:574-575`→`:618-619`、
  `wrapper.rs:769-772`→`:719-725`）。→ round 2で修正。
- **R1-S5**: 「孫プロセスの孤児化」を「事実に反すると判明」と断定
  したのは早計——`.status()`が孫を待たないことと孫が実際に残るかは
  別問題（`ssh(1)`自身がProxyCommandをkillする可能性がある）。
  → round 2で「実装時にまず`pgrep`で実測し、残る場合のみ
  process-group killを実装する」方針に変更（ADR本文§2.2.2）。

## MINOR

- **M-1**: PR3の「STUN経路にもrelayと同じ`give_up`無マーカー方針を
  適用するか」はOpen QuestionではなくPR3の受け入れ条件として
  決めきれる。→ round 2で決定済み事項として§2.3へ格上げ。
- **M-2**: §4.1「良くなる点」が「STUN P2P・Relay双方」としていたが、
  relay経路は元々10日resumeでカバーされ即死していなかった。→ round 2で
  文言を「STUN P2PとWindows mux経路」に訂正。
- **M-3**: 改訂履歴表とpseudocode内コメントで「B2」/「B5」の採番が
  食い違っていた。→ round 2で「B5」に統一。

## opus-critic-aによるOpen Question 2への具体案（採用）

`relay_loop`が既に持つ`exit_code: Option<u8>`
（`ChannelMsg::ExitStatus`受信時のみ`Some`）をそのまま判別子に使う。
子プロセスの終了コード/シグナルを新たに覗く必要はない。詳細は
ADR本文§2.2.1参照。

## 検証済みの正しさ（opus-critic-aが再確認した箇所）

`owner.rs:706-707`の無条件`Frame::Exit`送出、`client.rs:395-400`の
`OwnerLost`分岐、`run_with_reconnect`の各定数（`RECONNECT_BUDGET`=24h・
`RECONNECT_BACKOFF`=500ms/10s/0.25・`RECONNECT_STABLE_THRESHOLD`=60s）、
B6の`give_up`レースは、いずれもround 1の記述通り実在すると再確認された。
また、opus-critic-a自身がround 1で懸念していた「死んだholderに24時間
再接続し続ける」シナリオは、`owner.rs:216-225`の`handle_died`分岐で
既に解決済みと判明し、§2.2.1の設計を支持する追加根拠となった。

---

# Review Round 3: opus-critic-aによるround 2 draftの再レビュー

opus-critic-bはround 2で収束（「収束、blocking/significantなし」）。
opus-critic-aはblockingなし・significant 1件・minor 3件を発見。

## SIGNIFICANT

- **R2-S1**: round 2で採用した`exit_code.is_some()`判別子には穴が
  あった。SSHプロトコルはリモートプロセスがシグナルで終了した場合
  `exit-status`ではなく`exit-signal`（`ChannelMsg::ExitSignal`）を
  送るが、`owner.rs`/`native/connect.rs`はどちらも末尾の`_ => {}`で
  これを捨てており`exit_code`は`None`のままになる。`kill -9`・
  OOM killer・`tmux kill-server`・リモートcrashのようなシグナル
  終了が「transport死」と誤判定され、勝手に新しいシェルで再接続
  してしまう。→ round 3で`remote_reported_exit: bool`という別変数を
  判別子にする方式へ修正（ADR本文§2.2.1）。

## MINOR

- **R2-M1**: `outcome_summary`（`wrapper.rs:648-653`）への新variant
  アーム追加が`.claude/rules/always-connects.md`の既存ルールとして
  必要なのに、ADRに一度も登場していなかった。→ round 3でPR1
  （`Unknown`）・PR2（`MidSessionDisconnect`）双方の作業項目に明記
  （ADR本文§2.1・§2.2.2）。
- **R2-M2**: 孫プロセス対応（S1）が「実測してから決める」方針に
  変わったのに、§4.2・§5・§2.2.2の擬似コードの3箇所が機構を入れる
  前提のまま残っていた。→ round 3で条件付き表現に統一。
- **R2-M3**: Windows mux経路では`isekai-pipe connect`が
  `OwnerLost`のたびに新しいプロセスとして起動されるため、STUN試行
  回数を数える状態の置き場所が未定。→ round 3でPR2実装時の決定事項
  として明記（ADR本文§2.2.1）。
