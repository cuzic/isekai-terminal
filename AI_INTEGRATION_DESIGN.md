# AI統合機能 設計書（草案、2026-07-24）

> ステータス: 設計段階、大半は未着手。`PLAN.md`へのPhase番号割り当ては実装着手時に行う
> （2026-07-24時点でPhase 11/12はKeystoreKekベースvault移行に予約済みのため、
> ここでは番号を占有しない。`MACRO_DESIGN.md`と並行して設計中の別機能であり、
> 両者は§6.3/6.4がscreen_text_snapshot()を共有する形で連携する）。
> **例外(2026-07-25更新)**: §6.1(ctl Notify)はワイヤプロトコル・CLI・送出側
> (claude-hookd、`ISEKAI_PIPE_DESIGN.md` Epic Q)が実装済み(このworktree上、未マージ)。
> 受信側のバックグラウンドシステム通知は`isekai-ssh`(OSC9ポップアップ)・Android本体
> アプリ(`TabAlertNotifier`、2026-07-25追加)ともに実装済み。「状態ドット」・通知
> タップでのタブジャンプはどちらも未実装のまま——詳細は§6.1・§9 Epic AI-8/AI-9。
> §6.2(構造化パネル)はこのworktreeには存在しないが、未マージの別ブランチ
> `feat/ai-rich-panel-cost-viz`にプロトタイプがある(§9 Epic AI-7参照)。複数機能を
> 組み合わせた実用例・応用アイデアは§11にまとめた。

## 1. 概要

isekai-terminalに、Claude(Anthropicアカウント)を使ったAI機能を追加する。動機は
「Claude Code前提で機能強化したい」というプロダクト方針であり、次の4つを実現する:

1. リモートでClaude Code等を実行中のセッションが入力待ち/完了になったことをモバイルに通知する。
2. リモートのAIエージェントが提示する構造化コンテンツ(フォーム・ドキュメント)を、
   生のVT100テキストでなくネイティブUIパネルとして表示する。
3. モバイルでの長文タイピングを避けるため、自然言語からシェルコマンドを生成する。
4. コマンド実行後(特に失敗時)に、AIが次の一手を複数選択肢として提案する。

きっかけは`github.com/receptron/mulmoterminal`(ブラウザ上でClaude Code/Codexの
並行セッションを監督するツール)の調査。ただし同ツールはNode.jsサーバー+Socket.IO+
ブラウザという構成が前提であり、isekai-terminal(Android単体・SSH直結・独自バックエンド
非依存)にそのまま持ち込める部分と持ち込めない部分がある。本設計書はその選別の記録である。

## 2. 検討の経緯・却下した案

- **AIモデル呼び出しをリモートホスト側(ユーザーが既にSSH先に入れているclaude/codex/
  aider等)に任せ、アプリはUIだけ提供する案**: 当初検討したが却下。ユーザーの方針
  (2026-07-24)により、AI呼び出しは**BYO AI**としてクライアント(Androidアプリ/rust-core)
  から直接Claudeを呼ぶ方式を採る。理由: Claude Code前提で機能強化する方針が明確化された
  ため、リモート側にAIツールの有無を仮定しない設計の方が一貫性がある。
- **APCを新規の私的制御シーケンスとして新設する案**: 却下。`rust-core/src/kitty_graphics.rs`
  の`ApcInterceptor`が既にKitty graphics(#53)用に`ESC _ … ST`を捕捉しており
  (`session_state.rs:398`→`terminal.rs:1010`の`dispatch_kitty_apc`)、2つ目のAPC
  インターセプタを足すのは筋が悪い。既存インターセプタへの**ペイロード先頭バイトでの
  名前空間分岐**(`G`=Kitty graphics、新しいprefixバイト=AIパネルエンベロープ)として
  設計し直す(Opusレビュー、2026-07-24で指摘)。
- **注目通知もAPC経由にする案**: 却下。ttyを持たない/tmux越しのhookから確実に届く
  必要があるため、既に帯域外・型付きの`isekai-protocol/src/ctl.rs`(Epic M)へ
  `Notify`メッセージを追加する方式にする(§5参照)。
- **`~/.claude/projects/<cwd>/*.jsonl`を読んでコスト/コンテキスト%を表示する案**:
  優先度を大きく下げた(§6.5)。非公開・不安定なフォーマットに依存する上、リモートの
  ファイル読み出し経路を新設する必要があり、価値/実装コスト比が悪い(Opusレビュー指摘)。
- **画面内容のAI送信前リダクション(サーバー側で自動マスキング)**: 本設計のスコープ外。
  リダクションは原理的に不完全(何が秘密情報かをヒューリスティックで判定しきれない)
  ため、「リダクションはしない、ユーザーが送信内容に同意する」方針にする(§7参照)。
  自動マスキングは将来の追加検討候補として残すのみ。

## 3. 設計不変条件(AIとSSH接続経路の分離)

**AI機能の障害・遅延・認証失効は、SSH接続そのものの成否に一切影響してはならない。**
`.claude/rules/always-connects.md`の「常に接続できる」原則と衝突しないよう、以下を
必須の設計制約とする:

- AI機能はすべて既定OFF(opt-in)。`PLAN.md` Phase 7-7/9で確立した「opportunistic」
  設計方針(使えない環境では黙ってフォールバック)をAI機能にも適用する。
- OAuth認証切れ・API呼び出し失敗・レート制限は、対象のAI機能をサイレントに無効化する
  だけで、SSH接続・既存のセッション状態には一切波及させない。
- AI機能のRust側実装は、`SessionOrchestrator`(接続状態のSSOT)とは独立したモジュールに
  置き、接続状態機械に新しい分岐を持ち込まない。

## 4. 認証: Claude(Anthropicアカウント)OAuth / BYO AI

**現時点で未確定**(タスク: Claude(Anthropicアカウント)OAuthの実現可能性・規約を確認する、
実装着手前に結論を出す)。対象はClaude Code CLI固有のOAuthではなく、Claude Pro/Max/Team
等の**Anthropicアカウントそのもの**のOAuth(claude.ai/Claude Desktop/Claude Code CLIが
共通して使っている認証基盤)。

- 第三者アプリからの利用が規約上・技術上サポートされているか要確認。コミュニティに
  よる非公式な再実装例は存在するが、Anthropicが正式に第三者アプリ向けにOAuthクライアント
  登録を公開しているかは別問題であり、確認が取れるまでは以下の2方式を並行して設計しておく:
  - **方式A(OAuth)**: 実現可能と確認できた場合、Custom Tabs経由でAnthropicの認可
    エンドポイントに遷移し、ユーザーのClaude Pro/Max/Teamサブスクリプション枠でAPIを
    呼び出す。
  - **方式B(APIキー、フォールバック)**: OAuthが規約上グレー/不可の場合、Anthropic
    Console発行のAPIキーをユーザーに直接入力してもらう従量課金方式を既定にする。
- **rust-ssot.md準拠**: HTTP呼び出し・トークンのライフサイクル判断(refresh/失効/
  再認証要否)はRust側(rust-core)に置く。Kotlin側の責務は次の2つのみ:
  1. Custom Tabsでのリダイレクト受領(方式Aの場合)、またはAPIキー入力フォームの表示(方式B)。
  2. Rust側からのコールバック(認証状態・エラー)を受けてUIに反映する。
  Kotlin側に「今認証済みか」等のミラー状態を持たせない。
- rust-core/Cargo.tomlには現状HTTPクライアント依存が無いため、新規追加が必要
  (reqwest等、pure Rustでのmusl/Android NDKビルドとの相性を確認すること)。
- **KeystoreKekベースvaultの非対称性**: AndroidはPhase 12予約で未実装、iOSは
  `CredentialVault`として実装済み。本機能をAndroid vault完成に依存させると着手が
  大幅に遅れるため、暫定の保管インターフェース(トレイト)を切り、vault実装完了後に
  差し替え可能にする。

## 5. アーキテクチャ: 2チャネル構成

isekai-terminal版の構造化メッセージは、方向によって経路を分ける(MulmoTerminalの
`gui-chat-protocol`のように単一エンベロープに統一しない、Opusレビューで指摘された
訂正を反映):

| 方向 | 用途 | 経路 |
|---|---|---|
| リモート→デバイス、その場に描画するコンテンツ | 構造化パネル(presentForm/presentDocument相当) | APC名前空間拡張(§6.2) |
| リモート→デバイス、状態変化の通知 | 注目通知(attention/waiting) | 既存ctlソケットへの`Notify`追加(§6.1) |
| デバイス内で完結するAI呼び出し | 自然言語→シェル変換、次アクション提案 | どちらも不要。Compose側でAI応答を直接レンダリング |

3行目が重要な区別: 自然言語→シェル変換(§6.3)・次アクション提案(§6.4)は、
**リモートから何かを受信するわけではない**。Androidアプリ自身がClaude APIを呼び、
その応答をその場でCompose UIに表示するだけなので、新しいワイヤプロトコルは要らない。

## 6. 機能別設計

### 6.1 リモートセッションの注目通知(ctl Notify)

> **実装状況(2026-07-25更新)**: ワイヤプロトコル・CLI・送出側(claude-hookd)は
> **実装済み**(このworktree上、未マージ)。受信側は`isekai-ssh`(OSC9ポップアップ)・
> Android本体アプリ(バックグラウンドシステム通知、2026-07-25追加)ともに実装済み。
> 「状態ドット」(タブバーの永続的な視覚インジケータ)・通知タップでの該当タブへの
> ジャンプは、当初の設計文にあった記述だが**いずれのkind・クライアントについても
> 未実装のまま**——下記参照。以下は当初の設計文のまま残しつつ、実装済み/未実装を明記する。

既存の`bell_generation`(rust-core/src/terminal.rs:425-434、タスク#24/#25で実装済み、
BEL受信を取りこぼし無く検知するカウンタ)は、現状`TerminalTabsViewModel.kt:251-254`で
150msの振動を鳴らすだけで止まっている。これを次のように拡張する、という当初案だったが、
実装は**別カウンタ**として着地した(下記「実装済みの実際の姿」参照)。

- `isekai-protocol/src/ctl.rs`の`CtlMessage`enum(`SetTitle`/`ClipboardPush`/`SetVar`
  等が既にある、`isekai-pipe ctl setvar`のようなCLIサブコマンド経由でリモートから
  送出される設計と同型)に`Notify { kind: NotifyKind, title: String, body: String }`
  を追加する(host→device、fire-and-forget、`SetTitle`と同じパターン)。
- Claude Code hook(`UserPromptSubmit`/`Stop`/`Notification`)から
  `isekai-pipe ctl notify --kind waiting --title "..." --body "..."`のような
  CLI呼び出しでctlソケットへ送出する設定をユーザー自身の`.claude/settings.json`に
  追加してもらう(MulmoTerminalの`--settings`によるhook注入と同種の発想だが、
  isekai-terminal側はHTTPエンドポイントでなくctlソケットで受ける)。
- 受信側: タブバーの各タブに状態ドット(非フォーカスタブで`Notify`受信時に表示)、
  アプリバックグラウンド時はシステム通知(タップで該当タブへジャンプ)。
- AI認証(§4)もAPCエンベロープ(§6.2)も不要な独立した経路のため、他のAI機能より
  先に実装・実機検証できる。

**実装済みの実際の姿(2026-07-25、上記との差分)**:

- **`Notify`は独立した経路ではなく、無関係な`#57` tmux hook通知(`PLAN.md`タスク#57)と
  同じワイヤ型に統合されている**: `CtlMessage::Notify { kind: NotifyKind, tmux_tag:
  String, seq: u64, title: String, body: String }`(`isekai-protocol/src/ctl.rs:213-273`)。
  `NotifyKind`は計7バリアント: `Waiting`/`Done`/`Info`(本節が扱うAI系、`title`/`body`
  使用)、`Bell`/`Activity`/`Silence`/`JobDone`(tmux hook系、`tmux_tag`/`seq`使用)。
  2026-07-25、両者が同じ`op":"notify"`タグへ後から統合されたための結果であり、上記の
  「AI認証もAPCエンベロープも不要な独立した経路」という説明はもう正確ではない
  (バリデーション・dispatch・受信側の`when`分岐はすべて7バリアント全てを意識する
  必要がある)。
- **`isekai-pipe ctl notify`のCLI引数は上記の例と異なる**: `--title`/`--body`という
  名前付きフラグではなく、実際は位置引数
  (`isekai-pipe ctl notify --kind waiting <title> <body>`)。tmux系は
  `--kind bell --tag <tag> --seq <n>`。
- **呼び出し元は「hookスクリプトが`ctl notify`を直接叩く」設計から変わった**:
  実際に本番で使われる経路は`isekai-pipe claude-hookd event`(`ISEKAI_PIPE_DESIGN.md`
  「Epic Q」)——常駐daemonがdebounce付き状態機械を持ち、同一プロセス内で
  `send_ctl_message`を直接呼ぶ(`isekai-pipe/src/ctl.rs:849-853`が`pub(crate)`化されて
  いる理由)。`isekai-pipe ctl notify` CLI自体は変わらず存在し手動呼び出しにも使えるが、
  §6.1本文が想定する「hookが直接`ctl notify`を呼ぶ」構成は現状claude-hookdに
  置き換わっている。
- **受信側UXはクライアント2種それぞれで別実装**: `isekai-ssh`(Windows Terminal等
  OSC9対応端末向け)は**実装済み** — `rust-core/isekai-ssh/src/ctl_forward.rs:495-513`
  `osc_sequence_for`が`Waiting`/`Done`/`Info`を`\x1b]9;{title}: {body}\x07`(OSC 9、
  iTerm2/Growl系の「システム通知を出す」規約)へ変換し、ローカル端末へそのまま書き出す
  (Unix版・Windows-native版で共有)。tmux系kind(`Bell`等)はここでは意図的に`None`
  (「Android appの仕事」とコメントで明記)。**Android本体アプリ側も2026-07-25に
  実装済み**: Rust側は`notify_generation`という`bell_generation`とは別の新しい単調
  増加カウンタ(`terminal.rs:436-441`)でAI系`Notify`を検知し、Kotlin側
  (`TerminalTabsViewModel.kt`の`onNotify`コールバック)がログ出力に加えて
  `TabAlertNotifier.notify()`(タスク#57のtmux hook系kindと全く同じ経路、
  §11.1.4参照)を呼ぶ。ただしRust側`notify_from_ctl`(`session.rs:877`)には
  tmux hook系kindが持つ「アプリがフォアグラウンド、かつ当該タブ表示中なら抑制する」
  という判断が無いため、当該タブを見ている最中でも通知が出うる(既知の差異、
  `TabAlertNotifier`クラスdocコメント参照)。「状態ドット」・通知タップでのタブ
  ジャンプは、tmux系kind(`Bell`/`Activity`/`Silence`/`JobDone`、`PLAN.md`タスク#57)
  も含めどのkindについても未実装のまま(`TabAlertNotifier`が提供するのは一過性の
  システム通知のみ)。
- **iOS受信側は空実装のまま**: `ios/Sources/IsekaiTerminalCore/TerminalSessionController.swift:989`
  の`onNotify(kind:)`は本体が空(`{}`)。UniFFIの配線自体は生成済みで動くが、
  アプリ側の実装が何もしていない。

### 6.2 構造化パネル(APC名前空間拡張)

`rust-core/src/kitty_graphics.rs`の`ApcInterceptor`(既存、Kitty graphics専用)に
名前空間分岐を追加する:

- ペイロード先頭バイトで振り分け: `G`=既存のKitty graphics、新規prefixバイト(要選定、
  Kitty自身の予約と衝突しない値)=AIパネルエンベロープ。
- 既存の`MAX_APC_PAYLOAD`(32MB)・truncation・8bit `0x9f`非対応の判断をそのまま踏襲する
  (`kitty_graphics.rs:67`)。
- 最小スキーマ(案、`gui-chat-protocol`のToolResultを参考にするが独自定義):
  `{ "type": "presentForm" | "presentDocument", "title": string, "fields"/"markdown": ... }`。
  MulmoTerminalの`html-plugin`/`collection-plugin`のような任意HTML/JS系は、Composeに
  安全な相当物が無いため対象外。
- **信頼境界**: ペイロードはPTY上のin-bandであり、リモートの任意プロセス・`cat`した
  悪意あるファイル・curlの出力が偽造できる。したがって:
  - エンベロープに実行権限を一切与えない(自動実行・クリップボード書き込み・認証
    プロンプト誘発は不可)。
  - パネルは常に「リモート由来」であることをUI上明示する(枠線色・ラベル等)。
  - 表示レート/頻度を制限し、偽装スパム通知経路になることを防ぐ。
  - **実装(2026-07-25追記)**: 上記2点(opt-in既定OFF、表示レート制限)は当初
    `Terminal::dispatch_apc`に実装されておらず、opt-inしていないユーザーにも任意の
    リモートバイト列由来のダイアログが無制限に表示され得るバグとして
    コードレビューで指摘された。`Terminal::panel_enabled`(既定`false`、
    `ConnectionProfile.enableAiPanel` → `SessionOrchestrator::set_ai_panel_enabled`
    → `SessionCmd::SetPanelEnabled`という`set_theme`と同じ配線経由でのみ変わる)と
    `PANEL_MIN_UPDATE_INTERVAL`(2秒、`Terminal::panel_rate_limit_allows`)で
    `dispatch_apc`をゲートすることで修正済み。
- **フィードバック**: フォーム送信結果はPTYへの通常stdin文字列書き込みで返す
  (MulmoTerminal自身の`docs/gui-protocol-spike.md`で実機検証済みの方式: Claude
  CodeのTUIがテキスト+CRをペーストと誤認しないよう、CRを遅延させる実装上の工夫が
  必要)。`isekai-protocol/src/ctl.rs`の「no general-purpose exec RPC」方針
  (`ctl.rs:180`、`BuildRequest`がコマンドでなくプロファイル名しか渡せない設計と
  同じ理由)を踏襲し、新しいctl-socket経由のexec相当は追加しない。
- 優先度は他のAI機能より低い(§6.3/6.4が先)。

### 6.3 自然言語→シェルコマンド変換

- 呼び出し口: アクセサリバーに専用ボタン(例:「✨ Ask AI」)、既存の
  `SnippetPickerSheet`的なボトムシートUIパターンを踏襲。
- AI呼び出し: §4のAI認証経由でClaude APIをクライアント(Android/rust-core)から
  直接呼び出す。直近の画面出力(`screen_text_snapshot()`、MACRO_DESIGN.md §5・本設計と
  共有する基盤、未実装)をコンテキストとして渡す。
- **確認UI**: 生成コマンドは即実行せず、Run/Edit/Dismissの確認を必須にする
  (TermAI/Warp等の先行事例、自動実行しない)。
- **誤タップ対策**: モバイルはIME表示/非表示でレイアウトが動くため、事故実行の
  リスクが高い。危険コマンド分類器をRust側に置き、破壊的コマンド(`rm`/`reboot`等)は
  二段確認を必須にする(MACRO_DESIGN.md §8の安全性方針と統一)。

### 6.4 コマンド実行後の複数選択肢AI提案

Warpの"Active AI(Next Command)"を参考に、コマンド実行後(特に失敗時)にAIが
2-3個の次アクション候補を未依頼で提案する。

- トリガー: OSC133由来のコマンド終了検知(既存`last_command_output_text()`、
  `terminal.rs:2121`)または`screen_text_snapshot()`の画面静定検知。
- 提案生成: §4のAI認証経由でClaude APIを呼び出し、直近の出力+終了コード+cwdを
  コンテキストとして渡す。
- 表示: タップで選択するチップUI(Compose側で完結、リモートからの受信は不要)。
- 確認UI: 選択したコマンドも即実行せずRun/Edit/Dismiss確認を必須にする。
- **コスト/レイテンシ**: 全コマンド完了ごとに毎回AI呼び出しすると課金・体感速度の
  問題になるため、既定は明示的トリガー(ボタン押下)にする。コマンド完了ごとの
  自動提案は将来のopt-in機能として明確に区別する。
- 実装順序: §6.3(自然言語→シェル変換)で認証・コンテキスト取得・確認UIの基盤を
  検証してから本機能に展開する。

### 6.5 (低優先度・再検討)コスト/コンテキスト%可視化

`~/.claude/projects/<cwd>/*.jsonl`からトークン使用量・コンテキスト使用率を読み取り、
タブヘッダーにバッジ表示する案。Opusレビューで価値/変更頻度比が悪いと指摘されたため、
他のAI機能が一通り実装・価値実証された後に再検討する。実装する場合もDoze/バッテリー
制約からポーリングでなくpush型(§6.1のctl `Notify`経由でリモートhookから能動的に
送出)にする。

## 7. 同意・リダクション・opt-in方針

Warpが「セッション内容を同意なくLLMに送っている」とHNで批判された事例と同型の
リスクがあるため、§6.3/6.4の実装前に以下を確定させる:

- 機能自体をホスト単位/プロファイル単位でopt-in(既定OFF)にする。
- 初回有効化時に、画面内容がClaude APIへ送信される旨の明示的同意UIを表示する。
- リモートの本番サーバーの画面には認証情報・秘密情報が映り得るが、自動リダクションは
  原理的に不完全なため実装しない。「リダクションはしない、ユーザー責任である」旨を
  同意UIで明示する方針にする(§2で却下した案の理由を参照)。
- MACRO_DESIGN.md §8の「秘密情報はマクロ本体に保存しない」という既存の安全性方針との
  整合を取る(AI機能側も同様に、パスワード/OTP等をコンテキストに含めない配慮を検討)。

## 8. 安全性

- AI生成コマンドはいかなる機能でも自動実行しない(Run/Edit/Dismissを必須にする)。
- 破壊的コマンドは二段確認(§6.3参照)。
- APCパネル・ctl Notifyはいずれもリモートの任意プロセスが偽造できる前提で、
  実行権限を持たせない(§6.1/6.2の信頼境界を参照)。
- AI認証情報(OAuthトークン/APIキー)は§4の暫定vaultインターフェース経由で保管し、
  平文でファイル・ログに残さない。

## 9. 既知のギャップ・今後の課題

- **Epic AI-1: Claude(Anthropicアカウント)OAuthの実現可能性・規約 — 未着手**:
  §4参照。結論が出るまで方式A/Bの両方を設計上並行させる。
- **Epic AI-2: リモートへのhook配置主体 — v1方針決定(2026-07-25、`ISEKAI_PIPE_DESIGN.md`
  Epic Q参照)**: 自動配布はしない、ユーザー自身が`.claude/settings.json`に手動追加する
  方針で確定(「no general-purpose exec RPC」方針との整合を優先)。将来の増分として
  自動配布は引き続き検討候補。
- **Epic AI-3: env var伝播 — 部分的に回答(2026-07-25、`ISEKAI_PIPE_DESIGN.md` Epic Q参照)**:
  `$ISEKAI_CTL_SOCK`と同じexport機構に相乗りする`$ISEKAI_TAB_IDLE_COLOR`について、
  tmuxの別接続跨ぎ再アタッチ時に古い値が残る既知の制限はEpic Mと同じ扱い(サイレント
  失敗、次の新規ペインで解決)と整理した。sudo/docker越しの伝播は未調査のまま。
- **Epic AI-4: Claude Codeのバージョン検出とhookスキーマ変動 — 未調査**。
- **Epic AI-5: Doze中のトークンrefresh — 未調査**: Android Doze modeでの
  バックグラウンドOAuthトークン更新の扱い。
- **Epic AI-6: screen_text_snapshot()の実装 — MACRO_DESIGN.md側の構想のみで
  未実装**。§6.3/6.4・MACRO_DESIGN.md §5の`wait-for-pattern`が共有する基盤として、
  AI機能着手前に先出しする。
- **Epic AI-7: APC新規prefixバイトの選定 — 未調査(ただし未マージブランチに参考実装あり)**:
  Kitty自身の予約・他ターミナル(iTerm2/kitty/wezterm)のAPC/OSC私的利用との衝突を避ける
  値を選ぶ必要がある。**参考**: 未マージの`feat/ai-rich-panel-cost-viz`ブランチ
  (このworktreeには無い、2026-07-24、`rust-core/src/ai_panel.rs`)に§6.2のプロトタイプ
  実装があり、そこでは新規prefixバイトを予約せず「ペイロード先頭バイトが`{`ならJSON
  エンベロープ」という内容ベースの分岐で済ませている(Kittyは`G`のまま)。本セクションを
  正式に着手する際は、そのブランチから設計を再導出せずまず参照すること。ただし同ブランチの
  実装は§6.2が要求する「表示レート/頻度の制限」(スパム対策)を実装しておらず、フォーム
  送信フィードバックの「CRを遅延させる工夫」も実装されていない(通常のstdin書き込みを
  1回行うのみ)——マージ時にはこの2点を要フォロー。
- **Epic AI-8: ctl Notifyの受信側UXがAndroid本体アプリで未実装 — 新規、2026-07-25判明、
  バックグラウンドシステム通知は同日解消**: §6.1参照。当初(発見時点)は
  `RemoteLogger`へのログ出力止まりだったが、tmux hook系kind(`PLAN.md`タスク#57〜#63)
  向けの`TabAlertNotifier`をAI系kindからも呼ぶよう`TerminalTabsViewModel.kt`の
  `onNotify`を配線し、バックグラウンドシステム通知は解消した(§11.1.4参照、
  `isekai-ssh`のOSC9ポップアップと合わせクライアント2種とも通知自体は届くように
  なった)。**残課題**: (a) Rust側`notify_from_ctl`にtmux hook系kindのような
  フォアグラウンド/タブフォーカス抑制が無いため、当該タブ表示中でも通知が出る
  (既知の差異のまま残す判断、影響は小さいとみて今回は見送り)。(b)「状態ドット」
  (タブバーの永続的な視覚インジケータ)・通知タップでのタブジャンプは、tmux hook系
  kindも含めどのkindについても未実装のまま——これは新規UIコンポーネントの追加が
  必要で低コストではないため、今回のスコープには含めなかった。
- **Epic AI-9: ctl Notifyの受信側がiOSに存在しない — 新規、2026-07-25判明、
  低コストではないと判明**: `TerminalSessionController.swift`の`onNotify(kind:)`は
  空実装。UniFFI配線自体は生成済みで動作するが、**Android版のような「既存実装への
  一行配線」では済まない**: iOS側`ConnectionProfile`相当のモデル
  (`ProfileDatabase.swift`/`ProfileEditView.swift`)には
  `enableTabNotifications`に相当する永続化フィールドが無く(`TmuxTabWindowCoordinator`
  経由の`enableNotifications`は呼び出し側で`false`固定——§11.2.3参照)、
  `UNUserNotificationCenter`の利用実績もiOS側に一切無い。実装するには
  (1)プロファイルモデルへのフィールド追加、(2)設定画面へのトグル追加、
  (3)通知権限リクエストフロー、(4)Android`TabAlertNotifier`相当のローカル通知
  投稿ロジック、の4点が新規に必要で、Android側の対応より着手コストが高い
  (2026-07-25、当初「Swift側に繋ぐだけ」としていた見積もりを訂正)。

## 10. 参照

- `.claude/rules/rust-ssot.md`: Rustを状態/意思決定ロジックのSSOTにする原則
- `.claude/rules/always-connects.md`: 「常に接続できる」原則(§3の設計不変条件の根拠)
- `rust-core/src/kitty_graphics.rs`: `ApcInterceptor`(§6.2で拡張する既存実装、
  `MAX_APC_PAYLOAD`は67行目)
- `rust-core/src/session_state.rs:398`: 既存APC分岐(`ApcStep::Apc`→
  `dispatch_kitty_apc`)
- `rust-core/src/terminal.rs:1010`: `dispatch_kitty_apc`、`terminal.rs:425-434`:
  `bell_generation`(§6.1で汎用化する既存実装)、`terminal.rs:2121`:
  `last_command_output_text()`(§6.4のトリガー候補)
- `rust-core/isekai-protocol/src/ctl.rs`: `CtlMessage`enum(§6.1で`Notify`を
  追加する既存の帯域外制御チャネル、138-204行目)、180行目「no general-purpose
  exec RPC」方針。実装済みの`CtlMessage::Notify`/`NotifyKind`(7バリアント)は213-273行目、
  バリデーションは382-413行目
- `rust-core/isekai-pipe/src/ctl.rs`: `isekai-pipe ctl notify` CLI実装(432-509行目)、
  `send_ctl_message`(849-853行目、`pub(crate)`化されておりclaude-hookd daemonが
  同一プロセス内で直接呼ぶ)
- `rust-core/isekai-pipe/src/claude_hookd/daemon.rs:242-267`: `send_notify_popup`
  (§6.1の`Notify`を組み立てて送出する、現状の実際の送出元)
- `rust-core/src/terminal.rs:436-441`: `notify_generation`(§6.1のAI系`Notify`検知用、
  `bell_generation`とは別の独立したカウンタとして実装された)
- `android/src/main/kotlin/tools/isekai/terminal/TerminalTabsViewModel.kt:252-283`:
  `onNotify`/`onNotifyRequested`コールバック配線(2026-07-25、両kindファミリーとも
  `TabAlertNotifier`へ)
- `android/src/main/kotlin/tools/isekai/terminal/TabAlertNotifier.kt`: バックグラウンド
  システム通知の実装(状態ドット・タップでジャンプは未実装)。`notify()`の`message`
  引数(2026-07-25追加)でAI系kindの実際のtitle/bodyを渡せる
- `ios/Sources/IsekaiTerminalCore/TerminalSessionController.swift:989`:
  `onNotify(kind:)`(Epic AI-9、空実装、Android版より着手コストが高い理由は§9参照)
- `MACRO_DESIGN.md` §5: `screen_text_snapshot()`構想(Epic AI-6で本設計と共有)、
  §8: 安全性方針(§7で整合を取る)
- MulmoTerminal(`github.com/receptron/mulmoterminal`)調査結果(2026-07-24):
  `gui-chat-protocol`(`{toolName, data}`エンベロープ)、`docs/gui-protocol-spike.md`
  (PTY stdinへのフィードバック書き戻し方式)
- 外部prior art(2026-07-24調査): Warp(Active AI/Next Command、HN炎上事例含む)、
  GitHub Copilot CLI、Amazon Q Developer CLI、TermAI(モバイルAI SSHクライアント、
  Run/Edit/Dismiss確認UI)

## 11. 機能を組み合わせた実用例・応用アイデア

本節は個々の機能節(§6等)や`PLAN.md`/`ISEKAI_PIPE_DESIGN.md`の各Epicを横断し、
「複数の機能を一緒に使うと何が嬉しいか」を具体例つきでまとめる(2026-07-25追加)。
§11.1は現時点のコードで実際に動く組み合わせ、§11.2はまだ実装されていないが既存基盤の
上に乗せられる応用アイデア。**重要な前提**: 注目通知(ctl Notify)の受信側は
クライアント・kindによって実装状況が違う(§6.1参照)——`isekai-ssh`(Windows Terminal等
OSC9対応端末)とAndroid本体アプリはバックグラウンドシステム通知については両方とも
実装済み(2026-07-25時点)だが、iOS本体アプリは空実装のまま、かつ「状態ドット」・
通知タップでのタブジャンプはどのクライアント・kindでも未実装。以下では組み合わせごとに
どのクライアントを前提にしているか明記する。

### 11.1 すでに動く組み合わせ(検証済み)

#### 11.1.1 `isekai-ssh` + Windows Terminalでの並列Claude Codeダッシュボード

**組み合わせる機能**: `claude-hookd`(`ISEKAI_PIPE_DESIGN.md` Epic Q、`session_id`単位の
Idle/Attention状態機械)× tmuxの複数ペイン(ユーザー自身が管理する、isekai-terminal本体の
tmux統合`PLAN.md`タスク#57〜#63とは別物)× `SetTabColor`(Windows Terminalタブ背景色)×
`ctl notify`のOSC 9ポップアップ(`ctl_forward.rs`)。

**具体例**: 開発者が1台のリモートホスト上のtmuxセッションに、それぞれ別のリポジトリ/
タスクを担当する5〜10個のClaude Codeインスタンスをtmuxペインとして並べて走らせる。
各ペインは同じ`$ISEKAI_CTL_SOCK`(=同じisekai-sshのタブ/接続)を共有するが、daemonは
`session_id`ごとに独立してIdle/Attentionを追跡する(`PLAN.md`「既知の制限」参照)。
いずれか1つのペインが`AskUserQuestion`や権限確認で止まると、そのタブ全体が
attention色に変わり、同時に一度だけOSC 9のシステム通知ポップアップが飛ぶ
(集約値がidleからattentionへ変わる瞬間のみ、同一`session_id`内のdebounceでは再送しない)。

**効果**: Windows Terminal上でタブを何十個も開いて並列にAIエージェントを走らせている
状況でも、通知音やバイブレーションに頼らず「どのタブに戻る必要があるか」を色だけで
判別できる。ポーリングも常駐監視プロセスも不要(daemonは最初のhook呼び出しで遅延起動し、
1時間イベントが無ければ自発的に終了する opportunistic 設計)。

#### 11.1.2 Android/iOSアプリでの切断耐性tmux監視

**組み合わせる機能**: QUIC接続耐性(Phase 7〜9、ローミング・resume)× tmuxウィンドウ
マッピング(`PLAN.md`タスク#60)× reconnect時scrollback backfill(タスク#58)×
tmux hook注目通知(タスク#57、`TabAlertNotifier`)。こちらは`claude-hookd`とは無関係の
機能群で、Claude Codeを使っていないtmuxセッションでも成立する。

**具体例**: 電車で移動しながら、リモートホストのtmuxペインでビルドやテストなど数分
かかるジョブを走らせている。トンネルで一時的に電波が切れ、resumeが間に合わずフル
reconnectになった場合でも、アプリは同じ`TmuxTag`を持つ同じtmuxウィンドウへ再接続する
(新しいシェルが立ち上がるのではない)。再接続直後に、切断中にそのペインへ溜まった
出力が`capture-pane`でscrollbackへbackfillされるため、何が起きていたかを画面を
スクロールして確認できる。あわせて`alert-activity`/`alert-silence`/`pane-died`のtmux
hookが有効化されていれば(タブごとのopt-in、`ConnectionProfile.enableTabNotifications`)、
バックグラウンド中でもジョブの進行/完了がAndroidのシステム通知として届く。

**効果**: モバイル回線の不安定さそのものは変えられないが、「再接続したら真っ新な
シェルで、直前まで何が表示されていたか分からない」という体験を無くせる。tmux自体の
汎用hookを使っているため、Claude Code専用の仕組み(claude-hookd)が無くても、長時間
コマンド全般の完了をおおまかに拾える(§11.2.1で述べるAndroid側のAI系kind未対応を、
tmux純正のactivity/silence検知で部分的に代替する形にもなっている)。

#### 11.1.3 claude-hookdの2つの通知チャネルの併用(持続表示+一過性ポップアップ)

**組み合わせる機能**: `SetTabColor`(persistent、debounce付き)× `ctl notify`の
OSC 9ポップアップ(momentary)。どちらも同じctl-socket・同じdaemonから、集約値が
idle→attentionに変わった同じ瞬間に送出される(`ISEKAI_PIPE_DESIGN.md` Epic Q「4.
daemon本体の状態機械」参照)。

**具体例**: Windows Terminalを最小化して他の作業をしていても、一瞬だけOSC 9の
トースト通知でどのタブが入力待ちになったかに気付ける。その後Windows Terminalへ
戻ってタブ一覧を見た時には、ポップアップは既に消えていてもタブの色は
attentionのまま残り続ける(タイムアウトまたはResolveされるまで)ので、
「見逃した通知を後から一覧できる」という一過性通知の弱点を、持続的な色表示が補う。

**効果**: 新しいSSH/QUICチャネルや追加の常駐監視プロセスを一切増やさず、既存の
ctl-socket 1本を再利用するだけで「即時性のある通知」と「後から見ても分かる状態表示」
の両方を実現している——Epic Qがゼロから設計せず既存のOSC 9/SetTabColorを転用した
ことの実利。

#### 11.1.4 Android本体アプリでのAI系通知UI実装(Epic AI-8、2026-07-25実装)

**組み合わせる機能**: tmux hook系kind向けに既に実装済みだった`TabAlertNotifier`
(タスク#57)× AI系kindの`notify_generation`検知(§6.1)。§9 Epic AI-8で判明した
「Android本体アプリはAI系kindをログ出力するだけ」というギャップを、既存の
`TabAlertNotifier`をAI系kindからも呼ぶよう`TerminalTabsViewModel.kt`の`onNotify`を
配線するだけで解消した(新しいUIコンポーネントは追加していない)。あわせて
`TabAlertNotifier.notify()`に`message`引数を追加し、`titleAndTextFor`の固定文言
ではなく送出側(`isekai-pipe ctl notify`/claude-hookd)が実際に送ったtitle/bodyを
通知に反映できるようにした。

**具体例**: 11.1.1のダッシュボード体験(Windows Terminal + claude-hookd)と同じ
`Waiting`通知が、Android実機でもバックグラウンドシステム通知として届くようになった。
`isekai-pipe ctl notify --kind waiting <title> <body>`を手動で叩いた場合も、
指定した実際のtitle/bodyがそのまま通知に出る(以前の固定文言では送出側の意図が
失われていた)。

**効果**: 「並列Claude Codeダッシュボード」の恩恵をデスクトップ(Windows Terminal)
だけでなくAndroid実機でも得られるようになった。**残っている既知の差異**: (a) Rust側
`notify_from_ctl`にtmux hook系kindのようなフォアグラウンド/タブフォーカス抑制が
無いため、該当タブを表示中でも通知が出うる(§9 Epic AI-8参照、今回は見送り)。
(b)「状態ドット」・通知タップでのタブジャンプは、tmux hook系kindも含めどのkindに
ついても未実装のまま(新規UIコンポーネントが要るため低コストではなく、今回のスコープ外)。

### 11.2 応用アイデア(未実装、既存基盤の上に低コストで乗せられる拡張)

#### 11.2.1 iOS側`onNotify`実装によるクロスプラットフォームパリティ(Epic AI-9)

`TerminalSessionController.swift`の空実装を埋めれば、iOS版でも同様の通知体験が
得られる。**ただし2026-07-25の調査で判明した通り、Android版ほど低コストではない**
(§9 Epic AI-9参照): iOS側`ConnectionProfile`相当のモデルに
`enableTabNotifications`相当の永続化フィールドが無く、`UNUserNotificationCenter`の
利用実績もiOS側に一切無いため、プロファイルモデル拡張・設定画面・権限リクエスト
フロー・通知投稿ロジックの4点を新規に用意する必要がある。着手する場合はまず
プロファイルモデルへのフィールド追加から始めることになる。

#### 11.2.2 tmux session group越しの複数デバイス同時監視(未検証)

`PLAN.md`「タスク#57〜#63」のtmux session group機構は、同じprofileの複数デバイス
(例: 自宅のデスクトップ+外出中のAndroid端末)が同じウィンドウ/ペイン集合を共有しつつ
それぞれ独立した「現在のウィンドウ」を持てる設計になっている。理論上は、デスクトップで
`isekai-ssh`+Windows Terminal+claude-hookdによる並列AIダッシュボード(§11.1.1)を
動かしながら、外出先のAndroidアプリで同じホストの別ウィンドウの進行状況を
確認する、という運用が考えられる。**ただし**、`TmuxLocatorRegistry`はプロセス単位
(デバイスごと)であり、`claude-hookd`もデバイスごとの`$ISEKAI_CTL_SOCK`に紐づく
別々のdaemonインスタンスとして動くため、両デバイスの通知/色表示が実際に整合するか
(片方のdaemonが別デバイス由来のResolveを認識できるか等)は未検証。実装・検証してから
初めて「動く組み合わせ」として§11.1へ格上げすべき項目。

#### 11.2.3 長時間ジョブ完了をAI提案(§6.4)のトリガーにする

§6.4(コマンド実行後の複数選択肢AI提案、未実装)とtmux hook通知(タスク#57の
`JobDone`)を組み合わせるアイデア。現状`JobDone`はtmuxの`pane-died`相当のイベントを
Androidの通知にするだけだが、将来§6.4が実装されれば、同じイベントをトリガーに
「次に何をすべきか」のAI提案をその場で自動生成し、通知をタップした瞬間に候補が
表示されている状態にする、という連携ができる。§6.4自体は明示的トリガー(ボタン押下)を
既定にする方針(コスト/レイテンシ上の理由)なので、これは「通知タップ=明示的トリガー」
とみなせる範囲の話であり、コマンド完了ごとの自動AI呼び出しにはならない。
