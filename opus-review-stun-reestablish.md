# Opus review: cross-family resume-preserving fallback 実装レビュー

対象: `feat/stun-reestablish-continuity`(`origin/main..HEAD` の4コミット)
基準: `ADR_STUN_REESTABLISH_CONTINUITY.md` rev4 §3.2 タスク1〜9、§3.3、§4
性質: 読み取り専用レビュー(ファイルは一切編集していない)。行番号はこの worktree の
HEAD 時点のもの。

---

## 総評

配線(タスク2)・bail-outガード維持(タスク3)・一方向切替(タスク4)・
policy再計算の**機構**(タスク7)・telemetry doc更新(タスク5)は、いずれも
ADRの意図通りに実装されている。機構レベルの実装品質は高い。

一方で、**タスク7の「いつ」を取り違えている critical 1件**と、**ADRが指示した
「事前検証」を「事前失敗」として実装したことによる always-connects 違反 1件**、
**切替トリガーが元targetを1回も試さない設計上の副作用 1件**がある。特に critical 1件は、
「この実装が救おうとしているまさにそのケースで、§3.3が未検証と警告していた前提が
崩れていたとき、`isekai-pipe connect` が事実上永久にハングする」という、
ADRが最も避けたかった結果を招く。

---

## CRITICAL

### C1. cross-family 切替時に `max_resume_window` を「成功前」に `None` にしているため、切替が外れたときギブアップ不能になる(最大10日)

**該当**:
- `rust-core/isekai-pipe/src/resume_loop.rs:1152-1165`(episode開始時の即時切替)
- `rust-core/isekai-pipe/src/resume_loop.rs:1225-1243`(バックオフループ途中の切替)
  いずれも `max_resume_window = None; resume_window = effective_resume_window(grace, None);
  deadline = disconnected_at + resume_window;`

**問題**: ADR §3.2 タスク7の該当文は

> `max_resume_window` を STUN 経路の `Some(STUN_RESUME_GIVE_UP_WINDOW)` から
> relay 相当(`None`)へ切り替える。切り替えないと、**cross-family resume が成功した後も**
> 以後ずっと120秒でgive upし続け、relay本来の耐性を静かに失う。

であり、切替の動機は明確に「**成功した後の**以後のepisodeで relay 本来の耐性を得ること」。
一方 ADR §3.2 タスク4 は、cross-family resume の試行そのものについて

> cross-family resume は**短い有界リトライ**(目安: 最大 `UNKNOWN_SESSION_CONFIRM_THRESHOLD` 回、
> 合計10〜30秒程度)にし〔…〕有界リトライ窓は、回数だけでなく
> `disconnected_at + UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`(切断から30秒)に到達するまでは
> 終わらせないこと。

と、**下限30秒・上限は数十秒オーダーの窓**を要求している。

実装は、この2つを1つの操作に潰し、**まだ一度も cross-family resume が成功していない
時点で** deadline を relay 相当へ伸ばしてしまっている。`effective_resume_grace_secs` は
サーバー既定で `isekai-pipe-core/src/lib.rs:73` の `DEFAULT_RESUME_GRACE_SECS = 864_000`
(=10日)なので、`deadline = disconnected_at + 10日` になる。

**失敗シナリオ**(§3.3が「未検証の前提」として名指ししていたケースそのもの):

1. ユーザーが Wi-Fi → セルラーへローミング。netmon が発火し
   `episode_started_by_network_change = true`。
2. `resume_with_backoff_until_deadline:1152` が即座に cross-family relay target
   (`profile.rs` の `cached_relay_addr` 由来)へ切替え、同時に deadline を10日へ。
3. しかし `cached_relay_addr` が tailnet / LAN アドレスで、セルラーからは到達不能
   (§3.3 が「この場合relayも同時に到達不能になりうる」と明示していたケース)。
4. 切替は一方向なので(`switched_this_call.is_none()` ガード、resume_loop.rs:1225)、
   生きていたかもしれない STUN target へは**二度と戻らない**。
5. 結果: 到達不能な relay アドレスに対して**10日間**バックオフ再試行し続ける。
   `resume_loop.rs:1169` の deadline 判定は事実上到達せず、
   `UnknownSession` streak(`:1326`)もネットワーク到達不能では発火しない
   (`is_unknown_session_rejection` は `ResumeRejected(UnknownSession)` のみ true)。

**なぜ critical か**: `isekai-pipe connect` プロセスが終了しないため、
`isekai-ssh/src/wrapper.rs:670-671` / `:801-826` の
`lightweight_retries` → `MAX_LIGHTWEIGHT_RETRIES` → `redeploy_gate.try_consume()`
という `.claude/rules/always-connects.md` 上の唯一の自動復旧エスカレーションが
**一度も起動しない**。変更前は STUN の120秒 give-up が必ずこの経路を叩いていたので、
最悪でも約2分で(連続性は失いつつ)新セッションに復帰できた。この変更は
「連続性を保つ代わりに、外したときは永久にハングする」というトレードに
すり替えてしまっている。ADR §4.2 が「接続そのものは自動復旧する見込みなので
`always-connects.md` には抵触しない」と結論した根拠そのものを無効化する。

**副次的な帰結(単独でも major 級)**: `resume_loop.rs:1192-1209` の
`continuity-lost reason: relay-unreachable` は deadline 超過でしか出ない。
deadline が10日なら、この分類は**実運用でほぼ絶対に出力されない**。
ADR §3.3/§6 が「§3.3の未検証の前提を運用データから検証するために
`session-gone` と `relay-unreachable` を分けて記録する」と定めた、計装の
一番の目的(タスク5の存在理由)がこれで達成されなくなる。
`ISEKAI_PIPE_DESIGN.md` の Epic S 節も「`relay-unreachable`(デッドライン超過)」と
書いており、書いた本人も短時間で到達する前提でいたことが読み取れる。

**推奨修正**: 「切替時の deadline」と「切替成功後の以後のepisodeの deadline」を分離する。
具体的には、切替時は `max_resume_window` を `None` にせず、
`Some(max(UNKNOWN_SESSION_MIN_ELAPSED_FLOOR, 短い有界窓))`
(ADR文言通りなら30〜45秒程度)に**差し替える**。`None` への昇格は
`run_resume_loop:1629-1635`(=cross-family resume が実際に成功して
`switched` が `Some` で返ってきた後)でだけ行う。この形なら
タスク4の下限30秒・上限数十秒の両方と、タスク7の「成功後はrelay相当」の
両方が同時に満たせる。

---

## MAJOR

### M2. `build_cross_family_target(&intent)?` が、プライマリ接続を試す前に connect 全体を失敗させる(always-connects違反)

**該当**: `rust-core/isekai-pipe/src/connect.rs:717`、`:768`
(定義は `connect.rs:836-854`)

```rust
let cross_family_target = build_cross_family_target(&intent)?;   // ← `?`
let stun_result = run_stun_p2p_with_fallback(&target, &candidates, cross_family_target, ...).await;
```

`build_cross_family_target` は `decode_secret` / `validate_endpoint_identity` /
`helper_addr.parse()` の3つで `Err` を返しうる。`?` でそのまま伝播するため、
**`cross_family_fallback` の内容が壊れているだけで、プライマリのSTUN接続を
1回も試さずに connect 全体が失敗する**。

変更前は、これらの検証は `recover_via_cross_family_fallback`(`connect.rs:870-895`)の
中でしか実行されず、しかもそこへ到達するのは「プライマリが既に失敗した後」だけだった。
つまり**壊れた fallback エントリは、これまで正常な接続を一切妨げなかった**。

**失敗シナリオ**: `~/.config/isekai-ssh/` のプロファイルで `legacy_relay_transport` の
`helper_addr` が(旧フォーマット・手編集・部分的な再デプロイ等で)パースできない
文字列になっている。STUN P2P 経路は完全に健全。変更前は普通に繋がっていたが、
変更後は `isekai-pipe connect` が `invalid cross_family_fallback helper_addr ...` で
即死する。ユーザーが手で `doctor --fix`/`init` するまで復旧しない
= `.claude/rules/always-connects.md` が「原則バグ」と定義する状態。

なお ADR §3.2 タスク2 は「バリデーションは `connect.rs` 側で**接続開始前に済ませ**、
検証済みの `RelayTarget` を渡す」とは言っているが、**検証失敗時に connect 全体を
落とせとは言っていない**。同じタスクの3つ目のbulletは
「`intent.cross_family_fallback` が `None` の場合は、cross-family resume を
単にスキップし従来通りgive upする」と、欠落時のデグレード方針を定めている。
壊れている場合も同じく「スキップ」に倒すのがADRの意図と整合する。

**推奨修正**: `build_cross_family_target` を `-> Option<RelayTarget>` にして、
検証失敗は `log::warn!` の上 `None` を返す(cross-family resume を無効化するだけ)。

### M3. ネットワーク変化起因のepisodeでは、元のSTUN targetを1回も試さずに切り替える

**該当**: `resume_loop.rs:1152-1165`(ループに入る**前**の即時切替)

`episode_started_by_network_change` が真なら、`reconnect_and_resume` を
STUN target に対して**一度も呼ばずに** cross-family へ移る。ADR §3.2 タスク1 は
「ネットワーク変化シグナルを受け取った(前倒しの主経路)」を切替条件の一方として
認めているので、文言上の違反ではない。しかし実際に発火する条件を確認すると、
この扱いは広すぎる:

- STUN 経路は `resume_loop.rs:412` で `experimental_network_rebind = false` を
  ハードコードして `run_resume_loop` を呼ぶ。
- したがって `spawn_reconnect_signal` は既定アーム(`resume_loop.rs:507-514` の
  doc が言う「every OS-reported network change が即座に転送される」)で動く。
- つまり**OSが報告するあらゆるネットワーク変化**——`Wake`(サスペンド復帰)、
  VPN/Tailscale インターフェースの上げ下げ、docker/bridge の追加削除、
  セカンダリNICのDHCP更新など、**クライアントの実効アドレスが変わっていない
  変化を含む**——が `episode_started_by_network_change = true` を生む。
  Linux の `AF_NETLINK` バックエンドではこの種のイベントは珍しくない。

**失敗シナリオ**: 健全なSTUN P2Pセッション中に Tailscale が再起動して
インターフェースが上下する。クライアントのWi-Fiアドレスは不変で、
STUN bare redial なら数百msで復旧したはず。しかし実装は即座に cross-family へ移り、

- relay が到達可能なら → resume は成功するが、`run_resume_loop:1629-1632` で
  `current_target` が置き換わり `cross_family_target = None` になるため、
  **そのセッションの残り全期間、P2Pへは二度と戻らない**(恒久デグレード)。
- relay が到達不能なら → C1 の永久ハングに直行する(元のSTUN targetは
  既に捨てられている)。

**推奨修正**: ネットワーク変化起因でも、切替前に少なくとも1回は
現 target への `reconnect_and_resume` を試す(1回失敗したら即切替、で
ADR の「120秒待たない」要求は十分満たせる——初回backoffは500ms)。
これだけで、誤検知イベントによる恒久デグレードと C1 への直行の両方が
大幅に減る。

---

## MINOR

### m1. `wait_backoff_or_network_change` の doc コメントが `BackoffWaitOutcome` に付け替わった

`resume_loop.rs:856-877`。新しい `#[derive(...)] enum BackoffWaitOutcome` が、
既存の関数 doc(`/// One backoff wait inside [...] retry loop: ...`)と
`async fn wait_backoff_or_network_change` の**間**に挿入されている。
Rust のdocコメントは直後の item に付くので、この説明文は現在 enum の
ドキュメントになり、関数側は無ドキュメントになった。enum 定義を doc の
**上**へ移すだけで直る。

### m2. deadline give-up の分類コメントが成立しない場合がある

`resume_loop.rs:1193-1200` のコメントは

> never an `UnknownSession` rejection, or the streak give-up below would have fired first

と断言しているが、`update_unknown_session_streak`(`:1073-1076`)は
`UnknownSession` 以外のエラーで streak を 0 に戻す。したがって
「`UnknownSession` と他のエラーが交互に来る」パターンでは streak が
閾値3に達しないまま deadline を超えうる。そのとき実装は
`continuity-lost reason: relay-unreachable` と記録するが、実際には
サーバーがセッションを知らないだけかもしれない。C1で述べた通り
この分類は §3.3 の前提検証に直接使われるデータなので、誤分類は
そのまま誤った結論を招く。最後のエラーが `UnknownSession` だったかを
`state` に持っておいて分岐する等が素直。

### m3. telemetry の `attempts`/`elapsed` が全て `0`/`ZERO`

`resume_loop.rs:1202-1208`、`:1296-1302`、`:1352-1358`。
doc コメント(`telemetry.rs:225-229`)とは整合しているので
**依頼された「引数とdocの整合」という観点では問題なし**。ただし、
`"cross-family-resumed"` の呼び出し地点では `attempt`(`:1217`)も
`disconnected_at`(`:1147`)も在圏しており、
`attempts = attempt` / `elapsed = Instant::now() - disconnected_at` を
渡せば、ADR §6 が求める「relay到達可能だったケースのうち何%で連続性を
保てたか」に加えて「切替から復旧までに何秒かかったか」という
体験指標がそのまま取れる。現状は0固定なのでこの情報が捨てられている。

### m4. 失敗理由の2分類が telemetry の field ではなく隣接する別ログ行

`resume_loop.rs:1201` / `:1351` の `log::info!("... continuity-lost reason: ...")` は
`log_rendezvous_outcome` の直前に出るだけで、両者は行の隣接でしか
結び付いていない。`isekai-pipe connect` は warm-standby タスク・
app-ack タスク等から並行に `log::info!` しうるので、ログファイル上で
間に別の行が挟まりうる。ADR §3.2 タスク5 の「分けて記録する」は
満たしているが、集計(§6 の手動ログ検分)の堅牢性としては
`class = "continuity-lost-relay-unreachable"` / `"continuity-lost-session-gone"`
のように `class` 側に畳むほうが確実。

### m5. 切替後のライブ表示が `.../864000s` になる

`resume_loop.rs:1221` の `print_reconnect_status(true, disconnected_at, resume_window)` は
切替後の `resume_window`(=10日)を分母に表示する。ユーザーには
`connection lost, trying to reconnect... (15s/864000s)` と出る。
C1 を修正して切替時の窓を短く保てば自動的に解消する。

### m6. warm-standby の陳腐化(依頼事項5の検証結果): **現状は無害で正しい。ただしペアが不整合**

自己申告の通り無害であることを確認した。根拠:

- `WarmStandby` は `tethering_interface.map(...)` でのみ構築される
  (`resume_loop.rs:1449-1450`)。
- `tethering_interface` が `Some` になりうる呼び出し元は
  `run_relay_resumable`(`:354`)と `run_relay_resumable_with_fallback`(`:377`)だけで、
  **この2つはどちらも `cross_family_target` に `None` を渡している**。
- STUN 経路(`run_stun_p2p_resumable`)は `resume_loop.rs:415` で
  `/* tethering_interface */ None` をハードコードしている。

したがって「`warm_standby` が `Some`」と「`cross_family_target` が `Some`」は
現状**同時に成立しえない**ので、`target` から作った `WarmStandby` が
family switch 後に古くなることは起こらない。見落としではない。

ただし1点だけ指摘: `resume_loop.rs:1602` は
`promote_warm_standby_once(ws, &current_target, ...)` と `current_target` を渡す一方、
`ws` 自身は `target.clone()` で構築されている(`:1450`)。
現状は両者が常に一致するので無害だが、「片方だけ current 化された」状態は
将来 STUN + tethering を組み合わせたときに静かに壊れる形になっている。
`WarmStandby` 側も current 化できないなら、`:1450` に
「ここは switch を想定していない(両者は排他)」という短いコメントを
置いておくのが安全。

### m7. replay 失敗の `continue` 経路が `stun_failures` を増やさない

`resume_loop.rs:1262-1276`。「resume は成功したが replay が不整合」で
`continue` する経路は `Err` アームを通らないため、
`stun_failures`(`:1307-1309`)が増えない。この状態が繰り返し起きると、
回数条件による切替(`:1227`)が永久に発火しない。ネットワーク変化シグナルが
来れば救われるが、来なければ STUN の120秒窓を使い切る。発生頻度は
低いと思われるので minor。

### m8. ADR タスク7 最終bullet(`experimental_network_rebind`/warm-standby の扱い)の判断が記録されていない

ADR は「cross-family で relay へ移った後、relay 経路本来の設定に
切り替えるかを**実装時に決める**(`run_resume_loop` は現状これらを
引数で1回受け取るだけなので更新経路が無い点に注意)」と、明示的な
意思決定を求めている。実装は `experimental_network_rebind` を
`false` のまま据え置いている(これ自体は保守的で妥当な選択)が、
コード上にも `ISEKAI_PIPE_DESIGN.md` Epic S にも「据え置くと決めた」旨の
記録が無い。`resume_loop.rs:1498-1504` 付近に1行コメントを足すだけでよい。

### m9. 新しい振る舞いに対するテストが1件も無い

`git diff origin/main..HEAD -- rust-core/isekai-pipe/src/resume_loop.rs` で
追加された関数は `NetworkChangeReconnectSignal` の `fmt` 1つだけ。
既存テスト(`:2400`、`:2483`)は新引数に `None,false,false,0` を足して
シグネチャを合わせただけで、切替そのものは一切踏んでいない。
このリポジトリは `update_unknown_session_streak` のような純粋関数を
わざわざ切り出して単体テストする文化(`:2040-2100`)なので、
少なくとも

- 切替後に `deadline`/`resume_window`/`notify_on_give_up` が
  再計算されること(C1 の修正後の期待値を固定する回帰テスト)
- `switched_this_call` が一度 `Some` になったら同一callで再切替しないこと
- `episode_started_by_network_change = true` かつ
  `cross_family_target = None` のとき従来通りの挙動に落ちること

は、`resume_with_backoff_until_deadline` を直接呼ぶ既存テストの形
(`FailingMuxFactory` 相当 + `NoopNetworkChangeMonitor`/`FireOnceNetworkChangeMonitor`)で
そのまま書ける。C1 のような「定数1つの意味の取り違え」は、まさに
この種のテストが止める類のバグ。

---

## 問題なしと判断した項目(根拠つき)

### 依頼事項1: タスク7(パラメータ束の切替)の**機構**は正しい

`ResumeDeadlinePolicy` を field-patch せず、`resume_window` を
`effective_resume_window(effective_resume_grace_secs, None)` から**再導出**し、
`deadline` も `disconnected_at + resume_window` で引き直している
(`:1158-1163` と `:1236-1241` の両方、同一の4行セット)。ADR が
「`max_resume_window` フィールドだけ差し替えても `deadline` は古いまま残る」と
警告していた罠は回避できている。`notify_on_give_up` も同じ箇所で
`max_resume_window.is_none()` から引き直しており、doc の要求通り policy から
自動追随する(直前に `None` を代入しているので常に `true` になる冗長な式だが、
「policy から導出する」という形が保たれているので将来の変更に強い)。

3つのケースすべてを確認した:

- **① episode 開始時点で即座に切り替わるケース**: `:1152-1165`、ループに入る前に
  4値すべて再計算。正しい。
- **② バックオフループ途中で切り替わるケース**: `:1225-1243`、同じ4値を同じ式で
  再計算。`deadline` が伸びる方向なので、直後の `delay.min(deadline - now)`
  (`:1216`)も破綻しない(relayの窓 ≥ STUNの窓は
  `clamp_resume_window` の定義上つねに成立)。正しい。
- **③ 2回目以降のepisode(`already_cross_family`)**: `run_resume_loop:1629-1635` が
  `current_target` / `cross_family_target = None` / `switched_to_cross_family = true` /
  `max_resume_window = None` / `resume_window` を更新し、次周回の `:1593` で
  `deadline = disconnected_at + resume_window` が新しい窓で引き直される。
  `ResumeDeadlinePolicy`(`:1623`)にもその値が渡るので `notify_on_give_up` も
  追随する。**呼び出し元側での引き継ぎは正しく行われている**。

(C1 は「この機構が正しいこと」ではなく「発動タイミングが早すぎること」の指摘であり、
機構そのものの評価とは独立。)

### 依頼事項2: タスク4の2つの罠は(C1の副作用として)回避できている

- **罠(a)「回数条件だけでは足りない、時間条件(切断から30秒)も必要」**:
  切替後の `deadline` が `disconnected_at + 10日` なので、
  `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`(30秒、`:1035`)より前に
  ループが終わることはありえない。`update_unknown_session_streak`
  (`:1078`)の `elapsed_since_disconnect >= FLOOR` は必ず評価される機会を得る。
  **ただしこれは C1 の「窓が長すぎる」ことの副作用として成立している**——
  C1 を修正する際、短い窓を入れ直すときに 30秒下限を割らないよう
  明示的に `max(UNKNOWN_SESSION_MIN_ELAPSED_FLOOR, ...)` を書く必要がある。
  現状のコードにはこの下限を守るという意図がどこにも表現されていない。
- **罠(b)「一方向切替でないと streak がリセットされ続ける」**: 回避できている。
  `switched_this_call.is_none()` ガード(`:1225`)により同一call内で2度目の
  切替判定は行われず、`current_target` は切替後 `reconnect_and_resume`
  (`:1249`)と `reestablish_control_stream`(`:1278`)の両方で一貫して使われる。
  STUN target へ戻る経路はコード上存在しない。episode をまたぐ側も
  `run_resume_loop:1631` が `cross_family_target = None` にして
  以後の切替判定自体を無効化する。
- **streak を切替時にリセットしていない点**: ADR §3.2 タスク4 が
  「`state.consecutive_unknown_session` をセッション単位で持つこと自体は正しい
  ——`UnknownSession` は session_id についての主張であってアドレスについての
  主張ではないため」と明示的に承認している通りなので、仕様通り。
- **回数カウンタのチューニング**: `STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS = 5`
  (`:89`)は、`stun_failures` が `Err` アームでのみ増える(`:1307-1309`)ため
  「STUN target への5回の実試行」を意味し、`RESUME_BACKOFF`
  (500ms/1s/2s/4s/8s)の累積で約15.5秒。doc コメントの記述と一致し、
  120秒よりはるかに手前という ADR タスク1 の要求も満たす。

### 依頼事項3: telemetry の引数と doc の整合

整合している。

- `"cross-family-resumed"`(`:1296-1302`): `previous = Some(state.session_id)`、
  `new = Some(state.session_id)` で同値。更新後の doc
  (`telemetry.rs:220-224`)が「`previous_session_id` と `new_session_id` が
  同じ値になるのがこのクラスの定義」と明記しており一致。
  ADR が要求した「`telemetry.rs:212-221` の『`previous == new` は意図的に
  スコープ外』という一文を明示的に撤回すること」も、その段落ごと
  書き換えられて履行されている(`telemetry.rs:218-224`)。
- `"continuity-lost"`(`:1202-1208`、`:1352-1358`): `new = None`。doc と一致。
- `attempts = 0` / `elapsed = Duration::ZERO`: doc(`telemetry.rs:225-229`)が
  3クラスすべてについてそう書いているので**整合はしている**(改善余地は m3 を参照)。
- doc の producer 記述も
  「`connect_via_relay_resumable_with_fallback` と `isekai-pipe` の
  cross-family switch の両方」に更新済み(`telemetry.rs:210-217`)。
  ADR が「round 1 で起きた『relay専用の計装を汎用だと誤解する』事故の再発防止」
  として要求した更新を満たしている。
- 発火回数の設計も妥当: `"cross-family-resumed"` を
  `already_cross_family` で gate していない(`:1287`)理由が
  コメントで説明されており、「切替という遷移を1回だけ記録し、
  以後の毎回の resume 成功では出さない」という意図と実装が一致している。

### 依頼事項4: タスク3 の bail-out ガードは無変更

`git diff origin/main..HEAD -- rust-core/isekai-pipe/src/connect.rs` の hunk は
4つ(715行付近・765行付近・795行付近・813行付近の追加)のみで、
`recover_via_cross_family_fallback` の本体(現 `connect.rs:844-905`)には
**1行の変更も入っていない**。`MidSessionDisconnectSignal` チェック
(現 `connect.rs:863-865`)も含めてそのまま。

加えて、このガードが実際に効き続けることも確認した:
`run_stun_p2p_resumable`(`resume_loop.rs:393-421`)は本体全体を
`.map_err(|e| e.context(MidSessionDisconnectSignal))` で包んでおり、
`run_resume_loop` 内の cross-family 切替が失敗して返す `Err`
(C1 の10日 deadline 超過、または `UnknownSession` streak 確定)も
確実にこのマーカーを帯びる。したがって `connect.rs:863` で
新規 ATTACH(`run_relay_resumable`)へ落ちることはなく、
ADR §4.1 が非目標とした「生きている `ssh(1)` に別セッションの
ハンドシェイクバイトが流れ込む」破壊は起きない。タスク3の要求を満たしている。

### 依頼事項5: warm-standby の陳腐化 → 無害であることを確認(詳細は m6)

### その他の確認

- **`NetworkChangeReconnectSignal` の型付きマーカー化**(ADR タスク2(b)):
  `resume_loop.rs:1514` で `anyhow!(...).context(NetworkChangeReconnectSignal)` として
  付与し、`:1519-1522` で `downcast_ref` で拾っている。文字列マッチは残っていない。
  `anyhow` の `context` は `downcast_ref::<C>()` で context 値自体を取り出せるので
  この使い方は正しい(既存の `MidSessionDisconnectSignal` と同じパターン)。
- **`wait_backoff_or_network_change` の戻り値化**(ADR タスク2(a)): 実装済み。
  既存テスト2件も `BackoffWaitOutcome` の期待値アサートに更新されている
  (`:2596-2615` 付近)。
- **`cross_family_fallback` が `None` のとき**: `build_cross_family_target` が
  `Ok(None)` を返し、`resume_with_backoff_until_deadline` の
  `if let Some(fallback_target) = cross_family_target` が常に外れるので、
  切替は起きず従来通り STUN の120秒窓で give up する。ADR タスク2の
  3つ目の bullet 通り。
- **ADR §9 の唯一の残作業 R3-C5 の実在確認**: `isekai-ssh/src/wrapper.rs:670-671` に
  `lightweight_retries` と `RedeployGate::new()` があり、`:801-826` で
  `lightweight_retries > MAX_LIGHTWEIGHT_RETRIES` 時に
  `redeploy_gate.try_consume()` で再デプロイへエスカレーションしている。
  **ADR §4.2 が「未確認、もし存在しなければ非目標ではなくバグに格上げ」と
  していた経路は実在する**——したがって §4.2 は非目標のままでよい。
  ただし C1 はこの経路への到達自体を塞ぐ、という点は再掲しておく。
- **サーバー側(`engine/`)の無変更**(タスク6): diff に含まれておらず、
  ADR の「サーバーは既に `SessionId` のみをキーに任意アドレスからの
  RESUME を受理する」という前提に沿っている。
- **preempt ラッチ非実装**(タスク8): ADR の「実測してから決める」方針通り。
  `STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS` の doc(`:82-88`)に
  その判断理由とサーバー側 preempt 待ち(2秒)が切替窓(約15秒)に
  収まることまで書かれており、ADR の要求(「タスク1のトリガー/
  タイムアウト設計ではこれを見込むこと」)を満たしている。

---

## 優先度つきの推奨アクション

1. **C1 を修正する**(必須)。切替時の窓と、切替成功後の窓を分ける。
   これを直さないと、この機能は「うまくいけば体験が良くなり、
   外れたら以前より遥かに悪くなる」変更になる。
2. **M2 を修正する**(必須)。`?` を `log::warn!` + `None` に変える。
   1行の変更で always-connects 違反が消える。
3. **M3 を検討する**(強く推奨)。ネットワーク変化起因でも1回は現targetを試す。
4. m9 のテストを追加する(C1/M3 の修正と同時に書けば期待値を固定できる)。
5. m1・m2・m8 は機械的に直せる小修正。
6. m3・m4 は §6 の計測品質を上げる改善で、実運用データを取り始める前に
   入れておくと後から取り直さずに済む。

---
---

# Round 2: 修正コミット `83d6eb6d` の再確認

対象: `git diff df0e3221..83d6eb6d`(`connect.rs` / `resume_loop.rs`)
依頼された重点: (a) C1修正がADRタスク4(30秒下限・数十秒上限)とタスク7
(成功後にrelay相当)の**両方**を満たしているか、(b) M3修正が streak /
一方向切替のロジック(round 1 依頼事項2の結論)を壊していないか。

## 結論(先に)

**M2 と m1 と m8 は完全にクローズ。M3 はほぼクローズ(残課題1件、minor)。
C1 は「10日ハング」という critical な部分は完全に解消したが、
ADR タスク4 の「最大3回の有界リトライ」は netmon が無言の経路で満たせておらず、
実質1回しか cross-family target を試さないケースが残る(major)。**
一方向切替と streak のロジックは壊れていない(詳細は下記 OK-2)。

---

## 残 MAJOR

### R1. `CROSS_FAMILY_SWITCH_DEADLINE` を `disconnected_at` 起点にしたため、切替が遅いケースで cross-family target を1回しか試せない(タスク4「最大3回」未達)

**該当**: `rust-core/isekai-pipe/src/resume_loop.rs:1286-1288`

```rust
max_resume_window = Some(CROSS_FAMILY_SWITCH_DEADLINE);           // 45s
resume_window = effective_resume_window(effective_resume_grace_secs, max_resume_window);  // = 45s
deadline = disconnected_at + resume_window;                       // = disconnected_at + 45s
```

`CROSS_FAMILY_SWITCH_DEADLINE` の doc(`:93-122`)は

> a switch that happens at, say, `STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS`'s own
> ~15.5s still leaves ~30s of budget against the cross-family target

と、**切替が約15.5秒で起きる前提**で予算を計算している。しかしこの15.5秒は
`RESUME_BACKOFF` の待ち時間の累積(500ms/1s/2s/4s/8s)だけで、
**各試行そのものにかかる時間を含んでいない**。

`reconnect_and_resume` の1試行は `isekai-transport/src/resume.rs:94` の
`TRANSPORT_STEP_TIMEOUT = 15秒` で上限を切られている
(`resume.rs:734-744` の connect ステップ、加えて `:814` の resume ステップにも
同じ15秒)。そして本機能が救おうとしている当のケース——クライアントの
アドレスが変わり、サーバー側 restricted-cone NAT がパケットを落とす——では、
STUN bare redial は即座に `ECONNREFUSED` で返ってこず、**15秒のハンドシェイク
タイムアウトを使い切って失敗する**のが典型。

**失敗シナリオ**(netmon が無言の環境 = ADR §3.2 タスク1 が論理和にした理由そのもの):

| t | 出来事 |
|---|---|
| 0s | 切断。`deadline = 0 + 120s`(STUN窓) |
| 0.5s | backoff(0.5s)明け、STUN試行1 → 15秒タイムアウト |
| 15.5s | 失敗、`stun_failures = 1` |
| 31.5s | 試行2失敗(backoff 1s + 15s)、`= 2` |
| 48.5s | 試行3失敗、`= 3` |
| 67.5s | 試行4失敗、`= 4` |
| 90.5s | 試行5失敗、`= 5` |
| 100.5s | backoff(10s)明け、`stun_failures >= 5` → **切替**。`deadline = 0 + 45s`(**既に55秒前に過ぎている**) |
| 100.5s | cross-family target へ**1回だけ**試行 |
| ~115s | その1回が失敗すると、ループ先頭の `now >= deadline` で即give-up |

つまり cross-family target への試行は **1回**。ADR §3.2 タスク4 の

> cross-family resume は**短い有界リトライ**(目安: 最大
> `UNKNOWN_SESSION_CONFIRM_THRESHOLD` 回、合計10〜30秒程度)にし、
> **既存の streak ロジックがそのまま意味を持つようにする**

という要求——特に後半「streak ロジックがそのまま意味を持つように」——が
満たせない。1回しか試さないなら `UNKNOWN_SESSION_CONFIRM_THRESHOLD = 3` には
到達しえず、タスク4が単発試行を禁じた理由(単発では `UnknownSession` の
3状況を区別できない)がそのまま再発する。加えて、relay 側の
一過性の失敗(まだ park されていないレース)1回で連続性を捨てることになる。

さらに構造として逆転している点も指摘しておく: **切替は残り予算を
「増やす」べきなのに、この実装では 120s → 45s へ減らす**。上記の表では
切替の瞬間に予算が負になっている。

**ネットワーク変化起因の経路(`switch_attempts_before_cross_family = 1`)では
問題ない**ことも確認した: 切替は t≈16.5s に起き、`deadline = 45s` なので
約28.5秒・2〜3試行の予算が残り、30秒下限も満たす。**つまりこれは
「netmon が無言 / イベントを出さないネットワーク変化」の経路だけに出る穴**で、
ADR タスク1 がまさに論理和にしてまで救おうとした経路。

**推奨修正**(2行): 予算を「切替の瞬間」起点にし、30秒下限との `max` を取る。
`max_resume_window` を `Some(..)` に保つ形は維持できるので、
`notify_on_give_up` も `run_resume_loop` 側の昇格ロジックもそのままでよい。

```rust
// 切替時点からの相対予算。ただし disconnected_at からの経過が
// UNKNOWN_SESSION_MIN_ELAPSED_FLOOR を下回らないようにする。
let elapsed = Instant::now().saturating_duration_since(disconnected_at);
max_resume_window = Some((elapsed + CROSS_FAMILY_SWITCH_DEADLINE).max(UNKNOWN_SESSION_MIN_ELAPSED_FLOOR));
resume_window = effective_resume_window(effective_resume_grace_secs, max_resume_window);
deadline = disconnected_at + resume_window;
```

これで `deadline = 切替時刻 + 45秒`(かつ最低でも `disconnected_at + 30秒`)となり、
タスク4の下限・上限の両方と、上表のケースでの3試行以上が同時に満たせる。
`CROSS_FAMILY_SWITCH_DEADLINE` の doc の「not from the moment of the switch itself」
という一文も併せて書き換えが必要(現状は意図的に絶対時刻を選んだと読めるので、
このままだと次の読み手が「意図的な設計」と誤解して直さない)。

---

## 残 MINOR

### R2. `NetworkChanged` トリガーは依然として0試行の切替を許す(M3の穴が一部残存)

`resume_loop.rs:1266-1267`:

```rust
if switched_this_call.is_none()
    && (backoff_wait_outcome == BackoffWaitOutcome::NetworkChanged || stun_failures >= switch_attempts_before_cross_family)
```

`stun_failures == 0`(まだ1回も試していない)のまま、最初の backoff(約500ms)中に
OS がもう1つネットワーク変化イベントを出せば、`NetworkChanged` 側の条件だけで
切替が起きる。netlink は1回の Wi-Fi 遷移で link down / link up / addr / route と
**数百ms以内に複数イベントを出すのが普通**なので、M3 が問題にした
「健全な STUN セッションを1回も試さずに捨てる」経路は実質残っている。

C1 修正後なので被害は限定的(45秒の有界プローブ→失敗すれば wrapper へ戻る、
成功すればそのセッションの残りは relay 固定で P2P を失うだけ)なので minor。
直すなら 1 行:

```rust
&& ((backoff_wait_outcome == BackoffWaitOutcome::NetworkChanged && stun_failures >= 1)
    || stun_failures >= switch_attempts_before_cross_family)
```

(`episode_started_by_network_change` の場合は `switch_attempts_before_cross_family == 1`
なので、この2条件は同じ意味に縮退し、挙動は変わらない。)

### R3. m2(誤分類)は据え置きだが、C1修正で発生確率が上がっている

`resume_loop.rs:1227-1241` のコメントは書き直されたものの、
「never an `UnknownSession` rejection, or the streak give-up below would have
fired first」という断定はそのまま残っている。R1 の通り cross-family target への
試行回数が1〜3回に減ったため、`UnknownSession` が来ていても streak が3に
到達せず deadline 側で give-up する確率は**修正前より上がっている**。
その場合 `continuity-lost reason: relay-unreachable` と記録されるが、実際は
session-gone であり、ADR §3.3 の前提検証データが汚れる。
据え置き判断自体は妥当(実害は計測品質のみ)だが、R1 を直すなら
ついでに「最後のエラーが `UnknownSession` だったか」を見て分岐するのが安い。

### R4. `m5`(ライブ表示の分母)は切替中は解消、切替成功後の episode では `864000s` のまま

切替中は `resume_window = 45s` なので `(20s/45s)` と表示される(解消)。
切替成功後の次以降の episode は `run_resume_loop:1694-1697` が
`resume_window = effective_resume_window(grace, None)` を入れるため
`(20s/864000s)` になる。ただしこれは**素の relay 経路
(`run_relay_resumable`)の既存挙動とまったく同じ**なので、
新たな不整合ではなく仕様通り。指摘を取り下げる。

---

## 修正が正しいことを確認した項目

### OK-1. C1 の critical 部分(10日ハング)は完全に解消

- `max_resume_window = None` を書く箇所は、リポジトリ全体で
  `resume_loop.rs:1694`(`run_resume_loop` 内、
  `resume_with_backoff_until_deadline` が `Ok` を返し `switched` が `Some`
  だったときのみ実行される分岐)**1箇所だけ**になった
  (`grep -n "max_resume_window = None"` で確認)。
  これは ADR §3.2 タスク7 の「cross-family resume が**成功した後**」という
  文言と厳密に一致する。**タスク7 は満たされている。**
- 切替直後は `Some(CROSS_FAMILY_SWITCH_DEADLINE)` なので、
  `.claude/rules/always-connects.md` が依存する
  `isekai-ssh/src/wrapper.rs:670-671` / `:801-826` の
  `lightweight_retries` → `redeploy_gate` エスカレーションへ、
  最悪でも十数秒〜約2分で制御が戻る。round 1 の「10日ハング」は消えた。
- `notify_on_give_up = max_resume_window.is_none()`(`:1289`)は切替後 `false` に
  なる。有界プローブ中の give-up で OS 通知を出さないのは、
  `ResumeDeadlinePolicy` の doc が言う「軽量な STUN リトライでユーザーを
  通知で埋めない」方針と整合し、切替成功後の episode では `None` 経由で
  通知が有効に戻る。意図通り。
- **パニックの懸念なし**: 切替で `deadline` が過去に縮んでも、
  `deadline - now`(`:1259`)を評価するのは同一 iteration の**切替より前**で、
  縮んだ deadline は次の iteration 先頭の `now >= deadline`(`:1204`)で
  安全に処理される。また切替と同じ iteration 内で必ず
  `reconnect_and_resume`(`:1292`)が1回呼ばれるため、
  「切替したのに1回も試さず give-up」は構造上起こりえない。

### OK-2. M3 修正は streak / 一方向切替のロジックを壊していない(重点確認事項(b))

round 1 依頼事項2 の結論がそのまま維持されていることを、変更点ごとに確認した:

- **一方向切替(罠(b))**: 切替ガード `switched_this_call.is_none()`(`:1266`)は
  そのまま。切替後 `current_target` は `reconnect_and_resume`(`:1292`)と
  `reestablish_control_stream`(`:1321` 付近)の両方で一貫して使われ、
  STUN target へ戻る経路はコード上存在しない。episode をまたぐ側も
  `run_resume_loop` が `cross_family_target = None` にして以後の切替判定を
  無効化する(`:1691` 付近)。**M3 修正は切替の「タイミング」だけを変え、
  「一度切り替えたら戻らない」性質には一切触れていない。**
- **`stun_failures` のカウント条件**: `switched_this_call.is_none()` ガード付きの
  インクリメント(`:1356-1358`)は無変更。切替後に増えないので、
  `switch_attempts_before_cross_family` との比較が二重発火することはない。
- **`switch_attempts_before_cross_family` の定数化**: `:1200` で
  ループに入る前に1度だけ決まる不変値で、ループ内で書き換わらない。
  `episode_started_by_network_change` を「即時切替」ではなく
  「閾値を5→1に下げる」に翻訳したのは、round 1 の M3 で推奨した
  「少なくとも1回は現 target を試す」をそのまま実装した形で、
  既存のトリガー構造(1箇所の論理和)に吸収されているため
  分岐が増えていない。良い直し方。
- **streak(`state.consecutive_unknown_session`)**: 触られていない。
  切替時にリセットしない点も維持されており、ADR §3.2 タスク4 の
  「session 単位で持つこと自体は正しい」という明示的承認と整合。
- **30秒下限(罠(a))**: `deadline = disconnected_at + 45s > 30s` なので、
  `update_unknown_session_streak`(`:1078` 付近)の
  `elapsed_since_disconnect >= UNKNOWN_SESSION_MIN_ELAPSED_FLOOR` は
  窓の内側で必ず満たされうる。round 1 では「C1 の副作用として偶然成立して
  いる」状態だったが、今回 `CROSS_FAMILY_SWITCH_DEADLINE` の doc(`:113-118`)に
  「Must stay comfortably above `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR` (30s)」と
  **意図として明記された**ので、将来この定数を縮める変更が来ても
  レビューで止まる。round 1 で懸念した「意図がどこにも表現されていない」は解消。
  (ただし R1 の通り、窓の**内側に試行が何回入るか**は別問題。)

### OK-3. M2 は完全にクローズ

`connect.rs:826-841` の `build_cross_family_target` は
`Option<RelayTarget>` を返し、検証失敗は `log::warn!` の上 `None`。
呼び出し元2箇所(`:718`、`:768`)の `?` も削除済み。
壊れた `cross_family_fallback` は cross-family resume を無効化するだけで、
プライマリの STUN P2P 接続には一切影響しない。
`.claude/rules/always-connects.md` 違反は解消。
doc コメント(`:816-825`)にこの「絶対に呼び出し元を失敗させない」という
不変条件と理由が明記されているのも良い(将来 `?` に戻す変更を止められる)。
`try_build_cross_family_target` への分離も、検証ロジック自体は
1文字も変えずに移しているだけであることを diff で確認した。

### OK-4. m1・m8 はクローズ

- **m1**: `BackoffWaitOutcome` が `:888-896` へ移り、独自の doc を持ち、
  `:898-909` の doc が `wait_backoff_or_network_change` に正しく付き直した。
- **m8**: `run_resume_loop:1495-1508` に、ADR タスク7 最終 bullet が
  求めていた「`experimental_network_rebind` / warm-standby を切替後に
  再導出しないと決めた」旨と、その安全性の根拠(両者が共存しえない)が
  コメントとして記録された。round 1 の m6(warm-standby のペア不整合)も
  同じコメントでカバーされている。

### OK-5. 据え置き判断は妥当

m3(telemetry の `attempts`/`elapsed` 固定0)・m4(理由を別ログ行に出す)・
m7(replay 不整合 `continue` が `stun_failures` を増やさない)・
m9(テスト未追加)を今回見送った判断は妥当。いずれも実害は
「計測品質」か「稀な経路」に限られ、機能の正否を左右しない。
ただし m9 については、**R1 は典型的に「定数の起点(相対/絶対)を取り違えた」
バグであり、`resume_with_backoff_until_deadline` を直接呼ぶ既存テストの形
(`:2440` / `:2520` 付近)で `cross_family_target = Some(..)` を渡し
`FailingMuxFactory` を使えば、試行回数を数えるだけで確実に固定できる**ことを
改めて挙げておく。R1 を直すタイミングで1本だけ足す価値は高い。

---

## Round 2 の推奨アクション

1. **R1 を修正する**(必須)。`max_resume_window` を切替時刻起点にし
   `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR` との `max` を取る(上記2行)。
   併せて `CROSS_FAMILY_SWITCH_DEADLINE` の doc から
   「not from the moment of the switch itself」を削除する。
2. R2 を1行で直す(推奨)。M3 の意図を完全に達成できる。
3. R1 の修正に合わせて、cross-family target への試行回数を数える
   テストを1本足す(m9 の部分的な解消)。
4. R3 は R1 を直すついでに検討すればよい。

R1・R2 が入れば、ADR §3.2 のタスク1〜9はすべて意図通りに実装された状態になり、
このブランチはマージ可能と判断してよい。

---
---

# Round 3: 修正コミット `8fa7a9f6` の再確認 — **収束**

対象: `git diff 83d6eb6d..8fa7a9f6`(`resume_loop.rs` のみ)
依頼された重点: 新しい `elapsed_since_disconnect` 起点の計算が、
「disconnected_at 起点の下限」と「switch 起点の予算」の**両方**を
同時に満たせているか。

## 結論

**R1・R2 とも意図通りに修正されている。未解決の指摘は残っていない。
このブランチはマージ可能と判断する。**
m9(テスト)・R3(誤分類)の据え置き判断も妥当(条件は下記 F-3)。

---

## R1 の検証: 両方の下限を同時に満たしている

**該当**: `resume_loop.rs:1313-1314`

```rust
let elapsed_since_disconnect = Instant::now().saturating_duration_since(disconnected_at);
max_resume_window = Some((elapsed_since_disconnect + CROSS_FAMILY_SWITCH_DEADLINE).max(UNKNOWN_SESSION_MIN_ELAPSED_FLOOR));
resume_window = effective_resume_window(effective_resume_grace_secs, max_resume_window);
deadline = disconnected_at + resume_window;
```

`resume_window = min(resume_window_for(grace), elapsed + 45s)` なので、
既定 grace(10日)では `deadline = disconnected_at + elapsed + 45s`
= **切替の瞬間 + 45秒**。依頼された2条件を個別に確認した:

**(1) switch 起点の予算 = 常に45秒**。Round 2 の R1 で挙げた
「netmon 無言 + 15秒タイムアウト」シナリオを再計算:

| t | 出来事 |
|---|---|
| 90.5s | STUN 試行5回目が失敗、`stun_failures = 5` |
| 100.5s | backoff 明け → 切替。`elapsed = 100.5` → `max_resume_window = 145.5s` → `deadline = t=145.5s` |
| 100.5〜145.5s | cross-family target に**45秒の予算**(round 2 修正前は 0 秒、実質1試行だった) |

Round 2 で指摘した「切替が予算を 120s → 45s に**減らす**」という逆転も解消。

**(2) disconnected_at 起点の 30秒下限 = 常に満たされる**。
`elapsed >= 0` かつ `CROSS_FAMILY_SWITCH_DEADLINE (45s) > UNKNOWN_SESSION_MIN_ELAPSED_FLOOR (30s)`
なので `elapsed + 45s >= 45s > 30s` が恒真。最速の切替
(ネットワーク変化経路、STUN 試行1回が即失敗 → t≈0.6s)でも
`deadline = 45.6s` で 30秒下限を割らない。
ADR §3.2 タスク4 の「有界リトライ窓は `disconnected_at + 30秒` に到達するまでは
終わらせないこと」は満たされている。

**(3) タスク4「最大3回の有界リトライ」と streak の噛み合い**——これが
Round 2 で最も懸念した点だが、45秒の予算で十分足りることを確認した。
鍵は「`UnknownSession` は**サーバーが応答した**ときにしか返らない」こと:
`is_unknown_session_rejection` が見る `ResumeRejected(UnknownSession)` は
QUIC ハンドシェイクと RESUME 往復が成功して初めて得られる値なので、
その種の失敗は `TRANSPORT_STEP_TIMEOUT`(15秒)ではなく
サブ秒〜数秒で返る。したがって:

- **セッション消滅ケース**(streak を積みたいケース): 試行が速いので、
  `RESUME_BACKOFF`(0.5/1/2/4/8/10s)と合わせて45秒の中に
  6〜7回の試行が入る。`UNKNOWN_SESSION_CONFIRM_THRESHOLD = 3` に
  余裕で到達し、streak 判定が**本来の意味で機能する**。
- **relay 到達不能ケース**(15秒タイムアウトが効くケース): 45秒で
  2〜3試行。ここでは streak は積まれようがなく、deadline 側の
  give-up が `continuity-lost reason: relay-unreachable` を記録するのが
  **正しい分類**。

つまり「試行が遅いのは到達不能のときだけ / streak を必要とするのは
試行が速いときだけ」という自己整合があり、45秒という単一の予算で
両方を成立させられている。**タスク4は満たされた。**

**(4) `.max(UNKNOWN_SESSION_MIN_ELAPSED_FLOOR)` は現状 no-op**
(上記(2)の通り左辺が常に大きい)。実行時に効くことはないが、
`CROSS_FAMILY_SWITCH_DEADLINE` を将来30秒未満に縮める変更が来たときの
ガードとして機能するので、残すのが正しい。
(意図はコメント `:1306-1311` に明記済み。)

**(5) 副作用の確認**: `resume_window` が `elapsed + 45s` になるため、
ライブ表示(`print_reconnect_status(is_tty, disconnected_at, resume_window)`)の
分母は「切断からの総許容秒数」となり、分子(`disconnected_at` からの経過秒)と
起点が揃う。`(20s/61s)` のように読めて意味が通る。表示上の不整合なし。

**(6) 型・パニックの確認**: `Instant::now().saturating_duration_since(disconnected_at)`
でアンダーフローなし、`Duration + Duration` / `Duration::max`(`Ord` 由来)
ともに問題なし。`deadline` が過去になりうるのは
`grace < elapsed + 45s`(ユーザーが `--resume-grace` に極端に小さい値を
指定した場合)だけで、その場合はサーバー側でもセッションが消えているので
即 give-up が正しい挙動——かつこれは STUN 経路の既存挙動と同じ
(`clamp_resume_window` の性質)で、本変更が持ち込んだものではない。

## R2 の検証: 0試行切替は不可能になった

**該当**: `resume_loop.rs:1276-1278`

```rust
if switched_this_call.is_none()
    && ((backoff_wait_outcome == BackoffWaitOutcome::NetworkChanged && stun_failures >= 1)
        || stun_failures >= switch_attempts_before_cross_family)
```

- `switch_attempts_before_cross_family >= 1` が両分岐で保証されている
  (`:1200` の `if ... { 1 } else { STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS }`)ため、
  **どちらの disjunct も `stun_failures >= 1` を含む**。切替前に必ず1回は
  現在の target への `reconnect_and_resume` が実行される。M3 の意図が完全に達成された。
- `episode_started_by_network_change == true` のときは
  `switch_attempts_before_cross_family == 1` なので2つの disjunct が同じ意味に縮退し、
  挙動は Round 2 時点から変わらない(= ネットワーク変化起因では1試行後に即切替)。
  ADR §3.2 タスク1 の「120秒待たない」要求は維持されている。
- 一方向切替(`switched_this_call.is_none()` ガード)・`stun_failures` の
  インクリメント条件・streak・`run_resume_loop` 側の昇格ロジックは
  いずれも無変更。Round 2 の OK-2 の結論はそのまま有効。

## Round 3 時点の残項目(いずれもマージ阻害要因ではない)

### F-1. R3(`relay-unreachable` 誤分類)— 据え置き妥当、発生確率は改善

Round 2 では「試行回数が減ったので誤分類が増える」と書いたが、R1 修正で
45秒の予算が確保され、かつ上記(3)の通り `UnknownSession` 系の失敗は
速いので、45秒内に streak が3に到達しやすくなった。**Round 2 時点より
誤分類は起きにくい。** 残るのは「`UnknownSession` と他エラーが交互に来て
streak がリセットされ続ける」という元々の狭いケースのみで、
実害は §6 の計測データがわずかに汚れることに限られる。据え置きで問題ない。

### F-2. m3 / m4 / m7 — 据え置き妥当

いずれも計測品質か稀な経路に限られ、機能の正否を左右しない
(Round 2 の OK-5 の判断から変更なし)。

### F-3. m9(テスト未追加)— 今回の据え置きは受け入れるが、follow-up として名前を付けて残すことを推奨

「次の機会に回す」という判断自体は、Round 3 で未解決の指摘がゼロになった
以上、マージを止める理由にはならない。ただし記録として:
**この機能は3ラウンドのレビューで correctness の修正が3回入っており
(C1 → R1 → 今回の確定)、そのいずれもテストではなくレビューが検出した。**
特に C1/R1 は「定数の起点(相対か絶対か)を取り違える」という、
`resume_with_backoff_until_deadline` を直接呼ぶ既存テストの形
(`:2440` / `:2520` 付近、`FailingMuxFactory` + `NoopNetworkChangeMonitor`)で
`cross_family_target = Some(..)` を渡して**試行回数と give-up 時刻を数えるだけ**で
確実に固定できる種類のもの。次にこのコードを触る人がレビューなしで
同じ取り違えを再導入するのを止めるものが現状ゼロなので、
「次の機会」を漠然と残すのではなく、ADR か `ISEKAI_PIPE_DESIGN.md` Epic S の
末尾に「未実施: cross-family 切替の予算/試行回数の回帰テスト」という
1行の follow-up として明記しておくことを推奨する。

---

## 最終判定

ADR_STUN_REESTABLISH_CONTINUITY.md §3.2 のタスク1〜9は、すべて意図通りに
実装されていることを確認した。round 1 の critical 1件・major 2件・minor 9件、
round 2 の major 1件・minor 3件について、対応されたものは正しく対応され、
据え置かれたものはいずれも実害が計測品質か稀な経路に限られる。
**correctness 観点での指摘は残っていない。CI(`rust-core-test-linux` 等)が
緑になることを確認のうえ、マージしてよい。**

---
---

# Round 4: `a441addf` + `7b85a2e7`(`/code-review` 由来の追加修正)の再確認

対象: `git diff 8fa7a9f6..7b85a2e7`
依頼された重点: Site A(`NetworkChanged`、待機直後)と Site B(回数条件、`Err` アーム内)が
同一イテレーションで二重発火しないか / 一方向切替・streak ロジックとの相互作用。

## 結論

**二重発火はしない(構造的に排他)。一方向切替・streak・budget 計算のいずれも
壊れていない。タイミング修正そのものは正しい。**
新規指摘は **minor 1件(N1、計装の帰属)** と記録用の注記2件のみで、
correctness の退行はない。**マージ可の判断は維持。**

---

## 重点確認(a): Site A / Site B の排他性 — **構造的に排他、二重発火なし**

| | 位置 | ガード |
|---|---|---|
| Site A | `resume_loop.rs:1315`(backoff 待機直後) | `switched_this_call.is_none() && backoff_wait_outcome == NetworkChanged && stun_failures >= 1` |
| Site B | `resume_loop.rs:1401`(`Err` アーム、`stun_failures` インクリメント直後) | 外側 `if switched_this_call.is_none()` の中の `stun_failures >= switch_attempts_before_cross_family` |

同一イテレーション内の実行順は **A → 試行 → B** で、A が発火すると
`switched_this_call = Some(..)` になるため、

- B の**外側**ガード `if switched_this_call.is_none()`(`:1394`)が false になり、
  B 本体だけでなく `stun_failures` のインクリメント自体もスキップされる。
- 逆に B が発火した場合、次イテレーション先頭の A は `switched_this_call.is_none()` が
  false で素通りする。

いずれか一方しか発火せず、かつ**全体で高々1回**。
`switched_this_call` が単調(None → Some のみ、Some → None への遷移がない)なことも
確認したので、一方向切替の不変条件は維持されている。

`switch_attempts_before_cross_family == 1`(ネットワーク変化起因 episode)のときは
B が必ず最初の失敗で発火するため A は事実上到達不能になるが、これは冗長なだけで
害はない(round 2 の R2 修正で A にも `stun_failures >= 1` を入れた意図と一致)。

## 重点確認(b): streak / budget との相互作用 — 壊れていない

- **`stun_failures` のカウント**: 切替後は外側ガードで増えなくなる(`:1394`)。
  cross-family target への失敗が STUN 側のカウンタに混入しない。round 1 からの
  性質が維持されている。
- **`state.consecutive_unknown_session`(streak)**: 一切触られていない。
  切替時にリセットしない仕様(ADR §3.2 タスク4 が明示承認)も維持。
- **budget 再計算**: 2箇所とも `cross_family_switch_budget()`(`:1218-1223`)を
  呼ぶ形に集約され、`max_resume_window` / `resume_window` / `deadline` の
  3値が必ず同時に更新される。C1/R1 で2度起きた「導出値の同期漏れ」の
  再発経路が構造的に塞がれた。**round 3 で確認した計算式は1文字も変わっていない**
  (`(elapsed + 45s).max(30s)` → `effective_resume_window` → `disconnected_at + resume_window`)。
- **`notify_on_give_up` の削除**: 旧変数は初期化時と切替時にしか書かれず、
  常に `max_resume_window.is_none()` と同値だった。2つの give-up 地点
  (`:1273`、`:1444`)で直接評価する形に置き換わっており、意味は完全に等価。
  ミュータブルな導出値が1つ減ったのは、このADRが C1/R1 で踏んだ事故の
  同型リスクを1つ潰す妥当な変更。
- **`record_continuity_lost_if_applicable`(`a441addf`)**: 旧2箇所のガード
  `already_cross_family || switched_this_call.is_some()` を
  `if !already_cross_family && !switched_this_call { return; }` に
  ド・モルガン変換しただけで、telemetry 引数の並びも含めて等価。
  忠実なリファクタであることを diff で確認した。
- **タイミング修正の正しさ**: B が「Nコ目の失敗と同じ地点」で発火するため、
  切替までの backoff 累積は 0.5+1+2+4+8 = **15.5秒**となり、
  `STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS` の doc が主張する「roughly 15s」と一致する。
  指摘(実測 ~25.5秒)は正しく、修正も正しい。
  なお試行そのものに掛かる時間(最大15秒 × N)はこの15.5秒に含まれないが、
  それは R1 修正で budget を切替時刻起点にしたことで既に吸収済み。

---

## 新規指摘

### N1(minor). Site B が streak give-up の**前**にあるため、cross-family target を1回も試していないのに `"continuity-lost" / session-gone` が記録されうる

**該当**: `resume_loop.rs:1393-1414`(Site B)と `:1431-1460`(streak give-up)の順序

`Err` アームは現在この順で実行される:

1. `stun_failures += 1` → 閾値到達なら **Site B が切替**(`switched_this_call = Some(..)`)
2. `update_unknown_session_streak(...)` → `should_give_up` なら
   `record_continuity_lost_if_applicable(already_cross_family, switched_this_call.is_some(), ..., "session-gone")`
   を呼んで `return Err`

**失敗シナリオ**: `switch_attempts_before_cross_family = 5` の episode で、
STUN peer は到達可能だがサーバー側でセッションが消えている。

- 試行1〜4 が `UnknownSession`(streak 1〜4)。ただし `elapsed < 30s` のうちは
  `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR` が効いて give-up しない。
- 試行5 が `UnknownSession`。ここで **まず Site B が発火**して
  `switched_this_call = Some(..)` になり、**その直後**に streak 判定が
  `elapsed >= 30s` を満たして give-up → `switched_this_call.is_some() == true` のまま
  `"continuity-lost" / session-gone` を記録して `return Err`。

つまり **cross-family target へは1回も `reconnect_and_resume` していないのに、
「cross-family resume も失敗した」を意味する計装が出る**。
ADR §6 は分母を「cross-family resume が実際に試みられた回数」と定義しているので、
この1件は分母・分子の両方を1ずつ水増しし、§3.3 の前提(relay の到達可能性)を
検証するための比率をわずかに歪める。

この修正コミット以前は、切替判定が次イテレーション先頭にあったため
streak give-up のほうが先に `return` し、この記録は出なかった。
**タイミング修正の副作用として新しく入った経路。**

**推奨修正**(ロジックの移動のみ、1ブロック): Site B のブロックを
`if should_give_up { ... return Err(...) }` の**後ろ**へ移す。
`stun_failures` のインクリメントは今の位置(ガードの中)のままでよい。
これでタイミング修正の利得(余分な backoff を1回待たない)は保ったまま、
「切替を記録するのは実際に cross-family を試す直前だけ」に戻せる。

なお挙動面では、この経路で `return Err` すること自体は**正しい**。
`UnknownSession` は ADR §3.2 タスク4 が明記する通り
「session_id についての主張であってアドレスについての主張ではない」ため、
cross-family へ移っても同じ拒否が返るだけで救済にならない。
直すべきは計装の帰属だけ。

### N2(記録のみ). round 2 で確認した「切替と同一イテレーション内で必ず1回は試行される」という不変条件は、Site B については成立しなくなった

Site A は「切替 → 同じイテレーション内で `reconnect_and_resume`」だが、
Site B はイテレーション末尾で切替えるため、次に起きるのは
ループ先頭の `now >= deadline` 判定(`:1264`)である。したがって
「切替えたのに cross-family を1回も試さず give-up」が理論上ありうる。

実際に到達する条件は2つだけで、どちらも結果としては正しい挙動:

- `grace_window <= elapsed`(ユーザーが `--resume-grace` に極端に小さい値を
  指定し、サーバー側の猶予も既に尽きている)→ 即 give-up が正しい。
  既定の10日 grace では到達しない。
- N1 の streak give-up 経路 → 上述の通り give-up 自体は正しい。

実害はないが、round 2 の OK-1 で「構造上起こりえない」と書いた不変条件が
弱まった事実は記録しておく(次にこの関数を触る人が round 2 の記述を
根拠に何かを仮定しないように)。N1 を修正すれば、残るのは1つ目の
条件だけになる。

### N3(trivia). 切替ログが、それを引き起こした失敗のログより先に出る

Site B は `log::info!("switching to cross-family relay fallback (...)")` を
出した後で、同じ `Err` アームの末尾(`:1468` 付近)が
`resume attempt N failed: ...` を出す。ログファイル上は
「切替 → その原因の失敗」という逆順に見える。
`--isekai-log-file` を人手で読む運用(ADR §6)なので、気づけば読み解けるが、
切替ログを `Err` アーム末尾(失敗ログの後)に置くか、失敗の要約を
切替ログ側に含めると読みやすい。修正は任意。

### R3 は引き続き有効(据え置きのまま)

deadline give-up 側のコメント(`:1283-1286`)の
「never an `UnknownSession` rejection, or the streak give-up below would have
fired first」は依然として厳密には不正確(`UnknownSession` が他エラーと
交互に来ると streak がリセットされ、deadline 側で give-up しうる)。
round 2 の R3 と同じ内容で、据え置き判断も変わらず妥当。

---

## Round 4 の推奨アクション

1. **N1 を直す**(推奨、必須ではない)。Site B のブロックを
   streak give-up の `return` の後ろへ移すだけ。計装の正確さは
   ADR §6 の成功基準を測る唯一の手段なので、運用データを取り始める前に
   入れておく価値がある。
2. N2 は記録のみ。N3・R3 は任意。
3. m9(回帰テスト)は `ISEKAI_PIPE_DESIGN.md` Epic S 末尾に
   フォローアップとして明記済み(`2bb9e098`)であることを確認した。
   round 3 の F-3 で推奨した対応が取られている。

**N1 を直しても直さなくても correctness の退行はない。CI が緑ならマージしてよい。**

---
---

# Round 5: `f489b97c`(N1 修正)ほか3コミットの確認 — **見落としなし**

対象: `git diff 7b85a2e7..f489b97c`(`0b4666bd` / `100286a9` / `7f682f7b` / `f489b97c`)

## N1 修正は正しい

Site B のブロックが `if should_give_up { ... return Err(...) }` の**後ろ**
(`resume_loop.rs:1483-1502`)へ移り、ガードが
`switched_this_call.is_none() && stun_failures >= switch_attempts_before_cross_family`
として明示された。確認した点:

- **N1 の経路は閉じた**: streak give-up が先に `return` するため、
  `record_continuity_lost_if_applicable(..., switched_this_call.is_some(), ..., "session-gone")`
  (`:1469`)に渡る値は、この経路では必ず `false`(`already_cross_family` でない限り)。
  cross-family を1回も試していないのに計装が出ることはなくなった。
- **排他性は維持**: `stun_failures` のインクリメントは依然
  `if switched_this_call.is_none()`(`:1424`)の中。Site B 自身にも
  同じ条件が入ったので、Site A が発火済みのイテレーションでは
  どちらも走らない。全体で高々1回・一方向という不変条件は保たれている。
- **タイミング修正の利得は失われていない**: Site B は依然として同じ
  イテレーションの `Err` アーム内にあり、次の backoff 待機に入る前に
  切替が適用される。`7b85a2e7` が直した「余分な1回の待機(~25.5s)」は
  再発しない。
- **streak との関係**: `state.consecutive_unknown_session = streak`(`:1440`)は
  Site B より前で更新済みなので、切替後も streak が引き継がれる
  (ADR §3.2 タスク4 が明示承認した仕様)。変更なし。
- **N2 の残り**: N1 経路が閉じたことで、round 4 の N2(「切替したのに
  1回も試さず give-up」)に到達しうる条件は
  「`grace_window <= elapsed`(極端に小さい `--resume-grace`)」1つだけになった。
  その場合の即 give-up は正しい挙動。記録として残すだけでよい。

## 併せて入った2コミットも確認した(いずれも妥当)

- **`7f682f7b`(切替時に `attempt = 0`)**: 正しい修正。`RESUME_BACKOFF.delay_for_attempt`
  は `attempt` のみの純関数なので、リセットしないと初めて触る
  cross-family target への最初のプローブが STUN 側で積み上がった
  カウントに基づく ~10秒(cap)待ちを食い、45秒しかない probe 窓を
  大きく削っていた。リセット後は 0.5/1/2/4/8/10s と刻み直すため、
  45秒の窓に5〜6試行が入る。round 2 の R1 / round 3 で
  「タスク4の『最大3回』が満たせるか」を論点にしていた部分が、
  これでさらに余裕を持って満たされる。無限ループ等の副作用もない
  (`deadline` が依然として上限)。
- **`100286a9`(doc 訂正)**: 指摘内容・訂正内容とも正しい。
  R1 修正後の実効エピソード長は最悪 ~135秒になりうるので、
  旧 doc の「comfortably below `STUN_RESUME_GIVE_UP_WINDOW` (120s)」は
  確かに成立しない。「これは cross-family target *単体* への予算であって
  エピソード全体の保証ではない」「`always-connects.md` は *eventual* な
  自動復旧の要求であってレイテンシ SLA ではない」という整理に同意する。
  変更前も同シナリオで ~120秒使っていた(しかも連続性を保つ試みは
  一切しなかった)ので regression でないという評価も正しい。

## 残る指摘(trivia、対応不要)

### T1(trivia). `attempt = 0` リセットにより、その周回の失敗ログが `resume attempt 0 failed` になる

`attempt` はループ先頭で `saturating_add(1)` されてから使われる 1-origin の
値だが、Site A(`:1358`)/ Site B(`:1498`)のリセットはどちらも、
そのイテレーションの失敗ログ(`:1508` / `:1510`
`"resume attempt {attempt} failed"`)より**前**に実行される。
結果、切替が起きた周回だけ「`resume attempt 0 failed`」と出る
(Site B の場合、実際には5回目の試行の失敗)。
ログ上の見た目だけの問題で挙動には影響しない。気になるなら
リセットを `state.last_resume_error = Some(msg)` の後ろに置くか、
ログ側で `attempt.max(1)` を使えばよい。修正は任意。

N3(切替ログが原因の失敗ログより先に出る)と R3(deadline give-up 側
コメントの厳密性)は round 4 のまま有効。いずれも据え置きで問題ない。

---

## 最終判定(Round 5)

**N1 の直し方に見落としはない。correctness の指摘は残っていない。
CI が緑ならマージしてよい。**

---
---

# Round 5: `/code-review` 由来の「短い `--resume-window` で切替が餓死する」指摘の裁定

対象: HEAD(`172d2c25`)時点の `rust-core/isekai-pipe/src/resume_loop.rs`
依頼された論点: (1) 算術とコード経路の独立検証、(2) always-connects 上の重大度、
(3) 候補A/Bの選択と実装スケッチ、(4) C1/M2/M3/R1/R2/N1 との整合、(5) テスト。

## 結論(先に)

1. **指摘は成立する。ただし依頼文の数値は2箇所ずれており、実害は依頼文より広い。**
   - 切替が発火するための条件は「窓が90秒以上」ではなく**「窓が約67.5秒以上」**
     (5回目の試行が*開始*できればよく、切替判定は失敗直後の `Err` アーム内なので
     deadline を跨いでも実行される)。
   - `reconnect_and_resume` は **15秒で有界ではなく、最悪30秒**
     (`TRANSPORT_STEP_TIMEOUT` は「各ネットワーク往復」を縛るもので、connect と
     request_resume に**別々に**掛かる)。この場合5回目の失敗は t≈165秒で、
     **既定設定(10日 grace → 事前窓は120秒クランプ)ですら回数トリガーに到達しない。**
     つまりこれは「短い `--resume-window` を設定した運用者だけの問題」ではない。
2. **always-connects.md 違反ではない。** 依頼文の見立て通り、give-up は運用者が設定した
   deadline どおりに起き、`isekai-ssh` の `lightweight_retries`/`redeploy_gate` は
   ADR 導入前とまったく同じタイミングで起動する。C1 とは**逆向き**の失敗
   (C1は復旧経路を最大10日殺した / 本件は復旧経路を一切触らない)。
   重大度は **MAJOR(サイレントな機能無効化 + §6 計装の誤記録)**、
   blocking の理由は always-connects ではなく **ADR §6 のデータが汚れること**。
3. **候補Aは却下、候補Bを採る。** ただしAの却下理由は「スコープクリープ」ではなく、
   もっと強い技術的理由がある(後述 D2): **A は「サーバーが既に破棄した
   parked session に対して retry し続ける」ことを意味し、このファイルが
   現在どこでも守っている不変条件を初めて破る。**
4. 候補Bには**必須の補強が1つ**ある(D3): 早期切替の OR 枝だけでなく、
   **「1回分のプローブすら入らないなら切り替えない」下限ガード**を
   *両方の*切替サイトに入れること。これは新機能ではなく、round 4 の N2 が
   「記録のみ」として残した穴(切替直後に0回試行で give-up)が、実は
   **`continuity-lost / relay-unreachable` の偽記録**を生む——N1 とまったく同じ
   分母水増しの別経路——ことが分かったための修正。

---

## 1. 独立検証

### 1.1 コード経路(読んだうえで確認)

| 要素 | 現在地 | 確認内容 |
|---|---|---|
| ループ先頭の deadline 判定 | `resume_loop.rs:1276-1310` | `now >= deadline` で give-up。**試行の開始前にだけ**評価される |
| 事前窓の合成 | `:1771` → `:790`/`:778` | `min(resume_window_for(grace), Some(STUN_RESUME_GIVE_UP_WINDOW))` |
| `effective_resume_grace_secs` | `engine/mod.rs:1535-1542` | `requested==0 ? max : min(requested, max)`。`max` は `serve --resume-window` |
| Site A(netmon) | `:1336-1360` | `switched_this_call.is_none() && NetworkChanged && stun_failures>=1` |
| Site B(回数) | `:1484-1500` | streak give-up の**後**(N1 修正済み)、`stun_failures >= switch_attempts_before_cross_family` |
| 切替後の予算 | `:1236-1241` | `max_resume_window = (elapsed+45s).max(30s)`、`resume_window = min(grace, それ)` |
| 1試行の上限 | `isekai-transport/src/resume.rs:734-744` と `:813-822` | **15秒 × 2段**(connect / request_resume それぞれ独立の `timeout`) |
| backoff | `backoff.rs:47-56` | `500ms × 2^attempt`(10s上限)、ジッター ±25% |

**重要な訂正**: `TRANSPORT_STEP_TIMEOUT` の doc 自身が
「Bounds **each network round trip** in `reconnect_and_resume`」と書いており、
`reconnect_and_resume` は `timeout(15s, endpoint.connect(..))` と、その後
`resume_on_connection` 内の `timeout(15s, request_resume(..))` を**直列に**踏む。
依頼文(および `CROSS_FAMILY_SWITCH_DEADLINE` の doc `:139-150`)が前提にしている
「1試行=最大15秒」は**片方の段だけ**を数えている。

### 1.2 算術

ジッター無しの公称値で、`W = min(#@isekai resume-grace, serve --resume-window, 120s)`
を事前窓とする。n回目の試行の失敗時刻 `T(n) = (累積backoff) + (1試行の所要時間)×n`、
累積backoff = 0.5 / 1.5 / 3.5 / 7.5 / 15.5 秒。

| 1試行の所要 | T(1..5) | 5回目が**開始**できる条件 | 切替の成立時刻 |
|---|---|---|---|
| 15秒(connect が黙って落ちる) | 15.5 / 31.5 / 48.5 / 67.5 / **90.5** | `W > 67.5s` | t≈90.5s |
| 30秒(connect は通るが RESUME が無応答) | 30.5 / 61.5 / 93.5 / 127.5 / **165.5** | `W > 127.5s` → **120秒クランプにより恒久的に不成立** | 到達しない |

したがって 15秒ケースでの帯域は以下の4つに分かれる(`grace` = 実効 grace 秒):

| 帯域 | 起きること |
|---|---|
| `grace ≤ ~67.5s` | **切替が一度も発火しない**。ADR の機能が丸ごと無効(サイレント) |
| `~67.5s < grace ≤ ~90.5s` | 切替は発火するが、直後の予算が `min(grace, 135.5s) = grace ≤ now` となり**ループ先頭で即 give-up。cross-family へのパケットは0回**。にもかかわらず `record_continuity_lost_if_applicable(.., switched_this_call.is_some()=true, "relay-unreachable")`(`:1305`)が**発火する** |
| `~90.5s < grace < ~135.5s` | プローブ窓が45秒未満に切り詰められる(例: `grace=120` → 29.5秒) |
| `grace ≥ ~135.5s`(既定の10日を含む) | 設計どおり45秒のフル予算 |

30秒ケースでは、**上の全帯域が「切替が一度も発火しない」に潰れる**
(`W` は 120秒クランプを超えられないため)。

### 1.3 到達可能性 — 「運用者の特殊設定」ではない

- `#@isekai resume-grace <duration>` は `isekai-ssh` の**ユーザー向け公開ディレクティブ**
  (`isekai-ssh/src/wrapper/config.rs:175-178`)で、解決値は
  (a) クライアントの要求 grace と (b) リモート `isekai-pipe serve --resume-window`
  の**両方**に同じ値が流れる(`wrapper.rs:1541-1554` → `install_script.rs:242-273`)。
  つまり `effective_resume_grace_secs` は**ほぼそのままこのディレクティブの値**になる。
  リポジトリ自身のテストが使っている値は `120` / `180` / `999`
  (`wrapper.rs:2753` ほか、`:3460` の doc 例は `180s`)——**危険帯域のすぐ内側**。
- 回数トリガーが唯一のトリガーになるのは、まさに本ADRの中核シナリオの一つである
  **「ローカルのインターフェースは何も変わらないまま、キャリアNATのマッピングだけが
  張り替わる」**ケース(netmon が何も報告しない)。round 1 の M3 が指摘した
  「netmon は過敏」の裏返しで、**この経路では過敏どころか無言**であり、
  回数トリガーは「めったに使わない保険」ではない。
- ADR §3.2 タスク1 自身が、条件を論理積にすると
  「netmon が無言の環境で**このタスクが禁じている『120秒待ち』に静かに退行する**」
  と警告している。今回の欠陥は**同じ退行の一段深い版**である:
  論理和にはなっているが、**回数側の disjunct が壁時計 deadline と競争しており、
  負けると黙って消える**。ADR の文言違反ではないが、明確に文言の意図違反。

---

## 2. always-connects.md 上の重大度 — **違反ではない(依頼文の見立てを支持)**

`resume_with_backoff_until_deadline` は `Err` を返し、
`run_resume_loop` → `MidSessionDisconnectSignal` → `write_connect_outcome_for_wrapper`
→ wrapper の `lightweight_retries`/`redeploy_gate` という**ADR導入前と同一の
エスカレーション**に、**同一のタイミングで**到達する。接続そのものは自動復旧する。

C1 との差は決定的で、C1 は `max_resume_window = None` により
**エスカレーション自体が最大10日起動しなくなる**ものだった。本件は
エスカレーション経路にまったく触れていない。
`always-connects.md` は「*最終的に*自動復旧すること」の規約であり
(`CROSS_FAMILY_SWITCH_DEADLINE` の doc `:147-150` が自分で書いているとおり)、
連続性(セッション保持)の維持はその規約の対象外。

**したがって blocking 判定の根拠は always-connects ではない。**
私がそれでもマージ前修正を推す理由は1点だけ:

> ADR §6 の分母は「cross-family resume が**実際に試みられた**回数」と定義されている。
> 1.2 の第2帯域は、**1パケットも送っていない episode を分母にも分子にも1ずつ足す**。
> これは round 4 の N1 とまったく同じ欠陥で、round 4 はそれを(minor ながら)
> 「運用データを取り始める前に入れておく価値がある」として修正を推奨し、実際に修正された。
> 同じ基準を当てるなら、**同じ欠陥の別経路である本件も運用データ収集の開始前に塞ぐべき**。

加えて 1.2 の30秒ケースは「既定設定でも機能が一度も動かない」を意味するので、
**§6 のデータは「relay が到達不能だった」ではなく「そもそも試していない」を
測ってしまう**。ADR §3.3 の未検証仮定を検証する、という本ADRの主目的が達成できない。

---

## 3. 候補の裁定

### D2. 候補A(事前窓に下限を敷く)は却下 — スコープクリープ以前に、不変条件違反

依頼文は A の難点を「運用者が設定した窓を黙って延ばす」と整理しているが、
それより強い理由がある。`resume_window_for` の doc(`:756-758`)自身が書いている:

> granted — that, not our own request, is the real deadline: the server
> **will have already discarded the parked session past this point**
> regardless of how long we keep retrying.

そしてこれは doc 上の主張にとどまらない:
`engine/mod.rs:794` が `max_parked = Duration::from_secs(args.resume_window)`、
`:1535-1542` が `effective_resume_grace = min(requested, args.resume_window)` なので、
**`effective_resume_grace_secs` は常にサーバー側の parked 保持時間以下**である。
その先へ retry を延ばしても、サーバーには resume する対象がもう無い。

現状このファイルのすべての窓計算は `effective_resume_window`(= `min`)を通っており、
**「クライアントはサーバーの付与を超えて retry しない」という不変条件が
例外なく成立している**(`cross_family_switch_budget` ですら `min` を通す)。
候補Aはこれを破る**最初の1箇所**になる。しかも買える retry は原理的に無駄。

さらに副次的な理由として、Aの「~95秒」という下限値は 1.1 で訂正した
**誤ったコストモデル(1試行=15秒)から導かれている**。正しい上限(30秒)で
引き直すなら下限は ~170秒になり、120秒クランプとも grace とも整合しない。
**固定の時間下限は、可変な試行コストに対して原理的に正しく引けない。**

### D3. 候補Bを採る(依頼文の leaning を支持)+ 必須の補強

候補Bは「残り時間が1回分のプローブ予算を下回る前に切り替える」ことで、
試行コストが15秒でも30秒でも、grace が60秒でも10日でも**自動的に正しい側に倒れる**。
`--resume-window`(= `#@isekai resume-grace`)の意味も、サーバー付与との
`min` 不変条件も、どちらも変えない。

**依頼文の「窓が短すぎて1回分すら入らないなら切替自体を諦め、今日の挙動のまま」
という直感に同意する。** ただし理由は依頼文の「どうせ助からないから」より強い:
1.2 の第2帯域で見たとおり、**そこで切り替えると `continuity-lost /
relay-unreachable` を偽記録する**。つまり下限ガードは「無害な最適化」ではなく
**候補Bの正しさの一部**であり、しかも **Site A(netmon 経路)には
候補Bと無関係に今日から存在する穴**でもある
(例: `grace=10s` の episode → 15.5秒で Site A が発火 → 予算 `min(10s,60.5s)=10s`
→ 即 give-up + 偽記録)。round 4 の N2 が「実害はない」と記録した経路の実害は、
**計装の嘘**という形で存在していた。

---

## 4. 実装スケッチ

すべて `resume_loop.rs` 内で閉じる。`run_resume_loop` 側の状態追加は**不要**
(判定に必要な情報は `deadline` / `stun_failures` / `switch_attempts_before_cross_family`
だけで、いずれも1回の呼び出し内で完結する)。

### (1) 定数を1つ追加(`CROSS_FAMILY_SWITCH_DEADLINE`(`:151`)の直後)

```rust
/// The smallest remaining slice of the current deadline in which switching
/// to the cross-family relay target can still buy anything: one
/// `RESUME_BACKOFF` first delay (500ms, +25% jitter) plus one
/// `TRANSPORT_STEP_TIMEOUT`-bounded QUIC connect step (15s — mirrored here
/// rather than imported, for the same reason `REPLAY_WRITE_TIMEOUT` above
/// mirrors it: that constant is private to `isekai-transport`).
///
/// Deliberately *not* the ~30s worst case of a whole `reconnect_and_resume`
/// (its connect and its `request_resume` are bounded by that 15s
/// *separately* — `TRANSPORT_STEP_TIMEOUT` bounds "each network round
/// trip", not the call): demanding 31s of headroom would suppress the
/// switch across exactly the moderate `#@isekai resume-grace` band where it
/// is most valuable, and the case this ADR exists for (a `cached_relay_addr`
/// the client's new network cannot reach at all) is decided in the *connect*
/// step, so a probe that only gets this far still yields a real
/// relay-reachability verdict.
const CROSS_FAMILY_MIN_PROBE_BUDGET: Duration = Duration::from_secs(16);
```

### (2) 純粋関数を2つ追加(`cross_family_switch_budget`(`:1236`)の直前)

`update_unknown_session_streak`(`:1136`)と同じ「ダイヤル無しで単体テストできる
純粋な判定コア」の前例に揃える。

```rust
/// Whether a switch made *now* would still get at least one real probe in
/// before the current `deadline` — see `CROSS_FAMILY_MIN_PROBE_BUDGET`.
///
/// Switching without this is worse than not switching: the very next thing
/// `resume_with_backoff_until_deadline` does is its loop-top deadline
/// give-up, which then records `continuity-lost / relay-unreachable` for an
/// episode that never sent a single packet to the relay — the same ADR §6
/// denominator inflation N1 (opus review round 4) closed at the *other*
/// give-up site, reached here through the residual path that round's N2
/// recorded as harmless.
fn cross_family_probe_fits(remaining_before_deadline: Duration) -> bool {
    remaining_before_deadline >= CROSS_FAMILY_MIN_PROBE_BUDGET
}

/// The failure-count switch trigger (ADR_STUN_REESTABLISH_CONTINUITY.md
/// §3.2 task 1's second disjunct), made deadline-aware.
///
/// The count alone silently *never fires* whenever the episode's deadline is
/// shorter than the attempts the count needs (opus review round 5): each
/// attempt can burn up to two `TRANSPORT_STEP_TIMEOUT`s, so reaching
/// `STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS` takes ~90s in the 15s-per-attempt
/// case and ~165s in the 30s one — against a window that is
/// `min(#@isekai resume-grace, serve --resume-window, STUN_RESUME_GIVE_UP_
/// WINDOW)`. That is the same "silently degrades back into waiting out the
/// whole window" failure ADR §3.2 task 1 forbids, arrived at from the other
/// side. So also switch once what remains of the window is no longer bigger
/// than the cross-family probe itself would want: at that point another bare
/// redial can only consume the window, while the fallback can still change
/// the outcome.
///
/// Deliberately *lowers* the deadline's meaning, never raises it — the
/// client must not retry past the server's own grant, which is also what
/// discards the parked session (`resume_window_for`'s docs,
/// `engine/mod.rs`'s `max_parked`/`effective_resume_grace`).
fn should_switch_to_cross_family(stun_failures: u32, switch_attempts_before_cross_family: u32, remaining_before_deadline: Duration) -> bool {
    cross_family_probe_fits(remaining_before_deadline)
        && (stun_failures >= switch_attempts_before_cross_family || remaining_before_deadline <= CROSS_FAMILY_SWITCH_DEADLINE)
}
```

### (3) Site B(`:1484`)の条件を差し替える

```rust
if switched_this_call.is_none()
    && should_switch_to_cross_family(
        stun_failures,
        switch_attempts_before_cross_family,
        deadline.saturating_duration_since(Instant::now()),
    )
{
    if let Some(fallback_target) = cross_family_target {
        // ...ブロック本体(ログ / current_target / budget / switched_this_call / attempt = 0)は無変更
    }
}
```

### (4) Site A(`:1336`)には**下限ガードだけ**を足す

```rust
if switched_this_call.is_none()
    && backoff_wait_outcome == BackoffWaitOutcome::NetworkChanged
    && stun_failures >= 1
    && cross_family_probe_fits(deadline.saturating_duration_since(Instant::now()))
{
```

**ここで `should_switch_to_cross_family(stun_failures, 1, remaining)` に
置き換えてはいけない。** OR 枝が `stun_failures == 0` でも真になりうるため、
「netmon シグナルが最初の試行より前に来たら0回試行で切り替わる」という
**M3 / R2 が2度塞いだ穴がそのまま再び開く**。Site A の
`stun_failures >= 1` は独立した連言のまま残すこと。

### (5) doc の訂正(推奨)

- `STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS`(`:78-83`)の
  「cumulative wait is roughly 15s — comfortably far below the 120s」は、
  **backoff の待ち時間だけを数えていて試行そのものの所要を数えていない**。
  この誤ったメンタルモデルが本件の直接の原因なので、
  「これは待ち時間の合計であって切替到達時刻ではない」ことを明記する。
- `CROSS_FAMILY_SWITCH_DEADLINE`(`:139-150`)の「~90s in」は15秒/試行前提。
  30秒/試行の上限があることを追記する。

---

## 5. 既存指摘との整合(C1 / M2 / M3 / R1 / R2 / N1)

| 指摘 | 本修正の影響 |
|---|---|
| **C1**(成功前に多日 deadline を入れない) | `max_resume_window = None` への昇格は `run_resume_loop:1777-1783`(切替成功後)のまま。本修正は**どの deadline も延ばさない**。むしろ延ばす方向の候補Aを明示的に却下している |
| **M2**(`build_cross_family_target` を `Option` に) | `connect.rs` 側。無関係、無変更 |
| **M3 / R2**(0回試行での切替禁止) | Site A の `stun_failures >= 1` は連言のまま維持(4項の警告)。Site B の OR 枝は `Err` アーム内=必ず1回以上失敗した後にしか評価されない。**「切替前に最低1回は現 target を試す」不変条件は維持** |
| **R1**(予算は切替時刻起点、`disconnected_at` 起点でない) | `cross_family_switch_budget` は一切触らない。`.max(UNKNOWN_SESSION_MIN_ELAPSED_FLOOR)`(30秒)もそのまま。なお grace が30秒未満のとき、この `.max` は後段の `min(grace, ..)` に打ち消される——これは本修正の影響ではなく、grace クランプ由来の既存の性質(サーバーがその時点で parked session を捨てている以上、正しい挙動) |
| **N1**(試していないのに `session-gone` を記録しない) | Site B の**位置**(streak give-up の後)は変更しない。条件式だけを差し替える。加えて下限ガードにより、N1 と同型の**もう1つの**偽記録経路(`relay-unreachable` 側)が塞がる |
| 一方向切替 | `switched_this_call.is_none()` ガード・単調性ともに無変更。切替後は両サイトとも評価されないので振動しない |
| **N2**(round 4、記録のみ) | 「切替えたのに0回試行で give-up」は下限ガードにより、`grace` 起因の経路が消える。残るのは N1 が既に塞いだ streak 経路のみ。N2 の記述は更新が必要 |

---

## 6. テスト

**この修正は F-3 / m9 の「据え置き」対象にすべきではない。安価に、今すぐ書ける。**

理由: 判定ロジックを純粋関数2つ(`cross_family_probe_fits` /
`should_switch_to_cross_family`)に出す形にしてあるので、テストは
`update_unknown_session_streak` の既存テスト(`:2186-2241`)とまったく同じ形
——ダイヤルもランタイムも不要な `#[test]`——で書ける。最低限これだけ固定すれば
本件の再発は止まる:

1. `stun_failures = 4, attempts = 5, remaining = 大` → `false`(回数未達では切り替えない)
2. `stun_failures = 5, attempts = 5, remaining = 大` → `true`(従来の回数トリガーが無傷)
3. `stun_failures = 1, attempts = 5, remaining = 40s`(< 45s)→ `true`
   (**本修正の本体**: 回数未達でも deadline 逼迫で切り替わる)
4. `stun_failures = 4, attempts = 5, remaining = 10s` → `false`
   (**下限ガード**: 1回分も入らないなら切り替えない=偽 `relay-unreachable` を出さない)
5. `remaining = CROSS_FAMILY_MIN_PROBE_BUDGET` ちょうど → `true`(境界)

一方、**「短い grace の episode 全体で cross-family へ1回もダイヤルしないこと」を
end-to-end で固定するテストは、依然として m9 の側に置いたままでよい**:
`resume_with_backoff_until_deadline` は `Err` 経路では切替の有無を戻り値に
出さない(`switched` は `Ok` のときだけ返る)ので、観測にはログ/telemetry の
フックが要る。これは F-3 が指摘した「試行回数と give-up 時刻を数える」テストと
同じ土俵の作業であり、`ISEKAI_PIPE_DESIGN.md` Epic S 末尾の follow-up(`2bb9e098`)
に含めれば足りる。**上記5本の純粋関数テストは今回の修正と同じコミットに入れること。**

---

## 7. Round 5 の推奨アクション

1. **候補B + 下限ガードを実装する**(4項のスケッチそのまま、`resume_loop.rs` 内で閉じる)。
   Site A への適用は**下限ガードのみ**——OR 枝を持ち込むと M3/R2 が再発する。
2. **純粋関数テスト5本を同じコミットに入れる**(6項)。
3. **doc を2箇所訂正する**(4-(5))。特に
   `STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS` の「roughly 15s」は本件の原因そのもの。
4. round 4 の N2 の記述を「実害なし」から「`relay-unreachable` の偽記録を生んでいた、
   本修正で解消」に更新する(このファイル内の記録として)。
5. 候補A(窓の下限化)は**採らない**。採る場合は
   「サーバーが既に破棄した parked session に retry する」ことの是非を
   ADR 側で先に決める必要があり、それは本ADRのスコープ外。

**always-connects 上の blocking ではない。したがって「CI が緑ならマージしてよい」という
round 3/4 の判定自体は覆さない**——ただし §6 の運用データ収集を開始する前に
本修正を入れておくこと。入れずに測ると、測定対象(§3.3 の仮定)ではなく
測定器の欠陥(切替が発火していない/試していないのに失敗と記録される)を測ることになる。

---
---

# Round 6: 修正コミット `97264e5d` の再確認 — **実装は一致、残りは doc 3件とテスト1件**

対象: `git diff f489b97c..97264e5d`(`resume_loop.rs` のみ、+166/-16)

## 結論

**Round 5 の実装スケッチと完全に一致している。correctness の退行はない。**
定数・純粋関数2つ・Site B の条件差し替え・Site A の下限ガードのみ追加(OR枝を持ち込んでいない)、
いずれも指示どおり。**テスト5本の期待値はすべて正しい。**
新規指摘は **doc 3件(うち2件は `ISEKAI_PIPE_DESIGN.md` 側の見落とし)と、テスト1本の追加推奨**
だけで、いずれもマージ阻害要因ではない。

## 検証: 帯域ごとの挙動(修正前後)

`W` を事前窓として、Round 5 の 1.2 の帯域を修正後の実装で引き直した。

| ケース | 修正前 | 修正後 |
|---|---|---|
| `grace=30s`(1回分も入らない) | 切替なし | **切替なし(不変)**。t≈15.5s の失敗で `remaining=14.5s < 16s` → 下限ガードで抑止。偽記録も出ない |
| `grace=80s`(旧・第2帯域) | t≈90.5s で切替 → **0回試行で即 give-up + 偽 `relay-unreachable`** | t≈48.5s(3回目の失敗、`remaining=31.5s ≤ 45s`)で切替 → **約31.5秒の実プローブ** |
| `grace≥135s` / 既定の10日 | t≈90.5s で回数トリガー | **完全に不変**。t≈67.5s 時点の `remaining=52.5s > 45s` なので OR 枝は発火せず、従来どおり5回目の失敗で切替 |
| 1試行30秒 × 既定 grace | **切替が一度も発火しない**(5回目の失敗 t≈165.5s > 120s クランプ) | t≈93.5s(3回目の失敗、`remaining=26.5s ≤ 45s`)で切替 → 45秒のフル予算 |

**既定設定の挙動がビット単位で変わらないこと**を確認した(ジッター ±25% と
試行コストの上下を振っても、`T(4)` 時点の残りは45秒を上回る)。これは
Round 5 で候補Bを推した際の前提条件そのものなので、重要な確認点。

## テスト5本の期待値検証

| テスト | 引数 | 期待 | 検算 | 判定 |
|---|---|---|---|---|
| 1 | `(4, 5, 3600s)` | `false` | `fits(true) && (false ‖ false)` | ✓ |
| 2 | `(5, 5, 3600s)` | `true` | `fits(true) && (true ‖ …)` | ✓ |
| 3 | `(1, 5, 40s)` | `true` | `fits(true) && (false ‖ 40≤45)` | ✓ |
| 4 | `(4, 5, 10s)` | `false` | `fits(false)` → 短絡 | ✓(**下限ガードを実際に固定している**: ガードを外すと `10≤45` が真になり `true` に反転してテストが落ちる) |
| 5 | 境界(`16s` / `16s-1ms`) | `true` / `false` | — | ✓ |

## 新規指摘

### P1(minor). `CROSS_FAMILY_SWITCH_DEADLINE` の doc 内に、訂正し損ねた「15秒」が1箇所残っている

**該当**: `resume_loop.rs:127-130`(同じ doc コメントの **R1 の段落**)

```
/// against the *original* STUN target, each up to
/// `isekai_transport::resume::TRANSPORT_STEP_TIMEOUT` (15s), not just the
/// `RESUME_BACKOFF` waits between them.
```

この修正コミットは同じ doc コメントの**下の段落**(`:147-160`)を
「2段 × 15秒」に訂正したが、**25行上の R1 の段落は 15秒のまま**で、
1つの doc コメント内で同じ量に2つの異なる値が書かれている状態になった。
C1/R1 が2度踏んだ「導出値の同期漏れ」と同型の、doc 版の drift。

なお **R1 の結論自体は影響を受けない**(「切替時点で既に episode の数十秒が
経過しうる」という主張は、30秒/試行ならむしろ強まる)。
`(15s)` → `(up to two of them, ~30s)` 相当に直すだけでよい。

### P2(major-ish な見落とし、ただし doc のみ). `ISEKAI_PIPE_DESIGN.md` Epic S 節が未更新

**該当**: `ISEKAI_PIPE_DESIGN.md:2013` と `:2045`

- `:2013`「`STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS`(5回、**累積約15秒**)に達した時点」
- `:2045`「タスク1の**切替窓(約15秒)**に対して十分小さい(2秒)ため無視できる」

**本件の原因になった誤ったコストモデルが、設計ドキュメント側にそのまま残っている。**
`resume_loop.rs` の doc だけを直しても、次にこの機能を触る人が最初に読むのは
`CLAUDE.md` が「実装前に必ず目を通すこと」と指定している `ISEKAI_PIPE_DESIGN.md`
の方であり、そこに「累積約15秒で切り替わる」と書いてあれば同じ取り違えが再発する。
**依頼された「doc 訂正の見落とし」は、実質これ。**

加えて `:2007-2013` の切替トリガーの記述は ADR §3.2 タスク1 の2つの disjunct
しか列挙しておらず、**本コミットが追加した3つ目の条件(deadline 逼迫による
早期切替)と下限ガードがどこにも書かれていない**。Epic S 節は m9 フォローアップ
(`2bb9e098`)の置き場所でもあるので、ここに追記しておくのが自然。

`:2045` の結論(preempt 待ち2秒は無視できる)は**そのまま有効**
(2秒 « `CROSS_FAMILY_MIN_PROBE_BUDGET` の16秒)。直すのは前提の数値だけ。

### P3(記録/独立). 同じ Epic S 節のタスク7の記述が C1 修正前のまま

**該当**: `ISEKAI_PIPE_DESIGN.md:2038-2041`

> 切替時に`ResumeDeadlinePolicy`をfield-patchせず再計算(`max_resume_window`を
> `Some(STUN_RESUME_GIVE_UP_WINDOW)`から**`None`へ**、…)

これは **C1 が「まさにそれが critical バグ」と指摘した挙動**の記述である。
現在の実装は切替時に `Some((elapsed + 45s).max(30s))` を入れ、`None` への昇格は
`run_resume_loop`(切替が成功した後)でのみ行う。本コミットとは無関係の
既存の staleness だが、**放置すると「設計どおりに直す」つもりで C1 を
再導入されうる**ので、P2 を直すついでに同じ節で直しておくのが安全。

### P4(テスト、推奨1本追加). 下限ガードが「回数条件」側に対して固定されていない

テスト4 は `stun_failures = 4`(閾値未満)なので、**OR 枝経由でのみ**
下限ガードを固定している。将来誰かが

```rust
stun_failures >= switch_attempts_before_cross_family
    || (remaining <= CROSS_FAMILY_SWITCH_DEADLINE && cross_family_probe_fits(remaining))
```

のように「下限ガードは OR 枝だけの条件」へ書き換えると、**5本すべて緑のまま**
Round 5 の第2帯域(`grace` 67.5〜90.5秒: 回数条件が t≈90.5s で成立するが
残り時間ゼロ)の偽 `relay-unreachable` が復活する。1行足すだけで塞げる:

```rust
#[test]
fn should_switch_to_cross_family_does_not_fire_when_the_count_is_reached_but_no_probe_would_fit() {
    // The `#@isekai resume-grace` band (~68-90s) where the attempt count
    // *is* reached, but only at a point where the deadline has nothing
    // left — switching there would record `continuity-lost /
    // "relay-unreachable"` without ever contacting the relay.
    assert!(!should_switch_to_cross_family(
        STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS,
        STUN_TO_CROSS_FAMILY_SWITCH_ATTEMPTS,
        Duration::from_secs(10)
    ));
}
```

### P5(trivia). 削られた1文

`CROSS_FAMILY_SWITCH_DEADLINE` の doc から
「episode 全体が ~135秒に伸びるのは accepted trade-off であって regression ではない
(ADR 導入前も同じシナリオで120秒前後で give up していた)」という
正当化の2文が落ちた。同段落冒頭の「120秒に収まる約束ではない」は残っているので
実害はないが、「なぜ120秒超過を許容してよいか」の根拠だけが消えた形。
戻すかどうかは任意。

## Round 6 の推奨アクション

1. **P2 を直す**(`ISEKAI_PIPE_DESIGN.md` Epic S: 「累積約15秒」2箇所の訂正 +
   3つ目の切替条件と下限ガードの追記)。今回の依頼で聞かれた「doc の見落とし」は実質これ。
2. **P1 を直す**(`resume_loop.rs:129` の `(15s)`)。同一 doc コメント内の数値不一致。
3. **P4 のテストを1本足す**(下限ガードを回数条件側に対しても固定)。
4. P3 は本コミットとは独立の既存 staleness だが、P2 と同じ節なので同時に直すのが効率的。
5. P5 は任意。

**correctness 上の指摘はない。CI(`rust-core-test-linux`)が緑なら、
上記 doc/テストの追補とあわせてマージしてよい。**
