# android-tssh 実機動作確認手順書

## A. サーバー側事前設定

### A-1. sshd_config の確認

```bash
# SSH サーバーにログインして確認
sudo grep -E "^(PasswordAuthentication|PubkeyAuthentication|AuthorizedKeysFile)" /etc/ssh/sshd_config
```

デフォルト値（記載なし = コメントアウト）はディストロによって異なるため、明示的に設定する。

---

### A-2. パスワード認証を有効にする

テスト環境のみ。本番では有効にしないこと。

```bash
sudo sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config
# 変更後に確認
grep PasswordAuthentication /etc/ssh/sshd_config
```

sshd を再起動:

```bash
# systemd 系（Ubuntu/Debian/Fedora など）
sudo systemctl restart ssh     # Ubuntu/Debian
sudo systemctl restart sshd    # RHEL/Fedora/Arch

# 再起動の確認
sudo systemctl status ssh
```

別ターミナルから動作確認（アプリを使う前に必ず確認）:

```bash
ssh -o PasswordAuthentication=yes <USER>@<SERVER_IP>
```

---

### A-3. 鍵認証の準備

#### テスト用 ed25519 鍵ペアを生成（PC 側）

既存の `~/.ssh/id_ed25519` をデバイスに転送してもよいが、テスト専用鍵を作ることを推奨（実機を紛失したときの被害を最小化する）。

```bash
ssh-keygen -t ed25519 -f ~/.ssh/android_tssh_test -N "" -C "android-tssh-device-test"
# -N "" でパスフレーズなし（アプリがパスフレーズ入力に未対応のため）
```

生成されるファイル:
- `~/.ssh/android_tssh_test`       ← 秘密鍵（デバイスに転送する）
- `~/.ssh/android_tssh_test.pub`   ← 公開鍵（サーバーに登録する）

#### サーバーに公開鍵を登録

```bash
# サーバー上の authorized_keys に追記
ssh-copy-id -i ~/.ssh/android_tssh_test.pub <USER>@<SERVER_IP>

# または手動で
cat ~/.ssh/android_tssh_test.pub | ssh <USER>@<SERVER_IP> 'mkdir -p ~/.ssh && cat >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys'
```

登録確認（サーバー上で）:

```bash
tail -1 ~/.ssh/authorized_keys
# → ssh-ed25519 AAAA... android-tssh-device-test
```

鍵認証で接続できることを確認（PC から）:

```bash
ssh -i ~/.ssh/android_tssh_test -o PasswordAuthentication=no <USER>@<SERVER_IP>
```

---

### A-4. 秘密鍵をデバイスへ転送

```bash
# Downloads フォルダにコピー（SAF ファイルピッカーから選択できる場所）
adb push ~/.ssh/android_tssh_test /sdcard/Download/android_tssh_test.pem

# 転送確認
adb shell ls -la /sdcard/Download/android_tssh_test.pem
```

> **Note**: SAF のファイルピッカーで「Download」フォルダを開けば `android_tssh_test.pem` が見える。
> 見えない場合は「Files」アプリを一度開いて同フォルダを表示してから、アプリに戻ってファイルを選択する。

アプリでのインポート手順はテスト項目 4「鍵インポート」を参照。

---

## 0. 事前準備（アプリ側）

### ADB 接続確認
```bash
adb devices
# デバイスが見つからない場合
SAVED_IP=$(cat ~/.config/android-adb-ip 2>/dev/null)
[ -n "$SAVED_IP" ] && adb connect "$SAVED_IP"
```

### APK ビルド・インストール
```bash
cd /home/cuzic/android-tssh
./gradlew installDebug
```

### logcat フィルタ起動（別ターミナルで常時表示）
```bash
adb logcat -s IsekaiTerminalNav IsekaiTerminalProfile IsekaiTerminalKey IsekaiTerminalSSH IsekaiTerminalIME TsshSvc IsekaiTerminalVM
```

---

## 1. 起動確認

### 手順
1. アプリを起動する

### 期待ログ
```
IsekaiTerminalVM: TerminalViewModel created (rotationRecovery=false)
IsekaiTerminalNav: → ProfileList
IsekaiTerminalProfile: loaded 0 profile(s): []
```

### NG 時の確認ポイント
- `TerminalViewModel created` が出ない → ViewModel 生成でクラッシュ（`AndroidRuntime` タグで例外確認）
- `ProfileList` が出ない → `AppRoot` の screen=0 branch が到達できていない
- `loaded N profile(s)` が出ない → Room init 失敗（DB パスを確認するため `Repositories` ログを追加）

---

## 2. プロファイル追加

### 手順
1. 画面右下「＋」をタップ
2. ラベル「test」・ホスト「<SSH サーバー IP>」・ポート「22」・ユーザー名「<ユーザー名>」を入力
3. 認証方式「パスワード」を選択して保存

### 期待ログ
```
IsekaiTerminalNav: → ProfileEdit(new)
IsekaiTerminalProfile: saving profile: label='test' host=<IP>:22 user=<USER> authType=password keyId=null id=new
IsekaiTerminalNav: → ProfileList
IsekaiTerminalProfile: loaded 1 profile(s): ['test']
```

### NG 時の確認ポイント
- `saving profile` が出ない → 保存ボタンが無効（ラベル/ホスト/ユーザーが空）
- `loaded 1 profile(s)` の代わりに 0 件 → DB upsert 失敗（`canSave` 条件を確認）

---

## 3. プロファイル編集・削除

### 手順（編集）
1. プロファイルカードを長押し → 鉛筆アイコンをタップ
2. ラベルを「test2」に変更して保存

### 期待ログ（編集）
```
IsekaiTerminalProfile: edit: 'test' id=1
IsekaiTerminalNav: → ProfileEdit(id=1 'test')
IsekaiTerminalProfile: saving profile: label='test2' ... id=1
IsekaiTerminalNav: → ProfileList
IsekaiTerminalProfile: loaded 1 profile(s): ['test2']
```

### 手順（削除）
1. プロファイルカードのゴミ箱アイコンをタップ → 確認ダイアログで「削除」

### 期待ログ（削除）
```
IsekaiTerminalProfile: deleted profile id=1 'test2'
IsekaiTerminalProfile: loaded 0 profile(s): []
```

---

## 4. 鍵インポート

### 前提
- Android デバイスのファイルアプリに ed25519 PEM ファイルを転送済み
  ```bash
  adb push ~/.ssh/id_ed25519 /sdcard/Download/id_ed25519.pem
  ```

### 手順
1. 画面右上「鍵管理」をタップ
2. 「＋」をタップ → 鍵インポート画面
3. ラベル入力 → 「PEM ファイルを選択」→ ファイルを選択
4. 「保存」をタップ

### 期待ログ
```
IsekaiTerminalNav: → KeyList
IsekaiTerminalKey: loaded 0 key(s): []
IsekaiTerminalNav: → KeyImport
IsekaiTerminalKey: file selected via SAF: id_ed25519.pem uri=content://...
IsekaiTerminalKey: import start: label='mykey' file='id_ed25519.pem'
IsekaiTerminalKey: read PEM: <N> bytes
IsekaiTerminalKey: encrypted key saved → /data/data/tools.isekai.terminal/files/keys/<UUID>.enc
IsekaiTerminalKey: key saved to DB: id=1 label='mykey'
IsekaiTerminalNav: → KeyList
IsekaiTerminalKey: loaded 1 key(s): ['mykey']
```

### NG 時の確認ポイント
- `file selected via SAF` が出ない → SAF picker がキャンセルされた、またはファイルピッカーが起動しない
- `read PEM: 0 bytes` → ContentResolver が空 URI を返している（ファイルパスを確認）
- `import failed` → エラーメッセージを確認（KeystoreKek 初期化失敗の可能性）
- `key file exists=false` が後で出る → 保存先ディレクトリが作成されていない（`KeyManager.saveEncryptedKey` の `mkdirs()` を確認）

---

## 5. SSH 接続（パスワード認証）

### 前提
- パスワード認証を受け入れる SSH サーバーが起動していること

### 手順
1. プロファイル一覧でパスワード認証プロファイルをタップ
2. パスワードダイアログにパスワードを入力 → 「接続」

### 期待ログ
```
IsekaiTerminalProfile: tap → password dialog: 'test' <USER>@<IP>:22
IsekaiTerminalProfile: password dialog confirmed for: 'test'
IsekaiTerminalNav: ProfileList → Terminal via profile='test' authType=password
IsekaiTerminalNav: → Terminal(profile='test' host=<IP>)
IsekaiTerminalVM: TerminalViewModel created (rotationRecovery=false)
TsshSvc: service created
TsshSvc: onStartCommand label='SSH セッション' flags=0 startId=1
IsekaiTerminalVM: service bound OK (session=false)
IsekaiTerminalSSH: TerminalScreen: launch connectProfile 'test' <USER>@<IP>:22
IsekaiTerminalSSH: connectProfile: 'test' <USER>@<IP>:22 authType=password keyId=null
IsekaiTerminalSSH: buildAuth: password auth → OK
IsekaiTerminalSSH: connect: <USER>@<IP>:22
IsekaiTerminalSSH: network available
IsekaiTerminalSSH: host key fingerprint: SHA256:...
IsekaiTerminalSSH: ✓ connected: <USER>@<IP>:22
IsekaiTerminalSSH: first data: <N>B
IsekaiTerminalSSH: terminal geometry: <C>×<R> px=<W>×<H> connected=true
IsekaiTerminalSSH: resize → <C>×<R>
```

### NG 時の確認ポイント
- `buildAuth: password empty → abort` → パスワードダイアログの値が ViewModel に渡っていない
- `connect error:` → ネットワーク到達不可 or 認証失敗（エラー本文を確認）
- `✓ connected` が出るが画面が黒いまま → `first data` を確認、出ていれば terminal geometry 計算の問題
- `terminal geometry` は出るが `resize` が出ない → `connected=false` のまま（`collectAsStateWithLifecycle` の遅延）
- `IsekaiTerminalVM: service bound OK` が出ない → Foreground Service が起動できていない（Manifest の `foregroundServiceType` を確認）

---

## 6. SSH 接続（鍵認証）

### 前提
- サーバーに対応する公開鍵が `authorized_keys` に登録済み

### 手順
1. プロファイル編集で authType を「鍵認証」にし、インポート済み鍵を選択して保存
2. プロファイルをタップして接続

### 期待ログ
```
IsekaiTerminalSSH: connectProfile: 'test' ... authType=key keyId=1
IsekaiTerminalSSH: buildAuth: loading key id=1
IsekaiTerminalSSH: buildAuth: decrypting key 'mykey' path=.../keys/<UUID>.enc
IsekaiTerminalSSH: buildAuth: key file exists=true size=<N>B
IsekaiTerminalSSH: buildAuth: key decrypted OK (<M> bytes) → SshAuth.PublicKey
IsekaiTerminalSSH: connect: <USER>@<IP>:22
IsekaiTerminalSSH: ✓ connected: <USER>@<IP>:22
```

### NG 時の確認ポイント
- `buildAuth: key file exists=false` → 鍵ファイルが消えている（アンインストール後の再インストールは KEK も消えるため再インポートが必要）
- `buildAuth: key file exists=true size=0B` → 暗号化保存が失敗していた（再インポートが必要）
- `buildAuth: key error:` → KEK 復号失敗（Keystore エイリアスが異なる or デバイス再起動後の生体認証要求）
- `connect error: auth failed` → サーバーの `authorized_keys` に公開鍵が未登録

---

## 7. ターミナル入力

### 7-A ASCII 入力
1. 接続後、ターミナルをタップしてソフトキーボードを表示
2. `ls -la` を打って Enter

### 期待ログ
```
IsekaiTerminalIME: input view focus: true (onSendBytes=true)
```
（ `ls -la` + Enter の各キーは `setComposingText`/`commitText`/`sendKeyEvent` 経由で送信されるが、1 文字ずつは DEBUG レベルのため通常ログには出ない）

### NG 時の確認ポイント
- `input view focus: false` のまま → TerminalInputView の `requestFocus()` が呼ばれていない
- `input view focus: true (onSendBytes=false)` → `AndroidView` の `update` ブロックが設定前に focus が来ている競合

### 7-B 日本語 IME 入力
1. 日本語 IME に切り替えて「日本語」を入力し確定

### 期待ログ
```
IsekaiTerminalIME: composing start: '日'
IsekaiTerminalIME: composing finish: '日本語' (3 chars) → sent
```

### NG 時の確認ポイント
- `composing start` が出ない → `setComposingText` が呼ばれていない（IME が `commitText` 直結型の可能性）
- `composing finish` が出て送信されているのに画面に文字が出ない → SSH サーバー側の echo 問題

### 7-C ブラケッテドペースト
1. クリップボードに複数行テキストをコピー
2. ターミナルを長押し → 「貼り付け」

### 期待ログ
```
IsekaiTerminalIME: paste <N> chars → bracketed paste
```

### NG 時の確認ポイント
- `paste` が出るが画面がおかしい → ターミナルパーサが bracketed paste モードを未対応（`[200~`/`[201~` が生テキストとして表示されている）

---

## 8. ピンチズーム

### 手順
1. ターミナル画面でピンチアウト（拡大）・ピンチイン（縮小）

### 期待ログ
```
IsekaiTerminalSSH: terminal geometry: <C>×<R> px=...    ← 文字サイズ変化でジオメトリ再計算
IsekaiTerminalSSH: resize → <C>×<R>
```

### NG 時の確認ポイント
- `resize` が出るが画面が崩れる → `SshTerminalCanvas.onDraw()` の cellW/cellH がフォントスケール変更に追随していない

---

## 9. 画面回転

### 手順
1. SSH 接続中にデバイスを横向きに回転させる

### 期待ログ（回転 = Activity 再生成）
```
IsekaiTerminalVM: TerminalViewModel cleared (session=true)   ← 旧 VM が破棄（session は true のまま！サービスが保持）
IsekaiTerminalVM: TerminalViewModel created (rotationRecovery=true)   ← 新 VM が生成
IsekaiTerminalVM: service bound OK (session=true)
IsekaiTerminalSSH: terminal geometry: <C>×<R> px=...   ← 横向きの新ジオメトリ
IsekaiTerminalSSH: resize → <C>×<R>
```
`TsshSvc: service created/destroyed` は出ないことを確認（Service は回転でも生存）

### NG 時の確認ポイント
- `TerminalViewModel cleared (session=true)` で session が false → Service より先に session が null にされている（`onCleared` の順序問題）
- `rotationRecovery=false` → bindService が 0 フラグで呼ばれているが Service がすでに停止していた
- `✗ disconnected` が回転中に出る → Session の GC root が Service ではなくどこか別の場所にある

---

## 10. バックグラウンド移行・復帰

### 手順
1. SSH 接続中にホームボタンで背景に移動
2. 30 秒後にアプリアイコンから復帰

### 期待ログ（ホーム押下時）
```
IsekaiTerminalVM: TerminalViewModel cleared (session=true)   ← Activity が停止・VM 破棄
```
（`TsshSvc: service destroyed` が出ないことを確認 — Foreground Service はバックグラウンドでも生存）

### 期待ログ（復帰時）
```
IsekaiTerminalVM: TerminalViewModel created (rotationRecovery=true)
IsekaiTerminalVM: service bound OK (session=true)
IsekaiTerminalSSH: ✓ connected: ...   ← onConnected は出ない（既に接続済み）
IsekaiTerminalSSH: terminal geometry: ...
```

### NG 時の確認ポイント
- `TsshSvc: service destroyed` が出る → 通知が消えた/OOM Killer に殺された（Foreground 通知の設定を確認）
- `rotationRecovery=false` → processが再起動された（ `TsshSvc: service created` も出ているはず）
- 画面が黒いまま → `session?.let { terminalService?.holdSession(it) }` が service bound 前に呼ばれていない競合

---

## 11. ネットワーク切断

### 手順
1. SSH 接続中に Wi-Fi をオフにする（設定から）

### 期待ログ
```
IsekaiTerminalSSH: network lost (wasConnected=true)
IsekaiTerminalSSH: ✗ disconnected: reason='...' host=<IP>
```

### NG 時の確認ポイント
- `network lost` が出るが `✗ disconnected` が出ない → `session?.disconnect()` は呼ばれているが SSH ライブラリの `onDisconnected` が非同期に遅れている（数秒待つ）
- どちらも出ない → `ConnectivityManager.NetworkCallback` が登録されていない（`init` ブロックを確認）

---

## 12. 切断ボタン

### 手順
1. 接続中に「切断」ボタンをタップ

### 期待ログ
```
IsekaiTerminalSSH: disconnect called (connected=true)
IsekaiTerminalSSH: ✗ disconnected: reason='...'
IsekaiTerminalNav: → ProfileList
```

### NG 時の確認ポイント
- `disconnect called (connected=false)` → すでに切断済みの状態でボタンが有効だった（UI 状態のバインディング確認）

---

## まとめ：全タグ一覧

| タグ | 対象コンポーネント |
|---|---|
| `IsekaiTerminalVM` | TerminalViewModel ライフサイクル |
| `TsshSvc` | TerminalSessionService ライフサイクル |
| `IsekaiTerminalSSH` | SSH セッション接続・切断・データ |
| `IsekaiTerminalNav` | 画面遷移 |
| `IsekaiTerminalProfile` | プロファイル CRUD |
| `IsekaiTerminalKey` | 鍵インポート・管理 |
| `IsekaiTerminalIME` | IME 入力・ペースト・フォーカス |

```bash
# 全タグ同時フィルタ
adb logcat -s IsekaiTerminalVM TsshSvc IsekaiTerminalSSH IsekaiTerminalNav IsekaiTerminalProfile IsekaiTerminalKey IsekaiTerminalIME

# クラッシュが出た時
adb logcat -d -b crash | tail -n 200
adb logcat -d | grep -E "FATAL|AndroidRuntime|E/.*Exception" | tail -n 50
```

---

## 13. Phase 4D: trzsz 実機回帰テスト (#70)

trzsz ファイル転送（trz=アップロード / tsz=ダウンロード）が実機・実サーバーで
正しく動作することを確認する。Rust コアのプロトコルログはタグ `isekai-terminal-core` で出る。

### 前提

- 実機（Pixel 6+ 推奨）に最新 APK をインストール済み（`./gradlew installDebug`）
- Tailscale で到達できる Linux サーバーに trzsz-go がインストール済み
  ```bash
  # サーバー側で確認
  which trz tsz trzsz   # trz/tsz が PATH にあること
  trz --version
  ```
- ログキャプチャを別ターミナルで起動:
  ```bash
  ./scripts/capture_trzsz_log.sh           # isekai-terminal-core + Tssh* の trzsz 行のみ抽出
  ```

### 13-A アップロード (trz)

#### 手順
1. SSH 接続後、サーバーシェルで `trz` を実行
2. アプリ側でファイル選択ダイアログ（SAF）が表示されることを確認
3. 任意の小ファイル（< 1MB）を選択
4. 転送 UI が進捗を表示し、完了（Done / 成功）状態になることを確認
5. サーバー側で受信を確認: `ls -la <filename>`

#### MD5 一致確認
```bash
# アップロード前にローカル（PC 側または元ファイル）で算出した値と比較
md5sum <filename>          # サーバー側
```
アプリは upload 完了時に `#MD5:` を送出する。サーバーの `trz` 側で MD5 不一致が
報告されないこと（不一致ならログに `#SUCC:false` 相当が出る）。

#### 期待ログ（isekai-terminal-core）
```
isekai-terminal-core: ::TRZSZ:TRANSFER:S:1.1.7:<id>   ← upload(send) トリガ検出
isekai-terminal-core: #ACT ...                         ← action handshake
isekai-terminal-core: #NUM / #NAME / #SIZE ...         ← ファイルメタ送出
isekai-terminal-core: #DATA ...                        ← チャンク送出
isekai-terminal-core: #MD5 ...                         ← チェックサム
isekai-terminal-core: #SUCC ...                        ← 完了
```

### 13-B ダウンロード (tsz)

#### 手順
1. サーバーに小ファイルを用意:
   ```bash
   echo "hello trzsz" > /tmp/test.txt
   ```
2. SSH 接続後、サーバーシェルで `tsz /tmp/test.txt`
3. アプリ側でダウンロード UI が表示されることを確認
4. 転送完了後、ファイルが端末の Downloads に保存されることを確認
5. 内容一致を確認（保存ファイルを開く or `adb pull` して `md5sum`）

#### 期待ログ（isekai-terminal-core）
```
isekai-terminal-core: ::TRZSZ:TRANSFER:R:1.1.7:<id>   ← download(receive) トリガ検出
isekai-terminal-core: #ACT ... / #NAME ... / #SIZE ...
isekai-terminal-core: #DATA ...                        ← onTrzszDownloadChunk へ
isekai-terminal-core: #SUCC ...
```

### 13-C ACT/CFG ハンドシェイク確認

```bash
adb logcat -s isekai-terminal-core | grep -E '#ACT|#NUM|#NAME|#SIZE|#MD5|#SUCC'
```
`#ACT` に続いてメタ情報（`#NUM`/`#NAME`/`#SIZE`）→ `#DATA` → `#MD5`/`#SUCC`
の順で流れること。

### 13-D 回帰チェックリスト（PLAN.md Phase 4D）

```
□ tmux / htop / alt screen が転送前後で壊れない
□ 100MB+ ファイルで OOM しない
□ 画面回転・バックグラウンド中の転送が破綻しない
□ 転送キャンセル後の端末状態が正常（プロンプトに戻る）
□ 同一セッションで複数回 trz / tsz を連続実行できる
```

### 13-E エラーケース

- 転送中に Cancel ボタンで中断できること（端末がプロンプトに復帰）
- タイムアウト（無応答）で Recovering 状態に遷移し、生バイトが端末へ
  フラッシュされること（FSM の `test_transfer_timeout_goes_to_recovering`
  が保証する挙動の実機確認）

---

## 14. Phase 5 判断ゲート: TCP SSH 遅延計測 (#71)

tsshd/QUIC（Phase 5）へ進むべきかを判断するため、まず Tailscale 経由の
TCP SSH の遅延と切断耐性を実測する。

### 前提
- Tailscale 接続済みの実機 / PC
- Linux サーバー（例: 100.100.45.36、ユーザー `cuzic`）
- PC に `adb` と `ssh` が使えること

### 14-A 遅延計測スクリプト
```bash
./scripts/measure_latency.sh                       # 既定: 100.100.45.36:22 cuzic ×30
./scripts/measure_latency.sh <host> <port> <user> <iters>
```
出力: TCP 接続レイテンシ、SSH 往復（connect+echo+teardown）の min/avg/p50/p95/max。

### 14-B 切断耐性の実測（判断の本体）
1. 実機で SSH 接続し、`top` などを表示したまま保持
2. 5G（モバイル）→ WiFi（またはその逆）に切り替える
3. TCP SSH セッションが**実際に切れるか**を観察
   - `IsekaiTerminalSSH: network lost` / `✗ disconnected` が出れば切断
4. 判断:
   - 切れる → Phase 5（tsshd/QUIC によるローミング耐性）に価値あり
   - 切れない（TCP が生存し続ける）→ **Phase 5 はスキップ**

### 14-C QUIC 採用基準（Phase 5 に進む場合）
- 接続移行（migration）オーバーヘッド: < 100ms
- steady-state RTT: TCP の ±10% 以内

---

## スクリプト一覧

| スクリプト | 用途 |
|---|---|
| `scripts/measure_latency.sh` | Phase 5 判断ゲート: TCP SSH 往復遅延計測 |
| `scripts/capture_trzsz_log.sh` | Phase 4D: trzsz 転送ログ（isekai-terminal-core）キャプチャ |

---

## ホスト側ビルド・テスト（Rust コア）

```bash
cd rust-core
cargo build --lib
cargo test --lib      # 22 tests（trzsz FSM / detector / codec）
cargo check --lib     # warning 0 / error 0
```
