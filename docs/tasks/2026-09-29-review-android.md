# android/src/main コードレビュー指摘の対応タスク(2026-09-29)

レビュー全文(読み取り専用レビュー、古いコミット基準)の全指摘を1件1項目で起票する。
各項目は origin/main(`0ac06820`)のコードで今も有効かを確認済み。

状態: `[ ]`未着手 / `[x]`修正済み / `[-]`対応不要(理由) / `[~]`見送り(理由)

担当ファイル境界: `android/**` のみ(生成物 `uniffi/isekai_terminal_core.kt` は触らない)。
Rust側に新しいUniFFI APIが必要な部分は `[~]` として「Rust側新API要」と明記する
(rust-coreは別エージェントの担当)。

パスは `android/src/main/kotlin/tools/isekai/terminal/` からの相対。

## High

- [x] **AND-H1** (High) `session/TerminalSession.kt:688-692` — `close()`が`orchestrator.disconnect()`のみで
  UniFFIオブジェクトを`destroy()`/`close()`しない。callback↔Rust Arcの循環でCleanerが永遠に発火せず、
  閉じたペインごとにRust側セッションとKotlin側`TerminalSession`がプロセス寿命いっぱいリークする。
  pending中のagent署名要求もRustスレッドを25秒ブロックし続ける。
  - 方針: `close()`で`(orchestrator as? AutoCloseable)?.close()`を呼ぶ。`pendingAgentSignRequest`を
    `complete(false)`する。close後の呼び出しは生成バインディングが`IllegalStateException`
    ("already destroyed")を投げるため、公開メソッドをclose済みガードで包む。
- [x] **AND-H2a** (High) `TerminalTabsViewModel.kt:825-869` — `connected`の立ち下がり(=Rustの
  `Reconnecting`も含む)で物理マルチパスのNetworkRequest・upstream監視を畳み、
  `upstreamFailoverEnabledForCurrentSession=false`にするため、Rust自動再接続後にupstream
  フェイルオーバー監視/物理マルチパスが二度と復活しない。
  - 方針(Kotlin側でできる範囲): リソース解放は「セッションが生きている(`connected || isReconnecting`)」
    の立ち下がりに限定し、Reconnecting中は保持する。フェイルオーバー可否フラグは接続試行のたびに
    profileから導出し、切断で落とさない。監視は未登録時のみ登録。
- [~] **AND-H2b** (High) 同上のrust-ssot逸脱の根本対応 — 見送り(Rust側新API要: rust-coreは別担当。
  Kotlin側はAND-H2aで実害だけ止め、判断箇所に「Rust側イベント待ち」のコメントを残した) — リソース保持/解放のタイミングをKotlinが
  Rust状態ミラーのエッジから推論している。
  - 方針: Rust側から「論理セッション終了」「transport再確立」を区別したイベント
    (例: `onSessionEnded`/`onTransportReestablished`)を出す必要がある。
- [x] **AND-H3** (High) `ConnectionCoordinator.kt:69-129` / `session/TerminalSession.kt:445-449` /
  `ProfileEditScreen.kt:200` / `RelayCredentialVault.kt` — 接続コルーチンに例外処理が無く、
  relay JWT復号失敗(Keystoreエントリ欠落・平文レガシー値)やUniFFIの`InternalException`で
  アプリ全体がクラッシュする。例外時は復号済みPEMのwipeもスキップされる。編集画面は
  コンポジション中の復号失敗で即クラッシュ。
  - 方針: connectPaneのコルーチン本体を`try/catch(Exception)`で包み`preConnectError`へ反映、
    `finally`でwipe。`guardedConnect`は`SshException`以外も捕捉。`RelayCredentialVault.decrypt`は
    平文レガシー値(`.`を含むJWT、Base64 NO_WRAPの字種外)をそのまま返し、真の復号失敗は
    意味のある例外にする。編集画面は復号失敗を空欄に落とす。
- [ ] **AND-H4** (High) `TerminalTabsViewModel.kt:813-815` — `observeSummary`が`state`全体をcollect
  しており、端末の描画フレームごとにFGS通知を再postしている(Binder IPC + 通知レート制限)。
  - 方針: `state.map { it.connected }.distinctUntilChanged()`をcollectする。Service側も同一ラベルの
    再postを抑止する。

## Medium

- [ ] **AND-M1a** (Medium) `TerminalTabsViewModel.kt:1111-1138` — trzszアップロードにフロー制御・
  キャンセル確認が無く、Rust側の容量64の`try_send`を溢れさせてチャンクを取りこぼしうる。
  - 方針(Kotlin側): Rustが報告するack済みバイト数(`TrzszUiState.InProgress.transferred`、
    upload時はリモートの`SUCC`で進む)に対する送信先行量をウィンドウで制限し、転送が
    Done/消滅したら読み出しを止める。
- [ ] **AND-M1b** (Medium) 同上のRust側 — `session.rs`の`try_send`失敗黙殺、bounded+awaitのAPIや
  「次チャンク要求」コールバックによるRust主導のフロー制御。
- [ ] **AND-M2** (Medium) `TerminalSessionService.kt:41-45` / `session/AndroidAppExecutor.kt:57-66` —
  最後のタブを閉じても`stopSelf()`だけで`stopForeground`されず、`BIND_AUTO_CREATE`でbindされた
  ままのためFGS/常駐通知が残り続ける。
  - 方針: `totalCount<=0`で`stopForeground(STOP_FOREGROUND_REMOVE)`→`stopSelf()`。executor側も
    unbindしてサービスが破棄されうるようにする。
- [ ] **AND-M3a** (Medium) `session/NetworkPathMonitor.kt:76-83` — PathId単位で状態を持つため、
  Wi-Fiとセルラーが両方いる状態でWi-Fiだけ失うとDIRECTがFAILEDに張り付き、誤った「経路なし」を
  Rustへ送る。
  - 方針: PathIdごとに`Set<Network>`で追跡し、空になった時だけFAILEDにする。
- [ ] **AND-M3b** (Medium) `session/AndroidAppExecutor.kt:80-91` — 「どちらか一方でも生きていれば
  onLostを鳴らさない」集約判断がKotlin側にある(rust-ssot)。
  - 方針: 生のavailable/lost(transport種別付き)をRustへ渡す新UniFFI APIを追加し集約はRust側で行う。
- [ ] **AND-M4** (Medium) `session/PhysicalPathProvider.kt:156-172` — `bindAndDetach`が元の
  `DatagramSocket`を閉じない(`fromDatagramSocket`はdupを返す)。失敗経路でも閉じずfdリーク。
  - 方針: `DatagramSocket(null).use { ... }`でdup取得後に必ず元socketを閉じる。
- [ ] **AND-M5** (Medium) `TerminalTabsViewModel.kt:1059-1065` — tmux連携の予約がprofileId単位の
  Setのため、同一タブの(手動/Rust自動)再接続でensure/フック再インストールがスキップされる。
  - 方針: 予約を「profileId→所有tabId」のマップにし、所有タブ自身の再接続は通す。所有タブを
    閉じたら解放する。
- [ ] **AND-M6** (Medium) `TerminalTabsViewModel.kt:817-823` / `session/AndroidAppExecutor.kt:185-209` —
  ダウンロード保存の例外が未捕捉でプロセスが落ち、`IS_PENDING=1`の行がMediaStoreに残る。
  - 方針: collect側でtry/catchしてログに落とし、executor側は失敗時に挿入した行を`delete`する。
- [ ] **AND-M7** (Medium) `input/TerminalInputConnection.kt` / `input/TerminalInputView.kt:97-100` —
  (a) `BaseInputConnection(fullEditor=true)`のEditableが送信済みテキストで無限に肥大、
  (b) Ctrlトグル経路でcomposing spanが残りショートカット無効化・古い文字の再送、
  (c) `deleteSurroundingText`の`beforeLength`が無制限、(d) onKeyDown由来の未処理キーが
  `super.sendKeyEvent`で同じViewへ再注入されるループ。
  - 方針: commit/finish後に`editable.clear()`、Ctrl経路でcomposing spanを除去、`beforeLength`を
    coerce、onKeyDownからは`super.sendKeyEvent`に回さない専用入口を使う。
- [ ] **AND-M8a** (Medium) `session/TerminalSession.kt:255,403,479-482` — (a) 画面更新を
  `connected`ミラーでゲートしており、Connected直後の初回フレームが状態callbackより先着すると
  捨てられる、(b) `disconnect()`の楽観的ミラー書き換えが`isReconnecting`を落とさず不整合表示になる。
  - 方針(Kotlin側): ゲートを「接続中または接続済み」に緩めて初回フレームの取りこぼしを防ぐ。
    `disconnect()`はRustへ`cancelReconnect()`(ループが動いていなければRust側で無音)も転送し、
    ミラーの`isReconnecting`も落とす。
- [ ] **AND-M8b** (Medium) 同上の根本対応 — ScreenUpdateの有効/無効と切断状態の反映を完全に
  Rustへ委ねるには、Rust側が「`disconnect()`後は必ず`Disconnected`を通知し、以後フレームを
  送らない」ことを全フェーズ(Connecting/Reconnecting含む、`reconnect_epoch`)で保証する必要がある。

## Low

- [ ] **AND-L1** (Low) `TerminalTabsViewModel.kt:233-247` / `RemoteClipboardPolicy.kt:28-29` /
  `RemoteClipboardImagePolicy.kt` — クリップボードpull/write callbackの例外・OOMがUniFFI callback
  境界へ漏れる。4000万画素までフルデコードを許す。
  - 方針: `RemoteClipboardPolicy`で例外/OOMを捕捉してnull/no-opに落とす。画像はinSampleSizeで
    縮小デコードする。
- [ ] **AND-L2** (Low) `session/TerminalSession.kt:363-381` — agent署名要求の同時到着で、1件目の
  `finally { set(null) }`が2件目のdeferredを消す。
  - 方針: `compareAndSet(deferred, null)`にし、表示中fingerprintも自分の分だけ消す。
- [ ] **AND-L3** (Low) `KeyManager.kt:13` / `KeystoreKek.kt:74` — `KeyEntry.kekAlias`に実際とは違う
  エイリアス(`tssh_kek_v2`)が記録される。エントリ欠落時に意味の薄いTypeCastException。
  - 方針: `KeyManager.KEK_ALIAS`を`KeystoreKek`の実エイリアスに揃え、`loadKey`は欠落時に
    明示的な例外を投げる。
- [ ] **AND-L4** (Low) `KeyImportViewModel.kt:35` / `KeyListViewModel.kt` — 鍵インポートのサイズ上限・
  PEM妥当性・パスフレーズ付き鍵の検証が無く、平文PEMのゼロ化も無い。
  - 方針: 上限付き読み出し、PEM/OpenSSH形式の検証とパスフレーズ付き鍵(未対応)の拒否、
    保存後の`fill(0)`。
- [ ] **AND-L5** (Low) `AndroidManifest.xml` — `dataExtractionRules`が無くAndroid 12+のD2D転送が
  止まらない(Keystoreは移らないので暗号化鍵ファイルが復号不能なゴミになる)。
  - 方針: 全ドメインを除外する`data_extraction_rules.xml`を追加。
- [ ] **AND-L6** (Low) `session/TerminalSession.kt:584-606` — host key信頼の書き込みが非同期で、
  直後の再接続がDB反映前にcheckして再プロンプトになりうる。
  - 方針: 書き込みJobを保持し、`onHostKey`(Rustのblockingスレッド)で完了を待ってからcheckする。
- [ ] **AND-L7** (Low) `TerminalSessionService.kt:78-80,102-106` / `session/AndroidAppExecutor.kt` —
  FGS起動(`startService`/`startForeground`)の例外未捕捉。
  - 方針: 両方をtry/catchしてログに落とす(`startForeground`失敗時は`stopSelf`)。
- [ ] **AND-L8** (Low) `session/TerminalSession.kt:680-686` — `appendLog`がチャンク境界でUTF-8を
  分断して化ける。受信ごとに最大200KBの文字列を再構築(O(n))。
  - 方針: ストリーミング`CharsetDecoder`で境界をまたぐマルチバイトを保持し、`StringBuilder`へ
    追記(トリムは閾値超過時のみ)。ログは要求時にスナップショットを返す。
- [ ] **AND-L9** (Low) `filepreview/ImageViewer.kt:27-29` — サンプリング無しのフル解像度デコードを
  コンポジション中(メインスレッド)に行う。
  - 方針: bounds→inSampleSizeで縮小し、`produceState`でバックグラウンドデコードする。
- [ ] **AND-L10** (Low) `session/TerminalSession.kt:212-216` — AIパネルのフォーム送信がPTYへの生書き込みで、
  要求元が終了済みだとシェルにコマンドとして解釈されうる。
  - 方針: 送信前に「要求元がまだ待っているか」をRust側で確認する必要がある。
- [ ] **AND-L11** (Low) `data/AppDatabase.kt:16-17` — `exportSchema=false`で`MigrationTestHelper`による
  スキーマ照合ができない。
  - 方針: スキーマexportを有効化してCIで検証する。
