# ADR: isekai-ssh(Windows)の接続ライフサイクル可観測性

- **Status**: **Approved**(2026-09-11〜12起草・同日Approved。
  opus-adversarial-consult 4ラウンドで収束。Round 1: 当初案の中核前提
  (「Windowsネイティブ経路は`init_verbose`を一度も呼ばない」)がそもそも
  誤りだったと判明し、真因(§1.3、`isekai-pipe connect`子プロセスへの
  `ISEKAI_PIPE_LOG_FILE`未伝播)に差し替え、計装先を`stun_p2p.rs`から
  `resume.rs`/`telemetry.rs`へ訂正、tracing移行案を撤回。Round 2:
  `log_file.rs`の`truncate_over`のsemantics誤認(先頭切り詰めではなく
  open時一度きりの全削除)を指摘され、holder専用の実行時ローテーション
  設計に訂正。Round 3: §3.1/§3.2の矛盾・行番号誤り・holder判別方法・
  依存方針(`isekai-pipeは薄く`という既存決定)の4点を機械的に訂正して
  最終Approve)
- **対象**(見込み、要精査): `rust-core/isekai-ssh/src/native/child_stdio.rs`・
  `rust-core/isekai-pipe/src/connect.rs`・`rust-core/isekai-transport/src/
  resume.rs`・`rust-core/isekai-transport/src/telemetry.rs`
- **入力**: ユーザーが外出先(モバイル回線)から帰宅(自宅Wi-Fi/Tailscale)した
  タイミングでisekai-ssh(Windows、STUN P2P接続)が切断し、比較対象の別
  クライアント`tssh`は繋がり続けた、という相談。当初`ADR_CONNECTION_
  OBSERVABILITY.md`(isekai-terminal/Android向け)として着手したが、
  ユーザーから「isekai-ssh側の調査であってisekai-terminalではない」と
  訂正され本ADRとして独立に起票。Android向けADRはそのまま残し、Phase
  割り当ては変更しない(両ADRは独立)
- **拘束される既存ルール**: `.claude/rules/rust-ssot.md`、
  `.claude/rules/always-connects.md`(本ADRが直接支える)、
  `.claude/rules/prefer-gh-actions-over-local-cargo.md`、
  `ADR_STUN_REESTABLISH_CONTINUITY.md`(未実装のDraft。本ADRはこのADRの
  §3「現状、真の再ランデブーのケースは検知すらされていないのか」という
  未決の論点に直接答える前提作業)

---

## 1. 背景

### 1.1 何が起きたか

外出先(モバイル回線、STUN P2P接続)では接続を維持できていたが、帰宅して
自宅Wi-Fi/Tailscaleへネットワークが変わったタイミングでisekai-ssh
(Windows)の接続が切れ、比較対象の別クライアント(`tssh`)は繋がり続けた。
朝になって気づいたため、発生時点の内部状態は一切追えなかった。

### 1.2 Round 1レビューで判明した誤診断と、その修正

**当初案は「Windowsネイティブ経路では`init_verbose()`が一度も呼ばれて
おらず、既存の`log_line_verbose!`呼び出しが全滅している」と主張していたが、
これは誤り。** `wrapper.rs:499-510`の`init_logging()`は、Unixエントリ
ポイント(`wrapper::run`)と**Windowsネイティブエントリポイント
(`native::connect::prepare_with_tofu`、`connect.rs:182-192`)の両方から
呼ばれる共有ヘルパー**として既に実装されている。しかも`prepare_with_tofu`
自身のコメントが、「この関数を各エントリポイントが個別にif/else-ifで
実装していた時代に、まさにこの配線(native側の`init_logging`相当)が
一度実際に丸ごと欠落し、Windows CIで検出された」という経緯を明記して
おり、その再発防止のために共有ヘルパー化された、という**既に一度
直っているクラスのバグ**だった。当初案はこの修正後のコードを読んで
「漏れている」と逆方向に誤診断していた——`init_verbose`という文字列を
直接呼んでいる箇所だけをgrepし、`init_logging`という共有ヘルパー経由の
間接呼び出しを見落としたのが原因。

### 1.3 実際の欠落箇所(Round 1レビューで特定)

isekai-ssh自身はSTUN P2P/QUICを一切喋らない——`Cargo.toml`に
`isekai-transport`/`noq`/`quicmux`への依存が無く、実際にSTUN P2P/QUICを
扱うのは**別プロセス`isekai-pipe connect --stdio`**
(`native/child_stdio.rs::spawn_isekai_pipe_connect`が起動する子プロセス)。

`isekai-pipe connect`は`ISEKAI_PIPE_LOG_FILE`環境変数を見て、設定されて
いればそこへ、無ければ(継承した)stderrへ`env_logger`で書く
(`isekai-pipe/src/connect.rs`)。この環境変数を設定しているのは:

- **Unix経路(`wrapper.rs:1139-1152`)**: `plan.isekai.log_file.is_none()`
  (`--isekai-log-file`未指定)の時のみ、`default_log_file()`を
  `ISEKAI_PIPE_LOG_FILE`として子に渡す。明示指定時はこの変数を敢えて
  未設定のままにし、代わりに子のstderrを`wrapper.rs`側でpipeして
  ユーザー指定の1ファイルに集約する(`is_enabled()`分岐)。
- **Windowsネイティブ経路(`native/child_stdio.rs:57-70`)**: 現状
  `ISEKAI_INTENT_ID`/`ISEKAI_PIPE_RUNTIME_DIR`の2つしか子のenvに
  設定しておらず、**`ISEKAI_PIPE_LOG_FILE`を一切渡していない**。かつ
  native経路は子のstderrをpipeせず`inherit`するだけ(Unixのような
  集約経路が無い)。

この結果、`isekai-pipe connect`子プロセスの診断出力(STUN P2P/QUICの
実際の挙動が書かれる場所)は継承したstderrへ行くのみになる:

- foregroundクライアント配下で起動された子: 一時的にコンソールへ
  (再現・保存されない)。
- **holder(`native/mux/holder.rs:114-118`、`.stdout(Stdio::null())`/
  `.stderr(Stdio::null())`で起動される長寿命detachedプロセス)配下で
  起動された子: holderのnull stderrへ吸われて完全に消える。**

`holder`はenvブロックを丸ごと差し替えたりしない(`HOLDER_MARKER_ENV`と
いうマーカー1つを追加するだけ、`env_clear()`も無し)ため、
`default_log_file()`が解決するパス自体はforegroundクライアントと
holderで一致する——`ISEKAI_PIPE_LOG_FILE`さえ設定すれば両者の子のログを
同じファイルへ集約できる、という意味では単純。ただしholderは長寿命
プロセスでありログの無限成長対策(ローテーション)が要るため、実際には
holder配下だけ別ファイルへ分ける設計にする(§3.2)。ローミングを跨いで
生き続けるのは実際にはholder配下の`isekai-pipe connect`であり、今回の
インシデントで欲しかった情報はまさにここにあった。

### 1.4 `ADR_STUN_REESTABLISH_CONTINUITY.md`との関係、および計装先の訂正

同ADR(2026-09-07起票、未実装のDraft)は、クライアント・サーバー双方の
実効アドレスが同時に変わる「真の再ランデブー」で、resume連続性
(scrollback)は失うが接続自体は続くはずという既知のギャップを記録して
おり、「そもそも検知すらされていないのか」を未決の論点として残している。

当初案はこの計装先を`isekai_transport::stun_p2p`に置こうとしていたが、
Round 1レビューにより誤りと判明: `stun_p2p.rs`はステートレスな
per-candidate確立関数群で、モジュールdoc自身が「full re-rendezvousは
out of scope、signalingチャネルが無い」と明言しており、新旧
`SessionId`の対応関係を持つ状態機械はこのモジュールには存在しない。

**実際に「セッションを諦めて新しいIDを振る」判断が行われているのは
`isekai-transport/src/resume.rs`の`GenerationCoordinator`
(`random_session_id()`で新規`SessionId`を発行、~402行)と、完全に
諦めた場合の`SequentialConnectError::GaveUpAfterGenerationRetries`
(同ファイル、~474行/~496行の2箇所で返される)。** さらに
`isekai-transport/src/telemetry.rs`(289行)という、この種の判断を
`log::info!`/`log::warn!`ベースの`key=value`平文1行として既に記録する
専用モジュール(`log_candidate_attempt`/`log_generation_advance`/
`log_must_resume_convergence`)が既に存在する——今回追加する計装は
この既存の規約(`tracing`ではなく`log`マクロ+`key=value`)に合わせて
拡張するのが筋であり、新しいログ体系を持ち込む必要は無い。

## 2. 目的・非目的

**目的**: Windows/STUN P2P経路で、真のネットワークroaming(クライアント・
サーバー双方のアドレス変化)を跨いだ際に何が起きたかを、発生後に本人が
確実に再構成できるようにする。特に「継続性を失っただけ(新セッションで
繋がっている)」と「完全に諦めて切断した」を区別できる粒度にする。

**非目的**(Round 1レビューを受けて追加):
- **`tracing`クレートの導入・Subscriber設計の刷新は行わない**
  (当初案の§3.3を撤回)。`isekai-ssh`自身の`main.rs::run()`に
  Subscriberを置いても、実際にSTUN/QUICを喋る`isekai-pipe connect`は
  別プロセスなので何も観測できない——プロセス境界を取り違えていた。
  また`isekai-transport::telemetry`は既に`log`ベースの`key=value`
  規約で機能しており、置き換える理由が無い。`ADR_CONNECTION_
  OBSERVABILITY.md`(Android向け)のtracing化はそちらのADRのスコープに
  留め、混同しない。
- `ADR_STUN_REESTABLISH_CONTINUITY.md`が扱う「連続性そのものを保てないか」
  という設計課題の解決。本ADRはあくまで**観測**であり、その判断材料を
  提供する前段。
- `ADR_INPUT_RESUME_SYMMETRY.md`/`ADR_ISEKAI_SSH_LOCAL_SCROLLBACK.md`
  (同じ2026-09-07セッション由来の別ADR、いずれも未実装)への波及。
- Prometheus等の集計metricsパイプライン(単一ユーザー・単一デバイスの
  個人用途)。

## 3. 設計

### 3.1 真因の是正: Windowsネイティブ経路でも`ISEKAI_PIPE_LOG_FILE`を渡す

`native/child_stdio.rs::spawn_isekai_pipe_connect`(57-70行目)の
`.env("ISEKAI_PIPE_RUNTIME_DIR", runtime_dir)`(59行目)の直後に、
`ISEKAI_PIPE_LOG_FILE`を設定する分岐を足す。ただしUnix側
(`wrapper.rs:1139-1152`)の`if plan.isekai.log_file.is_none()`ガードを
そのまま移植してはいけない——Unixはこのガードが真の時(既定動作)だけ
envを設定し、`--isekai-log-file`明示時は代わりに子のstderrをpipeして
集約する。**nativeパスは子のstderrをpipeせず`inherit`するだけで、
このpipe集約経路自体が存在しない**ため、Unixと同じガードを移植すると
`--isekai-log-file`指定時にholder配下でログが消える穴がそのまま残る。
正しくは(優先順位): (1) `--isekai-log-file`明示指定(`plan.log_file()`
がSome)があれば最優先でその値、(2) 無指定かつholder経由なら§3.2の
holder専用パス、(3) それ以外(無指定・foreground)は`default_log_file()`
——**いずれの場合も**`ISEKAI_PIPE_LOG_FILE`として設定する(native独自の
分岐)。`spawn_isekai_pipe_connect`は現状`(isekai_pipe_path, runtime_dir,
intent)`の3引数で`plan`を受け取っていないため、この変更には`plan`
(または`plan.log_file()`の値)を渡す第4引数の追加を伴う——production
呼び出し元は`native/connect.rs:647`の1箇所(`plan`はスコープ内)、
ユニットテストは`child_stdio.rs:206`が追随する。

holderはenvブロックを差し替えないため、`default_log_file()`の解決結果は
foregroundクライアントとholderで一致する(=env継承が効いている証拠)。
ただし**実際にどちらのパスへ`ISEKAI_PIPE_LOG_FILE`を向けるかはholder経由
かどうかで分ける**(§3.2で詳述——holderは長寿命でローテーションが要る
ため、foregroundと同じ無制限追記ファイルは共有しない)。副次効果として、
`connect.rs`の`env_logger`は出力先がファイルの時だけ`\r\x1b[K`(進行中の
再描画クリア)を出さない設計になっている(391-396行)ため、常時ファイル
出力にする本設計はいずれの経路でもANSIエスケープ混入を自動的に防ぐ。

### 3.2 保持: `isekai-pipe`側ファイル出力にサイズ上限が無い(Round 2で
### 設計を訂正——「既存ロジックの再利用」では解決しない)

`isekai-pipe/src/connect.rs::open_log_file_target`(359-364行)は
`OpenOptions::new().create(true).append(true)`で開くだけで上限が無い。
`isekai-ssh`側`log_file.rs::Sink::open`の`truncate_over`(78-89行)を
「再利用すればよい」としていたが、これは**実行時ローテーションではない**
——`open()`が呼ばれた瞬間(=プロセス起動時)に1回だけサイズを見て、
超過していれば**ファイルごと削除**するだけの仕組みで、先頭を切り詰めて
以後書き続ける動作ではない。holderは1プロセスが日〜週単位で生き続ける
前提なので、この「起動時1回きり」の判定ではholder自身の寿命の中で
一切上限が効かず、無限成長を防げない。さらに、当初案(foreground/holder
双方が同じファイルへ追記する設計)にこの`remove_file`方式を素朴に
適用すると、後から起動したforegroundクライアントの初期化が**holderが
書いている最中のファイルを削除してしまう**(Windowsでは使用中ファイルの
削除がプロセス間で不整合な挙動になりうる)——これも§3.1でforeground/
holderのログ先を分ける理由の1つ。

正しい対応は、**holder用に実行時ローテーションが効く別経路**を用意する
こと:
- **foregroundクライアント配下の子はこれまで通り`default_log_file()`
  (無制限追記)のままでよい**——1回のssh接続の間しか生きないプロセスで、
  無限成長の懸念自体が無い。
- **holder配下の子だけ、別の(ローテーション対応の)ログパスへ向ける。**
  `isekai-pipe/Cargo.toml`のログ系依存は`log`+`env_logger`の2つのみで
  `tracing`系は無く、`ADR_ISEKAI_SSH_LOCAL_SCROLLBACK.md`のユーザー
  決定事項(「isekai-pipeは薄くありたい」)は明確に依存追加を避ける
  方向を示している。`tracing-appender`は`tracing-subscriber`を
  芋づる式に引き込むため(「単なるWrite実装として使う」つもりでも
  依存グラフにはtracingエコシステム一式が入る)採用しない。代わりに
  `isekai-ssh`側`log_file.rs::Sink`の構造(`append_bytes`相当の箇所に
  現在サイズを判定し、閾値超過時にrenameしてから新規オープンする
  数十行程度のロジック)を雛形に、`isekai-pipe`側で自前実装する。
  holder経由かどうかの判定は、呼び出し元にフラグを引き回さず
  `native::mux::holder::is_holder_reexec()`(既存、`HOLDER_MARKER_ENV`を
  見るだけの1行関数、`main.rs`の起動時分岐が既に使っている)を
  `spawn_isekai_pipe_connect`内部で直接呼んで判定する。holder経由の
  時だけ別のログファイル名(例: `isekai-ssh-holder.log`)を
  `ISEKAI_PIPE_LOG_FILE`として渡す——単一ファイル共有にはこだわらず
  役割ごとに分ける。

### 3.3 真の再ランデブー境界の計装(`telemetry.rs`の既存規約を拡張)

`isekai-transport/src/telemetry.rs`に、`log_generation_advance`/
`log_must_resume_convergence`と同列の関数を1本追加する
(仮称`log_rendezvous_outcome`、既存と同じ`log::info!`/`key=value`規約):

```rust
pub fn log_rendezvous_outcome(
    previous_session_id: SessionId,
    new_session_id: Option<SessionId>, // Noneならbare redial(同一ID)
    class: &str,                       // "bare-redial" | "fresh-rendezvous" | "abandoned"
    attempts: u32,
    elapsed: Duration,
)
```

呼び出し箇所:
1. `GenerationCoordinator::new(random_session_id())`で新規`SessionId`を
   発行する箇所(`resume.rs`~402行付近)——`class = "fresh-rendezvous"`。
2. `SequentialConnectError::GaveUpAfterGenerationRetries`を返す2箇所
   (`resume.rs`~474行・~496行)、および`AllCandidatesFailed`を返す
   `resume.rs:566`("every candidate cleanly failed pre-attach in a
   single round"——原因は異なるが結果は同じく「完全に諦めた」)の
   計3箇所で`class = "abandoned"`を記録する。原因(generation retry
   枯渇 vs 全candidate即時失敗)を区別したい場合は`class`値をさらに
   分けてもよいが、本ADRでは「接続できなかった」という粒度で1つに
   まとめる。
3. (bare redialの既存経路があれば同様に1行追加、無ければ本ADRの
   スコープでは追わない——`ADR_MIDSESSION_DISCONNECT_RECOVERY.md`
   Task 3.4で「一度だけログ」が既に実装済みとされている経路)。

これにより`ADR_STUN_REESTABLISH_CONTINUITY.md` §3の未決論点
(「真の再ランデブーは検知されているか」)に直接答えられるようになる。

### 3.4 相関ID

既存の`SessionId`/`ISEKAI_INTENT_ID`をそのまま使う。新しいID型は作らない。

## 4. 検証方法

`prefer-gh-actions-over-local-cargo`によりローカルbuild/testは行わない。

- §3.3は`GenerationCoordinator`の分岐選択自体はLinux CIの
  `cargo test -p isekai-transport`で検証できる(`log::info!`が実際に
  呼ばれたかまでは通常の`log`セットアップでは検証しづらいため、分岐が
  正しい引数で通ることを確認する)。
- §3.1/§3.2はWindows実機でしか検証できない
  (`.claude/rules/parallel-worktree-agent-operations.md`が既に警告する
  通り、`native/`は全プラットフォームでビルド・単体テストされるが
  実際に*呼ばれる*のはWindowsのみ)。`isekai-ssh-remote-build`スキル
  経由でのユーザー実機検証、またはWindows CI(`test-windows`、現状
  mainのrequired checksには未登録)を想定する。
- 実機再現手順: Windows機でモバイルテザリング→自宅Wi-Fiの手動切り替えで
  真の再ランデブーを誘発し、`isekai-ssh.log`(foreground経由)と
  `isekai-ssh-holder.log`(holder経由)のうちholder側に
  `isekai-pipe connect`のログが現れること、`log_rendezvous_outcome`の
  イベントが記録されることを確認する。

## 5. Open Questions

- §3.2: holder用ログファイル名(`isekai-ssh-holder.log`案)と、自前実装
  するローテーションの閾値・世代数の具体値。
- `tssh`側が同じ状況で繋がり続けた具体的な理由は依然未調査。本ADRの
  計装が入れば次回の同種インシデントで直接比較できるが、現時点では
  仮説のまま。
