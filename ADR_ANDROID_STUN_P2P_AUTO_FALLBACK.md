# ADR: STUN P2Pに、relayへの自動フォールバックをopt-in変種として追加する(Android)

- **Status**: Draft(2026-09-16起草。Windows `isekai-ssh` との接続安定化
  ギャップ分析セッションから派生。実装着手前にレビュー要——本ADRは
  `opus-adversarial-consult`の対象には含めていない(ユーザー指定)が、
  §3の「既存のセキュリティ判断と矛盾しないか」は着手前に必ず再確認すること)
- **対象**(見込み、要精査): `rust-core/src/isekai_stun_p2p_transport.rs`、
  `rust-core/src/orchestrator.rs`(`LastConnectAttempt`新variant)、
  `rust-core/src/lib.rs`(`TransportPreference` enum)、
  `android/src/main/kotlin/tools/isekai/terminal/data/ConnectionProfile.kt`・
  `ProfileEditScreen.kt`。**前提として`rust-core/src/isekai_link_relay_transport.rs`
  (MASQUE relay)の実装完了が必要**(§3参照)
- **入力**: `ADR_ANDROID_RECONNECT_TIMEOUT.md`と同一のセッション
- **拘束される既存ルール**: `.claude/rules/always-connects.md`。
  PLAN.md Phase 10「STUN/Relayのフォールバックなし設計」の既存判断
  (下記§1.1)

---

## 1. 背景

`rust-core/src/isekai_stun_p2p_transport.rs`のdocコメント:

> relay は一切経路に登場しない…穴あけ不成立時のフォールバックを
> 持たない(ユーザーが別の`TransportPreference`に切り替える運用)

`orchestrator.rs`の`connect_isekai_stun_p2p`も同様に「フォールバック無し
(穴あけ不成立時は接続失敗として扱う)」と明記している。

### 1.1 これは意図的な設計判断であり、バグではない

PLAN.md Phase 10の記述、および外部レビュー(2026-07-03、ChatGPT相談)の
結論:

> STUN/Relayの「フォールバックなし」を内部実装のままにしつつ、ユーザー
> 向けには…Strict Isekai Link(実験的・フォールバックなし)(自作ヘルパー
> QUIC・マルチパス・STUN P2P・relay P2Pの4方式、すべて明示的にフォール
> バック無し)
>
> 判断が割れなかった点(両者一致): …**フォールバックなし設計自体は
> セキュリティ的に正しい**

「Strict Isekai Link」を選ぶユーザーは、通信が黙って別経路(特に
第三者(seera-networks)が運用するrelayサーバー経由)へ切り替わらないことを
期待して選んでいる可能性が高い。**したがって「フォールバックを足す」ことは
既存の意図的なセキュリティ判断への回帰であってはならない。**

一方、Windows(isekai-ssh)の`wrapper.rs::select_transport`は
`primary: IntentTransport::StunP2p`に対し`fallback: profile.to_legacy_relay_transport()`
を常に組んで接続しており、Android版のような「フォールバック無し専用
モード」を持たない。またAndroidの`TransportPreference::Auto`
(`connect_isekai_pipe_quic_auto`)は既に「ヘルパーQUIC失敗→plain SSHへ
自動フォールバック」という、Strictとは別の"Smart"変種のパターンを
持っている(`ProfileEditScreen`の3択: Plain SSH / **Smart Connect(推奨)**
/ Strict Isekai Link)。

## 2. 問題

対称NAT等で穴あけが成立しない環境のユーザーは、STUN P2Pのみでは接続
できず、`always-connects.md`が要求する「ユーザー操作なしの自動復旧」を
満たさない。ただし前述の通り、これは「Strict」を選んだユーザーには
**むしろ仕様**であり、直すべきは「Strict以外の選択肢が無い」ことの方。

## 3. 実装方針

既存の"Auto"パターンを踏襲し、`TransportPreference`に新しい変種
(仮称`IsekaiStunP2pQuicAuto`、UIラベル案「Smart P2P(推奨)」)を追加する。
**既存の`IsekaiStunP2pQuic`(Strict、フォールバック無し)はそのまま残し、
デフォルトも変更しない。** ユーザーが新しい"Auto"variantを明示的に
選んだときだけ、STUN P2P失敗時にrelay(`isekai_link_relay_transport.rs`)
へ自動フォールバックする。

`isekai_pipe_quic_transport.rs::connect_auto`と同型の関数を
`isekai_stun_p2p_transport.rs`に追加し、STUN穴あけ失敗
(`AcquireOutcome::DialFailed`相当)を検出したらrelay接続を試みる構成にする。

**前提条件**: `isekai_link_relay_transport.rs`(MASQUE relay)は現状
未実装(PLAN.md記載、`bound-udp-server`のローカルビルド断念によりe2e検証
手段を模索中)。relay自体が動かない限り、本ADRのフォールバック先が
存在しない。したがって本ADRは:

- (a) relay実装完了を待ってから本ADRに着手する、
- (b) 暫定的にフォールバック先をrelayではなくplain SSH(既存の`Auto`と
  同じ着地点)にする、

のいずれかを選ぶ必要がある。(b)は「Strict Isekai Link」の対義語として
ユーザーが期待する「P2Pが無理でも、せめてrelay経由のIsekai Linkは保つ」
という体験からは外れる(plain SSHはIsekai Link基盤を一切使わない別の
接続方式)ため、素直には(a)が本命だが、relay実装のタイムラインに依存する。

## 4. 非目標

- 「Strict Isekai Link」のフォールバックなし挙動自体は変更しない。
- `isekai_link_relay_transport.rs`(MASQUE relay)本体の実装は本ADRの
  範囲外(前提タスクとして別途完了させる)。

## 5. Open Questions

- 上記(a)/(b)のどちらを取るか、あるいはrelay実装完了まで本ADRの着手を
  明示的に保留するか。
- UIラベリング: 既存の3択(Plain SSH/Smart Connect/Strict Isekai Link)に
  4つ目を足すのか、「Strict Isekai Link」の下にサブオプションとして
  ぶら下げるのか。
- フォールバック発生をユーザーにどう通知するか——黙って切り替わると、
  "Smart"を選んだユーザーが「今relay経由になっている(=第三者サーバーを
  経由している)」ことに気づかないまま使い続けるリスクがある。
  `TransportEvent`経由でUI通知を追加すべきか。
- 本ADRの着手前に、既存の「フォールバックなし設計はセキュリティ的に
  正しい」という外部レビュー結論(§1.1)と矛盾しないことを、
  `opus-adversarial-consult`等で改めて確認すべきか(ユーザーは今回
  ADR1・2のみレビュー対象に指定したが、本ADRのセキュリティ的な
  センシティビティを踏まえ、実装着手前には別途レビューを推奨する)。

## 6. 参照実装

(実装後に追記)
