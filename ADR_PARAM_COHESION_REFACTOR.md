# ADR: 引数過多・凝集性不足の是正リファクタ(5件)

- **Status**: **Accepted**(2026-09-05、round 1〜3でopus-critic-a・
  opus-critic-bへ独立並行レビューを依頼。round 1で§2.3の根本原因説明が
  事実誤認と判明し当初案を撤回・縮小、round 2で両者とも「設計面には異論
  なし」としつつround 1の修正自体が導入した算術ミス・型不一致(`token`の
  Option化・「14→9引数」)を独立に2件ずつ発見・訂正、round 3で両者とも
  「収束・設計への追加指摘なし」に到達した(詳細は§0改訂履歴)。round 3で
  残った軽微な文書整形指摘も反映済み)
- **ユーザー決定事項**(2026-09-05): 監査で見つかった5件の「引数過多/feature
  envy」ホットスポットを全部やりたい、という初期方針。ただしround 1レビューで
  §2.3(当初案)の根本原因説明そのものが事実誤認と判明したため、§2.3は
  「型統合」ではなく「意図的に別型であることの明文化」という縮小版に変更した
  (詳細は§1.3・§2.3・改訂履歴参照)。
- **対象**:
  - `rust-core/isekai-ssh/src/native/mux/client.rs`(`run_inner`)
  - `rust-core/isekai-pipe/src/engine/mod.rs`(`handle_connection`/`handle_attach_stream`)
  - `rust-core/src/helper_bootstrap.rs`(`IsekaiPipeP2pMode`。§2.3の対象は
    ドキュメント追記のみに縮小、`isekai-bootstrap/src/types.rs`側の変更は無し)
  - `android/src/main/kotlin/tools/isekai/terminal/ui/SshTerminalCanvas.kt`
    (`drawRow`/`redrawDirtyRows`)
  - `rust-core/isekai-pipe/src/resume_loop.rs`(`resume_with_backoff_until_deadline`)
- **入力**: 本セッションでの「引数過多/feature envy」監査(2回、根本原因分析込み)、
  および本ADRのround 1敵対的レビュー(`ADR_PARAM_COHESION_REFACTOR_REVIEW_ROUND1.md`、
  opus-critic-a・opus-critic-bへの独立並行依頼)。
- **拘束される既存ルール**: `CLAUDE.md`(`isekai-ssh`/`isekai-terminal-core`は
  「独立したcrate群」と明記。round 1レビューで、この独立性は**相互依存の禁止**
  であり一方向依存(`isekai-terminal-core → isekai-bootstrap`)は既存かつ許容
  済みと確認された——`rust-core/Cargo.toml:101-105`参照)、
  `.claude/rules/rust-ssot.md`(§2.4のKotlin側変更はUI描画状態のみでセッション/
  接続状態を含まないため抵触なし、round 1で確認済み)、
  `.claude/rules/uniffi-binding-regeneration.md`(下記の通り本ADRの範囲では
  不要と確認済み)、`.claude/rules/main-branch-protection.md`(§2.1が触る
  `isekai-ssh/src/native/`は`cfg(windows)`時のみ実行されるが全プラットフォーム
  でビルド・ユニットテストされるため、required checkの`rust-core-test-linux`
  でカバーされる——round 1時点の「Windows専用でrequired外」という評価は
  round 2で過大評価と訂正)。
  ~~`prefer-gh-actions-over-local-cargo.md`~~はこのリポジトリの
  `.claude/rules/`には存在しないファイルであり(round 1 opus-critic-a指摘)、
  round 0での引用は誤り。「ローカルbuild/test禁止、検証はCI経由」という方針
  自体はセッションメモリ由来として引き続き踏襲する。
- **UniFFIへの影響**: **なし**(round 1で両レビュアーが再確認済み)。対象関数は
  すべて`pub(crate)`または非公開(`run_inner`: `pub(crate)`、`handle_connection`/
  `handle_attach_stream`: private、`IsekaiPipeP2pMode`: `#[uniffi::export]`
  対象外)。`uniffi-binding-regeneration.md`のCIフロー
  (`regenerate-uniffi-bindings.yml`)は不要。

---

## 0. 改訂履歴

### Round 1(2026-09-05)— opus-critic-a・opus-critic-bへの独立並行レビュー

round 0 draftを両エージェントへ独立に(互いの指摘を見せずに)実コード裏取り込みで
レビューさせた。詳細は`ADR_PARAM_COHESION_REFACTOR_REVIEW_ROUND1.md`。要点:

| 変更 | 由来 |
|---|---|
| **§2.3を全面撤回・縮小**: 「`IsekaiPipeP2pMode`と`LaunchSpec`は互いを知らないまま2度設計された」という根本原因説明自体が事実誤認(`rust-core`は既に`isekai-bootstrap`に依存し`LaunchSpec`をimport可能)と両者が独立に指摘。`isekai-protocol`への型新設は撤回し、「意図的に別型であることをdocに明記する」最小コスト案に置換 | opus-critic-a S6・opus-critic-b B-1(両者独立に同一結論) |
| **§2.3の統合案が抱えていた個別の設計破綻も記録として残す**(撤回済みなので実装はしないが、将来同種の提案が出た時のために): `None`バリアントの行き場が無い、`Direct`/`Relay`のargv生成がbind機構から非対称、`Stun`バリアント先行追加は既に「対象外」と決定済みの判断を無自覚に覆す、フィールド差異比較表が3行とも事実誤認、解消される重複が9行に対し113箇所のdiffで費用対効果が成立しない、core側変更はCIで一切テストされない | opus-critic-a B1-B3・S1・S5・S6・S9・opus-critic-b B-2〜B-7 |
| **§2.1のコードブロックを実シグネチャに修正**: `term: String`(誤`Option<String>`)、`tty_exec: Option<String>`(誤`bool`、Frame::Helloにそのまま載る値なのでbool化は情報欠落)。I/O構造体化は分割借用の複雑さゆえ見送り、`PtySessionRequest`のみ導入(この時点では「14→9引数」としたが、round 2でこの算術自体が誤りと判明——§Round 2参照) | opus-critic-a/b 両者独立に同一結論 |
| **§2.2を`Arc<ServeConfig>`から`#[derive(Clone, Copy)] struct ServeConfig`へ変更**。理由も「タイポ耐性」から「secret(`relay_jwt`)の伝播範囲を広げない」「起動時設定と接続ごとの値の軸を混在させない」に差し替え。フィールド名は`effective_resume_grace`(`:1536`)に揃え`max_resume_grace_secs`とする | opus-critic-a S4・opus-critic-b S-4/S-5 |
| **§2.4を全面修正**: `glyphFallback`は`Typeface`ではなく`GlyphFallbackResolver`。`clearPaint: Paint`(`redrawDirtyRows`のみ保持)を追加。構築場所を「composable本体での`remember`」から「`Canvas{}`のDrawScope内、`baseline`確定直後にプレーンなローカル変数として毎フレーム構築」に修正(`cellW`/`baseline`等はDrawScope内でしか確定せず、`remember`可能な場所には存在しないため——実装するとcomposable側に置けずコンパイル不能、または点滅・テーマ更新が凍結する)。`data class`ではなく`internal class`(`private`型は`internal fun`のパラメータにできないコンパイルエラー、`equals`はPaintの参照比較目的では不要)。Robolectricテスト2箇所(`SshTerminalCanvasTest.kt:474,523`)の更新をスコープに追加 | opus-critic-a B4/B5・opus-critic-b S-6/S-7(両者独立に同一結論) |
| **§2.5の`max_resume_window`を`Duration`から`Option<Duration>`に修正**: `None`が「STUNリトライ時のgive-up通知を抑制する」という分岐条件そのものであり、`Duration`に潰すと通知スパムを引き起こす | opus-critic-a B6・opus-critic-b S-8(両者独立に同一結論) |
| **未決事項5点全てに結論を反映**(詳細は§5)。§4のリスク欄にあった「CLAUDE.md独立性への抵触」は、B1により論点自体が空振りと判明したため記述を整理 | opus-critic-a/b 両者 |
| ADR冒頭の「拘束される既存ルール」から実在しない`prefer-gh-actions-over-local-cargo.md`のパスを削除 | opus-critic-a S7 |

### Round 2(2026-09-05)— round 1改訂版の再確認、両者とも未収束と判定

round 1の修正を反映したADRを同じ2エージェントへ再送し、独立に再確認させた。
詳細は`ADR_PARAM_COHESION_REFACTOR_REVIEW_ROUND2.md`。両者とも「設計面には
異論なし(round 1のBLOCKING/SIGNIFICANTは全て意図通り解消)」としつつ、
round 1の修正自体が新たに導入した誤りを独立に2件ずつ発見した:

| 変更 | 由来 |
|---|---|
| **§2.1「14→9引数」は算術ミス、正しくは14→7引数**: `PtySessionRequest`が8フィールド(`term`/`cols`/`rows`/`host`/`remote_command`/`want_pty`/`tty_exec`/`token`)を吸収し、残る個別引数(`conn_read`/`conn_write`/`stdin`/`stdout`/`stderr`/`resize_rx`)6+`request`1=7。7はclippy閾値(7、8以上で発火)以内なので`#[allow(clippy::too_many_arguments)]`を実際に外せる(round 1時点の「9引数では外せない」は前提・結論とも逆) | round 2 opus-critic-a(R2-2)・opus-critic-b(N-2)、両者独立に同一結論。なおこの誤りはround 1時点で両者自身が算出したものが引き継がれたもの |
| **§2.1の`token`フィールドはOptionではなく`Vec<u8>`**: 実シグネチャ`token: &[u8]`は非Optionで、`Frame::Hello`生成時に無条件で`to_vec()`される。呼び出し元9箇所すべてが実トークンを渡すため`None`に相当する状態は存在せず、Option化は新たなパニック経路または認証拒否経路を生む | round 2 opus-critic-a(R2-1)・opus-critic-b(N-1)、両者独立に同一結論 |
| **§1.1の時間関連4値の記述を訂正**: 「`max_resume_window`だけがスカラー」は誤り(§2.5の`ResumeDeadlinePolicy`が4値すべてを包む設計と自己矛盾していた)。4値すべてがスカラーのまま、に訂正 | round 2 opus-critic-a(R2-3) |
| **§2.5・§3の行番号・required check評価を訂正**: `resume_loop.rs`の行番号(957→956/958)、§2.1が実際には`rust-core-test-linux`(required)でカバーされる(`native/mod.rs:4-6`が「全プラットフォームでビルド・ユニットテストされる」と明記——round 1の「Windows専用でrequired外」は過大評価だった) | round 2 opus-critic-a(R2-4/R2-5) |
| **§0の帰属訂正**: round 1のopus-critic-a分の表記から二重計上だったB4を除去(B4は§2.4の指摘で§2.3とは無関係)、clippy CI不在の指摘をopus-critic-b単独の指摘として整理 | round 2 opus-critic-a(R2-6) |
| **§2.4に3点補足**: `clearPaint`は`GridRenderStyle`に同居させ2クラスに分けない、`bitmapWidthPx`は意図的に個別引数のまま残す、`gridCache.planRender`/`markRendered`への配線変更は本ADRのスコープ外とし二重管理を増やさないよう実装時に判断 | round 2 opus-critic-b(R2-7相当) |
| **§2.3のdocコメント案に`launch_fingerprint`除外リストを3つ目の理由として追加**、参照先を一時文書(ADR/レビュー)から恒久ドキュメントへ変更 | round 2 opus-critic-b |

両者とも「上記の反映後はround 3で収束する見込み」「設計面には異論なし」と
結論。

### Round 3(2026-09-05)— 収束確認

round 2の修正を反映したADRを同じ2エージェントへ再送し、独立に最終確認させた。
**両者とも「収束・設計への追加指摘なし」**と判定。opus-critic-bは「技術的には
収束・追加指摘なし。BLOCKING・SIGNIFICANTはゼロ」、opus-critic-aは
「収束。round 1・round 2で自分が挙げた指摘は全22件すべて解消を確認、実装を
進めて問題ない」と結論した。

残った指摘は文書整形・引用精度のみ(いずれも設計・実装をブロックしない):
§0の表組みの一部がRound 2セクション末尾に取り残され表として描画されない
不具合(opus-critic-b M-1・opus-critic-a N-1、両者独立に発見、本改訂で修正)、
帰属ラベルの二重計上1件(opus-critic-b M-2、修正済み)、`engine/mod.rs`の
`Args`構造体・パーサの行番号精度(opus-critic-a N-2、修正済み)、§2.3内の
一時レビュー文書への内部採番参照(opus-critic-a N-3、恒久ドキュメント参照
方針との不整合を修正済み)、日付の1日ずれ(opus-critic-a N-4、修正済み)。
これらを反映した上で**Accepted**とする。

---

## 1. 背景・根本原因

### 1.1 パターンA: 機能追加のたびに1引数ずつ積み増された(§2.1, §2.5)

`run_inner`(`mux/client.rs:207`)は`term`→`cols/rows`→`want_pty`→`remote_command`→
`tty_exec`という順で機能フェーズごとに1引数ずつ増え、`#[allow(clippy::too_many_arguments)]`
で警告を都度握り潰してきた(round 1で行番号・allow実在を両レビュアーが確認済み)。
`resume_with_backoff_until_deadline`(`resume_loop.rs:946`)も同様に、
`state: &mut ResumeLoopState`は既に構造体化されているのに、後から追加された
時間関連4値(`resume_window`/`disconnected_at`/`deadline`/
`max_resume_window: Option<Duration>`)がスカラーのまま取り残されている
(round 2で「`max_resume_window`だけ」という誤記を訂正——4値すべてが対象)。

### 1.2 パターンB: 「設定」と「プロトコルコンテキスト」の概念混在(§2.2)

`handle_connection`/`handle_attach_stream`(`engine/mod.rs:903,1155`)は、
`isekai-pipe serve`起動時に一度だけ決まる設定値(`resume_buffer_size`/
`max_resume_grace_secs`)と、接続ごとに変わるプロトコルの値(`target`/
`session_secret`/`hello_bytes`)が同じ引数リストに並んでいる(round 1で
両レビュアーが「分析自体は正しい」と確認)。

### 1.3 修正: 「crate独立性の代償」という当初の根本原因説明は誤りだった

round 0では、`IsekaiPipeP2pMode`(isekai-terminal-core)と`LaunchSpec`
(isekai-bootstrap/isekai-ssh)が「互いを知らないまま2度設計された」ことを
§2.3の根本原因としていたが、**これは事実誤認だった**(round 1、opus-critic-a・
opus-critic-bが独立に指摘)。実際には`rust-core/Cargo.toml:105`に
`isekai-bootstrap = { path = "isekai-bootstrap" }`という一方向依存が既にあり、
`rust-core/src/helper_bootstrap.rs:17`が実際に`isekai-bootstrap`のAPIを
使用中。`LaunchSpec`は`isekai-bootstrap/src/lib.rs:40`で`pub use`されており、
isekai-terminal-coreは今日この瞬間からimportできる。

とはいえ、両者は「この2つの型は実際には**軸の違う切り方**であり、単純な型
統合はどちらにせよ危険」という点でも一致した(§2.3参照)——`IsekaiPipeP2pMode`
は「P2P方式の選択」のみを表し、`LaunchSpec`は「起動argv全体のポリシー」を
内包する。二重設計ではなく、意図的に異なる抽象度の型が2つ存在している。

### 1.4 パターンD: ループの不変値がローカル変数のまま関数分割された(§2.4)

`drawRow`/`redrawDirtyRows`(`SshTerminalCanvas.kt:407,481`)は、dirty-row最適化の
ために1つの描画ループ本体を機械的に関数分割した際(`drawRow`のdoc自身が
「描画結果は従来のインラインループと完全に一致する純粋なリファクタ」と記載、
round 1で確認済み)、ループ内不変値もローカル変数のまま個別引数として素通し
された。

---

## 2. 決定・詳細設計

### 2.1 `PtySessionRequest`構造体化(`mux/client.rs::run_inner`)

```rust
pub(crate) struct PtySessionRequest {
    pub term: String,
    pub cols: u16,
    pub rows: u16,
    pub host: String,
    pub remote_command: Option<String>,
    pub want_pty: bool,
    pub tty_exec: Option<String>,
    pub token: Vec<u8>, // 実シグネチャのtoken: &[u8]に対応(所有型化。
                         // client.rs:232のto_vec()呼び出しを前倒しするだけで、
                         // Optionではない——呼び出し元9箇所すべてが実トークンを
                         // 渡し、Noneに相当する状態は存在しない。round 2で訂正)
}
```

I/Oストリーム(`reader`/`writer`/`stdin`/`stdout`/`stderr`/`resize_rx`)は
`<CR, CW, I, O, E>`のジェネリクス・借用形の複雑さ(`select!`内の分割借用)から
**構造体化を見送り、個別引数のまま残す**(round 1 opus-critic-a/b 指摘)。
`run_inner`は14→7引数(個別I/O引数6+`request`1)に縮小する(round 2で
「14→9」という算術ミスを訂正)。7引数はclippyの`too_many_arguments`閾値
(7、8以上で発火)以内に収まるため、`#[allow(clippy::too_many_arguments)]`は
実際に外せる——ただしこのリポジトリのCIにclippyジョブが無く
(`.github/workflows/`全体をgrepして`cargo clippy`は0件、round 1
opus-critic-b指摘)外しても再増加を検知する効果は無いため、外すかどうかは
実装時の判断に委ねる。呼び出し元(本番1・テスト8箇所)の機械的書き換えを伴う。

### 2.2 `ServeConfig`によるCLI設定の集約(`engine/mod.rs`)

```rust
#[derive(Debug, Clone, Copy)]
struct ServeConfig {
    resume_buffer_size: usize,
    max_resume_grace_secs: u64,
}
```

(実装時の訂正: 利用箇所——`handle_connection`/`handle_attach_stream`——が全て同一
モジュール内のprivate関数のため、`pub(crate)`ではなくmodule-privateな`struct`で
十分。PR #113の実装レビューでopus-critic-bが指摘。)

手書きパーサ(`Args`構造体定義は`engine/mod.rs:81-140`、パーサは
`parse_args_from`(`:257`)の`next_val(&mut iter, "--resume-buffer-size")`方式、
clapではない——round 0の「clapで解析済み」という記述は誤りだったため訂正)
から一度だけ構築し、
`handle_connection`→`handle_attach_stream`へ`Copy`で渡す(`Arc`は不要——
16バイトのCopy値であり、`attach_runtime`/`sessions`が`Arc`なのは共有可変
状態だからで「流儀を揃える」ことに意味はない、round 1指摘)。
`target`/`session_secret`/`hello_bytes`(接続ごとに変わるプロトコル値)とは
分離し続ける理由は、単なる概念の違いだけでなく「`Args`は`relay_jwt`のような
secretを保持しており、専用structに絞ることでsecretの伝播範囲を広げない」
「起動時設定と接続ごとの値という軸を混在させない」の2点(round 1で
「タイポ耐性」という当初の理由を差し替え)。

### 2.3(縮小版)`IsekaiPipeP2pMode`/`LaunchSpec`の関係をdocコメントで明文化する

**当初案(`isekai-protocol`への`HelperLaunchOptions`新設)は撤回。**
round 1レビューで、この2型は「意図せず重複した概念」ではなく「異なる抽象度で
意図的に別々に設計された型」であり、統合するとargvレンダラの非対称
(`ADR_PARAM_COHESION_REFACTOR_REVIEW_ROUND1.md`のB2-B6/B-2〜B-7参照)により
`always-connects.md`が最優先とする接続経路の挙動を壊すリスクがあることが
判明したため。

代わりに、`rust-core/src/helper_bootstrap.rs`の`IsekaiPipeP2pMode`定義の
直前に、次のような対応関係を明記するdocコメントを追記するのみに留める:

```rust
/// `isekai-bootstrap::types::LaunchSpec`(isekai-ssh側)と対応する概念だが、
/// 意図的に別型として保つ:
/// - bind機構が異なる(このenumの利用先は`[::]:{port}`固定ポート・IPv6
///   dual-stack bind、`LaunchSpec::Direct`は`0.0.0.0:0`+ポート範囲——
///   Phase 9-4の実機検証に基づく設計判断)
/// - デフォルト値の方針が異なる(このenumの利用先は`--max-idle-lifetime`/
///   `--log-level`/`--resume-window`を一切渡さずisekai-helperの既定値に
///   任せるが、`LaunchSpec`はこれらを常に明示的に渡す)
/// - `launch_fingerprint`(isekai-bootstrap/src/reuse.rs)の除外フィールド
///   リストがLaunchSpec側にしか無い(helper再利用判定=常に接続できる原則の
///   中核。統合すると、この不変条件を知らないまま片方にフィールドを足す人が
///   静かに壊しうる)
/// 将来どちらかに統合する場合は、まずargv生成ロジック
/// (`helper_bootstrap.rs`の`launch_and_capture_handshake`と
/// `isekai-bootstrap/src/install_script.rs`の`build_install_script`)を
/// 共通化してから型を統合すること——型だけ先に
/// 統合すると、レンダラが対応していないフィールドが黙って無視される
/// (`ISEKAI_PIPE_DESIGN.md`のbootstrap関連Epic参照。ADRやレビュー文書は
/// 将来archive移動されうるため、恒久ドキュメントを参照する)。
```

`Stun`バリアントの`LaunchSpec`側への先行追加はしない——isekai-sshのbootstrap時
STUNは既に`Direct`+`stun_servers`として正規化済み(`isekai-bootstrap/src/
backend.rs:47`の`BootstrapBackend::install_and_start(..., stun_servers: &[SocketAddr])`、
`install_script.rs:218-221`)であり、`Stun`バリアントを足すと表現方法が2通りに
なる(`ADR_PARAM_COHESION_REFACTOR_REVIEW_ROUND1.md`のB4/B-4参照)。

このためPR2(旧: §2.3独立PR)は消滅し、このdocコメント追記はPR1に含める。

### 2.4 `GridRenderStyle`のDrawScope内構築(`SshTerminalCanvas.kt`)

```kotlin
internal class GridRenderStyle(
    val cellW: Float, val cellH: Float, val baseline: Float,
    val themeBgArgb: Int, val effectiveBlinkPhase: Boolean,
    val bgPaint: Paint, val textPaint: Paint, val clearPaint: Paint,
    val typeface: Typeface, val italicTypeface: Typeface,
    val glyphFallback: GlyphFallbackResolver,
)
```

`data class`ではなく`internal class`(`drawRow`/`redrawDirtyRows`は`internal
fun`であり、`private`型はそのシグネチャに置けずコンパイルエラーになる。
`equals`も、`Paint`が`equals`をoverrideしない以上、値比較としての意味を
持たないため不要)。

**構築場所は`remember`ではなく`Canvas {}`のDrawScope内、`baseline`が確定した
直後**(round 0の「`remember(typeface)`と同じ場所」は撤回——`cellW`/`baseline`
等はDrawScope内でしか確定せず、composable本体スコープにはまだ存在しない
ため、そもそも実装不可能だった)。この配置ならcomposable引数にも`remember`
キーにもならないため、Composeのrecomposition/`equals`判定は一切関与しない
(未決事項5への回答、round 1で確定)。`bgPaint`等は既存同様、同一インスタンスの
`.color`をループ中に書き換えて使い回す。`clearPaint`は`redrawDirtyRows`のみが
使う値だが、クラスを分けず`GridRenderStyle`に同居させ`drawRow`側は単に
参照しない(round 2で「クラスを2つに分ける」と誤読されないよう明記)。
`bitmapWidthPx`(`pixelW`由来)は`GridRenderStyle`に含めず、意図的に個別引数の
まま残す(スタイルではなくper-frameのBitmap実ピクセル幅のため、round 2追記)。
`gridCache.planRender(...)`/`markRendered(...)`にも`cellW`等の一部が個別に
渡っているが、これらは本ADRのスコープ外とし、ローカル変数と`style`フィールドの
二重管理を増やさないよう、この2関数への配線を変えるかどうかは実装時に判断する
(round 2 opus-critic-b指摘)。Robolectricテスト2箇所
(`SshTerminalCanvasTest.kt:474,523`、名前付き引数で`glyphFallback`等を渡している)
の更新をスコープに含める。

### 2.5 `ResumeDeadlinePolicy`構造体化(`resume_loop.rs`)

```rust
struct ResumeDeadlinePolicy {
    resume_window: Duration,
    disconnected_at: Instant,
    deadline: Instant,
    max_resume_window: Option<Duration>, // Durationではなく Option<Duration>
}
```

`max_resume_window`は`None`が「STUNリトライ時のgive-up通知を抑制する」という
分岐条件そのもの(宣言は`resume_loop.rs:956`、`:958`の
`notify_on_give_up = max_resume_window.is_none()`——round 2で行番号を訂正)
であり、`Duration`に潰すと通知スパムを引き起こす(round 1で修正)。
`state: &mut ResumeLoopState`とは別の第2引数として渡す。

---

## 3. 実施順序・PR構成

**単一PR構成に変更**(round 0のPR1/PR2分割は、PR2の対象だった§2.3が
ドキュメント追記のみに縮小されたため不要になった)。

- §2.1/§2.2/§2.4/§2.5/§2.3(docコメント追記のみ)を1PR、コミットは項目ごとに
  分ける(コミット規約通り)。
- §2.1(`mux/client.rs`)は`isekai-ssh/src/native/`配下にあり、この
  モジュールは`cfg(windows)`時のみ*実行*されるが、*ビルド・ユニットテスト*は
  全プラットフォームで行われる(`native/mod.rs:4-6`のdocコメントに明記、
  `#[cfg(test)] mod tests`にも`cfg(windows)`ゲートは無い)。よって§2.1が
  書き換えるテスト8箇所はrequired checkの`rust-core-test-linux`で実際に
  カバーされ、追加の手動確認は不要(round 2 opus-critic-a指摘——round 1時点の
  「Windows専用でrequired checks外、手動確認が必要」という評価は過大評価
  だった)。

## 4. リスク・トレードオフ

- §2.4の`internal class`化で、Composeの`remember`/recomposition判定への
  影響が懸念点として挙がっていたが、round 1でDrawScope内構築という配置を
  取ることでこの懸念自体が解消されることを確認した(§2.4参照)。
- §2.3を縮小したことで得られる実質的な効果は「ドキュメント上の対応関係の
  明記」のみであり、コード上の重複解消効果はゼロになった。これは
  round 1レビューで判明した「9行の型宣言を消すために113箇所のdiffを
  発生させる元案は費用対効果が成立しない」という結論を踏まえた、
  意図的なスコープ縮小である。

## 5. 未決事項(round 1–2で全て解消・結論反映済み)

1. ~~type aliasか完全置換か~~ → **どちらも採らない**。§2.3はdocコメント
   追記のみに縮小(§2.3参照)。
2. ~~`Stun`バリアント先行追加の是非~~ → **追加しない**。既存の
   `Direct+stun_servers`という正規化された表現と衝突するため。
3. ~~`ServeConfig`のスコープ~~ → 専用structは妥当、ただし`Arc`ではなく
   `Copy`(§2.2参照)。
4. ~~CLAUDE.mdの独立crate方針との整合~~ → 抵触しない(一方向依存は既存かつ
   許容理由も明記済み)が、そもそも§2.3当初案の前提自体が誤りだったため
   論点として空振りだった(§1.3参照)。
5. ~~GridRenderStyleのCompose recomposition影響~~ → DrawScope内でプレーン
   構築する配置を採れば影響なし(§2.4参照)。
