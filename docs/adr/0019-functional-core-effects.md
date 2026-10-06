# ADR: Functional Core / Imperative Shell と「Effectをデータとして返すreducer」の段階的導入

- **Status**: **Accepted(2026-10-06、ユーザーApprove済み)**。rev4。Approve時のユーザー決定: D1=Step 2bの後で2cを実施、
  D3=純粋性検査ジョブのみrequired check化(mutants週次ジョブは非required)。それ以外の未解決事項は§10の既定案に従う。
  **rev5(2026-10-06、amendment)**: ユーザー承認済みの追加Step 7a・9・10・11・12・13をロードマップに追加(§0 rev5)。
  rev4までの決定は一切変更していない。
  **rev6(2026-10-06、amendment)**: Step 9の不具合履歴報告を根拠に、ユーザーが§6の順序変更を決定(§0 rev6)。
  Step 3aに`pending_wake`を取り込み、Step 5を再評価から昇格、Step 7をStep 6へ統合、Step 4を後ろへ、8bを再評価へ移した。
  (以下は承認前のレビュー経過)敵対的レビューは4ラウンドで収束済み。
  round 1〜4の敵対的レビュー(`scratchpad/opus-review-fcis-adr-round{1,2,3,4}.md`、Opus)の全指摘と、
  それを受けたユーザー決定を反映。round 4の判定は「converged」(残りは軽微6件で、rev4に取り込み済み)。
  ユーザーが決めるべき未解決事項は§10「未解決(ユーザー判断待ち)」に一箇所にまとめてある。
- **対象**:
  - rust-core(`isekai-terminal-core`): `src/{orchestrator,pool,session_state,terminal,rebind_manager,net_health_policy,lib}.rs`
  - `isekai-pipe`: `src/engine/{attach_arbiter,attach_runtime,resume,mod}.rs`、`src/resume_loop.rs`
  - `isekai-ssh`: `src/{wrapper,reconnect_backoff}.rs`、`src/native/{connect.rs,mux/mod.rs}`
  - `isekai-transport`: `src/{path_health_fsm,path_health,candidate_pool,backoff}.rs`
  - Android: `TerminalTabsViewModel.kt`、`session/{TerminalSession,ConnectionStateMapper}.kt`
  - CI: `.github/workflows/{rust-core-test-check,cargo-mutants-check,regenerate-lockfile,regenerate-uniffi-bindings}.yml`、
    `rust-core/.config/nextest.toml`、新設`rust-core/clippy.toml`・`rust-core/pure_modules.toml`
  - rev5追加分: `isekai-pipe/src/connect.rs`(Step 12)、`isekai-pipe-core/src/outcome.rs`(Step 12、読むだけ)、
    Kotlin/Swiftのテスト(`android/src/test/`、`ios/Tests/IsekaiTerminalCoreLogicTests/`、Step 13)、
    `.github/workflows/ios-logic-linux-check.yml`(Step 13のマージ条件)
- **ユーザーの目的**: FCIS・algebraic effects風(Effect enumをshellが解釈)・ASGI風のプロトコル境界・
  Elm architecture・Redux(単一State/Event/純粋reduce)の考え方をisekai-terminal(Android+rust-core)と
  isekai-ssh/isekai-pipeに持ち込み、**CIで決定論的に検証できる範囲を広げる**。
- **拘束される既存ルール/ADR**: `.claude/rules/rust-ssot.md`、`.claude/rules/always-connects.md`、
  `.claude/rules/main-branch-protection.md`(required 5本、特に`lockfile-drift`)、
  `.claude/rules/uniffi-binding-regeneration.md`、`docs/adr/0018-connection-resilience-simulation.md`(L1保留)、
  ローカルbuild/test禁止(検証はGitHub Actionsのみ)。
- **Room**: どのStepも`AppDatabase.kt`/Room migrationに触れない(round 1で確認、§8)。
- **表記**: 「確認済み」はmain `beb74a03`でコードを読んで確認した事実。「推測」は導出のみで未実行。
  「[EXT]」はこのリポジトリ外の一般知識(ツールの仕様等)で、実装時に確認が要るもの。

---

## 0. 改訂履歴

### rev6(2026-10-06)— Step 9の不具合履歴報告による順序変更(ユーザー決定)

Step 9の報告(`scratchpad/defect-history-rank.md`。mainの`fix:`/`revert:`コミット等を領域別に分類し、主要コミットの本文を
手で読んだもの)を根拠に、ユーザーが次の順序変更を決めた。scratchpadはリポジトリ外なので、採用した根拠をここに書き写す
(コミットハッシュは報告が挙げたもの。各コミットが「このStepがあれば防げた」という評価は報告自身がOPINIONと区別している)。

**新しい順序**: `0 → 1 → 1.5 → 2a → 2b → 2c → 2.5 → 3a(+pending_wake) → 5 → 6+7 → 4 → 8a′`、8bは再評価へ。
rev5の追加Stepは依存に従って差し込む(§6の順序行)。

| 変更 | 根拠(報告より) |
|---|---|
| 0〜2cの先頭配置は維持 | 報告の第1位領域(serve engine)。fencing slot⇔SessionTableの不整合という同じ型の不具合が6回以上: 33973aec、2308e7d4、886bbb15、b449d7da、8eb81411、857f6ae6(D-2)。いずれも`always-connects.md`違反で、多くはshell原子性(報告の分類A)のためStep 1.5も重視 |
| **Step 3aに`pending_wake`を取り込む** | eecba351(実機分析由来): `retry_attempt_in_flight`中にwakeの許可が落ち、ネットワーク復帰に数分気づかない。`pending_wake`はこれを受けて追加されたフィールドで、rev4までの再評価表が3b/3cの判断材料に挙げていた`epoch`/`in_flight`/`pending_wake`の三つ組そのもの。tick会計・`woke_early`(3b/3cの残り)は再評価のまま |
| **Step 5(`resume_loop.rs`の`ResumePlanner`)を再評価から昇格し、3aの直後へ** | 判断ロジック(報告の分類D)の修正が約12件。give-up方針が3回直し直された(cc5fb926 → 71292e68 → dbb80d56)、BUSY_OTHER_SESSIONの再試行期限も3回(75d08a39 → fd32ce11 → 3d5e0da5)、ほか03224b11、204d8f59、a266f1f3(一部)、857f6ae6(D-4)。rev4の再評価基準「既存の`start_paused`テストと純粋helperで足りない不具合が出たか」を履歴が既に満たしている |
| **Step 7をStep 6へ統合(6+7)** | a266f1f3(redeploy+retryが1回しか走らない、ユーザー観測)と857f6ae6 D-4(jitter欠落)は、どちらも同じ不具合を重複した2つのループ/2つの`ReconnectBackoff`で別々に直す必要があった |
| **Step 4(`pool.rs`)を6+7の後へ** | `pool.rs`の不具合は履歴上1件だけ(#120、4793ebcd)で、#124(0ac06820)のモデルテストで既に固定済み。タイマー世代競合の事例は無い。2.5 → 4の固定依存は後ろへずらしても満たされる |
| **8a′は維持、8bは再評価へ** | `observeConnectionTransitions`のStateFlow conflationによるエッジ取りこぼしの記録は0件(ADR自身も「実発生は未計測」)。Android側の不具合は配線漏れ(W)とプラットフォーム(P)が主。8a′は`rust-ssot.md`上の理由で残す |
| 新規の未解決事項D5(§10) | 報告の横断的観察: 「callback/フラグを実装したが配線していない」(分類W)が大きく、どのStepも対象にしていない(例: 2a07a5e3、8baa49d8、db3a6d87、ce214ef5、ae8ed13b、119205f6、e1c370e5、7b10472a)。CIでの検出案を**提案**として載せ、決定はユーザーに委ねる |

rev5のD5(「Step 9の報告で順序を変えるか」)はこの決定で解決済みとし、D5の番号は上の新規事項に振り直した。

### rev5(2026-10-06)— Approve後のamendment: 追加Step 7a・9・10・11・12・13

ユーザーが承認した追加Stepを、既存の決定(rev1〜rev4、D1・D3を含む)を変えずにロードマップへ組み込んだ。
rev5で「確認済み」と書いたコード事実はmain `b413c2ac`で読んだもの(rev4までの`beb74a03`との間にこれらのファイルへの
変更は無い)。

| 追加 | 内容 | 依存 |
|---|---|---|
| **Step 7a**(lint) | Effect interpreter関数に`#[deny(clippy::wildcard_enum_match_arm)]`を付け、Effect enumは`match`の明示armでのみ消費する。Effect追加時に全interpreterでの対応がコンパイル時に強制される。`AttachRuntime::activate`(`attach_runtime.rs:210-222`)が`if let AttachEffect::StartRelay`で他のEffectを黙って捨てている箇所(確認済み)が、2aで`Activated`が`Discard{Evicted}`も返すようになると実害になる | Step 0の後 |
| **Step 9**(調査) | 不具合履歴に基づくStep順序の再ランク付け。別エージェントが`scratchpad/defect-history-rank.md`を作成中で、**本ADRはその結果を先取りしない**。結果はStep順序見直しの入力であり、順序の変更はユーザーが決めるADR amendmentとして行う | なし(いつでも) |
| **Step 10**(shell競合の検証層) | 実shell(`start_paused`)と純粋集約モデルに同じ操作列を流す差分テスト(PR #124の`pool.rs`方式)と、`AttachArbiter`+`SessionIndex`集約の有界網羅探索の評価(手書き探索器/stateright/loom)。loomはtokio非同期mutexとparking_lotの使用から集約には適用困難と評価 | Step 2aの後 |
| **Step 11**(Effect/callback列の不変条件検査) | e2e/orchestratorテストで記録したEffect・callback列に不変条件(例: `Established(g)`の後に`Lost(g)`が正確に1回)を課す。秘密を載せない(§3-3)。Q12(実機記録+replay)とは別物で、Q12の既定案「実施しない」は変えない | Step 3a・8a′の後 |
| **Step 12**(always-connects網羅性) | `isekai-pipe`の`run_connect`の全`Err`経路→`ConnectOutcomeClass`の分類と、`decide_connect_failure_recovery`の全クラス×入力の網羅表テスト | 独立 |
| **Step 13**(Android/iOS callback契約golden) | Rust(8a′の`on_connection_edge`等)から生成したgolden callback列をKotlin JVMテストとSwiftテストでreplayする。`ios-*`はrequiredでないため、該当PRでは`ios-logic-linux-check`の緑をマージ条件にする | Step 8a′の後 |

§6の順序行・固定依存・並行可の記述、§7(CI)、§10(新規の未解決事項Q15〜Q18・D5)を更新した。
§3に規則8(interpreterのEffect消費)を追加した(Step 7aの規則化。既存規則1〜7は不変)。

### rev4(2026-10-06)— round 4レビュー(収束判定)の軽微指摘の取り込み

round 4は「converged、新規BLOCKER/MAJORなし」。残りの軽微6件を以下のとおり取り込んだ(追加レビューラウンドは行わない)。

| 指摘 | 対応 |
|---|---|
| **m-R4-1**(必須) RESUMEがper-sessionの`Arc<Mutex<Session>>`(出力バッファ)を`sessions.get(&session_id)`(`mod.rs:1375`)で別途引くと、R3-2で除いた二段階パターンがバッファ側に残る(incarnation 2のソケットとincarnation 1のreplayバッファの組み合わせ) | `SessionIo`(集約内)が`Arc<Mutex<Session>>`も保持し、`ResumeGranted`は`(lease, parked_tcp, handle)`を一緒に返す。`Activated`が同じapplyでhandleを登録する。idでhandleを引く経路は残さない(Step 2a)。§2.2の要求の規則に「返すものはトークン・資源・ハンドルの全部」と明記 |
| m-R4-2 `RequestPreempt`→`reparked`待ち→再送の間の取りこぼし(`notify_waiters`は既存の`Notified`しか起こさない [EXT])。現状コードにも同じ窓がある | preempt後は、起床でもタイムアウト(`PREEMPT_WAIT_TIMEOUT` = 2秒、`mod.rs:79`)でも**必ず1回`ResumeRequested`を再送する**。要求は原子的なので安全で、取りこぼしは拒否ではなく最大2秒の遅延になる。Q9に統合 |
| m-R4-3 `path_health_fsm.rs`は`noq::PathStats`をインラインで使う(`:36,45,82,99`、確認済み)。`noq`は外部crate許可リストに無い | Step 0で`noq::PathStats`を項目単位の外部許可エントリとして登録(Q10と一緒に決める) |
| m-R4-4 `OrchestratorAdapter::new`と`connect_via`のEffectの受け渡し。`connect_via`は現状`Connecting`を公開しない(`orchestrator.rs:867-871`、確認済み) | 両者とも`SessionCreated`/phase遷移のapplyが返したEffectを**呼び出し元へ返し**、呼び出し元がロック解放後に公開する(§2.4-2)。8a′の「順序」の誤った主張を訂正(`connect_via`経路では`Lost(old)`が単独で出る。`uiState`は新セッションの報告までConnectedのままで、これは既存の挙動) |
| m-R4-5 テスト(e)が前提条件無しだとIdle→Connectingの場合(`Lost`無し)を検証して空振りしうる | テスト(e)のセットアップ手順を明記(Connected → `EnteredBackground` → `BackgroundBudgetExpired`/`MemoryWarning`でSuspended → Connectedのまま`WillEnterForeground`) |
| m-R4-6 §2.2/Step 2aの文言の残り | §2.2の要求の規則に返却物を全列挙。Step 2aの棚卸し表のunpark行をhandle込みに更新 |
| (Status) | 「Draft(4ラウンドで収束、Approve待ち)」に変更。未解決事項を§10の一箇所にまとめた |

### rev3(2026-10-06)— round 3レビューの反映

| 指摘 | 対応 |
|---|---|
| **R3-1** `apply_network_lost`(`orchestrator.rs:717-722`)はアダプタも世代も経由せず`handle_unexpected_disconnect`を直接呼ぶので、rev2のedge reducerでは`Lost`が遅れるか、最悪出ない | Step 8a′: `Lost(g)`は特定のEventではなく**reducerのphase遷移**で定義する(「`edge_open == Some(g)`のまま`phase`をConnectedから他へ動かすapplyは、同じapplyで`Lost(g)`を出す」)。`apply_network_lost`は現行の`session_generation`を付けた`AttemptDisconnected{kind: NetworkLost}`として同じ遷移を通す。8a′では**phaseを書く全経路**をapply経由にする(3aの1経路だけでは足りない)。退出経路ごとのorchestratorテスト6本を追加 |
| **R3-2** RESUMEのunparkはleaseを知る前に行われる(`mod.rs:1375-1386`→`:1431`)ので`Unparked{id, lease}`を運べず、素直に実装すると2aが閉じるTOCTOUが戻る | RESUMEは**要求**`ResumeRequested{id, now}`として1回のapplyで解決し、reducerが`(lease, parked_tcp)`を返す。§2.2/§3に「事実は世代トークンを運ぶ。クライアントが選んだidで引く要求は1回のapplyで解決し、トークンはreducerが返す」を追加。`:1431-1435`のslot無しrepark分岐は**削除**する(2aのPRで意図した挙動変更として明記、I-bの下で到達不能)。不変条件I-iを追加 |
| m-R3-1 `theme.rs`はグローバル可変状態(`static THEME: LazyLock<RwLock<Theme>>`、`theme.rs:1-2,44`)を持つ。clippyの禁止型に`RwLock`等が無い | Step 0の前提条件を訂正(`crate::theme::Theme`を項目単位で許可、モジュールは不可)。`disallowed_types`に`parking_lot::RwLock`・`std::sync::LazyLock`・`std::sync::OnceLock`・`std::thread::LocalKey`・atomic型(具体型を列挙)を追加。allowlistスクリプトは純粋モジュール内の内部可変`static`を拒否。`ai_panel.rs:26`のcrate root項目も前提条件に追加 |
| m-R3-2 同一世代の`AttemptConnected`重複で`Established`を再送しうる。`SessionCreated`を「直後」に送ると窓ができる | `edge_open == Some(generation)`の`AttemptConnected`は何もしない(冪等)と明記。`SessionCreated`は`OrchestratorAdapter::new`の`session_generation += 1`と同じ`state.lock()`臨界区間内でapplyする |
| m-R3-3 unresumableエントリの容量計数が曖昧。`Activated`の書き方が「満杯なら即unresumable」と読める | 2つの計数を区別して明記(table側`insert_existing`の計数からは除外、arbiter側`admit_new_session`の`session_count()`には含む。どちらも現状どおり)。`Activated`は「満杯**かつ立ち退けるparkedが無い**ならunresumable」に修正 |
| m-R3-4 arbiterの`RelayEnded`と`RelayTerminated`が2回のapplyになる | 集約レベルでは`RelayEnded{lease}`(`EstablishedLease::release`/`drop`由来)を`Discard{id, lease, TcpDied}`と**1つの遷移**として扱い、続く`RelayTerminated`は冪等なno-opとする |
| m-R3-5 8bと8a′はどちらも`TerminalSession.kt`を変える | 順序を**8a′ → 8b**に固定。並行可はStep 6だけ |
| m-R3-6 `DisconnectKind::classify`が`NETWORK_LOST_REASON`(`orchestrator.rs:715`)に依存 | 定数も`DisconnectKind`と一緒に`reconnect_fsm.rs`へ移す。R3-1の修正後は`kind`を分類済みで渡すので、文字列分類の依存も弱まる |
| m-R3-7 `Millis`は`isekai-protocol`(wire型のcrate)に置くが、shellエポック相対なのでプロセス間で無意味 | docに「wireに載せない、異なるshellの値を比較しない。serdeはreplay専用」と明記 |
| m-R3-8 allowlistスクリプトの死角 | 入れ子のグループimport(`use crate::{a::{b, c}, D}`)と`use crate::*`のglobを追加。未登録モジュールからのglobはスクリプトが拒否する |
| (round 3推奨、軽微) `apply_with`のクロージャ | 型を`FnOnce(Millis) -> Event`(非async)に固定し、クロージャ内でI/Oができないようにする |

### rev2(2026-10-06)— round 2レビューの反映

round 2の判定は「blockerなし、新規MAJOR 5件」。round 1指摘の意図はすべて満たされていると確認された。
**ユーザー決定(N-3、確定)**: 選択肢(a)。Step 2aで`Rejected`由来のリークを**明示的に修正**する
(unresumableなエントリの`Parked`は`Discard{cause: Unresumable}`)。TOCTOU修正と同じく、2aのPRで
意図した挙動変更として明記する。

| 指摘 | 対応 |
|---|---|
| **N-1** `now`をロック外で刻印すると順序が逆転し、`u64`の引き算がwrapして直前にparkしたsessionが期限切れになる | §2.2: 刻印は**臨界区間内**(`apply_with(\|now\| Event)`)、`Millis`の差は`saturating_sub`/`checked_sub`のみ、**非単調な`now`のproptestを必須化**。Step 2aもこの形に |
| **N-2** `RelayTerminated{id}`/`Discard{id}`はid再利用に対しABA非安全で、2aではfencing slotまで解放してしまう | 既存sessionに関する事実Event(`Parked`/`Unparked`/`RelayTerminated`)はすべて`lease`を運び、`IndexEntry`も`lease`を持つ。非現行leaseのEventは無視。§2.2/§3に「遅れて届く事実すべてにstale-guard」を一般原則として明記。I-fを追加 |
| **N-3** 2aで`Rejected`を忠実に保存すると、恒久的なfencing slotリークを保存してしまう(`mod.rs:1673-1695`、テーブル外にpark) | ユーザー決定(a): 2aで`Parked`(unresumable)→`Discard{Unresumable}`。I-g「unresumableなエントリはparked-and-Establishedに到達しない」を追加。I-cとStep 2cの範囲を更新 |
| **N-4** 8a′の`Lost`がConnected→手動`connect_*`の経路で発火せず、Kotlin側の資源がリークする | `Lost`を「`Established(g)`を出した全世代について、Connectedを離れる最初の遷移で正確に1回」と定義。発火点は「現行世代の予期しない切断」と「edgeが開いたまま新しいセッション(アダプタ)を作るあらゆる経路」(`begin_connect`、フォアグラウンド復帰の`connect_via`も)。旧世代はreducerの`edge_open`が保持するので、アダプタ生成で世代が進む前の捕捉を構造的に満たす。不変条件「`Established(g)`の後、`Established(g'>g)`より前に`Lost(g)`が正確に1回」を追加。Q11を回答 |
| **N-5** import allowlistがモジュール単位で、crate root(`RUNTIME`と公開値型が同居)を表現できない。3aの「`OrchestratorState`内」は登録不能 | §2.3を**項目単位のallowlist**に。3aは別ファイル`src/reconnect_fsm.rs`(登録)に`ReconnectState`と移設した`ConnPhase`/`BackgroundState`/`DisconnectKind`を置き、`OrchestratorState`がフィールドとして持つ。`session_state.rs`登録の前提条件を列挙 |
| m-R2-1 §2.4-4の連番規則に担当Stepが無い | 8a′は`Established`/`Lost`を`Connected`/`Disconnected`公開と**同じ呼び出し箇所から固定順で**出す方式にし、汎用の連番publisherは導入しない。§2.4-4は「将来の規則」と位置付け |
| m-R2-2 Step 6は`Cargo.lock`を変える | Step 6に§7の最小lockfile手順を明記。最小モードの候補に`cargo update --workspace`を追加 |
| m-R2-3 8a′の実装者一覧が不完全、iOSはrequiredでない | 実装・偽実装の全ファイル(Kotlin/Swift/Rustテスト)を列挙し、PRで`ios-logic-linux-check`の緑を必須化 |
| m-R2-4 `timed_fsm::clock`の本番Clockが`Instant::now()`を読む | 実際の型名は`timed_fsm::clock::MonotonicClock`(レビューの`SystemClock`は誤記、registryで確認)。`disallowed_types`に追加し、allowlistからも`timed_fsm::clock`を除外 |
| m-R2-5 clippyの未解決パス警告、allowlistスクリプトの死角 | Step 0のPRで確認する事項として明記。スクリプトの既知の死角を列挙 |
| m-R2-6 3aの`AttemptRef`とissue hintの入力 | `AttemptRef`は`begin_connect`だけが進める単調ID。`AttemptDisconnected`にshellが事前計算した`targets_local_network: bool`を載せる |
| m-R2-7 2.5と4はどちらも`pool::release`を変える。8a′は3aの後 | 順序を2.5 → 4に固定。「並行可」の記述を修正し、8a′は3aの後と明記 |
| m-R2-8 `Millis`の置き場所が未定 | `isekai-protocol`(core/pipe/ssh/transportのすべてから届く既存の純粋crate)に置く |
| m-R2-9 Step 1.5の範囲 | `AttachRuntime`(ローカル`TcpListener`をtargetにする)を含め、`release_slot_for`による実中継中のslot解放まで観測する形に |

### rev1(2026-10-06)— round 1レビュー + ユーザー決定の反映

**ユーザー決定(確定、再議論しない)**:
1. 純粋性の検査は grep を廃し、**clippy `disallowed_methods`/`disallowed_types`(モジュール単位deny)+
   import allowlist** の2層にする。通らないモジュールは外すか先に直す(`session_state.rs`は
   `terminal.rs:1113`の`now`をパラメータ化するまで外す)。
2. Kotlin: Step 8a(`observeConnectionTransitions`のKotlin reducer化)を**撤回**し、Rustが明示的な
   接続確立/喪失イベント(generation付きcallback)を出し、Kotlinは転送するだけにする。8b(UI専用reducer)は残す。
3. ロードマップ順: Step 0(修正版)→ 1 → **新L0特性テスト(sweep×RESUME競合)** → 2(**単一ロック下の単一集約**
   として再定義)→ **新Step: `cfg(test)` RUNTIME注入** → **3aのみ** → 3b/3c/5/7は「証拠を見て再評価」。
   Step 4・6は残す。
4. タイマーの標準形は**「stale-guardトークン付きEffect → `TimerFired{token}` Event」**。`timed_fsm`は既存4利用者の
   legacy形として容認し拡張しない。`now`はreducerローカルの`Millis(u64)`。

| 指摘 | 対応 |
|---|---|
| **B-1** 合成不変条件「Established ⊆ index」が現行設計で偽(`Rejected`、activate→insert間の窓) | §4.1・Step 2を全面改稿。不変条件を「成り立つべき状態」に限定し、`Rejected`は明示的な状態として模型化。`Rejected`解消・admit原子化は**別の挙動変更PR(2b/2c)**として明示ターゲット化 |
| **B-2** Step 2が原子性・ロック順を規定していない。sweep×RESUMEのTOCTOU、ABBA | Step 2を「単一ロック下の単一集約」として再定義し、ロック順・全park/unpark/removal箇所の棚卸し(§6 Step 2表)を明記。前段に**L0特性テスト(Step 1.5)**を追加 |
| **M-1** `Removed{id}`がDiscard単一interpreterを迂回、removal経路の列挙漏れ | removalはreducer起点のみ。shellは`RelayTerminated{..}`等の事実を送り、reducerが`Discard`を返す。interpreterは冪等。`SessionTable::remove`は非公開化 |
| **M-2** grepは偽陽性3/7・推移的不純を見逃す・迂回容易 | grep廃止。§2.3をclippy+import allowlistに置換。`session_state.rs`はStep 0から除外(Q8回答) |
| **M-3** タイマー作法が実質3〜4種 | §2.2で標準形を1つに確定。`TimedStateMachine`はlegacy容認。stale tokenプロパティを必須化。Q2は「拡張しない」で閉じる |
| **M-4** `std::time::Instant`はreplay/proptestと非整合、shell間でreal/virtualが混ざる | `Millis(u64)`に確定、shellごとに1箇所の刻印関数で`tokio::time::Instant`から変換(§2.2)。Q1を閉じる |
| **M-5** Step 3のEvent一覧が書き手~8箇所を網羅していない、`session_generation`と`epoch`を混同 | Step 3aで集約の全フィールドと**全エントリポイント→Event対応表**を定義。`session_generation`は`epoch`と別フィールドのまま |
| **M-6** effect実行と再入の規則が無い | §2.4に「ロック下でapply・解放後にinterpret」「解釈中に生じたEventは新規applyで戻す(ネスト禁止)」「状態公開は連番付きで直列化」を追加 |
| **M-7** 8aはKotlin側に接続判断reducerを作る=`rust-ssot.md`違反。conflationは実在 | 8a撤回。Rust側の明示callback(Step 8a′)に置換。UniFFI再生成とSwiftの`.sha256`3本に言及 |
| **M-8** mutants週次の「安い」根拠が誤り、`schedule`で`inputs`が空 | §7で別workflow・`pure_modules.toml`由来のmatrix・テストフィルタ付きに再設計 |
| **M-9** replay計画が秘密情報を漏らしうる(`SshAuth`が`Debug`) | §3に「Event/Stateに秘密を載せない、`AttemptRef`で参照」を規則化 |
| **M-10** Step 3の「置換」と「残す」が矛盾 | 実時間テスト14本の決定論化は新Step 2.5(RUNTIME注入+`start_paused`)の役割とし、Step 3の主張から外した |
| **m-1** Step 1の不変条件3つの誤り | Step 1で書き直し(stale leaseの`RelayEnded`、private `LeaseId`、`ClosingForSupersede`の言い方) |
| **m-2** `HashMap`の反復順で非決定 | §3に決定論的コンテナ規則。`SessionIndex`は`BTreeMap<[u8;16], _>`+明示タイブレーク |
| **m-3** proptestはrand 0.9、backoffはrand 0.8 | §7: `u64` seedから`StdRng`(0.8)を作る方式。dev-depは`default-features = false, features = ["std"]`。`Cargo.lock`はCI生成 |
| **m-4** `RECONNECT_STABLE_THRESHOLD` 200s/60s差 | Step 6で「この差は変えない、修正は別PR」と明記 |
| **m-5** CI専用下では`proptest-regressions`をコミットできない | §7: 失敗時artifact upload + ログのseedから`cc`行を手で起こす |
| **m-6** Step 7は誇張(`RedeployGate`は既に共有) | Step 7を縮小し「再評価」扱いへ |
| **m-7** Q6は非問題、Q3は回答可能 | Q6削除、Q3は事実で回答し§4.1に移した |
| §4「欠落」1〜8 | 1=§2.4/Step 2、2=§3、3=§3、4=§2.2、5=Step 2表、6=§10、7=§7、8=Step 2b/2c |

### rev0(2026-10-06)

初版。

---

## 1. 背景・問題

### 1.1 既に「純粋reducer + shell」になっている箇所(確認済み)

| 形 | 実例 | 備考 |
|---|---|---|
| untimed reducer | `isekai-pipe/src/engine/attach_arbiter.rs:208 AttachArbiter::apply(&mut self, AttachEvent) -> Vec<AttachEffect>` | 「never touches a socket, a clock, or an RNG」。RNG由来の`attach_token`はexecutor(`attach_runtime.rs`の`start_connect`、`OsRng`)が生成してEventに載せる。**タイマーも既にEffect化**: `SchedulePendingTimeout{lease}`→`PendingExpired{lease}`、stale判定はlease同一性(`:376-383`) |
| timed reducer(`timed_fsm::TimedStateMachine`) | `src/rebind_manager.rs:324`、`src/trzsz.rs:476`、`isekai-transport/src/path_health_fsm.rs:145`(+`session_state.rs`が`TrzszTransferFsm`を内包) | timerの取消はdriverが持つ |
| 純粋判断関数 | `net_health_policy.rs`(`Decision::NotifyAfterDebounce(Duration)`をshellがepochでgate)、`background_reliability_policy.rs`、`wrapper.rs:1007 decide_connect_failure_recovery`、`resume_loop.rs`の`reconnect_notify_due(disconnected_at, now)`等 | |
| reducer相当の中間形 | `src/session_state.rs`(`SideEffect` enum + `ProcessResult`、proptestあり) | ただし`Terminal::dispatch_apc`が`std::time::Instant::now()`を読む(`terminal.rs:1113`)ため**推移的に不純**(確認済み) |

`AttachRuntime`(`attach_runtime.rs`)は既に「`self.arbiter.lock().await.apply(..)`で一時ガードを
文末で解放 → `execute_effects`をロック外で実行」という形を取っている(確認済み)。§2.4の規則はこれを明文化したもの。

### 1.2 まだ判断とI/Oが混ざっている箇所(確認済み)

| 箇所 | 混在の内容 |
|---|---|
| `isekai-pipe/src/engine/resume.rs` `SessionTable` | `Session`がソケット半分(`parked_tcp`)・`Notify`とメタデータ(`parked_since`/`negotiated_resume_grace_secs`)を同居。`sweep_expired_parked`(`:279`)は(1)テーブルロック下で期限切れIDを集め、(2)**ロックを解放してから**`parked_since`を再確認せずに`self.remove`する。`find_oldest_parked`(`:118`)は等しい`parked_since`を`HashMap`反復順で決める |
| `isekai-pipe/src/engine/mod.rs` | park 3箇所(`:1525-1526`, `:1681-1682`, `:1706-1707`)とunpark 2箇所(`:1385-1386`, `:1409-1410`)がper-sessionロック下で`parked_tcp`/`parked_since`を直接書く。`admit_new_session`(`:1068`付近)は`session_count() < max_sessions()`を見てからロックを解放し、別ロックで`claim_oldest_parked`/`hello()`を行う(check-then-act)。`release_slot_for`は`established_lease_for`と`relay_ended`で2回別々にarbiterロックを取る |
| `src/orchestrator.rs`(4310行) | `state: parking_lot::Mutex<OrchestratorState>`(`:421`、非再入)。再接続関連フィールドの書き手が約8関数に分散(§6 Step 3a表)。`handle_unexpected_disconnect`(`:767`)はローカル`enum Action`で判断と実行を分けているが、`spawn_reconnect_loop`(`:963`、`RUNTIME.spawn` `:969`)は判断と待機が絡む。テストは`std::thread::sleep`14箇所 |
| `src/pool.rs` `release`(`:175`) | refcount判断の後に`crate::RUNTIME.spawn`+`sleep(idle_grace)`(`:189`)。PR #124のモデルテストはタイマーをモデル外にしている(`NEVER_EXPIRES = 3600s`) |
| `isekai-pipe/src/resume_loop.rs` | `Instant::now`はファイル全体で22箇所、**本番コード(`:2110`の最初の`#[cfg(test)]`より前)では13箇所**。既に`start_paused`テスト9箇所と多数の純粋helperがある |
| `isekai-ssh`回復経路 | `RedeployGate`と`reset_budget_if_stable`は**既に共有**(`reconnect_backoff.rs:111-128`のdocが明記)。重複しているのはループ本体2つ(`wrapper.rs::run_ssh_with_connect_failure_recovery`、`native/connect.rs::drive_connect_recovery`)と`MAX_LIGHTWEIGHT_RETRIES`(`wrapper.rs:594`, `native/connect.rs:382`) |
| backoff | `isekai-transport/src/backoff.rs` `BackoffPolicy`(RNG注入、seed付きテスト既存)と、`isekai-ssh`の`ReconnectBackoff`2コピー(`reconnect_backoff.rs:31`、`native/mux/mod.rs:131`、どちらも`thread_rng()`直読み) |

### 1.3 問題の本質

`docs/adr/0018-connection-resilience-simulation.md` §1の結論(不変条件を誰も書いていなかった)をもう一歩進めると、
**不変条件が書けない主因は、判断が時計・RNG・ソケット・spawn・ロックと同じ関数にいること**。
加えてround 1で、**判断の純粋化だけでは解けない「shellの原子性」問題**(sweep×RESUMEのTOCTOU、
admitのcheck-then-act)が実在することが分かった(§6 Step 1.5/2)。本ADRは両方を扱う:
判断はreducerへ、原子性は「集約ごとに1ロック、apply+in-lock effectsを同一臨界区間で」へ。

---

## 2. 決定

### 2.1 reducerの標準形(1つ)

```rust
pub struct XxxState { /* 所有データのみ。ソケット・Notify・JoinHandle・Arc・ロックを持たない */ }
pub enum XxxEvent { /* 起きた事実。I/O結果・RNG値・now・timerトークンもここに載る */ }
pub enum XxxEffect { /* shellにやってほしいこと。タイマー設定もここ */ }
impl XxxState {
    pub fn apply(&mut self, ev: XxxEvent) -> Vec<XxxEffect>;
}
```

- `&mut self`を採り、Reduxの値渡し`(State, Event) -> (State, Vec<Effect>)`にはしない。Rustでは`&mut`は
  外部から観測可能なaliasを持たず値渡しと等価で、既存の`AttachArbiter`とも一致する。テストで遷移前が要れば
  `Clone`してテスト側で比較する。
- **`timed_fsm::TimedStateMachine`はlegacy形として既存4利用者(`rebind_manager`/`trzsz`/`path_health_fsm`/
  `session_state`経由のtrzsz)に限り容認**し、新規reducerでは使わない。`timed-fsm`は拡張しない(Q2回答)。

### 2.2 時間とタイマー(1つに確定)

**reducerは時計を読まない。** 時間は次の2経路でのみ入る:

1. **タイマー**: reducerは`XxxEffect::ArmTimer { token, after: Duration }`を返し、shellは満了時に
   `XxxEvent::TimerFired { token }`を戻す。`token`は**stale-guard**(lease/generation/epochのいずれか、
   reducerが発行し現在値を保持する値)。reducerは「現在のtokenと一致しない`TimerFired`」を無視する。
   取消Effectは任意(取消し損ねてもtokenで無害化される設計にする)。既存の`AttachArbiter`
   (`SchedulePendingTimeout{lease}`→`PendingExpired{lease}`)と、Step 4の`pool`がこの形。
   - **必須プロパティ**: タイマーを持つ全reducerに「非現行tokenの`TimerFired`はStateを変えず、Effectも返さない」
     のproptestを付ける。
2. **時刻比較**: shellがEventに`now: Millis`を刻印する(例: `Parked { id, lease, now }`、`Sweep { now }`)。

**遅れて届く事実すべてにstale-guardを付ける(round 2 N-2)**: タイマーに限らず、既存エンティティに関する
「事実」Event(終了・park・unpark・I/O完了等)は、それが指すエンティティの**世代(incarnation)トークン**
(lease/generation/epoch)を運ぶ。reducerは現行の世代と一致しないEventを無視する。idだけで指すEventは、
クライアントがidを再利用する場合(`isekai-pipe`のsession_idは`ATTACH_HELLO`でクライアントが決める、
`mod.rs:1293-1296`)にABA問題を起こす。`AttachArbiter`の`LeaseId`(単調に発行され全session間で一意、
`attach_arbiter.rs:47-49`)が手本。

**事実と要求を区別する(round 3 R3-2)**: 上の規則は「既に世代を知っている側から届く**事実**」に適用する。
一方、クライアントが選んだidでエンティティを引く**要求**(RESUME等)は、送り手が世代をまだ知らない。
要求は**1回のapplyで解決**し、reducerがその要求の続きに必要なもの**すべて**を同じapplyで返す:
現行の世代トークン、in-lock effectで引き渡す資源(ソケット等)、そのエンティティに紐づく共有ハンドル(出力バッファ等)。
どれか1つでも別途idで引き直すと、そこに同じ二段階の窓が残る(round 4 m-R4-1)。
以後その経路で生じる事実は、そのとき返されたトークンを運ぶ。「先にクエリでトークンを取り、別のapplyで
事実を送る」二段階は、間にSweep/Discardが割り込めるcheck-then-actなので禁止する。

**`now`の刻印は臨界区間内で行う(round 2 N-1)**: ロック外で`now`を刻んでからロックを待つと、別タスクが
後から刻んだより新しい`now`のEventが先にapplyされ、reducerから見て時刻が逆行しうる。そこでshell APIは
`aggregate.apply_with(|now| XxxEvent::Sweep { now, .. })`の形にし、**ロック取得後に**刻印関数を呼ぶ。
これで同一集約に入る`now`は単調になる。クロージャの型は`FnOnce(Millis) -> XxxEvent`(非async)に固定し、
クロージャ内でI/Oや`await`ができないようにする(Eventを組み立てるだけで、ロック保持時間を伸ばさない)。
加えてreducer側も時刻逆行を前提に防御する:
- `Millis`同士の差は`saturating_sub`(または`checked_sub`→`None`なら「未経過」扱い)でのみ計算する。
  素の`-`は禁止(releaseビルドではoverflow checkが既定で無効なのでwrapする [EXT])。
- **必須プロパティ**: `now`を持つ全reducerに、**非単調な`now`列**(後のEventほど小さい値も混ぜる)を
  生成するproptestを付け、「時刻が逆行してもDiscard/期限切れ判定が誤発火しない」をassertする
  (単調な`now`だけを生成するstrategyではこの種のバグは見つからない)。

`Millis`の型(Q1回答)と置き場所(round 2 m-R2-8):

```rust
// isekai-protocol/src/millis.rs(新設)
/// shell が持つエポック(プロセス/shell起動時の tokio::time::Instant)からの経過ミリ秒。
/// **wire(プロトコルフレーム)に載せないこと。異なるshell/プロセスの値同士を比較しないこと**
/// (エポックがshellごとに異なるので無意味)。serde導出は §2.6 の replay 記録のためだけにある。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct Millis(pub u64);
impl Millis {
    pub fn saturating_sub(self, earlier: Millis) -> std::time::Duration { /* 逆行時は0 */ }
}
```

- 置き場所は`isekai-protocol`: `isekai-terminal-core`・`isekai-pipe`・`isekai-ssh`・`isekai-transport`の
  すべてが既に依存している純粋crate(Cargo.tomlで確認済み、`serde`も既存依存)。新しいleaf crateは作らない。
  `isekai-protocol`は本来wire型のcrateなので、上のdocの「wireに載せない」注記を必須とする(round 3 m-R3-7)。
- 理由: シリアライズ可能(§2.6 replay)、proptestで直接生成可能、`timed_fsm::clock::Clock::now_ms()`と同じ単位。
  `std::time::Instant`はプロセス外で再構成できず、proptestでは`base + Duration`でしか作れない。
- **shellごとに刻印関数を1つだけ置き**(例: `fn stamp(&self) -> Millis`、`apply_with`の内部からだけ呼ぶ)、
  `tokio::time::Instant::now()`から変換する。`tokio::time::Instant`は`start_paused`下で仮想時刻に追従するので、
  shellの配線テストとreducerの時刻が食い違わない。同じshell内で`std::time::Instant::now()`と混在させない
  (round 1 M-4)。[EXT] tokio pause下の挙動は実装PRで確認する。
- `candidate_pool.rs`の`Clock` trait注入は撤去しないが、新規コードの手本にしない(隠れ入力でreplay不能)。

### 2.3 置き場所と純粋性の機械検査

**置き場所**: reducerは駆動するshellと同じcrateの別モジュールに置く(`attach_arbiter.rs`↔`attach_runtime.rs`)。
reducer専用crateは作らない(`isekai-bootstrap-plan`が示すとおり、crateにしても依存グラフ上の純粋性は
保たれず、型移設コストが大きい)。命名: 状態を持つreducerは`*_fsm.rs`、状態を持たない判断は`*_policy.rs`、
shellは`*_driver.rs`/`*_runtime.rs`。既存ファイルは改名しない。

**純粋性検査(2層、grepは使わない)**:

1. **clippy `disallowed_methods`/`disallowed_types`**(直接呼び出しの検出。パス解決されるので
   `use std::time::Instant as I`等の別名でも捕まる [EXT])
   - 新設`rust-core/clippy.toml`(workspace共通)に列挙する。初期案:
     - methods: `std::time::Instant::now`、`std::time::Instant::elapsed`、`std::time::SystemTime::now`、
       `tokio::time::Instant::now`、`tokio::time::sleep`、`tokio::spawn`、`tokio::task::spawn_blocking`、
       `std::thread::spawn`、`std::thread::sleep`、`rand::thread_rng`、`rand::random`、`std::env::var`、
       `std::process::Command::new`
     - types: `std::sync::Mutex`、`std::sync::RwLock`、`parking_lot::Mutex`、`tokio::sync::Mutex`、
       `tokio::sync::Notify`、`std::fs::File`、`std::net::UdpSocket`、`std::net::TcpStream`、
       `tokio::net::TcpStream`、`rand::rngs::OsRng`、`rand::rngs::ThreadRng`、`std::time::Instant`、
       `tokio::time::Instant`、`timed_fsm::clock::MonotonicClock`(`now_ms()`が内部で`Instant::now()`を読む。
       registry `timed-fsm-0.4.0/src/clock.rs`で確認済み。round 2で`SystemClock`と書かれていたが実名はこちら)、
       **グローバル可変状態の型**(round 3 m-R3-1): `parking_lot::RwLock`、`std::sync::LazyLock`、`std::sync::OnceLock`、
       `std::thread::LocalKey`(`thread_local!`)、`std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize}`
       (clippyの`disallowed-types`はワイルドカードを取れない前提で具体型を列挙する [EXT])。
       実例: `theme.rs:44`の`static THEME: LazyLock<RwLock<Theme>>`は、純粋に見える関数にプロセス全体の
       隠れた入力を持ち込む(確認済み)
   - 各純粋モジュールの先頭に`#![deny(clippy::disallowed_methods, clippy::disallowed_types)]`を置き、
     そのモジュールの`#[cfg(test)] mod tests`には`#[allow(..)]`を付ける(テストは時計を使ってよい)。
   - CIジョブは`cargo clippy --workspace --lib --bins -- -A clippy::all`。ソース内の`deny`はコマンドラインの
     `-A`より優先されるため [EXT]、既存の警告負債を一切踏まずに純粋モジュールだけが検査される。
     このリポジトリにclippyジョブは現状無い(確認済み)。`isekai-terminal-core`はmusl埋め込み
     (`include_bytes!`)のため、テストジョブと同じく`build-isekai-pipe-musl.sh`を先に実行する。
   - 検査できない範囲: 別モジュールの不純な関数を呼ぶこと(→2層目で捕まえる)、マクロ展開内の一部 [EXT]。
   - **Step 0のPRで確認すること(round 2 m-R2-5)**: 最近のclippyは、`clippy.toml`の`disallowed-*`に書いた
     パスが検査対象crateで解決できない場合(`parking_lot`/`rand`/`tokio`を持たないcrateが多い)に設定レベルの
     警告を出す。これはlintではないので`-A clippy::all`で消えない可能性がある [EXT]。エントリごとの
     `allow-invalid`等で抑止できるかを確認する。
2. **import allowlist**(推移的不純の検出)。**項目単位**で判定する(round 2 N-5)
   - 新設`rust-core/pure_modules.toml`に純粋モジュールを登録し、各モジュールについて
     `use crate::..`/`super::..`/インラインの`crate::..`パスを検査する(CIのPythonスクリプト、コメント除去+
     `#[cfg(test)]`付きアイテムの括弧対応除去)。許可の単位は2種類:
     - **モジュール単位**: 参照先モジュール全体が`pure_modules.toml`に登録済みなら、その中のどの項目も可。
     - **項目単位**: crate root(`lib.rs`)や不純な項目と値型が同居するモジュールについては、**許可する項目を
       名指しで列挙**する。例: `crate::ConnectionIssueHint`(`lib.rs:1398`)・`crate::ConnectionPublicState`(`lib.rs:1406`)は可、
       同じcrate rootの`crate::RUNTIME`(`lib.rs:64`)・`crate::SshConfig`/`crate::SshAuth`(`lib.rs:712,792`)は不可。
       `crate`(root)をモジュール単位で許可することは**禁止**(`RUNTIME`と秘密を含む型が通ってしまい、
       §3-3が無意味になる)。
     - 公開値型が増えて項目列挙が煩雑になったら、`src/public_state.rs`のような値型専用モジュールへ移して
       モジュール単位で登録する方を選ぶ(移設はUniFFIの型名を変えないので再生成は不要の見込み、推測)。
   - 外部crateの許可リスト: `std::time::Duration`、`std::collections`、`core`、`isekai_protocol`、`base64`、
     `serde`、`timed_fsm`(ただし`timed_fsm::tokio_support`と`timed_fsm::clock`は不可、round 2 m-R2-4)。
   - **項目単位の例外**を許す(理由コメント必須)。例: `path_health_fsm.rs:18`は不純な`path_health.rs`
     (tokio driverを含む)から`classify_path_health`/`has_zero_response`を`use`している(確認済み)。
     例外として登録するか、両関数を純粋モジュールへ移すかはStep 0のPRで決める。
   - これで`session_state.rs → crate::terminal`(`terminal.rs:1113`の時計読み)のような推移的不純が検出される。
   - 限界: trait経由の動的呼び出しや、許可した値型モジュールに後から不純コードが入る場合は検出できない。
     許可リスト側モジュールの変更はレビューで見る。スクリプトは手書きの簡易字句解析なので、既知の死角を
     スクリプト冒頭に明記する(round 2 m-R2-5、round 3 m-R3-8): `self::`/`super::super::`等の相対パスの解決、
     許可済みモジュールを経由した再エクスポート(`pub use`)、マクロが生成するパス、`#[path]`属性、
     入れ子のグループimport(`use crate::{a::{b, c}, D}`、括弧対応で展開して全項目を個別に判定する)。
   - スクリプトが**能動的に拒否する**もの: 未登録モジュールからのglob import(`use crate::x::*`、`use crate::*`。
     項目単位の判定ができなくなるため)、純粋モジュール内の内部可変な`static`/`thread_local!`(clippy層と二重に見る)。

### 2.4 shell(interpreter)の規則

1. **集約ごとにロック1つ。** 1つのreducerが表すStateは1つのロックで守る。
2. **Effectは2種類に分ける。**
   - *in-lock effect*: 同じロックが守るshell側データ(例: ソケットマップからのエントリ除去=`drop`)だけを
     同期的・非ブロッキングに変更するもの。**applyと同じ臨界区間内で解釈する**(原子性が要る、Step 2)。
   - *out-of-lock effect*: コールバック送出、ネットワークI/O、別ロック取得、`await`を伴うもの。
     **ロック解放後に解釈する**(`parking_lot::Mutex`は非再入、round 1 M-6)。
3. **解釈中に同期的に生じたEventは、新しい`apply`呼び出し(ロック再取得)として戻す。ネストしたapplyは禁止。**
4. **状態公開の順序**: 現状のコールバック順は「たまたま到着した順」で(`on_connected`、`orchestrator.rs:525-544`と、
   再接続ループの`Reconnecting`公開、`:1102-1106`付近は別スレッドで走る)、`Reconnecting`が`Connected`の後に
   届く逆転をreducerのproptestは検出できない。**本ADRのStepでは汎用の連番publisherは導入しない**(round 2 m-R2-1)。
   順序が意味を持つ通知の組(Step 8a′の`Connected`→`Established`等)は、**同じ呼び出し箇所から固定順で**出す
   ことで保証する。汎用の連番/直列化publisherは、逆転が実害として観測された時点で別Stepとして追加する。
5. I/O結果・RNG値・IDはshellが生成してEventに載せる。shellはStateを覗いて判断しない(読み取りクエリは可、
   ただしクエリ結果で分岐したくなったらreducerへ移す兆候)。
6. **ロック順**: 集約ロックを保持したまま他の(per-session等)ロックを取らない。逆も同様(ネスト禁止)。
   どうしてもネストが要る場合はADRのStep本文で順序を明記する。
7. **RAIIバックストップは撤去しない**(`EstablishedLease::drop`、`SessionTableEntryGuard::drop`)。
8. shellの配線テストは`#[tokio::test(start_paused = true)]`+fake port(`rebind_driver.rs`の`FakeFdSource`等)。

### 2.5 5つの思想との対応

| 思想 | 採るもの | 採らない/合わないもの |
|---|---|---|
| **FCIS** | ほぼそのまま。ただしshellの原子性規則(§2.4-1,2,6)を伴う | — |
| **Algebraic effects** | 「効果をデータとして返し、handler(shell)が解釈する」部分のみ。呼称は「effects-as-data」 | 継続を握って計算を再開する仕組みは無い。結果は新しいEventとして戻る(ElmのCmd→Msg)。effect-handler系crate・`async`による疑似継続は導入しない |
| **ASGI** | プラットフォーム⇔Rust境界を「生イベントを送る/公開状態・コマンドを受け取る」メッセージ往復として扱う(`rust-ssot.md`と同じ形)。Step 8a′のcallback追加もこの形 | ASGI appは自分で`receive()`を`await`するasync callable。reducerは同期でループを持たない。`scope`/`lifespan`相当は新設しない |
| **Elm** | Model=State、Msg=Event、update=`apply`、Cmd=Effect、Sub=shellの長寿命入力源(timer、network monitor、socket read) | Elmは全体で1つのModel。本ADRは集約ごとに複数 |
| **Redux** | 「Stateを変えるのはreduceだけ」「Eventはシリアライズ可能なデータ」「middleware=shell」 | プロセス/アプリ全体の単一storeは採らない(アプリ・`isekai-ssh`・`isekai-pipe serve`は別プロセスで、同一プロセス内でも寿命とロックが独立)。**単一Stateは集約単位**で適用する(Step 2の`isekai-pipe serve`1プロセスの「fencing+session table」が典型) |

### 2.6 (任意)Event列の記録とreplay

`now: Millis`とtokenをEventに載せる規約により、Event列だけでreducerの挙動を再現できる。実機で記録した
Event列をCIのreplayテストにする構想は残すが、**§3の秘密情報規則が前提**であり、着手はStep 3a完了後に判断する。

---

## 3. 新規コードの規則

1. セッション/接続/トランスポート/再試行の**判断**は§2.1の形のreducerか`*_policy.rs`の純粋関数として書き、
   `pure_modules.toml`に登録する(§2.3の2層検査の対象になる)。
2. タイマーは§2.2-1の形(token付きEffect)で書き、stale tokenプロパティのproptestを同じPRで付ける。
   タイマー以外の「遅れて届く事実」Eventも世代トークンを運ぶ(§2.2)。idだけで既存エンティティを指す事実Eventは書かない。
   クライアントが選んだidで引く要求は1回のapplyで解決し、トークンはreducerが返す(クエリ→別applyの二段階は禁止)。
   `now`を使うreducerは`Millis`の差を`saturating_sub`でのみ計算し、非単調`now`のproptestを付ける。
3. **秘密情報をEvent/State/Effectに載せない。** `SshAuth`は`Debug`を導出し`Password{password}`/
   `PublicKey{private_key_pem}`を持つ(`lib.rs:791-795`、確認済み)。`LastConnectAttempt`(`SshConfig`等)を
   reducerに渡さず、shellが保持する接続設定への**不透明な参照`AttemptRef`**だけを載せる。
4. **決定論的コンテナ**: Effectの順序やタイブレークに影響するコレクションは`BTreeMap`/`BTreeSet`か、
   明示的なタイブレーク(例: `(parked_since, id)`の辞書順)を使う。`HashMap`は一意キー検索だけに使う
   (既存`AttachArbiter::session_for_lease`の`find_map`はleaseが一意なので決定論的、確認済み)。
   複数IDを返すEffectのVecはソートして返す。
5. 破棄/解放/立ち退きは既存のEffect variant(Step 2の`Discard`等)に集約する。新しい破棄原因を足すときは
   §4.1の不変条件proptestを更新する。
6. reducerの型を`#[uniffi::export]`しない。UniFFIへ出すのは公開状態と生イベントの入口だけ。
7. 新しいreducerには最低1本のproptest(任意Event列→不変条件)を同じPRで付ける。
8. (rev5、Step 7a)**Effectを解釈する関数(interpreter)はEffect enumを`match`の明示armでのみ消費する。**
   `_`/束縛ワイルドカードarm・`if let`・`let .. else`・`matches!`でEffectを選り分けて残りを捨てない。
   新しいinterpreterは`pure_modules.toml`の`[[interpreter]]`に登録し、関数に
   `#[deny(clippy::wildcard_enum_match_arm)]`を付ける(Step 7a)。

---

## 4. 既存不変条件との関係

### 4.1 `always-connects.md`: 破棄経路で必ずfencing slotが解放されること

**現状(確認済み)**: `SessionTable`は`AttachArbiter`を知らず、破棄IDを戻り値で返し、`engine/mod.rs`が
`release_slot_for`を呼ぶ。戻り値は`#[must_use]`ではない。round 1で次の実害候補が見つかった:

- **sweep×RESUME TOCTOU**: sweepが期限切れIDを集めてロック解放 → RESUME(`mod.rs:1375-1386`)が
  `sessions.get`→`parked_since = None; parked_tcp.take()`で再開 → sweepが再確認なしに`remove` →
  `mod.rs:811-813`が`release_slot_for`→ 中継中のleaseの`relay_ended`。以後そのsessionはresume不能
  (UnknownToken)かつ同じsession_idが再admit可能。窓は小さい(sweepは5秒ごと、期限境界のみ)が経路は実在
  (コード経路は確認済み、再現は未実施)。
- **admitのcheck-then-act**: 2つの新規sessionが同時に`session_count() < max_sessions()`を通過しうる(max+1)。
- **`InsertOutcome::Rejected`**(`mod.rs:1269-1291`): arbiterは`Established`のまま、テーブルエントリ無しで
  中継を続ける。コード内コメント自身が「`always-connects.md`がバグとして扱う状態」と認めている既知のギャップ。
  **round 2 N-3で、これが恒久的なfencing slotリークであることが分かった**(確認済み): data streamが切れると
  `finish_or_park_session`の`DataStreamDied`分岐(`mod.rs:1673-1695`)が`lease.keep()`でslotを`Established`の
  まま残し、TCPを**どのテーブルにも載っていない**`handle`へparkする。sweep・LRU・RESUME(UnknownToken)の
  どれからも二度と見つからず、slotとtarget TCPはプロセス終了まで残る。同じsession_idの再HELLOは
  `AttachAlreadyEstablished`になり、`admit_new_session`の空き枠も1つ恒久的に減る。
  `always-connects.md`が「クライアントの再試行では原理的に回復できない」とするサーバー側状態リークそのもの。
- **id再利用によるABA**: `SessionTableEntryGuard::drop`はfire-and-forgetのspawnなので、その`remove(&id)`が
  任意に遅れて着き、同じsession_idの**新しいincarnation**のエントリを消しうる(現状でも起こりうるが、影響は
  テーブルに限られる)。2aで破棄とslot解放を同一遷移にするなら、世代トークン無しでは新incarnationの
  slotまで解放してしまう(round 2 N-2)。
- **activate→insert間の窓**: `activate()`で`Established`になってから`insert_existing`までテーブルに無い。
- **Q3回答**: `EstablishedLease::drop`(`attach_runtime.rs:107-132`)は`relay_ended`をspawn、
  `SessionTableEntryGuard::drop`(`mod.rs:1125-1147`)は`remove`をspawnする。互いに独立のfire-and-forgetで
  順序保証は無く、両方をpanic下で同時に検証するテストは見当たらない(grepレベル)。

**Step 2での扱い**: fencing(`AttachArbiter`)とsession index(+parkedソケット)を**1つの集約・1つのロック**に
まとめ、破棄はreducer内の遷移で「indexから除く」と「arbiter slotを解放する」を同一applyで行う。
これにより`relay_ended`の呼び忘れは「呼ぶべき関数を呼ばない」問題ではなく「同じ遷移の中にある」性質になる。

**不変条件(proptestで検証、B-1を踏まえ成り立つ状態に限定)**:
- I-a: `Discard{id}`を返したapplyの後、`id`はindexにもarbiterの`Established`にも存在しない。
- I-b: indexに存在しparked状態の`id`は、arbiterで`Established`である(parkはfencing slotを保持する設計)。
  **現状のコードはこれを破っている**(round 3 R3-2、確認済み): RESUMEがparkedソケットを取り出した後に
  `established_lease_for`が`None`を返すと、slotの無いsessionへソケットを戻す(`mod.rs:1431-1435`)。2aでこの分岐は削除する(Step 2a)。
- I-c: arbiterで`Established`な`id`はindexに存在する。容量超過(現状の`Rejected`)のsessionも
  `IndexEntry { unresumable: true }`としてindexに載せる。RESUME不可・LRU対象外という現状の外部挙動は2aで保存する。
  **容量の計数は2つあり、どちらも現状どおりに保つ**(round 3 m-R3-3、確認済み):
  - table側の上限判定(現`insert_existing`の`inner.len() >= max_sessions`): unresumableエントリを**数えない**
    (現状`Rejected`はテーブルに居ないので数えられていない)。
  - admission側の判定(現`admit_new_session`の`attach_runtime.session_count() < max_sessions()`、`mod.rs:1068-1072`):
    arbiterのslot数なので、unresumableエントリも**数える**(現状もarbiterでは`Established`なので数えられている)。
  activate→insertの窓は、2aで「`Activated`遷移の同一applyでindex登録」することで消える。
- I-d: activeな(parkedでない)`id`には`Discard{cause: Evicted|Expired}`を出さない。
- I-e: 非現行lease/tokenのEventはStateを変えない(§2.2必須プロパティ)。
- I-f(round 2 N-2): 非現行leaseを運ぶEventは、決して`id`の`Discard`を引き起こさない。
- I-g(round 2 N-3): **unresumableなエントリはparked-and-Establishedに到達しない**。unresumableなエントリへの
  `Parked`は`Discard{cause: Unresumable}`(slot解放+TCP close)になる(Step 2a、ユーザー決定で2aの意図した挙動変更)。
- I-h: 時刻が逆行する`now`列を与えても、parkしてから`max_parked`未満のエントリに`Discard{Expired}`を出さない
  (§2.2の`saturating_sub`規則、round 2 N-1)。
- I-i(round 3 R3-2): `ResumeRequested{id}`がparkedソケットを渡す(`ResumeGranted`)のは、同じapplyの時点で
  `id`がparkedかつarbiterで`Established`かつunresumableでない場合だけであり、渡したleaseはその`Established` leaseと一致し、
  渡したソケットと出力バッファhandleは同じincarnation(そのleaseで`Activated`登録されたもの)に属する(round 4 m-R4-1)。
  それ以外では`id`の状態を変えずに拒否(または`RequestPreempt`)を返す。slotの無いsessionへのparkは起こらない。
- I-j(round 3 m-R3-4): arbiterの`RelayEnded{lease}`が現`Established` leaseに対して起きたapplyでは、同じapply内で
  indexエントリも除かれる(slotの無いindexエントリという中間状態を作らない)。

**限界**: これらは「判断と原子性が正しい」ことの検証で、shellが実際に`drop`/送信したことの検証ではない。
`docs/adr/0018-connection-resilience-simulation.md` §3のI2a(実engine必須)は置き換えない。RAIIバックストップは残す。

### 4.2 `always-connects.md`: 自動復旧しない失敗を作らない

reducer化した再接続系FSMには有界到達性のプロパティを課す:「到達可能な任意の状態から、ネットワーク回復+
試行成功に相当するEvent列を与えると、有限ステップ内に`Connected`の公開または試行開始Effectに到達する」。
真のlivenessではない(有界探索のみ)ことを明記する。`ConnectOutcomeClass`(`isekai-pipe-core/src/outcome.rs`)の
分類はどのStepでも変えない。Step 2はサーバー側で、新しいDiscard原因はクライアント側の`ConnectOutcome`経路に
影響しない(`always-connects.md`のサーバー側状態リーク規則の方が該当)。

### 4.3 `rust-ssot.md`

- RustのreducerがSSOT。iOSもUniFFI経由で同じreducerを使う。
- Kotlin側reducerは**UI表示状態の畳み込みのみ**(Step 8b)。接続エッジの判断はRustが明示イベントとして出す(Step 8a′)。
- Step 3aは集約`ReconnectState`を別ファイル`src/reconnect_fsm.rs`に定義し、`OrchestratorState`がそれを
  フィールドとして持つ(別ストアを作らない)。未移行の書き手が同じフィールド(`pub(crate)`)を書き続けても
  「状態のコピーが2つ」にはならない。所有者は常に`OrchestratorState`1つ。

---

## 5. `docs/adr/0018-connection-resilience-simulation.md`(L1保留)との関係

**L1(仮想時間+in-memory UDP)の保留は覆さない。** ただし同ADR §5が「L1で必要」とした
`RUNTIME`の`cfg(test)`注入は、in-memory UDPとは独立に切り出せるため、本ADRのStep 2.5として先行実施する
(同§5の受け入れ条件「`try_current()`フォールバックは採らない」「既存`rt.block_on`型テストの挙動不変」を
そのまま引き継ぐ)。reducerは実russh/noq/カーネルを含まないので、L0実Handleテスト・L2 netlabの役割は不変。
本ADRのStep 1.5〜3aの後、「reducerと`start_paused`で拾えないバグは何か」がL1再評価の材料になる。

---

## 6. 移行ロードマップ

順序(rev6): **0 → 7a → 1 → 1.5 → 2a → 10 → 2b → 2c → 2.5 → 3a(+pending_wake) → 5 → 6+7 → 4 → 8a′ → 11 → 13**。
8bは**再評価**へ移した(rev6)。3b/3cのうち`pending_wake`以外(tick会計・`woke_early`)は再評価のまま(§6末尾)。
rev5で7a・10・11・13を挿入し、rev6でユーザー決定の順序(`0 → 1 → 1.5 → 2a → 2b → 2c → 2.5 → 3a(+pending_wake) → 5 → 6+7 → 4 → 8a′`)
に組み替えた。rev5の追加Stepの位置は下の依存から決まる。
Step 12は独立(どこに入れてもよい。ただし6+7と同時には進めない、下記)。Step 9(調査)は完了し、その結果がrev6の順序変更の根拠になった(§0 rev6)。
rev5の固定依存: **0 → 7a**(7aはStep 0のclippyジョブ・`pure_modules.toml`・allowlistスクリプトに乗る)、
**2a → 10**(10の差分テストと網羅探索の対象は2aの`ServeAggregate`)、**3a → 11**・**8a′ → 11**(11が検査するEffect/
callback列は3aの`ReconnectEffect`と8a′の`on_connection_edge`)、**8a′ → 13**(goldenの中身が8a′のcallback)。
推奨順序(固定依存ではない): 7aは2aより前(2aで`activate`が`Discard`も受け取るようになる前にlintを効かせる)、
10は2bより前(2b/2cの挙動変更に対する安全網になる)、11 → 13(13のgolden生成は11の記録器を再利用する)。
8bを再評価の結果行う場合は13の後に行う(13のKotlin replayテストが8bの`_state.update`書き換えの安全網になる。
13はテストファイルだけを足し`TerminalSession.kt`本体を変えないので、8a′ → 8bのファイル衝突規則には当たらない)。
固定の依存関係(round 2 m-R2-7): **2.5 → 4**(どちらも`pool::release`のシグネチャ/spawn箇所`pool.rs:175-196`を
変えるので、逆順や並行は確実に衝突する。rev6でStep 4を6+7の後へ移しても満たされる)、**3a → 8a′**(8a′は3aが作る
`reconnect_fsm.rs`にedge状態を足す)。
**8a′ → 8b**(どちらも`android/.../session/TerminalSession.kt`を変える。8a′は新callbackの実装者として、
8bは`_state.update`22箇所の書き換えとして。round 3 m-R3-5。8bは再評価に移ったが、行う場合はこの依存が残る)。
並行してよいのは、Step 0の後のStep 6(rev6以降は6+7)だけ(他のStepと触るファイルが重ならない)。
rev5で追加: Step 12(`isekai-pipe/src/connect.rs`・`isekai-ssh/src/wrapper.rs`・`isekai-ssh/src/native/connect.rs`のみ)も
いつでも並行してよい。**ただしrev6でStep 7(回復ループ本体の共通化)が6に統合されたため、6+7とStep 12はどちらも
`wrapper.rs`/`native/connect.rs`を変える。この2つは同時に進めず、Step 12を先に行う**(12の網羅表テストが、
6+7のループ共通化に対する回帰検査になる)。
各Stepはそれぞれ独立PR、コミットは細かく分ける。

### Step 0: 純粋性検査の導入(本番コード変更は最小)

- `rust-core/clippy.toml`、`rust-core/pure_modules.toml`、import allowlistスクリプト、CIジョブ(§2.3)を追加。
- 初期登録: `isekai-pipe/src/engine/attach_arbiter.rs`、`src/rebind_manager.rs`、`src/trzsz.rs`、
  `src/net_health_policy.rs`、`src/background_reliability_policy.rs`、`isekai-transport/src/path_health_fsm.rs`
  (`path_health.rs`由来の2関数の扱いを決めてから)。`path_health_fsm.rs`は`noq::PathStats`をインラインで使う
  (`:36,45,82,99`、確認済み)。`noq`は外部crate許可リストに無いので、**`noq::PathStats`を項目単位の外部許可エントリとして登録**する
  (統計値の値型。round 4 m-R4-3。登録しないとStep 0の初回CIがこのモジュールで落ちる)。Q10と一緒に決める。
  - **`session_state.rs`は除外**(Q8回答: `crate::terminal::Terminal`経由で`terminal.rs:1113`の
    `Instant::now()`を読む)。登録は別PRで、その前提条件は「`now`を引数化する」だけでは済まない
    (round 2 N-5、確認済み)。`session_state.rs`が参照するものすべてを純粋側に揃える必要がある:
    1. `terminal.rs`自体の登録: `dispatch_apc`の`Instant::now()`(`:1113`)を引数化(テスト専用の
       `dispatch_apc_at(payload, now)`が`#[cfg(test)]`で既にある、`:1124-1125`)、かつフィールド
       `last_panel_update_at: Option<std::time::Instant>`(`:516`)を`Millis`へ(型そのものが`disallowed_types`)。
       `terminal.rs`は`crate::session::SCROLLBACK_LIMIT`(`:1740`)を不純な`session.rs`から参照しているので、
       項目単位の許可か定数の移設が要る。
    2. `crate::session::to_cell_data`(`session.rs:167`): `session.rs`は`RUNTIME.spawn`(`:331`, `:552`)を含む不純
       モジュールなので、項目単位で許可するか純粋モジュールへ移す。
    3. `kitty_graphics.rs`・`sixel.rs`・`ai_panel.rs`の登録(本番コード部分に時計・RNG読みは見当たらない、
       grepレベルで確認。clippy+allowlistで正式に確認する)。`ai_panel.rs:26`はcrate rootの
       `PanelField`/`PanelFieldKind`/`PanelKind`を`use`しているので、これらの項目単位許可も要る。
       **`theme.rs`はモジュールとしては登録できない**(round 3 m-R3-1、確認済み): `static THEME: LazyLock<RwLock<Theme>>`
       (`theme.rs:1-2,44`、`parking_lot::RwLock`)というプロセス全体の可変状態を持つ。`crate::theme::Theme`(値型)だけを
       項目単位で許可するか、グローバル部分を別ファイルへ分ける。現時点で`theme::current()`(`theme.rs:49`)を
       呼ぶのはshell側の`session.rs:324`だけで、`session_state.rs`/`terminal.rs`は呼んでいない(確認済み)。
    4. crate rootの`CellData`/`CursorColor`/`CursorShape`/`LineDamage`/`ScreenUpdate`/`TabColor`/`TabProgress`の
       項目単位許可(またはこれらの`public_state.rs`への移設)。
  - round 1でgrep方式に対し偽陽性だった`net_health_policy.rs`/`path_health_fsm.rs`のdoc中の
    `tokio::`/`spawn`/`Notify`は、clippyはコメントを見ないので問題にならない。
- **得られるCI検証**: 既存純粋モジュールが直接・推移的に時計/I/O/ロックへ依存し始める退行の検知。
- **リスク/ロールバック**: 誤検知時は登録解除のみ。clippyジョブはrequired化しない。

### Step 7a(rev5新規): Effect interpreterの網羅性lint

旧Step 7(isekai-ssh回復ループの共通化。rev6でStep 6へ統合)とは無関係。番号は「Step 0の検査基盤に乗る小さなlint」
であることを示すために付けた。

- **問題(確認済み)**: Effectを解釈する側が`match`以外でEffectを選り分けると、reducerが新しいEffectを返し始めたときに
  黙って捨てられる。実例: `AttachRuntime::activate`(`isekai-pipe/src/engine/attach_runtime.rs:210-222`)は
  `apply(AttachEvent::Activated{..})`の戻り値を`for effect in effects { if let AttachEffect::StartRelay { lease, .. } = effect { .. } }`
  で走査し、`StartRelay`以外のvariantは無視する。現状の`on_activated`(`attach_arbiter.rs:330-344`)は`StartRelay`しか
  返さないので無害だが、**Step 2aで`Activated`は「満杯なら最古parkedを`Discard{Evicted}`」も返す**(Step 2a)。
  この`if let`のままだとそのDiscard(in-lock effect)は解釈されず、立ち退きが起きない。
  一方`execute_effects`(`attach_runtime.rs:253-275`)は6 variantすべてを明示armで`match`しており(`StartRelay`は
  到達しない前提の`log::warn!` arm)、これが目標の形。
- **決定**:
  - interpreter関数(Effect列を受け取って実行する関数)に`#[deny(clippy::wildcard_enum_match_arm)]`を**関数単位で**付ける。
    モジュール単位にしないのは、同じ`attach_runtime.rs`にEffect以外の正当なワイルドカード`match`があるため
    (`:185-187`の`state_for`、`:359-361`の`leases`、確認済み)。`wildcard_enum_match_arm`はclippyのrestriction群のlint [EXT]
    で`clippy::all`に含まれないが、ソース内の`deny`はStep 0のジョブの`-A clippy::all`より優先されるので(§2.3)、
    同じジョブで検査される。
  - lintが捕まえない形(`if let`/`let .. else`/`matches!`/入れ子パターン内の`_`。どこまで捕まるかは [EXT] で
    Step 7aのPRで確認)は、Step 0のallowlistスクリプトを拡張して捕まえる: `pure_modules.toml`に
    `[[interpreter]] file = .., fn = .., effect = "AttachEffect"`を登録し、スクリプトはその関数本体で
    `if let <effect>::`・`let <effect>::`・`matches!(.., <effect>::`を拒否する(§2.3と同じ簡易字句解析なので、
    既知の死角をスクリプト冒頭に追記する)。
  - 初期登録: `AttachRuntime::execute_effects`、`AttachRuntime::activate`(本PRで`if let`を全variantの明示`match`に
    書き換える。`StartRelay`以外のarmは`execute_effects`の`StartRelay` armと同じく`log::warn!`のみ=現状到達しないので
    **挙動保存**)。以後、2aの`ServeAggregate` interpreter、3aの`ReconnectEffect` interpreter(`handle_unexpected_disconnect`の
    実行部)、4の`pool` interpreter、将来の再接続ループinterpreterは、それぞれのStepのPRで登録する(§3-8)。
  - Effect enumに`#[non_exhaustive]`は付けない(同一crate内では効かず、付けても網羅性検査は強まらない [EXT])。
  - Step 12の`decide_connect_failure_recovery`のように「Effectではないが網羅性が不変条件を担うenum」の`match`にも
    同じ属性を付けてよい(Step 12で判断)。
- **得られるCI検証**: Effect variantを追加したPRは、登録済みの全interpreterで明示armを書かない限りclippyジョブが落ちる。
  「reducerは正しいEffectを返したが、shellが黙って捨てた」という、reducerのproptest(§3-7)では原理的に見えない
  種類の退行がコンパイル時に止まる。
- **リスク**: 低。本番コードの変更は`activate`の書き換えだけで、到達しないarmの追加のみ。lint・スクリプトの誤検知は
  関数単位の属性/登録の解除で戻せる。
- **前提**: Step 0(clippy純粋性ジョブ・`pure_modules.toml`・allowlistスクリプト)がmainにあること。D3により
  純粋性検査ジョブがrequired化された後は、7aの検査もそのrequired check内で走る(ジョブを分けないので
  `main-branch-protection.md`のcontext追加作業は発生しない)。
- **ロールバック**: 属性と`[[interpreter]]`登録の削除、スクリプト拡張部分のrevert。`activate`の書き換えは挙動保存なので残してよい。

### Step 1: `AttachArbiter`のproptest不変条件(テストのみ)

- `isekai-pipe`のdev-dependencyに`proptest = { version = "1", default-features = false, features = ["std"] }`
  (ルートcrateと同じ指定)。`Cargo.lock`は§7の手順でCI生成。
- テストは`attach_arbiter.rs`内の`#[cfg(test)] mod`に置く(`LeaseId(u64)`のフィールドはprivate、
  `:49`——テストのためだけに本番側へ`pub`コンストラクタを足さない)。leaseは「k番目に発行されたlease」を
  発行済みEffectから回収して参照するstrategyにする。
- 不変条件(round 1 m-1で修正):
  - session_idごとのエントリは高々1つ(=at-most-one-lease)。
  - `ClosingForSupersede`に入った後、そのsessionの最初の`ConnectTarget`は`LeaseStopped{old_lease}`への
    応答としてのみ出る(`on_lease_stopped`、`:368-374`)。
  - `RelayEnded{lease}`は、それが**そのsessionの現`Established` lease**だった場合にのみエントリを除去し、
    それ以外(stale lease)ではStateを変えない(`on_relay_ended`、`:385-391`)。
  - 現行でない`LeaseId`を名指すEvent(`TargetConnected`/`TargetConnectFailed`/`PendingExpired`/`LeaseStopped`)は
    Stateを変えない(§2.2必須プロパティ)。
  - `StartRelay`は`Activated`遷移でのみ、同一leaseに高々1回。
- **得られるCI検証**: 既存の手書きテスト約20本が列挙していないインターリーブの探索。
- **リスク**: なし(テストのみ)。

### Step 1.5(新規): sweep×RESUME競合のL0特性テスト(テストのみ)

- 実`SessionTable`+`#[tokio::test(start_paused = true)]`。決定論的に窓を突くため、`sweep_expired_parked`の
  2段階(収集→除去)の間に`#[cfg(test)]`のフック(例: `Notify`による一時停止点)を設けるか、
  2段階を`#[cfg(test)]`から個別に呼べる形に分ける(本番の挙動は変えない)。
- テストは**現状の挙動をそのままassertする**特性テスト(例:
  `sweep_after_concurrent_unpark_currently_discards_live_session`)として入れる。Step 2aのPRがこのassertを
  反転させ、「リファクタが競合を移動させたのでなく閉じた」ことを証明する。
- 可能なら`admit_new_session`の同時admit(max+1)についても同様の特性テストを置く(2bの前提)。
- **範囲(round 2 m-R2-9)**: `SessionTable`単体では「テーブルから消えた」半分しか見えず、実害である
  「`release_slot_for`(`mod.rs:811-813`)が中継中のleaseの`relay_ended`を呼び、fencing slotが空く」半分は
  `engine/mod.rs`側にある。テストには`AttachRuntime`も含め(targetはローカル`TcpListener`で足りる)、
  sweep後に`AttachRuntime::established_lease_for(id)`が`None`になる(=中継中なのにslotが解放された)ことまで
  assertする。`Rejected`→`DataStreamDied`の孤児park(§4.1、N-3)も、`#[cfg(test)]`のテーブル直接挿入
(`resume.rs:319`以降の既存テスト用impl)でactiveなエントリを満杯にして`Rejected`を起こせるなら同様に特性化し、
2aで反転させる(起こせない場合は2aのproptest I-gだけで担保し、その旨をPRに書く)。
- **得られるCI検証**: 既知の競合が「レビューの記述」から「赤/緑で見える事実」になる。
- **リスク**: テストフックが本番コードに残る点(`cfg(test)`限定にする)。

### Step 2: `isekai-pipe serve`の単一集約(`AttachArbiter` + `SessionIndex` + parkedソケット)

**目的の再定義(round 1 B-2)**: 純粋化そのものより、**原子性**が本当の成果。

**2a(挙動保存リファクタ+TOCTOU解消)**
- 集約: `struct ServeAggregate { arbiter: AttachArbiter, index: SessionIndex }`(純粋、`pure_modules.toml`登録)。
  `SessionIndex`は`BTreeMap<[u8;16], IndexEntry { lease: LeaseId, parked_since: Option<Millis>, negotiated_grace_secs: Option<u32>, unresumable: bool }>`
  (`lease`はそのincarnationの`Established` lease。RESUMEで`resumed_lease`を受け取ったときも同じleaseのまま)
  (`resume::SessionId = [u8;16]`は`Ord`、`isekai_protocol::SessionId`は`Ord`でない——確認済み)。
  最古parkedの選択は`(parked_since, id)`の辞書順でタイブレーク。
- shell: `AttachRuntime`の`arbiter: Mutex<AttachArbiter>`を`Mutex<(ServeAggregate, BTreeMap<[u8;16], SessionIo>)>`
  に置き換える。`SessionIo`は`parked_tcp`・`preempt`/`reparked`の`Notify`(cloneして外で通知)、および
  per-sessionの`handle: Arc<Mutex<Session>>`を持つ。`output_buffer`/`helper_committed_offset`/`output_space_available`は
  中継のホットパスなので従来どおりその`Arc<Mutex<Session>>`の中に残す(ロック自体は集約ロックと別、ネストしない)。
  **handleの登録・取得は集約経由だけ**(round 4 m-R4-1): `Activated`のapplyで`SessionIo`ごと登録し、RESUMEは
  `ResumeGranted`で受け取る。現在の`sessions.get(&session_id)`(`mod.rs:1375`)のような「idでhandleだけ引く」経路は残さない。
  `leases`/`waiters`は既存どおり別ロック。
  中継ループは現在`preempt`/`output_space_available`をper-session handleからcloneしている(`mod.rs:1821-1822`)ので、
  `preempt`を`SessionIo`へ移す際はこの受け渡しを付け替える(Q9と合わせて2aのPRで具体化)。
- **`now`の刻印**: すべての`now`付きEventは`apply_with(|now| ..)`で集約ロック取得後に刻む(§2.2、N-1)。
  sweepタスク(`mod.rs:797`付近の5秒周期)も同様。
- **ロック順**: 集約ロックとper-sessionロック・`leases`/`waiters`ロックはネストさせない(§2.4-6)。
  現在park箇所はper-sessionロック下で`parked_tcp`を書いているが、2a後は`parked_tcp`が集約側に移るので
  per-sessionロックは不要になる(ABBAの原因を構造的に除く)。
- **Event(removalはreducer起点。既存sessionに関する事実はleaseを運び、idで引く要求は1回のapplyで解決する)**:
  `HelloReceived`等の既存`AttachEvent`に加え、
  - 事実: `Activated{.., now}`(この同一applyでindex登録。tableが満杯**かつ立ち退けるparkedエントリが無い**ときだけ
    `unresumable: true`で登録。満杯でもparkedがあれば現状の`insert_existing`どおり最古parkedを`Discard{Evicted}`して
    通常登録する、round 3 m-R3-3)、`Parked{id, lease, now}`、`Sweep{now, max_parked}`、
    `RelayTerminated{id, lease, reason: TcpDied|GuardDropped}`。
  - 要求: `ResumeRequested{id, now}`(round 3 R3-2、下記)、`AdmitRequested{id}`(2b)。
  - rev2の`Unparked{id, lease}`は**廃止**: RESUME側はunparkの時点でleaseを知らない(`mod.rs:1375-1386`でソケットを
    取り出してから`:1431`で`established_lease_for`を引く)ため運べず、「先にクエリ、後で事実」にすると2aが閉じる
    TOCTOUが戻る。unparkは`ResumeRequested`の結果としてreducer内で起こる。
  shellが直接「消す」Eventは持たない(round 1 M-1)。`lease`は`EstablishedLease`/`resumed_lease`の保持者が
  知っている値を載せる。`SessionTableEntryGuard`も生成時にleaseを保持し、Drop時の`RelayTerminated`に載せる。
  **reducerは`IndexEntry.lease`と一致しないleaseの事実Eventを無視する**(round 2 N-2、I-f)。
- **Effect**: `Discard{id, lease, cause: Expired|Evicted|TcpDied|GuardDropped|Unresumable}`(in-lock:
  ソケットマップから除去して`drop`=TCP close。arbiter slotの解放は同じapply内のState遷移で済む)、既存の
  `AttachEffect`(out-of-lock)。`Discard` interpreterは**1箇所・冪等**で、`(id, lease)`の組で照合する
  (同じidでもleaseが違えば別incarnationとして触らない。`on_relay_ended`が不在leaseに`vec![]`を返すのと同じ方針)。
- **RESUMEの原子的解決(round 3 R3-2)**: `ResumeRequested{id, now}`に対しreducerは1回のapplyで次のどれかを返す。
  - `id`がparkedかつ`Established`かつunresumableでない → `ResumeGranted{id, lease, parked_tcp, handle}`(in-lock effect:
    `SessionIo.parked_tcp`と出力バッファの`handle`を**一緒に**shellへ引き渡す。同じapplyでエントリをunparked=activeにする。
    round 4 m-R4-1: handleを別途`get(id)`すると、間にSweep+同じidの新incarnationが挟まった場合に
    incarnation 2のソケットとincarnation 1のreplayバッファが組み合わさる。実際の窓はマイクロ秒で事実上踏めないが、
    §2.2が禁じるABAそのもので、塞ぐコストはゼロ)。
  - `id`がactive(parkedでない)かつ`Established` → `RequestPreempt{id, lease}`(out-of-lock: 現在の中継への`preempt`通知。
    Q9)。shellは`reparked`を待ち、**起床でもタイムアウト(`PREEMPT_WAIT_TIMEOUT` = 2秒、`mod.rs:79`)でも必ず1回、
    新しい`ResumeRequested`を送り直す**(round 4 m-R4-2)。`notify_waiters`は既に存在する`Notified`しか起こさない [EXT]
    ので、`RequestPreempt`の返却から`reparked.notified()`の生成までの間に中継が別理由でparkすると起床を取りこぼす
    (現状コードにも同じ窓がある、`:1384`→`:1404`)。再送は前回の結果に依存しない原子的な要求なので安全で、
    取りこぼしは「拒否」ではなく「最大2秒の遅延」になる。再送の結果が再び`RequestPreempt`なら、それ以上は待たずに拒否する。
  - それ以外(エントリが無い・slotが無い・unresumable) → `ResumeRejected{id, UnknownToken}`。状態は変えない。
  以後この経路の事実はすべて返された`lease`を運ぶ: OffsetGone/RESUME_ACK書き込み失敗時の再park
  (`repark`、`mod.rs:1519-1527`)は`Parked{id, lease, now}`、`resumed_lease(lease)`、`SessionTableEntryGuard::new(.., lease)`
  (`mod.rs:1474`, `:1480`、ガードは既にlease確定後に作られている、確認済み)。
- **slot無しrepark分岐の削除(round 3 R3-2、2aの意図した挙動変更その3)**: 現状`handle_resume_stream`は、
  parkedソケットを取り出した後に`established_lease_for`が`None`なら、slotの無いsessionへソケットを戻して
  UnknownTokenを返す(`mod.rs:1431-1435`)。これはI-b(parked ⇒ Established)を破る状態を自ら作る経路で、
  そのソケットは以後のRESUMEでも同じ分岐に落ち続け、sweepで消えるまでtarget TCPを保持する。2aでは
  `ResumeRequested`が「slotが無ければソケットに触れずに拒否」するので、この分岐は**到達不能になり、翻訳せず削除する**。
  I-bが2aの集約で成り立つことと合わせ、2aのPR本文に**意図した挙動変更**として明記する
  (TOCTOU修正・unresumable修正と同じ扱い)。不変条件はI-i(§4.1)。
- **arbiterの`RelayEnded`と`RelayTerminated`は1遷移(round 3 m-R3-4)**: TcpDied経路は現状
  `lease.release()`(→`EstablishedLease::release`、`attach_runtime.rs:93-97`→arbiterの`RelayEnded{lease}`)の後に
  `sessions.remove`(`mod.rs:1665-1666`)を別に行う。集約では、現`Established` leaseに対する`RelayEnded{lease}`
  (`release()`由来でも`EstablishedLease::drop`由来でも)を**そのまま`Discard{id, lease, TcpDied}`と同じ1つの遷移**として扱い、
  同じapplyでindexエントリも除く(I-j)。続いて届く`RelayTerminated{id, lease, TcpDied}`はエントリが既に無いので冪等なno-op。
- **unresumableエントリの扱い(ユーザー決定、round 2 N-3 (a))**: unresumableなエントリへの`Parked`は
  parkせず`Discard{cause: Unresumable}`を返す(fencing slot解放+target TCP close)。現状は孤児parkで
  slotとTCPが恒久的に残る(§4.1)。この変更は**2aのPRで意図した挙動変更(バグ修正)として明記**する
  (TOCTOU修正と同じ扱い)。クライアントから見た挙動: 現状もこの時点のRESUMEはUnknownTokenで失敗するので
  resumeの可否は変わらず、変わるのは「その後の同じsession_idの再ATTACHが`AttachAlreadyEstablished`で永久に
  拒否される」が「受理される」になる点だけ。
- `SessionTable::remove`(`resume.rs:254`)・`insert_existing`等は2aでファサードとして残し、内部を集約applyに
  差し替えた上で`pub(crate)`以下へ縮める。
- **全park/unpark/removal箇所の棚卸し(確認済み、2aですべてEvent化する)**:

  | 種類 | 箇所 | 2a後 |
  |---|---|---|
  | park | `mod.rs:1525-1526`(`repark`)、`:1681-1682`(`DataStreamDied`)、`:1706-1707`(`Preempted`) | `Parked{id, lease, now}` |
  | unpark | `mod.rs:1385-1386`(RESUME直後)、`:1409-1410`(preempt後) | `ResumeRequested{id, now}`→`ResumeGranted{id, lease, parked_tcp, handle}`(集約ロック下でソケットと出力バッファhandleを一緒に引き渡す) |
  | handle取得 | `mod.rs:1375`(`sessions.get(&session_id)`) | **削除**(`ResumeGranted`に含まれる) |
  | slot無しrepark | `mod.rs:1431-1435` | **削除**(`ResumeRequested`がslot無しを拒否するので到達不能、意図した挙動変更) |
  | removal | sweep(`resume.rs:299`→呼び出し`mod.rs:811`)、LRU evict(`resume.rs:199-201`)、`claim_oldest_parked`(`resume.rs:243-245`→`mod.rs:1076`)、`TcpDied`(`mod.rs:1666`、`lease.release()`経由)、`SessionTableEntryGuard::drop`(`mod.rs:1145`)、公開`remove`(`resume.rs:254`) | すべて事実Event→reducerの`Discard` |
  | insert | `insert_existing`(`mod.rs:1269`) | `Activated`遷移内 |
  | 孤児park(`Rejected`後) | `DataStreamDied`分岐(`mod.rs:1673-1695`、`table_guard == None`のとき) | `Parked{id, lease, now}`→unresumableなので`Discard{Unresumable}` |

- **TOCTOU**: sweepは集約ロック下の1回のapplyで判定と除去を行い、RESUMEのunparkも`ResumeRequested`として
  同じロックの1回のapplyで解決されるので窓は消える。Step 1.5の特性テストのassertをこのPRで反転させる
  (これは意図した挙動変更=バグ修正としてPR本文に明記)。
- **2aに含める意図した挙動変更は3つだけ**: (1) sweep×RESUME TOCTOUの解消、(2) unresumableエントリの孤児park→
  `Discard{Unresumable}`、(3) RESUMEのslot無しrepark分岐の削除。それ以外の外部挙動は保存する。PR本文で3つとも列挙する。
- **得られるCI検証**: I-a〜I-j(§4.1)を任意Event列(非単調`now`・stale lease・id再利用・要求と事実の交錯を含む)で検証。
  容量超過時にactiveを立ち退かせない/最古parkedを決定論的に選ぶ/per-session graceの短い方を使う、を任意時刻列で検証。
- **リスク/ロールバック**: ロック構造が変わるため「ファサード内部だけ戻す」ロールバックは成立しない。
  **PRごとrevertする**前提とし、Step 1.5の特性テスト(反転後)と既存`resume.rs`/engineテストが緑であることをマージ条件にする。

**2b(挙動変更、別PR)**: `AdmitRequested`を集約applyで処理し、`admit_new_session`のcheck-then-act(max+1)を
解消する。Step 1.5の同時admit特性テストを反転させる。

**2c(挙動変更、別PR、D1によりStep 2bの後で実施)**: 2aでslotリーク自体は解消済みなので、残るのは「容量超過で登録されたsessionは
data streamが一度切れると(RESUMEできずに)接続を失う」というresume不能性だけ。admission時点で容量を予約し
unresumableなエントリを到達不能にすれば、これも解消できる(I-cの「table側の計数からは除外、admission側では数える」
という2つの計数の食い違いも不要になる)。
2bの後に、`--max-sessions`到達が実際に観測されているかを見て判断する。
2b/2cは2aに混ぜない(round 1 B-1「リファクタと挙動変更を混ぜない」。2aの挙動変更は上記2点に限定)。
見送る場合はこのADRに「延期」と明記して記録する。

### Step 2.5(新規): orchestrator/poolの`cfg(test)` RUNTIME注入

- `OrchestratorShared`に`tokio::runtime::Handle`を持たせ、本番は`RUNTIME.handle().clone()`、テストは
  明示的に現在のテストランタイムのHandleを渡す(`try_current()`フォールバックは採らない、
  `docs/adr/0018-connection-resilience-simulation.md` §5)。`pool::release`はspawn先のHandleを引数で受け取る形にする
  (呼び出し元は本番で`RUNTIME`のHandleを渡す)。正確なAPIはPRで決める。
- `orchestrator.rs`の`std::thread::sleep`14本を`#[tokio::test(start_paused = true)]`+`tokio::time::sleep`に書き換える。
- **得られるCI検証**: 既存14本の実時間依存(負荷でflaky化する種類)が決定論的になる。round 1 M-10の指摘どおり、
  これはreducer化より直接的な手段。
- **リスク**: テスト中に`reconnect_attempt`が実接続(`connect_via`→`build_and_store_session`)を呼ぶ経路で
  `block_on`を使っていればcurrent_threadランタイム下でpanicしうる(推測、現テストは偽の`reconnect_attempt`
  クロージャを使うので回避されている可能性が高い)。`rt.block_on`型テスト約20本の挙動不変を受け入れ条件にする。

### Step 3a: `handle_unexpected_disconnect`の判断を純粋化(rev6: `pending_wake`の遷移を含む。3b/3cの残りは再評価)

**集約の定義(この時点で全体を定義し、移行は3aの範囲だけ行う)**:
**新設ファイル`src/reconnect_fsm.rs`(`pure_modules.toml`に登録)**に
`ReconnectState { phase, reconnect_epoch, reconnect_loop_active, retry_attempt_in_flight, pending_wake,
user_initiated_disconnect, background_state, last_attempt: Option<AttemptRef> }`を置き、
`orchestrator.rs`のprivate enum `ConnPhase`(`:171`)・`BackgroundState`(`:185`)・`DisconnectKind`(`:737`)と、
`DisconnectKind::classify`が直接比較する定数`NETWORK_LOST_REASON`(`:715`、round 3 m-R3-6)も
このファイルへ移す。`OrchestratorState`は`reconnect: ReconnectState`を**フィールドとして持つ**(round 2 N-5)。
`orchestrator.rs`自体は`RUNTIME.spawn`(`:969`, `:1678`)と`parking_lot::Mutex`を使うので登録できず、reducerを
その中に書くと§2.3の検査が効かないため、別ファイルが必須。`reconnect_fsm.rs`が使うcrate rootの型は
`crate::ConnectionPublicState`/`crate::ConnectionIssueHint`の項目単位許可で参照する(§2.3)。
所有者は`OrchestratorState`1つのままなので§4.3の「状態のコピーを作らない」は保たれる。
- `AttemptRef`(round 2 m-R2-6): `begin_connect`だけが進める単調なID(`u64`)。`last_connect_attempt`の唯一の
  書き手が`begin_connect`であるという既存の設計(`orchestrator.rs:857-866`のdoc)に合わせる。実体の
  `LastConnectAttempt`(秘密を含む`SshConfig`等)は従来どおり`OrchestratorState`のshell側フィールドに置く。
- `app_foreground`/`tab_focused`のような「OSからの生の事実」フィールドは、判断に使われない限り集約に入れない。
**`session_generation`はこの集約に含めず、別の世代カウンタのまま残す**(`orchestrator.rs:322-329`のdocが
「`reconnect_epoch`とは別物」と明記。古いアダプタからの遅延コールバックを捨てる仕組みで、
ループの生存確認である`epoch`と統合すると古いコールバックが受理されうる)。Eventには
`generation`を別フィールドとして載せる。

**全エントリポイント→Event対応表**(確認済みの行番号):

| エントリポイント | Event |
|---|---|
| `OrchestratorAdapter::on_connected`(`:525`、`is_current()`で`session_generation`照合) | `AttemptConnected{generation}` |
| `OrchestratorAdapter::on_disconnected`(`:546`) → `handle_unexpected_disconnect`(`:767`) | `AttemptDisconnected{generation, kind: DisconnectKind, targets_local_network: bool}`(最後のフィールドはshellが`last_connect_attempt`から事前計算。`classify_disconnect_issue_hint`、`:243`、`:807-811`で使用) |
| `OrchestratorAdapter::new`(`:458`、`session_generation`を進める) | `SessionCreated{new_generation}`(Step 8a′のLost判定用。3aでは定義のみ。**`session_generation += 1`と同じ`state.lock()`臨界区間内でapplyする**、round 3 m-R3-2) |
| `apply_network_lost`(`:717-722`、debounce満了。`s.disconnect()`の後、アダプタを経由せず`handle_unexpected_disconnect`を直接呼ぶ) | `AttemptDisconnected{generation: 現行の session_generation, kind: NetworkLost, ..}`(アダプタ経由の切断と**同じ遷移**を通す。round 3 R3-1。`:705-710`のdocが「同じ`handle_unexpected_disconnect`を経由させる」と意図している形をEventでも保つ)。debounce自体のstale判定は従来どおり`path_observer`のepoch(shell側)で行う |
| `begin_connect`(`:1215`) | `ManualConnectStarted{attempt: AttemptRef}` |
| `disconnect`(`:1315`) | `UserDisconnect` |
| `cancel_reconnect`(`:1327`) | `CancelReconnect` |
| `notify_did_enter_background`(`:1360`) | `EnteredBackground{budget_ms}` |
| `notify_background_budget_expired`(`:1376`) | `BackgroundBudgetExpired` |
| `notify_memory_warning`(`:1386`) | `MemoryWarning` |
| `notify_will_enter_foreground`(`:1400`) | `WillEnterForeground` |
| `notify_network_path_changed`(`:1638`、内部で`RUNTIME.spawn`のdebounce `:1678`) | `NetworkPathChanged{satisfied}` |
| 再接続ループのtick / wake / 同期エラー(`:963`〜) | `ReconnectTick{epoch}` / `ReconnectWake{epoch}` / `AttemptFailedSync{epoch}` |
| `debug_set_reconnect_policy`系 | `PolicyChanged` |

**3aの範囲**: 上表のうち`AttemptDisconnected`の遷移だけを`ReconnectState::apply`に移す。既存の
ローカル`enum Action { Suppress, StartLoop, NotifyDisconnected }`(`:768`)がそのままEffectになる。
他の書き手は従来どおり`OrchestratorState.reconnect`の(`pub(crate)`)フィールドを書く(所有者は1つのまま、§4.3)。
- 実行は§2.4-2,3: ロック下でapply → 解放後に`spawn_reconnect_loop`/コールバック/`path_observer.invalidate()`を実行。
- `Action::StartLoop`が運ぶ`LastConnectAttempt`は`AttemptRef`に置き換え、実体はshellが保持(§3-3)。
- **得られるCI検証**: `DisconnectKind`×`was_connected`×`user_initiated`×`reconnect_loop_active`×`pending_wake`の
  全組み合わせをproptestで網羅し、「ループ動作中は二重起動しない」「`GracefulRemoteExit`は再接続しない」
  「Connected未到達の失敗だけにLocal Networkヒント」を検証。
- **リスク**: 小〜中。既存テストは残し両方緑を条件にする。3aは2.5の後なので、既存テストは既に`start_paused`化済み。

**rev6: `pending_wake`の取り込み(ユーザー決定、根拠eecba351、§0 rev6)**
- 背景: eecba351が、試行中(`retry_attempt_in_flight`)に届いたネットワーク復帰のwakeが落ちてネットワーク復帰に数分気づかない
  不具合を受けて`pending_wake`を追加した。rev4まではこの`epoch`/`in_flight`/`pending_wake`の三つ組を3b/3cの再評価の判断材料に
  していたが、履歴がその判断材料を既に満たしているので、wakeに関わる遷移は3aで移す。
- `pending_wake`/`retry_attempt_in_flight`を書く箇所(確認済み、`orchestrator.rs`本番コード、main `b413c2ac`):
  `on_connected`(`:534-535`、両方クリア)、`handle_unexpected_disconnect`(`:779-802`、`reconnect_loop_active && pending_wake`なら
  `Action::Suppress { wake_reconnect_loop: true }`で、`:839-843`で`reconnect_wake.notify_one()`)、再接続ループのネットワークwake分岐
  (`:1026-1036`、試行中なら`pending_wake = true`)、tick分岐(`:1090-1091`、`:1114-1117`、`due || pending_wake`で試行開始)。
- 3aで追加で移す遷移: 上表の`ReconnectWake{epoch}`(試行中でなければ試行開始Effect、試行中なら`pending_wake`を立てる)と、
  `AttemptConnected`/`AttemptDisconnected`/`AttemptFailedSync{epoch}`の`retry_attempt_in_flight`・`pending_wake`の更新、
  tick時の「`pending_wake`なら試行開始」の判断。tickの**会計**(`elapsed`/`tick_count`による`due`の計算、`woke_early`分岐)は
  3b/3cの範囲として再評価に残し、3aではshellが計算した`due: bool`を`ReconnectTick{epoch, due}`に載せて渡す。
- 追加の不変条件(proptest): 「ループ動作中(`reconnect_loop_active`)に届いた`ReconnectWake{epoch}`(現行epoch)は、試行中でも
  失われない: その後の試行結果(`AttemptDisconnected`/`AttemptFailedSync`)のapplyか次の`ReconnectTick`のapplyで、
  必ず試行開始かwake通知のEffectになる」。非現行epochの`ReconnectWake`/`ReconnectTick`はStateを変えない(§2.2必須プロパティ)。
- 範囲外: `resume_client.rs`の再アタッチwake(報告ではe221d2ec、a1293255、773f807cの3件。`notify_waiters`/`notify_one`という
  通知プリミティブの意味の問題)はreducerでは捕まらない種類なので3aに含めず、Step 2.5と同じ`start_paused`のshellテストで扱う
  (報告の評価をそのまま採用。どのStepに割り当てるかは決めていない)。

### Step 5(rev6で再評価から昇格): `resume_loop.rs`の判断を`ResumePlanner`へ

- **経緯**: rev4までは「証拠を見て再評価」の1つで、判断材料は「既存の`start_paused`9本+純粋helperで足りない不具合が出たか」だった
  (§6末尾の旧再評価表)。Step 9の報告で、この基準を履歴が既に満たしていることが分かり、ユーザーが3aの直後へ昇格させた(§0 rev6)。
- **根拠(報告より)**: `isekai-pipe/src/resume_loop.rs`の判断ロジック(分類D)の修正が約12件。Epic N-3のUnknownSession give-up方針が
  同日に3回直し直された(cc5fb926 → 71292e68 → dbb80d56、各修正が前の修正の方針を訂正)。Epic N-4のBUSY_OTHER_SESSION再試行期限も
  3回(75d08a39 → fd32ce11 → 3d5e0da5、時間演算)。ほか03224b11(resume windowの超過が`Ok`を返しwrapperが自動再接続しない、
  結果の分類)、204d8f59(pumpの失敗をLocal/Remoteに分類していない)、a266f1f3(一部、15秒の猶予なしに再接続通知)、
  857f6ae6 D-4(jitter欠落)。報告によれば、このファイル最初の`start_paused`テスト(f335aa51)より後の修正の大半はレビューか
  本番で見つかったもので、それらのテストでは見つかっていない(「テストで捕まえられたか」は報告もOPINIONとしている)。
- **現状(確認済み、§1.2)**: `Instant::now`はファイル全体で22箇所、本番コード(`:2110`の最初の`#[cfg(test)]`より前)では13箇所。
  既に`start_paused`テストと多数の純粋helper(`reconnect_notify_due(disconnected_at, now)`等)がある。`ResumePlanner`という型は
  現コードに無く、本Stepで新設する名前(確認済み)。
- **決定**: give-up判定・再試行期限・再接続通知の猶予・失敗分類という、上の不具合が集中した判断を、§2.1の形のreducer
  (`ResumePlanner`、`*_fsm.rs`命名に従い置き場所はPRで決める)へ移し、`pure_modules.toml`に登録する。時間は§2.2の形
  (`now: Millis`の刻印と、token付きタイマーEffect)でのみ入れる。13箇所の`Instant::now`は、shellの刻印関数1箇所に集約する。
  移行は判断ごとに分けたPRで行い、各PRで該当する過去の修正(上のコミット)の挙動をproptest/表テストで固定する。
- **得られるCI検証**: give-up・期限・猶予の判断を、非単調な`now`列(§2.2必須プロパティ)と任意のサーバー応答列でproptest。
  過去3回ずつ直し直された2つの方針(UnknownSessionのgive-up、BUSY_OTHER_SESSIONの期限)を、境界値を含めて不変条件として固定する。
- **リスク**: 中。`resume_loop.rs`は接続耐性の中核で、既存の`start_paused`9本と純粋helperがすでに多くの期待値を持っている。
  既存テストは残し、両方緑をマージ条件にする。`ConnectOutcomeClass`の分類(§4.2、Step 12の網羅表)は変えない。
- **前提**: Step 0(純粋性検査)。3aと触るファイルは重ならない(順序はユーザー決定による優先度)。
- **ロールバック**: 判断ごとにPRを分けるので、PR単位でrevertする。

### Step 4: `pool.rs`のidle timerをtoken付きEffectへ

- `release`はrefcountが0になったら`ArmIdleTimer{key, token: idle_generation, after: idle_grace}`を返し、shellが
  満了時に`IdleTimerFired{key, token}`を戻す。削除判断(`refcount == 0 && idle_generation == token`)はreducer側。
- **得られるCI検証**: PR #124の参照モデルに「タイマー発火」操作を加え、release→attach→release→古いタイマー発火の
  世代競合を任意順で検証。§2.2必須プロパティ(stale token無視)もここで満たす。
- **リスク**: 低。**Step 2.5の後に行う**(2.5も`pool::release`のspawn先を変えるため、round 2 m-R2-7)。
  rev6で6+7の後へ移した(履歴上の`pool.rs`の不具合は#120の1件だけで、#124のモデルテストで固定済み、§0 rev6)。
  2.5の後ならshell側も`start_paused`で配線テストできる。

### Step 6(+7): backoff/jitterの統一と回復ループ本体の共通化(rev6でStep 7を統合)

- 依存関係はmanifestで確定済み(Q4回答): `isekai-ssh/Cargo.toml`は`isekai-pipe-core`を非optionalで依存し、
  `isekai-pipe-core/Cargo.toml`は`isekai-transport = { path = "../isekai-transport" }`(optional/feature指定無し)。
  よって`isekai-ssh`は既に`isekai-transport`をリンクしており、`native/mux/mod.rs:124-130`の重複理由
  (「isekai-ssh's binary otherwise doesn't link against」)は陳腐化している。`cargo tree`は不要。
- `isekai-ssh`の2つの`ReconnectBackoff`を`isekai_transport::backoff::BackoffPolicy`(RNG注入)に置き換え、
  陳腐化したdocを削除する。
- **`Cargo.lock`が変わる**(round 2 m-R2-2): 推移的には既にリンクされているが、`isekai-ssh/Cargo.toml`に
  `isekai-transport`の**直接**依存を足すと`Cargo.lock`の`isekai-ssh`パッケージの依存リストに1行増える。
  §7の最小lockfile手順でCIに生成させ、`lockfile-drift`(required)を通す。
- **値は変えない**。特に`RECONNECT_STABLE_THRESHOLD`は`reconnect_backoff.rs:73`が200秒、
  `native/mux/mod.rs:156`が60秒で意図的に異なり、`reconnect_backoff.rs:64-71`が「既知のfollow-up」と記録している。
  60秒側の修正は挙動変更であり別PR(round 1 m-4)。
- **得られるCI検証**: isekai-ssh側の2コピーがseed付きで決定論テスト可能になる(`BackoffPolicy`自体は既に
  seed付き`StdRng`テストを持つので、新規の利得はこの2コピー分に限られる)。
- **リスク**: 低。

**rev6: 旧Step 7(isekai-ssh回復ループの共通化)の統合**
- 根拠(報告より、§0 rev6): a266f1f3(redeploy+retryが1回しか走らない、ユーザー観測の「クラッシュのような終了」)と
  857f6ae6 D-4(jitter欠落)は、どちらも同じ不具合を重複した2コピーで別々に直す必要があった。
- 内容は旧再評価表の記述のまま: 残る重複はループ本体2つ(`wrapper.rs::run_ssh_with_connect_failure_recovery`、
  `native/connect.rs::drive_connect_recovery`)と`MAX_LIGHTWEIGHT_RETRIES`(`wrapper.rs:594`、`native/connect.rs:382`)のみ
  (`RedeployGate`/`reset_budget_if_stable`は共有済み、`reconnect_backoff.rs:111-128`)。`RedeployGate::due()`は
  `tokio::time::Instant::now()`を内部で読むが`start_paused`下で既に決定論的。移す場合は「jitter付き期限を一度だけ解決する」
  意味(`reconnect_backoff.rs:121-128`)を保つこと。
- PR列: backoff統一(上記)→ ループ本体と`MAX_LIGHTWEIGHT_RETRIES`の共通化、の順に分ける。値と挙動は変えない。
- Step 12と同時に進めない(どちらも`wrapper.rs`/`native/connect.rs`を変える)。Step 12を先に行い、その網羅表テストを
  ループ共通化の回帰検査にする(§6)。
- **リスク**: 低〜中(ループ共通化はUnix/Windows両経路の挙動保存が要る。既存の`drive_connect_recovery`のfakeテストと
  `wrapper.rs`のテストを両方緑で保つ)。

### Step 8a′(8aを置換): Rustが接続エッジを明示的に通知する

- 問題(確認済み): `observeConnectionTransitions`(`TerminalTabsViewModel.kt:825-868`)は`prevConnected`という
  Kotlin側ミラー状態でエッジを検出し、非UIの処理(`registerUpstreamFailoverMonitor`、`physicalMultipathHandle`の
  close、`maybeEnsureTmuxTabWindow`、`persistReattachRecord`、`setAiPanelEnabled`)を駆動している。
  入力の`pane.uiState`は`session.state.combine(preConnectError)`(`TerminalTabsViewModel.kt:161`)で
  `StateFlow`(`TerminalSession.kt:119`)由来のためconflateされる。`Connected→Reconnecting→Connected`が収集より
  速いとエッジを両方取りこぼしうる(経路は確認済み、実発生は未計測)。
- 決定: `OrchestratorCallback`(`#[uniffi::export(callback_interface)]`、`lib.rs:1441`付近)に、Rustが出す
  明示的なエッジ通知を追加する。例: `fn on_connection_edge(&self, edge: ConnectionEdge, generation: u64)`、
  `enum ConnectionEdge { Established { host: String }, Lost }`。`generation`は`session_generation`。
- **`Lost`の定義(round 2 N-4 → round 3 R3-1で改訂)**: `Lost`は特定のEventではなく**reducerのphase遷移**で定義する。
  > `edge_open == Some(g)`のまま`phase`をConnectedから他の値(Idle/Connecting)へ動かすapplyは、**同じapplyで**
  > `Lost(g)`を出し`edge_open = None`にする。
  - rev2はEvent単位(`AttemptDisconnected`/`SessionCreated`/別世代の`AttemptConnected`)で定義していたが、
    `apply_network_lost`(`orchestrator.rs:717-722`)はアダプタも世代も経由せず`handle_unexpected_disconnect`を直接呼んで
    `phase`をIdleにする(`:781`)ため、どのEventにも当たらず`Lost`が遅れる(旧セッションの`on_disconnected`待ち)か、
    `last_connect_attempt == None`でループも始まらず旧コールバックも来なければ**永久に出ない**(round 3 R3-1、確認済み)。
  - **phaseを書く全経路**(確認済み、`orchestrator.rs`本番コード): `on_connected`(`:530`、→Connected)、
    `handle_unexpected_disconnect`(`:781`、→Idle。アダプタ経由の切断・`apply_network_lost`・ユーザー`disconnect()`の合流点)、
    `connect_via`(`:868`、→Connecting。再接続ループの試行とフォアグラウンド復帰)、`begin_connect`(`:1226`、→Connecting)、
    フォアグラウンド復帰の同期失敗(`:1464`、→Idle)。**8a′ではこの5箇所すべてを`ReconnectState::apply`経由にする**
    (3aが移すのは`handle_unexpected_disconnect`の1経路だけなので、8a′は「3aでは定義のみ」より大きい)。
    phaseをapply外で書く経路が1つでも残ると、上の定義はそこで破れる。
  - `apply_network_lost`は現行の`session_generation`を付けた`AttemptDisconnected{kind: NetworkLost}`として同じ遷移を通す
    (§6 Step 3aの対応表)。後から旧セッションの`on_disconnected`が同じ世代で届いても、`edge_open == None`なので何も出さない。
  - `begin_connect`/`connect_via`でConnectedのまま新しいセッションを作る経路(`begin_connect`は`phase == Connected`の
    呼び出しを意図的に受理する、`:1209-1214`)は、phaseをConnectingへ動かすapplyそのもので`Lost(old)`が出る。
    `SessionCreated`(`OrchestratorAdapter::new`の`session_generation += 1`と同じ臨界区間でapply)は、phase遷移を伴わない
    経路が将来できた場合の保険として残す(edgeが開いていれば`Lost(old)`)。
  - **Effectの受け渡し(round 4 m-R4-4)**: `OrchestratorAdapter::new`(現在は`Self`だけを返す、`:458-465`)と
    `connect_via`(`:867-871`)はロック下でapplyを行うので、そのapplyが返したEffect(`Lost(old)`等)を**呼び出し元へ返し**、
    呼び出し元がロック解放後に公開する(§2.4-2。ロック下でコールバックを呼ばない)。具体的には`new`は
    `(Self, Vec<ReconnectEffect>)`、`connect_via`は`Result<Vec<ReconnectEffect>, SshError>`のような形にする(正確な型はPRで決める)。
  - `AttemptConnected{generation}`: `edge_open == Some(generation)`なら**何もしない**(同一世代の`on_connected`重複に対する
    冪等性、round 3 m-R3-2)。`edge_open == Some(old)`(old ≠ generation)なら先に`Lost(old)`、続けて`Established(generation)`。
    `None`なら`Established(generation)`。
  - 旧世代はreducerの`edge_open`が保持しているので、「アダプタ生成で世代が進む前に旧世代を捕捉する」要件は構造的に満たされる。
  - **不変条件(proptest)**: 任意のEvent列について、(1)出力されたエッジ列は「各`g`について`Established(g)`は高々1回、
    その後`Established(g'>g)`より前に`Lost(g)`が正確に1回」、(2)「applyの前後で`phase`がConnectedから離れ、かつ
    apply前に`edge_open`がSomeだったなら、そのapplyの出力に`Lost`が含まれる」。
  - **shell側の検証(proptestでは証明できない部分)**: reducerのproptestは「reducerが正しい」ことしか示さず、
    「shellが全経路でreducerにEventを渡している」ことは示さない。そこで、Step 2.5の`start_paused`化した
    orchestratorテストとして**退出経路ごとに1本**、`Established(g)`の後に`Lost(g)`が届くことを確認する:
    (a)ユーザー`disconnect()`、(b)トランスポートエラー(`on_disconnected`)、(c)network-lost debounce満了、
    (d)Connected中の手動`connect_*`、(e)Suspended後のフォアグラウンド復帰による`connect_via`、
    (f)切断→再接続ループの試行成功。
    - **(e)のセットアップ手順(round 4 m-R4-5)**: フォアグラウンド復帰の`connect_via`は
      `was_suspended && !reconnect_loop_active && phase != Connecting`のときだけ走る(`orchestrator.rs:1407-1411`)。
      `Lost`を出すのは**Connectedのまま**このパスに入る場合だけなので、手順を固定する:
      (1)接続してConnected(`Established(g)`を確認)、(2)`notify_did_enter_background`で`Quiescing`、
      (3)`notify_background_budget_expired`または`notify_memory_warning`で`Suspended`、
      (4)**切断を起こさずに**(phaseはConnectedのまま)`notify_will_enter_foreground`、
      (5)`Lost(g)`が届き、続いて新世代の`Established(g+1)`が届くことを確認。
      途中で切断が起きてIdleになっていると、Idle→Connectingの場合(`Lost`無し、既に(b)/(c)で出ている)を
      検証することになり空振りする。
- **順序(round 2 m-R2-1)**: 汎用の連番publisherは導入しない(§2.4-4)。`Established(g)`は`on_connected`(`:525-544`)の
  中で`Connected`公開の**直後に同じ呼び出し箇所から**、`Lost`はphaseを動かしたapplyのEffectとして、そのapplyの
  呼び出し元から出す。`handle_unexpected_disconnect`(`apply_network_lost`経路を含む)では`Reconnecting`/`Disconnected`公開と、
  `begin_connect`では`Connecting`公開(`:1239`)と同じ呼び出し箇所になる。**`connect_via`経路では状態公開を伴わない**
  (round 4 m-R4-4で訂正、確認済み: `connect_via`は現状`Connecting`を公開しない、`:867-871`)。この経路では`Lost(old)`が
  単独で出て、Kotlinの`uiState`は新セッションが報告するまでConnectedのまま残る(既存の挙動で、本Stepでは変えない)。
  異なるスレッド間の順序(再接続ループの`Reconnecting`公開等)については何も主張しない。
- Kotlinは受け取ったら既存の処理を呼ぶだけにし、`prevConnected`を削除する。Kotlin側で重複排除・エッジ判定をしない。
- **実装者・偽実装の一覧(round 2 m-R2-3、確認済み)**: callback interfaceへのメソッド追加は全実装者の対応が必須。
  - Kotlin: `android/src/main/kotlin/tools/isekai/terminal/session/TerminalSession.kt`、
    `android/src/test/kotlin/tools/isekai/terminal/FakeSshGateway.kt`、
    `android/src/androidTest/kotlin/tools/isekai/terminal/FakeSshGateway.kt`
  - Swift: `ios/Sources/IsekaiTerminalCoreLogic/CallbackIngress.swift`、
    `ios/Sources/IsekaiTerminalCore/TerminalSessionController.swift`、
    `ios/Tests/IsekaiTerminalCoreLogicTests/KeyManagerTests.swift`、
    `ios/Tests/IsekaiTerminalCoreTests/SshVerticalSliceTests.swift`
  - Rustテスト: `rust-core/src/test_callbacks.rs`(`ForwardingOrchestratorCallback`)、
    `rust-core/src/orchestrator.rs`のテスト内`RecordingCallback`
  - `ios-*`チェックはrequiredでない(`main-branch-protection.md`)ため、Swift側の対応漏れは緑のままマージされうる。
    **このPRでは`ios-logic-linux-check`の緑をマージ条件にする**。
- UniFFIバインディングは`regenerate-uniffi-bindings.yml`で再生成し、Kotlin本体1ファイルと、Swiftの
  `ios/Sources/IsekaiTerminalCoreLogic/generated/`の3ファイル**と対応する`.sha256`サイドカー3本**
  (`isekai_terminal_core.swift.sha256`、`isekai_terminal_coreFFI.h.sha256`、`isekai_terminal_coreFFI.modulemap.sha256`、
  確認済み)をすべてコピーする(`uniffi-binding-regeneration.md`)。
- **得られるCI検証**: エッジの発火条件(特に手動再接続・フォアグラウンド復帰で旧世代の`Lost`が出ること)が
  Rust側のproptestとorchestratorテストで検証可能になり、conflationによる取りこぼしが構造的に無くなる。
- **リスク**: 中(UniFFI変更、両プラットフォーム同時対応)。

### Step 8b: `TerminalSession`のUI状態をreducerへ(rev6: 再評価へ移した)

- **rev6**: Step 9の報告でエッジ取りこぼしの事例が0件、Android側の不具合は配線漏れとプラットフォーム由来が主だったため、
  ユーザー決定で再評価へ移した(§6末尾、§0 rev6)。以下は行う場合の内容(rev4までのまま)。

- `TerminalSession.kt`の`_state.update`22箇所を、`ConnectionStateMapper`を拡張した`UiMsg`→`TerminalUiState`の
  純粋な畳み込みに寄せる。**対象はUI表示状態のみ**(`rust-ssot.md`の例外条項)。
- **得られるCI検証**: `android-unit-test`(required)内でRobolectric無しの高速テスト。
- **リスク**: 低。

### Step 9(rev5新規): 不具合履歴によるStep順序の再ランク付け(調査のみ)

- **内容**: 過去の不具合履歴(コミット・issue・PR・`PLAN.md`等の記録)を、本ADRの各Stepが「防げた/検出できた」かで
  分類し、Stepの期待効果を実績ベースで並べ直す材料を作る。コード変更もPRも伴わない調査。
- **成果物**: `scratchpad/defect-history-rank.md`(rev5執筆時点では作成中だったため、rev5では結果を先取りしなかった)。
  **rev6で完了**し、ユーザーがその結果に基づく順序変更を決めた。採用した根拠の要約は§0 rev6にある。
- **位置付け**: 結果は§6の順序行・D2(3b/3c/5/7の再評価)の**入力**にすぎない。順序を変える場合は、ユーザーが決めた上で
  本ADRのamendment(rev6以降)として記録する。報告が順序変更を勧めても、amendmentが入るまでは§6の順序行が有効。
  scratchpadはリポジトリ外なので、amendmentで採用した根拠は要約してADR本文に書き写す(参照だけで済ませない)。
- **得られるCI検証**: なし(調査)。
- **リスク**: なし。履歴の分類は事後判断なので「このStepがあれば防げた」は推測を含む。報告側で確認済み/推測を区別することを
  amendment時の採用条件にする。
- **前提**: なし(いつでも並行可)。
- **ロールバック**: 不要(成果物はADRの外)。

### Step 10(rev5新規): shell競合の検証層(差分テスト+有界網羅探索の評価)

**位置付け**: Step 2aの集約proptest(I-a〜I-j)は「reducerが正しい」ことを示すが、「shell(`AttachRuntime`+`engine/mod.rs`の
ファサード)がreducerと同じことをしている」ことと、「reducerが表す単一集約の状態空間を**網羅的に**見た」ことは示さない
(proptestはランダム探索)。Step 10はこの2つの隙間を埋める。どちらも**テストのみ**で本番の挙動は変えない。

**10-1 差分テスト(採用、2aの後)**
- 手本はPR #124の`pool.rs`モデルベーステスト(`src/pool.rs:613`の`mod model_based`、`:832-838`の
  `pool_matches_reference_model_under_random_operations`、確認済み): 任意の操作列を実装と参照モデルの両方に流し、
  各操作の後で観測値を突き合わせる。
- Step 10では「実装」=実shell、「参照モデル」=純粋な`ServeAggregate`(2a)。操作の語彙は
  `Hello`/`Activate`/データストリーム切断によるpark/`Resume`/`Cancel`/時間経過(`tokio::time::advance`)+sweep/target TCP切断。
  実shellは`#[tokio::test(start_paused = true)]`相当の仮想時刻で、targetはStep 1.5と同じローカル`TcpListener`。
- 観測値の射影: `AttachRuntime::established_lease_for(id)`の有無とlease、indexでのparked/active/unresumable、
  RESUMEの結果種別(`ResumeGranted`/`RequestPreempt`/`UnknownToken`)。モデル側は同じ射影を`ServeAggregate`から計算する。
- proptest+非同期: `proptest!`の各ケース内で`tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true)`
  のランタイムを作って`block_on`する形を想定(マクロの組み合わせ方は [EXT]、PRで確認)。
- **限界(明記)**: 操作は**逐次**に流すので、await点での非同期タスクのインターリーブは探索しない。差分テストが捕まえるのは
  「shellがEventを渡し忘れる/Effectを取り違える/in-lock effectを別臨界区間で実行する」等の**翻訳誤り**。
  2a以前のTOCTOU(sweep×RESUME)のようなインターリーブ由来の競合は、2aの設計(単一ロック・単一apply)で
  構造的に除いた上でStep 1.5の特性テスト(反転後)が見る。

**10-2 集約の有界網羅探索(評価、採否はQ15)**
- 対象: 2aの`ServeAggregate`(`AttachArbiter`+`SessionIndex`)。純粋なので、小さい有界な宇宙(例: session_id 2個、
  クライアント2本、`now`は数点の離散値、Event列長≤N)で到達可能状態を**全列挙**し、各状態でI-a〜I-jを検査できる。
- 候補と評価:
  - **手書きの有界BFS(既定案)**: `#[cfg(test)]`内に、Event語彙を列挙して`apply`を適用し、訪問済み集合で重複を
    除くだけの探索器を書く(依存追加なし、`Cargo.lock`不変)。必要な前提: 状態を重複判定できること。
    `AttachArbiter`は現在`#[derive(Debug, Default)]`のみで`Clone`/`Eq`/`Hash`を持たず、内部が`HashMap<SessionId, AttachState>`
    (`attach_arbiter.rs:154-158`、確認済み)なので、`Clone`の導出と、`BTreeMap`へ射影した正規形(またはテスト用の
    fingerprint関数)が要る。`SessionIndex`は2aで`BTreeMap`なのでそのまま使える。
  - **stateright [EXT]**: 状態機械/actorモデルの有界モデル検査crate。BFS/DFS・`always`/`eventually`性質・反例経路の
    出力を持つ。`ServeAggregate`を`Model`として包めば上と同じことができ、eventually性質(§4.2の有界到達性)も書ける。
    コスト: dev-dependencyの追加で`Cargo.lock`が変わる(§7の最小lockfile手順、`lockfile-drift`required)、推移依存の
    規模は未確認 [EXT]、状態型に`Clone + Hash + Eq`が要る点は手書きと同じ。**§8「新しいFSM/effectフレームワーク
    crateの導入」はしないとの関係**: staterightはテスト専用dev-dependencyで本番コードの形を規定しないので、
    §8が禁じた「フレームワーク」には当たらないと解釈する。ただしこの解釈もQ15でユーザーが確認する。
    既定案は「まず手書きBFS、反例経路の可読性やeventually性質が要るとわかったらstateright」。
  - **loom [EXT](この集約には適用しない、と評価)**: loomは`std::sync`/atomic/スレッドを`loom::sync`等に差し替えた
    コード(`#[cfg(loom)]`のshim)を、メモリモデルを含めて全インターリーブで実行する道具。適用できない理由:
    1. 2a後の`ServeAggregate`自体は純粋でロックを持たない。並行性はshell側にしか無い。
    2. そのshellの集約ロックは`tokio::sync::Mutex`(`attach_runtime.rs:28,136`、確認済み)で、並行性の単位は
       スレッドではなく**asyncタスクのawait点**。loomはtokioランタイム/非同期mutexをモデル化しない(tokioは内部で
       loomを使うが、下流crate向けのloom対応ビルドは提供していない、という理解 [EXT])。
    3. `parking_lot`はloom非対応なので、`parking_lot::Mutex`を使う箇所は`cfg(loom)`で`loom::sync::Mutex`へ差し替える
       自前shimが要る(`try_lock`等のAPI差の吸収を含む [EXT])。
    loomが意味を持ちうるのは、同期ロック+スレッドで組まれた小さな部品だけ。候補は`pool.rs`(`parking_lot::Mutex`、
    `src/pool.rs:20,46`、確認済み)の`try_lock`失敗→false-alive(§9の既存記述)だが、`release`が`RUNTIME.spawn`の
    タイマーを含むので、Step 4でタイマーをEffect化した後の純粋部分にしか当てられない。採否はQ16(既定: 導入しない)。
- **得られるCI検証**: 10-1でshellの翻訳誤りを任意操作列で検出。10-2(採用時)で有界宇宙内の全到達状態について
  I-a〜I-jを証明(ランダム探索では漏れうる深い組み合わせを含む)。
- **リスク**: 10-1は低(テストのみ、ただし実ソケットを使うので実行時間はStep 1.5並み。nextestの`retries`対象には入れない、§7)。
  10-2は状態爆発: 宇宙のサイズはCI時間(目安: 数十秒以内)に収まる範囲に固定し、PRで実測値を書く。
- **前提**: Step 2a(`ServeAggregate`とファサード)。10-1はStep 1.5の`AttachRuntime`込みテスト基盤を再利用する。
- **ロールバック**: テストのrevertのみ。stateright採用時はdev-dependencyの削除と`Cargo.lock`の再生成(§7)。

### Step 11(rev5新規): Effect/callback列の不変条件検査(テストのみ)

- **問題**: Step 8a′の「shell側の検証」は退出経路ごとに1本ずつの専用テスト(a)〜(f)で、それ以外の既存orchestratorテスト・
  e2eテストは、途中で出たcallback列が不変条件を満たしているかを見ていない。reducerのproptest(8a′)は「reducerが正しい」
  ことしか示さないので、shellの経路漏れは専用テストが無いシナリオでは見えない。
- **決定**: テスト用の記録器が受け取ったEffect/callbackを、**テスト内メモリ上の列**として持ち、テスト終了時に純粋な
  検査関数`check_trace(&[TraceEvent]) -> Result<(), TraceViolation>`(`#[cfg(test)]`の共有モジュール)を通す。
  - 記録点: `orchestrator.rs`テスト内の`RecordingCallback`(`:1952`、確認済み)、`rust-core/src/test_callbacks.rs`の
    `ForwardingOrchestratorCallback`、2a後の`isekai-pipe` engineテスト(interpreterへのテスト用フック)。
  - 記録器の`Drop`で検査する場合は、テスト自体のpanic中に二重panicしないよう`std::thread::panicking()`なら検査を省く。
  - 既存のorchestratorテストすべてが自動的に不変条件の検査にもなる(個々のテストのassertは変えない)。
- **不変条件の初期セット**:
  - T1(8a′): 各`g`について`Established(g)`は高々1回。テストが切断で終わるなら、`Established(g)`ごとに`Lost(g)`が正確に1回。
  - T2: `Lost(g)`は、先行する`Established(g)`がある場合にだけ出る。
  - T3(2a後のengine): 同じ`(id, lease)`の`Discard`は高々1回(interpreterの冪等性に頼らずreducerが1回しか出さないこと)、
    `StartRelay`は同一leaseに高々1回(Step 1の不変条件のshell側の確認)。
  - **順序についての注意(推測、8a′の実装で確定)**: §2.4-4により、別スレッドから出るcallbackの順序は保証しない。
    `Established(g)`(`on_connected`の呼び出し元)と`Lost(g)`(別スレッドの切断経路)は、それぞれロック解放後に公開
    されるので、callback到着順が`Lost(g)`→`Established(g)`に逆転しうる。そこでT1は**世代ごとの回数**として検査し、
    順序は同じ呼び出し箇所から出る組(§2.4-4、例: `Connected`公開→`Established`)に限って検査する。順序の逆転を
    テストで観測した場合は、それを§2.4-4の「逆転が実害として観測された」証拠として扱い、連番publisherのStepを
    別途提案する(本Stepでは導入しない)。
- **秘密情報(§3-3)**: `TraceEvent`はvariant名・世代/lease・公開状態のタグだけを持ち、`SshConfig`/`SshAuth`/
  `LastConnectAttempt`を参照しない。`Established{host}`のhost文字列も記録しない(検査に不要)。
- **Q12との違い**: Q12(§2.6)は「実機で記録したEvent列をCIでreplayする」構想で、既定案は「実施しない」のまま
  変えない。Step 11は本番コードに記録機構を入れず、ファイルにも書き出さず、テストが自分で起こしたEffect/callbackを
  その場で検査するだけ。
- **得られるCI検証**: 既存シナリオテストすべてで、8a′のエッジ契約とengineのEffect契約が自動検査される。
  専用テストを書いていない経路のshell漏れを拾える。
- **リスク**: 低。既存テストが新たに赤くなった場合、それは既存の契約違反の発見なので、検査を緩めずに原因を調べる
  (ただし上記の順序注意に該当するものは回数検査へ落とす)。
- **前提**: Step 3a(`ReconnectEffect`)と8a′(`on_connection_edge`)。T3は2aの後で足す。Step 2.5で
  orchestratorテストが`start_paused`化済みであること。
- **ロールバック**: 検査呼び出しの削除のみ。

### Step 12(rev5新規): always-connectsの網羅性テスト(`run_connect`の`Err` → `ConnectOutcomeClass` → 回復動作)

- **現状(確認済み)**:
  - `isekai-pipe connect`の失敗記録は`write_connect_outcome_for_wrapper`(`isekai-pipe/src/connect.rs:564`。
    `always-connects.md`は`main.rs`と書いているが、実体は`connect.rs`)。`run_connect`(`connect.rs:681`)の`Err`は呼び出し元が
    無条件に記録し(`:528`)、panicはDropガードが`"run_connect panicked"`として記録する(`:440-450`)。
  - 分類は同関数内のインライン`if/else`(`:574-584`): `isekai_transport::StaleTrustSignal`(`isekai-transport/src/error.rs:108`)→
    `StaleTrust`、`resume_loop::MidSessionDisconnectSignal`(`resume_loop.rs:237`)→`MidSessionDisconnect`、それ以外→
    `Unreachable`。`resume_loop::ParentGoneSignal`(`resume_loop.rs:301`)は**意図的に何も書かない**(`:565-567`、親の`ssh(1)`が
    既に居ないので回復しても意味が無い)。`ISEKAI_INTENT_ID`が無い・runtime dirが取れない場合も書かない(`:568-573`。
    wrapperは`ISEKAI_INTENT_ID`を必ず設定して起動する、`isekai-ssh/src/wrapper.rs:1135`・`native/child_stdio.rs:74`)。
  - `ConnectOutcomeClass`は`StaleTrust`/`Unreachable`/`MidSessionDisconnect`/`Unknown`(`isekai-pipe-core/src/outcome.rs:65-114`)。
  - 読み手の判断は`decide_connect_failure_recovery`(`isekai-ssh/src/wrapper.rs:1007-1013`)で、`MidSessionDisconnect`以外を
    `Some(_)`のワイルドカードで扱う。Unix経路(`wrapper.rs:707-782`付近)とWindows native経路
    (`native/connect.rs:436`)が同じ関数を呼ぶ。加えて「`Unknown`かつremote commandあり」なら自動再試行しないガードが
    2箇所に重複している(`wrapper.rs:727`、`native/connect.rs:454`)。
- **決定(テスト中心、本番の変更は挙動保存の抽出だけ)**:
  1. `isekai-pipe`: 分類部分を純粋関数`classify_connect_error(&anyhow::Error) -> Option<ConnectOutcomeClass>`
     (`None`は`ParentGoneSignal`のときだけ)に抽出し、`write_connect_outcome_for_wrapper`はそれを呼ぶだけにする。
     **表テスト**: 各marker型 × `.context()`の包み深さ(0/1/2) × 組み合わせ(例: ParentGoneと他markerの同居は
     ParentGone優先=現状の判定順)と、markerを持たない任意のエラー→`Unreachable`、panicガードのメッセージ→`Unreachable`。
     else分岐が`Unreachable`なので分類関数は構造的に全域だが、表テストは**新しいmarkerや早期returnを足したときに
     表の更新を強制する**ためのもの。
  2. `isekai-ssh`: `decide_connect_failure_recovery`の`Some(_)`をvariantごとの明示armに書き換える(現在の結果と同じ値を返す、
     挙動保存)。Step 7aの後なら`#[deny(clippy::wildcard_enum_match_arm)]`を付ける。`Unknown`+remote commandのガード条件は
     純粋な述語関数に抽出して2箇所から呼ぶ(ループ本体2つの共通化はStep 6+7の範囲で、ここでは行わない。rev6)。
  3. **網羅表テスト**: 全`ConnectOutcomeClass`(テスト内の`all_classes()`はvariantごとの明示`match`で作り、variant追加で
     コンパイルが落ちるようにする)× `should_bootstrap` × remote commandの有無 → 期待する`ConnectFailureRecoveryAction`。
     この表の上で`always-connects.md`の性質を明示的にassertする: 「記録されたどのクラスについても、`should_bootstrap = true`
     かつ非冪等ガードに当たらない限り、結果は`RebootstrapAndRetry`か`RetryConnectLightweight`であり、
     `NoRecoverableSignal`にはならない」。例外は表の行として列挙し、理由を書く: `ParentGoneSignal`(記録なし→
     `NoRecoverableSignal`)、`should_bootstrap = false`(ユーザーの明示的なopt-out→`AutoBootstrapDisabled`)、
     `Unknown`+remote command(非冪等コマンドの再実行防止)。
  4. `outcome_summary`(`wrapper.rs:870-882`)は既に全variantを明示`match`している(確認済み)ので変更しない。
- **`ConnectOutcomeClass`の分類そのものは変えない**(§4.2)。新しいクラスや判定の変更は本Stepの範囲外。
- **得られるCI検証**: `always-connects.md`の「`run_connect`が失敗する限り回復シグナルが書かれ、wrapperがそれを自動回復に
  つなげる」が表として固定される。新しい`ConnectOutcomeClass`を足したPRは、`decide_connect_failure_recovery`の明示armと
  網羅表の行を書かないとコンパイル/テストが落ちる(`always-connects.md`が今は文章で求めている`outcome_summary`への追記と
  同じ強制が、回復判断側にも効く)。
- **限界**: 「`run_connect`の**全**`Err`経路が呼び出し元の記録点を通る」こと自体は、呼び出し構造(`:528`と`:440-450`)に
  依存しており表テストでは証明しない。`run_connect`の呼び出し箇所が増えた場合に気づけるよう、呼び出し箇所が1つで
  あることを確認する仕組み(テスト or Step 0のスクリプト)を入れるかはPRで決める(推測: スクリプトの方が安い)。
- **リスク**: 低。抽出と明示armへの書き換えは挙動保存で、既存の`decide_connect_failure_recovery_*`テスト
  (`wrapper.rs:2614-2672`)がそのまま回帰検査になる。
- **前提**: なし(独立)。7aの後なら7aのlint属性も付ける。
- **ロールバック**: PRごとrevert。

### Step 13(rev5新規): Android/iOSのcallback契約golden

- **問題**: Step 8a′で`OrchestratorCallback`に`on_connection_edge`を足すと、Kotlin(`TerminalSession.kt`)とSwift
  (`CallbackIngress.swift`)は「受け取ったら既存処理を呼ぶだけ」になる(§6 Step 8a′)。この転送が正しいこと
  (エッジ1回につき対応処理が1回、Kotlin/Swift側で重複排除・エッジ判定をしていない)は、Rust側のテストからは見えない。
- **決定**:
  - **golden生成(Rust)**: Step 8a′の退出経路テスト(a)〜(f)とStep 11の記録器を使い、各シナリオのcallback列
    (メソッド名・`ConnectionEdge`のvariant・`generation`・公開状態のタグ)をJSONのgoldenとしてリポジトリにコミットする
    (置き場所の案: `rust-core/tests/golden/callback_contract/<scenario>.json`、Q17)。Rustテストは生成結果とコミット済み
    goldenの一致をassertし、不一致なら`<scenario>.actual.json`を書いて失敗する(更新手順は§7)。
  - goldenに入れるのは**順序が保証された列**だけ(§2.4-4、Step 11の順序注意)。別スレッド由来で順序が揺れうる部分は
    世代ごとの射影(回数)として記録する。
  - **Kotlin replay**: `android/src/test/`(JVM、`android-unit-test`=required)のテストがgoldenを読み、Kotlin側の
    `OrchestratorCallback`実装へ順に流して、`Established`ごとに`registerUpstreamFailoverMonitor`等の処理が1回、`Lost`ごとに
    close系の処理が1回呼ばれることをfake越しにassertする。goldenは`rust-core/`配下に置くので、`android-test-check.yml`の
    pr-path-gate(`^rust-core/`を含む、確認済み)によりgolden変更PRでもKotlin側が走る。gradleテストからのファイル参照方法
    (作業ディレクトリ基準の相対パス等)は [EXT]、PRで決める。
  - **Swift replay**: `ios/Tests/IsekaiTerminalCoreLogicTests/`のテストが同じgoldenを`CallbackIngress.swift`へ流す。
    `ios-logic-linux-check.yml`がLinuxで実行するのは`IsekaiTerminalCoreLogic`の`swift test`だけ(`:119-124`、確認済み)で、
    `IsekaiTerminalCoreTests`(`SshVerticalSliceTests.swift`等)はこのジョブでは走らないので、replayはLogic層に置く。
    同ワークフローのpull_requestのpathsは`rust-core/**`を含む(`:24-31`、確認済み)のでgolden変更PRで発火する。
    SwiftPMのリソース宣言か`#filePath`基準の読み込みかは [EXT]、PRで決める。
  - **マージ条件**: `ios-*`チェックはrequiredでない(`main-branch-protection.md`)ため、Swift側が赤のままマージされうる。
    **Step 13のPRと、以後goldenを変更するPRでは`ios-logic-linux-check`(job `build-and-test`)の緑をマージ条件にする**
    (8a′と同じ扱い)。required化そのもの(protection設定の変更)は本Stepでは行わない(`main-branch-protection.md`のPhase 6の
    判断事項)。
  - UniFFI公開APIは変えない(goldenはテスト用データで、UniFFIの型ではない)。バインディング再生成は不要。
  - goldenは秘密を含まない(§3-3。hostはテスト用の固定値か記録しない)。
- **得られるCI検証**: Rustのcallback契約と、Kotlin/Swiftの転送実装の食い違いが、`android-unit-test`(required)と
  `ios-logic-linux-check`で検出される。Rust側の変更でcallback列が変わるとgoldenの更新が必要になり、PRの差分で契約の変化が
  可視化される。8bを再評価の結果行う場合は、その`_state.update`書き換えに対する安全網にもなる(§6、rev6で8bは再評価へ)。
- **リスク**: 低〜中。goldenの更新手順(CI artifact経由)の手間と、Kotlin/Swiftテストからのファイル参照の仕組み作り。
  goldenを過剰に細かくすると無関係な変更でも更新が要るので、射影は契約に必要な項目に限る。
- **前提**: Step 8a′(`on_connection_edge`とその実装者)。Step 11の記録器(推奨)。
- **ロールバック**: テストとgoldenの削除のみ。

### 再評価(証拠を見てから決める): Step 3b/3c(残り)・8b

rev6で、Step 5は昇格(3aの後)、Step 7はStep 6へ統合、3b/3cのうち`pending_wake`はStep 3aへ取り込み、8bを再評価へ移した。

| Step | 内容 | 再評価の判断材料 |
|---|---|---|
| 3b/3c(残り) | 再接続ループのtick会計(`elapsed`/`tick_count`による`due`の計算)・`woke_early`分岐を`ReconnectState`へ | Step 2.5で決定論化した既存テストと、`pending_wake`を含めた3aのproptestの後に、tick会計まわりの未検証インターリーブや不具合が実際に残っているか |
| 8b | `TerminalSession.kt`のUI状態のreducer化(§6 Step 8b) | UI状態(`_state.update`22箇所)由来の不具合が観測されたか。行う場合は8a′ → 8bの依存と、13の後に行う推奨順序に従う |

---

## 7. CIの変更

- **proptest**
  - dev-dependencyを`isekai-pipe`(Step 1)、他crateは必要になったStepで追加。指定はルートcrateと同じ
    `default-features = false, features = ["std"]`(`rusty-fork`/`tempfile`を引き込まず`Cargo.lock`差分を抑える)。
  - **RNG**: proptestは`rand 0.9`(`Cargo.lock`、確認済み)、`BackoffPolicy`等は`rand 0.8`。strategyは`u64`のseedを
    生成し、テスト内で`rand::rngs::StdRng::seed_from_u64(seed)`(0.8)を作って渡す。proptestの`TestRng`を直接渡さない。
  - 既定ケース数(256)のまま。reducerは小さく実行はミリ秒単位なのでrequired checkの時間への影響は無視できる(旧Q6を削除)。
  - **反例の保存**: CI専用運用ではローカルで`proptest-regressions/`が生成されない。`rust-core-test-check.yml`に
    `if: failure()`で`**/proptest-regressions/**`を`actions/upload-artifact`するstepを追加する(現状upload stepは無い、
    確認済み)。ダウンロードした`cc`行をソースツリーの`proptest-regressions/`にコミットする。artifactが取れない
    場合はログに出る失敗seedから`cc`行を起こす。
- **`Cargo.lock`**: ローカルでcargoを実行しない。既存の`regenerate-lockfile.yml`(workflow_dispatch、artifact出力、
  確認済み)を使う。ただし同workflowは`cargo generate-lockfile`で**全依存を再解決**するため、無関係な更新が
  混入しうる。最小差分モード(入力`mode: minimal`)を同workflowに追加する。候補は
  `cargo update --workspace`(ワークスペースメンバーの依存宣言の変化だけを反映し、既存の外部crateの版は
  動かさない、という文書化された用途 [EXT])、次点で`cargo metadata --format-version 1 > /dev/null`
  (`--locked`無しなら既存lockを保ったまま必要なエントリだけ追加する [EXT])。ダウンロード後に`diff`で
  意図した追加だけであることを確認してコミットする。`lockfile-drift`(required)がこれを検証する。
  対象: Step 1(`isekai-pipe`へのproptest)、Step 6(`isekai-ssh`への`isekai-transport`直接依存)、§2.2の`Millis`
  (`isekai-protocol`は既に`serde`を持つので変化しない見込み)。
- **nextest**: 純粋reducerのテストは`.config/nextest.toml`の`retries`対象に**入れない**
  (`dead_pooled_handle_is_not_reused_after_silent_blackhole`を意図的に外している既存方針と同じ)。
- **cargo-mutants(round 1 M-8で再設計)**
  - 既存`cargo-mutants-check.yml`(workflow_dispatch)は変更しない。週次は**別workflow**
    `cargo-mutants-pure.yml`(`schedule` + `workflow_dispatch`)にする(`schedule`では`inputs.*`が空になるため [EXT])。
  - `pure_modules.toml`の各エントリ(package、file、テストフィルタ)からmatrixを生成し、
    `cargo mutants -p <package> -f <file>`に**そのモジュールのテストだけを走らせるフィルタ**を渡す
    (例: `--test-tool=nextest`+`-E 'test(/^<module>::/)'`。フラグの正確な書式は [EXT] で実装時に確認)。
    フィルタ無しでは1ミュータントごとにパッケージ全テスト(実時間テスト・QUIC e2e含む)が走り、安くならない。
  - `isekai-terminal-core`のエントリは`build-isekai-pipe-musl.sh`と`--copy-target true`が必要(既存workflowと同じ)。
  - 結果はartifact、required化しない。
- **clippy純粋性ジョブ / import allowlist**: §2.3。`rust-core-test-check.yml`内の軽量jobとして追加し、当面required化しない。
- **(rev5)Step 7aのinterpreter網羅性lint**: 新しいジョブは作らず、上の純粋性ジョブの中で走る(関数単位の
  `#[deny(clippy::wildcard_enum_match_arm)]`はソース側の指定なので、ジョブのコマンドラインは変えない)。
  allowlistスクリプトに`[[interpreter]]`登録の検査(`if let`/`let .. else`/`matches!`によるEffectの選り分けの拒否)を足す。
  D3のrequired化はこのジョブ単位なので、7aのためにprotectionのcontextを増やす必要は無い。
- **(rev5)Step 10**: 差分テストは通常の`cargo nextest`内で走る(新ジョブ不要)。実ソケットを使うが、nextestの`retries`対象には
  入れない(上のnextest方針と同じ。flakeは隠さずに直す)。有界網羅探索は宇宙のサイズをCI時間に収まる範囲に固定する。
  stateright(Q15)を採る場合はdev-dependency追加なので、上の最小lockfile手順で`Cargo.lock`をCI生成する。
- **(rev5)Step 13のgolden**: Rustテストがgoldenとの不一致時に`*.actual.json`を書いて失敗する。`rust-core-test-check.yml`の
  `if: failure()`のartifact upload step(上の反例保存と同じstep)の対象に`**/golden/**/*.actual.json`を加える。
  ダウンロードした`.actual.json`を内容確認の上で`.json`としてコミットする(ローカルでcargoを実行しない運用のため)。
  Kotlin/Swift側のreplayテストは既存の`android-unit-test`(required)・`ios-logic-linux-check`(非required、Step 13の
  PRと以後goldenを変更するPRでは緑をマージ条件にする)で走る。新しいworkflowは作らない。

---

## 8. 対象外(non-goals)

- アプリ/プロセス全体の単一Redux store。
- russh/noq/VTE/リレーのバイトポンプのsans-io化。
- L1(仮想時間+in-memory UDP)、turmoil/madsimの採用(`RUNTIME`注入だけはStep 2.5で先行)。
- 新しいFSM/effectフレームワークcrateの導入、`timed-fsm`の拡張。
- Kotlin/Swift側に接続判断のreducerを作ること(旧8aは撤回)。
- Step 8a′以外でUniFFI公開APIを変えること。
- Room/`AppDatabase.kt`の変更(どのStepも触らない)。
- 既存純粋モジュールの改名・形の統一のためだけのリファクタ。

---

## 9. 検討した代替案

- **reducer化せず`start_paused`だけで行く**: 既に13ファイル(`isekai-ssh`/`isekai-pipe`/`isekai-transport`/
  `quicmux`/`isekai-netmon`/ルートcrate)で実績があり、Step 2.5で`orchestrator`/`pool`にも広げる。配線と
  タイミングにはこれで十分。ただし書いたシナリオしか検証できず、任意のインターリーブ(epoch×in_flight×pending_wake、
  fencing×park×sweep)の網羅はreducer+proptestが必要。→ 役割分担として両方採る。
- **turmoil/madsim**: L1と同じ大投資。L1保留に従い比較しない。
- **loom**: `pool.rs`の`try_lock`失敗→false-alive(L0-3)のようなロック粒度の競合には有効かもしれない。(rev5: Step 10-2で評価。`ServeAggregate`のshellには適用できず、`pool.rs`の同期部分のみ候補。採否はQ16)
- **既存FSM/Redux系crate**: 個別評価はしていない。自作`timed-fsm`があり、標準形はさらに単純(plain `apply`)なので
  外部crateを足す利益は薄い。
- **reducer専用crate**: 依存グラフ上の純粋性は自動では保たれず型移設コストが大きい。§2.3の2層検査で代替。
- **grepによる純粋性検査(rev0案)**: 偽陽性3/7、推移的不純を検出できず、別名で迂回可能(round 1 M-2)。却下。
- **Clock trait注入**: replay不能な隠れ入力。却下(§2.2)。
- **`TimedStateMachine`を標準にする(rev0案)**: 既存の模範`AttachArbiter`やStep 4がtoken付きEffect形であり、
  作法が分裂する(round 1 M-3)。legacy容認に格下げ。
- **Kotlin側に`reduceTransition`を置く(rev0の8a)**: `rust-ssot.md`違反をreducerにするだけで、conflationも解決しない。却下。

---

## 10. Open Questions

### 解決済み

- **Q1(`now`の型)**: `Millis(u64)`(shellエポックからの経過ミリ秒、`isekai-protocol`に置く)。shellごとに1箇所の
  刻印関数で`tokio::time::Instant`から変換し、刻印は集約ロック取得後(`apply_with`)。差は`saturating_sub`のみ(§2.2)。
- **Q2(`timed-fsm`の拡張)**: 拡張しない。legacy形として既存4利用者に限り容認(§2.1)。
- **Q3(2つのDropバックストップ)**: 独立したfire-and-forget spawnで順序保証なし、同時検証テストなし(§4.1)。
  Step 2aでも両方残す。
- **Q4(`isekai-ssh`→`isekai-transport`依存)**: manifestで確定。既に依存している(Step 6)。
- **Q6(proptestのCI時間)**: 非問題として削除(§7)。
- **Q7(arbiterとtableを1集約にするか)**: する(Step 2)。原子性(TOCTOU・admit・activate→insert窓・`Rejected`)が根拠。
- **Q8(`session_state.rs`は純粋か)**: 推移的に不純(`terminal.rs:1113`、加えて`terminal.rs:516`の`Instant`フィールド、
  不純な`session.rs`からの`to_cell_data`/`SCROLLBACK_LIMIT`参照)。Step 0から除外し、登録の前提条件はStep 0に列挙。
- **Q5(Kotlinエッジ検出)**: Rust側の明示callbackへ(Step 8a′)。
- **Q11(`Lost`の発火点、連番案との比較)**: 「`handle_unexpected_disconnect`が合流点」という前提は誤り
  (`begin_connect`/フォアグラウンド復帰が旧世代の切断通知を経由せずConnectedを離れる、round 2 N-4)。
  `reconnect_fsm.rs`内の`edge_open`によるRust側エッジreducerを採り、`Lost`は特定Eventではなく
  「Connectedから離れるphase遷移」で定義する(round 3 R3-1で`apply_network_lost`経路の漏れを受けて改訂、Step 8a′)。`ConnectionPublicState`に連番を足す案は
  採らない: 連番は「通知が抜けたこと」をKotlinに検出させるだけで、何を閉じるべきかの判断がKotlin側に戻り、
  `rust-ssot.md`に反する。
- **Rejected由来のリーク(round 2 N-3)**: ユーザー決定(a)によりStep 2aで修正(I-g)。
- **旧D5(rev5、Step 9の報告で§6の順序を変えるか)**: ユーザー決定でrev6の順序に変更(§0 rev6)。D5の番号は新規事項に振り直した。

### 未解決(ユーザー判断待ち、この一覧がすべて)

本文の他の箇所に散らばっていた「後で決める」事項もここに集約した。どれもApproveをブロックしない
(各Stepの着手時またはPR内で決められる)。表の「既定案」は、ユーザーが何も指定しなければ実装時に採る案。

| # | 何を決めるか | いつ決めるか | 既定案 |
|---|---|---|---|
| **A** | **このADRをApproveするか** | 今 | — |
| Q9 | Step 2aのpreempt/repark順序の具体化: `SessionIo`の`preempt`/`reparked`通知を集約ロック外で行うときの順序(`mod.rs:1400-1412`付近)、中継ループ側の`preempt`受け渡し(`mod.rs:1821-1822`)の付け替え | Step 2aのPR | preempt後は起床/タイムアウトに関わらず`ResumeRequested`を1回再送し、再び`RequestPreempt`なら拒否(round 4 m-R4-2、Step 2a本文) |
| Q10 | `path_health_fsm.rs`が使う`path_health.rs`の`classify_path_health`/`has_zero_response`と`noq::PathStats`を、項目単位で許可するか純粋モジュールへ移すか | Step 0のPR | 3つとも項目単位で許可(コード移動なし) |
| Q12 | §2.6のEvent記録とreplayを実施するか(§3-3の秘密情報規則が前提) | Step 3aの後 | 実施しない(必要が生じたら再検討) |
| Q13 | crate rootの公開値型を項目単位許可のまま運用するか、`src/public_state.rs`へ移設するか | Step 3aと`session_state.rs`登録の時点 | 項目単位許可のまま。許可リストが10項目を超えたら移設 |
| Q14 | Step 0のclippy設定で、解決できない`disallowed-*`パスの警告をどう抑止するか([EXT]) | Step 0のPR | エントリごとの`allow-invalid`等で抑止できるか試し、不可ならcrateごとの`clippy.toml`に分ける |
| D1 | Step 2c(admission時の容量予約でunresumableを到達不能にする)を行うか | Step 2bの後 | **決定済み(Approve時): Step 2bの後で実施する**(当初の既定案「観測まで延期」は採らない) |
| D2 | Step 3b/3c(残り)・8bを行うか(§6末尾の再評価表)。**rev6で範囲を変更**: Step 5は昇格、Step 7はStep 6へ統合、3b/3cのうち`pending_wake`はStep 3aに取り込み済みなので、3b/3cはもう全体が延期ではなく、残りのtick会計・`woke_early`だけが対象 | Step 2.5・3aの後(8bは8a′・13の後) | 再評価表の判断材料で未検証のインターリーブや不具合が見つかった場合だけ行う |
| D3 | 純粋性検査ジョブ(clippy+allowlist)とmutants週次ジョブをrequired checkにするか | Step 0のジョブ導入後 | **決定済み(Approve時): 純粋性検査ジョブのみrequired化する。mutants週次ジョブは非required。** 実施時は`main-branch-protection.md`に従い、ジョブに明示的な`name:`を付け、`gh api -X PUT .../branches/main/protection`の`checks[].context`へ同時に追加する(追加を忘れるとcontextが恒久pendingになる)。誤検知でmainが詰まらないよう、まず数回のCI実績で安定を確認してからprotectionに追加する |
| D4 | `native/mux/mod.rs`の`RECONNECT_STABLE_THRESHOLD`(60秒)を200秒に揃える挙動変更を行うか | Step 6とは別PR | 本ADRの範囲外。`reconnect_backoff.rs:64-71`の既知follow-upとして別途 |
| Q15(rev5) | Step 10-2の有界網羅探索を、手書きBFS(依存追加なし)で行うかstateright(dev-dependency)で行うか。stateright採用時、§8「新しいFSM/effectフレームワークcrateの導入」をしないとの関係を「テスト専用の検証道具は対象外」と解釈してよいか | Step 10のPR | 手書きBFS。反例経路の可読性やeventually性質(§4.2)が要るとわかった時点でstaterightを再提案する |
| Q16(rev5) | loomを導入するか(Step 10の評価では、tokio非同期mutexで組まれた`ServeAggregate`のshellには適用できない。候補は`pool.rs`の同期部分のみ) | Step 4の後 | 導入しない(`pool.rs`のロック粒度の競合が実害として観測されたら再検討) |
| Q17(rev5) | Step 13のgoldenの置き場所・形式・射影項目、Kotlin/Swiftテストからの参照方法 | Step 13のPR | `rust-core/tests/golden/callback_contract/<scenario>.json`、射影はメソッド名・edge variant・generation・公開状態タグのみ |
| Q18(rev5) | Step 7aで`[[interpreter]]`登録をどこまで広げるか(`decide_connect_failure_recovery`のような「Effectではないが網羅性が不変条件を担うenum」の`match`を含めるか)、lintが捕まえない形の検出をスクリプトで行うか | Step 7aのPR(Step 12の判断も同様) | interpreter関数は全登録。非Effectのenumは`always-connects.md`に関わるもの(Step 12)だけ属性を付ける。`if let`等はスクリプトで拒否する |
| D5(rev6) | **「callback/フラグを実装したが配線していない」不具合クラス(Step 9報告の分類W)への対処**。報告によればリポジトリ全体で多く(例: 2a07a5e3、8baa49d8、db3a6d87、ce214ef5、ae8ed13b、119205f6、e1c370e5、7b10472a)、reducer・純粋性検査・原子性テストのどれも「Eventが一度も届かない」ことは捕まえないため、本ADRのどのStepも対象にしていない。**提案のみ(未決定)**: CIで「`OrchestratorCallback`の各メソッドと、プラットフォームから呼ばれるべきUniFFI公開メソッド(例: `notify_did_enter_background`等のライフサイクル入口)が、それぞれKotlin/Swift側に1つ以上の呼び出し箇所(またはそれを呼ぶテスト)を持つ」ことを検査する。実装手段の候補はStep 0のallowlistスクリプトと同じ簡易字句解析、またはStep 13のgolden replayの拡張。対象メソッドの列挙方法(UniFFI生成物から取るか手書きリストか)と、意図的に片方のプラットフォームだけで呼ぶメソッドの例外の書き方も未定 | ユーザーが決める(本ADRの範囲に入れるか、別ADRにするかを含む) | 既定案なし(ユーザー判断待ち) |
