# ADR_STUN_REESTABLISH_CONTINUITY.md 批判的レビュー（round 3）

- **対象**: `/home/cuzic/isekai-terminal/ADR_STUN_REESTABLISH_CONTINUITY.md`（Status: Draft rev3）
  および `/home/cuzic/isekai-terminal/ADR_INPUT_RESUME_SYMMETRY.md`（ヘッダ更新分）
- **レビュー日**: 2026-09-13
- **前回**: `ADR_STUN_REESTABLISH_CONTINUITY_REVIEW.md`（round 1）、
  `ADR_STUN_REESTABLISH_CONTINUITY_REVIEW_ROUND2.md`（round 2）
- **レビュー種別**: 実装着手前の設計方針レビュー（読み取り・分析のみ。コードは一切変更していない）

---

## 目次

- [総評 — Approved に進めてよいか](#verdict)
- [A. round 2 指摘（MUST-FIX 6 / SHOULD-FIX 5 / NICE 4 / 入力ADR 2）の反映状況](#section-a)
- [B. MUST-FIX: rev3 で新たに生じた矛盾・そのままでは動かない記述](#section-b)
  - [R3-B1. タスク4の有界リトライは、タスク1の前倒し切替と組み合わさると `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR` 側で再び不発になる](#r3-b1)
  - [R3-B2. streak は「UnknownSession 以外のエラー」でゼロリセットされる — 切替は一方向でなければタスク4が機能しない](#r3-b2)
  - [R3-B3. タスク1のトリガー情報は現状の API では取り出せない（`wait_backoff_or_network_change` は勝者を返さない）](#r3-b3)
  - [R3-B4. タスク1の条件が論理積のため、netmon が無言の環境では「120秒待ち」に静かに退行する](#r3-b4)
- [C. SHOULD-FIX](#section-c)
  - [R3-C1. §3.1 の「理由(a)は解消する」は無条件に読めてしまい、R2-B1 を再発させうる](#r3-c1)
  - [R3-C2. 配線の深さとパラメータ束切替の実装形が、実際のコード構造と1段ずれている](#r3-c2)
  - [R3-C3. 「netmon は STUN 経路でも生きている」ことを ADR に明記すべき（書かないとタスク1が放棄される）](#r3-c3)
  - [R3-C4. §6 の 80% は、§3.3 の未検証仮定と同じ観測期間で評価すると誤読を招く](#r3-c4)
  - [R3-C5. §4.2 のケースが `always-connects.md` 違反にならないことの確認が抜けている](#r3-c5)
- [D. NICE-TO-HAVE](#section-d)
- [E. `ADR_INPUT_RESUME_SYMMETRY.md` ヘッダ更新の確認](#section-e)
- [F. 結論 — Approved までに必要な差分と round 4 の要否](#section-f)

---

<a id="verdict"></a>

## 総評 — Approved に進めてよいか

**あと一歩。MUST-FIX 4件を直せば Approved に進んでよい。round 4 は不要と考える**
（理由は §F 末尾）。

round 2 の MUST-FIX 6件・SHOULD-FIX 5件・NICE 4件・入力ADR 2件は、
**17件すべてが意図通りに反映されている**（§A の表を参照）。特に重点確認を依頼された2件:

- **R2-B1（bail-out 維持）**: §3.2 タスク3 が「残す（変更しない）」に書き直され、
  §3.1 に理由(a)/(b)の分解が入り、タスク3 の末尾に「置き換えたり削除したりしてはいけない」
  という禁止形まで入った。**意図通り**（ただし §3.1 の一文に読み違いを招く余地が残る
  → R3-C1）。
- **R2-B6（有界リトライ）**: §3.2 タスク4 が単発試行を撤回し、`UNKNOWN_SESSION_CONFIRM_THRESHOLD`
  を明示した有界リトライになった。**意図通り**。ただし rev3 で同時に入った
  タスク1（前倒し切替）との相互作用で、**別の conjunct（`UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`）
  側から同じ不発が再発する**（→ R3-B1）。これは round 2 の指摘を正しく反映した結果、
  新しい組み合わせで生じたもので、rev3 固有の新規問題。

新たに見つかった MUST-FIX 4件（R3-B1〜B4）は、**いずれも「§3.2 のタスク本文に
1〜2文足せば解消する」種類**で、設計の形は変わらない。方針（§2/§3.1/§3.3/§4/§5/§6/§7）は
rev3 で技術的に正しい状態になっている。

---

<a id="section-a"></a>

## A. round 2 指摘の反映状況

| round 2 項目 | 反映先 | 判定 |
|---|---|---|
| **R2-B1** bail-out は残す | §3.2 タスク3（175-184行）、§3.1（132-142行） | **反映済み**。禁止形（「置き換えたり削除したりしてはいけない」）まで入っており、実装者が誤る余地は大きく減った。§3.1 の一文のみ精度不足 → R3-C1 |
| **R2-B2** `BUSY_OTHER_SESSION` は RESUME に存在しない | §3.2 タスク6 の括弧書き（223-227行）、旧§8 第1項は削除 | **反映済み**。`map_reject_reason`（`resume.rs:693-699`）の引用も正確。3値のみという記述も正しい |
| **R2-B3** プラットフォーム範囲 | §5（334-374行）、§4.1（303-308行） | **反映済み**。3経路の列挙が `ADR_MIDSESSION_DISCONNECT_RECOVERY.md` §2.4 と一致。「方針として除外したのではなく構造上到達しない」という言い回しも入った。§4.1 の経路名も一般化済み |
| **R2-B4** `max_resume_window` を `None` へ | §3.2 タスク7（228-248行） | **反映済み**。`notify_on_give_up` 連動（`resume_loop.rs:1087`）まで含めて「パラメータ束の切替」としてまとめられている。実装形だけ1段ずれ → R3-C2 |
| **R2-B5** `log_rendezvous_outcome` 再利用 | §3.2 タスク5（196-217行）、§4.2（328-330行） | **反映済み**。新 class 値2つ、失敗理由の2分類、`telemetry.rs:200-228` の doc 更新まで入った。`"abandoned"` の再利用も `"continuity-lost"` へ修正済み |
| **R2-B6** 有界リトライ | §3.2 タスク4（185-195行）、タスク1（146-158行、「1回試みる」撤回） | **反映済み**。ただし新しい不発経路が発生 → R3-B1 |
| **R2-C1** 前倒し切替 | §3.2 タスク1（146-158行） | **反映済み**。「120秒待ってから1回をデフォルト実装にしない」がタスク要件として明記された。条件の成立性に問題 → R3-B3、R3-B4 |
| **R2-C2** 配線 | §3.2 タスク2（159-174行） | **反映済み**。検証の実行場所・`local_bind_port_range`・`None` ケースの3点すべて記載。深さが1段ずれ → R3-C2 |
| **R2-C3** `cached_relay_addr` 到達可能性の仮定 | §3.3（278-291行）、§3.2 タスク5（208-213行） | **反映済み**。Tailscale/LAN の具体例も、失敗理由の2分類要件も、`relay_endpoints` 等への将来の移行も入った |
| **R2-C4** preempt ラッチを実測へ格下げ | §3.2 タスク8（249-260行） | **反映済み**。`ADR_MIDSESSION_DISCONNECT_RECOVERY.md` §2.2.2 S1 の前例参照も入った。サーバー側 preempt 待ちのレイテンシも残っている。**ただし別の理由で「一方向の切替」が必要** → R3-B2 |
| **R2-C5** 成功基準 | §6（376-392行） | **反映済み**。分母・目標値（暫定80%、再較正前提）・読み出し手段（手動検分、集計基盤は作らない）の3点すべて記載。評価タイミングのみ懸念 → R3-C4 |
| **R2-D1** `"abandoned"` 再利用と `（§7）` 誤参照 | §4.2（328-330行） | **反映済み**。`"continuity-lost"` へ、参照も `（§9）` へ修正済み |
| **R2-D2** ヘッダに入力キューが混在 | ヘッダ（14-16行） | **反映済み**。「置き場所は本ADR§7.1で決定するが、実装自体は`ADR_INPUT_RESUME_SYMMETRY.md`の担当（本ADRの変更範囲には含めない）」と明記 |
| **R2-D3** 毎回新 ephemeral socket から dial | §3.1 末尾（116-121行） | **反映済み**。`resume.rs:733` の引用も正確 |
| **R2-D4** §4.1 の経路名一般化 | §4.1（303-308行） | **反映済み** |
| **入力ADR ①** ヘッダの対象更新 | 入力ADR 7-11行 | **反映済み**。`isekai-pipe/src/resume_loop.rs` 中心へ。当初案を採用しなかった理由（Android 経路との無用な共有回避）まで書かれている |
| **入力ADR ②** Status 書き分け | 入力ADR 3-6行 | **反映済み**。「論点1・4 は決着済み、論点2・3 は同ADR§3の確定待ちで未決」 |

---

<a id="section-b"></a>

## B. MUST-FIX: rev3 で新たに生じた矛盾・そのままでは動かない記述

<a id="r3-b1"></a>

### R3-B1. タスク4の有界リトライは、タスク1の前倒し切替と組み合わさると `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR` 側で再び不発になる

**該当**: §3.2 タスク4（185-195行）と タスク1（146-158行）の組み合わせ

タスク4 は round 2 の R2-B6 を受けて
「最大 `UNKNOWN_SESSION_CONFIRM_THRESHOLD` 回、合計10〜30秒程度」の有界リトライに
書き直された。これは `streak >= UNKNOWN_SESSION_CONFIRM_THRESHOLD` という conjunct を
満たすための修正として正しい。

**しかし `update_unknown_session_streak` の give-up 条件は conjunct が2つある**
（`rust-core/isekai-pipe/src/resume_loop.rs:1021`）:

```rust
let should_give_up = streak >= UNKNOWN_SESSION_CONFIRM_THRESHOLD
    && elapsed_since_disconnect >= UNKNOWN_SESSION_MIN_ELAPSED_FLOOR;
```

実際の値は
- `UNKNOWN_SESSION_CONFIRM_THRESHOLD = 3`（`resume_loop.rs:959`）
- `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR = 30秒`（`resume_loop.rs:978`）

そして `elapsed_since_disconnect` は**切断時刻からの経過時間**であって、
cross-family resume を始めてからの経過時間ではない。

ここで **タスク1 が「120秒待たずに前倒しで切り替える」ことをタスク要件にした**ため、
cross-family resume は切断から数秒〜十数秒で始まりうる。
仮に切断から 5秒後にネットワーク変化シグナルで切り替え、タスク4 の有界リトライが
10秒で 3回試行して終わったとすると:

- `streak = 3` → 1つ目の conjunct は満たす
- `elapsed_since_disconnect = 15秒 < 30秒` → **2つ目の conjunct を満たさない**
- → `should_give_up = false` → **「連続性喪失が確定」と判定されない**

**round 2 の R2-B6 が指摘した不発が、別の conjunct 側から再発する。**
しかも今度は「回数側だけ満たされ、時間側だけ永久に満たされない」という、
R2-B6 の裏返しの形になる。

さらに悪いことに、タスク5 の計装は
「`"continuity-lost"`（`UnknownSession`、タスク4の streak が閾値到達）」を
**streak 判定の成立を前提に記録する設計**になっている（§3.2 タスク5、208-209行）。
この判定が発火しないと、**§6 の成功基準の分子・分母も、§3.3 の仮定検証も、
すべて計測できなくなる**——rev3 で新たに積み上げた仕組みが連鎖的に無効化される。

**修正案（§3.2 タスク4 に1文追加する）**:

> cross-family resume の有界リトライ窓は、回数（`UNKNOWN_SESSION_CONFIRM_THRESHOLD` = 3）
> だけでなく、**`disconnected_at + UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`（切断から30秒、
> `resume_loop.rs:978`）に到達するまで**は終わらせないこと。
> `update_unknown_session_streak`（`resume_loop.rs:1021`）の give-up 条件は
> 回数と経過時間の**両方**を要求するため、タスク1の前倒し切替（切断から数秒で開始しうる）
> と組み合わせると、回数だけ満たして時間を満たさず判定が永久に発火しない。
> 「前倒しで切り替えること」と「切断から30秒は確定判定を出さないこと」は両立する
> （早く relay を試し始めるが、確定を宣言するのは30秒後）。

この「早く試すが、諦めるのは早くしない」という整理は、
`UNKNOWN_SESSION_MIN_ELAPSED_FLOOR` の doc（`resume_loop.rs:962-978`、
「30s is double the idle-timeout default, leaving margin without meaningfully
eating into the deadline」）の趣旨とも整合する。

---

<a id="r3-b2"></a>

### R3-B2. streak は「UnknownSession 以外のエラー」でゼロリセットされる — 切替は一方向でなければタスク4が機能しない

**該当**: §3.2 タスク4（185-195行）と タスク8（249-260行）の相互作用

`update_unknown_session_streak`（`resume_loop.rs:1016-1023`）の冒頭:

```rust
if !is_unknown_session {
    return (0, false);
}
```

**`UnknownSession` 以外のあらゆるエラー（ネットワーク/mux エラーを含む）で streak は 0 に戻る。**
これは元の設計（単一 target への連続試行）では正しい——「別の理由で失敗した」なら
`UnknownSession` の連続性は途切れているから。

ところが cross-family resume を導入すると、**2つの target への試行が同じ
`state.consecutive_unknown_session`（`resume_loop.rs:872`）を共有する**。
もし実装が「STUN target への試行と relay target への試行を交互に行う」形になると:

1. relay 試行 → `UnknownSession` → streak = 1
2. STUN 試行 → ネットワークエラー（クライアントのアドレスが変わっているので当然失敗）→ **streak = 0**
3. relay 試行 → `UnknownSession` → streak = 1
4. …以下ループ

**streak は永久に 3 に到達せず、タスク4 の有界リトライは意味をなさない。**

つまり **「一度 cross-family へ切り替えたら STUN 側の試行には戻らない」という
一方向（one-way）の切替が、正しさのために必要**である。

ここが重要なのだが、**これは round 2 の R2-C4 でタスク8 に格下げした
「preempt ping-pong 対策のラッチ」とは別物**である:

- タスク8 が格下げしたのは「サーバー側 preempt の撃ち合いを防ぐためのラッチ」——
  これは round 2 の再検討通り、単一プロセス内では駆動主体が1つなので不要かもしれず、
  「実測してから決める」で正しい。
- **R3-B2 が要求しているのは「streak カウンタの意味を保つための、target 切替の
  一方向性」**——これは実測を待つ必要がなく、コードを読めば必要性が確定する。

なお **`consecutive_unknown_session` を target 別にスコープするのは誤り**である:
`UnknownSession` は「その `session_id` をサーバーが知らない」という
**セッションについての主張**であって、アドレスについての主張ではない
（`is_unknown_session_rejection` の doc、`resume_loop.rs:979-998` が
「`sessions.get()` が `None` / `parked_tcp` が `None` / `AttachArbiter` の lease 不一致」の
3状況を潰していると説明している通り、いずれも session の状態）。
したがってカウンタはセッション単位のままでよく、**直すべきは「交互に試さない」ほう**。

**修正案（§3.2 タスク4 に1文追加、§3.2 タスク8 に注記）**:

> タスク4: cross-family resume へ切り替えたら、**同じ episode 内で STUN target への
> 試行には戻らない（一方向の切替）**。`update_unknown_session_streak`
> （`resume_loop.rs:1016-1019`）は `UnknownSession` 以外のエラーで streak を 0 に戻すため、
> 2つの target へ交互に試行すると streak が毎回リセットされ、閾値に到達しなくなる。
> `state.consecutive_unknown_session`（`resume_loop.rs:872`）をセッション単位で持つこと自体は
> 正しい（`UnknownSession` は session_id についての主張であってアドレスについての主張では
> ないため）——直すべきは試行順序のほう。
>
> タスク8 への注記: ここで言う「一方向の切替」は、タスク8 が実測待ちに格下げした
> 「preempt ping-pong 対策のラッチ」とは別の要請であり、実測を待たずに実装する。

---

<a id="r3-b3"></a>

### R3-B3. タスク1のトリガー情報は現状の API では取り出せない（`wait_backoff_or_network_change` は勝者を返さない）

**該当**: §3.2 タスク1（151-156行）

> 既存の`spawn_reconnect_signal`（`resume_loop.rs:514`）・
> `wait_backoff_or_network_change`（`resume_loop.rs:816`）がネットワーク経路変化を
> バックオフ中断シグナルとして既に扱っているので、**「STUN bare redialをN回失敗 かつ
> ネットワーク変化シグナルを受け取った」時点で前倒しでcross-family resumeへ切り替える**

**「ネットワーク変化シグナルを受け取った」という事実は、現状のコードから取り出せない。**

`wait_backoff_or_network_change`（`resume_loop.rs:816-836`）のシグネチャは:

```rust
async fn wait_backoff_or_network_change(
    delay: Duration,
    is_tty: bool,
    mut on_tick: impl FnMut(),
    network_monitor: &mut dyn isekai_netmon::NetworkChangeMonitor,
)
```

**戻り値が `()`**。内部の `tokio::select!` はどちらの branch が勝ったかを
`log::info!` に出すだけで（`resume_loop.rs:829-834`）、**呼び出し元
（`resume_with_backoff_until_deadline`、`resume_loop.rs:1120-1126`）には伝えない**。

同様に、**episode の開始原因**（ネットワーク変化でポンプを打ち切ったのか、
ポンプ自身が失敗したのか）も、現状は `run_resume_loop` の `select!` で
`Err(PumpFailure::Remote(anyhow::anyhow!("network change detected, reconnecting")))`
という**エラー文字列としてのみ**エンコードされている（`resume_loop.rs:1355-1357` 付近）。
文字列マッチで判定するのは、このリポジトリが `StaleTrustSignal`/
`MidSessionDisconnectSignal`/`ParentGoneSignal` で確立している
「型付きマーカーを `anyhow::context` で載せて `downcast_ref` で拾う」パターンに反する。

**修正案（§3.2 タスク1 または タスク2 に追記）**:

> トリガー判定のために、次の2つの小さな API 変更が必要:
> 1. `wait_backoff_or_network_change`（`resume_loop.rs:816`）が
>    **どちらの branch が勝ったかを戻り値で返す**ようにする（現状 `()` を返し、
>    `log::info!` に出すだけで呼び出し元には伝わらない）。
> 2. episode の開始原因（ネットワーク変化 vs ポンプ失敗）を判別したい場合は、
>    `run_resume_loop` の `select!` がネットワーク変化時に生成している
>    `PumpFailure::Remote(anyhow!("network change detected, reconnecting"))` に
>    **型付きマーカーを付ける**（既存の `MidSessionDisconnectSignal`/`StaleTrustSignal` と
>    同じ「attach-at-the-source / downcast-at-the-top」パターン）。文字列マッチはしない。

これを書いておかないと、実装者は「シグナルを受け取ったかどうかが分からない」と気づいた
時点で、**タスク1 が明示的に禁じた「120秒待ってから1回」に落とす**可能性が高い。

---

<a id="r3-b4"></a>

### R3-B4. タスク1の条件が論理積のため、netmon が無言の環境では「120秒待ち」に静かに退行する

**該当**: §3.2 タスク1（154-156行）「**STUN bare redialをN回失敗 かつ
ネットワーク変化シグナルを受け取った**時点で」

条件が**論理積（AND）**になっている。ネットワーク変化シグナルが一度も来なければ、
条件は永久に成立せず、**タスク1 自身が「デフォルト実装にするな」と書いている
「120秒待ってから」の挙動にそのまま落ちる。**

シグナルが来ない状況は実在する:

- `isekai-netmon` はプラットフォーム別実装
  （`isekai-netmon/src/{linux,macos,windows}.rs`、`lib.rs:36-44` の `cfg(target_os = ...)`）。
  コンテナ内・特殊なネットワークスタック・権限不足などで、監視が張れず無言になる可能性がある。
- `wait_backoff_or_network_change` 自身の doc（`resume_loop.rs:822-826`）が
  **「monitor が `None`（permanently stopped）を返した場合は、その呼び出しでは
  monitor branch が無効化され、以後 plain timeout にフォールバックする」**という
  ケースを明示的に想定している。つまり「monitor が黙る」ことは設計上想定済みの状態。
- そもそもクライアント側アドレスの変化が OS レベルのインターフェース変化を伴わない
  ケース（NAT の再バインド、キャリア側のアドレス再割当てなど）では、
  netmon は何も報告しない。**§3.3 が「救うケース」と呼んでいるシナリオの一部が、
  まさにこれに該当しうる。**

**修正案（§3.2 タスク1 の条件を論理和寄りに変える）**:

> 切替条件は次の**いずれか**が成立した時点とする（論理積にしない）:
> - ネットワーク変化シグナルを受け取った（前倒しの主経路）、**または**
> - ネットワーク変化シグナルの有無にかかわらず、STUN bare redial が N 回失敗した
>   （N は 120秒よりはるかに手前で切り替わる値を選ぶ）。
>
> 論理積にすると、netmon が無言の環境（プラットフォーム実装の欠落、
> `next_change()` が `None` を返して永久停止した場合——
> `wait_backoff_or_network_change` の doc が明示的に想定しているケース——、
> あるいは OS のインターフェース変化を伴わないアドレス変化）で、
> 本タスクが禁じている「120秒待ち」に静かに退行する。

---

<a id="section-c"></a>

## C. SHOULD-FIX

<a id="r3-c1"></a>

### R3-C1. §3.1 の「理由(a)は解消する」は無条件に読めてしまい、R2-B1 を再発させうる

**該当**: §3.1（137-138行）

> **本ADRのcross-family resumeは理由(a)を解消するが、理由(b)は解消しない**

その直後（139-142行）で、ガードの地点で `run_relay_resumable` を呼ぶと
「生きている `ssh(1)` に別セッションのハンドシェイクバイトが流れ込む」と書いている——
これは**まさに理由(a)（別トランスポートで黙って新セッションを始めてしまう）の
被害そのもの**である。つまり文章内で
「(a) は解消した」→「(a) の被害が起きる」と続いており、**自己矛盾して読める**。

正確には: cross-family resume が理由(a)を解消するのは
**タスク1の位置（`run_resume_loop` 内部、ポンプ生存中に RESUME する場合）のみ**。
`connect.rs:835-843` のガードの地点では、fallback が呼ぶのは依然
`run_relay_resumable`（新規 ATTACH）なので、**(a) も (b) も両方成立したまま**である。

これは些細な言い回しの問題に見えるが、**R2-B1（bail-out を誤って削除する）を
引き起こした思考経路そのもの**であり、将来の読者が
「ADR に理由(a)は解消済みと書いてある」を根拠にガードを外しにいく余地を残す。

**修正案**:

> 本ADRのcross-family resumeが理由(a)を解消するのは、**タスク1の位置
> （`run_resume_loop`内部、ポンプ生存中にRESUMEする場合）に限られる**。
> `connect.rs:835-843`のガードの地点で呼ばれるのは依然`run_relay_resumable`
> （新規ATTACH）であり、そこでは**理由(a)も理由(b)も成立したまま**である。
> したがってこのガード自体は残す（§3.2タスク3）。

<a id="r3-c2"></a>

### R3-C2. 配線の深さとパラメータ束切替の実装形が、実際のコード構造と1段ずれている

**該当**: §3.2 タスク2（159-166行）と タスク7（228-238行）

いずれも `run_resume_loop` を変更対象として名指ししているが、
**実際に `reconnect_and_resume` を呼ぶのは1段内側の
`resume_with_backoff_until_deadline`**（`resume_loop.rs:1077`）である。

具体的なコード構造:

```rust
async fn resume_with_backoff_until_deadline(
    factory: &AnyMuxFactory,
    target: &RelayTarget,          // ← 単一の target。毎回これで dial する
    profile: &str,
    policy: ResumeDeadlinePolicy,  // ← resume_window/deadline/max_resume_window を含む
    state: &mut ResumeLoopState,
    warm_standby_task: &Option<tokio::task::JoinHandle<()>>,
    network_monitor: &mut dyn isekai_netmon::NetworkChangeMonitor,
) -> Result<AnyByteStream>
```

（`resume_loop.rs:1077-1085`、`reconnect_and_resume` の呼び出しは 1131-1138行）

したがって:

1. **タスク2 の `cross_family_target` は `run_resume_loop` で止めず、
   `resume_with_backoff_until_deadline` まで通す必要がある。**
   「どちらの target で今回の試行を行うか」の判断は、この関数の `loop` の中に置かれる。
2. **タスク7 の「パラメータ束の切替」は、フィールドを in-place で書き換える形にはならない。**
   `max_resume_window` は `ResumeDeadlinePolicy` の一部であり、
   この構造体は `resume_with_backoff_until_deadline` に**値渡しで1回渡される**
   （`policy: ResumeDeadlinePolicy`）。`resume_window`/`deadline` は
   その `max_resume_window` から `effective_resume_window`（`resume_loop.rs:681`）で
   導出済みの値なので、**`max_resume_window` だけ後から差し替えても `deadline` は古いまま**になる。
   実装形は「**`ResumeDeadlinePolicy` を relay 向けに再計算し、
   その新しい policy でバックオフループを再入する**」になる。
   `notify_on_give_up`（`resume_loop.rs:1087`）もこの policy から関数内で導出されるので、
   再計算すれば自動的に追随する。

**修正案**: タスク2 に「`run_resume_loop` だけでなく
`resume_with_backoff_until_deadline`（`resume_loop.rs:1077`）まで引数を通す。
target 選択の判断はこの関数のループ内に置く」を、
タスク7 に「`max_resume_window` は `ResumeDeadlinePolicy` の一部で値渡しされ、
`resume_window`/`deadline` はそこから導出済みなので、フィールド差し替えではなく
**policy 全体を relay 向けに再計算してループを再入する**形になる」を、それぞれ追記する。

<a id="r3-c3"></a>

### R3-C3. 「netmon は STUN 経路でも生きている」ことを ADR に明記すべき（書かないとタスク1が放棄される）

**該当**: §3.2 タスク1 全体（記述が無いこと自体が問題）

タスク1 はネットワーク変化シグナルを前提にしているが、**ADR を読んだ実装者が
最初に確認するのは「STUN 経路でそのシグナルは生きているのか？」**であり、
そこで誤解する材料が揃っている:

- `run_stun_p2p_resumable` は `experimental_network_rebind = false` を渡している
  （`resume_loop.rs:363-370`、「STUN P2P's punched NAT mapping is tied to the socket
  used for initial establishment. Rebinding or warm-standby promotion would switch
  sockets/interfaces ... so both are deliberately disabled on this path」）。
- ここだけ見ると「STUN 経路ではネットワーク監視も無効」と読める。

**実際にはシグナルは生きている**——これを確認した根拠:

1. `spawn_reconnect_signal` は `run_resume_loop` のループ冒頭で
   **無条件に**呼ばれる（`resume_loop.rs:1340-1346`。`isekai_netmon::system_monitor()` を
   毎回渡している）。
2. `experimental_network_rebind == false` のとき、`spawn_reconnect_signal` の
   `_ =>` アーム（`resume_loop.rs:556-560`）が
   `if network_monitor.next_change().await.is_some() { ... }` で
   **ネットワーク変化をそのまま再接続シグナルとして転送する**。
   無効化されているのは「rebind を試みること」だけで、「変化を検知すること」ではない。
3. `resume_with_backoff_until_deadline` も
   `network_monitor: &mut dyn NetworkChangeMonitor` を**既に引数で受け取っている**
   （`resume_loop.rs:1084`）。

つまり**タスク1 が必要とするデータは、STUN 経路でも既に流れている**。
ADR にこの3点を1段落で書いておくこと。書かないと、実装者が
`experimental_network_rebind = false` を見て「この経路では使えない」と誤判断し、
タスク1 を放棄する（＝R2-C1 の指摘が実装されない）リスクが高い。

<a id="r3-c4"></a>

### R3-C4. §6 の 80% は、§3.3 の未検証仮定と同じ観測期間で評価すると誤読を招く

**該当**: §6（383-388行）

> **目標値**: 初期目標は「give-up境界に到達したケースの80%以上で `ssh(1)` を
> 再起動せず（cross-family resumeで）復旧する」とする。

§3.3 が明記した通り、**この割合の上限は「`cached_relay_addr` がクライアントの
新しいネットワークから到達可能である」という未検証の仮定に支配される**。
もしその仮定が（例えば tailnet アドレスのために）半分のケースで崩れていれば、
実装が完璧でも達成率は 50% 程度で頭打ちになる。

このとき「80% 目標に届かなかった」という結果は、
**設計や実装の失敗ではなく仮定の不成立**を意味するが、数値目標を先に置いておくと
「この ADR の方針は失敗だった」と誤読されうる。

**修正案**: 最初の観測期間を **「仮定の検証期間」と位置づけ、合否判定を置かない**。
§3.2 タスク5 の2分類（「セッション消滅」vs「relay 到達不能」）の比率が出てから、
**「relay 到達可能だったケースのうち何%で連続性を保てたか」**という、
実装の良否だけを測る分母に対して数値目標を設定する。
現状の「give-up 境界到達回数」を分母にすると、実装の良否と環境の性質が混ざる。

<a id="r3-c5"></a>

### R3-C5. §4.2 のケースが `always-connects.md` 違反にならないことの確認が抜けている

**該当**: §4.2（318-330行）

§4.2 は「双方同時にアドレスが変わるケースは救えない（非目標）」と宣言しているが、
**「救えない」が何を意味するのか**が書かれていない。2通りありうる:

- **(i) 連続性だけ失う**（scrollback は消えるが、新セッションとしては自動的につながる）
- **(ii) そもそも自動では再接続できない**（ユーザーが `isekai-ssh doctor --fix` を
  手で叩くまで復旧しない）

**(ii) だとすれば `.claude/rules/always-connects.md` 違反**であり、
同ルールが「原則としてバグとして扱う」と明記しているケースに該当する。
ADR が非目標として畳んでよいのは (i) の場合だけ。

懸念の根拠: cross-family resume が失敗して `MidSessionDisconnect` が wrapper に届くと、
`decide_connect_failure_recovery`（`isekai-ssh/src/wrapper.rs:1007-1010`）は
**`should_bootstrap` の値にかかわらず** `RetryConnectLightweight` を返す
（この挙動を固定するテストが `wrapper.rs:2644-2651` にある:
`decide_connect_failure_recovery_retries_lightweight_for_mid_session_disconnect_regardless_of_bootstrap_flag`）。
lightweight retry は**再デプロイをしない**ので、§4.2 のケース
（キャッシュ済みアドレスが両方とも陳腐化している）では、
**古いアドレスに対して再試行を繰り返すだけ**になりうる。

`run_ssh_with_connect_failure_recovery`（`wrapper.rs:661-680`）には
`lightweight_retries` カウンタと `redeploy_gate`（`reconnect_backoff::RedeployGate`）が
あるので、**一定回数後に再デプロイへエスカレーションする仕組みが存在する可能性が高い**が、
本レビューではその経路を最後まで追っていない。

**修正案**: §4.2 に1文足す。

> なお「救えない」のは**連続性のみ**であり、接続そのものは
> `MidSessionDisconnect` → wrapper の `RetryConnectLightweight`
> （`wrapper.rs:1007-1010`）→（N回失敗後の）再デプロイへのエスカレーション
> （`wrapper.rs`の`lightweight_retries`/`redeploy_gate`）によって自動的に復旧する。
> したがって`.claude/rules/always-connects.md`には抵触しない。

**ただしこれは実装着手前に実際に確認すること**（エスカレーションが実在しなければ、
§4.2 は「非目標」ではなく「別途対処が必要なバグ」に格上げされる）。
本レビューでの追跡は `wrapper.rs:661-680` のカウンタの存在確認までで、
エスカレーションの発火条件は未確認。

---

<a id="section-d"></a>

## D. NICE-TO-HAVE

- **R3-D1. §3.2 タスク5 の `"cross-family-resumed"` と `telemetry.rs` の既存 doc の食い違い**

  タスク5 は `"cross-family-resumed"` を
  `previous_session_id = Some(旧)`, `new_session_id = Some(同じ値)` として定義している。
  つまり `previous == new` のケース。ところが `telemetry.rs:212-221` は
  「the one case where it would be meaningful — a bare redial that keeps the *same*
  `session_id`, i.e. `previous_session_id == new_session_id` — is deliberately out of
  scope here」と、**`previous == new` を明示的にスコープ外と書いている**。
  タスク5 が doc 更新を要求しているので実務上は問題ないが、
  **更新時にこの1文を明示的に撤回する**ことをタスクに書いておくと、
  「スコープ外と書いてあるのに使っている」という矛盾がソースに残らない。

- **R3-D2. §3.2 タスク9 と §6 が相互参照で完結していない**

  タスク9 は doctor 表示について「§6の集計手段としても利用できる余地がある」と書き、
  §6 は「§3.2タスク9で触れた…将来の簡易集計手段の候補として検討する」と書いている。
  **どちらも相手に委ねていて、誰もコミットしていない**。実装スコープ外なら
  「本 ADR では実装しない」と片方に明記するほうがよい。

- **R3-D3. §1.1 は履歴であり、§1（背景）の一部として読むには重い**

  内容は正しく、かつ再発防止に有用なので削るべきではないが、
  付録（Appendix A: round 1 で訂正された前提）へ移すと §1 が問題そのものの説明に
  集中できる。純粋に編集上の提案。

- **R3-D4. §5 の「3経路とも `isekai-pipe connect` を子プロセスとして起動する」に根拠を1つ添えられる**

  正しい記述だが、Windows 側の根拠として
  `resume_loop.rs:1044-1050` の `give_up` doc（"The Windows-native connect path
  (`native/connect.rs`, `connect_attempt`) has an analogous but distinct race:
  it guards against dropping its `isekai-pipe connect` `Child` ..."）を引くと、
  「Windows でも子プロセスとして起動される」ことがコード上の根拠付きで示せる。

---

<a id="section-e"></a>

## E. `ADR_INPUT_RESUME_SYMMETRY.md` ヘッダ更新の確認

round 2 で指摘した2点とも**正しく反映されている**。

- **対象**（7-11行）: `rust-core/isekai-pipe/src/resume_loop.rs`（`C2hReplayBuffer` の隣）に
  更新済み。当初案の `isekai-transport`・`isekai-ssh/src/session.rs` を
  「採用しないことになった」と、理由（Android 経路との無用な共有回避）付きで
  明記している——単に消すより良い（なぜ消えたかが後から分かる）。
- **Status**（3-6行）: 「§3論点1・4 は決着済み（2026-09-13）、論点2・3 は同ADR§3の確定待ちで未決」
  と正しく書き分けられている。

**追加の指摘なし。** ただし1点だけ整合の注意:

本レビューの R3-B1/R3-B2 により §3.2 タスク4 の有界リトライ窓が
「切断から最低30秒」に伸びる可能性がある。入力ADR §3 論点3（危険コマンドの遅延実行）の
リスク予算は、本ADR §7.2 が述べる通り「サイレント再接続窓が 120秒 → relay grace（10日）」
という大きな変化に支配されるので、30秒の差は実質的な影響を与えない。
**入力ADR 側の再修正は不要**と判断する。

---

<a id="section-f"></a>

## F. 結論 — Approved までに必要な差分と round 4 の要否

### 判定

**MUST-FIX 4件を反映すれば Approved に進んでよい。**
round 2 で指摘した17件は**すべて意図通りに反映されている**ことを確認した。
新規指摘4件は、いずれも既存タスクへの1〜2文の追記で解消し、
**設計の形（どこに何を実装するか）は一切変わらない。**

### Approved に必要な差分（MUST-FIX 4件）

1. **R3-B1**: §3.2 タスク4 に「有界リトライ窓は
   `disconnected_at + UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`（切断から30秒）に達するまで
   終わらせない」を追記。タスク1 の前倒し切替と組み合わせると、
   `update_unknown_session_streak`（`resume_loop.rs:1021`）の2つ目の conjunct が
   満たされず、R2-B6 と同じ不発が別経路から再発する。
   **これを落とすと、タスク5 の計装も §6 の成功基準も §3.3 の仮定検証も
   連鎖的に機能しなくなる**ので、4件のうち最も影響が大きい。
2. **R3-B2**: §3.2 タスク4 に「cross-family へ切り替えたら同一 episode 内で
   STUN target に戻らない（一方向の切替）」を追記。
   `update_unknown_session_streak` は `UnknownSession` 以外のエラーで streak を 0 に戻す
   （`resume_loop.rs:1017-1019`）ため、交互試行では閾値に到達しない。
   **タスク8 が実測待ちに格下げした preempt ラッチとは別の要請**である旨も注記する。
3. **R3-B3**: §3.2 タスク1/2 に「`wait_backoff_or_network_change`
   （`resume_loop.rs:816`）が勝った branch を戻り値で返すよう変更する」と
   「episode 開始原因は文字列ではなく型付きマーカーで判別する」を追記。
   現状トリガー情報が取り出せないため、放置すると実装者が
   タスク1 が禁じた「120秒待ち」に落とす。
4. **R3-B4**: §3.2 タスク1 の切替条件を論理積から**論理和**へ。
   netmon が無言の環境（プラットフォーム実装欠落、`next_change()` の永久 `None`——
   `wait_backoff_or_network_change` の doc が明示的に想定、OS インターフェース変化を
   伴わないアドレス変化）で、タスク1 自身が禁じた挙動に静かに退行する。

### 併せて入れることを推奨（SHOULD-FIX 5件）

5. **R3-C1**: §3.1 の「理由(a)を解消するが」を
   「**タスク1の位置に限り**理由(a)を解消する。ガードの地点では(a)も(b)も成立したまま」へ。
   R2-B1 の再発防止として、4件の MUST-FIX と同等に重要。
6. **R3-C2**: タスク2 に「`resume_with_backoff_until_deadline`（`resume_loop.rs:1077`）まで
   引数を通す」、タスク7 に「`ResumeDeadlinePolicy` を再計算してループ再入する形になる」を追記。
7. **R3-C3**: §3.2 タスク1 に「netmon は `experimental_network_rebind = false` の
   STUN 経路でも生きている」ことを根拠3点（`resume_loop.rs:1340-1346` の無条件 spawn、
   `resume_loop.rs:556-560` の `_ =>` アーム、`resume_loop.rs:1084` の既存引数）付きで明記。
8. **R3-C4**: §6 の最初の観測期間を「仮定の検証期間（合否判定なし）」とし、
   数値目標は「relay 到達可能だったケース」を分母に再定義してから置く。
9. **R3-C5**: §4.2 に「救えないのは連続性のみで、接続自体は wrapper の
   lightweight retry → 再デプロイのエスカレーションで自動復旧するため
   `always-connects.md` には抵触しない」を追記。
   **ただしこのエスカレーションの実在を実装着手前に確認すること**
   （本レビューでは `wrapper.rs:661-680` のカウンタ存在確認まで）。
   実在しなければ §4.2 は「非目標」ではなく「別途対処が必要」に格上げされる。

### round 4 の要否

**不要と判断する。** 理由:

- 新規指摘4件（+推奨5件）はすべて**既存タスクへの追記**であり、
  round 2 の R2-B1/R2-B6 のように「タスクの構造を変える」ものが1つも無い。
- 各指摘の修正文案を本レビューに具体的に書いてあるので、
  反映結果の妥当性は文面の照合で判断できる。
- 唯一の未確認事項は **R3-C5（wrapper のエスカレーションの実在）**だが、
  これは ADR のレビューではなくコードの確認事項であり、
  実装着手前のチェックリスト項目として扱えば足りる。

したがって **MUST-FIX 4件 + SHOULD-FIX 5件を反映したうえで、
R3-C5 の確認を済ませたら Status を Approved に進めてよい。**

---

## 参照した実装箇所（round 3 で新たに確認したもの）

- `rust-core/isekai-pipe/src/resume_loop.rs`
  — `spawn_reconnect_signal` 514-560（特に `experimental_network_rebind == false` の
  `_ =>` アーム 556-560）、`wait_backoff_or_network_change` 816-836（戻り値が `()`、
  monitor が `None` を返す場合の doc 822-826）、`ResumeLoopState` 843-872
  （`consecutive_unknown_session` 872）、`UNKNOWN_SESSION_CONFIRM_THRESHOLD = 3` 959、
  `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR = 30s` 962-978、
  `is_unknown_session_rejection` の doc 979-1003、
  `update_unknown_session_streak` 1016-1023（非 `UnknownSession` で streak を 0 に戻す
  1017-1019、2つの conjunct 1021）、`give_up` doc の Windows native 経路への言及 1044-1050、
  `resume_with_backoff_until_deadline` のシグネチャ 1077-1085（単一 `target`、
  値渡しの `ResumeDeadlinePolicy`、`network_monitor` を既に受け取っている 1084）、
  `notify_on_give_up` 1087、`wait_backoff_or_network_change` 呼び出し 1120-1126、
  `reconnect_and_resume` 呼び出し 1131-1138、streak リセット（resume 成功時）1142、
  `run_resume_loop` の無条件 `spawn_reconnect_signal` 1340-1346、
  ネットワーク変化を `PumpFailure::Remote` の文字列にする `select!` 1352-1358
- `rust-core/isekai-ssh/src/wrapper.rs`
  — `run_ssh_with_connect_failure_recovery` の `lightweight_retries`/`redeploy_gate`
  661-680、`decide_connect_failure_recovery` 1007-1010、
  `..._regardless_of_bootstrap_flag` テスト 2644-2651
- `rust-core/isekai-netmon/src/lib.rs`
  — プラットフォーム別実装の `cfg(target_os = ...)` 36-44 / 185-205、
  および `linux.rs`/`macos.rs`/`windows.rs` の存在
- `rust-core/isekai-transport/src/telemetry.rs`
  — `previous_session_id == new_session_id` を「deliberately out of scope」と
  書いている箇所 212-221
