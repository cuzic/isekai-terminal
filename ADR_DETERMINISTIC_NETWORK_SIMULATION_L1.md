# ADR: L1(仮想時間 + in-memoryネットワーク)の決定論的シミュレーションを再評価する

- **Status**: **Draft rev2(2026-10-06)**。round 1・2の敵対的レビュー(`scratchpad/adr2-review-round{1,2}.md`、Opus)を反映。
  round 2は「N1・N2を文書で直せば収束」と判定しており、rev2でN1〜N6と残りの軽微指摘を取り込んだ(§0 rev2)。
  **rev0の推奨(「L1 SPIKEを今行う」)は撤回した**。rev1の推奨は「**クライアント/サーバーの時間定数の関係テストを今行い、
  Step 2a・5の後に2つのreducerを1つのproptestで合成する。L1は保留を続け、合成テストとL2では見えない不具合クラスが
  実際に出たときだけ再評価する**」(§5)。L1を再開する場合のSPIKEは、isekai-pipeの実際に危険なコードを通す形に設計し直して
  §6に残した(今は行わない)。
- **対象**: `rust-core/isekai-pipe/src/{engine/mod.rs,engine/attach_runtime.rs,engine/resume.rs,resume_loop.rs,connect.rs}`、
  `rust-core/quicmux/src/{noq_backend.rs,test_support.rs}`、`rust-core/isekai-transport/src/{system.rs,resume.rs}`、
  比較対象として`rust-core/src/faulty_udp_socket.rs`・`rust-core/isekai-link-masque/src/relay_client.rs`、
  CI: `.github/workflows/rust-core-test-check.yml`、`rust-core/.config/nextest.toml`、`.github/workflows/rust-core-netlab-check.yml`
- **入力**: `ADR_CONNECTION_RESILIENCE_SIMULATION.md`(rev4、以下「RES ADR」。L0/L1/L2の定義、§5の仮想時間の制約、§2のL1保留理由)、
  `ADR_FUNCTIONAL_CORE_EFFECTS.md`(rev6、以下「FCIS ADR」。Step 0・1・1.5・2.5・6・7aはmain `535eac85`にマージ済み、
  Step 2aはブランチ`feat/step2a-serve-aggregate` `2a45198f`で未マージ)、Step 9の不具合履歴報告(`scratchpad/defect-history-rank.md`、
  リポジトリ外。採用した事実は§7に書き写す)、round 1レビュー。
- **拘束される既存ルール/ADR**: `.claude/rules/rust-ssot.md`、`.claude/rules/always-connects.md`、
  `.claude/rules/main-branch-protection.md`(required 5本、`lockfile-drift`)、ローカルbuild/test禁止(検証はGitHub Actionsのみ)、
  FCIS ADR §8(「L1、turmoil/madsimの採用」は同ADRの対象外。本ADRはその再評価を別ADRとして行う)。
- **表記**: 「確認済み」はmain `535eac85`(または`Cargo.lock`が指す版のcargo cache上のソース、またはブランチ名を明記したコミット)を
  実際に読んで確認した事実。「推測」は導出のみで未実行。「[EXT]」はこのリポジトリ外の情報(crateのdocs等)で、実装時に確認が要るもの。
  「OPINION」は評価・見積もり。**本ADRのどの主張も、テストを実行して確かめたものではない**(ローカルbuild/test禁止)。

---

## 0. 改訂履歴

### rev2(2026-10-06)— round 2レビューの反映

推奨(§5の方向)は変えていない。各指摘はコード(main `535eac85`とブランチ`feat/step2a-serve-aggregate` `2a45198f`)で確かめてから反映した。

| 指摘 | 内容(要約) | 対応 |
|---|---|---|
| **N1** | 2a後の`ServeEffect::ResumeRejected { id }`は拒否理由を持たない。D-2の時間部分(preempt後の`reparked`待ち`timeout(PREEMPT_WAIT_TIMEOUT, ..)`と、その後の`UnknownToken`応答)とHELLO admissionの`BusyOtherSession`はshellに残る。合成proptestの性質(3)(4)を2a+5だけで書くと、テストハーネスがshellを再実装することになる | 確認した(§3.3に追記)。§4.6で**性質ごとの前提**を明記: (1)(2)は2a+5、(4)は**Step 2b**(admissionのreducer化)+`ResumeRejected`への理由追加、(3)の時間部分はpreempt待ちを**token付きタイマーEffect**にする後続作業(現在どのStepにも無い)。§7の857f6ae6行を「reducerの判断(拒否でなくpreempt)は届く。時間の合成は、preempt待ちがEffectになるまで届かない」へ格下げ。R4・§5-3に盲点として追記 |
| **N2** | 再開時SPIKEのG2(tie順を除いて全実行が一致)とG6(tieの両分岐を通す)は両立しない。分岐後の未来が異なるため | G2を「**tie解決ベクトルごとの決定論性**」に再定義(各tieでの分岐選択列を記録し、同じ列の実行群の中でtraceが一致、全群で不変条件が成立)。G6は「2群以上を観測」に変更(§6.3) |
| **N3** | 「今1 PR」の定数関係テストは、serverの定数がprivateで、2aが書き換え中の`engine/mod.rs`・`attach_runtime.rs`に`pub(crate)`を足す必要がある | 確認した。§4.5を**(a) 今: client側とcrate間でpubな値だけ**(例: `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR` ≥ 2×client idle timeout。後者は`isekai_transport::system::isekai_mux_config(true).max_idle_timeout`で変更なしに参照可)と、**(b) 2aのマージ後: serverの定数**(`pub(crate)`化と`engine/mod.rs:264`のリテラル抽出を含む)に分けた。§5・§9・Q-L1-1も更新 |
| **N4** | G1(実socketを1つもbindしない)は満たせない。`reconnect_and_resume`→`factory.create_endpoint`(`isekai-transport/src/resume.rs:740`)→`NoqFactory::create_endpoint`は常に実socketをbindしてからadapterを呼ぶ | 確認した。G1を「**実socketがデータグラムを1つも運ばない**」に緩めた(bindだけされた未使用のloopback socketは、完了待ちのI/Oを持たないので決定論性に影響しない)。seamは増やさない |
| **N5** | L1保留はL2夜間化に頼っているのに、L2夜間化に担当も時期も無い | R4・§5-4を改め、L2夜間化を**本ADRの採択とあわせて登録する追跡事項**とした(担当: RES ADRのL2担当=team-lead経由で割り当て、既定の時期: 定数関係テスト(a)と同時期)。**L2夜間化が無い間、L1が捕まえるはずのクラスの検出層は実機/本番だけ**であることをR4に明記 |
| **N6** | 意図された関係「serverの`TARGET_CONNECT_TIMEOUT`(20秒)> clientの1ステップ上限(15秒)」が候補に無い | 確認した(`attach_runtime.rs:39-49`のdoc: clientが新しいgenerationで再試行すれば`ClosingForSupersede`で自己回復するので「stuck-foreverではない」)。§4.5(b)に「**意図的な非関係**」(どちらの向きに「直して」もいけない)として追加 |
| 軽微 | `PREEMPT_WAIT_TIMEOUT`の行番号(main `:81`、2aブランチ`:84`)、性質(1)の「経路がある」が検査不能、Q-L1-3に非単調`now` | §4.5に両方の行番号、§4.6の性質(1)を検査可能な形に書き直し、Q-L1-3に非単調`now`を追加 |

### rev1(2026-10-06)— round 1レビューの反映

| 指摘 | 内容(要約) | 対応 |
|---|---|---|
| **B1** | rev0のSPIKE(`isekai-transport/tests`、mock server)はisekai-pipeのコードを1行も通らず、本当のリスク(`select!`26箇所・std時計の`parked_since`・実TCP target・stdio)を通らないので、no-goを出せない | 再確認した(§3.1、§3.3)。**rev0のSPIKEは削除**。L1を再開する場合のSPIKEを、isekai-pipe crate内・Step 2aの時計の上・実`AttachRuntime`・in-memory target・`run_resume_loop`1世代を通る形に作り直した(§6)。G2は「同じ仮想時刻に準備完了になるtimer分岐とpacket分岐を持つisekai-pipeの`select!`を1つ以上通るシナリオ」で判定する |
| **B2** | `run_resume_loop`は実プロセスのstdin/stdoutを直に使う(`resume_loop.rs:1734-1735`)。tokioのstdioはblocking poolで動き、auto-advanceを止める。stdinが`/dev/null`ならEOFで`c2h_already_done`が立ち、全resumeシナリオが誤った理由でgive upする | 再確認した(§3.3)。P3の評価を「小、テスト用入口のみ」から「**中、本番シグネチャ変更あり**(stdio seam)」に改め、Step 5と同時かその後と規定した(§8.1、§9) |
| **M1** | BUSY_OTHER_SESSIONの再試行は初回確立のラッパー(`run_relay_resumable*`等)にしかなく、そこは自前で`system_quic_factory()`を作るのでadapterを差せない。P3(`run_resume_loop`直呼び)はこれを通らない | 再確認した(§3.3)。factory注入をseam S-bとして数えた(§8.1)。§7のBUSY行は「L1固有」から外した |
| **M2** | §7の「L1にしか見えない3〜5件」は成り立たない。BUSY×3はバージョン差(古いserve)への防御で、同一コミット同士のL1では模型化できず、算術は定数関係テストで足りる。3205178fは`start_paused`のL0で再現できる。残るのは857f6ae6 D-2の約1件で、修正済みかつStep 2aでモデル化済み | 再確認し、受け入れた(§7)。**推奨そのものを変えた**(§5) |
| **M3** | rev0が「実loopback TCPでも問題ないかも」の根拠に挙げた`sweep_resume_race_tests.rs`は、まさに`start_paused`を**使わない**理由として、実TCP接続中にauto-advanceが`TARGET_CONNECT_TIMEOUT`/`PENDING_ACTIVATION_TIMEOUT`を発火させうると書いている | **rev0の引用は誤りだった**。訂正した(§3.3)。in-memory target connectorは条件付きではなく前提(seam S-c)にした。rev0の「条件付きgo」は削除 |
| **M4** | `rng_seed`はtokioのunstable API(`--cfg tokio_unstable`が要る)で、`#[tokio::test]`マクロからは設定できない。「テスト側で1日以内に手当て」は成り立たない | 再確認した(§3.1)。決定論性の基準を「完全一致」から「**同時刻のtie順を除いた一致**(不変条件+時刻ごとの多重集合)」に変えた。本番の`select!`に`biased;`を足すことは範囲外と明記(§6.2、§10) |
| **M5** | noqのendpoint RNGは`EndpointConfig::rng_seed`で固定できるが、quicmuxは`EndpointConfig::default()`を直書き。CID生成・reset keyは常にthread rng。TLS(ECDSA署名長)も毎回変わる | 再確認した(§3.4)。traceはisekaiレベルのイベントと(送信元,宛先,仮想時刻)ごとのパケット数だけを記録し、サイズ・バイトを記録しない。L1用テスト証明書はEd25519。quicmuxの`EndpointConfig`注入をseam S-dとして数えた |
| **M6** | 本番seamの数え漏れで、見積もりは6〜9 PRではなく9〜12 PR+Step 2a/5/11待ち | 受け入れた。§8.1をseam一覧つきで作り直した(9〜12 PR) |
| **M7** | G3(noqのidle timeout)はどのQUICでも通り、isekaiの判断を見ない。G6(5回連続緑)は標本が少なすぎる。「決定論のために本番変更が要った」がno-go条件に無い | G3をisekaiの判断(`resume_loop`のgive-up/resume期限)に置き換え、G6を200回以上の反復に変え、**「本番変更が要った」をre-decideの強制条件**にした(§6.2) |
| m1 | tokio版の[EXT]は閉じられる | 1.52.3と1.53.1の`time/clock.rs`が同一であることを確認(§3.1) |
| m2 | noq `Path::close`の実時計読みはisekaiから到達しない | 確認し、既知のflake源から外した(§3.1) |
| m3 | `resume_loop.rs:1770-1773`の`SystemTime`はwarm-standbyタスク内(`tethering_interface`があるときだけ)で、一般のsleep/wake検出ではない | 確認し、§2のsleep/wake行とQ-L1-8を直した |
| m4 | `isekai-link-masque`の`RelayUdpSocket`(mpsc上の`noq::AsyncUdpSocket`)が手本として良い | 確認し、§4.3・§11に追加 |
| m5 | 実noqを仮想時間で動かしている既存テストは無い。「noqのタイマーは仮想時間に載る」は推測 | 「推測」に格下げ(§3.1) |
| m6 | Step 2aの依存は正しいが、ブランチを引用すべき | `feat/step2a-serve-aggregate`(`2a45198f`)の`stamp()`・`parked_since: Option<Millis>`を確認して引用(§3.1、§9) |
| m7 | turmoilの却下理由は、時計ではなく「TCP/stdioのseamに何も寄与しない」が本質 | §4.2を書き直した |
| 代替案1・2 | 定数関係テストと、2a/5のreducer合成proptestが最有力の競合案なのに評価されていない | §4.5・§4.6として評価し、**推奨に採用**(§5) |

### rev0(2026-10-06)

初版。「in-memory UDP socket + `start_paused`のL1 SPIKEを今行う」を推奨していた(rev1で撤回)。

---

## 1. 背景

RES ADRはL1を「L0とL2で足りなくなるまで着手しない」と保留した(RES ADR §2)。理由は (a) 作業量に対しL2と見つかるバグが重なる、
(b) 仮想時間固有の制約(RES ADR §5)、(c) L0のモデルテストで(B)の一部が安く満たせる、の3つ。

その後FCIS ADRが承認され、判断ロジックはreducer+proptestへ、shellの原子性は単一集約へ、という方針が決まった。FCIS ADR §5は
「Step 1.5〜3aの後、『reducerと`start_paused`で拾えないバグは何か』がL1再評価の材料になる」と書いている。本ADRはその再評価を
前倒しで行う。問いは「**reducerが原理的に見ないもの(実noqのタイマー・パス検証、2プロセスの時間方針の合成、shellの配線)のうち、
L1でしか安く決定論的に見られないものはあるか**」。

rev1の結論を先に書くと: 2プロセスの時間方針の合成は、Step 2a(server)とStep 5(client)のreducerを1つのproptestで合成すれば
noq・socket・tokio無しで見られる(§4.6)。L1にしか見えないのは「実noqのタイマー意味論とisekaiの判断の組み合わせ」と
「shellの配線を故障つきで通すこと」で、履歴上その実例は約1件(修正済み、2aでモデル化済み)しか無い(§7)。一方でL1には
本番seamが少なくとも6つ要る(§8.1)。費用対効果が合わないので、L1は保留を続ける。

## 2. シナリオと検証層

不変条件の記号はRES ADR §3(I1復旧・I2aサーバー側リーク無し・I2bクライアント側リーク無し・I3死んだHandle非再利用・
I4状態の単調性・I5無音の失敗無し)。「合成proptest」は§4.6(Step 2a+5のreducer合成)、「定数関係」は§4.5。

| シナリオ | 検証する不変条件 | L0 / reducer(FCIS)/ 合成proptest | L1(保留中、再開した場合) | L2 netns | 実機のみ |
|---|---|---|---|---|---|
| NAT rebind(passive migration) | I1(QUIC migrationで継続) | 対象外(実noqが要る) | 可(仮想ルータが見かけのアドレスを書換え。noq-proto自身のテストに同種の操作`TestOp::PassiveMigration`がある、`noq-proto/src/tests/random_interaction.rs`、確認済み) | 可 | キャリアCGNAT |
| ローミング(rebind) | I1 | 対象外 | 可(quicmuxのrebindがseamを通らない問題の解消が前提、seam S-e) | 可 | `Network.bindSocket()`の切替 |
| サイレントblackhole | I5、検出時間の上限(RES ADR Q8) | L0-2(russh keepaliveのみ) | 可(仮想時間で検出時間を測れる) | 可(実時間で数分) | — |
| 完全切断 → resume | I1、I2a、I5 | Step 1.5/2a/10がserver集約、Step 5がclient判断を単独で。**合成proptestが両者の時間関係を**見る | 可(ただし`run_resume_loop`のstdio seamが前提、seam S-a) | 可(resume windowを短縮した設定で) | — |
| server側zombie relay(857f6ae6 D-2型) | I1、I2a | Step 2aの`RequestPreempt`(判断部分)。時間部分(preempt待ち×clientのステップ×serverのidle timeout)は、preempt待ちがEffect化されるまで合成proptestでは見られない(rev2、§4.6の性質(3)) | 可 | 可 | — |
| BUSY_OTHER_SESSION × park失効 | I1 | **定数関係テスト**(算術)、合成proptest(同一版の振る舞い。Step 2b+拒否理由の追加が前提、§4.6の性質(4)) | 初回確立ラッパーにfactory注入が要る(seam S-b)。古いserveとのバージョン差は模型化できない(§7) | 古いバイナリを並べれば可 | — |
| relay⇔STUN直結⇔cross-familyの候補切替 | I1 | 候補選択の純粋部分 | 部分的(cross-familyのみ。STUN/relayのsim化は範囲外) | 可 | Tailscale実体 |
| 物理multipath | — | — | **対象外**: noq issue #738で断念済み(`rust-core/Cargo.toml:17-18`のコメント、`isekai-transport/Cargo.toml:52-53`、確認済み) | — | — |
| sleep/wake(プロセス凍結→時間跳躍) | I1、I5 | Step 3a(`pending_wake`) | 限定的: 両方向blackhole+時間経過でネットワーク面だけ近似できる。単一ランタイムでは「自プロセスの凍結」は表せない。なお`resume_loop.rs:1770-1773`の`SystemTime`跳躍検出は**warm-standbyタスク内**で、`tethering_interface`が`Some`のときだけ動く(`:1761-1786`、確認済み)。実インタフェースにbindする(`WarmStandby::new_bound_to_interface`)のでsimの範囲外 | SIGSTOP/SIGCONT | Doze、OEM kill、Windows holder |
| fencing slotの規律 | I2a | Step 1/1.5/2a/10 | 可(実engine) | 可 | — |
| 複数クライアントが同じhostへ(05c40376型) | I2a | Step 1 | 可 | 可 | — |
| Android app経路(`resume_client.rs`/`isekai_pipe_quic_transport.rs`/orchestrator) | I1〜I5 | Step 2.5/3a/8a′ | 範囲外(transport層の`RUNTIME.spawn`16箇所、§3.2) | L2はCLIのみ | 実機 |
| 配線漏れ(W)、Windows/bootstrap/quoting(P)、mux holder | — | 対象外 | 不可 | 一部 | 実機 |

**注意(確認済み)**: L1が対象にできるのは`isekai-pipe connect`(= `isekai-ssh`経由のCLI)の`resume_loop.rs`と共通のserve engine。
Androidアプリは`resume_loop.rs`ではなく`rust-core/src/resume_client.rs`を使う。合成proptest(§4.6)も同じで、clientはStep 5の
(CLI用)reducer。Android側のclient判断との合成は、Step 3aのreducerを使えば同じ形で書ける見込み(推測)。

## 3. 判明した事実

### 3.1 時計・乱数(決定論性の障害)

- **std時計とtokio時計の混在(main)**:
  - serve engine: `Session::parked_since: Option<std::time::Instant>`(`engine/resume.rs:59`)、sweepの期限は`since.elapsed()`
    (`engine/resume.rs:290`付近)、parkの刻印は`std::time::Instant::now()`(`engine/mod.rs:1528,1684,1709`)。sweepの周期だけ
    `tokio::time::sleep(5s)`(`engine/mod.rs:797`付近)。→ mainでは仮想時間でpark失効を起こせない(確認済み。Step 1.5のテストも
    `parked_since`を過去にずらす方法で失効を作っている、`sweep_resume_race_tests.rs:49-53`)。
  - **Step 2aで解消される(ブランチ`feat/step2a-serve-aggregate` `2a45198f`、確認済み)**: `AttachRuntime`に
    `epoch: tokio::time::Instant`(`engine/attach_runtime.rs:248,263`)と`fn stamp(&self) -> Millis`(`:270-271`)、
    `parked_since: Option<Millis>`(`engine/serve_fsm.rs:45`)。
  - `resume_loop.rs:11`は`std::time::{Duration, Instant}`をimportし、本番部分(`:2110`より前)で`Instant::now()`を13箇所読む
    (FCIS ADR §1.2の数え、確認済み)。`:937-943`のコメントが、両者を混ぜて「pause中にビジーループした」実害を記録している。
    **Step 5で解消される予定**(Step 5は未着手)。
  - `isekai-transport`/`quicmux`本体にも`Instant::now`/`SystemTime::now`の行が17行ある(grepの行数。テスト内か本番かは未精査)。
  - noq: `noq::Path::close`は`crate::Instant::now()`を直接読む(`noq/src/path.rs:201`)が、quicmux・isekai-transport・isekai-pipeは
    `Path::close`/`close_path`を呼ばない(grep、確認済み)。**現在isekaiから到達しない**。将来呼ぶ場合、pause下で実時計の
    `Instant`をnoq-protoへ渡すと、仮想時刻が進んだ後の`now`より過去の値になる(時間の逆行)ことに注意。
- **noqのタイマーが仮想時間に載るか(推測)**: noqの`TokioRuntime`は`sleep_until`でタイマーを作り、`now()`は
  `tokio::time::Instant::now().into_std()`(`noq/src/runtime/tokio.rs:24-40`、rev `db998f2d`、確認済み)。ソース上は仮想時間に載るが、
  **実noqを仮想時間で動かしている既存テストは無い**(pause付きテストでQUIC型に触れるのは`src/resume_client.rs:681,714`、
  `isekai-transport/src/resume/app_ack.rs:241`、`resume_loop.rs:2940-2988`で、どれもnoqのhandshakeを駆動しない。レビューの調査、
  ファイルの存在は確認済み)。
- **tokioの版とauto-advance**: `Cargo.lock`は`tokio 1.52.3`(確認済み)。RES ADR §5が引いた1.53.1と`src/time/clock.rs`は**同一**
  (diff空、確認済み)。auto-advanceの機構(`runtime/time/mod.rs:258-280`、1.52.3、確認済み): `can_auto_advance()`なら
  `park_timeout(0)`してから、時間ドライバが起こされていなければ`clock.advance(duration)`する。**その瞬間に完了していない実I/Oは、
  最も早いタイマーに負ける**。
- **blocking poolはauto-advanceを止める(確認済み)**: blocking poolのタスクは生成時に`clock.inhibit_auto_advance()`を呼ぶ
  (`tokio-1.52.3/src/runtime/blocking/schedule.rs:20-28`、current_threadの場合)。止まっている間、`park_thread_timeout`は実時間で
  寝る(上の`else`分岐)。tokioの`Stdin`は`Blocking<std::io::Stdin>`(`io/stdin.rs:28-31`)。
- **`select!`の分岐順の乱数(確認済み)**: `Builder::rng_seed`は`cfg_unstable!`の中(`tokio-1.52.3/src/runtime/builder.rs:1225,1369`)で、
  `--cfg tokio_unstable`が要る。`tokio-macros`(cacheにある2.7.0〜2.7.2)に`rng_seed`属性は無い(grep)。`rust-core/.cargo`と
  `.github/workflows`に`tokio_unstable`の指定は無い(grep)。`isekai-pipe/src`の`select!`は26箇所で`biased;`は0箇所
  (quicmux+isekai-transportで6箇所、grep、確認済み)。→ **同じ仮想時刻に2つの分岐が準備完了になると、どちらが選ばれるかは
  実行ごとに変わりうる**。
- **noq/rustlsの乱数(確認済み)**: `noq_proto::EndpointConfig::rng_seed`(`noq-proto/src/config/mod.rs:161`)があり、無ければ
  `StdRng::from_rng(&mut rand::rng())`(`endpoint.rs:70-72`)。quicmuxは`noq::EndpointConfig::default()`を直書きする
  (`noq_backend.rs:332,423`)ので、seedを入れるには本番変更が要る。CID生成は既定の`HashedConnectionIdGenerator`でも
  `rand::rng()`を読み(`cid_generator.rs:137-140`)、stateless reset keyも`rand::rng()`(`config/mod.rs:188-190`)。TLSの乱数
  (client random、ECDHE、ECDSAの署名nonce)はカスタム`CryptoProvider`無しには固定できない [EXT]。テスト証明書は
  `rcgen::generate_simple_self_signed`(`quicmux/src/test_support.rs:36`)で、既定はECDSA P-256 [EXT] — DERの署名長が70〜72バイトで
  揺れるので、handshakeのパケット長も毎回変わりうる(推測)。noq自身のmapは`FxHashMap`(RandomStateでない)だが、CIDをキーにする
  mapの反復順はCIDの乱数に依存する(推測)。
- `faulty_udp_socket.rs`: フォルト判定は`rand::thread_rng()`(`:97`)、遅延は`std::time::Instant::now()`(`:207,231,259`)、
  遅延送信は`tokio::spawn`(`:183`付近)。seed再現不能で、仮想時間に一部しか載らない(RES ADR §4と同じ)。

### 3.2 グローバル`RUNTIME`

- `crate::RUNTIME`(`rust-core/src/lib.rs:64`)。Step 2.5(#141)でorchestrator(`shared.rt`、`orchestrator.rs:1210`)とpool
  (`release_on`、`pool.rs:183`)は注入可能になった(確認済み)。
- `isekai-terminal-core`のtransport層には`RUNTIME.spawn`が16行残る: `isekai_pipe_quic_transport.rs:133,166,478,552,588`、
  `multipath_transport.rs:188,765,819,1024`、`session.rs:331,552`、`quic_transport.rs:57`、`isekai_stun_p2p_transport.rs:99`、
  `isekai_link_relay_transport.rs:87`(確認済み)。Android経路のL1にはこれらの注入が要る。
- `isekai-pipe`はグローバル`RUNTIME`を持たず(`isekai-pipe/src`の`RUNTIME`は環境変数名`ISEKAI_PIPE_RUNTIME_DIR`のみ)、
  `isekai-terminal-core`にも依存しない(`isekai-pipe/Cargo.toml`、確認済み)。

### 3.3 isekai-pipeのコード上の障害(確認済み)

- **crate構造**: `isekai-pipe/Cargo.toml:8-10`は`[[bin]]`のみ。外部テスト(`tests/*.rs`)からengineは呼べないが、crate内の
  `#[cfg(test)]`からは`engine`も`resume_loop`も呼べる(Step 1.5の`engine/sweep_resume_race_tests.rs`、#140が実例)。
- **Step 1.5のテストは`start_paused`を使っていない(rev0の引用を訂正)**: 同ファイル`:47-57`の「# Why not `start_paused`」は、
  (1) `parked_since`がstd時計なのでpauseは何も足さない、(2) **実loopbackの`TcpStream::connect`が完了する前にauto-advanceが
  `TARGET_CONNECT_TIMEOUT`/`PENDING_ACTIVATION_TIMEOUT`を発火させうるので有害**、と書いている。テストは素の`#[tokio::test]`
  (`:173,202`)で、決定論性はtokio `Mutex`のFIFO公平性から得ている。rev0はこのファイルを「実loopback TCPでも決定論的に動く」
  根拠に挙げたが、**逆の主張をしているファイルだった**。
- **engineのtarget接続**: `AttachRuntime::start_connect`が`tokio::time::timeout(TARGET_CONNECT_TIMEOUT, TcpStream::connect(target_addr))`
  を`tokio::spawn`する(main `engine/attach_runtime.rs:315`。Step 2aブランチでは`:539`)。`TARGET_CONNECT_TIMEOUT`=20秒(`:49`)、
  `PENDING_ACTIVATION_TIMEOUT`=5秒(`:37`)。pause下で実TCPを使うと上のauto-advanceの機構で誤発火しうる。
- **engineの入口**: `run_from_args`(`engine/mod.rs:534`)だけで、ロガー初期化と`install_default`(`:538-544`)、実socketのbind
  (`:630-632`、STUN probeのため生socketが要る)を含む。listenerを外から渡す入口は無い。server側の既定値(例: QUIC idle timeout
  15秒)は定数ではなく引数解析内のリテラル(`let mut idle_timeout = 15u64;`、`engine/mod.rs:264`)。
- **`run_resume_loop`のstdio(B2)**: `let mut stdin = tokio::io::stdin(); let mut stdout = tokio::io::stdout();`
  (`resume_loop.rs:1734-1735`)。引数は8個(`:1684-1693`)でstdioを受け取らない。結果:
  - stdinが`/dev/null`(nextestの典型)なら最初の読み出しでEOF → `c2h_already_done`が立ち、最初の`PumpFailure::Remote`で
    `should_give_up_without_resuming`(`:1676-1682`)が`true`を返す。**resumeシナリオが全部「give up」で終わる**(推測、ロジックは確認済み)。
  - stdinが開いたままのpipeなら、blocking poolの読み出しが続く間auto-advanceが止まり、仮想sleepが実sleepになる(推測、機構は§3.1で確認済み)。
- **BUSY_OTHER_SESSIONの再試行の位置(M1)**: `retry_while_busy_other_session(BUSY_OTHER_SESSION_RETRY_WINDOW, ..)`の本番呼び出しは
  初回確立のラッパーだけ(`resume_loop.rs:445,468,540`、`connect.rs:785`)。`run_relay_resumable`は自前で
  `relay_endpoint_factory(relay_transport)`(`resume_loop.rs:443`)→`system_quic_factory()`(`connect.rs:95-97`)を作るので、
  adapter付きfactoryを差せない。`run_resume_loop`を直接呼ぶハーネスはこの再試行を通らない。
- **`BUSY_OTHER_SESSION_RETRY_WINDOW`(180秒)の意味(M2)**: docコメント(`resume_loop.rs:369-388`)によれば、preemption修正
  (`hello_with_parked_preemption`)を持つserveに対してはほぼ即座に成功し、この窓は**修正前の古いserve**(helper再利用で最大30日
  生き残る)に対する防御。
- **Step 2a後もshellに残る判断(rev2、N1。ブランチ`feat/step2a-serve-aggregate` `2a45198f`で確認)**: `ServeEffect`のRESUME結果は
  `ResumeGranted { id, lease }`・`RequestPreempt { id, lease }`・`ResumeRejected { id }`で、**拒否理由を持たない**(`serve_fsm.rs`)。
  preempt後の待ち(`tokio::time::timeout(PREEMPT_WAIT_TIMEOUT, notified)`、ブランチの`engine/mod.rs:1359`)と、その後の
  `respond_resume_rejected(.., UnknownToken)`(`:1365`)、HELLO admissionの`AttachRejectReason::BusyOtherSession`(`:1066`)はshell側。
  同ブランチの`serve_fsm.rs`自身が「admissionのcheck-then-act自体の解消はStep 2bの範囲」と書いている。

### 3.4 socketのseam(quicmux)

- クライアント: `AsyncUdpSocketAdapter`(`noq_backend.rs:87-88`)、`NoqFactory::with_socket_adapter`(`:303`)/
  `AnyMuxFactory::noq_with_socket_adapter`(`mux.rs:45`)。`create_endpoint`は先に実socketをbindしてからadapterに渡す(`:307-311`)。
  `endpoint_from_abstract_socket`(`:331-334`)なら実bind無し。
- サーバー: `AnyMuxListener::from_abstract_socket_noq`(`mux.rs:199`、`noq_backend.rs:418-429`)。
- seamを通らない経路: `NoqRebinder::rebind_socket`/`rebind`は`noq::Endpoint::rebind(std::net::UdpSocket)`(`noq_backend.rs:378-385`)
  で、noqの`rebind_abstract`(`noq/src/endpoint.rs:280`)を使わない。`NoqListener::bind`(`:399-403`)も`noq::Endpoint::server`で直接bind。
- **既存のchannel上の`noq::AsyncUdpSocket`(m4)**: `isekai-link-masque/src/relay_client.rs`の`RelayUdpSocket`(mpscの
  `recv_rx`/`send_tx`、`:417-421`)と`impl UdpSender for RelayUdpSender`(`:439`)・`impl AsyncUdpSocket for RelayUdpSocket`(`:452`)。
  本番で`from_abstract_socket_noq`経由でnoqと動いている。L1を再開する場合の`SimUdpSocket`の手本(`FaultyUdpSocket`より適切)。

### 3.5 noq-proto(sans-io)

- `noq-proto 1.1.0`(git rev `db998f2d`、`Cargo.lock`確認済み)はsans-io API一式を公開: `Endpoint::handle`(`endpoint.rs:150`)、
  `Endpoint::connect`(`:333`)、`Connection::poll_transmit`(`connection/mod.rs:1039`)、`handle_event`(`:2228`)、`handle_timeout`(`:2450`)、
  `poll_timeout`(`:468`)、`poll`(`:478`)、`open_path`(`:570`)、`handle_network_change`(`:5837`)。時刻は引数の`std::time::Instant`
  (`lib.rs:115`)。
- noq-proto自身の決定論的シミュレーション基盤(`tests/util.rs:38`の`Pair`、`blackhole_step` `:160`、`advance_time` `:280`、
  `Routing` `:1654`、`tests/random_interaction.rs`のproptest)は`#[cfg(all(test, ..))] mod tests;`(`lib.rs:31-32`)の中の`pub(super)`で、
  下流からは使えない(`util.rs`だけで2416行)。
- isekaiのコードは`noq-proto`を直接使わない(`noq_proto`のgrepはコメント2行のみ)。

### 3.6 既存のQUIC故障テストとL2

- `faulty_udp_socket::tests`・`multipath_transport::tests`は実loopback UDP+実時間で、CI負荷でflakeするため`nextest.toml`で
  `retries = 2`(確認済み)。`isekai-transport/tests/{rebind_e2e,resume_e2e,multipath_e2e}.rs`は実loopback上で実rebind/resume/multipathを
  検証(`resume_e2e.rs`のserverはmockで実engineではない、冒頭doc)。`isekai-pipe/tests/fencing_e2e.rs`は実`isekai-pipe serve`
  サブプロセス+実QUICでfencingを見る(冒頭doc)。
- L2: `rust-core-netlab-check.yml`は`workflow_dispatch`のみ、シナリオ1本、`timeout-minutes: 15`(確認済み)。夜間化は未実施。
- 既存の「時間定数の関係」テストの手本: `isekai-transport/src/system.rs`の`#[cfg(test)] mod timing_relations`
  (`keep_alive_is_a_third_of_the_idle_timeout`等、`:85-93`、確認済み)。

### 3.7 外部シミュレータ [EXT](2026-10-06にdocs.rs/GitHubで確認、cargo cacheには無い)

- **turmoil 0.7.2**: 複数hostを単一スレッドで決定論的に実行。`turmoil::net`が`tokio::net`を模す。アプリ側の差し替え方法には
  「まだ意見を持たない」。`turmoil::net::UdpSocket`に`poll_*`は無く、`readable()`/`try_recv_from()`/`try_send_to()`/async版を持つ。
- **madsim 0.2**: `tokio`→`madsim-tokio`、`RUSTFLAGS="--cfg madsim"`。互換版はtonic/etcd-client/rdkafka/aws-sdk-s3等で、quinn/noqは無い。
  libcレベルでの時計・乱数の横取りはREADMEでは確認できなかった(未検証)。
- **shuttle**: スレッド/`std::sync`の並行性テスト道具という理解(未確認)。ネットワークをモデル化しない。

## 4. 選択肢の比較

### 4.1 選択肢1: noq-proto(sans-io)を直接駆動する — 不採用

API面では十分(§3.5)で完全に決定論的だが、**isekaiのコードを通らない**。isekaiの接続耐性ロジック(quicmuxのresume/control stream、
`isekai-transport`の`reconnect_and_resume`/候補/warm standby、engineのfencing/park/sweep、`resume_loop`の判断)はすべて非同期`noq`の上にある。
テストできるのはnoq自身のQUIC実装で、上流が`Pair`+`random_interaction.rs`で既に検証している。

### 4.2 選択肢2: turmoil / madsim / shuttle — 不採用

- **turmoil**: noqに使うには`turmoil::net::UdpSocket`→`noq::AsyncUdpSocket`のadapter(`readable()`を保持して`try_recv_from`)が要り、
  これは選択肢3の`SimUdpSocket`とほぼ同じ量。**そのうえで、L1の本当の障害であるTCP target(seam S-c)とstdio(seam S-a)には何も
  寄与しない**(turmoilの`net::TcpStream`を使うにはengineの`tokio::net::TcpStream`を本番コードで差し替える必要があり、stdioは対象外)。
  時計の横取りの有無は、Step 2a/5の後なら問題にならない。得られるのはhost再起動・partitionのAPIで、isekaiのL1で要る2〜3 hostの
  UDP経路操作には過剰。dev-dependency追加(`Cargo.lock`、`lockfile-drift`)も伴う。
- **madsim**: 依存グラフ全体を`--cfg madsim`でビルドし直す。noqのmadsim版は無く、noq fork・rustls・russhの互換性は未確認。費用が桁違い。
- **shuttle/loom**: ネットワークをモデル化しない(FCIS ADR Step 10/Q16の範囲)。
- どのシミュレータもプロセス全体のグローバルランタイムへのspawnは捕まえられない(Android経路の16箇所、§3.2)。

### 4.3 選択肢3: 既存seamの裏にin-memory UDP socket(L1本体)— 保留

- 形: テスト専用の`SimNet`(仮想アドレス→キュー、リンクごとのloss/遅延/blackhole/NAT書換え、seed付き`StdRng`)と、
  `RelayUdpSocket`(§3.4)を手本にした`SimUdpSocket: noq::AsyncUdpSocket`。`start_paused`の単一current_threadランタイム。
- **rev1で判明した前提**: 実際にisekai-pipeを通すには本番seamが6つ要る(§8.1のS-a〜S-f)。決定論性はtokioの`select!`乱数
  (unstable API無しでは固定不能)、noqのCID・reset key、TLSの乱数のため「完全一致」は得られず、「tie順を除いた一致」が上限(§3.1)。
- 判定(OPINION): 価値に対して費用が大きい(§7・§8)。**保留**。再開条件は§5-3。

### 4.4 選択肢4: L0 + reducer(FCIS)+ netns L2のみ

- FCIS ADRのStep 2a/5/10/11とL2夜間化で、serverの集約・clientの判断・shellの翻訳誤り・実環境の挙動は見える。
  見えないのは「2プロセスの時間方針の合成」と「実noqタイマーとの組み合わせ」。前者は§4.6で埋められる。

### 4.5 選択肢5(rev1新規): クライアント/サーバーの時間定数の関係テスト — **採用(今)**

- 手本: `isekai-transport/src/system.rs`の`mod timing_relations`(§3.6)。clientとserverはどちらも`isekai-pipe` crate(と
  `isekai-transport`・`isekai-pipe-core`)にある。
- **rev2(N3): 2つに分ける**。serverの定数はすべてprivate(`HELLO_TIMEOUT` `engine/mod.rs:72`、`PREEMPT_WAIT_TIMEOUT` main `:81`/
  2aブランチ`:84`、`PENDING_ACTIVATION_TIMEOUT`・`TARGET_CONNECT_TIMEOUT` `engine/attach_runtime.rs:37,49`、確認済み)で、1つのテストから
  両側を見るには、Step 2aが書き換え中のファイルに`pub(crate)`を足す必要がある。そこで:
- **(a) 今(1 PR)**: client側の定数と、crate間で既に`pub`な値だけを使う関係。本番変更なし。置き場所は`resume_loop.rs`の
  `#[cfg(test)]`(client側privateの定数が見える場所)。
  - `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`(30秒、`resume_loop.rs:1133`)≥ 2 × client QUIC idle timeout。意図は同`:1128-1131`のdoc
    「30s is double the idle-timeout default」(確認済み)。idle timeoutは`isekai_transport::system::isekai_mux_config(true).max_idle_timeout`
    (`pub mod system` `isekai-transport/src/lib.rs:93`、`pub fn isekai_mux_config` `system.rs:59`、`MuxClientConfig::max_idle_timeout`
    `quicmux/src/config.rs:39`、確認済み)で本番変更なしに参照できる。dbb80d56の「roaming誤爆」の根拠になった関係。
  - `CROSS_FAMILY_MIN_PROBE_BUDGET` = `TRANSPORT_STEP_TIMEOUT` + 1秒(`resume_loop.rs:186`が式で定義済みなので、テストは式の意図を
    名前付きで固定するだけ。PRで要否を判断)。
  - `BUSY_OTHER_SESSION_RETRY_WINDOW`(180秒)と`DEFAULT_RESUME_GRACE_SECS`(864000秒、`isekai-pipe-core/src/lib.rs:73`)は**意図的に
    無関係**(`resume_loop.rs:369-388`のdoc)。関係テストではなく「無関係であること」をdocに残すだけにする。
- **(b) Step 2aのマージ後(1 PR)**: serverの定数を使う関係。serverの定数への`pub(crate)`付与と、server既定QUIC idle timeoutの
  リテラル(`let mut idle_timeout = 15u64;`、`engine/mod.rs:264`)の定数抽出を含む(挙動保存の本番変更、小)。
  - serverの`PREEMPT_WAIT_TIMEOUT`(2秒)< clientの`TRANSPORT_STEP_TIMEOUT`(15秒、`isekai-transport/src/resume.rs:101`、`pub`)。
    preempt待ちがclientの1ステップ内に収まること。
  - serverの`HELLO_TIMEOUT`・`PENDING_ACTIVATION_TIMEOUT`(どちらも5秒)< clientの`TRANSPORT_STEP_TIMEOUT`。
  - client idle timeout(15秒)とserver既定idle timeout(15秒)の関係(どちらの向きが意図かは、PRで両側のdocから確認する。推測では等値)。
  - **(rev2、N6)意図的な非関係**: serverの`TARGET_CONNECT_TIMEOUT`(20秒)はclientの`TRANSPORT_STEP_TIMEOUT`(15秒)より**長い**。
    `attach_runtime.rs:39-49`のdoc(確認済み)は、targetが遅いとclientが先にタイムアウトするが、新しいgenerationでの再試行が
    `ClosingForSupersede`で自己回復するので「stuck-foreverではない」こと、20秒はnetwork越しのTCP handshakeを縛るもので
    同一プロセス内の5秒系より意図的に長いことを書いている。テストは`TARGET_CONNECT_TIMEOUT > TRANSPORT_STEP_TIMEOUT`を
    「現在の設計判断」として固定し、どちらかの向きに「直す」変更にはこのdocの再検討を求める(assertのメッセージでdocを指す)。
- 費用: (a)(b)それぞれ1 PR。依存追加なし。
- 限界: 値の関係しか見ない。振る舞いの合成(どの順序でEventが届くか)は§4.6。

### 4.6 選択肢6(rev1新規): Step 2a(server)とStep 5(client)のreducerを1つのproptestで合成 — **採用(2a・5の後)**

- 形: `ServeAggregate`(2a、ブランチで実在)と`ResumePlanner`(Step 5、**未実装**。FCIS ADR §6 Step 5の名前)を、テスト内の
  おもちゃのメッセージ路(クライアント→サーバーのRESUME/HELLO、サーバー→クライアントの`ResumeGranted`/`BusyOtherSession`/
  `UnknownSession`等)と、明示的な`Millis`時計で結ぶ。故障は「メッセージの遅延・喪失」と「各側の**QUIC idle timeout満了**Event
  (設定値から導出した時刻で発火)」としてEvent列に入れる。proptestがEvent列(時刻の割当・順序・喪失)を任意に生成する。
- 検査する性質と、**性質ごとの前提(rev2、N1)**。ハーネスがshellのコード(`ServeEffect`→wire上の拒否理由の翻訳、preempt待ち、
  HELLO admission)を手で書き直すと、検査しているのは「ハーネスが書いたshellのモデル」になり、FCIS Step 10が扱う「shellの翻訳誤り」を
  見ないまま緑になる。そこで、shellの再実装が要る性質は、前提のStepが揃うまで書かない:

  | 性質 | 内容 | 前提 |
  |---|---|---|
  | (1) | **clientのplannerがGaveUpなのに、serverのindexがそのsessionについてresume可能なエントリ(parkedで期限内)を保持している**状態に到達しない(I1の有界版。rev2で「経路がある」を検査可能な形に言い換えた) | Step 2a + Step 5 |
  | (2) | server側でfencing slotが現行leaseに無いのにEstablishedのまま(I2a) | Step 2a + Step 5 |
  | (3) | zombie Established relayがある間に届いたRESUMEが、有限ステップ内に許可される(857f6ae6 D-2型) | **判断部分**(拒否ではなく`RequestPreempt`を返す)は2a + 5で書ける。**時間部分**(preempt待ち2秒と、その後の`UnknownToken`応答、§3.3)は、preempt待ちがFCIS §2.2形の**token付きタイマーEffect**として`ServeAggregate`に入るまで書けない。これは2a/2cの後続作業で、**現在どのStepにも予定されていない** |
  | (4) | client期限とserver失効の任意の時刻割当で、give-upとpark失効が食い違わない(BUSY/UnknownSession型) | **Step 2b**(HELLO admissionのreducer化。`BusyOtherSession`は2a後もshell、§3.3)+ `ServeEffect::ResumeRejected`への**拒否理由の追加**(2a後は`{ id }`のみ) |

  代替として、Step 10の差分テストの参照モデル(実shellとreducerの対応表)をハーネスが再利用できるなら、翻訳を二重に手書きせずに
  済む(Step 10のPRで、そのオラクルをテスト間で共有できる形にするかを確認する。Q-L1-12)。
- noq・socket・tokioを使わないので、FCIS ADR §3の規則(非単調`now`、stale token)をそのまま継げる。シミュレーションの
  決定論性の問題(§3.1)は原理的に起きない。
- 限界: QUICの実タイマー意味論(idle timeoutの実効値が`max(idle, 3·PTO)`になる等)は「設定値からの導出」で近似する。
  実際のnoqの挙動との乖離はL2で突き合わせる。shellの翻訳誤りはStep 10(server側)の範囲で、client側のshellは見ない。
- 費用: 1〜2 PR(テストのみ)。前提: 性質(1)(2)はStep 2a・5。(4)はさらに2b+拒否理由、(3)の時間部分はさらにpreempt待ちのEffect化。

## 5. 決定(提案)

1. **今**: 選択肢5(a)(client側の定数関係テスト、本番変更なし)を1 PRで行う。**Step 2aのマージ後**: 選択肢5(b)(serverの定数、
   `pub(crate)`化とリテラル抽出を含む)を1 PRで行う(rev2、N3)。
2. **Step 2a・5の後**: 選択肢6(reducer合成proptest)の性質(1)(2)を行う。(4)はStep 2b+拒否理由の追加の後、(3)の時間部分は
   preempt待ちのEffect化の後に足す(rev2、N1)。FCIS ADRのStep 11(trace検査)の後なら、その`check_trace`形式を流用する。
3. **L1(選択肢3)は保留を続ける**。再評価の条件(どれか1つ):
   - (a) 選択肢6とL2(夜間化後)で検出されず、実機/本番で見つかった不具合のうち、根本原因が「実noqのタイマー/パス検証の意味論と
     isekaiの判断の組み合わせ」または「shell(client側)の配線を故障つきで通らないと見えない」ものが**1件以上**出た。
   - (b) L2の実時間シナリオの実行時間かflakeが運用に耐えなくなり、仮想時間化の需要が具体的になった。
   - 再評価するときは§6のSPIKE設計から始める。
   - **既知の盲点(rev2、N1)**: 合成proptestの性質(3)の時間部分(zombie relayのpreempt待ち×clientのRESUMEステップ)と性質(4)は、
     前提(preempt待ちのEffect化、Step 2b+拒否理由)が揃うまで書けない。それまでの間、このクラスの検出層は定数関係テスト(b)
     (`PREEMPT_WAIT_TIMEOUT` < `TRANSPORT_STEP_TIMEOUT`の値の関係のみ)とL2夜間(§5-4)だけになる。
4. **L2の夜間化(RES ADR Q11)を、本ADRの採択とあわせて追跡事項として登録する**(rev2、N5)。L1保留の判断は「L1が捕まえるはずの
   クラスの最初の検出層はL2夜間」という前提に立っているため。担当はteam-lead経由で割り当て(既定: RES ADRのL2を担当するエージェント)、
   既定の時期は選択肢5(a)と同時期。内容は`rust-core-netlab-check.yml`への`schedule`追加と、§2の「L2: 可」の行のうち少なくとも
   サイレントblackhole・完全切断→resume・zombie relay(旧パスのみblackhole)の3シナリオ。登録されない、または実施されない間は、
   R4のとおり**検出層は実機/本番だけ**になる。
5. シナリオDSL、汎用シミュレーションframeworkは作らない(L1再開時も)。

## 6. (保留中)L1を再開する場合のSPIKE設計

rev0のSPIKEはisekai-pipeのコードを通らず、no-goを出せない設計だった(B1)。再開時は次の形で行う。

### 6.1 前提と置き場所

- 置き場所: `isekai-pipe` crate内の`#[cfg(test)]`モジュール(例: `isekai-pipe/src/engine/sim_spike_tests.rs`)。
- **Step 2aがmainにマージ済みであること**(engineの時計が`tokio::time::Instant`由来の`Millis`、§3.1)。mainのstd時計の上では
  park失効を仮想時間で起こせない。
- テスト証明書はEd25519(決定的な固定長署名 [EXT])。`self_signed_server_config`(ECDSA)は使わない。

### 6.2 通すコード(すべて実物)

- 実`AttachRuntime`とengineのaccept経路を`from_abstract_socket_noq`の裏で。
- in-memory target connector(`tokio::io::duplex`)。**実loopback TCPは使わない**(§3.3の機構で誤発火する)。
- `run_resume_loop`を少なくとも1世代(stdio seam経由のin-memory pipe)。
- 少なくとも1つの`select!`で、timer分岐とpacket分岐が**同じ仮想時刻に**準備完了になるシナリオ(例: RESUMEの応答がちょうど
  `TRANSPORT_STEP_TIMEOUT`の満了時刻に届く)。

### 6.3 go/no-go基準

| # | 基準 | goの条件 |
|---|---|---|
| G1 | 実socketが何も運ばない(rev2、N4で緩和) | 実UDP・実TCPが**データグラム/バイトを1つも運ばない**。bindだけされた未使用のloopback UDP socketは許す: clientの`reconnect_and_resume`→`factory.create_endpoint`(`isekai-transport/src/resume.rs:740`)→`NoqFactory::create_endpoint`は**常に**実socketをbindしてからadapterを呼ぶ(`quicmux/src/noq_backend.rs:307-311`、確認済み)ので、bind 0は本番seam無しには満たせない。未使用socketは完了待ちのI/Oを持たないのでauto-advanceと競合しない(§3.1の機構から推測) |
| G2 | 決定論性(**tie解決ベクトルごと**、rev2、N2で再定義) | 同一seedで200回以上の反復(GitHub Actions上で、simテストだけを繰り返し実行するジョブ)。各実行で、**同じ仮想時刻に複数分岐が準備完了だった`select!`ごとの分岐選択の列**(tie解決ベクトル)を記録し、実行をこのベクトルで群に分ける。**同じ群の中では**、isekaiレベルのイベント列(接続/park/resume/give-up/slotの取得・解放)と(送信元,宛先,仮想時刻)ごとのパケット**数**が一致し、**すべての群で**不変条件(I1/I2a/I5)が成り立つ。パケットのサイズ・バイトは比較しない(§3.1のTLS・CID乱数)。tie解決ベクトルの記録には、§6.2の対象`select!`の各分岐にテスト用の記録点(`cfg(test)`のカウンタ等)が要る(どこまで本番コードに触れるかはSPIKEで判断し、触れる場合は下のre-decide条件に当たるかを確認する) |
| G3 | isekaiの判断の時刻 | `resume_loop`のgive-up/resume期限、serverのpark失効が、設定から導出した仮想時刻に発火する(noqのidle timeoutではなくisekaiの判断で判定する) |
| G4 | 誤発火なし | 200回の反復で、`TARGET_CONNECT_TIMEOUT`・`PENDING_ACTIVATION_TIMEOUT`・`TRANSPORT_STEP_TIMEOUT`の誤発火が0 |
| G5 | 速さ | 1シナリオの実時間がCI上で2秒以下 |
| G6 | tie分岐の両側 | G2の群が**2つ以上**観測される(§6.2の同時刻`select!`の両分岐が実際に選ばれた)。tie順の非決定性を「隠す」のでなく「両側を通す」ことの確認。両分岐の後の未来(例: 応答分岐ならresume成功、タイムアウト分岐なら`TransportError`→backoff→次世代、`isekai-transport/src/resume.rs:742`以降)は異なってよく、G2は群ごとに判定するので両立する |

- **re-decideの強制条件**: SPIKEの中で、決定論性のために§8.1以外の本番変更(例: 本番`select!`への`biased;`追加、
  `--cfg tokio_unstable`のjob全体への適用、noqへのseed注入以外のnoq fork変更)が必要になった場合は、goでもno-goでもなく
  **その時点で止めてADRを改訂し、ユーザーが再判断する**。「条件付きgo」は設けない。
- no-go: G1〜G6のいずれかが満たせない。

### 6.4 kill switch

- SPIKEは単独のテストファイルなので、no-go時はrevert 1つで消える。SPIKEに必要なseam(§8.1)は挙動保存のリファクタとして
  独立にマージ可能にする(L1を止めても残してよい)が、**SPIKEのためだけにseamを先行マージしない**(go判定前は同じブランチ上に置く)。
- 運用中: L1テストは`nextest.toml`のretry対象に入れない。flakeは2週間以内に直せなければ該当シナリオを夜間ジョブへ移し、
  週1件超のflakeが1か月続いたらL1全体を夜間のみに格下げする。

## 7. 不具合履歴に対する評価(rev1で訂正)

分類(Step 9報告): A=shell原子性/順序、D=判断ロジック、P=プラットフォーム/IO/設定、W=配線漏れ。コミットの存在とsubjectは確認済み。
「捕まえられたか」はOPINION(該当シナリオ/性質を書けば、という仮定つき)。

| コミット | 内容 | 分類 | L1で | 定数関係(§4.5) / 合成proptest(§4.6) / 既存・予定の層 |
|---|---|---|---|---|
| 33973aec / 2308e7d4 | park期限切れ・LRU立ち退きでslot未解放→永久BUSY | A | 可 | Step 1.5/2a(構造的に除去)、合成proptest(I2a) |
| 886bbb15 | parkedが新規接続を最大10日ブロック | A+D | 可 | Step 2a、合成proptest |
| 05c40376 | 同一hostへの2本目が180秒拒否 | D | 可 | Step 1 |
| **857f6ae6 D-2** | zombie Established relayがRESUMEを拒否 | A+D | 可 | 修正済みで、Step 2aが`RequestPreempt`をモデル化済み。**合成proptestで届くのはreducerの判断(拒否ではなくpreempt)だけ**。D-2をL1候補にしていた**時間の合成**(preempt待ち2秒×clientのステップ×serverのidle timeout)は、preempt待ちがtoken付きタイマーEffectになるまで合成proptestでは届かない(rev2、N1で格下げ。§4.6)。それまではL2夜間と定数関係(b)の値の関係だけ。**L1が最も固有に近い唯一の候補であることは変わらない** |
| 75d08a39 → fd32ce11 → 3d5e0da5 | BUSY_OTHER_SESSIONのclient期限 vs serverのpark失効 | D | **L1固有ではない**(rev0の主張を撤回) | 算術は定数関係テストで届く。残っているリスクは修正前の古いserveとの**バージョン差**(§3.3)で、同一コミット同士のL1も合成proptestも模型化できない(L2で古いバイナリを並べるか、実機)。加えて再試行は初回確立ラッパーにあり、L1ハーネスで通すにはseam S-bが要る |
| cc5fb926 → 71292e68 → dbb80d56 | UnknownSession give-upがroaming中に誤爆 | D | 部分的 | Step 5、定数関係(`UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`×idle timeout) |
| 03224b11 | resume window超過で`Ok`を返す | D | 可 | Step 5、Step 12 |
| 3205178f | connect/RESUMEにtimeoutが無い | A/P | 可 | **L1固有ではない**: 応答しない実loopback UDP socketに対する`reconnect_and_resume`を`start_paused`で走らせれば、完了待ちのI/Oが無いのでauto-advanceと競合せず、L0で決定論的に再現できる(推測)。「ネットワークawaitには必ずtimeout」をStep 7a型のlintにする案もある |
| 5d66a7b6 | sweepがACKのresume-graceを無視 | D | 可 | Step 2a |
| b449d7da / 8eb81411 / 4c02e605 | panic窓の孤児化、sweepループの死 | A | 不可 | RAII、Step 2a |
| eecba351 / e221d2ec / a1293255 / 773f807c | orchestrator・`resume_client`のwake取りこぼし | A | 不可(Android経路) | Step 3a、`start_paused`のshellテスト |
| a266f1f3 / 204d8f59 | wrapperのretry 1回きり、孫プロセスのループ | D | 不可 | Step 6+7、12、5 |
| 538ed313 | `--experimental-network-rebind`がmultipath未ネゴで壊れる | 設定(QUIC) | 可 | `rebind_e2e.rs`(実loopback)が既に固定 |
| W分類(2a07a5e3、8baa49d8、db3a6d87、ce214ef5、ae8ed13b、119205f6、e1c370e5、7b10472a) | 配線漏れ | W | 不可 | FCIS ADR D5(未決) |
| P分類(Windows/bootstrap/native mux等) | | P | 不可 | 実機・e2e |

**集計(OPINION、rev1で訂正、rev2で補足)**: L1にしか見えない過去の不具合は**約1件(857f6ae6 D-2)で、修正済み・Step 2aで判断部分は
モデル化済み**。合成proptestが届くのはその判断部分だけで、時間の合成はpreempt待ちのEffect化までL2夜間に頼る(rev2、N1)。
rev0の「3〜5件」はBUSY×3と3205178fを数え過ぎていた(M2)。L1の残る価値は「将来の、実noqの
タイマー意味論やclient shellの配線が絡む2プロセスの時間的不具合」という**仮定上の**クラスに限られる。これが§5の推奨変更の根拠。

## 8. 費用(L1を再開した場合)

### 8.1 本番seamとPR数(rev1で作り直し、OPINION)

| seam | 内容 | 触るファイル | FCIS Stepとの衝突 |
|---|---|---|---|
| **S-a** stdio | `run_resume_loop`がstdin/stdoutを引数(`AsyncRead`/`AsyncWrite`、または`LocalIo`)で受け取る。呼び出し元(`resume_loop.rs:448,471,498`、`connect.rs`)の変更 | `resume_loop.rs`、`connect.rs` | **Step 5**(同関数のshellを書き換える)。Step 5と同時かその後 |
| **S-b** 初回確立のfactory注入 | `run_relay_resumable*`/`run_stun_p2p_*`がfactoryを受け取る(現在は`relay_endpoint_factory`を内部で作る) | `resume_loop.rs`、`connect.rs` | Step 5、Step 12(`connect.rs`) |
| **S-c** target connector | `AttachRuntime::start_connect`の`TcpStream::connect`を差し替え可能にする(in-memory duplex) | `engine/attach_runtime.rs` | **Step 2a**(同ファイルを書き換え中)、Step 10。2aの後 |
| **S-d** `EndpointConfig`注入 | quicmuxが`EndpointConfig`(`rng_seed`、`cid_generator`)をtest-support経由で受け取る | `quicmux/src/noq_backend.rs` | なし |
| **S-e** rebindのseam | `NoqRebinder`がadapter経由で`rebind_abstract`を呼べる。`NoqListener`のadapter付きbind | `quicmux/src/noq_backend.rs`、`mux.rs` | なし |
| **S-f** serve入口 | `run_from_args`から`serve_on(listener, ServeConfig)`を抽出(ロガー初期化・実bind/STUNを分離) | `engine/mod.rs` | **Step 2a/10/2b/2c**(同ファイル)。それらの後 |

| Phase | 内容 | PR数 |
|---|---|---|
| SPIKE | §6(S-a・S-c・S-d・S-fの最小版を同じブランチに含む) | 1〜2 |
| seam正式化 | S-a〜S-fを挙動保存リファクタとして個別PRに | 4〜6 |
| `SimNet`/`SimUdpSocket`の昇格 | quicmux test-supportへ | 1 |
| シナリオ+不変条件 | §2の「可」の行、Step 11の`check_trace`形 | 2 |
| (任意)ランダム故障探索 | 夜間 | 1 |

合計 **9〜12 PR**。加えてStep 2a・5・11の完了待ち。S-a/S-b/S-c/S-fはFCIS ADRの進行中Stepと同じファイルを触るので、
実際の直列化はさらに長くなる(§9)。比較: §4.5は(a)(b)で計2 PR、§4.6は1〜2 PR(性質(3)(4)の前提作業を除く)。
(rev2、N4: clientの`create_endpoint`が常に実socketをbindする点は、G1を緩めたのでseamを追加しない。)

### 8.2 CI費用とflake

- 仮想時間なのでシナリオあたり実時間は秒以下の見込み(未計測)。G2/G4の200回反復は別ジョブ(workflow_dispatchか夜間)。
- flake源(既知): `select!`のtie順(G6で両側を通す)、TLS・CIDの乱数(比較対象から外す)、実I/O(使わない)。

### 8.3 保守

- `SimUdpSocket`はnoqの`AsyncUdpSocket`/`UdpSender` traitに追従する必要がある(noqはfork `cuzic/noq` rev `db998f2d`、上流PR #784
  レビュー中、`rust-core/Cargo.toml:28-39`)。`FaultyUdpSocket`・`android_quic_endpoint.rs`・`RelayUdpSocket`と同じ負担が1つ増える。
- seam S-a〜S-fは本番コードの抽象を増やす(特にS-aの`run_resume_loop`は既に8引数)。
- 忠実度の乖離(GRO/GSO、ECN、MTU、dual-stackのアドレス正規化)はモデル化しない。L2/実機の担当。

## 9. reducer Stepとの関係と順序

- 選択肢5(a)(client側): いつでも(`resume_loop.rs`の`#[cfg(test)]`のみ。ただしStep 5が同ファイルを書き換え始めたら、その前に入れる)。
- 選択肢5(b)(server側): Step 2aのマージ後(`engine/mod.rs`・`attach_runtime.rs`への`pub(crate)`とリテラル抽出、rev2、N3)。
- 選択肢6(合成proptest): 性質(1)(2)はStep 2a・Step 5の後。Step 11の後なら`check_trace`を流用。FCIS ADR rev6の順序
  `0 → 7a → 1 → 1.5 → 2a → 10 → 2b → 2c → 2.5 → 3a → 5 → 6+7 → 4 → 8a′ → 11 → 13`では、5の直後(6+7と並行可、触るファイルが
  テストのみ)に置くのが自然(OPINION)。性質(4)は2b(順序上5より前に済む)+`ResumeRejected`への拒否理由追加(どのStepにも無い。
  2bか2cのPRに含めるか別PRにするかはQ-L1-13)。性質(3)の時間部分はpreempt待ちのEffect化(どのStepにも無い、Q-L1-13)の後。
- L2夜間化(§5-4): FCIS ADRの順序と独立。選択肢5(a)と同時期。
- L1(再開した場合): S-c・S-fは2a/10/2b/2cの後、S-a・S-bはStep 5と同時か後、S-bはStep 12とも`connect.rs`で重なる。
- 本ADRはFCIS ADRの順序を変えない。

## 10. 対象外(non-goals)

- 物理multipath(noq #738)。
- Androidアプリ経路(`isekai-terminal-core`)のL1化。
- `isekai-ssh` wrapper・bootstrap・native/mux holder・プロセス木。
- STUNサーバー、MASQUE relay、Tailscaleのsim化。
- カーネル/キャリア固有の挙動(conntrack、CGNAT、dual-stack正規化、GRO/GSO、MTU)。
- プロセス凍結(sleep/wake)の忠実な再現。
- **決定論性のために本番の`select!`へ`biased;`を足すこと**(公平性/飢餓の挙動変更。必要になったら§6.3のre-decide条件に当たる)。
- **`--cfg tokio_unstable`を既存のrequiredジョブに適用すること**。
- TLS/QUICのバイト列の決定論性(比較しない)。
- シナリオDSL、汎用シミュレーションframework、noq-protoの直接駆動、turmoil/madsimの採用。
- L1・合成proptestをL2・実機smokeの代替とすること。

## 11. 検討した代替案(§4以外)

- **L2夜間化だけ**: 2プロセスの合成も原理的に見えるが、実時間なのでresume window等を縮めた特殊設定でしか回せず、seed再現も無い。
  §5-4のとおりL2夜間化は進めるが、合成proptestの代わりにはならない。
- **`FaultyUdpSocket`の流用**: `isekai-terminal-core`の`pub(crate)`で、実socketを包む設計。手本にするなら`RelayUdpSocket`(§3.4)。
- **noq上流の`Pair`ハーネスのコピー**: 2416行で、isekaiのコードを通らない。
- **「ネットワークawaitには必ずtimeout」のlint**(3205178f型): FCIS ADR Step 7aと同じ仕組みで書ける可能性がある。本ADRでは
  提案に留める(Q-L1-10)。

## 12. リスク

- **R1 合成proptestの「QUIC idle timeout」の近似が実noqと乖離する**: 設定値から導出した時刻(例: `max(idle, 3·PTO)`の扱い)を
  docに明記し、L2夜間化後にL2の実測値と突き合わせる(RES ADR Q8)。
- **R2 Step 5の遅延**: 合成proptestはStep 5のreducerが前提。Step 5が遅れれば合成proptestも遅れる。その間は定数関係テスト+L2で埋める。
- **R3 定数関係テストが「現在の値」を固定しすぎる**: 各関係について、定数のdocに書かれた意図(例: keep-alive = idle/3)があるもの
  だけをassertする(RES ADR §2の「意図として書かれている関係だけを守る」)。
- **R4 L1保留の判断が誤っている**(実noqタイマー由来の2プロセス不具合が本番で出る): §5-3(a)の再評価条件で拾う。L2夜間化が
  最初の検出層になる前提なので、§5-4で追跡事項として登録する(rev2、N5)。**L2夜間化が実施されるまでは、このクラスの検出層は
  実機/本番だけ**で、再評価条件(a)は本番の事故でしか発火しない。加えて、合成proptestの性質(3)の時間部分と(4)は前提作業が
  揃うまで書けない(§5-3の既知の盲点、rev2、N1)。

## 13. Open Questions

| # | 何を決めるか | いつ決めるか | 既定案 |
|---|---|---|---|
| **A** | **§5の推奨(定数関係テストを今、合成proptestを2a・5の後、L1は保留継続)を採るか** | 今 | — |
| Q-L1-1 | 定数関係テストの置き場所 | 選択肢5(a)/(b)のPR | (a)は`resume_loop.rs`の`#[cfg(test)] mod timing_relations`(client側privateの定数が見える)。(b)は2aマージ後に`isekai-pipe`のcrate内`#[cfg(test)]`モジュール(serverの定数を`pub(crate)`化して参照) |
| Q-L1-2 | 合成proptestのclient側にStep 5の`ResumePlanner`だけを使うか、Android側(Step 3aの`ReconnectState`)との合成も書くか | Step 5の後 | `ResumePlanner`のみ。Android側は3aのreducerが`isekai-pipe`のserver集約と同じプロトコル面を持つかを確認してから |
| Q-L1-3 | 合成proptestのEvent列の長さ・時刻の離散化 | 選択肢6のPR | 列長≤N(CI時間数十秒以内に収まる値をPRで実測)、時刻は設定値由来の境界±1msを必ず含むstrategy。FCIS ADR §2.2の必須プロパティに従い、**非単調な`now`列**(後のEventほど小さい値も混ぜる)も生成する(rev2) |
| Q-L1-4 | Android経路(`isekai-terminal-core`)のL1化 | L1再評価時 | しない |
| Q-L1-5 | QUIC層の前提を`cuzic/noq` fork側のテスト(上流`Pair`ハーネス)に足すか | 合成proptestで「QUIC idle timeoutの近似」を決めるとき | 足さない。近似の根拠はnoqのソース引用で書く |
| Q-L1-6 | L1再開時、relay/STUN/cross-familyの切替を扱うか | L1再評価時 | cross-familyのみ |
| Q-L1-7 | turmoilを再評価するか | L1再評価時 | しない(TCP/stdioのseamに寄与しないため、§4.2) |
| Q-L1-8 | warm-standbyの壁時計跳躍検出(`resume_loop.rs:1770-1773`、`tethering_interface`時のみ)をどの層で検証するか | Step 5 | Step 5でこの判定がreducerへ移るならそのproptestで。移らないならL2のSIGSTOP |
| Q-L1-9 | RES ADR Q8(QUIC経路のサイレント遮断の検出時間)をどこで答えるか | L2夜間化時 | L2の実測(rev0の「L1で一次回答」は撤回) |
| Q-L1-10 | 「ネットワークawaitには必ずtimeout」をlint化するか(3205178f型) | FCIS ADR Step 7aの拡張を検討するとき | 本ADRでは決めない。FCIS ADRの未解決事項として提案するかをユーザーが決める |
| Q-L1-11 | L1再評価条件§5-3(a)の「1件」を誰がどう判定するか | 不具合の事後分析時 | 事後分析(ADR/コミット本文)で根本原因が「実noqタイマー意味論×isekai判断」または「client shellの故障つき配線」と書かれた時点で、本ADRを改訂して再評価する |
| Q-L1-12(rev2) | 合成proptestのハーネスがStep 10の差分テストの参照モデル(shell↔reducerの対応)を再利用できる形にするか | Step 10のPR | 再利用できる形にする(翻訳の二重手書きを避ける)。Step 10のPRで共有可能にできなければ、合成proptestはshellの再実装が要らない性質(1)(2)に留める |
| Q-L1-13(rev2) | 性質(3)(4)の前提作業をどのStepに載せるか: (i) `ServeEffect::ResumeRejected`への拒否理由の追加、(ii) preempt待ち(`PREEMPT_WAIT_TIMEOUT`)のtoken付きタイマーEffect化 | Step 2bのPR(i)、Step 2cの後(ii) | (i)は2bのPRに含めることをFCIS ADRの担当に提案する。(ii)は新しい小Stepとして提案するかをユーザーが決める(既定: 提案のみ、実施は性質(3)の必要が具体化してから) |
| Q-L1-14(rev2) | L2夜間化(§5-4)の担当と時期 | 本ADRの採択時 | team-lead経由で割り当て、選択肢5(a)と同時期。3シナリオ(サイレントblackhole、完全切断→resume、旧パスのみblackholeのzombie relay) |
