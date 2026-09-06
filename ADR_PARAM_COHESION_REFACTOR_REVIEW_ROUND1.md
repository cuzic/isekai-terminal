# Review Round 1: `ADR_PARAM_COHESION_REFACTOR.md`(opus 2体、独立並行レビュー)

opus-critic-a・opus-critic-bの2エージェントに、ADR draft(round 0)を独立に
(互いの指摘を見せずに)実コード裏取り込みでレビューさせた。両者が
**独立に同一の最重要結論**——「§2.3(`HelperLaunchOptions`統合)の根本原因説明
そのものが事実誤認であり、`rust-core`(isekai-terminal-core)は既に
`isekai-bootstrap`crateに依存していて`LaunchSpec`を今日から直接importできる」
——に到達したことは、指摘の信頼度が高いことの強い傍証。§2.1/§2.2/§2.4/§2.5の
提示コードブロックについても、両者が独立に同じ型不一致・仕様欠落を発見した。

以下、両者の指摘を統合・重複排除して記録する。個々の発言者の帰属は各項目末尾。

---

## BLOCKING

### B1. §2.3の統合前提「互いを知らないまま2度設計された」が事実誤認

`rust-core/Cargo.toml:105`に`isekai-bootstrap = { path = "isekai-bootstrap" }`
が既に存在し、`rust-core/src/helper_bootstrap.rs:17`が実際に
`isekai_bootstrap::client_candidates::fresh_bootstrap_request_v2`を使用中。
`LaunchSpec`は`isekai-bootstrap/src/lib.rs:40`で`pub use`済みなので、
`isekai-terminal-core`は今日この瞬間から`isekai_bootstrap::LaunchSpec`を
直接importできる。§2.3が解こうとしている「crate独立性の代償で概念が
2度設計された」という問題は存在せず、`isekai-protocol`への新設も
`isekai-ssh`側の呼び出し元変更も本質的には不要。
(opus-critic-a S6・opus-critic-b B-1、両者独立に同一結論)

### B2. 統合enumに`IsekaiPipeP2pMode::None`(最多数派の経路)の行き場がない

`helper_bootstrap.rs:106-113`の`None`バリアントは`isekai_pipe_quic_transport.rs:216`・
`multipath_transport.rs:952`という実プロダクション経路2つとテスト5箇所が使う本流だが、
ADR §2.3の`HelperLaunchOptions`(`Direct`/`Relay`/`Stun`の3バリアント)はこれの
マッピング先を規定していない。(opus-critic-a B1・opus-critic-b B-2)

### B3. `None`+`bind_port`と`LaunchSpec::Direct`は別メカニズムで、「型置換のみ」は成立しない

argv生成が非対称:
- core側(`helper_bootstrap.rs:301-304`)は`[::]:{port}`固定ポート・IPv6
  dual-stack bind(Phase 9-4実機検証済み設計)、`--max-idle-lifetime`/
  `--log-level`/`--resume-window`を一切出さない
- isekai-bootstrap側(`install_script.rs:270-277`)は`0.0.0.0:0`+ポート範囲、
  上記3フラグを常に出す

統合型を採用すると、レンダラが対応していないフィールド(`relay_transport: Qmux`
等)が黙って無視される「型だけ統一して実体が伴わない」状態になり、IPv6
dual-stack bindの恩恵も失われる。統合の前提は「argv置換ロジックの共通化」が
先であり、型統合はその後の副産物であるべき。
(opus-critic-a B3・opus-critic-b B-3、両者独立に同一結論)

### B4. `Stun`バリアント先行追加は、既に「対象外」と決定済みの判断の無自覚な覆し

`ISEKAI_PIPE_DESIGN.md:618`のEpic G(STUN自動bootstrap route)は**2026-07-09に
完了済み**であり「未実装」ではない。同`:164`は「STUN P2P自体をbootstrap時の
アップロード経路として使うこと」を明示的に対象外と決定済み。bootstrap時のSTUNは
既に`LaunchSpec::Direct`+`stun_servers`引数(`install_script.rs:216-221`)として
正規化済みで、`LaunchSpec::Stun`を追加すると「STUN付き起動」の表現方法が
`Direct{..}+stun_servers`と`Stun{..}`の2通りになり、二重設計を防ぐどころか
新規に作り出す。CLAUDE.mdが要求する「対象外と明記された判断を覆す場合は
経緯を踏まえること」に反する。(opus-critic-a B4・opus-critic-b B-4、両者独立に同一結論)

### B5. §1.3のフィールド差異比較表が3行とも事実と異なる

`idle_lifetime_secs`「Relay版のみ暗黙」→実際は全バリアントで不在。
`resume_window_secs`「RelayLaunchSpecのみ」→実際は`LaunchSpec::Direct`にも
存在(`types.rs:204-206`、`install_script.rs:263,276`で使用)。`stun_server`/
`punch_peer`「無し(未実装)」→isekai-ssh側STUNは実装済み。ADR §2.3の
提案enumは`Direct`から`resume_window_secs`を落としており、これは
`types.rs:155-165`が「exactly the bug this field exists to close」と明記した
既知バグ(resume-grace設定の無視)の再導入になる。(opus-critic-a S1・opus-critic-b B-5)

### B6. §2.3が解消する重複は実質ゼロ(費用対効果)

削除できる型宣言は`helper_bootstrap.rs:105-113`の9行のみ。一方で触る必要のある
参照箇所は`LaunchSpec`系91箇所/13ファイル+`IsekaiPipeP2pMode`22箇所/6ファイル=
**113箇所**。本当の重複であるargv組み立てロジック(`helper_bootstrap.rs:386-406`
vs `install_script.rs:227-279`)はB3の理由で1行も減らない。9行のために113箇所+
`always-connects.md`最優先経路のリスクを取るのは費用対効果として成立しない。
(opus-critic-b B-6)

### B7. §2.3のcore側変更はrequired checkで一切テストされない

`helper_bootstrap.rs:474-476`のテスト5件は`ISEKAI_PIPE_BOOTSTRAP_TEST_KEY`が
必要で、`.github/workflows/`全19ファイルにこの変数は0ヒット→CIで常にスキップ。
ADRが「変更後はe2eテストを重点的に回す」と書いても、最もリスクの高いAndroid
bootstrap経路は実行不能。`prefer-gh-actions-over-local-cargo`方針(セッション
メモリ由来、リポジトリのルールファイルではない——後述S7参照)によりローカル
実行も禁止のため、「誰も実行していないコードを書き換えて実機まで気づかない」
構図になる。(opus-critic-b B-7)

### B8. §2.4「`remember(typeface)`と同じ場所で1フレーム1回構築」はComposeの構造上不可能

`cellW`/`cellH`/`themeBgArgb`/`effectiveBlinkPhase`/`baseline`は`Canvas {}`の
**DrawScope内**で確定する値(`SshTerminalCanvas.kt:620-643`)であり、`remember`
呼び出し可能なcomposable本体スコープ(`:536`)にはまだ存在しない。無理に
composable側でrememberすると、`effectiveBlinkPhase`をキーに含めない限り
530ms周期の点滅(SGR 5・点滅カーソル)が停止し、含めれば`remember`が
実質無効化される。`baseline`も`FontFitCache`再フィット結果を跨いで古い値を
保持しフォントサイズ変更時にグリフがずれる。
**対処**: DrawScope内(`baseline`確定直後)でプレーンなローカル変数として
毎フレーム構築する。この配置なら composable境界を跨がないためComposeの
recomposition/`equals`判定は一切関与せず、未決事項5自体が消滅する。
(opus-critic-a B4・opus-critic-b B-3(S-7)、両者独立に同一結論)

### B9. `private data class`は`internal fun`のパラメータ型にできない

`drawRow`/`redrawDirtyRows`はどちらも`internal fun`。Kotlinは`internal`関数の
シグネチャに`private`型を置くとコンパイルエラーになる。加えて`redrawDirtyRows`
はRobolectricテスト2箇所(`SshTerminalCanvasTest.kt:474,523`)から直接呼ばれて
おりADRに更新対象として記載がない。**対処**: `internal class GridRenderStyle`
(`data`は付けない——B8の配置を採ればequalsは不要で、`Paint`は`equals`を
overrideしないため生成`equals`は事故のもとにしかならない)。
(opus-critic-a B5・opus-critic-b S-6/S-7)

### B10. §2.5の`max_resume_window: Duration`は`Option<Duration>`を潰しており、give-up通知の抑制ロジックを壊す

実際は`resume_loop.rs:955`が`Option<Duration>`で、`:957`の
`notify_on_give_up = max_resume_window.is_none()`がSTUNリトライ時のデスクトップ
通知spam抑止に使っている。`Duration`に潰すと`is_none()`が書けず通知が
スパムする。(opus-critic-a B6・opus-critic-b S-8、両者独立に同一結論)

---

## SIGNIFICANT(要修正だが実装は可能)

- **S1. §2.1のフィールド型が2箇所誤り**: `term: String`(ADRは`Option<String>`)、
  `tty_exec: Option<String>`(ADRは`bool`)。`tty_exec`は`Frame::Hello`にそのまま
  載るプロトコル値なのでbool化は情報欠落。`token: &[u8]`の帰属も未定義。
  (opus-critic-a/b 両者)
- **S2. §2.1の「2引数相当まで縮小」は非自明**: `run_inner`は`<CR,CW,I,O,E>`5
  ジェネリクス+所有権形がバラバラで、`select!`内の分割借用のためI/O構造体化は
  「機械的書き換えのみ」では済まない。**I/O構造体化は落とし、
  `PtySessionRequest`のみ導入(14→9引数)を推奨**——ただし9引数では
  `#[allow(clippy::too_many_arguments)]`(閾値7)を外せないため、ADRの
  「allowを外す」という記述も修正が必要。(opus-critic-a/b 両者)
- **S3. §2.1の「clippyで再増加を検知」は担保されない**: `.github/workflows/`
  全19本・`.claude/hooks/`にclippyは0ヒット、`clippy.toml`も無し。
  (opus-critic-a/b 両者)
- **S4. §2.2の効果はほぼゼロ**: 呼び出し元は各1箇所のみ、7→6/10→9で
  `#[allow]`は元々どちらにも無い(9引数では外せない)。「効果最小」ラベルは
  §2.5ではなく§2.2に付け替えるべき。(opus-critic-a/b 両者)
- **S5. `Arc<ServeConfig>`は過剰**: 中身は`usize`+`u64`=16バイトのCopy値。
  `#[derive(Clone, Copy)]`で十分、接続ごとのヒープ確保・refcountは純損。
  フィールド名は`effective_resume_grace`(`:1536`)に揃え`max_resume_grace_secs`
  を推奨。(opus-critic-a/b 両者)
- **S6. `RelayTransportKind`が既に2箇所に重複**しており(`isekai-bootstrap/src/types.rs:114`・
  `isekai-pipe/src/main.rs:94`)、`isekai-protocol`への移設が必要になるが
  ADRに記載がない。さらに`isekai-protocol/src/lib.rs`の自己定義する責務
  (byte layouts/JSON schemas)にargv仕様は該当せず、置くなら`isekai-bootstrap`。
  (opus-critic-a S5/未決4・opus-critic-b S-9)
- **S7. ADR冒頭が存在しないルールファイルを引用**: `.claude/rules/
  prefer-gh-actions-over-local-cargo.md`はこのリポジトリに存在しない
  (実体は`always-connects.md`/`main-branch-protection.md`/
  `parallel-worktree-agent-operations.md`/`rust-ssot.md`/
  `uniffi-binding-regeneration.md`/`worktree-artifact-sharing.md`の6本のみ)。
  「ローカルbuild/test禁止」の方針自体はセッションメモリ由来で正しいが、
  出典表記を直す必要がある。(opus-critic-a S7)
- **S8. §2.4のフィールドが型違い・欠落**: `glyphFallback`は`Typeface`ではなく
  `GlyphFallbackResolver`(`SshTerminalCanvas.kt:419`)。`redrawDirtyRows`のみが
  持つ`clearPaint: Paint`が欠落。(opus-critic-a/b 両者)
- **S9. `launch_fingerprint`(`reuse.rs:83-92`)の除外フィールドリストへの言及が無い**:
  `remote_log_level`/`idle_lifetime_secs`/`remote_bind_port_range`/
  `resume_window_secs`は意図的にfingerprintから除外されている
  (helper再利用判定=`always-connects.md`の中核)。統合型にすると、この
  除外リスト更新を忘れて静かに壊すリスクがある(既存メモリ
  `helper-reuse-stale-binary-gotcha`と同型の失敗モード)。(opus-critic-b S-10)
- **S10. PR1のWindows経路が`rust-core-test-windows`(required外)でしか
  検証されない**: §2.1(`mux/client.rs`)はWindows専用経路で、
  `main-branch-protection.md`の「対象外」節よりrequired checksでは
  検証されない。PR1マージ前の手動確認運用を§3に明記すべき。
  (opus-critic-b S-11)

---

## 「5. 未決事項」への結論(両者収束)

1. **type alias/完全置換のどちらも採らない。§2.3は現行スコープでは実施しない。**
   代替: (A)最小コスト案——`helper_bootstrap.rs`のdocに「`isekai_bootstrap::
   LaunchSpec`と対応関係にあるが、bind機構(`[::]:port`固定 vs `0.0.0.0:0`+range)
   とデフォルト値方針の違いから意図的に別型」と1段落追記するに留める。
   (B)統合するとしても`isekai-protocol`への新設ではなく、coreが既存の
   `isekai_bootstrap::LaunchSpec`を直接使う形にすれば、isekai-ssh側91箇所は
   無変更でcore側22箇所のみのdiffになる——ただしB3の挙動差(bind機構等)の
   解消は別途の設計判断が必要で、本ADRのスコープを超える。
2. **`Stun`バリアントの先行追加には反対。** 既存の`Direct+stun_servers`という
   正規化された表現と衝突する第2の表現を作るだけ。
3. **`ServeConfig`専用structは妥当だが`Arc`は不要**(`Copy`で十分)。理由も
   「タイポ耐性」ではなく「secretの伝播範囲を広げない」「設定軸を混在させない」
   に差し替える。
4. **CLAUDE.mdの独立crate方針には抵触しない**(一方向依存は既存かつ許容
   理由も`Cargo.toml`に明記済み)が、そもそもB1によりこの論点は空振り。
   真の設計判断は「共有先を`isekai-protocol`にするか`isekai-bootstrap`に
   するか」であり、後者を推奨(S6参照)。
5. **GridRenderStyleのrecomposition影響は懸念不要**——B8の配置
   (DrawScope内でプレーン構築、`remember`しない)を採れば`@Composable`の
   引数にもrememberキーにもならないため、Compose側の判定は一切関与しない。

---

## 出典一覧

- opus-critic-a: B1/B2/B3/B4/B5(=本ラウンドB2/B3/B4/B5/B10)、S1-S9(=本ラウンド
  S1-S9)、未決事項1-5への結論、検証して正しいと確認した6項目
- opus-critic-b: B-1〜B-7(=本ラウンドB1/B2/B3/B4/B5/B6/B7)、S-1〜S-11(=本ラウンド
  S1-S10)、未決事項1-5への結論、検証して正しいと確認した7項目
- 両者が完全独立にB1(§2.3前提の事実誤認)・B3(argvレンダラ非対称)・
  B4(Stunバリアント追加の是非)・B8(Compose配置)・B10(Option型)で
  同一結論に到達
