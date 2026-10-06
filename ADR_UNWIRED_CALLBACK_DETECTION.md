# ADR: 「実装したが配線していない」不具合クラス(分類W)の構造的検出

- **Status**: **Draft(2026-10-06)**。rev2。敵対的レビューround 1(`scratchpad/adr1-review-round1.md`、Opus)の指摘F1〜F12と、
  round 2(`scratchpad/adr1-review-round2.md`、同じレビュアー。判定は「意図のレベルで収束、blockerなし」)の指摘R2-1〜R2-9を
  反映済み(§0 rev1/rev2)。追加のレビューroundは行わない。ユーザー判断待ちの事項は§10にまとめてある。
- **起点**: `ADR_FUNCTIONAL_CORE_EFFECTS.md` §10の未解決事項D5(rev6)。同ADR Step 9の不具合履歴報告
  (`scratchpad/defect-history-rank.md` §3。scratchpadはリポジトリ外なので、本ADRが採用した根拠は§1に書き写す)が
  「callback/フラグ/メソッドを実装し単体ではテストもしたが、本番の呼び出し元が無い/プラットフォーム側の
  実装者が何もしない」という分類Wが大きく、FCIS ADRのどのStepも対象にしていないと指摘した。本ADRはその対処を
  **FCIS ADRとは別のADR**として提案する(採否はユーザーが決める、§10 U1)。
- **対象**:
  - rust-core: `isekai-terminal-core`(`src/`)、`isekai-pipe`、`isekai-pipe-core`、`isekai-netmon`、`isekai-ssh`、`isekai-bootstrap`
  - 生成物(読むだけ): `android/src/main/kotlin/uniffi/isekai_terminal_core/isekai_terminal_core.kt`、
    `ios/Sources/IsekaiTerminalCoreLogic/generated/isekai_terminal_core.swift`
  - Android: `session/{TerminalSession,NetworkPathMonitor,AppExecutor,AndroidAppExecutor,HostKeyChecker}.kt`、
    `TerminalTabsViewModel.kt`、`TerminalScreen.kt`(`TerminalScreenActions`)、`TerminalHostScreen.kt`、
    テストの`{DumbAppExecutor,FakeSshGateway,TerminalTabsViewModelTest,TerminalHostScreenTest}.kt`
  - iOS: `ios/Sources/IsekaiTerminalCore/{TerminalSessionController,TerminalTabsHostView,TerminalView}.swift`
  - CI: `android-test-check.yml`(required `android-unit-test`、既存)、`rust-core-test-check.yml`の`purity-check`/`test-windows`job、
    新設`.github/workflows/wiring-lint.yml`(案)
  - 新設(案): `android/src/test/kotlin/tools/isekai/terminal/WiringContractTest.kt`、`scripts/check-wiring-lint.py`、
    `scripts/wiring_lint.toml`
- **拘束される既存ルール/ADR**: `.claude/rules/rust-ssot.md`、`.claude/rules/uniffi-binding-regeneration.md`、
  `.claude/rules/main-branch-protection.md`(required 5本)、`.claude/rules/parallel-worktree-agent-operations.md`
  (§2: `isekai-ssh/src/native/`はLinuxでdead_code、§4: 診断ツールはワークスペースマニフェスト起点)、
  `ADR_FUNCTIONAL_CORE_EFFECTS.md`(Step 0・1・7a・8a′・11・13、§10 D5)、ローカルbuild/test禁止(検証はGitHub Actionsのみ)。
- **表記**: 「確認済み」はmainの作業ツリー(rev0は`54dd8eae`、rev1は`535eac85`)でコード・`git show`・CIログを読んで確認した事実。
  「推測」は導出のみで未実行。「[EXT]」はこのリポジトリ外の一般知識(ツール・言語の仕様等)で、実装時に確認が要るもの。
  「概算」は本ADR執筆時のscratchpadの簡易スクリプト(`scratchpad/wirecheck.sh`、型解決なしの字句grep)の値。

---

## 0. 改訂履歴

### rev2(2026-10-06)— round 2レビューの反映

| 指摘 | 重さ | 変更 |
|---|---|---|
| R2-1 到達を検査するのは`OsEvent`だけで、`UiAction`/`Command`/`Query`(約30メソッド)は分類表に行があるだけ。新しいOSイベントを`Command`に分類すれば通る(db3a6d87の形の再来)。また「UI由来の入口はJVMで駆動できない」は誤り: `TerminalHostScreenTest`(required job内、Robolectric+Compose、15テスト)が本物の`TerminalHostScreen`+`TerminalTabsViewModel`を`FakeOrchestrator`で描画している | MAJOR | §3(a′)に3つの規則を追加: (1)名前規則(`notify`で始まるメソッドは`OsEvent`/`UiAction`/`NotUsedOnAndroid(ref)`のみ)、(2)`UiAction`をCompose台本(同じ記録Proxy、`performSemanticsAction(OnClick)`)で観測、(3)`Command`/`Query`は既存のconnect系テストでの観測か本番呼び出し元シンボルの明記を必須に。「JVMで駆動できない」の記述を訂正(確認済み) |
| R2-2 `notifyUpstreamHealthDegraded`はmultipath+upstream failoverのプロファイルでしか発火しない | MINOR | §5 Phase 1に台本の前提(`ISEKAI_PIPE_QUIC_MULTIPATH`+`enableUpstreamFailover=true`、pane毎の`simulateWifiUpstreamBroken(index)`)を明記。分類変更でassertを「直す」ことを禁止 |
| R2-3 ファサードクラスには27関数以外に公開static(`uniffiEnsureInitialized`・`use`)がある。checksumは`IntegrityCheckingUniffiLib`の反射で数えられる | MINOR | §3(a′)を訂正: 2関数を許可リストに、全数照合は`IntegrityCheckingUniffiLib`の`declaredMethods`(初期化しない`Class.forName`)で行い、ファイル読み込みの[EXT]を削除 |
| R2-4 字句ガードの誤検知・すり抜け: `orchestratorFactory`の既定値は本番が依存、`TerminalScreen.kt:300 onUserActivity`はdata class外、改行でのすり抜け | MINOR | §3(g)で`orchestratorFactory`を明示的に許可(no-opではなく本番の既定実装)、ガードの範囲を`TerminalScreenActions`のdata class本体に限定(`onUserActivity`も既定値を外す案と比較して前者を採用)、パラメータリストを閉じ括弧まで正規化してから照合 |
| R2-5 iOS字句レポートはcontroller内部から直接`orchestrator.X(`を呼ぶ経路(`TerminalSessionController.swift:244`の`notifyNetworkPathChanged`)を誤警告する | MINOR | 透過扱いを「同名forwarder(`func X(...) { orchestrator.X(`)」だけに限定し、同ファイル内の他メソッドからの`orchestrator.X(`は呼び出し元として数える |
| R2-6 「最後のOS hop」は同じJVMスイートで一部検査可能(`NetworkPathMonitorTest`・`AndroidAppExecutorDownloadTest`が既にRobolectricで動いている) | MINOR | §7に「必須ではないが後から追加可能」として記載 |
| R2-7 coreの「19件」は警告数で、項目数は約14 | MINOR | §1.3(4)・§2・§3(b)・§5を「警告19件・項目約14」に訂正。coreのAndroid専用cfgは`lib.rs:68,77`のみでLinuxでのdenyに偽陽性が無いことを追記 |
| R2-8 Phase 4の事実誤り: `isekai-bootstrap`の内部依存は`isekai-protocol`だけではない、argv生成は`install_script.rs`のみ、`isekai-protocol`へのproptest追加は`Cargo.lock`を変える | MINOR | §3(h)を訂正(内部依存6個、確認済み)。往復テストを`isekai-pipe`(proptest導入済み)に置くことにし、§6の「`Cargo.lock`を変えない」と整合させた |
| R2-9 `-p`は`--workspace`とfeature統一の範囲が違い、共有依存が再checkされうる | MINOR(OPINION) | §3(b)に追記し、Phase 3のPRで追加時間を実測することにした |

### rev1(2026-10-06)— round 1レビューの反映

最大の変更: rev0の中核だった「TOML台帳+正規表現で呼び出し箇所を数え、`wired`を検査する」案(旧§3(a)/(f))は、
**同名のforwarderがあれば必ず通る**(F1)ため、Androidについては撤回した。代わりに、required `android-unit-test`内で
型解決付きで動く**配線契約テスト**(生成interfaceの反射による全メソッド分類+記録Proxyによるライフサイクル台本)を採る(§2, §3(a′))。

| 指摘 | 重さ | 変更 |
|---|---|---|
| F1 正規表現の`wired`はforwarder(`TerminalSession.kt:576`の`fun notifyDidEnterBackground() = orchestrator.notifyDidEnterBackground(0u)`)の宣言・内部呼び出しで必ず満たされ、決して落ちない | MAJOR | 旧(a)/(f)のAndroid部分を撤回(§3(a)に評価として残す)。§3(a′)配線契約テストを新設し、Phase 1をこれに差し替え。副分類×機構表のW1を更新 |
| F2 事例の欠落(ee34304a・8d480d01・3965e322・e8ed36ee ほか) | MAJOR | 4件を検証して§1.2に追加(17件)。新副分類**W8**(本番で常に偽の条件の背後にある経路)を追加。8baa49d8の行を「警告は消えたが機能は死んだまま(ee34304a)」に訂正し、Goodhartの例として§1.4に明記。64fc32cf/3cd005e9(iOSのno-opスタブ慣習)・68ce1256/7de46a7a(呼び出し元ゼロのAPI削除)は件名のみ確認し補足として記載 |
| F3 dead_codeの基準値は数えられる(約214件)、Windows抑止は存在しない、allowは6箇所、vendor crateもworkspace member | MAJOR | CIログを自分でも取得して再集計(§3(b))。Phase 3をcrate単位の段階導入(`isekai-terminal-core`から)に書き換え、`isekai-ssh`はLinux/Windows両ログの積集合で扱う(U4の既定を変更)。誤記(「native/は抑止済み」「allowは3箇所」)を訂正 |
| F4 既存のdead_code警告は無視されており、その中にW分類の残骸がある | MAJOR | 残骸を§1.3に列挙。Phase 3の「件数をjob summaryに出すだけ」の段を削除し、1PRでcoreの19件を片付けてから即deny |
| F5 両プラットフォームで呼び出し0は4個ではなく6個(`push`・`publish`も) | MAJOR | §1.3を訂正。rev0の簡易スクリプト自体が汎用名・コメントのFNを起こした事実として§3(a)に記載 |
| F6 同名メソッドの所属不明、constructorの見落とし、生成物のパース脆弱性 | MINOR | 配線契約テストは反射でinterface単位に列挙するので所属・同名問題は消える。全数の照合にUniFFIの`checksum_{func,method,constructor}_*`(27/63/3、確認済み)を使う(§3(a′)) |
| F7 Phase 2は一度きりの掃除で再発防止が無く、platform→Rust側の既定no-op(`NetworkPathMonitor.start`、`TerminalScreenActions`)を見ていない | MAJOR | Phase 2に字句ガード(`[no_default_lambdas]`登録ファイルで関数型パラメータの`= {`を拒否)を追加し、対象を3ファイルに拡大(§3(g))。U9は回答済み(iOSコントローラには注入closureが無い、確認済み)として解決済みへ |
| F8 `pending`+警告だけの`review_by`は形骸化する | MAJOR | `pending`状態を廃止。状態は「配線済み(テストで証明)」か「このプラットフォームでは使わない(理由=コミット/ADR節への参照必須)」の2つだけ(§3(a′))。`mirrored`は書面の例外(ADR節/ルール)への参照を必須に |
| F9 「導入PR 710aecf2で検出できた」は不正確(710aecf2はrust-coreのみ、Kotlinバインディングはc848eb03で別コミット) | MINOR | §3(a)を「バインディング再生成コミットc848eb03で検出できた」に訂正し、当時はbranch protection前の直push運用だった点を追記 |
| F10 型解決付きの代替(記録Proxyテスト、Periphery、Qodana、閉じたイベントenum)が評価されていない | MAJOR | 記録Proxyテストを採用(§3(a′))。Periphery・Qodana・閉じたイベントenumを§3/§8で評価 |
| F11 W5(最大の副分類)を対象外にした理由が弱い。argvは往復proptestで機械化できる | MINOR | §3(h)を往復テスト案で書き直し、Phase 4として採用(ただしe1c370e5は捕まらない、と範囲を限定) |
| F12 走査範囲(ios/Appのテストターゲット、`testDebug`)、ファイル配置の慣習、U2の昇格基準、U7の着手条件、TOMLのマージ衝突 | MINOR | 字句検査の範囲を許可リスト方式に(§3(g))。配置は`scripts/`(リポジトリ直下、`check-room-migrations.sh`と同じ横断系の慣習)に統一し理由を記載。U2は「誤検知0」ではなく字句ガードのFP基準に、U7(Phase 4条件)は配線契約テストで代替されたので削除。TOML台帳自体を廃止したので衝突問題はKotlinの分類表に移る(§9) |

### rev0(2026-10-06)

初版。git履歴から分類Wの具体例を収集・再分類し、検出手段6候補+追加2候補を評価し、段階的導入を提案した。

---

## 1. 背景・問題

### 1.1 不具合クラスの定義

**分類W(wiring omission)**: ある機能の部品(UniFFI公開メソッド、callback、注入lambda、設定値、レジストリ登録関数)が
実装され、多くの場合その部品単体のテストも緑だが、**本番の経路でそこへ到達する呼び出し/伝搬が無い**。結果として
機能は「黙って不活性」になり、エラーもログも出ない。reducer・純粋性検査・原子性テスト(FCIS ADRの全Step)は
「届いたEventを正しく処理する」ことを検証するもので、「Eventが一度も届かない」ことは原理的に見えない。

### 1.2 証拠(git履歴、確認済み)

各コミットの本文と差分を読んで分類した。「導入」は`git log -S`で該当シンボルが最初に現れたコミット、または修正コミット
本文が名指しする導入コミット。TTD(time-to-discovery)は導入から修正/削除コミットまでの日数。

| 修正 | 日付 | 何が配線されていなかったか | 導入 | 発見経路 | TTD | 副分類 |
|---|---|---|---|---|---|---|
| db3a6d87 | 2026-07-28 | `SessionOrchestrator`の前景/背景4メソッド(`notify_did_enter_background`等)がAndroid本番コードから一度も呼ばれず、`app_foreground`が永久にForeground → tmux通知が常に抑制 | 710aecf2(2026-07-13、本文が名指し。Kotlinバインディングはc848eb03で同日) | 実機テスト | **15日** | W1 |
| ce214ef5 | 2026-07-25 | `TerminalSession`の`onNotify` lambda(既定値`{ _, _, _ -> }`、修正前`TerminalSession.kt:85`)をViewModelが渡さず、ctl Notifyが既定no-opに吸われていた | 8d21ba52(2026-07-24、`-S onNotify`) | Codexレビュー(PR #36) | 1日 | W2 |
| 7b10472a | 2026-07-12 | ネットワーク断/復帰を`TabState.session`(primary paneのみ)へ流しており、split pane側に届かない | bfa2ba2f(2026-07-12) | 本文に記載なし | <1日 | W3 |
| 2a07a5e3 | 2026-07-24 | `ensure_tmux_tab_window`が`TmuxLocator`を`TMUX_LOCATOR_REGISTRY`へ一度も登録せず、#57/#58/#59(通知フック・ctl-socket伝播・scrollback backfill)が本番で全てno-op | 03125a44 / 2340c67e(2026-07-19) | Opusレビュー(H1) | 5日 | W4 |
| 8baa49d8 | 2026-07-25 | `set_tmux_backfill_locator`がどこからも呼ばれず、backfillが常にスキップ。**この修正でsetterは配線され警告は消えたが、機能は死んだまま**(下のee34304a) | f356766b(2026-07-19) | **rustcの`dead_code`警告**(本文が明記) | 6日 | W4 |
| **ee34304a** | 2026-08-09 | tmux scrollback backfill機能を削除。`fetch_tmux_scrollback_history`は`TmuxTargetKind::Pane`のロケータしか受け付けないが、唯一の呼び出し元が使うロケータは`ensure_tab_window`が両分岐で常に`Window`を構築するため、「実装当初から一度も実行されたことのない到達不能コード」(本文) | f356766b(2026-07-19) | 整理作業(WU-R7) | **21日**(8baa49d8の後さらに15日) | **W8** |
| 142fcbde | 2026-07-27 | 既定transport(isekai-pipe QUIC)が呼び出しごとに`AppPaneId::generate_process_local()`で使い捨てIDを作り、タブの安定ID`shared.app_pane_id`を素通ししていなかった(「タスク#59 follow-up」と既知コメントあり) | 03125a44(2026-07-19) | 実機ログ | 8日 | W5 |
| e1c370e5(=880b0e3a) | 2026-07-17 | `#@isekai resume-grace`(クライアント設定)がリモートの`isekai-pipe serve`起動引数に一切渡らず、サーバは既定resume windowで破棄 | 未確認 | ユーザー報告(ノートPCスリープ復帰で切断) | 未確認 | W5 |
| 119205f6 | 2026-07-19 | 上の伝搬が`russh_backend`の`LaunchSpec::Relay`アームで漏れた。Directアームは個別分解パターンなのでフィールド追加でコンパイルエラーになったが、Relayは構造体丸ごと受け取りのため素通りした(本文が明記) | e1c370e5(2026-07-17) | Opusレビュー | 2日 | W5 |
| f538dc71 | 2026-07-17 | Windows native経路の`spawn_isekai_pipe_connect`が`ISEKAI_INTENT_ID`/`ISEKAI_PIPE_RUNTIME_DIR`を子へ渡さず、`ConnectOutcome`記録(always-connectsの根幹)がスキップ | 同一作業内 | Codexレビュー | 0日 | W5 |
| 1fb3d5a6(=07b5461b) | 2026-07-18 | `applicationKeypadMode`がKeySequence(打鍵列/マクロ)送信経路へ伝播せず、常にnumeric mode | 同一タスク#43 | Codexレビュー | 0日 | W5 |
| ae8ed13b | 2026-07-19 | native経路のエントリ`native/connect.rs::prepare`がUnix版`wrapper::run`にある`init_verbose`を一度も呼ばず、`log_line_verbose!`が恒久的に無出力 | 未確認(本文「最初から一度も」) | mainのWindows CI回帰 | 未確認 | W6 |
| **e8ed36ee** | 2026-08-09 | UniFFI公開`add_local_forward`/`remove_forward`がAndroid本番UIから一度も呼ばれていなかった(本文「呼び出しはFakeSshGateway.ktのUniFFIテストダブルのみ」)。4dde90fb(2026-07-24)がこれらのe2eテストを足していた=**テストがdead exportにカバレッジを与えていた** | 500b711e(2026-07-02、`-S 'pub fn add_local_forward'` orchestrator.rs) | 整理作業 | **38日** | W1 |
| **8d480d01** | 2026-09-16 | `debug_reconnect`の計測基盤が`cfg!(debug_assertions)`ガードの背後にあったが、Android向けRustはdebug APKでも`--release`でビルドされる(`android/build.gradle.kts:85`、確認済み)ため実機で常にfalse、丸ごとno-op | e89eb2f5(2026-09-16) | Opus実装レビュー(B-1) | 0日 | **W8** |
| **3965e322** | 2026-09-16 | `DebugReconnectLog.setRustLogPath`がUniFFI関数を`Class.forName("uniffi.isekai_terminal_core.Isekai_terminal_coreKt")`+`firstOrNull{name==...}`+無言`runCatching`で呼んでおり、どの失敗でも無言no-op(本文: 8d480d01と同じ観測結果を別の機構で作り直していた) | e89eb2f5と同日 | Opus実装レビュー(N-2a) | 0日 | W1(反射呼び出し) |
| 6ee526d4 | 2026-08-09 | `isekai-pipe`の`datagram_relay`(815行)が`mod`宣言以外どこからも参照されず。モジュール先頭に`#![allow(dead_code)]`(削除前`mod.rs:48`)があり警告も抑止 | — | 整理作業 | — | W7 |
| 8cdb325c | 2026-08-09 | `isekai-transport::dual_path`(555行)が呼び出し元ゼロ。lib crateの`pub mod`+`pub use`なのでrustcは警告しない | — | 整理作業 | — | W7 |

補足(件名のみ確認、本文は未精読): 64fc32cf / 3cd005e9(2026-07-24)はiOS側の`onNotify`実装漏れ・`ensureTmuxTabWindow`の
opt-in引数への追従で、レビュー指摘によればiOSには「UI未実装ならno-opスタブで追従する」慣習がある(W2のiOS版が慣習化している)。
68ce1256(2026-07-26、`rebind_to_fd`)・7de46a7a(2026-08-09、quicmux `ResumeAcceptor`)は「呼び出し元ゼロ」のAPI削除(W7)。

境界例(分類Wに入れない): cba309f1 / 66d4bdf6(2026-07-27)は配線自体はあったが、ロケータ登録より先に
`push_ctl_socket_to_tmux`がほぼ常に完走するレースで永久にno-opになった。順序/原子性の問題(Step 9報告の分類A)として扱う。

### 1.3 現在も残っている「未配線」と残骸(確認済み、main `535eac85`)

**(1) 意図的な未配線**(意図はコミット本文やタスク文書にしか書かれておらず、CIからは区別できない):

| シンボル | Android | iOS | 意図の出典 |
|---|---|---|---|
| `try_claim_tmux_window` / `release_tmux_window_claim`(`tmux_window_claim.rs:16-35`) | 呼び出し0 | 呼び出し0 | 8ced2131(2026-08-23、PR #104)本文「実際の呼び出し配線は別フェーズY-P2で行う」。本ADR時点で44日。その間Androidは独自のKotlinミラー`tmuxClaimedProfileIds`(`TerminalTabsViewModel.kt:418,670,1059,1080`)で排他しており、`rust-ssot.md`上の緊張がある |
| `OrchestratorCallback::on_foreground_resume`(`lib.rs:1518`) | 実装はログのみ(`TerminalSession.kt:393-395`) | ログのみ(`TerminalSessionController.swift:1115-1116`) | 同PR本文「実UIはY-P3」「UX活用は別follow-up、ADR Q10」 |
| `notify_background_budget_expired` / `notify_memory_warning`(`orchestrator.rs:1390,1400`) | 呼び出し0(テストのfakeのみ、`FakeSshGateway.kt:104-105`) | 配線済み(`TerminalTabsHostView.swift:103,117`) | db3a6d87本文「Suspended遷移経路に既知の危険なバグがあるため配線しない(Opusレビュー指摘)」 |
| `quic_transport.rs:283`の`app_pane_id` | — | — | 142fcbdeが「既定transportではないのでスコープ外」と残したタスク#59コメント |

**(2) 両プラットフォームで呼び出し0の公開シンボル(rev1で訂正: 4個ではなく6個)**: 上の2個に加え、
`reattach_grace_window_secs`(`reattach_persistence.rs:64`)、`terminal_unicode_char_bytes`(`lib.rs:607`)、
`DiagnosticEventQueue.push` / `DiagnosticFrameMailbox.publish`(`lib.rs:198,307`付近。呼び出しはRustテストのみ、
iOSの唯一のヒットは`CallbackIngress.swift:39`のdocコメント内の「`push()`」)。後2個はレビュー(F5)の指摘で、rev0の
簡易スクリプトが汎用名(`push`/`publish`は無関係なメソッドにもヒット)とコメントのせいで見落としたもの。
**rev0の試作自体が§3(a)の想定したFNを起こした**。

**(3) プラットフォーム非対称は常態**(概算): 片方だけ0のシンボルが十数個あり、大半は設計上の非対称(Androidは
`TerminalKeyEncoder.kt`でRustのキー変換をミラーする確定方針(f6298d23本文)、iOSは`drainEvents`/`setWakeListener`の
EventQueue方式、`decide_battery_guidance`/`set_ai_panel_enabled`等はAndroid専用機能)。検出機構は例外の宣言方法を
最初から持たなければ使えない。

**(4) 既存の`dead_code`警告の中のW残骸**(確認済み: `purity-check`のCIログ、run 37430493543 / job 112160014484、
ブランチ`step7a-interpreter-lint`、2026-10-06。レビューの集計を本ADR執筆者も同じログを`gh api .../jobs/112160014484/logs`で
取得して再集計した):
- coreの警告19件は項目数では約14(`session.rs:706`はtransportごとにマクロ展開されるimpl内にあり、同じ項目が複数回警告される、レビュー集計)。
- `inject_scrollback_history`が未使用(`src/orchestrator.rs:161`、`src/session.rs:613,706`): ee34304aのbackfill削除の残骸。2026-08-09から警告され続けている。
- `variant RemoveForward is never constructed`(`src/transport/ssh_handler.rs:63`): e8ed36eeの残骸。
- `TmuxTargetKind::Pane`・`TmuxSessionScope::Standalone`がnever constructed(`src/tmux_locator.rs:117,126`): `Pane`はee34304aの根本原因。
  **ee34304a以前にこの警告が出ていたかは未検証**。
- ほか`rebind_manager.rs:154`、`multipath_transport.rs:213,261`、`helper_bootstrap.rs:46`、`resume_client.rs:117`、`session.rs:195`、
  `terminal.rs:950`、`faulty_udp_socket.rs:277`。

### 1.4 まとめ(確認済みの事実から言えること)

- 17件の発見経路: 実機・ユーザー報告3件(db3a6d87、142fcbde、e1c370e5)、mainのCI回帰1件(ae8ed13b)、rustc警告1件(8baa49d8)、
  レビュー7件(ce214ef5、2a07a5e3、119205f6、f538dc71、1fb3d5a6、8d480d01、3965e322)、整理作業4件(ee34304a、e8ed36ee、6ee526d4、8cdb325c)、
  記載なし1件(7b10472a)。実機・整理作業で見つかったものはTTDが長い(8〜38日)。
- 副分類:
  - **W1** 言語境界越しの入口が呼ばれない(platform → Rust): db3a6d87、e8ed36ee、3965e322(反射経由で実質未呼び出し)
  - **W2** callbackは届くがプラットフォーム側の最初のhopで捨てられる(既定no-op lambda・ログのみ・no-opスタブ): ce214ef5(+iOSの慣習)
  - **W3** N個の受け手の一部にしか届かない(fan-out): 7b10472a
  - **W4** Rust内部のsetter/レジストリ登録が呼ばれない: 2a07a5e3、8baa49d8
  - **W5** 値が構造体・argv・env・プロセス境界を越えて伝搬しない: 142fcbde、e1c370e5、119205f6、f538dc71、1fb3d5a6
  - **W6** 並行する2つのエントリポイントの片方だけ初期化が漏れる: ae8ed13b
  - **W7** モジュール丸ごと未配線: 6ee526d4、8cdb325c
  - **W8(rev1新規)** 経路は配線されているが、本番では常に偽の条件の背後にある(到達不能なenum variant、`cfg!(debug_assertions)`): ee34304a、8d480d01
- **Goodhartの実例**: 8baa49d8は`dead_code`警告に従ってsetterを配線し、警告は消えたが、機能はW8のため死んだままだった(ee34304a)。
  「呼び出し箇所がある」「警告が無い」を配線の証明として扱う機構(rev0の`wired`も含む)は、同じ失敗をする。
  **配線の証明は「本番の入口から発火させると末端に届く」ことを観測するテストでしかできない**。これがrev1の設計の前提。
- 単一の機構で全副分類は捕まらない。§3で機構ごとにどの副分類を捕まえるかを評価する。

---

## 2. 決定(提案)

1. **Phase 1 — Android配線契約テスト(W1・W3、§3(a′))**: required `android-unit-test`内に`WiringContractTest`を置く。
   - **分類表**: 生成interface `SessionOrchestratorInterface`・`OrchestratorCallback`のメソッドと生成トップレベル関数を
     **反射で列挙**し、テスト内の分類表(Kotlinの`Map`)にすべてが載っていることをassertする。分類は
     `OsEvent`/`UiAction`/`Command`/`Query`/`NotUsedOnAndroid(ref)`(callbackは`UiState`/`InjectedLambda`/`LogOnly(ref)`)。
     **`pending`は無い**。`ref`はコミットハッシュかADR節への参照で、自由文だけは不可(F8)。
     **名前規則(rev2、R2-1)**: `notify`で始まるメソッドは`OsEvent`・`UiAction`・`NotUsedOnAndroid(ref)`のいずれかにしか分類できない
     (Rust側でプラットフォームの生イベント入口を`notify_*`と命名している慣習を使う)。`Command`/`Query`へ逃がすことはできない。
   - **記録Proxy+ライフサイクル台本**: `java.lang.reflect.Proxy`で`SessionOrchestratorInterface`を包んで呼ばれたメソッド名を記録し、
     `TerminalTabsViewModel`を`DumbAppExecutor`のsimulator(`simulateAppBackgrounded`等、`DumbAppExecutor.kt:125-130`)で
     台本通りに動かし(接続 → 背景 → 前景 → ネットワーク断/復帰 → upstream劣化 → split pane → 切断)、
     **`OsEvent`に分類された全メソッドが全paneで観測された**ことをassertする。
   - **UI由来の入口(rev2、R2-1)**: `UiAction`に分類された全メソッドを、`TerminalHostScreenTest`と同じ形のCompose台本
     (本物の`TerminalHostScreen`+`TerminalTabsViewModel`、同じ記録Proxy)で意味木から`performSemanticsAction(OnClick)`等で発火させ、
     記録されたことをassertする。ライフサイクル台本とは別テストにし、UI側のflakeが配線の結果を隠さないようにする。
     ジェスチャ専用等で駆動できないものは`ref`付きで個別に除外する。
   - **`Command`/`Query`(rev2、R2-1)**: 既存のconnect系テスト等で記録Proxyに観測されるか、分類表に本番の呼び出し元シンボル
     (例: `ConnectionCoordinator.connect`)を書くことを必須にする。後者は字句的にしか確かめられないので弱い。
   - **callback方向**: `InjectedLambda`に分類されたcallbackは、記録用lambdaを全部渡した`TerminalSession`のfakeの`callback`へ
     各callbackを直接呼び、対応するlambdaが発火することをassertする。
   - **全数の照合**: 反射で得た集合の大きさを、生成物のUniFFI checksumシンボル(`checksum_func_*` 27、`checksum_method_*` 63、
     `checksum_constructor_*` 3、確認済み)とinterface単位で照合し、生成器の変更で反射の列挙が黙って縮むのを防ぐ(F6)。
     checksumは`internal object IntegrityCheckingUniffiLib`(生成物`:814`)のメンバーなので、これも反射で数える(rev2、R2-3)。
2. **Phase 2 — 既定no-op lambdaの撤廃と再発防止ガード(W2・W1のUI hop、§3(g))**: `TerminalSession`・`NetworkPathMonitor.start`・
   `TerminalScreenActions`の関数型パラメータから既定値を外し、本番の生成箇所(それぞれ1箇所、確認済み)で明示的に渡させる。
   新設の字句ガード`scripts/check-wiring-lint.py`が、`scripts/wiring_lint.toml`の`[no_default_lambdas]`に登録されたファイルで
   関数型パラメータの`= {`を拒否する(既存の「既定はno-op」というKDocの慣習をコピーして再発するのを止める、F7)。
3. **Phase 3 — rustc `dead_code`のcrate単位deny(W4・W7・W8の一部、§3(b))**: `isekai-terminal-core`の警告19件(項目約14)を1PRで片付け
   (§1.3(4)の残骸を含む)、`purity-check`jobに`cargo clippy -p isekai-terminal-core --lib -- -A clippy::all -D dead_code`を追加。
   続いて`isekai-pipe`/`isekai-pipe-core`/`isekai-netmon`(計数件)。`isekai-ssh`(195件、うち`native/`配下184件)はdenyにせず、
   Linuxと`rust-core-test-windows`の両ログの**積集合**(どちらのプラットフォームでも未使用=本当に死んでいる)を報告する。
   あわせて`isekai-terminal-core`の本番コードで`cfg!(debug_assertions)`/`#[cfg(debug_assertions)]`を字句的に禁止する(W8の8d480d01型、現状0箇所)。
4. **Phase 4 — argvの往復テスト(W5の一部、§3(h))**: `isekai-pipe serve`の起動引数を、生成(`isekai-bootstrap`)と解析
   (`isekai-pipe/src/engine/mod.rs:259 parse_args_from`)の両方が使う1つの型+関数に寄せ、`parse(build_argv(spec)) == spec`を
   proptestで検査する。119205f6型(片方のbackendがフィールドを落とす)を捕まえる。e1c370e5型(そもそもフィールドが無い)は捕まえない。
5. **iOS**: Linux上で型解決付きの同等物が作れない(§3(a′)末尾)。**強制はしない**。`check-wiring-lint.py`がforwarder透過の字句レポートを
   警告として出すだけにとどめる。透過扱いにするのは**同名forwarder**(`func X(...) { orchestrator.X(`の形のメソッド)だけで、ファイル全体ではない。
   `TerminalSessionController.swift:244`の`self?.orchestrator.notifyNetworkPathChanged(isSatisfied:)`は、同ファイルの
   `startNetworkPathMonitoring()`(`:240`付近、`NWPathMonitor`の生イベント)から直接呼ばれており同名forwarderも他ファイルの呼び出しも無いが、
   正しく配線されている。同ファイル内の他メソッドからの`orchestrator.X(`は呼び出し元として数える(rev2、R2-5、確認済み)。

W6(エントリポイント重複)は本ADRでCI機構を入れない(FCIS ADR Step 6+7の回復ループ共通化で扱う)。

---

## 3. 検出機構の評価

各機構について、CI-onlyでの実現性・誤検知(FP、配線済みなのに落ちる)・見逃し(FN、未配線なのに通る)・コスト・
原理的に捕まえられないものを書く。最後に副分類×機構の表を置く。

### (a) 字句照合スクリプト(rev0の中核案、Androidについては撤回)

- **rev0の案**: 生成済みバインディングから公開シンボル名を取り、Kotlin/Swiftの本番ソースで`\bname\s*\(`の出現を数え、
  TOML台帳で`wired`と宣言されたシンボルが0なら失敗させる。
- **致命的なFN(F1、確認済み)**: `rust-ssot.md`が要求する「そのまま転送する」forwarderがKotlin(`TerminalSession.kt`に
  `SessionOrchestratorInterface`の35メソッド中31の同名forwarder、レビュー集計)とSwift(`TerminalSessionController.swift`)の
  両方で**標準の構造**になっている。正規表現はforwarderの宣言`fun notifyDidEnterBackground(`と内部の`orchestrator.notifyDidEnterBackground(`に
  ヒットするので、forwarderを書いた時点で、ViewModel・`AppExecutor`・OSのobserverが一度も呼ばなくても`wired`は満たされる。
  forwarderは誰でも最初に書くhopなので、この検査は**事実上落ちない**。
- **db3a6d87については**: 修正前のAndroid本番にはforwarderすら無く、生成物以外に`notifyDidEnterBackground`の出現が0だった
  (`git grep db3a6d87^`、確認済み)ので、この検査でも見つかった。ただし「導入PR 710aecf2で」ではない(rev0の誤り、F9):
  710aecf2はrust-coreのみを変更し(`git show --stat`、確認済み)、Kotlinバインディングは同日の別コミットc848eb03、Swift側は
  ee6ec5de/f9cb8cc7で入った。当時はbranch protection導入(2026-08-17)前で直pushだった。現在はrequired `android-uniffi-drift`が
  API変更とバインディング再生成を同じPRに揃えるが、`enforce_admins: false`の間は直pushでこれも迂回できる(`main-branch-protection.md`)。
- **改良案(レビューF1の提案)とその評価**: (1)宣言を除外し呼び出し式だけ数える、(2)forwarderファイルを透過扱いにして
  forwarder名の**外部**呼び出しを数える、(3)台帳に`via = "TerminalSession.notifyDidEnterBackground"`を書かせる。
  これで1hop分は改善するが(iOSで残す場合の透過範囲は§2-5の通り同名forwarderに限る)、hopは`TerminalSession` → `TerminalTabsViewModel.onAppBackgrounded`(`:608`) →
  `AppExecutor.registerLifecycleCallbacks`(`:460-463`) → `AndroidAppExecutor`の`ProcessLifecycleOwner`登録(`:104-116`)と続き、
  db3a6d87の修正は3hopを全部足した。字句で各hopを透過させる設定を書くのは、型解決のやり直しを手書きでするのと同じで、
  汎用名(`connect`/`send`/`push`/`publish`、§1.3(2))の衝突も残る。**Androidでは採らない**。iOSでは型解決付きの代替が
  Linux上に無いので、(2)の透過扱いを警告レベルのレポートとしてだけ使う(§2-5)。
- **その他のFN**: 反射/文字列経由の呼び出し(rev0は「見当たらない(推測)」としたが、3965e322が`Class.forName("uniffi.isekai_terminal_core...")`で
  実在した(F2)。字句検査ではむしろ「未配線」と判定されるのが正しい結果)。これはPhase 2の字句ガードで
  `Class.forName("uniffi.`を禁止して扱う(§3(g))。

### (a′) Android配線契約テスト(rev1で採用、required `android-unit-test`内)

- **形**: `android/src/test/kotlin/tools/isekai/terminal/WiringContractTest.kt`(Robolectric/JVM)。
  1. **分類表の網羅**: `SessionOrchestratorInterface::class.java.methods`・`OrchestratorCallback::class.java.methods`と、
     トップレベル関数のファサードクラス`uniffi.isekai_terminal_core.Isekai_terminal_coreKt`(このクラス名は3965e322の修正前コードが
     使っていた。生成物冒頭に`@file:JvmName`は無い、確認済み)の公開staticメソッドを反射で列挙し、
     (ファサードには27関数のほかに生成物の公開トップレベル関数`uniffiEnsureInitialized`(`:1596`)と`inline fun <T : Disposable?, R> T.use`(`:1694`)も
     公開staticとして現れるので、この2つは固定の許可リストで除外する。rev2、R2-3、確認済み)テスト内の分類表にすべてのメソッドがあること、分類表に
     存在しないメソッドが無いことをassertする。新しいUniFFIメソッドを足したPRは、Kotlinバインディング再生成(required drift check)
     の時点でinterfaceにメソッドが増え、分類しない限りこのテストが落ちる。
  2. **OS由来入口の到達**: 記録Proxy(`Proxy.newProxyInstance(loader, arrayOf(SessionOrchestratorInterface::class.java)) { _, m, args -> recorded += m.name; m.invoke(fake, *(args ?: emptyArray())) }`)
     で既存の`FakeOrchestrator`(`FakeSshGateway.kt:11`)を包み、既存テストと同じく`TerminalSession(FakeHostKeyChecker(), orchestratorFactory = { cb -> ... })`
     (`TerminalTabsViewModelTest.kt:76-78`の形)で注入する。`DumbAppExecutor`のsimulatorとViewModelの公開APIだけで台本を進め、
     `OsEvent`に分類された全メソッドが**primaryとsplitの両pane**で記録されたことをassertする。
  3. **callbackの到達**: `InjectedLambda`に分類されたcallbackについて、全注入lambdaを記録用にした`TerminalSession`を作り、
     fakeが保持する`callback`の該当メソッドを直接呼んでlambdaの発火をassertする。
  4. **全数照合**: UniFFI checksumシンボル数(func 27・method 63・constructor 3、うちmethodは`SessionOrchestrator` 35・
     `OrchestratorCallback` 19・診断系9、確認済み)と反射で列挙した数を照合する。checksumの外部関数は`internal object IntegrityCheckingUniffiLib`
     (生成物`:814`、確認済み)のメンバーなので、`Class.forName("uniffi.isekai_terminal_core.IntegrityCheckingUniffiLib", false, loader).declaredMethods`
     で、objectを初期化せず(=JNAのロードを起こさず [EXT])列挙できる。リポジトリ内のファイルを読む必要は無い(rev1の[EXT]を解消)。
  5. **名前規則(R2-1)**: `notify`で始まる全メソッドの分類が`OsEvent`/`UiAction`/`NotUsedOnAndroid(ref)`のいずれかであることをassertする。
  6. **UI入口の到達(R2-1)**: `UiAction`の全メソッドについて、Compose台本で発火・記録をassertする(§2-1)。
     `TerminalHostScreenTest`(`android/src/test/kotlin/tools/isekai/terminal/TerminalHostScreenTest.kt`、`@RunWith(RobolectricTestRunner::class)`+
     `createComposeRule()`、15テスト、required `android-unit-test`内)が既に本物の`TerminalHostScreen`と`TerminalTabsViewModel`を
     同じ`FakeOrchestrator`のsessionFactoryで描画している(`:60-75`、確認済み)ので、本番の`TerminalScreenActions(...)`生成
     (`TerminalHostScreen.kt:530`)と`onFocusChanged = { focused -> pane.session.notifyFocusChange(focused) }`(`:566`)をそのまま通せる。
     クリックは`performSemanticsAction(OnClick)`を使う(`performClick()`はFilterChip等で無言no-opになる既知事例あり)。
- **反射の注意 [EXT]**: Kotlinはinline class(`UInt`等)を引数に持つメソッドのJVM名をマングリングする(例: `notifyDidEnterBackground-xxxx`)。
  名前の正規化(`-`以降を落とす)が要る。`suspend fun`(`ensureTmuxTabWindow`)は`Continuation`引数が増える。
  ファサードクラスの反射は`Class.forName(name, false, loader)`(静的初期化=ネイティブライブラリのロードを起こさない形)で行う。
- **捕まえるもの**: W1のうちOS由来入口(db3a6d87は**半配線でも**捕まる。forwarderがあってもViewModel/`AppExecutor`の登録が無ければ
  記録されない)、W3(7b10472a、split paneを台本に含めるので)。W2のうち`InjectedLambda`のcallbackがlambdaへ届くこと。
  e8ed36ee型(dead export)は「捕まえる」のではなく、`NotUsedOnAndroid(ref)`と書かせることで**PR差分上で可視化**する。
- **捕まえないもの**:
  - **本番のsession生成lambda**: テストは`sessionFactory`を差し替えるので、`TerminalTabsViewModel.kt:249`の本番の
    `TerminalSession(...)`生成(注入lambdaの実際の配線)は通らない。ce214ef5型はPhase 2のコンパイル強制で扱う。
  - **(rev2で訂正)UI由来の入口**: rev1は「ViewModelテストからは発火できないので`UiAction`は宣言だけ」と書いたが誤り。
    ViewModel単体のテストからは発火できないが、同じrequired job内のComposeテスト(`TerminalHostScreenTest`)からは発火できる(上の6)。
    ただしCompose台本はライフサイクル台本よりflakeしやすい(推測)ので、別テストに分ける。ジェスチャ専用で意味木から駆動できない
    actionは`ref`付きで除外し、その到達はPhase 2のコンパイル強制(本番の生成箇所で全actionを渡すこと)だけに頼る。
  - **最後のOS hop**: `AndroidAppExecutor`が実際に`ProcessLifecycleOwner`/`ConnectivityManager`へ登録する部分(`AndroidAppExecutor.kt:86,104-116,128`)。
    `AppExecutor`interfaceのメソッドとしてはコンパイラが実装を強制するが、実装が空なら通る。ネットワーク側は既存のRobolectricテスト基盤で
    後から検査できる(rev2、§7)。ライフサイクル側をRobolectricで駆動できるかは [EXT]。
  - W8(テストのfakeは本番の偽の条件を再現しない)、W4〜W7(Rust側)。
  - 分類の嘘: `NotUsedOnAndroid`と書けば何でも通る。ただし`ref`必須と、分類表がPR差分に出ることで、レビューで見える。
    `Command`/`Query`への逃避は`notify*`については名前規則で止める(R2-1)。`notify`で始まらない新しいOSイベント入口を`Command`に
    分類する逃避は残る(名前規則はRust側の命名慣習に依存する)。
- **CI-only実現性**: 高。既存のrequired `android-unit-test`の中で走るので、新しいrequired contextもworkflowも要らない。
  pr-path-gateで`rust-core/`変更でも走る(バインディング再生成はAndroid側ファイルの変更を伴う)。
- **FP**: 台本が不安定だと落ちる(Robolectricの負荷起因flake、memory上の既知事例あり)。台本は`DumbAppExecutor`の同期simulatorだけで組み、
  非同期待ちは既存の`awaitConnectCalled`等に揃える。
- **コスト**: テスト1ファイル(推測で300〜500行、分類表が大半)。維持は「UniFFIメソッドを足したら1行分類」で、rev0の台帳と同等。
- **TOML台帳との比較(F10の問い)**:

  | | rev0のTOML台帳+正規表現 | 配線契約テスト |
  |---|---|---|
  | forwarder(F1) | 必ず素通り | 影響なし(呼ばれたことを観測する) |
  | 汎用名・同名メソッド(F5・F6) | 衝突でFN/FP | 型で区別(interface単位の反射) |
  | 半配線(db3a6d87の3hop) | 透過設定を手書き | 台本が入口から通すので自動 |
  | 走る場所 | 新workflow(非required) | 既存required job |
  | iOS | 字句でなら同じ仕組みで可 | 不可(同等物なし) |
  | UI由来の入口 | 字句で数えられるが上記FN | Compose台本で観測(rev2) |

  **推奨: Androidは配線契約テスト、iOSは字句レポート(警告のみ)**。
- **iOSで同じことができない理由(確認済み+[EXT])**: `TerminalSessionController`は`IsekaiTerminalCore`ターゲットにあり、
  Linuxの`ios-logic-linux-check`は`IsekaiTerminalCoreLogic`の`swift test`しか走らせない(FCIS ADR Step 13が確認済み)。
  Swiftにはプロトコルのメソッドを実行時に列挙する反射が無い [EXT]ので、(1)の網羅はSwiftでは書けない。
  型解決付きの未使用検出はPeriphery [EXT](index storeを使う、macOS/Xcode必須)で、既存のmacOS系`ios-*`job(非required)に足すことはできる(§10 U10)。

### (b) rustc/clippyの`dead_code`(rust-core内の非公開項目)

- **基準値(確認済み、§1.3(4)と同じCIログ)**: `purity-check`の`cargo clippy --workspace --lib --bins -- -A clippy::all`
  (`rust-core-test-check.yml:338`)の出力で、`isekai-ssh`(bin) 195件、`isekai-terminal-core`(lib) 19件、`isekai-pipe` 1〜2件、
  `isekai-pipe-core` 1件、`isekai-netmon` 1件。種類はほぼすべてdead_code系(`function ... is never used` 109、`constant` 39、
  `struct ... never constructed` 15、`enum` 13、`method` 12ほか)。`isekai-ssh`の195件のうち`native/`配下が184件(レビュー集計、
  `native/mux` 111・`native` 73)。rev0の推測「`-A clippy::all`でもrustcのdead_codeは出る」はこのログで確認された。
- **rev0の誤りの訂正(F3)**: rev0は「`native/`は`isekai-ssh/src/main.rs:111,123`の`cfg_attr(not(windows), allow(dead_code))`で抑止している」と
  書いたが、この2箇所は定数2つ(`EXIT_MUX_OWNER_LOST`・`EXIT_USER_CANCELED`)だけで、`native/`は抑止されていない。本番コードの
  `allow(dead_code)`は3箇所ではなく6箇所(`src/test_callbacks.rs:53`、`isekai-ssh/src/main.rs:111,123`、`isekai-protocol/src/ctl.rs:977`、
  `isekai-transport/src/candidate_pool.rs:55`、`local-ipc-mux/src/lib.rs:111`、確認済み。ほかに`tests/`配下に4箇所)。
- **何が見えるか**: lib crateの非`pub`項目と、bin crateの`main`から到達しない項目のうち、本番ビルド(非test)で使われないもの。
  8baa49d8(setterの未呼び出し)、enum variantの未構築(`TmuxTargetKind::Pane`、W8のうちenum型)。
- **見えないもの**: `#[uniffi::export]`(`pub`、W1は不可視)、lib crateの`pub`項目(8cdb325c)、「どこかで1回」読まれるフィールド(119205f6)、
  呼ばれているが偽の条件の背後にある経路(8d480d01)。**そして配線しても機能が死んでいる場合**(8baa49d8 → ee34304a、§1.4)。
- **cfgの落とし穴**:
  1. `cfg(feature)`: ワークスペースマニフェスト起点でないと偽の警告が出る(`quicmux`の既知事例)。`-p <crate>`をワークスペースから
     選ぶ形(`cargo clippy -p isekai-terminal-core`)は満たす。
  2. `cfg(windows)`: `isekai-ssh`は`native/`をLinuxでもコンパイルする意図的な設計(`native/mod.rs`のdoc、`parallel-worktree-agent-operations.md` §2)で、
     Linuxでは`native/`経由でしか使われない共有モジュールの項目も警告される(`wrapper.rs:1934 resolve_for_native`、
     `log_file.rs:229 init_holder_log`等、レビュー指摘)。逆にWindowsではUnix専用経路の項目が警告されると推測される(未計測)。
     **`isekai-ssh`は単一プラットフォームでのdenyが成り立たない**。`cfg_attr(not(windows), allow(dead_code))`で約190項目を
     抑止すると、ae8ed13b(W6)が起きた場所そのものを恒久的に見えなくする。
  3. vendor crate(`quicsock`・`h3-noq`・`isekai-link-masque`)もworkspace member(`rust-core/Cargo.toml:2`、確認済み)。`--workspace`に
     `-D`を付けると「vendor元のCargo.tomlをそのまま保つ」方針と衝突する。crate単位(`-p`)にすれば自然に除外される。
- **採用する形(Phase 3)**: crate単位。`--workspace`への一括`-D`も、`Cargo.toml`の`[lints.rust] dead_code = "deny"`(ローカル・IDE・
  テストビルドにも効く)も採らず、CIの追加clippy呼び出し`cargo clippy -p <crate> --lib --bins -- -A clippy::all -D dead_code`にする
  (依存のビルドキャッシュは同じjob内で共有され、再checkされるのは対象crateだけ、と推測 [EXT])。
  - 順序: `isekai-terminal-core`(警告19件・項目約14を1PRで。coreのAndroid専用cfgは`lib.rs:68,77`の`target_os = "android"`だけなので、
    Linuxでのdenyにプラットフォーム起因の偽陽性は無い(確認済み)。残骸は削除、意図的なものは`#[expect(dead_code, reason = "...")]`
    [EXT: Rust 1.81+。CIとF-Droidレシピは`stable`、workspaceの`rust-version = "1.75"`は継承するcrate(`quicsock`)にしか効かない、とレビューが確認]。
    `expect`は使われるようになると逆に警告するので、残した理由の陳腐化も検出される) → `isekai-pipe`・`isekai-pipe-core`・`isekai-netmon`。
  - `isekai-ssh`: denyしない。`purity-check`(Linux)と`rust-core-test-windows`(非required)の両方でdead_code警告の項目名一覧を
    artifactに出し、**積集合**(両プラットフォームで未使用=本当に死んでいる)を報告するスクリプトを置く。報告は失敗させない。
    積集合が0になったら`isekai-ssh`もdenyを検討する(§10 U4)。
  - rev0の「まず件数をjob summaryに出すだけ」の段は削除した(F4): job summaryに出る214件の警告が既に誰にも読まれていないのが現状で、
    段を挟んでも何も変わらない。
  - モジュール先頭の`#![allow(dead_code)]`(6ee526d4の815行を隠した形)は、`check-wiring-lint.py`で`scripts/wiring_lint.toml`の
    `[allow_dead_code]`に理由付きで登録されたファイル以外を拒否する(現状モジュール先頭形は本番コードに無い、`tests/common/mod.rs:34`のみ)。
- **W8への追加の字句禁止**: `isekai-terminal-core`の本番コードで`cfg!(debug_assertions)`/`#[cfg(debug_assertions)]`を禁止する。
  Androidは`--release`でビルドする(`android/build.gradle.kts:85`)ので、これは本番で常に偽になる。現状0箇所(`debug_reconnect.rs:3`の
  docコメントのみ、確認済み)なので導入コストは無い。デバッグ専用の経路は既存規約通りKotlinの`src/debug`ソースセットで到達不能にする(8d480d01本文)。
- **`-p`とfeature統一(R2-9、OPINION+[EXT])**: `-p isekai-terminal-core`は選んだパッケージの依存グラフだけでfeatureを統一するので、
  既存の`--workspace`呼び出しと異なるfeature集合で共有依存が再checkされ、`purity-check`(timeout 30分)の時間が伸びうる。
  cfg依存のdead_codeの見え方が変わる可能性もある。`parallel-worktree-agent-operations.md` §4はワークスペースマニフェストからの`-p`を
  認めているので形としては許容し、Phase 3のPRで追加時間を実測して書く。
- **D3との関係**: `purity-check`がrequired化されていれば、crate単位の`-D dead_code`はrequired checkを落としうる。coreの既存違反を
  片付けたPRの直後に入れるので、導入時点の既存違反は0。

### (c) Kotlin側のcallback interface実装者の網羅性

- **評価**: `OrchestratorCallback`へのメソッド追加は、Kotlin・Swiftの全実装者でコンパイラが実装を強制する(FCIS ADR Step 8a′の
  実装者一覧と同じ)。**「実装者が存在する」は既に保証されており、追加の機構は不要**。
- 不足しているのは「実装者が意味のあることをする」で、Androidは(a′)の分類表(`InjectedLambda`の到達テスト、`LogOnly`は`ref`必須)と
  (g)で扱う。iOSは「UI未実装ならno-opスタブで追従」が慣習(64fc32cf/3cd005e9、§1.2補足)なので、iOSのW2は本ADRでは検出しない。

### (d) contract-golden(FCIS ADR Step 13との関係)

- Step 13はRustが出すcallback列(当面は`on_connection_edge`のみ)をgoldenにし、Kotlin/Swiftへreplayする。これは
  **Rust → platform方向のW2**を、goldenに載ったcallbackに限って捕まえ、**iOSでも**(`IsekaiTerminalCoreLogic`層で)走る。
- **捕まえないもの**: platform → Rust方向(W1)。
- **本ADRとの関係**: (a′)の`InjectedLambda`到達テストと重なるのは`on_connection_edge`だけ。Step 13は8a′が前提で未着手なので、
  それまでの間callbackのW2は(a′)・(g)で扱う。goldenを全callbackに広げる案は採らない(§8)。

### (e) カバレッジによる「どのテストでも実行されない」報告

- **評価: 不採用**。信号の向きが逆。
  - **実例(F2で追加、確認済み)**: e8ed36eeで削除された`add_local_forward`/`remove_forward`は、Android本番から一度も呼ばれていなかったが、
    4dde90fbが足したe2eテストで実行されていた。カバレッジ上は「カバー済み」で、38日後に整理作業で削除された。
  - db3a6d87の`notify_did_enter_background`もRustの`orchestrator.rs`テストで実行されていた。
  - CIには現在カバレッジ計測が一切無い(`llvm-cov`/`tarpaulin`/`kover`/`jacoco`のいずれもworkflow・gradle・Cargo.tomlに無い、確認済み)。
- **唯一の用途**: 一度きりの棚卸し。必要になったらworkflow_dispatchの単発ジョブで(§10 U6)。

### (f) TOML台帳(rev0案、廃止)

- rev0は`uniffi_wiring.toml`に`wired`/`pending`/`not_applicable`/`mirrored`/`debug_only`/`dead`/`log_only`/`forwarded`を宣言させた。
  Androidの部分は(a′)の分類表(Kotlin、型付き)に置き換えたので**TOML台帳は廃止**する。
- **`pending`の廃止(F8)**: `pending`+`ticket`+警告だけの`review_by`は、エージェントが「CIを通すための最小の1行」として書き、
  数か月で「文書化されたdead exportの一覧」になる、という指摘を受け入れた。実例: `try_claim_tmux_window`は8ced2131本文で
  「Y-P2で配線」と宣言されて44日、その間Kotlinミラーが仕事をしている(§1.3(1))。期限付きで失敗させる案(PRが台帳・シンボル・
  所属ファイルに触れたときだけ失敗、レビュー提案)も検討したが、Kotlinの単体テストからは「このPRが何に触れたか」が見えず、
  日付で失敗させるとコード変更の無いmainが赤くなる。そこで状態そのものを無くし、**未配線は未配線と書く**
  (`NotUsedOnAndroid(ref)`)。「将来配線する」という約束を表す状態を持たないことで、約束の陳腐化という問題自体を消す。
  将来配線する予定は`ref`が指す文書(例: `TASKS_IOS_ADR_YR.md`のY-P2)に書く。
- **`mirrored`(F8-3)**: `rust-ssot.md`からの承認済みの逃げ道なので、`NotUsedOnAndroid(ref)`の`ref`がADR節またはルールファイルの節で
  あることを、`mirrored`相当(プラットフォームに同等の独立実装がある)の場合は必須にする。現状の`tmuxClaimedProfileIds`は書面の例外が
  無いので、Phase 1の初期化時に「`rust-ssot.md`違反として記録し、Y-P2で解消」と分類表のコメントに残す(この判断はU8)。

### (g) 既定no-op lambdaの撤廃と再発防止ガード

- **Rust → platform側(確認済み)**: `TerminalSession`のコンストラクタは`onClipboardWriteRequested: (ClipboardPayload) -> Unit = {}`(`:51`)・
  `onClipboardPullRequested = { null }`(`:60`)・`acquireWifiFd = { null }`(`:67`)・`onBell = {}`・`onNotify`・`onNotifyRequested`等の
  既定no-opを持つ。各既定値には「既定はno-op」というKDocが付いており、これが家風になっている(F7)。ce214ef5はこの既定値のせいで、
  渡し忘れがコンパイルを通った。本番の生成箇所は`TerminalTabsViewModel.kt:249`の1箇所、テストは20箇所(確認済み)。
- **platform → Rust側(rev1で追加、確認済み)**:
  - `NetworkPathMonitor.kt:49` `fun start(onAggregateChanged: (anyPathAvailable: Boolean) -> Unit = {})`: OSのネットワークイベントの入口。
    引数無しの`monitor.start()`は全イベントを捨てる。本番の呼び出しは`AndroidAppExecutor.kt:86,128`(どちらも現在は引数あり)。
  - `TerminalScreen.kt:97`の`data class TerminalScreenActions`: 関数型プロパティに約25個の既定値(`onCancelReconnect = {}` `:101`、
    `onClickToPromptCursor = { _, _ -> }` `:117`、`onForceReturnToWifi = {}` `:140`、`onFocusChanged = {}` `:144`等、字句grepで概算)。
    UI → `TerminalSession` → `notifyFocusChange`/`forceReturnToWifi`/`cancelReconnect`のhop。本番の生成箇所は`TerminalHostScreen.kt:530`の1箇所(確認済み)。
  - `HostKeyChecker.kt:37` `autoTrustNewHostKeys: () -> Boolean = { false }`: これは安全側の既定値(自動信頼しない)で、no-opではない。
    ガードの対象外とする(既定値撤廃の目的は「渡し忘れで機能が黙って消える」ことの防止で、渡し忘れで安全側に倒れるものは対象外)。
- **決定(Phase 2)**:
  1. 上記3ファイル(`TerminalSession.kt`のコンストラクタ、`NetworkPathMonitor.start`、`TerminalScreenActions`)の関数型パラメータから既定値を外す。
     本番コードの変更は挙動保存(本番の生成箇所が既に渡しているものはそのまま、渡していないものは現在の既定値と同じ値を明示する)。
     テストはファクトリ(既定no-opを持つ`testTerminalSession(...)`等)に寄せる。
  2. **再発防止ガード**: `scripts/check-wiring-lint.py`が`scripts/wiring_lint.toml`の`[no_default_lambdas] files = [...]`に
     登録された**範囲**で、関数型の型注釈(`-> ` を含む型)を持つパラメータ/プロパティの`= {`を拒否する(字句、簡易)。
     rev2(R2-4、確認済み)での精緻化:
     - 範囲はファイル単位ではなく宣言単位で登録する(`TerminalSession`のプライマリコンストラクタ、`NetworkPathMonitor.start`、
       `TerminalScreenActions`のdata class本体)。`TerminalScreen.kt:300`の`onUserActivity: () -> Unit = {}`(Composable関数の引数、
       data class外)を巻き込まないため。`onUserActivity`の既定値も外す案もあるが、対象を広げる理由になる事例が無いので採らない。
     - `TerminalSession`の`orchestratorFactory: (OrchestratorCallback) -> SessionOrchestratorInterface = { createSessionOrchestrator(it) }`
       (`TerminalSession.kt:43`)は8つ目の関数型既定値だが、no-opではなく本番の既定実装で、本番(`TerminalTabsViewModel.kt:249`)が依存している。
       `[no_default_lambdas]`の項目単位の許可(`allow = ["orchestratorFactory"]`)で明示的に除外する。
     - 型注釈と`= {`が改行で分かれる形をすり抜けないよう、宣言の開き括弧から対応する閉じ括弧までを空白正規化してから照合する。
     `Class.forName("uniffi.`(3965e322型の反射呼び出し)はAndroid全ソースで拒否する。
  3. 字句検査の走査範囲は**許可リスト方式**(F12): Androidは`android/src/main`・`android/src/debug`、iOSは`ios/Sources`(`generated/`除く)・
     `ios/App/IsekaiTerminalApp`。`ios/App/IsekaiTerminalAppTests`・`…UITests`・`android/src/{test,testDebug,androidTest}`は含めない。
- **iOS(U9、rev1で回答)**: `TerminalSessionController.init`(`TerminalSessionController.swift:215-224`)の引数はprofile・password・db・vault等の
  依存だけで、**注入closureは無い**(確認済み)。コントローラにPhase 2は適用不要。iOSのW2は注入closureではなく
  「no-opのプロトコル実装」として現れる(64fc32cf)。なお`TerminalView.swift:811-816`に`onShowSnippets`等6個の`= {}`既定値があり(確認済み)、
  これはUI操作のhopなので、iOS側でも同じガードを`[no_default_lambdas]`に登録できる(`@escaping () -> Void = {}`の字句形。§10 U9)。
- **CI-only実現性**: 高。既定値撤廃は`android-unit-test`のコンパイルで強制、字句ガードは新workflowで数秒。
- **FP**: 字句ガードが関数型以外の`= {`(ブロック初期化子等)を誤検出しうる。登録ファイルに限定し、`--self-test`で既知の形を固定する。
- **FN**: 渡したlambda自体が`{}`なら通る(意図的な空実装と区別できない)。

### (h) W5: argvの往復テスト

- **現状(確認済み、rev2で訂正)**: `--resume-window`のargvを生成するのは`isekai-bootstrap/src/install_script.rs:254,273`(シェルスクリプト文字列への
  `format!`)だけで、`russh_backend.rs`には`resume_window`の文字列が無い(119205f6当時はRelayアームの起動引数を自前で組んでいたが、現在は
  生成が`install_script.rs`に寄っている、推測)。解析側は`isekai-pipe/src/engine/mod.rs:349`(`parse_args_from`、`:259`)と`connect.rs:236`で手書き。
  検査はe2eの文字列`contains`(`openssh_e2e.rs`)だけ。`isekai-bootstrap`は`isekai-pipe`に依存しない(内部依存は`isekai-protocol`・`isekai-stun`・
  `isekai-fs-guard`・`isekai-trust`・`openssh-config`・`russh-stream-session`の6個、`isekai-bootstrap/Cargo.toml:8-13`、確認済み。rev1は「`isekai-protocol`のみ」と誤記)。
- **案(F11)**: `serve`の起動引数を表す型(例: `ServeLaunchArgs`)と`to_argv()`/`parse()`を、両者が依存する`isekai-protocol`に置き、
  `install_script.rs`と`russh_backend.rs`は`to_argv()`の結果だけを使い、`engine/mod.rs`は`parse()`を使う。`to_argv`は`..`無しの全分解で書く。
  `parse(to_argv(spec)) == spec`のproptestは**`isekai-pipe`のテストに置く**(rev2、R2-8)。`isekai-pipe`は`isekai-protocol`に依存し、
  proptestを既にdev-dependencyに持つ(`isekai-pipe/Cargo.toml:80`、FCIS Step 1・#137。ほかにルートcrateと`isekai-transport`も持つ、確認済み)ので
  `Cargo.lock`は変わらない。`isekai-protocol`にテストを置くとproptestのdev-dependency追加で`Cargo.lock`が変わり、required `lockfile-drift`の
  対象になる(§6の「`Cargo.lock`を変えない」と矛盾する)。
- **捕まえるもの**: 119205f6型(片方のbackendがフィールドを落とす。生成を1つの`to_argv`に限れば構造的に起きない)、
  解析側がフラグを読み落とす型。
- **捕まえないもの(レビューの主張を一部訂正)**: e1c370e5は「クライアント設定を運ぶフィールドがそもそもspecに無かった」型で、
  往復テストは存在しないフィールドを検査できない。f538dc71(env)は別の型で、Unix経路とnative経路の子プロセスenvのキー集合が一致する
  ことのパリティテスト(`Command::get_envs()`、[EXT])なら捕まえられる。142fcbde・1fb3d5a6(関数引数の素通し)は対象外。
- **コスト**: 中。シェルスクリプト生成側(`install_script.rs`)が`to_argv()`の結果をシェル引用して埋め込む形への変更と、
  シェルの単語分割を経た往復の確認(引用の正しさ、[EXT])が要る。`Cargo.lock`は変わらない(テストを`isekai-pipe`に置き、`isekai-protocol`に
  新しい依存を足さない限り)。
- **位置付け**: Phase 4(採用。ただし`isekai-bootstrap`/`isekai-pipe`のlaunch引数を次に変更するPRで行う、§10 U5)。

### (i) その他の型解決付き代替(F10)

- **Periphery(Swift)[EXT]**: index storeを使う未使用宣言検出で、呼ばれないforwarderも見つかる。macOS/Xcode必須なので非requiredの
  macOS系`ios-*`jobにしか置けない。iOSの唯一の型解決付き手段なので、警告レポートとしての追加を§10 U10に置く。
- **Qodana/IntelliJの`UnusedSymbol`(Kotlin)[EXT]**: 型解決付きの未使用公開宣言検出。publicリポジトリでの利用条件・CI時間は未調査。
  (a′)が「呼ばれない」ではなく「入口から届く」を直接検査するので、Androidでは優先度が低い。採らない(必要なら再提案)。
- **閉じたイベントenum**(`notify_platform_event(PlatformEvent)`): 送り手の網羅性はこれだけでは生まれないが、(a′)と組み合わせると
  台本が`PlatformEvent.entries`を回して全variantの観測を要求でき、新しいイベントが分類表の1行無しで自動的に対象になる。
  ただしUniFFI公開APIの形を変える大きな変更(全プラットフォームの呼び出し側)で、FCIS ADR §8「Step 8a′以外でUniFFI公開APIを変えない」とも
  衝突する。§8で不採用とし、理由を残す。

### 副分類 × 機構

| | (a)字句(iOSのみ警告) | (a′)配線契約テスト | (b)dead_code(crate単位) | (c)実装者網羅 | (d)golden | (e)カバレッジ | (g)既定値撤廃+ガード | (h)argv往復 |
|---|---|---|---|---|---|---|---|---|
| W1 入口が呼ばれない | △(forwarder透過でも1hopのみ) | **○**(OS由来入口、半配線も) | × | × | × | ×(逆の信号) | △(UI hopのコンパイル強制、反射呼び出し禁止) | × |
| W2 最初のhopで捨てる | × | ○(`InjectedLambda`のみ) | × | ×(既に保証済みの層) | △(golden対象のみ) | × | **○**(lambda渡し忘れ) | × |
| W3 fan-outの一部 | × | **○**(split paneを台本に含める) | × | × | × | × | × | × |
| W4 Rust内部の登録漏れ | × | × | **○**(テストからのみ呼ばれる場合) | × | × | × | × | × |
| W5 値の伝搬漏れ | × | × | × | × | × | × | × | △(argvの片側漏れのみ) |
| W6 エントリ重複の片側漏れ | × | × | × | × | × | × | × | × |
| W7 モジュール丸ごと | × | △(公開APIなら`NotUsedOnAndroid`で可視化) | ○(非pub、モジュール先頭allowの登録制とセット) | × | × | △(棚卸し) | × | × |
| W8 常に偽の条件の背後 | × | × | △(未構築enum variantのみ) | × | × | × | × | × |

W8の`cfg!(debug_assertions)`型は(b)の字句禁止で扱う。§1.2の17件に当てはめると(推測を含む): (a′)でdb3a6d87・7b10472a、(g)でce214ef5・3965e322、
(b)で8baa49d8・6ee526d4・8d480d01(字句禁止)、(h)で119205f6。e8ed36eeは可視化のみ。ee34304aは(b)の`Pane`未構築警告で見えた可能性があるが未検証。
2a07a5e3は(b)で捕まった可能性がある(未検証)。W5の残り4件・W6の1件は捕まらない。

---

## 4. 既存ルール・ADRとの関係

- **`rust-ssot.md`**: (a′)の分類表で、Rustに判断があるのにプラットフォームが独自実装している箇所(`tmuxClaimedProfileIds`、
  `TerminalKeyEncoder.kt`のミラー)は`NotUsedOnAndroid(ref)`の`ref`に書面の例外を要求される。書面の例外が無いものは、
  分類表上「未解消の違反」として残り、PR差分で見える。
- **`uniffi-binding-regeneration.md`**: (a′)は**コミット済みのKotlinバインディング**(=反射対象の生成interface)に対して走る。
  Rust公開APIを変えるPRは、(1)required `android-uniffi-drift`が再生成済みバインディングのコミットを強制し、(2)同じPRの
  `android-unit-test`で(a′)が新メソッドの分類を強制する。Swift版の`.sha256`サイドカーの扱いは同ルールのまま変えない。
- **`main-branch-protection.md`**: (a′)は既存のrequired `android-unit-test`内で走るので、protection設定の変更は不要。
  新workflow`wiring-lint`は当面非required(§10 U2)。required化する場合は同ルールの手順(明示`name:`と`checks[].context`の同時更新)に従う。
- **FCIS ADR**:
  - §10 D5の「提案」をこのADRが具体化する。採用時はFCIS ADR側のD5を「本ADRで扱う」として解決済みに移す(本ADRのPRではFCIS ADRを編集しない)。
  - Step 0の`check_pure_modules.py`と同じく字句スクリプトは`--self-test`付きにする。配置は、Rust専用の検査は`rust-core/scripts/`、
    Android/iOS/Rustを横断する検査はリポジトリ直下`scripts/`(`check-room-migrations.sh`の慣習)とし、`check-wiring-lint.py`は後者。
  - Step 7a(interpreterの網羅性lint)は「reducerが出したEffectをshellが捨てる」の検出で、W2の**Rust内部版**。重複しない。
  - Step 8a′で`on_connection_edge`を足すPRは、(a′)の分類表に1行(`InjectedLambda`等)を足すことになる。
  - Step 1(proptestの導入)は(h)の前提。Step 13のreplayは(d)の通りiOS側のW2の唯一の手段。

---

## 5. 段階的導入

### Phase 1: Android配線契約テスト(W1・W3)

- `WiringContractTest.kt`を追加。分類表の初期化は現状の全メソッド(`SessionOrchestrator` 35、`OrchestratorCallback` 19、
  トップレベル関数27、診断系9)を分類する(grandfathering。`NotUsedOnAndroid`の`ref`は出典コミットのハッシュ1つで可)。
- 台本に含めるOS由来入口は分類で決まる(初期の想定: `notifyDidEnterBackground`・`notifyWillEnterForeground`・`notifyNetworkPathChanged`・
  `notifyUpstreamHealthDegraded`。`notifyBackgroundBudgetExpired`/`notifyMemoryWarning`は`NotUsedOnAndroid(db3a6d87)`)。正確な一覧はPRで決める。
- **`notifyUpstreamHealthDegraded`の台本の前提(rev2、R2-2、確認済み)**: この入口は`executor.registerUpstreamFailoverMonitor`経由で、
  登録は`pane.upstreamFailoverEnabledForCurrentSession`が真のときだけ(`TerminalTabsViewModel.kt:831`)。このフラグは
  `TransportPreference.ISEKAI_PIPE_QUIC_MULTIPATH`の分岐でだけ`profile.enableUpstreamFailover`から設定される(`ConnectionCoordinator.kt:90-101`)。
  したがって台本は、両paneを`ISEKAI_PIPE_QUIC_MULTIPATH`かつ`enableUpstreamFailover = true`のプロファイルで接続し、paneごとに
  `DumbAppExecutor.simulateWifiUpstreamBroken(index)`(`DumbAppExecutor.kt:130`)を呼ぶ必要がある。台本でこの入口が観測されないとき、
  **分類を`NotUsedOnAndroid`等に変えてassertを「直す」ことはしない**(プロファイルの前提を直す)。
- **flake対策(レビューの観察)**: 同じハーネスの`TerminalTabsViewModelTest`には既知のflaky test(`withTimeout(3000)`のポーリング)がある。
  台本は`DumbAppExecutor.simulate*`の同期fan-outと`simulateConnected`だけで組み、`withTimeout`ポーリングは使わず、既存の
  `UnconfinedTestDispatcher(testScheduler)`の注入(`TerminalTabsViewModelTest.kt:75`)に揃える。
- UI入口のCompose台本(§3(a′)-6)は別のテストクラスにし、`TerminalHostScreenTest`のsetup(`:60-75`)を流用する。
- **ロールバック**: テストファイルの削除のみ。

### Phase 2: 既定no-op lambdaの撤廃とガード(W2・UI hop)

- 3ファイルの既定値削除(挙動保存)とテストのファクトリ化。`scripts/check-wiring-lint.py`(`--self-test`付き)・`scripts/wiring_lint.toml`・
  `.github/workflows/wiring-lint.yml`(`pull_request`はpaths無しで常時、`push: main`。`ubuntu-latest`、Pythonのみ。
  `room-migration-check.yml`と同じ「軽量なので常時実行」方針)を追加。iOSの字句レポート(警告)もこのjobで出す。
- **ロールバック**: workflowの削除(非requiredなのでprotection変更不要)。既定値削除は挙動保存なので残してよい。

### Phase 3: `dead_code`のcrate単位deny(W4・W7・W8の一部)

1. `isekai-terminal-core`の警告19件(項目約14、PRの規模は項目数で見積もる)を1PRで処理(残骸削除/`#[expect(dead_code, reason)]`)し、同じPRで`purity-check`に
   `cargo clippy -p isekai-terminal-core --lib -- -A clippy::all -D dead_code`を追加。`cfg!(debug_assertions)`禁止と
   モジュール先頭`#![allow(dead_code)]`の登録制を`check-wiring-lint.py`に追加。
2. `isekai-pipe`・`isekai-pipe-core`・`isekai-netmon`(各1〜2件)を同様に。
3. `isekai-ssh`: Linux/Windows両ログの積集合レポート(失敗させない)。
- **ロールバック**: 追加したclippy呼び出しの削除。

### Phase 4: argvの往復テスト(W5の一部)

- §3(h)。launch引数を次に変更するPRで行う(§10 U5)。
- **ロールバック**: PRごとrevert(型の共通化は挙動保存)。

---

## 6. CIの変更(まとめ)

- `android-test-check.yml`(required `android-unit-test`): 変更なし。Phase 1のテストと、Phase 2のコンパイル強制がこの中で走る。
- 新設`wiring-lint.yml`: job `name: wiring-lint`、`ubuntu-latest`、`python3 scripts/check-wiring-lint.py --self-test && python3 scripts/check-wiring-lint.py`。
  cargo/gradle/Swiftのセットアップ無し。数秒(推測)。
- `rust-core-test-check.yml`の`purity-check`job: Phase 3でcrate単位のclippy呼び出しを追加(新job不要)。`test-windows`job:
  `isekai-ssh`の警告一覧をartifactに出すstepを追加(非required)。
- `Cargo.lock`・Room: どのPhaseも変えない。UniFFI公開API: Phase 3の残骸削除で`#[uniffi::export]`を消す場合のみ再生成が要る(U8)。

---

## 7. 対象外(non-goals)

- iOSでの型解決付きの配線検証の強制(§3(a′)末尾、Periphery導入はU10で別判断)。
- OSの実エントリ(`ProcessLifecycleOwner`、`scenePhase`、`ConnectivityManager`)から本番Rustまでを実機/エミュレータで通すE2E。
  ただし最後のOS hopのうちネットワーク側は同じJVMスイートで後から検査できる(rev2、R2-6): `android/src/test/.../session/NetworkPathMonitorTest.kt`は
  既にRobolectricの`ConnectivityManager`に対して`NetworkPathMonitor`を動かし、`AndroidAppExecutorDownloadTest.kt`は`AndroidAppExecutor`を
  Robolectricで生成している(どちらもファイルの存在を確認済み)。「`registerNetworkCallbacks(a, l)`の後、shadowの登録callbackが空でなく、
  呼ぶと`a`/`l`が発火する」テストは書ける。ライフサイクル側(`ProcessLifecycleOwner.get()`をRobolectricで駆動できるか、
  `ProcessLifecycleInitializer`が走るか)は [EXT]。いずれも必須ではなく、後から追加可能とする。
- W5のうちargv以外(関数引数・env)とW6のCI検出。W6はFCIS ADR Step 6+7の共通化で扱う。
- callbackの`LogOnly`/`UiState`分類の正しさの機械検証。
- `unreachable_pub`による`pub`の一斉縮小。
- カバレッジ計測の常設(§3(e))。
- 既存の意図的な未配線(§1.3(1))を配線すること自体。

---

## 8. 検討した代替案

- **rev0のTOML台帳+正規表現(Android)**: forwarderで必ず素通り(F1)。却下(§3(a))。iOSでは警告レポートとしてだけ残す。
- **TOML台帳の`pending`に失敗する期限を付ける**: 単体テスト/字句スクリプトからは「PRが何に触れたか」が見えず、日付だけでmainが赤くなる。
  状態自体を廃止した(§3(f))。
- **Rustソースから公開シンボルを取る**: `#[uniffi::export]`のimplブロック・cfg・constructor・名前変換(`connectIsekaiStunP2p`のような
  UniFFIの変換規則)を再実装することになる。生成interfaceの反射+checksumシンボルで足りる。却下。
- **FCIS ADRのStep 13 goldenを全callbackに拡張**: platform → Rust方向を捕まえない上、高頻度callbackでgoldenが壊れやすい。却下(§3(d))。
- **カバレッジ**: 信号の向きが逆(e8ed36eeの実例)。却下(§3(e))。
- **`--workspace`一括の`-D dead_code`/`Cargo.toml`の`[lints]`**: `isekai-ssh`のプラットフォーム依存とvendor crateで成り立たない、
  後者はローカル・IDEにも効く。crate単位のCI呼び出しを採った(§3(b))。
- **閉じたイベントenum**: UniFFI公開APIの大きな変更でFCIS ADR §8と衝突。(a′)の分類表で同じ網羅性が得られる。却下(§3(i))。
- **Qodana**: (a′)で足りる。採らない(§3(i))。
- **FCIS ADR内のStepとして追加**: テーマ(reducer/Effect)と本クラス(配線)は直交する。別ADRとし、FCIS側のD5から参照する(§10 U1)。

---

## 9. リスク

- **分類表の形骸化**: `NotUsedOnAndroid`と書けば何でも通る。緩和: `ref`必須、`mirrored`相当は書面の例外必須、分類表がPR差分に出る。
  完全には防げない(§7)。
- **台本テストのflake**: Robolectricの負荷起因の失敗(既知)。台本は同期simulatorのみで組み、retryに頼らず直す。
- **並列worktree運用での分類表のマージ衝突**: 複数PRが同時にUniFFIメソッドを足すと、Kotlinの分類表(1メソッド1行、名前順)で隣接挿入が衝突しうる。
  1行1エントリにしておけば解決は機械的(推測)。
- **`-D dead_code`でmainが詰まる**: crate単位で、対象crateの既存違反を0にしたPRで同時に入れるので、導入時点の違反は無い。以後は新規の死にコードだけが落ちる。
- **`isekai-ssh`のdead_codeが見えないまま**: 積集合レポートが読まれなければ現状と同じ(F4)。積集合が小さければdenyへ進む(U4)。
- **反射の脆さ**: Kotlinのマングリング・UniFFI生成器の変更で列挙が変わる。checksumシンボル数との照合で、黙って縮むことは防ぐ。

---

## 10. Open Questions(ユーザー判断待ち、この一覧がすべて)

表の「既定案」は、ユーザーが何も指定しなければ実装時に採る案。

### 解決済み(rev1)

- **旧U3(`review_by`超過を失敗にするか)**: `pending`状態ごと廃止したので不要(§3(f))。
- **旧U7(Phase 4=`*_test`必須化の着手条件)**: 配線契約テスト(Phase 1)で置き換えたので不要。
- **旧U9(iOSコントローラへのPhase 2適用)**: `TerminalSessionController.init`に注入closureは無い(確認済み)ので適用不要。
  iOSの`TerminalView.swift`の既定値は新U9で扱う。

### 未解決

| # | 何を決めるか | いつ決めるか | 既定案 |
|---|---|---|---|
| **A** | **このADRをApproveするか**(round 2の敵対的レビューにかけるかを含む) | 今 | — |
| U1 | FCIS ADRのD5の扱い: 本ADRを別ADRとして採り、FCIS側D5を「本ADRへ移管」で解決済みにするか | Approve時 | 別ADR。FCIS ADRへはD5解決の1行amendmentのみ |
| U2 | `wiring-lint`(字句ガード)をrequired checkにするか | Phase 2導入後 | 字句ガードの弱点はFP(正当なコードを止める)なので、`--self-test`の既知形が揃い、数回のPRで誤検出が無いことを確認してからrequired化(`main-branch-protection.md`の手順)。配線契約テストは既にrequired job内なので対象外 |
| U4 | `isekai-ssh`の`dead_code`をどう扱うか | Phase 3の後 | Linux/Windows両ログの積集合レポートのみ(失敗させない)。積集合を0にできたらdenyを検討 |
| U5 | Phase 4(argv往復テスト)の実施時期 | launch引数を次に変更するPR | そのPRで実施。単独では行わない |
| U6 | カバレッジによる一度きりの棚卸しを行うか | 任意 | 行わない |
| U8 | 両プラットフォームで呼び出し0の6シンボル(§1.3(2))と、書面の例外が無いKotlinミラー`tmuxClaimedProfileIds`の扱い | Phase 1の分類表初期化時 | 分類表では`NotUsedOnAndroid(ref)`として記録。削除はUniFFI公開API変更(バインディング再生成)を伴うので別PR、各シンボルの導入意図を確認してから。`tmuxClaimedProfileIds`は「`rust-ssot.md`違反、Y-P2で解消」と記録 |
| U9 | iOSの`TerminalView.swift:811-816`の`= {}`既定値6個を字句ガードの対象に登録するか | Phase 2のPR | 登録する(字句ガードはLinuxで走るのでios-*の非required性に左右されない)。既定値撤廃に伴うSwift側の変更PRでは`ios-logic-linux-check`の緑をマージ条件にする |
| U10 | iOSにPeriphery(macOS、非required)の警告レポートを足すか | Phase 2の後 | 足さない。iOSで配線漏れの事例が出たら再検討 |
