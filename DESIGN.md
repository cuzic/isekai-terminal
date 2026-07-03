# 設計書：android-tssh

**プロジェクト目標**:
> Android スマートフォン単体（フリック入力・ソフトキーボード）で、日本語 IME に完全対応した SSH クライアントを実現する。

trzsz ファイル転送（tssh 互換）と Mosh は後続フェーズとして追加する。

---

## 1. スコープ

### やること
- SSH 接続（パスワード認証・公開鍵認証・ed25519/RSA/ECDSA）
- xterm-256color 互換ターミナルエミュレーション
- 日本語 IME 入力（フリック・QWERTY ローマ字・予測変換）
- 特殊キーボタン（Esc, Ctrl+C/D/Z, Tab, 矢印, F1〜F12）
- Host key verification（TOFU + known_hosts 管理）
- 接続プロファイル管理
- SSH 公開鍵管理（生成・インポート）
- trzsz ファイル転送（P5）
- Mosh 接続（P6、SSP over UDP + AES-128 OCB、Rust 実装）

### やらないこと（将来フェーズ）
- iOS 対応（Swift UI + 同 Rust コア共有。UniFFI が Swift バインディングを生成するため UI 層のみ別実装で済む）
- X11 転送
- ポートフォワーディング
- ~~SSH エージェント転送~~ → 方針転換して追加（既定 OFF・プロファイル単位 opt-in、
  署名要求ごとにユーザー確認必須。詳細は `PLAN.md` の該当節を参照）
- ZMODEM（trzsz で代替）
- OSC 52 クリップボード（セキュリティリスク検討後に判断）
- マウスレポーティング

---

## 2. アーキテクチャ

```
┌─────────────────────────────────────────────────────────────┐
│  UI Layer  (Kotlin / Jetpack Compose)                        │
│  ProfileListScreen │ TerminalScreen │ KeyManagementScreen    │
├─────────────────────────────────────────────────────────────┤
│  Input Layer  (kmp-terminal-input 1.0.3)                     │
│  IME composing → ByteArray  /  injectKey(VirtualKey)         │
├─────────────────────────────────────────────────────────────┤
│  TerminalSessionService  (Android Foreground Service)        │
│  ─ Rust runtime owner  ─ session lifecycle  ─ notification   │
│  SessionViewModel は bind して状態購読するだけ                 │
├──────────────────────────┬──────────────────────────────────┤
│  Terminal Renderer        │  Rust Core  (UniFFI)             │
│  (Kotlin / Compose)       │                                  │
│                           │  ssh/     russh 0.61+            │
│  TerminalSurface          │  terminal/ vte parser            │
│    Canvas {               │            screen model          │
│      drawText(runs)       │            attrs / charset       │
│      drawCursor()         │            scrollback            │
│      drawSelection()      │  trzsz/   detector + transfer    │
│    }                      │  mosh/    SSP + AES-OCB (P6)     │
│                           │                                  │
│  FFI update単位:          │  FrameUpdate {                   │
│    30〜60fps batch flush   │    epoch, rows[], cursor, title  │
│                           │  }                               │
├──────────────────────────┴──────────────────────────────────┤
│  Data Layer  (Room + EncryptedSharedPreferences)             │
│  ConnectionProfile  /  KnownHost  /  KeyEntry (KEK方式)      │
└─────────────────────────────────────────────────────────────┘
```

---

## 3. 技術スタック

| 層 | 選択 | 備考 |
|----|------|------|
| UI | Jetpack Compose | スパイクで実績あり |
| 入力 | kmp-terminal-input 1.0.3 | スパイクで Go 判定済み。fork/vendor 前提で扱う |
| FFI | UniFFI 0.28+ | Rust ↔ Kotlin/Swift。Gradle plugin は手作業が一部必要 |
| SSH | russh 0.61+ | 純 Rust、async/tokio。P0 で interactive PTY をスパイク |
| VT100 | vte crate（Rust）+ 独自 Perform 実装 | parser のみ。screen model は自前 |
| 描画 | Kotlin Canvas（行・run 単位） | LazyGrid は不採用 |
| Mosh | Rust 独自実装（SSH bootstrap + SSP over UDP + AES-128 OCB） | GPLv3 の C++ コードは参照のみ、clean-room 方針 |
| プロファイル DB | Room | |
| 秘密鍵保管 | KEK 方式（詳細は §8） | |
| ビルド | cargo-ndk → Gradle | |

---

## 4. Rust コア構成

### 4-1. ディレクトリ

```
rust-core/
├── src/
│   ├── lib.rs              # UniFFI エントリポイント
│   ├── ssh/
│   │   ├── client.rs       # russh セッション管理
│   │   ├── auth.rs         # パスワード・公開鍵認証
│   │   ├── channel.rs      # PTY チャンネル
│   │   └── hostkey.rs      # host key fingerprint
│   ├── terminal/
│   │   ├── parser.rs       # vte ベース（Perform trait 実装）
│   │   ├── screen.rs       # main/alt screen, cursor, scroll region
│   │   ├── attrs.rs        # SGR（bold/underline/inverse/italic/color）
│   │   ├── charset.rs      # UTF-8, combining, CJK 全角幅
│   │   ├── modes.rs        # bracketed paste, application cursor
│   │   └── scrollback.rs   # main screen スクロールバック
│   ├── trzsz/
│   │   ├── detector.rs     # マジックバイト検出（raw byte stream tee）
│   │   ├── receiver.rs     # trz ダウンロード
│   │   └── sender.rs       # tsz アップロード
│   └── mosh/               # P6
│       ├── bootstrap.rs    # SSH 経由で mosh-server 起動・鍵受取
│       ├── ssp.rs          # State Synchronization Protocol
│       ├── ocb.rs          # AES-128 OCB 暗号化
│       └── state_sync.rs   # サーバ terminal state 差分同期
└── uniffi.udl              # バインディング定義
```

### 4-2. UniFFI インターフェース（抜粋）

```rust
// 差分通知は FrameUpdate でバッチ化（1セルずつ渡さない）
[Trait, WithForeign]
interface TerminalCallback {
    void on_frame(&self, FrameUpdate update);
    void on_host_key_unknown(&self, string host, u32 port, string fingerprint);
    void on_host_key_changed(&self, string host, u32 port, string fingerprint);
    void on_disconnected(&self, string? reason);
};

dictionary FrameUpdate {
    u64 epoch;
    sequence<RowUpdate> rows;
    CursorState cursor;
    string? title;
    boolean bell;
};

dictionary RowUpdate {
    u32 row;
    sequence<TextRun> runs;
};

dictionary TextRun {
    string text;
    Color fg;
    Color bg;
    CellAttrs attrs;
};
```

### 4-3. trzsz インターセプト位置

```
SSH stdout raw bytes
  └─→ TrzszDetector
        ├─ trzsz シーケンス検出時:  terminal parser を一時停止 → trzsz transfer へ
        └─ 通常時:                  vte parser へ流す
```

trzsz が terminal parser より**前段**に入る点が重要。parser 後では検出が遅れる。

### 4-4. Mosh プロトコル（P6）

Mosh は DTLS を**使わない**。以下が正しい構成：

```
1. SSH セッションで mosh-server を起動
2. SSH stdout から UDP ポートと共有鍵（base64）を受け取る
3. SSH セッションをクローズ
4. Rust Mosh クライアントが UDP 接続を確立
5. SSP フレームを AES-128 OCB で暗号化して送受信
6. サーバ側 terminal state と差分同期（octet stream ではなく状態同期）
```

> ⚠️ GPLv3 ライセンス方針：mosh 公式 C++ 実装（GPLv3）のコードを移植しない。  
> 論文・RFC・仕様書・挙動観察のみを参考に clean-room で Rust 実装する。  
> P6 着手前に法務・ライセンス方針を正式に確認すること。

---

## 5. 描画戦略

**LazyGrid は採用しない。** 理由：

- 80×24 = 1,920 セルを Compose node として扱うと差分更新・カーソル点滅・`top` の連続更新で破綻する
- 30〜60fps の terminal update を Compose の recomposition で捌くのは困難

**採用：Canvas 行・run 単位描画**

```kotlin
@Composable
fun TerminalSurface(state: TerminalRenderState) {
    Canvas(modifier = Modifier.fillMaxSize()) {
        state.rows.forEach { row ->
            row.runs.forEach { run ->
                drawText(run.text, run.fg, run.bg, run.attrs, x, y)
            }
        }
        drawCursor(state.cursor)
    }
}
```

Rust → Kotlin の `FrameUpdate` は **30〜60fps でバッチ flush**。  
epoch 番号で古いフレームを捨てる。

---

## 6. セッションライフサイクル

```
TerminalSessionService (Foreground Service)
  ├─ Rust tokio runtime を所有
  ├─ SshSession / MoshSession を保持
  ├─ 通知バーにセッション状態を表示
  └─ ViewModel は startService / bindService で接続
        Activity / Compose は ViewModel 経由で状態購読

画面回転 → Activity 再生成 → Service はそのまま継続
バックグラウンド → Service 継続（foreground）
プロセス kill → Service 再起動ロジックで再接続試行（Mosh のみ）
```

> ⚠️ Android 14 以降では `AndroidManifest.xml` に `android:foregroundServiceType` の宣言が必要。  
> SSH/Mosh 用途は `remoteMessaging` または `connectedDevice` type が該当候補。P1 で確定する。

---

## 7. Host Key Verification

```kotlin
@Entity
data class KnownHost(
    @PrimaryKey val id: Long = 0,
    val host: String,
    val port: Int,
    val keyType: String,          // "ssh-ed25519" etc.
    val fingerprintSha256: String,
    val firstSeenAt: Instant,
    val lastSeenAt: Instant,
)
```

- **初回接続**：fingerprint を表示して TOFU 確認ダイアログ
- **2回目以降**：DB と照合、一致すれば透過接続
- **変化時**：強い警告ダイアログ（MITM の可能性）、ユーザー明示確認なしに接続しない

---

## 8. 秘密鍵保管方針

Android Keystore は key material をアプリプロセスに出さない設計のため、  
Rust (russh) に秘密鍵 bytes を直接渡す構成とは相性が悪い。

### 採用：案 A（P4 実装、KEK 方式）

```
private_key.enc  ── アプリ内部ストレージに暗号化保存
KEK alias        ── Android Keystore で管理（key material は出ない）
Rust russh       ── Kotlin で復号した鍵 bytes を受け取る（メモリ上のみ）
```

鍵素材はアプリプロセスのメモリには入るが、ストレージには平文で残らない。  
「Keystore に秘密鍵が保管されている」という表現は不正確なので使わない。

### 将来：案 B（ハードウェアバックド署名）

```
russh の Signer trait
  └─→ Kotlin KeystoreSigner（JNI 経由）
        └─→ Android Signature API（署名のみ、鍵は出ない）
```

実装難度が高いため将来フェーズとする。

---

## 9. Terminal 互換性ターゲット

| 機能 | 対応方針 |
|------|---------|
| xterm-256color | 対応（P2） |
| TrueColor（24bit） | 対応（P2） |
| Alt screen | 対応（vim/less/tmux 必須） |
| Bracketed paste | 対応（P3） |
| OSC title（xterm タイトル） | 対応（P2） |
| CJK 全角幅 | 対応（2セル幅、`unicode-width` crate） |
| Combining character | 対応（P2） |
| Emoji 幅 | ベストエフォート（P2） |
| Mouse reporting | 将来検討 |
| OSC 52 clipboard | セキュリティ検討後に判断 |

---

## 10. データモデル

```kotlin
@Entity
data class ConnectionProfile(
    @PrimaryKey val id: Long = 0,
    val label: String,
    val host: String,
    val port: Int = 22,
    val username: String,
    val authType: AuthType,       // PASSWORD / KEY
    val keyId: Long? = null,
    val useMosh: Boolean = false,
    val encoding: String = "UTF-8"
)

@Entity
data class KeyEntry(
    @PrimaryKey val id: Long = 0,
    val label: String,
    val publicKey: String,
    val encryptedPrivateKeyPath: String,  // アプリ内部ストレージのパス
    val kekAlias: String                  // Android Keystore の KEK エイリアス
)
```

---

## 11. 実装フェーズ

| フェーズ | 内容 | 成果物 |
|---------|------|--------|
| **P0** | 技術リスク潰し（russh interactive PTY・UniFFI コールバック・Canvas 描画・Keystore KEK 方式・FGS type・Mosh OCB 互換性調査） | スパイク群 |
| **P1** | SSH 接続 + host key verification + PTY + 最小描画 | `ls` が動く最小 APK |
| **P2** | terminal model + Canvas renderer + resize + scrollback + CJK | bash/vim/tmux が使えるターミナル |
| **P3** | 日本語 IME + RAW/TEXT + bracketed paste + 特殊キーボタン | 日本語でサーバ操作できる |
| **P4** | プロファイル管理 + known_hosts + 公開鍵認証（KEK 方式） | リリース相当の SSH クライアント |
| **P5** | trzsz ファイル転送（SAF 連携） | `trz`/`tsz` でファイル送受信 |
| **P6** | Mosh（clean-room Rust 実装、法務確認後） | Mosh 接続対応 |
| **P7** | tsshd 互換（UDP SSH + roaming）検討 | — |

---

## 12. 動作確認シナリオ（チェックリスト）

### P0: 技術スパイク

- [ ] russh で interactive PTY が確立できる（`ls`/`vim` が応答する）
- [ ] UniFFI コールバックが Tokio スレッドから Android Main スレッドへ安全に届く
- [ ] Canvas で 80×24 グリッドを 60fps で描画しても jank が出ない
- [ ] Android Keystore KEK 方式で鍵を暗号化→復号できる
- [ ] Foreground Service type を宣言し Android 14 実機で例外が出ない
- [ ] Mosh AES-128 OCB の Rust 実装候補（`ocb3` crate 等）を特定できる

### P1: SSH 接続・基本 PTY

- [ ] パスワード認証で SSH 接続できる
- [ ] PTY が確立される（`tty` で `/dev/pts/N` が返る）
- [ ] `echo hello` を送ると `hello` が返る
- [ ] `exit` で正常切断される
- [ ] 接続失敗時にエラーが表示される（ホスト不正・パスワード誤り）
- [ ] **初回接続時に host key fingerprint ダイアログが出る**
- [ ] **2回目以降は透過接続される**
- [ ] **fingerprint 変化時に強い警告ダイアログが出る**
- [ ] Foreground Service が起動し通知バーにセッション状態が出る
- [ ] 画面を回転させてもセッションが切れない
- [ ] バックグラウンドに移行してもセッションが維持される
- [ ] ネットワーク切断時にセッションが適切にクリーンアップされる

### P2: ターミナルエミュレーション

- [ ] `ls --color` でファイル名に色がつく（ANSI カラー）
- [ ] `vim` が起動し画面が正しくレイアウトされる
- [ ] vim でカーソルが移動する（矢印キー ANSI エスケープ）
- [ ] vim の Insert/Normal モード切替が動く（Esc キー）
- [ ] `top` / `htop` でリアルタイム更新が正しく描画される
- [ ] `tmux` が起動しステータスバーが描画される
- [ ] ウィンドウサイズ変更（ピンチズーム）で `SIGWINCH` が送られ再描画される
- [ ] 256色が正しく描画される
- [ ] TrueColor（24bit）が正しく描画される
- [ ] 日本語文字（全角）が 2 セル幅で正しく描画される
- [ ] スクロールバックが動作する（スワイプで過去ログを遡れる）
- [ ] Alt screen 切替で vim ↔ shell が正しく切り替わる
- [ ] OSC title でシェル側から設定したタイトルが反映される

### P3: 日本語 IME・特殊キー

- [ ] TEXT モード・フリック入力で日本語を確定送信できる
- [ ] TEXT モード・QWERTY ローマ字で日本語を確定送信できる
- [ ] 予測変換からの確定が 1 回の送信として届く（分割なし）
- [ ] RAW モードで各キーが即時送信される
- [ ] Esc ボタンで `1b` が送信される
- [ ] Ctrl+C ボタンで `03` が送信され実行中コマンドが中断される
- [ ] Ctrl+D ボタンで `04` が送信される（EOF）
- [ ] Ctrl+Z ボタンで `1a` が送信される（ジョブ停止）
- [ ] Tab ボタンでコマンド補完が動作する
- [ ] 矢印キーでコマンド履歴・カーソル移動が動く
- [ ] TEXT→RAW 切替時に composing 状態が宙吊りにならない
- [ ] Bracketed paste モードで貼り付けが正しく送信される

### P4: プロファイル管理・公開鍵認証

- [ ] プロファイルを追加・編集・削除できる
- [ ] ed25519 鍵ペアをアプリ内で生成できる
- [ ] 生成した公開鍵を表示・コピーできる
- [ ] 公開鍵認証で SSH 接続できる
- [ ] 外部ファイル（.pem 等）から秘密鍵をインポートできる
- [ ] アプリ再起動後もプロファイルが保持されている
- [ ] 秘密鍵は内部ストレージに KEK 方式で暗号化保存されている（平文なし）

### P5: trzsz ファイル転送

- [ ] サーバで `trz` を実行するとアップロードダイアログが出る
- [ ] Android SAF でアップロードファイルを選択できる
- [ ] ファイルアップロードが完了する（md5sum 一致）
- [ ] サーバで `tsz <file>` を実行するとダウンロードが開始される
- [ ] Android SAF でダウンロード保存先を選択できる
- [ ] ダウンロードが完了する（md5sum 一致）
- [ ] 転送中にプログレスが表示される
- [ ] 転送キャンセルが動作する
- [ ] 転送中に画面回転しても transfer job が継続する
- [ ] 転送中に SSH stdout を terminal parser に誤投入しない

### P6: Mosh 接続

- [ ] SSH 経由で `mosh-server` が起動し UDP ポートと鍵を受け取れる
- [ ] Mosh セッションが確立される
- [ ] 通常のコマンド実行が動作する
- [ ] ネットワーク一時切断後に自動再接続される（Mosh の主要機能）
- [ ] Wi-Fi ↔ モバイルデータ切替後もセッションが継続する
- [ ] `mosh-server` が見つからない場合に適切なエラーが出る

### 共通・非機能

- [ ] バックグラウンド移行でセッションが切れない（Foreground Service）
- [ ] 通知バーにセッション状態が表示される
- [ ] メモリリークがない（長時間接続で安定動作）
- [ ] フォントサイズをピンチズームで変更できる
- [ ] セッションログをクリップボードにコピーできる

---

## 13. 未解決・要調査事項

| 項目 | 状況 | 備考 |
|------|------|------|
| Mosh AES-128 OCB の Rust 実装 | P0 で調査 | `ocb3` crate が使えるか確認 |
| vte crate の CJK 全角幅対応 | P0 で調査 | `unicode-width` crate と組み合わせ |
| UniFFI コールバックのスレッド安全性 | P0 でスパイク | Tokio runtime と Android MainThread の境界 |
| Foreground Service type 確定 | P1 序盤で確定 | Android 14 の `fgs-types-required` |
| Mosh GPLv3 ライセンス方針 | P6 前に法務確認 | clean-room 実装でリスク低減 |
| tsshd 互換（UDP SSH + roaming）の評価 | P7 で検討 | Mosh SSP より統合しやすい可能性あり |
