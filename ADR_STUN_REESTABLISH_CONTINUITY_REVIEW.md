# ADR_STUN_REESTABLISH_CONTINUITY.md 批判的レビュー（round 1）

- **対象**: `ADR_STUN_REESTABLISH_CONTINUITY.md`（Status: Draft、2026-09-07起草）
- **レビュー日**: 2026-09-13
- **レビュー種別**: 実装着手前の設計方針レビュー（読み取り・分析のみ。コードは一切変更していない）
- **関連**: `ADR_MIDSESSION_DISCONNECT_RECOVERY.md`（前提ADR、Epic R）、
  `ADR_INPUT_RESUME_SYMMETRY.md`（スコープ重複の相手）、
  `.claude/rules/always-connects.md`、`PLAN.md:982`

---

## 目次

- [P0-1. `log_rendezvous_outcome` は本ADRの対象ケースを一切計装していない（前提の誤り）](#p0-1)
- [P0-2. 方向3は「難しい」のではなく、現状の isekai-ssh アーキテクチャでは実装不可能](#p0-2)
- [P0-3. 方向2と方向3は統合できる。しかも今すぐ安く実装できる（最重要）](#p0-3)
- [P1-4. Android は別アーキテクチャ。ADR の対象範囲行（5-6行目）が不正確](#p1-4)
- [P1-5. `ADR_INPUT_RESUME_SYMMETRY.md` とのスコープ重複を「実装時に整理」で先送りするのは不可](#p1-5)
- [P2-6. セキュリティ: なりすまし・リプレイ懸念はほぼ杞憂だが、実在する項目が2つある](#p2-6)
- [P2-7. ADR 本文の体裁・欠落](#p2-7)
- [P2-8. その他の見落としているエッジケース](#p2-8)
- [結論: どれから着手すべきか（最終推奨）](#conclusion)
- [主要参照ファイル一覧](#refs)

---

<a id="p0-1"></a>

## P0-1. `log_rendezvous_outcome` は本ADRの対象ケースを一切計装していない（前提の誤り）

レビュー依頼時に「PR #116（コミット `0df74233`）により `resume.rs` に
`log_rendezvous_outcome("fresh-rendezvous"/"abandoned", ...)` という計装が既に実装・
マージ済みで、『そもそも検知すらできないのでは』という懸念の一部は実装レベルでは既に
土台がある」という前提が与えられた。**この前提は誤っている。**

`log_rendezvous_outcome` の呼び出し元は
`rust-core/isekai-transport/src/resume.rs:408-414 / 486-492 / 515-521 / 592` の4箇所のみで、
**すべて `connect_via_relay_resumable_with_fallback` の内部**にある。この関数は relay 逐次
フォールバック専用である:

- 引数が `SequentialRelayCandidate` / `RelayTarget`（`resume.rs:385-389`）。
- telemetry identity が `resume.rs:429-434` で
  `kind: "relay", source: "config-relay", provider: "config-relay"` にハードコードされている。
- 関数自身の doc（`resume.rs:360-384`）が「relay-endpoint fallback specifically（`#12` のスコープ）」
  と明記している。

したがって:

- **STUN P2P 経路（`connect_stun_p2p_with_fallback` → `resume_loop.rs:383
  run_stun_p2p_with_fallback`、および `resume_loop.rs:335 run_stun_p2p_resumable`）は
  `log_rendezvous_outcome` を一度も呼んでいない。** リポジトリ全体を grep しても
  `stun_p2p.rs` には1件もヒットしない。
- **単一候補 relay 経路（`resume.rs:182 connect_via_relay_resumable`）も呼んでいない。**
  legacy profile の多くはこちらを通る（`connect.rs:739 ConnectRoute::SingleCandidate` →
  `connect.rs:740-757 CandidateRoute::Relay` → `run_relay_resumable`）。

### さらに、2つの class の意味論が「連続性喪失」とは別物である

- **`"abandoned"`** は「このラウンドで**一度も attach に成功しなかった**」という意味。
  具体的には (a) 全候補が `RetryablePreAttach` で失敗した（`resume.rs:592`）か、
  (b) generation リトライ予算を使い切った（`resume.rs:486-492` / `515-521`）かのいずれか。
  いずれも**セッションが存在する前**に出るログであり、**接続失敗**を表す。
  「確立済みセッションの連続性を失った」ことは一切意味しない。
- **`"fresh-rendezvous"`** は `resume.rs:408-414` で relay-fallback connect の**冒頭に無条件で**
  出る。プロセスの初回接続でも必ず出る。しかも `previous_session_id` は常に `None` であり、
  これは `telemetry.rs:212-221` が明示的に「この ADR のスコープが実際にカバーするどの
  呼び出し元でも `None` である。意味のある唯一のケース（same `session_id` を保った bare redial）
  は意図的にスコープ外」と書いている。

結果として、**「連続性を失ったから新セッションになった」と「ユーザーが今 `isekai-ssh` を
叩いたから新セッションになった」を原理的に区別できない。**

### 行動

1. 実運用ログの `"abandoned"` 件数を数えても本ADRの問いには何も答えない。
   「先にデータを取るべきか」という論点自体が、**取れるデータが存在しない**ため成立していない。
2. ADR §3（58-61行目）の「現状、真の再ランデブーのケースは検知すらされていないのか」への
   答えは **「検知されていない」と断定して書く**こと。
3. 本当に欲しいシグナルは、

   > 「`resume_loop.rs:335-374 run_stun_p2p_resumable` が `MidSessionDisconnectSignal` 付きで
   > 抜けた」→「`wrapper.rs:1007-1010` の `RetryConnectLightweight` が**別の** session_id で
   > 再確立した」

   という**ペアリング**である。これを記録する仕組みは現状どこにも無い。新規に計装タスクが要る
   （規模は小さい）。既存の `log_rendezvous_outcome` は relay 専用なので流用せず、
   `previous_session_id` 引数が既に用意されている（`telemetry.rs:212-221` が
   「将来そういう呼び出し元ができたら第2の似た関数を作らずに済むよう残してある」と明記）
   点だけを活かすのが素直。

---

<a id="p0-2"></a>

## P0-2. 方向3は「難しい」のではなく、現状の isekai-ssh アーキテクチャでは実装不可能

ADR §2-3（51-54行目）は「フルSTUN再確立後も、何らかの形で直前のセッションの resume バッファ
（`OutputBuffer`）を引き継げないか」を「技術的難度・価値ともに未評価」としている。
2つに分解すると明確な答えが出る。

### (a) プロトコル層: 既にできている（設計変更は不要）

- サーバー側セッション状態は **`SessionId` のみをキーとする** `HashMap`
  （`isekai-pipe/src/engine/resume.rs:22` の `pub type SessionId = [u8; 16]` と、
  `SessionTable` の `HashMap<SessionId, Arc<Mutex<Session>>>`）。
- `isekai-pipe/src/engine/mod.rs:915 handle_connection` は `mod.rs:962` で
  `quicmux::FRAME_RESUME` 分岐に入り、`handle_resume_stream`（`mod.rs:1341`）へ渡す。
  **送信元アドレスによる絞り込みは一切していない。**
- RESUME 認証は**新接続の TLS exporter 上**の
  `HMAC(session_secret, exporter || session_id)`（`isekai-transport/src/resume.rs:812`、
  `resume_on_connection` 内の `compute_proof`）。

つまり「まったく別アドレスから張った新しい QUIC 接続に、旧 session_id で resume する」は
**すでに `reconnect_and_resume`（`resume.rs:726`）が毎回やっていること**である。
ADR が想像している「バッファの引き継ぎ」は、プロトコルの問題ではない。

### (b) 本当の障壁: `ssh(1)` プロセスが死ぬこと

フルSTUN再確立は `isekai-ssh/src/wrapper.rs:661 run_ssh_with_connect_failure_recovery` が担当し、
その doc コメント（`wrapper.rs:628-633`）自身が

> this loop restarts the whole `ssh(1)` process rather than resuming one live QUIC connection

と書いている。`decide_connect_failure_recovery`（`wrapper.rs:1007-1010`）が
`MidSessionDisconnect` に対して返すのは `RetryConnectLightweight` であり、これは
「再デプロイはしないが、`ssh(1)` プロセスは作り直す」という意味である。

一方 `OutputBuffer` が保持しているのは、**旧 `ssh(1)` ↔ `sshd` の SSH トランスポート生バイト**
（暗号鍵・シーケンス番号・チャネルIDに束縛されたバイト列）である。これを新しい `ssh(1)` に
流し込むのは「連続性の復元」ではなく、**あるコネクションの暗号文を別のコネクションの
デクリプタに食わせる行為**であり、即座に MAC 検証失敗になる。

### ADR に書くべき具体的失敗シナリオ

1. **H2C 方向**: `ssh(1)` 再起動後に旧 `OutputBuffer` を replay すると、旧鍵で暗号化された
   バイト列が新 `ssh` の入力に入り `Corrupted MAC on input` / protocol error で即死する。
2. **C2H 方向（より悪い）**: resume は `helper_committed_offset`（`engine/resume.rs:51`）に基づいて
   **サーバー側からも** parked TCP へ再送する。旧セッションの未コミットバイトが、
   生きている `sshd` 側のストリームに注入され、**片方向の被害では済まない**。
3. **parked_tcp の生死**: そもそも `parked_tcp`（`engine/resume.rs:58`）は元の `sshd` への TCP。
   これが死んでいればセッションごと破棄され（`sweep_expired_parked`）、resume に意味が無い。
4. **立ち退き**: `--max-sessions` 到達時、`SessionTable::insert_existing` が
   `InsertOutcome::InsertedAfterEvicting` を返して古い parked セッションを立ち退かせる
   （`engine/resume.rs:39-46`）。クライアントが punch している最中に旧セッションが
   既に消えている可能性がある。

### 結論

**方向3を「文字どおりの形」では恒久的に非目標として明記すべき。**
`PLAN.md:982` の「対象外: SSHセッションそのものの再生成・代理応答・端末状態同期
（mosh的な state sync はやらない）」と同じ書式で畳むこと。今の「未評価」のまま置くと、
実装者が必ずここを掘って時間を溶かす（このレビューで実際に掘った）。

---

<a id="p0-3"></a>

## P0-3. 方向2と方向3は統合できる。しかも今すぐ安く実装できる（最重要）

### 発見: STUN primary と cross-family relay fallback は同じ helper・同じ session_secret

- `isekai-ssh/src/wrapper.rs:1305-1316 select_transport` は、primary を
  `IntentTransport::StunP2p { ..., session_secret_b64: legacy.session_secret_b64.clone() }` として作り、
  fallback として `profile.to_legacy_relay_transport()` を返す。
- `isekai-pipe-core/src/profile.rs:168-177 to_legacy_relay_transport` は
  `IntentTransport::Relay { helper_addr: legacy.helper_addr, ..., session_secret_b64:
  legacy.session_secret_b64 }` を作る。**primary とまったく同じ `session_secret_b64`。**
- `isekai-pipe/src/connect.rs:862-864` で、fallback も primary と同じ
  `intent.expected_server_identity.cert_sha256_hex` に対して検証される。
- `legacy_relay_transport.helper_addr` の出どころは `profile.rs:149-152` の
  `trust.cached_relay_addr`、すなわち **SSH ホスト（＝同じ `isekai-pipe serve` プロセス）の
  直接到達可能アドレス**である。

したがって **STUN P2P で確立したセッションは、同じ session_id + 同じ offsets のまま
relay（サーバー直アドレス）側へ `reconnect_and_resume` できる。**
新プロトコル不要・シグナリング不要・再punch不要・**`ssh(1)` も生存したまま**、
バイトレベル連続性が完全に保たれる。

### 現状これは明示的に拒否されている

`isekai-pipe/src/connect.rs:835-843`:

```rust
if primary_err.downcast_ref::<crate::resume_loop::MidSessionDisconnectSignal>().is_some() {
    return Err(primary_err).with_context(|| format!("isekai-pipe connect: {context_label} failed"));
}
```

コメントの根拠は「Doing so would silently start a brand-new session over a different transport
instead of reconnecting the one the user was actually using」。
**この理屈が真なのは、fallback が `connect.rs:871 run_relay_resumable`（＝新規 ATTACH）を
呼ぶからに過ぎない。** fallback を resume-preserving にすればこの反論は成立しなくなる。

### 実装位置

`isekai-pipe/src/resume_loop.rs:1077 resume_with_backoff_until_deadline` /
`resume_loop.rs:1251 run_resume_loop`。
STUN 専用の `max_resume_window`（`STUN_RESUME_GIVE_UP_WINDOW = 120秒`、`resume_loop.rs:68`）を
使い切って give up する**直前**に、relay target への `reconnect_and_resume` を1回試す。

### 射程 — 何を救い、何を救わないか

**救うケース（圧倒的多数）: 「クライアント側アドレスだけが変わった」**

Wi-Fi↔セルラー切替がこれにあたる。このときサーバー側の restricted-cone NAT は
新しいクライアントアドレスからのパケットを落とすため bare redial は失敗するが、
**サーバー自身のアドレスは変わっていないので relay 経路は生きている**。
現状のコードはこの一番よくあるケースで `ssh(1)` ごと殺し、スクロールバックと
シェル状態を失っている。

ADR が方向2（発生頻度を下げる）で狙っていた効果は、確立前のルーティングポリシー調整
という間接的手段ではなく、**確立後の連続性維持**という直接的手段でここで得られる。
方向2を別途やる必要が消える。

**救わないケース: ADR が主題に据えている「クライアント・サーバー双方のアドレスが同時に変わる」**

キャッシュ済み relay アドレス（`PersistentProfile::legacy_relay_transport.helper_addr`、
由来は `profile.rs:149-152` の `cached_relay_addr`）も同時に陳腐化するため、この手では救えない。
そこで必要になるのは in-process の再bootstrapシグナリングだが、
**`isekai-pipe` は `isekai-bootstrap`/russh に依存していない**
（`rust-core/isekai-pipe/Cargo.toml` と `rust-core/isekai-ssh/Cargo.toml` の依存リストを比較すれば明らか
 — `isekai-bootstrap`・`russh`・`russh-keys` を持つのは後者のみ）。
依存を足すことは `ADR_INPUT_RESUME_SYMMETRY.md:36-37` が記録している
「isekai-pipe は薄いトランスポート中継のままにしたい（ユーザー判断）」に真正面から反する。

**これが「両者同時変化はスコープ外」の本当の理由**であり、
今の ADR §2-3 にある「技術的難度・価値ともに未評価」よりはるかに強く、
かつ将来の蒸し返しを止められる根拠になる。ADR にはこの依存境界を根拠として明記すべき。

---

<a id="p1-4"></a>

## P1-4. Android は別アーキテクチャ。ADR の対象範囲行（5-6行目）が不正確

ADR は対象を `rust-core/isekai-transport`（`stun_p2p` モジュール）と
`rust-core/isekai-pipe`（`resume_loop.rs`）としているが、
真の再ランデブーの成立可能性は Android 側で根本的に異なる。

- Android は russh を **in-process** で持ち、`ReattachableStream` 越しに下のトランスポートを
  繋ぎ替える。`isekai-transport/src/resume.rs:16-24` が
  「この型は Android 側専用で、`isekai-ssh` には russh がループに居ないので意図的に移植していない」
  と明記している。つまり **SSH クライアント状態が再ランデブーを生き延びる**ので、
  P0-2(b) の決定的障壁（`ssh(1)` が再起動される）が Android には存在しない。
- SSH bootstrap チャネルも in-process にある
  （`rust-core/src/isekai_stun_p2p_transport.rs:213 bootstrap_via_ssh_with_punch`）。
  つまり「新しい観測アドレスをサーバーに教える」信号路を、プロセスを殺さずに使える。
- Android の STUN 経路は既に `reconnect_and_resume` ベースの reattach を持っている
  （`isekai_stun_p2p_transport.rs:253-275` 付近）。ただし同ファイルのコメント（260-272行目）が
  明記する通り、**再STUN・再punchはせず `peer_addr` へ直接張り直すだけ**という Phase 10 の
  既知制約付きで、「NATマッピング自体が失われるような長時間の切断・ネットワーク切り替えからは
  復旧できない（その場合はユーザーが再接続する）」と書かれている。

### ただし Android にも別の壁が1つある

**`--punch-peer` は `serve` 起動時のワンショット**である
（`rust-core/isekai-pipe/src/engine/mod.rs:646-670`。listen ループ突入前に punch probe を
`punch_targets` へ撃つだけで、以後の再punch経路は無い）。
稼働中の `isekai-pipe serve` に「新しいクライアントアドレスへ punch し直せ」と伝える
制御コマンドは存在しない。新しい serve を立ち上げれば session_secret も証明書も変わり、
旧セッション（`SessionTable` のエントリ）には原理的に到達できない。

### 行動

**ADR は Android を対象に含めるか否かを明示すること。**

- 含めるなら「serve 側の再punch制御コマンド（`isekai-pipe ctl` 相当の追加）」が
  前提タスクとして先に要る。
- 含めないなら、その理由（isekai-ssh 側の制約とは別物であること）を1段落書いておく。
  書かないと、対象範囲に `isekai-transport` と書いた時点で Android 経路も含むと読まれる
  （`isekai-transport` は Android/isekai-ssh 両方から使われる共有クレートであるため）。

---

<a id="p1-5"></a>

## P1-5. `ADR_INPUT_RESUME_SYMMETRY.md` とのスコープ重複を「実装時に整理」で先送りするのは不可

ADR 本文 64-66行目は「`ADR_INPUT_RESUME_SYMMETRY.md`（C→S入力のresume対称化）は、本ADRの
対象ケースでは新セッション扱いになるため保証範囲外になる。両ADRのスコープの重なりを
実装時に整理する必要がある」で止めている。

しかし衝突点は1箇所に特定でき、それが**入力キューのキー設計そのもの**を決めるため、
今決めないと確実に手戻りになる。

### 依存1（構造）— 今すぐ両ADRに書き込むべき結論

P0-3 を採用すると、入力キューの flush 境界が
「同一トランスポートファミリ内の再接続」から
**「同一 session_id での再接続、トランスポートファミリ不問」**に変わる。

素直に実装すると（＝キューを connection オブジェクトや transport オブジェクトに紐づけると）、
**まさに連続性が保たれるようになった瞬間に、溜めた入力を黙って捨てる**実装になる。
これは最悪の失敗の仕方で、しかもテストで検出しにくい（cross-family 切替が起きる環境でしか出ない）。

今決めておくべき結論は以下の3点:

1. **スコープ**: 入力キューは `run_resume_loop` が保持する `session_id`
   （`isekai-pipe/src/resume_loop.rs:1260` の `let session_id = established.session_id;`）に
   スコープする。connection にも transport にも紐づけない。
2. **置き場所**: `C2hReplayBuffer` の隣、すなわち `isekai-pipe/src/resume_loop.rs` 内。
   `isekai-ssh` 側の session 層でも、`isekai-transport` でもない。
3. **flush 地点**: `isekai-pipe/src/resume_loop.rs:594 replay_and_advance` と同じ場所。
   RESUME_ACK の `helper_committed_offset` を見て未ACKバイトを再送する既存処理の**直後**に、
   「そもそもオフラインで送信すらしていない新規入力」を継ぐ。

これは同時に `ADR_INPUT_RESUME_SYMMETRY.md` §3 の未決論点2つへの直接の回答になっている:

- **同§3 論点1（キューの置き場所: isekai-ssh 側 session 層か isekai-transport か）**
  → **どちらでもなく `isekai-pipe` の `resume_loop.rs`**。理由: resume の offsets 管理
  （`C2hReplayBuffer` / `helper_committed_offset`）が既にここにあり、
  session_id もここが唯一の保持者だから。`isekai-transport` に置くと Android 経路
  （`ReattachableStream` 経由、別の resume 駆動構造）と無理に共有することになる。
- **同§3 論点4（既存 `ReplayBuffer`/`ClientResumeState` との役割分離）**
  → 両者は「送信済みだが未ACKのバイト」を扱い、新キューは「未送信の新規入力」を扱う。
  同じ `replay_and_advance` 地点で**順序が確定的**（先に未ACK再送、次に未送信 flush）に
  なるよう並べれば、レイヤーは自然に分かれる。混線するのは「両方を1つのバッファに
  詰め込もうとしたとき」だけ。

### 依存2（安全要件）— 決定順序が逆だとやり直しになる

`ADR_INPUT_RESUME_SYMMETRY.md` §3 論点3 は「切断中に打った内容（危険なコマンド含む）が、
長時間後の再接続時に無警告で実行される事故をどう防ぐか」を挙げている。

P0-3 が入ると、**サイレントに再接続が成功する窓が 120秒（`STUN_RESUME_GIVE_UP_WINDOW`、
`resume_loop.rs:68`）から relay の resume grace（既定10日）へ伸びる**。
つまり方向3の採否が、入力ADRのリスク予算そのものを変える。

したがって ADR には「スコープが重なる」ではなく
**「片方の決定が他方の安全要件の前提を変える」**と書くのが正確。
順序としては **P0-3 を先に決めてから**、入力ADRの上限サイズ・TTL・可視化ポリシーを決めるべき
（逆順だと決め直しになる）。

---

<a id="p2-6"></a>

## P2-6. セキュリティ: なりすまし・リプレイ懸念はほぼ杞憂だが、実在する項目が2つある

### 新しい攻撃面は生まれない（ADRに明記すべき）

- サーバーは既に**任意の送信元アドレスからの RESUME を受理**している
  （`isekai-pipe/src/engine/mod.rs:962` の `quicmux::FRAME_RESUME` 分岐。
  アドレスによる絞り込みは一切ない）。
- 認証は**新接続の TLS exporter に束縛された** `HMAC(session_secret, exporter || session_id)`
  （`isekai-transport/src/resume.rs:812`）。

したがって「旧セッションの resume バッファを新しいランデブーに紐付ける」ことで
攻撃者に新たに要求されるものは無く、必要なのは依然 session_secret であり、
それを持っているなら新規 ATTACH も同様にできる。
**この1文を ADR に書いておくこと**（書かないとレビューのたびに再提起される）。

### リプレイについて

RESUME の proof は毎回新接続の exporter で計算されるため、旧接続で観測した proof を
そのまま再生しても通らない。offsets（`client_sent_offset` / `client_delivered_offset`）は
攻撃者が任意に申告できるが、それは現行の resume でも同じで、`OffsetGone` 判定
（`quicmux::ResumeRejectReason::OffsetGone` → `isekai-transport/src/resume.rs:693-699`
の `map_reject_reason`）が既に境界を守っている。**本ADRで新たに悪化する点は無い。**

### 実在する新規 failure mode A（ADRの失敗モード一覧に無い）

「後着優先」プリエンプション（`isekai-pipe/src/engine/resume.rs:78-86` の
`preempt` / `reparked` の2つの `Notify`。`ADR_SLEEP_RESUME_MUX_OWNER_DEATH.md` D-2 由来）。

cross-family resume を足すと、**ゾンビ化した旧 STUN 接続と新しい relay 接続の
両方が正当な「このクライアント」**になる。サーバーから見て生きているように見える
STUN 接続が park を握ったまま、relay からの RESUME が `preempt` を撃ち、
その直後に STUN 側が再接続してまた `preempt` を撃つ、という ping-pong が原理的に起こりうる。

「片方が勝ったら負けた側は再試行しない」ことを設計で保証する必要がある。
クライアント側で「cross-family へ切り替えたら旧ファミリの再試行を止める」ラッチを持つのが素直。

### 実在する項目B

`session_secret` はサーブプロセス単位で、rotation 機構が無い。
連続性の窓を伸ばすほど同じ秘密がより長寿命のセッションを守ることになる。
個人relayの脅威モデル（このプロジェクトの既定方針）では阻害要因にはならないが、
**意識的な選択であることを1行残すべき**。

---

<a id="p2-7"></a>

## P2-7. ADR 本文の体裁・欠落

- **§1（20-24行目）** は relay の resume window 既定10日だけを挙げているが、
  本ADRの対象ケースを実際に支配する数値は
  **`STUN_RESUME_GIVE_UP_WINDOW = 120秒`**（`isekai-pipe/src/resume_loop.rs:68`）である。
  これが「真の再ランデブーへ落ちるまでの猶予」の実体なので §1 に明記すること。
- **§3（58-61行目）** の「現状、真の再ランデブーのケースは検知すらされていないのか」は、
  P0-1 の結論（**検知されていない。既存の `log_rendezvous_outcome` は relay 逐次フォールバック専用**）
  で閉じること。open のまま残すと次の読者が同じ調査をやり直す（このレビューで丸ごとやり直した）。
- **§2-1「通知の改善」の位置づけ**: 方向1は独立した選択肢ではなく、
  P0-3 を実装する際に**必然的に必要になる副産物**である（cross-family resume が成功したか
  失敗したかを記録しないと、効果測定も回帰検出もできない）。
  「3つの方向のうち1つ」ではなく「方向2/3 の前提条件」として書き直すのが正確。
- **「非目標」節が無い**。P0-2 を踏まえると、このADRが後世に残せる最も価値ある記述は否定形である:

  > 再起動された `ssh(1)` プロセスに旧セッションの `OutputBuffer` を引き継ぐことは恒久的に対象外。
  > 理由: バッファの中身は旧 `ssh(1)`↔`sshd` の SSH トランスポート生バイトであり、
  > 鍵・シーケンス番号が異なる新プロセスに投入することは連続性の復元にならない。

  `PLAN.md:982` と同じ書式で書けば、将来同じ検討が再発しない。
- **成功基準が無い**。「STUN 経路で mid-session 切断が起きたとき、`ssh(1)` を再起動せずに
  復旧した割合」を指標として書けば、P0-3 の効果が計装（方向1）で直接測れる。

---

<a id="p2-8"></a>

## P2-8. その他の見落としているエッジケース

ADR draft が触れていないが、実装前に決めておくべきもの。

1. **cross-family resume 中の `BUSY_OTHER_SESSION` 競合**
   `retry_while_busy_other_session`（`resume_loop.rs:268`、窓は `BUSY_OTHER_SESSION_RETRY_WINDOW`）は
   **新規 attach 経路にしか掛かっていない**。P0-3 の cross-family resume は
   `reconnect_and_resume` を直接呼ぶ経路なので、この保護の外側に出る。
   自分自身の旧 STUN セッションが park を握っている状態で relay 側から resume すると、
   `BUSY_OTHER_SESSION` ではなく preempt 経路（P2-6 の failure mode A）に入るはずだが、
   どちらの経路に落ちるかは `parked_tcp` が `Some`/`None` のどちらかに依存する
   （`engine/resume.rs:56-60` と `mod.rs:1341 handle_resume_stream`）。
   **両方のケースで前進することを設計時に確認すること。**

2. **`effective_resume_grace_secs` の引き継ぎ**
   STUN 経路で確立したセッションの `effective_resume_grace_secs` は
   ATTACH_HELLO の ACK でサーバーが返した値（`resume.rs:157-164`）。
   cross-family で relay へ resume したとき、**bare RESUME/RESUME_ACK では再ネゴシエーションが
   起きない**（`resume.rs:634-648` の `finish_via_resume` のコメントがこの問題を既に踏んでいる
   ——`0` を入れると次の切断で即 give up する、という実バグとして修正済み）。
   同じ罠を P0-3 でも踏まないよう、引き継ぎ時に元の値を持ち回すことを明記する。

3. **サーバー側 `negotiated_resume_grace_secs` との整合**
   サーバーは `Session::negotiated_resume_grace_secs`（`engine/resume.rs:64-71`）と
   グローバル `--resume-window` の**短い方**で `sweep_expired_parked` する。
   クライアントが STUN の 120秒境界を越えて relay resume を試みる時点で、
   サーバー側の park がまだ生きているとは限らない。
   **`UnknownSession` 拒否を「連続性喪失が確定した」シグナルとして正しく扱うこと**
   （`resume_loop.rs:999 is_unknown_session_rejection` と
   `resume_loop.rs:1016 update_unknown_session_streak` に既存の判定ロジックがある。
   cross-family 経路もこれを通すべき）。

4. **`experimental_network_rebind` / warm-standby が STUN 経路で無効化されている**
   `resume_loop.rs:363-370` のコメント通り、STUN P2P では rebind も warm-standby promote も
   意図的に無効（punch 済み NAT マッピングがソケットに束縛されているため）。
   **cross-family で relay へ移った後は、これらを有効化してよいのか**を決める必要がある。
   relay 経路の本来の設定（`run_relay_resumable` が受け取る `experimental_network_rebind` /
   `tethering_interface`）に切り替えるのが筋だが、`run_resume_loop` はこれらを
   引数で1回受け取るだけなので、経路切替時に更新する仕組みが無い。

5. **`isekai-ssh doctor` の表示**
   直近のコミット群（`172d2c25` 他）で doctor に holder ログ所在の一覧表示が入った。
   P0-3 の cross-family resume が入ると「どの経路で今つながっているか」が
   セッション途中で変わりうるようになる。doctor / ステータス表示が
   「確立時の経路」を静的に見せているなら、実態とずれる。要確認。

6. **`ADR_ISEKAI_SSH_LOCAL_SCROLLBACK.md` との関係（未読だが同時に draft 中）**
   本ADRの主題は「スクロールバックの連続性」であり、ローカルスクロールバックADRが
   クライアント側で画面履歴を保持するなら、**方向3の価値そのものが目減りする**
   （サーバー側バッファを引き継げなくても、ローカルに履歴があれば体験上の損失が小さい）。
   3つの draft ADR（本ADR・入力対称化・ローカルスクロールバック）は相互に価値が依存している。
   本ADRの §3 に、この依存を1行入れておくべき。

---

<a id="conclusion"></a>

## 結論: どれから着手すべきか（最終推奨）

**方向1でも2でも3でもなく、「方向1の中身を差し替えたうえで、方向2と方向3を統合した1本の変更」
から着手すべき。** 具体的な順序は以下。

### 1. 最優先: P0-3 を実装する

STUN の give-up 境界（`STUN_RESUME_GIVE_UP_WINDOW`、`resume_loop.rs:68`）到達直前に、
同じ session_id / offsets のまま cross-family relay target へ `reconnect_and_resume` を試みる。
`connect.rs:835-843` の `MidSessionDisconnectSignal` bail-out は、
resume-preserving 版の fallback を導入したうえで書き換える。

**理由**:

- **(a) 発生頻度データを待つ必要がない。** この変更は「発火すれば必ず改善、発火しなくても
  余分な dial が1回増えるだけ」であり、正当化が頻度データに依存しない構造になっている。
  そもそも P0-1 の通り**現状取れるデータは存在しない**ので、データ待ちを理由に止める合理性が無い。
- **(b) ユーザーが実際に最も頻繁に踏む「クライアント側アドレスだけ変化」（Wi-Fi↔セルラー）を
  完全に救う。** 現状はこのケースで `ssh(1)` ごと再起動しており、
  `.claude/rules/always-connects.md` の字義（ユーザーの手動介入なしに復旧する）には
  適合しているが、体験としては最悪の復旧の仕方をしている。
- **(c) 新プロトコル・新シグナリング・新クレート依存がゼロ。** サーバー側は `SessionTable` が
  session_id のみをキーにしており、任意アドレスからの RESUME を既に受理する
  （`engine/mod.rs:962`）ので、**変更は完全にクライアント側で閉じる**。
- **(d) 方向2（relay優先フォールバック強化で発生頻度を下げる）が狙っていた効果を、
  確立前のポリシー調整という間接的手段ではなく、確立後の連続性維持という直接的手段で得られる。**
  方向2を別途やる必要が消える。

### 2. 同じPRに P0-1 の検知計装を入れる

STUN give-up 時に previous session_id 付きで、かつ cross-family resume の成否
（成功なら「連続性維持」、失敗なら「連続性喪失＋新セッション」）を記録する。
既存の `log_rendezvous_outcome` は relay 専用なので流用せず、
`previous_session_id` 引数が既に用意されている（`telemetry.rs:212-221` が
「将来そういう呼び出し元ができたら第2の似た関数を作らずに済むよう残してある」と明記）
点だけ活かす。**これで初めて「連続性が保たれたか」が実運用ログから読めるようになる。**

### 3. 方向3の文字どおりの形は非目標として閉じる

フルSTUN再確立後の `OutputBuffer` 引き継ぎは、P0-2 の根拠
（`ssh(1)` 再起動により旧 SSH トランスポート状態が失われる。`wrapper.rs:661` とその
doc コメント `wrapper.rs:628-633`）を書いて畳む。

### 4. 「両者同時変化」の真の再ランデブーは、計装で頻度が測れるようになってから再評価する

つまり**データ待ちにすべきなのはここだけ**であり、ADR 全体・P0-3 の着手を待たせる理由にはならない。
再評価の際は P1-4（Android なら成立しうるが serve 側の再punch制御が要る）と、
`isekai-pipe` への `isekai-bootstrap` 依存追加の是非が論点になる。

### 5. `ADR_INPUT_RESUME_SYMMETRY.md` は P0-3 の決定後に着手する

P1-5 の依存2（サイレント再接続窓が 120秒→10日に伸びることで、危険コマンド遅延実行の
リスク予算が変わる）があるため。
ただし **P1-5 の依存1（キューは session_id スコープ、`C2hReplayBuffer` の隣、
flush は `replay_and_advance` 地点）は今すぐ両ADRに書き込んでおくこと。**

---

<a id="refs"></a>

## 主要参照ファイル一覧

- `/home/cuzic/isekai-terminal/rust-core/isekai-transport/src/resume.rs`
  — `log_rendezvous_outcome` 呼び出し4箇所（408/486/515/592）、
  `connect_via_relay_resumable` 182、`connect_via_relay_resumable_with_fallback` 385、
  candidate identity ハードコード 429-434、`finish_via_resume` 606、
  `effective_resume_grace_secs` の罠コメント 634-648、`map_reject_reason` 693、
  `reconnect_and_resume` 726、`resume_on_connection` 795、proof 計算 812、
  `ReattachableStream` を移植しない理由 16-24
- `/home/cuzic/isekai-terminal/rust-core/isekai-transport/src/telemetry.rs`
  — `log_rendezvous_outcome` 229、意味論の docs 200-228、
  `previous_session_id` が常に None である説明 212-221
- `/home/cuzic/isekai-terminal/rust-core/isekai-transport/src/stun_p2p.rs`
  — モジュール doc の "Full re-rendezvous ... out of scope" 31-34、
  `connect_stun_p2p` 112、punch 定数 52-62
- `/home/cuzic/isekai-terminal/rust-core/isekai-pipe/src/resume_loop.rs`
  — `STUN_RESUME_GIVE_UP_WINDOW` 68、`MidSessionDisconnectSignal` docs 87-120、
  `retry_while_busy_other_session` 268、`run_stun_p2p_resumable` 335、
  rebind/warm-standby を STUN で無効化する理由 363-370、
  `run_stun_p2p_with_fallback` 383、`replay_and_advance` 594、
  `is_unknown_session_rejection` 999、`update_unknown_session_streak` 1016、
  `resume_with_backoff_until_deadline` 1077、`run_resume_loop` 1251、session_id 保持 1260
- `/home/cuzic/isekai-terminal/rust-core/isekai-pipe/src/connect.rs`
  — route dispatch 702-745、`recover_via_cross_family_fallback` 822、
  `MidSessionDisconnectSignal` bail-out 835-843、fallback の identity 検証 862-864、
  `run_relay_resumable` 呼び出し 871
- `/home/cuzic/isekai-terminal/rust-core/isekai-pipe/src/engine/resume.rs`
  — `SessionId` 22、`OutputBuffer` 36、`InsertOutcome` 39-46、`Session` 48-89、
  `helper_committed_offset` 51、`parked_tcp` 58、
  `negotiated_resume_grace_secs` 64-71、`preempt`/`reparked` 78-86
- `/home/cuzic/isekai-terminal/rust-core/isekai-pipe/src/engine/mod.rs`
  — `handle_connection` 915、`FRAME_RESUME` 分岐 962、`handle_resume_stream` 1341、
  punch がワンショットである箇所 646-670、`client_candidate_punch_targets` 505
- `/home/cuzic/isekai-terminal/rust-core/isekai-ssh/src/wrapper.rs`
  — `run_ssh_with_connect_failure_recovery` 661、
  「ssh(1) プロセスごと再起動する」doc 628-633、
  `outcome_summary` 870、`decide_connect_failure_recovery` 1007-1010、
  `select_transport` 1305-1317、`TofuConfirmation` 1357付近
- `/home/cuzic/isekai-terminal/rust-core/isekai-pipe-core/src/profile.rs`
  — `to_legacy_relay_transport` 168-177、`cached_relay_addr`/`cached_session_secret` 149-152
- `/home/cuzic/isekai-terminal/rust-core/src/isekai_stun_p2p_transport.rs`
  — Android 経路、`bootstrap_via_ssh_with_punch` 213、
  `connect_stun_p2p_stream` 233、Phase 10 の reattach 制約コメント 260-272
- `/home/cuzic/isekai-terminal/rust-core/isekai-pipe/Cargo.toml` と
  `/home/cuzic/isekai-terminal/rust-core/isekai-ssh/Cargo.toml`
  — `isekai-bootstrap`/`russh` 依存の有無＝「isekai-pipe は薄いまま」制約の実体
