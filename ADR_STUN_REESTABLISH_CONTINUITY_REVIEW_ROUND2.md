# ADR_STUN_REESTABLISH_CONTINUITY.md 批判的レビュー（round 2）

- **対象**: `/home/cuzic/isekai-terminal/ADR_STUN_REESTABLISH_CONTINUITY.md`（Status: Draft rev2）
  および `/home/cuzic/isekai-terminal/ADR_INPUT_RESUME_SYMMETRY.md`（§3 論点1/3/4/5 更新分）
- **レビュー日**: 2026-09-13
- **前回**: `/home/cuzic/isekai-terminal/ADR_STUN_REESTABLISH_CONTINUITY_REVIEW.md`（round 1）
- **レビュー種別**: 実装着手前の設計方針レビュー（読み取り・分析のみ。コードは一切変更していない）

---

## 目次

- [総評 — Approved に進めてよいか](#verdict)
- [A. round 1指摘の反映状況チェック](#section-a)
- [B. MUST-FIX: 新たに見つかった技術的な誤り・矛盾](#section-b)
  - [R2-B1. §3.2 タスク2（bail-out の「置き換え」）は §4.1 が禁じている破壊を再導入する](#r2-b1)
  - [R2-B2. §8 第1項の `BUSY_OTHER_SESSION` は cross-family resume 経路では構造上発生しない](#r2-b2)
  - [R2-B3. §5「Windows/native のみを対象」はプラットフォーム範囲の記述が逆](#r2-b3)
  - [R2-B4. cross-family 切替後に `max_resume_window`（120秒キャップ）を外す記述が無い](#r2-b4)
  - [R2-B5. §3.2 タスク3 は自己矛盾（「流用せず新規実装」vs「引数は既に用意されている」）](#r2-b5)
  - [R2-B6. §3.2 タスク6 の「既存の `UnknownSession` 判定を通す」は単発試行では機能しない](#r2-b6)
- [C. SHOULD-FIX: 設計として欠けている論点](#section-c)
  - [R2-C1. 「120秒待ってから relay」では救うケースの体験が壊れている — ネットワーク変化シグナルで即座に切り替えるべき](#r2-c1)
  - [R2-C2. cross-family target の入手経路（plumbing）が未記述 — 実装者が誤った層に実装する](#r2-c2)
  - [R2-C3. §3.3 の価値は「`cached_relay_addr` が新しいネットワークから到達可能」という未検証の仮定に依存している](#r2-c3)
  - [R2-C4. §3.2 タスク4（preempt ping-pong ラッチ）は駆動主体が1つしかないため前提が成立していない可能性](#r2-c4)
  - [R2-C5. §6 の成功基準は分母・目標値・読み出し手段が未定義で、このままでは測れない](#r2-c5)
- [D. NICE-TO-HAVE: 軽微な不整合・体裁](#section-d)
- [E. `ADR_INPUT_RESUME_SYMMETRY.md` 側の確認結果](#section-e)
- [F. 結論と、Approved までに必要な差分](#section-f)

---

<a id="verdict"></a>

## 総評 — Approved に進めてよいか

**まだ Approved には進めない。** ただし残っているのは方針レベルの問題ではなく、
**§3.2 のタスク記述に含まれる具体的な技術的誤りが2件（R2-B1、R2-B2）、
プラットフォーム範囲の記述誤りが1件（R2-B3）、実装すると必ず踏むバグが1件（R2-B4）**
という、いずれも「本文の該当段落を書き直せば解消する」種類のもの。

方針（§2、§3.1、§4）そのものは round 1 の指摘が正しく反映されており、**技術的に正しい**。
`SessionTable` が `SessionId` のみをキーにしていること、RESUME が任意アドレスから受理される
こと、primary と cross-family fallback が同じ `session_secret` を共有していること——
これら §3.1 の根拠はすべてコードで再確認して正しい。§4.1 の非目標化の論証も正しい。

特に **R2-B1 は放置すると Epic R PR2 Task 2.7 が塞いだバグ（しかも §4.1 が
「絶対にやってはいけない」と書いたのと同じ破壊）を再導入する**ため、必ず直すこと。

---

<a id="section-a"></a>

## A. round 1指摘の反映状況チェック

| round 1 指摘 | 反映先 | 判定 |
|---|---|---|
| P0-1（`log_rendezvous_outcome` は本ケースを検知していない） | §1.1 | **正しく反映**。呼び出し元4箇所・relay専用である根拠・`"abandoned"`/`"fresh-rendezvous"` の意味論・`previous_session_id` が常に `None` である点まで、すべて正確に記載されている。断定形（「検知すらされていない」）になっている点も意図通り |
| P0-2（方向3は実装不可能） | §4.1 | **正しく反映**。C2H 方向の巻き添え被害（`helper_committed_offset` 経由でサーバー側からも parked TCP へ再送する）まで書けている。`PLAN.md:982` と同じ書式で畳んだのも意図通り。ただし引用している `wrapper.rs:661` は Unix 経路のみ → R2-B3 参照 |
| P0-3（cross-family resume-preserving fallback を主方針に） | §2、§3.1、§3.3 | **方針としては正しく反映**。§3.1 の根拠は全てコードで再確認して正しい。ただし §3.2 のタスク化で誤りが混入 → R2-B1、R2-B2、R2-B4、R2-C1、R2-C2 |
| P1-4（Android 対象外の切り分け） | §5 | **反映されているが記述に誤り**。Android を対象外にする判断と理由（russh in-process、`--punch-peer` ワンショット）は正しい。ただし「isekai-ssh（Windows/native）のみを対象とし」が逆 → R2-B3 |
| P1-5（入力ADRとの依存を具体的結論に） | §7.1、§7.2 | **正しく反映**。スコープ／置き場所／flush 地点の3点と、決定順序の結論が両ADRに同じ内容で入っている。相互参照も張られている |
| P2-6（セキュリティ） | §8 第2項 | **正しく反映**。「新たな攻撃面は生まれない／`session_secret` rotation 無しは意識的選択」の両方が記載されている |
| P2-7（`STUN_RESUME_GIVE_UP_WINDOW` を §1 へ、成功基準を追加） | §1、§6 | 120秒の明記は**正しく反映**。成功基準は追加されたが測定可能性が不足 → R2-C5 |
| P2-8（BUSY_OTHER_SESSION 競合、LOCAL_SCROLLBACK との価値依存） | §8 | LOCAL_SCROLLBACK の依存は**正しく反映**。`BUSY_OTHER_SESSION` の項は**round 1 の私の指摘自体が不正確で、それがそのまま転記されている** → R2-B2 で訂正する |
| P2-8（`effective_resume_grace_secs` 引き継ぎ、park 生死、rebind/warm-standby、doctor表示） | §3.2 タスク5〜8 | 列挙されている。ただしタスク5は `max_resume_window` の観点が抜けており（R2-B4）、タスク6は既存判定をそのまま通せない（R2-B6） |

---

<a id="section-b"></a>

## B. MUST-FIX: 新たに見つかった技術的な誤り・矛盾

<a id="r2-b1"></a>

### R2-B1. §3.2 タスク2（bail-out の「置き換え」）は §4.1 が禁じている破壊を再導入する

**該当**: `ADR_STUN_REESTABLISH_CONTINUITY.md` §3.2 タスク2（135-136行目）

> 2. `isekai-pipe/src/connect.rs:835-843`の`MidSessionDisconnectSignal`
>    bail-outを、上記1のresume-preserving fallback呼び出しに置き換える。

**これは誤り。bail-out は残さなければならない。**

理由を分解する。

1. **タスク1とタスク2は互いに排他的な2つの層を指している。**
   タスク1は `resume_loop.rs` の `run_resume_loop` / `resume_with_backoff_until_deadline` 内部、
   つまり **`isekai-pipe connect` のデータポンプが生きている最中**の処理。
   タスク2は `connect.rs:822 recover_via_cross_family_fallback`、つまり
   **`run_resume_loop` が既に `Err` を返して抜けた後**の処理。
   タスク1が成功すればタスク2の地点には到達しない。到達したということは
   **cross-family resume が失敗した（＝連続性が確定的に失われた）**という意味である。

2. **その地点で `run_relay_resumable`（新規 ATTACH）を呼ぶことは、§4.1 が
   「絶対にやってはいけない」と論じた破壊そのもの。**
   `connect.rs:871` の `run_relay_resumable` は
   `connect_via_relay_resumable` → 新規 `ATTACH_HELLO` → **新しい TCP を `sshd` に張る**。
   その新しいストリームは同じ `isekai-pipe connect` プロセスの stdout 経由で、
   **既に SSH セッション中の `ssh(1)`** に流し込まれる。`ssh(1)` から見れば、
   暗号化セッションの途中に突然 `SSH-2.0-OpenSSH_...` のバナーが現れることになり、
   §4.1 が述べた `Corrupted MAC on input` と同じクラスの破壊が起きる。
   **§4.1 の論証（旧 ssh に別セッションのバイトを流し込んではいけない）は、
   §3.2 タスク2 にもそのまま適用される。**

3. **`connect.rs:835-843` のガードは、まさにこれを防ぐために Epic R PR2 Task 2.7 で
   追加されたものである。** ガード自身のコメントが
   「Doing so would silently start a brand-new session over a different transport
   instead of reconnecting the one the user was actually using, and would also
   bypass the `always-connects.md`-mandated recovery path for this signal:
   `isekai-ssh`'s own lightweight-retry loop (`wrapper.rs`/`native/connect.rs`'s
   `RetryConnectLightweight`), which is keyed on seeing this exact
   `ConnectOutcomeClass` unwrapped」と2つの理由を挙げている。
   §3.1（122-126行目）は前者だけを論破しているが、**後者（wrapper の
   lightweight-retry を迂回してしまう）は cross-family resume を導入しても消えない**。
   resume が失敗した以上、正しい次の手は `MidSessionDisconnect` を素通しして
   `wrapper.rs:1007-1010` の `RetryConnectLightweight`（＝`ssh(1)` を作り直して
   新しいセッションを始める）に委ねることであり、これは `always-connects.md` が
   要求する復旧経路そのものである。

**修正案**（§3.2 タスク2 をこう書き換える）:

> 2. `isekai-pipe/src/connect.rs:835-843` の `MidSessionDisconnectSignal` bail-out は
>    **そのまま残す**。cross-family resume はタスク1の位置（`run_resume_loop` 内部、
>    ポンプが生きている間）でのみ行い、そこで失敗した場合は従来通り
>    `MidSessionDisconnect` を素通しして wrapper の `RetryConnectLightweight` に委ねる。
>    `connect.rs` 側で行う変更は「cross-family target を `run_resume_loop` へ
>    渡せるようにする配線」（R2-C2）だけであり、bail-out の判定そのものは変えない。
>    **理由**: この地点まで来たということは連続性が既に失われており、ここで
>    `run_relay_resumable`（新規 ATTACH）を呼ぶと、SSH セッション中の `ssh(1)` に
>    別セッションのハンドシェイクバイトが流れ込む——§4.1 が非目標とした破壊と同型。

さらに §3.1（122-126行目）の「この反論は成立しなくなる」という記述も、
**「fallback を resume-preserving にすれば1つ目の理由は解消するが、2つ目の理由
（wrapper の lightweight-retry を迂回する）は残るため、ガード自体は残す」**
と正確に書き直すこと。現状の書き方だと、実装者は素直にガードを消しにいく。

---

<a id="r2-b2"></a>

### R2-B2. §8 第1項の `BUSY_OTHER_SESSION` は cross-family resume 経路では構造上発生しない

**該当**: §8 第1項（311-317行目）

> - cross-family resume中の`BUSY_OTHER_SESSION`競合:
>   `retry_while_busy_other_session`（`resume_loop.rs:268`）は新規attach
>   経路にしか掛かっていない。cross-family resumeは`reconnect_and_resume`
>   を直接呼ぶためこの保護の外側に出る。

**これは round 1 の私の指摘が不正確で、それがそのまま転記されている。訂正する。**

`BUSY_OTHER_SESSION` は **ATTACH のリジェクト理由**であり、
`isekai_protocol::attach::AttachRejectReason::BusyOtherSession` として
`TransportError::Rejected(...)` に載る（`isekai-transport/src/resume.rs:303-320`
`is_busy_other_session` の実装と docs、および `resume.rs:915` のテストフィクスチャを参照）。

一方 RESUME のリジェクト理由は
`isekai_protocol::resume::ResumeRejectReason` の**3値のみ**——
`Auth` / `UnknownSession` / `OffsetGone`（`isekai-transport/src/resume.rs:693-699`
`map_reject_reason`）。**`BusyOtherSession` は RESUME のワイヤ上に存在しない。**

したがって「cross-family resume が `retry_while_busy_other_session` の保護外に出る」
という懸念は、**そもそも防ぐべき事象が起きないので成立しない**。

しかし**「同じサーバー側状態が別の形で現れる」という本質は残っている**:
サーバー側で `parked_tcp == None`（旧 data stream の reset をまだ処理し終えていない、
または `AttachArbiter` の established lease が一致しない）という状態は、
RESUME 経路では `UnknownSession` に潰されて返ってくる。
これは `resume_loop.rs:979-998` の `is_unknown_session_rejection` の doc コメントが
「`UnknownSession` はサーバー側3状況（本当に消滅／まだ park されていない／fencing
slot 不一致）を同じ `quicmux::ResumeRejectReason::UnknownToken` に潰している」と
明記している通り。

**修正案**: §8 第1項を削除し、その内容を §3.2 タスク6 に統合したうえで、
タスク6 を R2-B6 の内容に書き換える。「`BUSY_OTHER_SESSION` 競合」という
見出しのまま残すと、実装者が存在しないエラーパスのハンドリングを書く。

---

<a id="r2-b3"></a>

### R2-B3. §5「Windows/native のみを対象」はプラットフォーム範囲の記述が逆

**該当**: §5（249-250行目）

> **本ADRはisekai-ssh（Windows/native）のみを対象とし、Androidは対象外とする。**

**これは二重に誤っている。**

1. **本ADRの変更対象は `isekai-pipe connect`（`connect.rs` / `resume_loop.rs`）であり、
   これは Unix でも Windows でも動く同じバイナリの同じコードパスである。**
   むしろ **Unix が主たる受益者**である——Unix では `isekai-ssh` の wrapper が
   `ssh(1)` の `ProxyCommand` として `isekai-pipe connect` を起動する構成が既定。
   「Windows/native のみ」と書くと、Unix が対象外だと読まれてしまう。

2. **`native/` は Windows 専用のモジュールであり、しかも Windows の既定経路ですらない。**
   `ADR_MIDSESSION_DISCONNECT_RECOVERY.md` §2.4 が明記している通り、経路は3つある:
   - **Unix**: `wrapper.rs::run_ssh_with_connect_failure_recovery`（`ssh(1)` を spawn）
   - **Windows 単一プロセス fallback**: `native/connect.rs` の対応関数
   - **Windows mux 経路（既定）**: 新ループを持たず既存 `native::mux::run_with_reconnect` を使う

   `isekai-ssh/src/main.rs` の `mod wrapper;` は `#[cfg]` ゲートされておらず、
   `wrapper.rs` 自体はクロスプラットフォームにビルドされる。

   3経路とも `isekai-pipe connect` を子プロセスとして起動する
   （Windows 側の起動の様子は `resume_loop.rs:1044-1050` の give_up doc コメントが
   「The Windows-native connect path (`native/connect.rs`, `connect_attempt`) ...
   before that child finishes writing its own outcome file」と言及している）。
   したがって **§3 の変更は3経路すべてに等しく効く**。

**修正案**:

> **本ADRは `isekai-pipe connect` を経由するすべての経路（Unix の `ssh(1)` +
> ProxyCommand 経路、Windows の mux 経路・単一プロセス fallback 経路）を対象とし、
> Android（`rust-core/src/isekai_*_transport.rs`、`isekai-pipe connect` を経由しない）
> は対象外とする。**

加えて、**Android が対象外なのは「方針として除外した」のではなく「構造上そもそも
到達しない」**点も書いておくこと。Android は `isekai-pipe connect` プロセスを一切
起動せず、`isekai-transport` を直接 in-process で使う。したがって §3 の変更は
Android のバイナリに何の影響も与えない——後から「フラグを立てれば Android でも
有効化できるのでは」と誤解されないよう、この構造的事実を明記する。

**関連**: §4.1（201-204行目）が「フルSTUN再確立は `wrapper.rs:661
run_ssh_with_connect_failure_recovery` が担当し」と書いているのも **Unix 経路のみ**の話。
Windows mux 経路では `native::mux::run_with_reconnect` が同等の役割を担う。
非目標の論証としては「どの経路であれ SSH セッションそのものが作り直される」点が
本質なので、経路名を1つだけ挙げるのではなく「Unix/Windows いずれの経路でも
SSH セッションが作り直される」と一般化して書くほうが正確。

---

<a id="r2-b4"></a>

### R2-B4. cross-family 切替後に `max_resume_window`（120秒キャップ）を外す記述が無い

**該当**: §3.2 タスク5（154-160行目）および タスク7（170-175行目）

タスク5 は `effective_resume_grace_secs` の引き継ぎに触れ、タスク7 は
`experimental_network_rebind`/warm-standby の扱いに触れているが、
**この2つより重要な `max_resume_window` の扱いが抜けている。**

`run_resume_loop` は `max_resume_window: Option<Duration>` を受け取り、
STUN 経路では `Some(STUN_RESUME_GIVE_UP_WINDOW)` = 120秒
（`resume_loop.rs:370`、`run_stun_p2p_resumable` の呼び出し）、
relay 経路では `None` が渡される。これは
`resume_window_for`（`resume_loop.rs:662`）→ `clamp_resume_window`（669）→
`effective_resume_window`（681）で、サーバー付与の grace と **短い方**が採用される。

**cross-family で relay へ切り替えた後もこの 120秒キャップが残っていると、
relay 経路に移ったセッションが以後ずっと 120秒でギブアップし続ける**——
relay の本来の耐性（サーバー付与 grace、既定10日）を失った、静かな後退になる。
しかもこれは「cross-family resume は成功したのに、その次の切断で妙に早く諦める」
という、原因が極めて追いにくい形で現れる。

さらに `resume_loop.rs:1087` の
`let notify_on_give_up = max_resume_window.is_none();` により、
**OS 通知の有無も `max_resume_window` に連動している**（STUN 経路ではギブアップ時の
OS 通知が抑制される）。切替後にこれも relay 相当へ揃えるかを決める必要がある。

**修正案**: §3.2 に新しいタスクとして明記する。

> N. **cross-family 切替時に `max_resume_window` を `None` へ切り替える**
>    （`resume_loop.rs:370` が STUN 経路に渡している `Some(STUN_RESUME_GIVE_UP_WINDOW)`
>    を、relay へ移った後は解除する）。解除しないと、relay に移ったセッションが
>    以後ずっと 120秒でギブアップし続け、relay 本来の耐性（サーバー付与 grace、
>    既定10日）を失う。`resume_loop.rs:1087` の `notify_on_give_up` も
>    `max_resume_window.is_none()` に連動しているため、通知挙動の変化も同時に確認する。

タスク5（`effective_resume_grace_secs`）とタスク7（rebind/warm-standby）と合わせて、
「**cross-family 切替は『STUN 経路向けのパラメータ束』から『relay 経路向けの
パラメータ束』への切り替えである**」という統一的な捉え方で書き直すと、
抜けが起きにくい。現状は個別項目の羅列なので、実装者が1つ落とす危険がある。

---

<a id="r2-b5"></a>

### R2-B5. §3.2 タスク3 は自己矛盾（「流用せず新規実装」vs「引数は既に用意されている」）

**該当**: §3.2 タスク3（137-145行目）

> 既存の`log_rendezvous_outcome`はrelay専用なので流用せず新規に実装
> するが、`previous_session_id`引数は`telemetry.rs:212-221`のコメント
> （「将来そういう呼び出し元ができたら第2の似た関数を作らずに済む
> よう残してある」）通り、この用途のために既に用意されている。

**新しい関数を作るなら、既存関数の引数が「用意されている」ことは何の役にも立たない。**
しかも引用しているコメントの趣旨は文字通り「**第2の似た関数を作らずに済むよう**」であり、
**「この関数を再利用せよ」という意味**である。つまり ADR の結論（新規実装）は、
その根拠として引いたコメントの意図と逆になっている。

`telemetry.rs:212-221` の原文:

> The parameter still exists (rather than being dropped outright) so a future
> caller that *does* have an earlier session to report (e.g. a bare-redial call
> site) can populate it without a second, near-identical logging function.

**修正案**: `log_rendezvous_outcome` を**再利用**し、`class` に新しい値を追加する。

- `class = "cross-family-resumed"`（`previous_session_id = Some(旧)`,
  `new_session_id = Some(同じ値)` — 連続性が保たれたことを表す）
- `class = "continuity-lost"`（`previous_session_id = Some(旧)`,
  `new_session_id = None` — cross-family resume も失敗し、新セッションへ落ちる）

そのうえで `telemetry.rs:200-228` の doc コメント（現在
「a round runner (`resume::connect_via_relay_resumable_with_fallback`) が
produce する2イベント」と限定して書いている）を、新しい producer を含むよう
更新することをタスクに含める。**この doc を直さないと、round 1 で起きた
「relay 専用の計装を汎用だと誤解する」事故がそのまま再発する。**

**重要**: 新しい class 名は既存の `"abandoned"` / `"fresh-rendezvous"` と
**明確に別の語**にすること。§1.1 でわざわざ「`"abandoned"` は接続失敗であって
連続性喪失ではない」と訂正したのに、§4.2（226行目）で
「"abandoned"（cross-family含めて連続性を喪失したケース）の実頻度」と
**同じ語を新しい意味で再利用してしまっている**（→ R2-D1）。同じ混乱を作らないこと。

---

<a id="r2-b6"></a>

### R2-B6. §3.2 タスク6 の「既存の `UnknownSession` 判定を通す」は単発試行では機能しない

**該当**: §3.2 タスク6（161-169行目）

> この場合の`UnknownSession`拒否は、既存の
> `is_unknown_session_rejection`（`resume_loop.rs:999`）/
> `update_unknown_session_streak`（`resume_loop.rs:1016`）の判定を
> cross-family経路にも通し、「連続性喪失が確定した」シグナルとして
> 正しく扱う（新セッションへフォールバック）。

**既存の判定は「連続試行ループ」を前提に作られており、タスク1が定める
「1回試みる」単発試行では原理的に成立しない。**

`update_unknown_session_streak`（`resume_loop.rs:1016-1023`）は、

```rust
let should_give_up = streak >= UNKNOWN_SESSION_CONFIRM_THRESHOLD
    && elapsed_since_disconnect >= UNKNOWN_SESSION_MIN_ELAPSED_FLOOR;
```

という2条件を要求する。この設計理由は
`ResumeLoopState::consecutive_unknown_session` の doc（`resume_loop.rs:857-871`）と
`is_unknown_session_rejection` の doc（`resume_loop.rs:979-998`）が詳しく書いている通り、
**`UnknownSession` がサーバー側の3状況（本当に消滅／まだ park されていない／
`AttachArbiter` の lease 不一致）を1値に潰しているため、1回では区別できない**から。
Codex レビューで発見され、`engine/mod.rs` の RESUME ハンドラを直接読んで再現条件まで
特定したうえで入った、実バグ由来の設計である。

タスク1 が「1回試みる」と定めている以上、`streak` は常に 1 にしかならず
（`UNKNOWN_SESSION_CONFIRM_THRESHOLD` に到達しない）、**`should_give_up` は永久に
`false`** になる。つまり「既存の判定を通す」だけでは、cross-family resume の
`UnknownSession` は「連続性喪失が確定した」とは決して判定されない。

さらに悪いことに、cross-family resume が発火するのは STUN の 120秒境界に到達した
**後**なので、`UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`（30秒、`resume_loop.rs:975-978`）は
既に満たされている——つまり**閾値のうち時間側だけが自動的に満たされ、回数側だけが
永久に満たされない**という、最も分かりにくい形の不作動になる。

**修正案（どちらかを選ぶ）**:

- **案A（推奨）**: cross-family resume を「1回」ではなく**短い有界リトライ**にする
  （例: 最大 `UNKNOWN_SESSION_CONFIRM_THRESHOLD` 回、合計 10〜30秒程度）。
  こうすれば既存の streak ロジックがそのまま意味を持ち、かつ
  「まだ park されていない」という一時的レースからも回復できる。
  タスク1 の「1回試みる」を書き直すこと。
- **案B**: cross-family resume の `UnknownSession` は単発でも確定扱いにする。
  ただし**この場合は「なぜ STUN 経路では単発判定が危険なのに cross-family では
  安全なのか」を ADR 内で論証する必要がある**（私の見る限り、
  `parked_tcp == None` の一時レースは cross-family でも等しく起こりうるので、
  この論証は成立しにくい）。採らないことを推奨する。

いずれにせよ、**「既存の判定を通す」という現在の書き方は、実装しても動かない**ので
書き直すこと。

---

<a id="section-c"></a>

## C. SHOULD-FIX: 設計として欠けている論点

<a id="r2-c1"></a>

### R2-C1. 「120秒待ってから relay」では救うケースの体験が壊れている — ネットワーク変化シグナルで即座に切り替えるべき

**該当**: §3.2 タスク1（130-134行目）「`STUN_RESUME_GIVE_UP_WINDOW`（120秒）を
使い切ってgive upする**直前**に」

§3.3 が「救うケース（圧倒的多数）」として挙げているのは
**クライアント側アドレスだけが変わった場合（Wi-Fi↔セルラー切替）**である。
この場合、サーバー側 NAT のマッピングは旧クライアントアドレスに紐づいているので、
**STUN の bare redial は最初から最後まで絶望的**（120秒間ずっと無駄に失敗し続ける）。

しかも `reconnect_and_resume`（`isekai-transport/src/resume.rs:733`）は
**毎回まっさらな ephemeral socket から dial する**
（`BindSpec::any_ipv4().with_port_range(...)`）——穴あけ済みのソケットを再利用しない。
つまり STUN 経路の resume は構造的に「サーバー側 NAT が寛容であることを期待する」
賭けになっており、クライアントのアドレスが変わったケースでは期待が外れることが
ほぼ確定している。

**結果として、ユーザーは「動かない端末を 120秒見つめてから、ようやく復旧する」**
という体験になる。現状（120秒後に `ssh(1)` ごと作り直し）より良いのは確かだが、
**救えるはずのケースで 2分の無反応を強いるのは、この ADR の価値を大きく削る。**

**提案**: 120秒を待たず、**ネットワーク経路の変化を検知した時点で即座に
cross-family resume へ切り替える**。既存の仕組みがそのまま使える:

- `spawn_reconnect_signal`（`resume_loop.rs:514`）と
  `wait_backoff_or_network_change`（`resume_loop.rs:816`）が既にネットワーク変化を
  バックオフの中断シグナルとして扱っている。
- **「ネットワーク経路が変わった」＝「クライアント側アドレスが変わった可能性が高い」
  ＝「STUN bare redial は期待薄、relay へ行くべき」** という対応関係が、
  §3.3 の救うケースの定義そのものと一致している。

少なくとも「STUN の bare redial を N 回失敗 **かつ** ネットワーク変化シグナルを
受け取った」時点で cross-family へ切り替える、という条件を検討し、
その判断（即時切替か 120秒待ちか、条件は何か）を ADR に明記すること。
現状の「give up する直前に1回」は、実装としては最も単純だが、
**この ADR が狙っている体験改善を最も損なう選択**である。

---

<a id="r2-c2"></a>

### R2-C2. cross-family target の入手経路（plumbing）が未記述 — 実装者が誤った層に実装する

**該当**: §3.2 タスク1 が「relay targetへ`reconnect_and_resume`」と書いているが、
**その `RelayTarget` がどこから来るのかが一切書かれていない。**

現状の構造:

- `run_resume_loop(factory, target, profile, established, ...)` の `target: &RelayTarget` は、
  STUN 経路では `helper_addr: peer_addr`（STUN 観測ピアアドレス）を持つ
  `RelayTarget`（`connect.rs:791-796` および `resume_loop.rs:418-436` で構築）。
  **`run_resume_loop` は cross-family relay アドレスを知らない。**
- cross-family relay アドレスは `intent.cross_family_fallback`
  （`isekai-pipe-core/src/lib.rs:160`）にあり、これを読んでいるのは
  `connect.rs:822 recover_via_cross_family_fallback` だけ。
- さらに `run_stun_p2p_with_fallback`（`resume_loop.rs:383`）は
  **`ConnectionIntent` を一切受け取らない**——この関数自身のコメント
  （`resume_loop.rs:428-433`）が「`run_stun_p2p_with_fallback` receives no
  `ConnectionIntent`, so this path intentionally has no source for
  `local_bind_port_range`」と、まさに同種の欠落を既に記録している。

つまり **`connect.rs` から `run_resume_loop` まで、新しい引数
（`cross_family_target: Option<RelayTarget>`）を通す配線が必須**。
これを書かないと、実装者は「`run_resume_loop` からは届かないから `connect.rs` 層で
やるしかない」と判断し、**R2-B1 が指摘した誤った層（bail-out の置き換え）へ
流れてしまう。** タスクとして明記すること。

併せて決めておくべき細部:

1. **バリデーションの実行場所**: 現在 `recover_via_cross_family_fallback` は
   `decode_secret`（`connect.rs:857`）と
   `validate_endpoint_identity`（`connect.rs:862-864`）を fallback 発動時に行っている
   （`connect.rs:857-860` のコメント通り、`cross_family_fallback` は
   `TryFrom<&TransportIntent> for CandidateDraft` を通らないので独自の検証点が要る）。
   cross-family resume を `run_resume_loop` 内部でやるなら、**この検証は
   `connect.rs` 側で接続開始前に済ませ、検証済みの `RelayTarget` を渡す**のが正しい
   （失敗時に即座に分かるし、ポンプの最中に検証エラーで詰まらない）。
2. **`local_bind_port_range`**: `intent.local_bind_port_range` を
   cross-family `RelayTarget` にも引き継ぐこと（上記コメントが記録している既存の
   欠落を、新経路で繰り返さない）。
3. **`intent.cross_family_fallback` が `None` のとき**: `select_transport`
   （`wrapper.rs:1305-1316`）は STUN primary のときだけ fallback を返す設計なので、
   STUN 経路なら基本的に `Some` のはず。ただし `legacy_relay_transport` を持たない
   プロファイル（`profile.rs:168-177` が `None` を返すケース）では `None` になりうる。
   その場合は cross-family resume を単にスキップして従来通りギブアップする、
   と明記すること。

---

<a id="r2-c3"></a>

### R2-C3. §3.3 の価値は「`cached_relay_addr` が新しいネットワークから到達可能」という未検証の仮定に依存している

**該当**: §3.3（182-187行目）

> **サーバー自身のアドレスは変わっていないのでrelay経路は生きている**

**この一文が本 ADR の価値のほぼ全てを支えているが、暗黙の仮定が含まれている。**

`cross_family_fallback` の `helper_addr` は
`PersistentProfile::legacy_relay_transport.helper_addr`、由来は
`HelperTrust::cached_relay_addr`（`isekai-pipe-core/src/profile.rs:149-152`）。
これは **ブートストラップ時にサーバーが自己申告した到達先アドレス**である。

「サーバーのアドレスが変わっていない」ことは確かだが、
**「そのアドレスがクライアントの*新しい*ネットワークから到達可能である」ことは
別の命題**である。特にこのプロジェクトは Tailscale を前提の1つに置いている
（`CLAUDE.md` の「Tailscale⇔直接アドレスのマルチパス」）。

具体的に成立しないシナリオ:

- `cached_relay_addr` が **Tailscale の tailnet アドレス**（`100.x.x.x`）で、
  クライアントが Wi-Fi→セルラーへ切り替えた際に tailnet への接続が一時的に落ちている。
- `cached_relay_addr` が **LAN 内アドレス**で、クライアントが同じ LAN から
  出た瞬間に到達不能になる。この場合、まさに「アドレスが変わった」というトリガ自体が
  「relay も同時に到達不能になった」ことを意味する。
- `cached_relay_addr` がグローバルアドレスでも、新しいネットワーク側のファイアウォール
  ポリシーが当該 UDP ポートを塞いでいる。

つまり **「クライアント側アドレスだけが変わった」ケースの中に、
「relay も同時に使えなくなる」部分集合が含まれている**。§3.3 が
「圧倒的多数」と断じている割合は、この部分集合の大きさに依存しており、未検証。

**修正案**: §3.3 に仮定として明記する。

> **前提（未検証、計装で確認する）**: cross-family fallback の価値は
> 「`cached_relay_addr` がクライアントの*新しい*ネットワークから到達可能である」
> ことに依存する。`cached_relay_addr` が tailnet アドレスや LAN アドレスの場合、
> クライアントのネットワークが変わった瞬間に relay も同時に到達不能になりうる。
> §3.2 タスク3 の計装では、cross-family resume の失敗を
> 「セッション消滅（`UnknownSession`）」と「relay アドレスへ到達できない
> （ネットワーク/mux エラー）」に**分けて記録**し、この仮定を検証する。

この「失敗理由を2種類に分けて記録する」点は計装タスクの要件として重要——
一括りに「失敗」とだけ記録すると、上記の仮定が崩れていても気づけない。

なお、プロファイルには `relay_endpoints`（複数）・`link_endpoints`・`rendezvous` という
より新しいフィールドも存在する（`profile.rs:141-145`）。
`legacy_relay_transport` は移行ブリッジであり、将来これらへ移ると
cross-family fallback の候補が複数になりうる。ADR にこの将来像を1行触れておくと、
「なぜ単一アドレスなのか」が後で疑問にならない。

---

<a id="r2-c4"></a>

### R2-C4. §3.2 タスク4（preempt ping-pong ラッチ）は駆動主体が1つしかないため前提が成立していない可能性

**該当**: §3.2 タスク4（146-153行目）

> STUN側が再接続してpreemptを撃ち、直後にrelay側がpreemptを撃ち返す
> ping-pongが原理的に起こりうる。**クライアント側で「cross-familyへ
> 切り替えたら旧ファミリの再試行を止める」ラッチを持つこと。**

**round 1 で私が提起した懸念だが、改訂版を読んで再検討した結果、
記述されたシナリオは単一プロセス内では成立しない可能性が高い。**

ping-pong が起きるには**独立した2つの再接続駆動主体**が必要である。しかし:

- cross-family resume はタスク1 の通り `run_resume_loop` 内部で行われる。
  この関数内の再接続ループ（`resume_with_backoff_until_deadline`）は **1つだけ**で、
  逐次実行される。STUN 側と relay 側が**同時に**resume を撃つ構造になっていない。
- そもそも cross-family resume が発火する時点で、STUN 側のポンプは既に失敗しており
  （それが resume ループに入った理由）、クライアントから見て旧 STUN 接続は死んでいる。
  ゾンビとして生きているのは**サーバー側から見た場合だけ**で、その状況は
  `handle_resume_stream` の preempt 待ち（`engine/mod.rs:72` のタイムアウト定数が
  「How long `handle_resume_stream` waits, after asking an in-flight relay to...」と
  説明している）で**有界に解決される**。

**本当に 2 駆動主体になるのは、wrapper の `RetryConnectLightweight` が
新しい `isekai-pipe connect` プロセスを起動し、それが旧プロセスの parked セッションと
競合する場合**だが、これは既知の問題として `retry_while_busy_other_session`
（`resume_loop.rs:383-400` の Task 2.11 コメント）で既に対処されている領域である。

**修正案**: タスク4 を「ラッチを持つこと」という断定から、
**「2駆動主体が実在するかをまず確認し、実在する場合のみラッチを入れる」**に
格下げする。これは `ADR_MIDSESSION_DISCONNECT_RECOVERY.md` §2.2.2 の S1 対応
（孫プロセス孤児化、「実装時にまず実測してから機構の要否を決める」）と同じ進め方であり、
このリポジトリに前例がある。存在しない競合のためにラッチという状態を1つ増やすのは、
`rust-ssot.md` が戒めている「判断のためのミラー状態を増やす」方向でもある。

ただし**サーバー側 preempt 待ちの時間だけは cross-family resume のレイテンシに
直接乗る**ので、タスク1 のタイムアウト設計（R2-C1 の切替タイミングとも関係）では
これを見込むこと——この点はタスクとして残す価値がある。

---

<a id="r2-c5"></a>

### R2-C5. §6 の成功基準は分母・目標値・読み出し手段が未定義で、このままでは測れない

**該当**: §6（254-258行目）

> STUN経路でmid-session切断が起きたとき、`ssh(1)`を再起動せずに
> （cross-family resumeで）復旧した割合を、§3.2タスク3の検知計装で
> 実運用ログから測れるようにする。

3点欠けている。

1. **分母が曖昧**。「mid-session 切断が起きたとき」だが、bare redial で 120秒以内に
   復旧した切断（＝そもそも cross-family が発火しない、既に成功しているケース）を
   分母に含めるのか否かで意味が全く変わる。含めると、STUN が元気な環境では
   分母が膨らんで割合が意味をなさない。**分母は「give-up 境界に到達した回数
   （＝cross-family resume が試みられた回数）」とするのが妥当**で、そう明記すること。
2. **目標値が無い**。「測れるようにする」は計測手段の話であって成功基準ではない。
   「give-up 境界に到達したケースの N% 以上で `ssh(1)` を再起動せずに復旧する」
   という形の閾値を置くこと（R2-C3 の仮定が崩れていればここが伸びない、という形で
   仮定の検証にもなる）。
3. **読み出し手段が無い**。これらは `log::info!` の行であり、集計基盤は無い
   （`--isekai-log-file` に落ちるだけ）。**「手動でのログ検分による評価であり、
   メトリクスパイプラインは作らない」と正直に書く**か、簡単な集計手段
   （`isekai-ssh doctor` への件数表示など、§3.2 タスク8 と合わせて検討）を
   タスクに含めるか、どちらかを決めること。書かないと「測れるようにした」つもりで
   誰も測らない状態になる。

---

<a id="section-d"></a>

## D. NICE-TO-HAVE: 軽微な不整合・体裁

<a id="r2-d1"></a>

- **R2-D1. §4.2 の `"abandoned"` 再利用と、参照先 `（§7）` の誤り**（226-227行目）
  > §3の計装（3.2タスク3）で "abandoned"（cross-family含めて連続性を喪失したケース）の
  > 実頻度が判明してから再評価する（§7）。

  2つ問題がある。(1) `"abandoned"` は §1.1 でわざわざ「接続失敗を意味し、連続性喪失では
  ない」と訂正した語であり、ここで新しい意味に再利用すると round 1 で起きた誤解が
  そのまま再発する（→ R2-B5 の新 class 名を使うこと）。(2) 参照先の `（§7）` は
  `ADR_INPUT_RESUME_SYMMETRY.md` との関係を述べる節であり、再評価の話は書かれていない。
  `§9`（次のステップ）を指すべき、あるいは §9 に「§4.2 の再評価トリガ」を追記する。

- **R2-D2. ヘッダの「対象」に入力キューが混ざっている**（8-9行目）
  > `resume_loop.rs`（give-up境界の直前処理・**入力キューの置き場所**）

  入力キューの実装は `ADR_INPUT_RESUME_SYMMETRY.md` の担当であり、本 ADR は
  「置き場所を決める」だけ（§7.1）。本 ADR の「対象（変更するファイル）」に含めると
  所有権が曖昧になる。「§7.1 で置き場所を決めるが、実装は別 ADR」と明記するか、
  対象から外すこと。

- **R2-D3. §3.3 の NAT 説明に、client 側が毎回新ソケットから dial する事実を補う**

  「restricted-cone NAT は新しいクライアントアドレスからのパケットを落とす」は正しいが、
  より根本的な事実として **`reconnect_and_resume` は穴あけ済みソケットを再利用せず
  毎回新しい ephemeral socket から dial する**（`isekai-transport/src/resume.rs:733`）。
  つまり STUN 経路の resume は、クライアントのアドレスが変わっていなくても
  「サーバー側マッピングが full-cone 相当に寛容であること」に賭けている。
  この1行があると、なぜ 120秒が「望み薄な賭けの待ち時間」なのか（R2-C1）が
  実装者にすぐ伝わる。

- **R2-D4. §4.1 の経路名を一般化する**（R2-B3 の関連）

  `wrapper.rs:661` は Unix 経路のみ。「Unix/Windows いずれの経路でも SSH セッション
  そのものが作り直される」と一般化して書くこと。

---

<a id="section-e"></a>

## E. `ADR_INPUT_RESUME_SYMMETRY.md` 側の確認結果

**結論: §7.1/§7.2 の内容が正しく、かつ矛盾なく反映されている。** 具体的には:

- **論点1（キューの置き場所）**（53-64行目）: 結論・理由・スコープ（session_id）・
  相互参照がすべて正しく転記されている。「cross-family resume 導入後は
  『同一トランスポートファミリ内の再接続』ではなく『同一 session_id での再接続、
  ファミリ不問』が flush 境界になる」という核心も落ちていない。**問題なし。**
- **論点3（危険なUX）**（69-80行目）: リスク予算が変わること、決定順序が逆だと
  やり直しになることが正しく書かれている。**問題なし。**
- **論点4（既存 ReplayBuffer との役割分離）**（81-88行目）: flush 順序の固定
  （先に未ACK再送、次に未送信flush）まで書けている。**問題なし。**
- **論点5（本ADRとの関係）**（89-99行目）: 前提が更新されたこと、救えるケースと
  救えないケースの切り分けが正確。**問題なし。**
- **§4 次のステップ**（101-106行目）: 「本ADR §3 の実装・確定を待ってから」という
  順序が明記されている。**問題なし。**

**ただし2点、整合を取り直す必要がある**:

1. **ヘッダの「対象」が更新されていない**（5-7行目）:
   > **対象**（見込み、要精査）: `rust-core/isekai-transport`、
   > `rust-core/isekai-ssh/src/session.rs`（`Session::send`）、必要なら `rust-core/isekai-pipe-core`

   §3 論点1 で「`isekai-pipe/src/resume_loop.rs` 内、`C2hReplayBuffer` の隣」と
   結論が出たのに、**ヘッダの対象には `isekai-pipe` が入っておらず、代わりに
   結論で明確に否定された `isekai-transport` と `isekai-ssh/src/session.rs` が
   残っている**。本文と見出しが食い違っている状態なので、ヘッダを
   `rust-core/isekai-pipe/src/resume_loop.rs` 中心に更新すること。

2. **Status 行が更新されていない**（3-4行目）:
   > **Status**: **Draft**（2026-09-07起草。設計相談・opus-adversarial-consultは未実施。）

   §3 の論点1・4 は本 ADR の round 1 レビューを経て**既に結論が出ている**。
   「設計相談は未実施」は現状と食い違う。「論点1・4 は
   `ADR_STUN_REESTABLISH_CONTINUITY.md` round 1 レビュー経由で決着済み、
   論点2・3 は同 ADR §3 の確定待ち」と書き分けること。

---

<a id="section-f"></a>

## F. 結論と、Approved までに必要な差分

### 判定

**Approved にはまだ進めない。** ただし残件はすべて本文の書き直しで解消し、
新たな調査や設計判断のやり直しは不要。方針（§2、§3.1、§4、§5 の判断、§7）は正しい。

### Approved に必要な最小限の差分（MUST-FIX、6件）

1. **R2-B1**: §3.2 タスク2 を「bail-out を置き換える」から**「bail-out は残す」**へ書き直す。
   §3.1（122-126行目）の「この反論は成立しなくなる」も
   「1つ目の理由は解消するが、wrapper の lightweight-retry を迂回するという
   2つ目の理由は残るのでガード自体は残す」に修正。
   **これを直さないと、Epic R PR2 Task 2.7 が塞いだバグと、§4.1 が非目標とした
   破壊を同時に再導入する。**
2. **R2-B2**: §8 第1項（`BUSY_OTHER_SESSION`）を削除し、§3.2 タスク6 に統合。
   RESUME のリジェクト理由は `Auth`/`UnknownSession`/`OffsetGone` の3値のみで、
   `BusyOtherSession` はワイヤ上に存在しない。
3. **R2-B3**: §5 の「isekai-ssh（Windows/native）のみを対象」を
   「`isekai-pipe connect` を経由する全経路（Unix + Windows mux + Windows 単一プロセス
   fallback）を対象、Android は構造上到達しないため対象外」へ修正。
   §4.1 の経路名も一般化（R2-D4）。
4. **R2-B4**: §3.2 に `max_resume_window` を `None` へ切り替えるタスクを追加。
   タスク5/7 と合わせて「STUN 向けパラメータ束 → relay 向けパラメータ束の切替」として
   まとめて記述する。
5. **R2-B5**: §3.2 タスク3 を「`log_rendezvous_outcome` を**再利用**し、
   新しい class 値（`"cross-family-resumed"` / `"continuity-lost"`）を足す。
   併せて `telemetry.rs:200-228` の doc を新 producer を含むよう更新する」へ修正。
   §4.2 の `"abandoned"` 再利用も新 class 名へ（R2-D1）。
6. **R2-B6**: §3.2 タスク6 を「既存判定を通す」から
   「cross-family resume を短い有界リトライにし、streak ロジックが機能する形にする」へ修正
   （案A推奨）。併せてタスク1 の「1回試みる」も書き直す。

### Approved 前に判断を書き込むべき項目（SHOULD-FIX、5件）

7. **R2-C1**: cross-family へ切り替えるトリガを「120秒使い切った直前」のままにするか、
   **ネットワーク変化シグナル（`spawn_reconnect_signal`/`wait_backoff_or_network_change`）で
   前倒しするか**を決めて明記する。現状のままだと、この ADR が救うと謳っている
   まさにそのケースで、ユーザーが2分間無反応な端末を見ることになる。
   **体験上の価値に直結するので、SHOULD-FIX の中では最優先。**
8. **R2-C2**: cross-family target の配線（`connect.rs` → `run_resume_loop` への
   新引数、検証の実行場所、`local_bind_port_range` の引き継ぎ、`None` のときの挙動）を
   タスクとして明記する。書かないと実装者が R2-B1 の誤った層へ流れる。
9. **R2-C3**: 「`cached_relay_addr` が新ネットワークから到達可能」という仮定を
   §3.3 に明記し、計装で**失敗理由を「セッション消滅」と「relay 到達不能」に
   分けて記録する**ことをタスク3 の要件に加える。
10. **R2-C4**: タスク4（preempt ラッチ）を「まず 2駆動主体が実在するか確認する」へ格下げ
    （`ADR_MIDSESSION_DISCONNECT_RECOVERY.md` §2.2.2 S1 と同じ進め方）。
    サーバー側 preempt 待ちがレイテンシに乗る点だけはタスクとして残す。
11. **R2-C5**: §6 の成功基準に分母（give-up 境界到達回数）・目標値・読み出し手段を書く。

### 付随（`ADR_INPUT_RESUME_SYMMETRY.md` 側）

12. ヘッダの「対象」を `isekai-pipe/src/resume_loop.rs` 中心へ更新（現状、本文の結論で
    否定された `isekai-transport`/`isekai-ssh/src/session.rs` が残っている）。
13. Status 行を「論点1・4 は決着済み、論点2・3 は `ADR_STUN_REESTABLISH_CONTINUITY.md`
    §3 の確定待ち」に書き分ける。

### round 3 の要否

MUST-FIX 6件のうち **R2-B1 と R2-B6 は設計の形を変える**（前者は変更箇所の
範囲が変わり、後者は「1回」が「有界リトライ」に変わる）ため、
**書き直し後に round 3 を1回挟むことを推奨する**。
残り4件（R2-B2/B3/B4/B5）と SHOULD-FIX は文言・タスク追加レベルなので、
round 3 では R2-B1/B6 の書き直し結果と、R2-C1 の判断（切替トリガ）に絞って
確認すれば足りる。

---

## 参照した実装箇所（round 2 で新たに確認したもの）

- `rust-core/isekai-pipe/src/resume_loop.rs`
  — `STUN_RESUME_GIVE_UP_WINDOW` 68、`spawn_reconnect_signal` 514、
  `replay_and_advance` 594、`resume_window_for` 662 / `clamp_resume_window` 669 /
  `effective_resume_window` 681、`wait_backoff_or_network_change` 816、
  `ResumeLoopState` 843-872（`consecutive_unknown_session` の doc 857-871）、
  `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR` 975-978、
  `is_unknown_session_rejection` の doc 979-1003、
  `update_unknown_session_streak` 1006-1023、
  `give_up` の doc（Windows native 経路への言及）1026-1050、
  `notify_on_give_up = max_resume_window.is_none()` 1087、
  `run_stun_p2p_resumable` の `Some(STUN_RESUME_GIVE_UP_WINDOW)` 370、
  `run_stun_p2p_with_fallback` が `ConnectionIntent` を受け取らない旨 428-433
- `rust-core/isekai-pipe/src/connect.rs`
  — STUN 経路の `RelayTarget` 構築 791-796、`recover_via_cross_family_fallback` 822、
  bail-out とその2つの理由 828-843、`decode_secret` 857、
  `validate_endpoint_identity` 862-864、`run_relay_resumable` 呼び出し 871
- `rust-core/isekai-transport/src/resume.rs`
  — `is_busy_other_session`（ATTACH 専用である根拠）303-320、
  `map_reject_reason`（RESUME のリジェクト理由が3値のみ）693-699、
  `reconnect_and_resume` が毎回新 ephemeral socket から dial する 733、
  `finish_via_resume` の `effective_resume_grace_secs` の罠 634-648
- `rust-core/isekai-transport/src/telemetry.rs`
  — `log_rendezvous_outcome` の doc 200-228（特に `previous_session_id` を
  残した理由 212-221）
- `rust-core/isekai-pipe/src/engine/mod.rs`
  — `handle_resume_stream` の preempt 待ちタイムアウト定数 72、
  `FRAME_RESUME` 分岐 962、`handle_resume_stream` 1341
- `rust-core/isekai-pipe-core/src/profile.rs`
  — `relay_endpoints`/`link_endpoints`/`rendezvous` 141-145、
  `cached_relay_addr`/`cached_session_secret` 149-152、
  `to_legacy_relay_transport` 168-177
- `rust-core/isekai-ssh/src/main.rs`
  — `mod wrapper;`（`#[cfg]` ゲートされていない）25行目付近
- `ADR_MIDSESSION_DISCONNECT_RECOVERY.md`
  — §2.4（Unix / Windows 単一プロセス fallback / Windows mux 既定 の3経路）、
  §2.2.2 S1（「実測してから機構の要否を決める」の前例）
