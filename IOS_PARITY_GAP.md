# Android→iOS 機能パリティ Gap分析（2026-08-21時点）

Android版(`android/`)で実装済みの機能のうち、iOS版(`ios/`)にまだ移植されていないものを
洗い出したドキュメント。`PLAN.md`の「Phase Y: iOS対応」節（2026-07-04〜07-05に集中して
書かれたもの）は2026-07-05以降更新が止まっており、その後もiOS側は継続的にコミットされている
（tmux通知・AIパネル・OSC 12/9;4・バッテリー最適化案内UI等、Android側の機能追加に伴う
UniFFIバインディング再生成コミットが2026-08-17まで続いている）ため、`PLAN.md`のgap分析
節は**もはや現状を反映していない**。本ドキュメントは現在のソースツリーを実際に読んで
検証した結果であり、`PLAN.md`のPhase Y節を置き換える更新版として位置づける。

調査方法: `android/src/main/kotlin/tools/isekai/terminal/`と`ios/Sources/`
(`IsekaiTerminalCore`アプリ本体 + `IsekaiTerminalCoreLogic`パッケージ)を機能領域ごとに
突き合わせ、Rust側(`rust-core/src/`)のUniFFI公開関数が実際にSwift側で呼ばれているか
(バインディング再生成されただけで未配線でないか)まで確認した。

## 未実装（要実装、優先度順の目安つき）

### 1. trzsz転送ファイルのプレビューUI【高工数】
- **Android**: `android/src/main/kotlin/tools/isekai/terminal/filepreview/`配下10ファイル
  (ディレクトリブラウザ、CSV/Markdown/画像/テキストビューア、シンタックスハイライト等)。
  `TerminalSession.kt`の`filePreviewRequest`/`onFilePreviewResult`経由で配線済み。
- **iOS**: `TerminalSessionController.swift`の`onFilePreviewResult`は日本語コメント付きの
  no-opスタブ(968-983行目付近)。`ios/`にfilepreview相当のファイルは1つも存在しない。
- **対応方針**: Android側のビューア群をSwiftUIへ1画面ずつ移植。丸ごと新規UIサブシステムの
  ため工数は本リストの中で最大。

### 2. AI/リッチパネルUI（PanelKind/PanelField）【中工数】
- **Android**: `ui/AiPanelDialog.kt`で完結したシートUIとして表示。
- **iOS**: `panelKind`/`panelTitle`/`panelMarkdown`/`panelFields`のデータは
  `TerminalScrollback.swift`のupdate構造体まで正しく流れてきているが、**それを描画する
  Swift Viewが存在しない**。データ配線済み・表示層のみ欠落という状態。
- **対応方針**: `AiPanelDialog.kt`相当のSwiftUIシートを1つ追加すればよく、Rust/データ層の
  変更は不要。

### 3. カスタム端末フォント読み込み + グリフフォールバック【中工数】
- **Android**: `ui/TerminalFont.kt`(ユーザーが`.ttf`/`.otf`をインポートし永続化、
  `Typeface.createFromFile`)+ `ui/TerminalGlyphFallback.kt`(カスタムフォントに無い
  グリフ—絵文字・CJK・記号—だけセル単位でシステムフォントにフォールバック)。
- **iOS**: `TerminalFrameRenderer.swift`は`UIFont.monospacedSystemFont`固定。フォント
  インポートUI・フォールバック機構とも皆無。
- **対応方針**: フォントインポート(ファイルピッカー→Documents保存→Core Text登録)と、
  セル描画時のグリフ存在チェック→フォールバックの2段階で実装。

### 4. tmuxフック注目通知【低〜中工数】
- **Android**: `TabAlertNotifier.kt`が実際の通知を発火。
- **iOS**: `onNotify(kind:)`はno-opスタブ(985-990行目付近)。Rust側`NotifyKind`と
  抑制/重複排除のSSOT(`orchestrator.rs`)は既に存在し、iOS向けにも共通で使える。
- **対応方針**: `UNUserNotificationCenter`への橋渡しを1箇所追加するだけで済む見込み。
  Rust側の変更は不要。

### 5. OSC 133セマンティックプロンプト ナビゲーションUI【低工数】
- **Android**: `TerminalSession.kt`に前後プロンプトへのジャンプ・直前コマンド出力のみ
  コピー、を実装済み。
- **iOS**: `onPromptJump`/`onPromptOutputCopyReady`はno-opスタブ(968-976行目付近)。
  Rust側の`Terminal::prompt_jump_target`/`last_command_output_text`は既にある。
- **対応方針**: リストの中で最も工数が小さい。アクセサリバーへのボタン追加+コールバック
  配線のみ。

### 6. 外部(Bluetooth)キーボードのJIS/US配列自動判定【低〜中工数】
- **Android**: `input/KeyboardLayoutDetector.kt`+`input/KeyboardLayoutMode.kt`で
  自動判定＋手動オーバーライドを提供。
- **iOS**: 該当機能なし(grep で JIS/外部キーボード関連のヒットゼロ)。iOSでは
  `GCKeyboard`/`UIKeyInput`系のAPIで代替実装する必要があり、Android実装の単純移植では
  済まない可能性が高い。
- **対応方針**: まずiOSでBluetoothキーボードのレイアウト判定に使えるAPI(`GCKeyboard`の
  `keyboardInput`、`UIKeyCommand`のlocalizedキー等)を調査するスパイクから。

### 7. 新規ホスト鍵の自動信頼設定（HostKeySettings）【低工数】
- **Android**: 「新規ホスト鍵を自動信頼する」opt-inトグル(既定OFF)をRoom(`HostKeySettings`)
  で永続化。
- **iOS**: `TerminalSessionController.swift:804`付近で`autoTrustNewHostKeys = false`が
  ハードコードされており、ユーザーが変更する設定UIが無い。
- **対応方針**: GRDBに1テーブル追加+設定画面にトグル1つ追加。

### 8. 定型コマンドのテンプレートギャラリー（SnippetTemplates）【低工数】
- **Android**: バンドル済みスターターテンプレート(例: tmuxセッションピッカー)を
  「テンプレートから追加」できる。
- **iOS**: `SnippetCommands.swift`/`SnippetListView.swift`は手動作成のみ対応、
  テンプレートギャラリーが無い。
- **対応方針**: テンプレート定義をRust共通層かSwift側の静的配列に持たせ、一覧→追加の
  簡単なUIを足すだけ。

### 9. OEMバッテリー最適化案内UI + バックグラウンド信頼性ダイアログ【要設計】
- **Android**: 2026-08-17のpre-mortem対応(`PLAN.md`約3529行目)で追加。
  `ui/BackgroundReliabilityDialog.kt`、`ui/BatteryGuidanceCopy.kt`(Xiaomi/OPPO/Vivo等
  メーカー別の案内文言)、`data/BatteryGuidanceSettings.kt`、Rust側
  `background_reliability_policy.rs`。
- **iOS**: 生成されたUniFFIバインディング以外、参照ゼロ。
- **対応方針**: **単純移植不可**。`background_reliability_policy.rs`のkill検知ロジックは
  「AndroidのOEMキラー検知はKotlin側に置く」という設計([[rust-ssot]]ルール参照)で
  意図的にAndroid固有。iOSにはOEM製バッテリーキラーという概念自体が無く、類似リスクは
  「バックグラウンド更新/サスペンドの積極性」なので、Androidの文言・UIをそのまま持ち込むのではなく
  iOS向けに何を案内すべきかを先に設計する必要がある(例: 設定アプリの「バックグラウンド更新」
  を促す案内、Live Activities導入検討等)。

### 10. マルチタブ時のバックグラウンド維持【アーキテクチャ制約、要設計】
- **Android**: `TerminalSessionService.kt`が単一のForeground Serviceで全タブを束ね、
  「N件のセッション接続中」の永続通知を出しながら無期限にバックグラウンドで生かし続ける。
  クリーン終了かOEM強制終了かを`SharedPreferences`マーカーで判定し、
  `TerminalTabsViewModel`がコールドスタート時に参照する。
- **iOS**: `TerminalTabsHostView.swift`(`TerminalTabsModel`)はコメントで意図的な
  スコープカットを明記しており、FGS相当の仕組みが無い。開いている全タブが一律
  約30秒の`beginBackgroundTask`猶予を共有するのみで、タブごとの優先度付け・永続通知・
  OEM強制終了検知のいずれも無い。タブ数が増えるほどAndroidとの体験差が拡大する。
- **対応方針**: 上記#9(バックグラウンド信頼性UI)と一体で検討すべき項目。iOSの構造的制約
  (真の無期限バックグラウンド実行手段が無い)を前提に、「ユーザーが戻った瞬間に同じ
  シェルに戻れること」を価値の中心に据える既存方針([[ios-port-plan]]memory参照)を
  タブ複数時にも一貫させる設計が必要。

## パリティ確認済み（差分なし）

以下は個別に検証し、iOS側が実質同等の実装を持つことを確認済み:

- SSH agent forwarding（`ProfileEditView.swift`/`ProfileDatabase.swift`/
  `TerminalSessionController.swift`で配線済み）
- 新規ホスト鍵TOFU確認UI（`NewHostKeyPrompt`/`SshHostTrustStore`/`TerminalView.swift`、
  Android版`TerminalSession.kt`と対称的に実装）
- マウスジェスチャ調停（press/drag/release、pinchハンドオフ、ホイールモード。
  `TerminalScreenView.swift`はAndroid`MouseGestureArbiter.kt`の対称移植）
- クリップボード画像同期（tmux ctl-channel経由。`RemoteClipboardBridge.swift`、
  `UIPasteboard`がAndroidの`FileProvider`より単純に処理できる分むしろ実装は軽量）
- trzszアップロード/ダウンロードシート（両OSとも`TrzszTransferSheet`相当のView）
- upstream failover / マルチパス設定配線・ProxyJump・ポートフォワード
  （`ProfileDatabase.swift`/`TerminalSessionController.swift`で配線済み）
- tmuxタブ⇔ウィンドウ紐付け + 再接続時scrollback backfill（`TmuxTabWindowCoordinator.swift`、
  Android版`maybeEnsureTmuxTabWindow`/`ClientIdentity`/`TmuxTabLocator`の1:1移植。
  ロジックの大半がRust側`orchestrator.rs`/`session.rs`にあるためプラットフォーム差分が
  そもそも小さい。tmuxフック**通知**部分のみ上記4番で別途欠落）
- IME中核ロジック（`TerminalIMEInputView.swift`とAndroid`TerminalInputView.kt`+
  `TerminalInputConnection.kt`はコード規模がほぼ同等で、ローマ字変換・変換中Backspace・
  確定・キャンセル・絵文字・複数行ペーストまで機能的に対応）
- Room⇔GRDBスキーマ（KeyEntry/ConnectionProfile(+transport/jumpフィールド)/Snippet/
  KeySequence/KeySequencePackInstallation/TmuxTabLocatorはテーブル単位で対応。
  `KnownHost`はGRDBテーブルではなくJSONファイルストア`SshHostTrustStore.swift`だが
  役割は同一で差分ではない）
- お気に入り/最終接続日時等の接続履歴メタデータ（Android/iOSどちらのプロファイルモデルにも
  存在しない — 両OS未実装という意味でパリティは取れている）

## iOSでは対象外（プラットフォーム制約により意図的にスコープ外、gapとしてカウントしない）

- **物理Wi-Fi/セルラー同時マルチパス**: AndroidのConnectivityManager直叩き実装
  (`NetworkPathMonitor.kt`/`PhysicalPathProvider.kt`)に相当するiOS APIが無い
  (`NWParameters.multipathServiceType`はMPTCP向けでQUIC multipathには使えないと
  既に調査済み)。2026-07のChatGPT相談で「論理マルチパス(Tailscale/直接/relay)+QUIC
  connection migration+NWPathMonitor駆動の高速再接続」を代替として提供する方針が
  確定済み(v1スコープ外)。
- **Androidと同じ無期限バックグラウンド接続維持そのもの**: iOSには真のForeground
  Service相当が無いという構造的制約であり、「ベストエフォートのsuspend/resume」を
  目指す価値観自体は意図的な設計判断（上記#10は「その範囲内で改善できる部分」の指摘であり、
  Android同等の無期限維持を目指す指摘ではない）。
- **Kitty graphics画像プロトコル対応**: 両OSとも明示的にwon't-do(`PLAN.md`
  「tty完全実装タスクで対象外と判断した機能」節)。
- **alt-screenでのwheel→矢印キー変換 / マウスレポーティング有効時のタッチスクロール**:
  両OSとも明示的に対象外・保留のまま(`PLAN.md`同節)。iOS固有のgapではない。

## 推奨着手順序

Rust側の変更が不要、または既にRust側は用意済みでSwift側の配線・UIのみで完結する項目
(低工数)から着手するのが最も費用対効果が高い:

1. OSC 133プロンプトジャンプUI（#5）
2. tmuxフック注目通知（#4）
3. AIパネルUI（#2、データは既に来ている）
4. HostKeySettings自動信頼トグル（#7）
5. Snippetテンプレートギャラリー（#8）
6. カスタムフォント読み込み+フォールバック（#3）
7. 外部キーボードJIS/US判定（#6、要API調査スパイク）
8. trzszファイルプレビューUI（#1、最大工数）
9. バックグラウンド信頼性UI（#9）とマルチタブ背景維持の設計（#10）— 両方とも
   Androidの単純移植ではなくiOS向けの設計検討が先に必要
