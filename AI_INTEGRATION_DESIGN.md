# AI統合機能 設計書（草案、2026-07-24）

> ステータス: 設計段階、未着手。`PLAN.md`へのPhase番号割り当ては実装着手時に行う
> （2026-07-24時点でPhase 11/12はKeystoreKekベースvault移行に予約済みのため、
> ここでは番号を占有しない。`MACRO_DESIGN.md`と並行して設計中の別機能であり、
> 両者は§6.3/6.4がscreen_text_snapshot()を共有する形で連携する）。

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

既存の`bell_generation`(rust-core/src/terminal.rs:425-434、タスク#24/#25で実装済み、
BEL受信を取りこぼし無く検知するカウンタ)は、現状`TerminalTabsViewModel.kt:251-254`で
150msの振動を鳴らすだけで止まっている。これを次のように拡張する:

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
- **Epic AI-7: APC新規prefixバイトの選定 — 未調査**: Kitty自身の予約・他ターミナル
  (iTerm2/kitty/wezterm)のAPC/OSC私的利用との衝突を避ける値を選ぶ必要がある。

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
  exec RPC」方針
- `MACRO_DESIGN.md` §5: `screen_text_snapshot()`構想(Epic AI-6で本設計と共有)、
  §8: 安全性方針(§7で整合を取る)
- MulmoTerminal(`github.com/receptron/mulmoterminal`)調査結果(2026-07-24):
  `gui-chat-protocol`(`{toolName, data}`エンベロープ)、`docs/gui-protocol-spike.md`
  (PTY stdinへのフィードバック書き戻し方式)
- 外部prior art(2026-07-24調査): Warp(Active AI/Next Command、HN炎上事例含む)、
  GitHub Copilot CLI、Amazon Q Developer CLI、TermAI(モバイルAI SSHクライアント、
  Run/Edit/Dismiss確認UI)
