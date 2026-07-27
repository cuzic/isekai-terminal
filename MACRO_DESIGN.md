# セッションマクロ機能 設計書（草案、2026-07-24）

> ステータス: 設計段階、未着手。`PLAN.md`へのPhase番号割り当ては実装着手時に行う
> （2026-07-24時点でPhase 11/12はKeystoreKekベースvault移行に予約済みのため、
> ここでは番号を占有しない）。

## 1. 概要

isekai-terminalに「ログインして同じキー入力を繰り返す」操作を自動化するマクロ機能を
追加する。差別化ポイントとして、CUI（通常シェル）だけでなくTUIアプリ・tmux中でも
動作すること、スマホの片手操作で作成・編集できることを狙う。

## 2. 検討の経緯・却下した案

- **rhai（Rust製組み込みスクリプト言語）をマクロ言語として採用する案**: 却下。
  スマホ画面でスクリプトを手打ちするコストが高く、想定用途（ログイン＋固定コマンド＋
  単純分岐＋繰り返し）は汎用スクリプト言語がなくても構造化されたステップ列で表現できる。
  rhaiは上級者向けのテキストエクスポート/インポート形式としてのみ残す余地がある
  （実装は本設計のスコープ外）。
- **ノードグラフ型ビジュアルエディタ（node-red/Blockly Canvas風）案**: 却下。
  片手縦持ちのスマホ画面ではピンチズーム・パン・ノード配置・線の引き回しの操作次元が
  多すぎ、node-redがデスクトップ前提であるのと同じ理由で不向き。想定用途は本質的に
  「逐次実行の1本道＋浅いネスト」であり、縦積みカード型（Tasker/Automate/iOS Shortcuts
  風）で十分表現できる。深いネスト・複雑な分岐合流が必要になった場合はビジュアルUIの
  守備範囲外と割り切り、上記のテキストエクスポート経由に誘導する。

## 3. アーキテクチャ方針

`.claude/rules/rust-ssot.md`の原則に従い、マクロのパース・ステップ実行・パターン
マッチ・変数展開・画面静定検知は**すべてrust-core側**（新設する macro実行モジュール）
に置く。Kotlin側の役割は次の2つのみ:

1. セッション記録（実際の入出力をrust-coreへそのまま転送）・ブロックエディタUIの
   レンダリング。
2. rust-coreからのコールバック（実行進捗・マッチ結果・エラー）を受けてUIに反映する。

Kotlin側に「今どのステップを実行中か」等のミラー状態を持たせない。

`isekai-protocol/src/ctl.rs`のモジュールdocに明記されている既存方針
「no general-purpose exec RPC」（`BuildRequest`がコマンドではなくプロファイル名しか
渡せない設計になっているのと同じ理由）を維持する。tmux操作を含め、マクロから
リモート側で任意コマンドをexecさせる新しいctl-socket経路は追加しない。tmux操作は
すべてPTY越しのキー入力（§7参照）で実現する。

## 4. データモデル: ステップ木

マクロは「ステップ」の木構造として表現する。各ステップは常に自己完結した内容
（文字列 / `KeyStep`相当の並び）を**インライン保持**し、インタプリタは他エンティティ
への参照解決を一切行わない（§4.2のSnippet/KeySequence再利用は編集時のみの補助で、
選んだ内容はその場でコピーされてインライン値になる。id参照方式にすると「参照先が
削除/変更された時の挙動」「TOMLエクスポート時に自己完結しない」といった複雑さが
増えるため、単純さを優先する決定。ユーザー承認済み、2026-07-24）。

実行系は2層に分かれる:

- **プリミティブ**: マクロインタプリタが直接解釈する実行単位。
- **プリセット**: ブロックエディタのパレット上で見せる専用ブロック。実体は
  プリミティブの組み合わせ（多くは`send`/`send-key`への糖衣）だが、一部
  （手元タブタイトル設定）は新規プリミティブへの薄いラップになる。

### 4.1 プリミティブ一覧

| 種別 | 概要 |
|---|---|
| `send` | 固定文字列の送信。編集画面に既存`Snippet`からの挿入ボタンを持つ（§4.2） |
| `send-key` | 特殊キー・複数キーの逐次送信（chord）。編集画面に既存`KeySequence`
  からの挿入ボタンを持つ（§4.2）。tmuxのprefixキー（既定`Ctrl-b`、ユーザー設定に
  より異なるため固定値にしない）を使う操作のためにchord対応が必須 |
| `wait-for-pattern` | 画面出力の正規表現マッチ待ち。詳細は§5 |
| `branch` | 出力パターンによる単純分岐（例: sudoパスワードプロンプト検出）。
  多くのケースは`wait-for-pattern`のtimeout分岐に畳み込める |
| `repeat` | N回、またはパターンに一致するまでの繰り返し |
| `variable` | `%{host}`等の固定プレースホルダ、および実行時にユーザーへ
  入力させる値（OTP等の秘密情報はマクロ本体に保存しない）。`send-key`ステップへの
  値差し込みは独自構文を新設せず、既存の`KeyStep.PlaceholderRef`表現を再利用する
  （§4.2） |
| `log` | 実行に影響しない注釈。記録から起こしたステップ列は後で読み返すと
  意図が失われやすいため |
| `abort` | 明示的な安全停止。`wait-for-pattern`のタイムアウト分岐先として使う |
| `set-local-title` | **新規プリミティブ**。PTYを経由せず、Epic Mの制御プレーンが
  使っている内部API（`session_state.rs::set_title_from_ctl`相当）をマクロ
  インタプリタから直接呼び出す。現在アクティブなタブに固定（他タブを対象にする
  ことは想定しない、ユーザー確認済み） |

### 4.2 既存Snippet/KeySequenceとの統合（編集時のみ）

isekai-terminalには既にアクセサリバー経由で使える2つの登録済み文字列送信機能がある:
「定型コマンド」= `Snippet`（`android/.../data/Snippet.kt`、単純文字列＋改行付加
オプション）と「打鍵列」= `KeySequence`（`data/KeySequence.kt`、`KeyStep`
（`CtrlChar`/`Text`/`Special`/`PlaceholderRef`）の並び、`input/KeyStep.kt`）。
マクロのステップ編集画面はこれらを再利用し、ゼロから打鍵させない（ユーザー要望、
2026-07-24）:

- **`send`ステップ編集画面**: テキスト欄への直接入力（短い使い捨て文字列向け）に
  加え、「📋 ライブラリから挿入」ボタンで既存の`SnippetPickerSheet`を開き、選んだ
  `Snippet.command`をその場で**コピー**してテキスト欄に入れる。「＋新しい定型文を
  作る」で`SnippetEditScreen`へ遷移し、保存すると呼び出し元のステップ編集画面に
  戻って新規作成した内容が自動挿入される（Compose Navigationの「作成して戻る」
  パターン）。
- **`send-key`ステップ編集画面**: 同様に既存`KeySequencePickerSheet`（個別登録分＋
  インストール済み`KeySequencePack`分の両方を含む）から選択してコピー、または
  `KeySequenceEditScreen`へ遷移して新規作成→自動挿入。

**tmux操作系プリセット（§4.3）はマクロ専用の再実装ではなく`KeySequencePack`として
追加する**方が適切。パック機構（`pack/KeySequencePack.kt`,
`pack/KeySequencePackResolver.kt`）が「アプリ同梱の静的テンプレート」を扱う場所と
して既にあるため、tmuxウィンドウ切り替え等はそこに実装し、マクロの`send-key`
ステップはそのパックを§4.2の挿入UIで取り込むだけにする。アクセサリバーからも
同じパックをそのまま使え、実装が二重にならない。

**`variable`との統合（調査済み、2026-07-24）**: `KeyStep.PlaceholderRef(name)`は
`KeySequencePackResolver.resolveStep`（`pack/KeySequencePackResolver.kt:29`）が
`paramValues: Map<String, KeyStep>`（`KeySequencePackInstallation.kt:24`、Room JSON
永続化）から解決する。ここで再利用されるのは**`KeyStep`丸ごとの置換**であって、
「テキストの一部に値を差し込む」文字列テンプレート機構ではない。実際、既存の唯一の
パック（`KeySequencePacks.TMUX`、`pack/KeySequencePack.kt`）は全シーケンスが
`[PlaceholderRef("prefix"), Text("固定1文字")]`のような「prefixキー＋固定文字」の
組み合わせのみで、`Text`の中身に動的な値を埋め込む用途は一度も無い。マクロの
`variable`が`send-key`ステップに値を渡す場合はこの`PlaceholderRef`表現をそのまま
再利用できるが、これは「chord中に値付きのキーを挟む」用途向けであり、後述の
シェルコマンド文字列構築の問題を解決するものではない（次項）。`Snippet.command`は
単純`String`でプレースホルダー機構を持たないため、値差し込みが必要な内容は
`Snippet`ではなく`KeySequence`側で表現する方針は変わらない。

**§9 Epic MACRO-6の調査結果**: 既存のKeySequencePack機構に、シェルクォート/
エスケープ相当の処理は**存在しない**。理由は上記の通り、既存パックがそもそも
「文字列へ値を埋め込む」ケースを一度も扱っていないため。よって§4.3のシェル注入
懸念は「既存機構のギャップ」ではなく、tmuxタイトル設定・ウィンドウ名選択の
2プリセットで**新規に設計が必要**な問題だと判明した。`Text("prefix'")`+
`PlaceholderRef(value)`+`Text("'")`のようにKeyStep置換スタイルで組んでも、送信時に
バイト列として連結される内容は結局同じなので、レイヤーを変えてもエスケープ処理
そのものは省略できない。

### 4.3 プリセット一覧（パレット上の専用ブロック）

| プリセット | 実体 |
|---|---|
| tmux: タイトル設定 | **（2026-07-24実機検証済み、推奨方式）** `KeySequencePack`
  として追加するchordの並び: prefix（既定`Ctrl-b`）→`,`（既定binding、tmux自身の
  `command-prompt`によるリネームプロンプトを開く）→`Ctrl-U`（プリフィルされた
  現在のウィンドウ名をクリア）→値（`Text`+`PlaceholderRef`）→Enter。シェルを
  一切経由しないため、シェルのプロンプトが空いているかどうかに関係なく確実に効き、
  シェルクォートのエスケープも不要（詳細は本節末尾） |
| tmux: 次/前のウィンドウへ | `KeySequencePack`として追加するprefixキーのchord
  （既定`Ctrl-b n`/`Ctrl-b p`）。tmuxクライアント自身がpane内の実行状態に関係なく
  横取りするため、シェルがbusyでも確実に効く |
| tmux: 番号/名前でウィンドウ選択 | **（2026-07-24実機検証済み、推奨方式）**
  `KeySequencePack`として追加するchordの並び: prefix→`'`（既定binding、
  `command-prompt -p index "select-window -t ':%%'"`）→値（番号でも名前でも可、
  `Text`+`PlaceholderRef`）→Enter。プリフィルが無いため`Ctrl-U`は不要。番号・名前の
  両方をこの1バインドでカバーできることを確認済み（詳細は本節末尾）。シェルコマンド
  文字列（`tmux select-window -t <name>`、要prompt idle）はもはや不要 |
| 手元タブ: タイトル設定 | `set-local-title`をそのままラップ（PTYを経由しない
  新規プリミティブのため、KeySequenceでは表現できない） |

いずれのtmuxパックも、prefixキーはパック側の設定値として持ち、`Ctrl-b`固定に
しない（ユーザーが`set -g prefix`で再設定している場合があるため）。

**Epic MACRO-7実機検証結果（2026-07-24、このサンドボックス上のtmux 3.3aで確認）**:
`sleep 300`を前景実行中（＝シェルプロンプトが埋まっている状態）のペインに対し、
実際にptyでアタッチしたクライアントから`Ctrl-B`→`,`→`Ctrl-U`→
`tmux-macro-test '%weird%' name`（シングルクォート＋tmux自身のテンプレート
メタ文字`%`を含む）→Enterを送信したところ、`#W`（ウィンドウ名）は入力した文字列
そのままになり（`pane_current_command`は終始`sleep`のまま、ペインには一切触れて
いないことも確認済み）、破損も注入も一切起きなかった。よって
**tmuxの`command-prompt`機構を経由するprefix chordは、シェルがbusyでも確実に効き、
かつシェルクォートのエスケープが一切不要**という結論になった（§4.2で述べた
「エスケープはマクロ側で新規設計が必要」というEpic MACRO-6の結論は、
「シェルコマンド文字列を直接送る」実装方式を選んだ場合にのみ当てはまる。
tmuxのタイトル設定・ウィンドウ選択プリセットは、上記の通り
`command-prompt`経由を第一候補にすることで、この問題自体を回避できる）。

**`prefix '`（ウィンドウ選択）の実機検証結果（2026-07-24、追加検証）**: 同じ手法で
`select-window`の既定binding（`command-prompt -p index "select-window -t ':%%'"`）
も検証した。window 0（`sleep 300`実行中でbusy）をアクティブにした状態で
`Ctrl-B`→`'`→`1`（番号指定）→Enterを送信したところ、アクティブウィンドウが正しく
window 1へ切り替わり、window 0のペインは`pane_current_command`が終始`sleep`のまま
（一切邪魔されない）ことを確認した。続けて番号の代わりに`target-win`（window 1に
付けた名前）を打った場合も同様に正しく切り替わり、**番号指定・名前指定のどちらも
同じ1バインドでカバーできる**ことを確認した。これにより「tmux: 番号/名前で
ウィンドウ選択」プリセットのシェルコマンド文字列フォールバックは不要と判断し、
上表から削除した。

シェルコマンド文字列へ値を埋め込む実装方式（`tmux rename-window '<value>'`等を
`send`する、上表の「最終手段」）を選ぶ場合のみ、値のエスケープが必須になる
（Codexレビュー2026-07-24で指摘）: シングルクォートやシェルメタ文字
（`;`, `` ` ``, `$()`等）を含む値をそのまま埋め込むと、コマンドが壊れるだけでなく
手元のシェルへ任意コマンドを注入するのと等価になる。実装時は (a) 値を単一引用符で
包み、値中のシングルクォートを`'\''`に置換してからシェルコマンド文字列を組み立てる、
または (b) シェルコマンド文字列化自体を避け、上記のchord方式に倒す、のいずれかを
採用する。(a)を新規実装する場合も、`KeySequencePack`機構を拡張する形にすれば
マクロ以外（アクセサリバーからの直接実行）にも恩恵がある。

## 5. `wait-for-pattern`の実装方針: 画面グリッドスナップショット

CUIだけでなくTUI（vim/htop等）・tmuxにも対応するため、rawバイトストリームに対する
正規表現マッチではなく、**描画済みの画面グリッドをテキスト化したスナップショットに
対してマッチする**方式にする。

### 5.1 既存の土台

`rust-core/src/terminal.rs`の`Terminal`は、Compose UIへの描画のためにVT100/VTE
パーサーの結果として既にフルスクリーンのセルグリッドを保持している
（`screen_cells() -> &[TermCell]`、`cols()`/`rows()`、`terminal.rs:1069,860-861`）。
これを行ごとに`TermCell.ch`で連結するだけでスナップショットが取れる。

alternate screen bufferの切り替えも既に検知・管理されている（`switch_to_alt`/
`switch_to_main`、`terminal.rs:1352,1392`）。tmux自体もrust-core視点では
「VT100シーケンスを描画するプログラムの1つ」に過ぎず、tmuxのステータスバー・
ペイン境界を含めて「今画面に見えている通り」のグリッドをそのままマッチ対象にすれば、
tmux固有の特別扱いは不要。

既存の`last_command_output_text()`（`terminal.rs:2121`、OSC 133セマンティック
プロンプト連携）はCUI専用でtmux/TUI中は発火しないため、代替にはならない。

### 5.2 新規実装が必要な要素

1. **`screen_text_snapshot()`（仮称）の新設**: `screen_cells()`を行ごとに連結して
   テキスト化する。行の自動折り返し（autowrap）を論理的に連結するかどうかは、
   行の折り返しメタデータの有無を要調査（現状`TermCell`自体にはその情報がない）。
   ただし単純に`TermCell.ch`を連結するだけでは不十分（Codexレビュー2026-07-24で
   指摘）: 全角文字の2セル目は`is_wide_placeholder`が立ったプレースホルダセルで、
   その`ch`は実装上半角スペース`" "`で埋められている（`terminal.rs:2301`）ため、
   単純連結すると「漢字」が「漢 字 」のように壊れ、`wait-for-pattern`の正規表現
   マッチが日本語を含む画面出力に対して意図通り動作しない。
   `screen_text_snapshot()`は`is_wide_placeholder`セルを連結対象から除外し、
   本体セル側の文字をそのまま1文字として扱う必要がある。`invisible`（SGR 8）
   フラグが立ったセルをマッチ対象に含めるかどうかも合わせて要検討。
2. **画面静定検知（デバウンス）**: TUIアプリは1回の更新で大量のエスケープ
   シーケンスをバースト送信して画面を作り直すため、bytes到着のたびに素朴に
   スナップショットを取ると描画途中の中間フレームにマッチしてしまう。
   「Nms新規データが来なければ確定」のような静定検知を、`Terminal`本体
   （同期的なVTEフィーダ）ではなくセッション/マクロオーケストレータ層に新設する。
   閾値は固定msか適応的かは未決定（§9参照）。実装には新しいタイマー機構を
   持ち込まず、`trzsz.rs`/`session_state.rs`/`rebind_manager.rs`等で既に
   使われている`timed_fsm`（タイムアウト付き状態機械）クレートに乗せるのが
   一貫性がある（§10参照）。

この方式にすることで、tmuxウィンドウ切り替え（§7）の後もマクロエンジン側に
特別なロジックを追加せず、`wait-for-pattern`/`branch`は「今画面に見えているもの」に
自然に追従する。

## 6. 作成方法（記録優先 + 既存ライブラリの再利用）

スマホでの手打ちコストを避けるため、作成は次を組み合わせる:

1. **セッションからの記録**: 「記録開始」→実際にログイン・コマンド入力・tmux操作を
   行う→「記録終了」でステップ列を生成する。
2. **既存Snippet/KeySequenceの挿入**（§4.2）: 記録に頼らずブロックエディタで直接
   ステップを組み立てる場合も、`send`/`send-key`ステップの編集画面からライブラリを
   挿入・新規作成できるため、都度の手打ちを避けられる。短い使い捨て文字列は
   テキスト欄へ直接入力すればよく、ライブラリ往復は必須にしない。
3. 記録後・作成後はブロックエディタでタップにより編集（並べ替え・削除・パラメータ
   変更）する。
4. 外付けキーボード接続時など上級者向けに、テキストでの直接編集（§2で却下した
   rhaiのエクスポート/インポート）を隠しオプションとして残す余地がある
   （本設計のスコープ外、将来検討）。

## 7. ブロックエディタUI設計

- **レイアウト**: 縦積みカード型（Tasker/Automate/iOS Shortcuts風）。ノードグラフ型は
  §2の理由で不採用。
- **ネスト表現**: `repeat`/`branch`は子ステップを内包する折りたたみ可能な
  コンテナカードとし、ヘッダタップで開閉。子は一段インデント＋左に縦ラインで
  スコープを表示する。
- **編集操作**: 主操作はタップ（誤操作が多いドラッグ&ドロップに依存しない）。
  各ステップ長押しで「上へ/下へ/このグループの外に出す/削除」のコンテキスト
  メニューを出す。ステップ追加はグループ内の「＋」タップ→ブロックパレットの
  ボトムシート。
- **変数トークン**: `variable`のプレースホルダはiOS Shortcuts風のインライン
  トークン表示にする。
- 参考UI: Tasker / Automate（Android）、iOS Shortcuts、Blockly（vertical block
  layoutの括り方のみ）、Scratch（ネストの折りたたみ表現）。

## 8. 安全性

- 破壊的コマンド（`rm`/`reboot`等）を含むマクロは、実行前にステップ一覧の
  プレビュー表示＋確認を必須にできるオプションを持たせる。
- 実行中は現在のステップ・マッチ待ち状態・失敗をリアルタイム表示し、
  途中で一時停止/中断できるようにする。
- 秘密情報（パスワード/OTP等）は`variable`の実行時入力モードでのみ扱い、
  マクロ本体（保存されるステップ木）には含めない。

## 9. 既知のギャップ・今後の課題

- **Epic MACRO-1: 折り返し行の連結ルール**: `wait-for-pattern`が長い1行の
  自動折り返しをまたいでマッチできる必要があるか、必要ならどう行メタデータを
  持たせるか。未調査。
- **Epic MACRO-2: 画面静定検知の閾値設計**: 固定ms vs 適応的（例: 直近の
  データ到着間隔から動的に決める）。未決定。
- **Epic MACRO-3: tmux prefixキーの設定粒度**: マクロ単位で持つか、ホスト
  プロファイル単位で持つか。未決定。
- **Epic MACRO-4: エクスポート/インポート形式**: TOML化を想定しているが、
  具体スキーマは未設計。既存の`known_ssh_hosts.toml`等の設定ファイル文化との
  整合を取る。
- **Epic MACRO-5: 上級者向けテキスト編集（rhaiエクスポート）**: §2/§6で
  「逃げ道」として言及したのみで、実装方針は本設計のスコープ外。
- **Epic MACRO-6: `PlaceholderRef`のエスケープ有無の調査 — 完了（2026-07-24）**:
  既存のKeySequencePack機構にシェルクォート/エスケープ処理は存在しない（既存の
  唯一のパックがそもそも「文字列へ値を埋め込む」ケースを扱っていないため）。ただし
  Epic MACRO-7の結果により、tmuxタイトル設定・ウィンドウ選択プリセットは
  `command-prompt`経由（chord方式）を第一候補にすることでこの問題自体を回避できる
  ことが判明したため、シェルコマンド文字列を直接組み立てる実装方式を選んだ場合の
  みエスケープ新規設計が必要、という条件付きの結論に縮小された（§4.3参照）。
- **Epic MACRO-7: tmux `command-prompt`経由のリネームの実機検証 — 完了
  （2026-07-24、このサンドボックス上のtmux 3.3aで検証）**: `sleep 300`実行中
  （シェルbusy）のペインに、ptyでアタッチした実クライアントから
  `Ctrl-B`→`,`→`Ctrl-U`→シングルクォート＋tmux自身のテンプレートメタ文字`%`を
  含む文字列→Enterを送信し、ウィンドウ名（`#W`）が入力文字列そのままになる
  （破損・注入なし）ことを確認した。`pane_current_command`が終始`sleep`のままで
  あることも確認し、ペインには一切触れていないことも検証済み。結論:
  **`command-prompt`経由のchordは、シェルがbusyでも確実に効き、かつシェルクォートの
  エスケープが一切不要**。tmuxタイトル設定プリセットはこの方式を第一候補として
  §4.3に反映済み。続けて`prefix '`（既定`command-prompt -p index
  "select-window -t ':%%'"`）も同様の手法で検証し、番号指定・名前指定の両方が
  busyなペインの影響を受けず正しく動作することを確認した（結果は§4.3末尾）。
  これによりウィンドウ選択プリセットのシェルコマンド文字列フォールバックは不要と
  判断し撤去した。`prefix f`（`find-window`）は未検証のまま（対象外、find-window
  自体が本設計のプリセット一覧に無いため優先度低）。

## 10. 想定crate

ワークスペースの既存`Cargo.toml`群を実際に調査した結果（2026-07-24）。

**既にワークスペース内に実績があり、そのまま流用できるもの**:

| 用途 | crate | 状況 |
|---|---|---|
| 全角文字幅判定 | `unicode-width`（`UnicodeWidthChar`） | `terminal.rs`で既に使用中。ただし§5.2のスナップショット生成では幅を
  再計算せず、既に計算済みの`TermCell.is_wide_placeholder`をそのまま使う方が
  二重計算にならず筋が良い |
| ステップ実行のタイムアウト/状態遷移 | `timed_fsm`（`TimedStateMachine`/
  `TimerCommand`/`Response`） | `trzsz.rs`・`session_state.rs`・
  `rebind_manager.rs`・`session.rs`で既に使われている「タイムアウト付き状態機械」の
  共通パターン。`wait-for-pattern`のタイムアウトも§5.2の画面静定検知（デバウンス）も
  同じパターンに乗せるのが一貫性がある |
| ステップ木のシリアライズ | `serde`/`serde_json` | 既に依存済み |
| TOMLエクスポート/インポート（Epic MACRO-4） | `toml = "0.8"` | `isekai-terminal-core`
  自体にはまだ無いが、`isekai-ssh`/`isekai-trust`で既に同バージョンを使用中。
  揃えて追加するだけで済む |

**ワークスペース全体で未使用、新規追加が必要なもの**（`grep -rn` で確認済み、
現状1件もヒットしない）:

| 用途 | 候補crate | 備考 |
|---|---|---|
| `wait-for-pattern`の正規表現マッチ | `regex`（標準）、または`regex-lite`
  （軽量版） | pure Rust実装でAndroid NDK/musl静的ビルドとの相性は良いはず。
  バイナリサイズを気にするなら`regex-lite`も検討候補 |
| tmuxシェルコマンド文字列のクォート（Epic MACRO-6） | `shlex`（クォート関数あり、
  将来トークン分割が要る場面にも使える）または`shell-escape` | 自前で
  `'`→`'\''`置換を書くより実績のあるcrateに任せる方が安全 |
| 上級者向けrhaiエクスポート（§2/§6/Epic MACRO-5、スコープ外） | `rhai` | 実装
  するとしたら新規追加、現時点では未着手のまま |

## 11. 参照

- `.claude/rules/rust-ssot.md`: Rustを状態/意思決定ロジックのSSOTにする原則
- `isekai-protocol/src/ctl.rs`: Epic M制御プレーン（`SetTitle`等）の
  「no general-purpose exec RPC」方針
- `rust-core/src/session_state.rs:275`: `set_title_from_ctl`（`set-local-title`
  プリミティブが直接呼ぶ内部API相当）
- `rust-core/src/terminal.rs`: `screen_cells`/`cols`/`rows`（1069,860-861）、
  `switch_to_alt`/`switch_to_main`（1352,1392）、`last_command_output_text`（2121）
- `android/src/main/kotlin/tools/isekai/terminal/data/Snippet.kt`,
  `SnippetCommands.kt`, `SnippetListScreen.kt`/`SnippetEditScreen.kt`: 既存の
  「定型コマンド」機能（§4.2で再利用するSnippet側）
- `android/src/main/kotlin/tools/isekai/terminal/data/KeySequence.kt`,
  `input/KeyStep.kt`, `KeySequenceCommands.kt`,
  `KeySequenceListScreen.kt`/`KeySequenceEditScreen.kt`,
  `pack/KeySequencePack.kt`, `pack/KeySequencePackResolver.kt`,
  `data/KeySequencePackInstallation.kt`: 既存の「打鍵列」機能（§4.2/§4.3で再利用・
  拡張するKeySequence側、tmuxプリセットの実装先）
- `android/src/main/kotlin/tools/isekai/terminal/TerminalHostScreen.kt:191`:
  現状のタブタイトル表示ロジック（`screenUpdate.title`優先、`tab.label`に
  フォールバック）。本設計の`set-local-title`とは別レイヤーの話だが、将来
  「ローカルタブ名を優先しリモート由来は補助表示にする」方向に手を入れる際は
  ここが起点になる
- `ISEKAI_PIPE_DESIGN.md` §8 Epic M: 制御プレーンの元設計
