# ADR: mid-session reattach(resume)の予算と失敗理由の区別をisekai-pipe相当に近づける(Android)

- **Status**: Draft(2026-09-16起草。round 1レビューで§1の予算値・対象トランスポート
  範囲・参照識別子の事実誤認を修正、優先順位を入れ替えた。round 2レビューで
  round 1の数値自体が再訂正され(§1.1)、新規タスク(§3.2早期打ち切り)・
  出荷ゲート(§3.4)・既存テストへの影響(§8)を追加。ユーザー判断により、
  これ以上の設計判断は`ANDROID_RECONNECT_SPIKE_PLAN.md`の実機スパイク結果を
  得てから行う——本ADRは現時点でのDraftとして凍結し、スパイク結果を受けて改訂する)
- **対象**(見込み、要精査): `rust-core/src/resume_client.rs`
  (`ReattachableStream`・`attempt_reattach`・`ReattachFn`・`REATTACH_MAX_RETRIES`・
  `REATTACH_BASE_DELAY`)。呼び出し元のうち**reattach層を実際に持つ3経路のみ**
  ——`isekai_pipe_quic_transport.rs`・`isekai_stun_p2p_transport.rs`・
  `isekai_link_relay_transport.rs`(§1.2参照。当初案にあった`multipath_transport.rs`は
  reattach層を持たないため対象外)
- **入力**: `ADR_ANDROID_RECONNECT_TIMEOUT.md`と同一のセッション。round 1レビュー
  (`.../scratchpad/opus-review-android-reconnect-round1.md`)・round 2レビュー
  (`.../scratchpad/opus-review-android-reconnect-round2.md`、両方とも
  `/tmp/claude-1001/-home-cuzic-isekai-terminal/2285f8d6-e7bb-4a20-83c5-de317b68d9c7/`配下)
- **拘束される既存ルール**: `.claude/rules/always-connects.md`

---

## 1. 背景

Android(`isekai-terminal-core`)は、確立済みQUICコネクション内でストリームが
一時的に失われた際、`resume_client.rs`の`ReattachableStream`が裏側で
redial+RESUMEフレームによる再接続(reattach)を試みる。russh(SSHクライアント
自体)はこのラップされたストリームの上で動き続けるため、reattachが成功すれば
`ssh`セッションを再起動せずに継続できる。

```rust
const REATTACH_MAX_RETRIES: u32 = 5;
const REATTACH_BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
```

### 1.1 予算の実効値は「約31秒」ではない。オフラインでは実効約90秒に張り付く(round 1→round 2で二段階に訂正)

`attempt_reattach`が呼ぶ`reattach_fn`の実体は
`isekai_pipe_quic_transport.rs:499-526`→`isekai_transport::resume::reconnect_and_resume`で、
dial・RESUMEリクエストのそれぞれが`TRANSPORT_STEP_TIMEOUT = Duration::from_secs(15)`
(`isekai-transport/src/resume.rs:94`、`resume.rs:735-745`/`812-820`)でバウンドされている。
指数バックオフの累計待ち時間(1+2+4+8=15秒)は1試行あたりの所要時間に比べて
**支配項ではない**。

**round 1レビューでは「オフラインなら即エラーで15秒」という最良ケースを根拠に
挙げたが、round 2レビューでこの前提自体が誤りだと判明した**:
`reconnect_and_resume`はまず`quicmux::BindSpec::any_ipv4()`でワイルドカードUDP
bindを行う(`resume.rs:734`)——これはネットワーク不通でも成功する。その後の
`endpoint.connect(...)`は到達不能な宛先へのパケット送信であり、多くの場合
即座にはエラーにならず`TRANSPORT_STEP_TIMEOUT`のタイムアウトに張り付く。
Android側エンドポイントの`max_idle_timeout=15秒`
(`rust-core/src/android_quic_endpoint.rs:42`)とも一致する。

したがって、まさにADRが心配している完全圏外のケースでは
**15(バックオフ累計)+5×15(各試行)≈90秒**、つまりレンジの**上端**に
張り付く公算が高い。提案値120秒との差はわずか30秒しかない。

**この数値自体、実測で確定していない(§3.3参照)。** 「予算が短すぎる、
最短15秒」という当初の第一根拠は撤回する——ADR2の中心的な論拠は
§3.1(分類による早期判断)と§3.2(network復帰による早期打ち切り)に
移る。予算の絶対値見直し(§3.3)は実測結果が出るまで保留とする。

### 1.2 「対象」の訂正: マルチパス経路にはreattach層が存在しない

`multipath_transport.rs:10`のモジュールdoc:「このトランスポートには独自の
resume/reattach層は無い」。`multipath_transport.rs:1062-1063`にも
「resume/reattach層は無いので…`resume_client::ReattachableStream`のような
特別なラッパーは不要」と明記されている。

実際に`ReattachableStream`を使うのは次の3経路のみ:
`isekai_pipe_quic_transport.rs:411,450,529,608` /
`isekai_stun_p2p_transport.rs:153,237,284` /
`isekai_link_relay_transport.rs:117,171,199`。

つまり**ユーザーがWi-Fi⇔セルラーのローミング耐性のために選ぶ、まさに今回の
相談の動機である`multipath_transport`経路には、mid-sessionのreattachが
最初から存在しない**。`RebindManager`のフェイルオーバーが失敗した場合、
即座に`ADR_ANDROID_RECONNECT_TIMEOUT.md`が扱うorchestratorの60秒ループへ
移行する(本ADRの層は関与しない)。この事実は`ADR_ANDROID_RECONNECT_TIMEOUT.md`
の優先度がADR2より高いことの根拠になる(同ADR参照)。

### 1.3 Windows(isekai-pipe)との対比、および対比が成立しない構造的理由

`rust-core/isekai-pipe/src/resume_loop.rs`は同種の「セッションを裏で繋ぎ直す」
ロジックに対し、より長い予算を持つ(STUN P2P経路: `STUN_RESUME_GIVE_UP_WINDOW = 120秒`、
relay経路: resume window既定10日)。

**ただしこの対比をそのまま「Androidも10日を目指すべき」という結論に結び付けては
いけない。** isekai-pipeが10日粘れるのは`ssh(1)`が別プロセスであり、OpenSSHの
既定`ServerAliveInterval=0`(無制限に待つ)だからである。Androidはrussh(SSH
クライアント自体)を同一プロセス内に抱えており、そのkeepalive設定が
reattach予算より先に効くクライアント側の締切になる(§3.3で詳述)。

## 2. 問題

- **予算がオフライン時に実効約90秒(§1.1)に張り付く公算が高く、これが
  妥当な長さかは未実測**。Windows側は120秒〜10日だが、§1.1の通り数値の
  対比自体が構造的に成立しない(異なるkeepalive前提の上に立っている)。
- `RebindManager`のフェイルオーバーが間に合わなかった場合の砦は
  **multipath経路には存在せず**(§1.2)、単一経路(isekai_pipe_quic/stun_p2p/
  link_relay)の場合のみ本ADRの層が最後の砦になる。
- 失敗理由(サーバーが確定的にセッションを忘れたケース vs 一時的な
  ネットワーク不通・race)を区別する仕組みが無い。
- reattachのバックオフ待機には、network復帰による早期打ち切りが無い
  (§3.2、round 2レビューで新規発見)。

## 3. 実装方針(round 1でタスク順序を入れ替え、round 2で早期打ち切りタスクを追加)

### 3.1 タスク1: 失敗理由の分類を先に入れる

**round 1レビューの指摘により、当初「1.予算見直し→2.分類」としていた順序を
「1.分類→2.予算見直し」に入れ替える。** 理由(D-3): Windows(isekai-pipe)と
Androidでは「reattachを諦めること」の意味が正反対だからである。

- isekai-pipe側: give-up = `ssh`プロセスの死 = ユーザーのシェル喪失。だから粘る
  価値が大きい。
- Android側: give-up = `ReattachableStream`が`terminal_error`を立てる→russh
  セッション終了→`handle_unexpected_disconnect`→orchestratorが**新しい
  セッションを張る**。シェルは失うが、接続自体はすぐ回復しうる。

したがってAndroidにおける分類の最大の価値は「長時間のハングを防ぐ」という
防御的な理由ではなく、**確定的にセッションが消滅したと分かった時点で
15〜90秒を無駄に使わず即座にorchestratorへ制御を渡し、新規接続させる**
という攻めの理由である。これは`always-connects.md`の原則に直接効く。

**実装範囲は小さい**(round 1レビューで確認済み): サーバーからの拒否理由は
既に型付きで手元まで来ている。
`isekai-transport/src/error.rs:44-49`の`TransportError::ResumeRejected(ResumeRejectReason)`が
`quicmux::ResumeRequestError::Rejected(reason)`(`isekai-transport/src/resume.rs:828`)から
既にマップされている。型情報が消えるのは**ただ1箇所**:
`isekai_pipe_quic_transport.rs:514`の`.await.map_err(|e| e.to_string())?`が、
`ReattachFn`(`resume_client.rs:146-150`)の`Result<_, String>`という型に
潰しているだけ。修正範囲は`resume_client.rs`の`ReattachFn`/`attempt_reattach`と、
呼び出し元3つ(`isekai_pipe_quic_transport.rs:499`、`isekai_stun_p2p_transport.rs`、
`isekai_link_relay_transport.rs`)+ 内部テストmock。**UniFFI境界は越えないため
バインディング再生成は不要。**

実際のワイヤ表現(参照識別子はround 1レビューで訂正——`HELPER_PROTOCOL.md`は
現在`archive/HELPER_PROTOCOL.md`、`REJECT_UNKNOWN_SESSION`/`REJECT_OFFSET_GONE`
という識別子はコードベースに存在しない):
`quicmux::ResumeRejectReason::{Auth, UnknownToken, OffsetGone}`
(`quicmux/src/resume.rs:78-105`、wireは0/1/2)が
`isekai_protocol::resume::ResumeRejectReason::{Auth, UnknownSession, OffsetGone}`
へマップされる(`isekai-transport/src/resume.rs:693-698`)。

**移植するガードは2つ組であること**(isekai-pipe側の`resume_loop.rs`を参照):
`UNKNOWN_SESSION_CONFIRM_THRESHOLD = 3`(`:959`)と
`UNKNOWN_SESSION_MIN_ELAPSED_FLOOR = Duration::from_secs(30)`(`:978`)の
**両方**を満たしたときだけgive upする(`:1021`)。floorが必要な理由
(`:962-977`のdoc)はAndroidにもそのまま当てはまる: break-before-makeの
ローミング(Wi-Fi drop・AP切替・機内モード)では、クライアントの切断が
旧経路経由でサーバーに届かないため、サーバーは自身のQUIC idle timeout
(`isekai-pipe serve --idle-timeout`既定15秒)でしか旧接続の死を知れず、
それまで「まだparkされていない」状態として`UnknownToken`を返し続ける。
**Androidの現行バックオフ(1,2,4,8秒)では3回目の試行がt≈3秒に来るため、
thresholdだけを移植するとfloorを確実に踏み抜いて誤give-upする。**
thresholdとfloorは必ずセットで導入すること。

### 3.2 タスク2(round 2レビューで新規追加、タスク3より優先度が高い可能性): reattachバックオフのnetwork復帰による早期打ち切り

isekai-pipe(`resume_loop.rs`)の`wait_backoff_or_network_change`(`:816-836`)は
`isekai_netmon::NetworkChangeMonitor`と`tokio::select!`し、バックオフ待機中に
ネットワーク変化があれば即座に待機を打ち切る。orchestrator(`orchestrator.rs`)も
`sleep_tick_or_network_restored`(`:925-930`)+`reconnect_wake`
(`:440`,`:1535-1537`)で同種の仕組みを持つ。**reattach層(`resume_client.rs`)
だけがこれを持たず**、`tokio::time::sleep(REATTACH_BASE_DELAY * 2u32.pow(attempt-1))`
(`:331`)という盲目のsleepのみである。

この欠落のコストはタスク3(予算延長)を実施すると比例して増える: 120秒
deadline+バックオフ上限8〜10秒クランプにすると、圏外から復帰した瞬間に
気付けず、毎回平均4〜5秒(最悪10秒)をバックオフで無駄に捨てることになる。

**配線先は既に存在する**: `notify_network_path_changed`の
`ConnPhase::Connected && is_quic`分岐(`orchestrator.rs:1545-1547`)は、
まさにreattachが走っている最中に呼ばれるが、現状は
`log::info!("... letting transport handle it")`とログを出すだけで終わっている。
ここから`ActiveSession`経由でreattach層へ生イベントを渡せばよく、
`rust-ssot.md`の「生イベントを転送し、判断はRustの一箇所に集約する」という
原則にもそのまま乗る。

**予算を伸ばさなくても、この1点だけで「Wi-Fiに戻った瞬間に繋がる」体感が
改善する。費用対効果はタスク3(予算延長)より高い可能性が高く、タスク3より
先に実装する。**

### 3.3 タスク3: 予算の見直し(実測結果が出るまで保留)

**§1.1の通り、予算延長の第一根拠(「最短15秒」)は撤回された。本タスクは
実測結果が出るまで着手を保留する。** 保留理由: reattach層の実効予算が
15〜90秒のどこに(あるいは§1.1の分析通り上端の90秒付近に)実際に張り付くかは
実機ログでしか確定できない。**`ANDROID_RECONNECT_SPIKE_PLAN.md`スパイク5
(ADR1・ADR2共通)は既にこれを対象に含めている**
(`resume_client.rs`の`attempt_reattach`/`reconnect_and_resume`の実際の
in-flight時間を計測対象に含む、round 3レビューで指示形から参照形に修正)——
本タスクはスパイク5の結果を得てから保留を解除する。

**reattach層の予算には物理的な上限がある**: 全トランスポートのrussh client
configで揃って`keepalive_interval: Some(Duration::from_secs(60))`,
`keepalive_max: 3`が設定されている
(`isekai_pipe_quic_transport.rs:688-689`、`isekai_stun_p2p_transport.rs:289-290`、
`isekai_link_relay_transport.rs:204-205`、`lib.rs:1641-1642`、
`quic_transport.rs:263-264`)。**この上限は出典を示さない断定ではなく、
設定値から導ける式として書く**(round 2レビューで指摘): reattach予算は
`keepalive_interval × (keepalive_max - 1) = 60秒 × 2 = 120秒`を超えては
ならない(russhが無応答のkeepaliveを`keepalive_max`回検出した時点でセッションを
自ら終了させるため、それより先に粘っても意味がない)。この式なら将来
`keepalive_interval`/`keepalive_max`を変更した際、予算の見直しも連動して
気付ける。

**具体的な数値の叩き台**(実測待ちの暫定値): 固定回数をやめて壁時計deadline
120秒(上記の式の上限と一致、`STUN_RESUME_GIVE_UP_WINDOW`とも同値)。
1試行は`TRANSPORT_STEP_TIMEOUT`で既にバウンド済みなので実効5〜8回。
バックオフ上限は8〜10秒程度でクランプ。タスク1の分類+floor(30秒)、
タスク2の早期打ち切りにより、確定的消滅・network復帰のいずれの場合も
120秒を待たずにorchestratorへ制御が渡る。

### 3.4 タスク4: `ADR_ANDROID_RECONNECT_TIMEOUT.md`との役割分担・UI可視化

**「二重の安全網」として両者を独立に長くする案は採らない**(round 1レビューで
筋が悪いと指摘): reattachを延ばすほど、確実に成功しうる新規接続の開始
(orchestrator側)が遅れるだけである(reattach中はorchestratorが何もできない)。
正しい形は「reattachは上限に張り付いた固定予算」+「その後orchestratorが
長く粘る」。

**reattach中はUIから見て「無表示」の区間になる**(round 1レビューで判明した
重大な事実): reattachが走っている間、orchestratorの`phase`は`Connected`の
ままであり、`on_disconnected`は来ず、`notify_network_path_changed`も
`ConnPhase::Connected && is_quic`分岐でログを出すだけ。つまり
t=0〜最大90秒(タスク2適用後は最大120秒)は「接続済み」表示のまま端末が
固まって見えるだけで、`Reconnecting`通知は一切出ない。「予算が足し算される」
のではなく「前半は無表示、後半だけ表示」という非対称な体験になっている。
**担当は`ADR_CONNECTION_OBSERVABILITY.md`ではなく本ADRが持つ(round 3レビューで
確定)**: `ADR_CONNECTION_OBSERVABILITY.md`は`tracing`による**事後診断のための
計装**(Phase 1を`rust-core/src`に限定、`attempt_reattach`等をログ対象として
列挙)が目的であり、生成するのはログであって**ライブなUI状態ではない**。
「reattach中であることをユーザーに見せる」仕事はどちらのADRにも
帰属していなかった——本タスク(3.4)がこれを持つと明記する。具体的には
orchestratorが新しい`ConnectionPublicState`値(または既存`Reconnecting`の
転用)でreattach中を通知し、Kotlin/Swift側のUIがそれを表示する形にする。

**出荷ゲート(round 2レビューで格上げ、round 3レビューで引受先を具体化)**:
`ADR_CONNECTION_OBSERVABILITY.md`の完了ではなく、**上記のライブUI状態
(reattach中を表す`ConnectionPublicState`とその表示)が実装されるまで**、
タスク3(予算延長)は出荷しない。observability ADR側の計装完了だけでは
このゲートは満たされない——それは事後ログであってユーザーに見える状態
ではないため、満たしたつもりで無表示の凍結区間がそのまま出荷されるのを
防ぐために明記する。無表示の凍結区間が伸びるだけでUXが悪化するため。

## 4. 非目標

- `isekai-pipe/src/resume_loop.rs`とのコード共有・統合はしない。
  `isekai-terminal-core`(Android/iOS共有)と`isekai-pipe`(isekai-ssh専用CLI)
  はアーキテクチャが異なる独立クレートである。
- サーバー(`isekai-pipe serve`)側のプロトコル・`SessionTable`実装の変更は
  本ADRの範囲外。
- `multipath_transport.rs`へのreattach層新設は本ADRの範囲外(§1.2の通り
  現状無いことが判明した既存事実であり、それを追加するかどうかは
  別途独立した設計判断とする)。

## 5. 実装前に固定すべき制約(round 1でopen questionから格上げ、round 2で精度修正)

- **russh keepaliveによる上限(§3.3)**: `keepalive_interval=60s`,
  `keepalive_max=3`が全トランスポートで一致。reattach予算は
  `keepalive_interval × (keepalive_max - 1) = 120秒`を導出式の上限とする
  (russh 0.48自体の内部実装を出典として確認したものではなく、設定値からの
  導出——将来この2定数を変更する際は予算も連動して見直すこと)。
- **`session_id`未確立の窓では予算がゼロになる**: `attempt_reattach`は
  `resume.session_id`が`None`なら即座に(retryせず)`Err`を返す
  (`resume_client.rs:294-300`)。`session_id`はcontrol streamが確立して
  初めて入るが、その確立自体が`CONTROL_STREAM_TIMEOUT`付きの
  バックグラウンドspawnである(`isekai_pipe_quic_transport.rs:467-495`)。
  接続直後(control stream確立前)にネットワークが落ちた場合、予算の議論
  以前に即terminal errorになる。この窓を対象に含めるか(control stream
  確立を待つのか、待たずにorchestratorへ譲るのか)は実装時に決定する。
- **`run_pump`の3分岐の意味を潰さないこと(round 2レビューで対応関係を訂正)**:
  read/write失敗→`attempt_reattach`(既存)、russh側がduplexを閉じた
  (セッション破棄側)→`terminal_error`を立てずに無言return
  (`FromCallerClosed`、`resume_client.rs:396`)、helper側read方向のclean EOF→
  reattachせず`helper_read_done=true`(`FromHelperClosed`、`:403-409`)。
  **ユーザーの正常な`exit`が実際に辿るのは`FromHelperClosed`である**
  (round 1レビュー時点の記述は`FromCallerClosed`としており誤り)。
  予算をdeadlineベースに変える際、`FromHelperClosed`(正常終了)を「失敗」に
  巻き込むと、ユーザーの正常な`exit`が無限リトライを誘発しうる
  (orchestrator側の`DisconnectKind::GracefulRemoteExit`が守っている性質と
  同種の罠)。

## 6. 既存テストへの影響(round 2レビューで追加)

- **タスク1**(`ReattachFn`のErr型変更): `resume_client.rs`内のテストmock
  3箇所(`:535-539`, `:565-574`, `:597-598`)の型を書き換える必要がある。
- **タスク3**(固定回数→壁時計deadline): `reattach_gives_up_after_max_retries_and_surfaces_error`
  (`:594-620`)を設計ごと壊す。このテストは`#[tokio::test(start_paused = true)]`で
  仮想時間を1秒ずつ20回進め、「バックオフ累計15秒を確実に超える」ことを
  前提にしている(`:605-611`)。120秒deadlineにすると、進める仮想時間も
  ループ回数も前提が変わるため、テスト自体の書き直しが必要。

## 7. Open Questions

- タスク1(分類)導入時、`ReattachFn`の戻り値型をどう設計するか
  (単純なenum2値か、`ResumeRejectReason`をそのまま伝播させるか)。
- `session_id`未確立の窓(§5)を、control stream確立待ちの猶予として
  明示的に予算に含めるかどうか。
- タスク4のUI可視化を本ADRと`ADR_CONNECTION_OBSERVABILITY.md`のどちらが
  実装するか。

## 8. 参照実装

(実装後に追記)
