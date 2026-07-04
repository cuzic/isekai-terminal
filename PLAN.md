# android-tssh 実装計画 v4

## プロジェクト定義

**Android ネイティブ tssh subset** — Rust (UniFFI) + Kotlin (Compose) 構成の Android SSH クライアント。
tssh（trzsz-ssh）の全機能ではなく、**tssh の中核体験を Android に移植**することを目標とする。

```
MVP スコープ:
  1. trzsz ファイル転送（trz / tsz）
  2. optional な tsshd/QUIC 接続耐性（ローミング）

対象外（現時点）:
  - tssh のバッチログイン・パスワード管理
  - tssh のプロキシ対応
  - tssh の OpenSSH 全オプション互換
```

---

## 完了済み（Phase 1–3 + Phase 0）

- SSH 接続（パスワード / ed25519 鍵、鍵生成・インポート）
- VT100/VTE パーサー、256色/TrueColor、alt screen（htop/tmux 動作済み）
- RAW モード入力 + IME composing（日本語入力）
- プロファイル管理、鍵管理
- ForegroundService（バックグラウンド・画面回転耐性）
- スクロールバック（Rust リングバッファ 1000行 + スワイプ UI）
- SSH keepalive（60秒）、TOFU ホスト鍵検証、フォントサイズ永続化

### Phase 0A: TerminalTransport 抽象化 ✅

`run_session()` を **Transport task + event dispatch loop** に分割。

```rust
enum TransportCommand { WriteStdin(Vec<u8>), Resize{cols,rows}, Disconnect }
enum TransportEvent  { HostKey(String), Connected, Stdout(Vec<u8>),
                       Resized{cols,rows}, Disconnected{reason} }
struct TransportHandle { cmd_tx: mpsc::Sender<TransportCommand> }
```

```
SshSession
  ├── TransportHandle  (→ run_russh_transport task)
  └── session_event_loop task
        ├── TrzszTransferFsm  ← Phase 4A で追加
        ├── VTE / Scrollback
        └── UniFFI callback   → Kotlin
```

---

## Phase 4: trzsz ファイル転送

### 使用クレート: timed-fsm

`rust-nicola/crates/timed-fsm`（MIT ライセンス、pure std、ゼロ依存）を
`TrzszTransferFsm` のバックエンドとして使う。

```toml
# rust-core/Cargo.toml
timed-fsm = { path = "../../rust-nicola/crates/timed-fsm" }
```

**なぜ timed-fsm か:**
- 30秒タイムアウトを FSM 内で宣言的に記述できる（`Response::with_timer(...)`）
- `on_event` / `on_timeout` が同期 → tokio `select!` から直接呼べる
- テストが `on_event`/`on_timeout` の直接呼び出しで書ける（tokio 不要）
- `StepCoro`（`!Send`）は**使わない**。`TimedStateMachine` trait のみを使う

### Phase 4A-0: trzsz detector golden test ✅

`TrzszTransferFsm` の動作仕様を先に golden test として固めた。
テストは `on_event(TrzszEvent::StdoutBytes(bytes))` を直接呼び、
`Response.actions` の `TrzszEffect` を検査する（tokio 不要）。

### Phase 4A: TrzszTransferFsm 実装

#### 設計図

```rust
// Events: FSM への入力
pub enum TrzszEvent {
    StdoutBytes(Vec<u8>),           // SSH stdout から
    KotlinAcceptUpload {            // Kotlin: ファイル選択完了（trz 用）
        transfer_id: String,
        file_name: String,
        file_size: u64,
        mode: u32,
    },
    KotlinChunk {                   // Kotlin: upload chunk 送信
        transfer_id: String,
        data: Vec<u8>,
        is_last: bool,
    },
    KotlinAcceptDownload {          // Kotlin: 保存先選択完了（tsz 用）
        transfer_id: String,
    },
    KotlinCancel {                  // Kotlin: ユーザーキャンセル
        transfer_id: String,
    },
}

// Effects: FSM からの出力（ActionExecutor が実行する副作用）
pub enum TrzszEffect {
    FlushVte(Vec<u8>),              // VTE パーサーに流す
    SendStdin(Vec<u8>),             // SSH stdin に送る
    // Kotlin コールバック
    OnTrzszRequest {                // trz/tsz 検出 → Kotlin に通知
        transfer_id: String,
        mode: TrzszMode,
        suggested_name: Option<String>,
        expected_size: Option<u64>,
    },
    OnDownloadChunk {               // tsz: download chunk → Kotlin
        transfer_id: String,
        data: Vec<u8>,
        is_last: bool,
    },
    OnProgress {
        transfer_id: String,
        transferred: u64,
        total: Option<u64>,
    },
    OnFinished {
        transfer_id: String,
        success: bool,
        message: Option<String>,
    },
}

// Timers
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrzszTimer {
    Transfer,   // 30 秒: 無応答でキャンセル
}

// FSM 内部状態
pub struct TrzszTransferFsm {
    state: TrzszFsmState,
    tail_buf: Vec<u8>,
    next_id: u64,
}

enum TrzszFsmState {
    Normal,
    WaitingKotlin { transfer_id: String, mode: TrzszMode },
    Transferring { transfer_id: String, mode: TrzszMode, transferred: u64, total: Option<u64> },
    Recovering,
}
```

**主要な遷移:**

```
Normal + StdoutBytes(bytes):
  → tail_buf に追加し magic を後方検索
  → 未検出: FlushVte(safe_prefix) を emit、残りを tail_buf に保持
  → 検出完了: WaitingKotlin へ遷移
    emit FlushVte(prefix), OnTrzszRequest(transfer_id, mode, ...)
    set timer(Transfer, 30s)

WaitingKotlin + KotlinAcceptUpload:
  → Transferring へ遷移
  reset timer(Transfer, 30s)

Transferring(upload) + KotlinChunk(data, is_last):
  → SendStdin(base64_json_chunk) を emit
  emit OnProgress(...)
  → is_last: Normal へ遷移, emit OnFinished(success=true), kill timer

Transferring + StdoutBytes(bytes):
  → trzsz JSON メッセージを解析
  → download: OnDownloadChunk を emit, reset timer
  → ACK/SUCC: Normal へ遷移, emit OnFinished(success=true), kill timer

Transferring/WaitingKotlin + KotlinCancel:
  → Recovering へ遷移
  emit SendStdin(Ctrl+C), OnFinished(success=false, "Cancelled")
  kill timer

Transfer timeout:
  → Recovering へ遷移
  emit SendStdin(Ctrl+C), OnFinished(success=false, "Transfer timeout")

Recovering + StdoutBytes(bytes):
  → FlushVte(bytes) を emit（VTE へ戻して自然回復を待つ）
  → プロンプトが戻ったら Normal へ（簡易: 一定量の bytes 後 or 改行検出）
```

**Recovery の原則（v3 から継承）:**
- VTE 状態を勝手に巻き戻さない
- サーバー側の自然回復に任せる
- UI に「転送を中断しました」を表示するだけ

#### TokioTimerRuntime（tokio との橋渡し）

```rust
struct TokioTimerRuntime {
    handles: HashMap<TrzszTimer, tokio::task::JoinHandle<()>>,
    timeout_tx: mpsc::Sender<TrzszTimer>,
}

impl TimerRuntime for TokioTimerRuntime {
    type TimerId = TrzszTimer;
    fn set_timer(&mut self, id: TrzszTimer, dur: Duration) {
        self.kill_timer(id);  // 既存タイマーをリセット
        let tx = self.timeout_tx.clone();
        let h = tokio::spawn(async move {
            tokio::time::sleep(dur).await;
            let _ = tx.send(id).await;
        });
        self.handles.insert(id, h);
    }
    fn kill_timer(&mut self, id: TrzszTimer) {
        if let Some(h) = self.handles.remove(&id) { h.abort(); }
    }
}
```

#### TrzszEffectExecutor（副作用の実行）

```rust
struct TrzszEffectExecutor {
    transport_cmd_tx: mpsc::Sender<TransportCommand>,  // SendStdin 用
    vte_tx: mpsc::Sender<Vec<u8>>,                      // FlushVte 用
    callback: Arc<dyn SessionCallback>,                 // Kotlin 通知用
}

impl ActionExecutor for TrzszEffectExecutor {
    type Action = TrzszEffect;
    fn execute(&mut self, actions: &[TrzszEffect]) {
        for eff in actions {
            match eff {
                TrzszEffect::FlushVte(bytes)   => { let _ = self.vte_tx.try_send(bytes.clone()); }
                TrzszEffect::SendStdin(bytes)  => { let _ = self.transport_cmd_tx.try_send(WriteStdin(bytes.clone())); }
                TrzszEffect::OnTrzszRequest{..} => { self.callback.on_trzsz_request(...); }
                // ...
            }
        }
    }
}
```

#### session_event_loop への統合

Kotlin からの trzsz 操作は `SessionCmd` チャンネル経由で受け取る:

```rust
// SshSession が保持する 2 本のチャンネル
struct SshSession {
    handle: Mutex<Option<TransportHandle>>,          // SSH I/O
    session_tx: Mutex<Option<mpsc::Sender<SessionCmd>>>,  // trzsz operations
    ...
}

enum SessionCmd {
    TrzszAcceptUpload { transfer_id: String, file_name: String, file_size: u64, mode: u32 },
    TrzszChunk        { transfer_id: String, data: Vec<u8>, is_last: bool },
    TrzszAcceptDownload { transfer_id: String },
    TrzszCancel       { transfer_id: String },
}
```

```rust
// session_event_loop: tokio::select! で 3 ソースを多重化
loop {
    tokio::select! {
        Some(event) = transport_event_rx.recv() => {
            if let TransportEvent::Stdout(bytes) = event {
                let resp = fsm.on_event(TrzszEvent::StdoutBytes(bytes));
                resp.dispatch(&mut timer_runtime, &mut effect_executor);
            }
            // HostKey / Connected / Disconnected は従来通りコールバック
        }
        Some(timer_id) = timeout_rx.recv() => {
            let resp = fsm.on_timeout(timer_id);
            resp.dispatch(&mut timer_runtime, &mut effect_executor);
        }
        Some(cmd) = session_cmd_rx.recv() => {
            let ev = match cmd {
                SessionCmd::TrzszChunk { transfer_id, data, is_last } =>
                    TrzszEvent::KotlinChunk { transfer_id, data, is_last },
                // ...
            };
            let resp = fsm.on_event(ev);
            resp.dispatch(&mut timer_runtime, &mut effect_executor);
        }
    }
}
```

VTE に流す bytes は `vte_tx` 経由で別チャンネルに入り、
もう一方の `select!` アームで VTE パーサーに渡す。

#### UniFFI に追加するメソッド

```rust
#[uniffi::export]
impl SshSession {
    // trzsz: Kotlin からの操作（session_tx に送る）
    pub fn trzsz_accept_upload(&self, transfer_id: String, file_name: String,
                               file_size: u64, mode: u32) { ... }
    pub fn trzsz_send_chunk(&self, transfer_id: String, data: Vec<u8>,
                            is_last: bool) { ... }
    pub fn trzsz_accept_download(&self, transfer_id: String) { ... }
    pub fn trzsz_cancel(&self, transfer_id: String) { ... }
}
```

#### SessionCallback への追加

```rust
#[uniffi::export(callback_interface)]
pub trait SessionCallback: Send + Sync {
    // 既存
    fn on_data(&self, data: Vec<u8>);
    fn on_host_key(&self, fingerprint: String);
    fn on_connected(&self);
    fn on_disconnected(&self, reason: Option<String>);
    fn on_screen_update(&self, update: ScreenUpdate);
    // 追加
    fn on_trzsz_request(&self, transfer_id: String, mode: String,
                        suggested_name: Option<String>, expected_size: Option<u64>);
    fn on_trzsz_download_chunk(&self, transfer_id: String, data: Vec<u8>, is_last: bool);
    fn on_trzsz_progress(&self, transfer_id: String, transferred: u64, total: Option<u64>);
    fn on_trzsz_finished(&self, transfer_id: String, success: bool, message: Option<String>);
}
```

### Phase 4A-0（再掲）: golden test の記述方針

golden test は `TrzszTransferFsm` を直接呼ぶ:

```rust
let mut fsm = TrzszTransferFsm::new();
let resp = fsm.on_event(TrzszEvent::StdoutBytes(b"hello\r\n".to_vec()));
assert!(resp.actions.iter().all(|a| matches!(a, TrzszEffect::FlushVte(_))));
// timer: なし
assert!(resp.timers.is_empty());

// trigger 検出
let resp = fsm.on_event(TrzszEvent::StdoutBytes(trigger("R")));
let req = resp.actions.iter().find(|a| matches!(a, TrzszEffect::OnTrzszRequest{..}));
assert!(req.is_some());
// timer: Transfer 30s がセットされる
let t = resp.timers.iter().find(|c| matches!(c, TimerCommand::Set { id: TrzszTimer::Transfer, .. }));
assert!(t.is_some());
```

### Phase 4B: trzsz upload MVP

TrzszTransferFsm が稼働した後に実装。
Kotlin: SAF ファイルピッカー → chunk 送信ループ → TrzszTransferSheet UI

### Phase 4C: trzsz download MVP

Kotlin: on_trzsz_download_chunk コールバック → SAF OutputStream

### Phase 4D: 実機回帰

```
□ tmux / htop / alt screen が転送前後で壊れない
□ 100MB+ ファイルで OOM しない
□ 画面回転・バックグラウンド中の転送
□ 転送キャンセル後の端末状態が正常
□ 同一セッションで複数回 trz / tsz を連続実行できる
```

---

## Phase 5: tsshd 接続耐性（Spike → MVP）

> **⚠️ 2026-07-01 追記（Phase 7-1 着手時に判明した実装との乖離）**: 本節は当初「外部の
> trzsz/tsshd（Go 実装）に wire-protocol 互換で接続する」という設計だったが、実際に
> `rust-core/tsshd/` として実装されていたのは **無認証の自作 Rust QUIC↔TCP プロキシ**
> （trzsz/tsshd とは無関係、`ssh_host`/`ssh_port` をクライアントから任意に指定できてしまう
> 試作段階のもの）だった。デプロイ・配布の仕組みも無く、ユーザーが手動でリモートに設置する
> ことが前提のまま止まっていた。Phase 7 で `rust-core/isekai-helper/` にリネームし、
> `HELPER_PROTOCOL.md` の契約（HMAC 認証・target 固定・self-deploy）に沿って作り直した
> ため、**この Phase 5 の tsshd 依存記述は歴史的経緯として残すのみで、今後の実装は
> Phase 7 の isekai-helper を正とする**。Kotlin 側の `useTsshd`/`tsshdHost` 命名も
> Phase 7-4（ActiveSession 統合）で整理する。

### 判断ゲート（Phase 4D 完了後）

```
□ Tailscale 経由の TCP SSH で 5G→WiFi 切替時に本当に切れるか？
□ 切れないなら Phase 5 はスキップ
```

### tsshd とは / android-tssh がやること

```
tsshd-compatible client transport
```

フロー:
1. TCP SSH で接続（RusshTransport）
2. SSH channel で tsshd 起動
3. ServerInfo (JSON) を SSH stdout で受信
4. QUIC で tsshd に接続
5. Bus stream / Session stream でターミナル I/O

### Phase 5A: Spike（4 段）

| # | 内容 |
|---|------|
| 5A-1 | tssh --udp で 5G→WiFi 実測 |
| 5A-2a | Android `Network.bindSocket(FileDescriptor)` + UDP socket を Rust へ渡す |
| 5A-2b | quinn::Endpoint を作れるか |
| 5A-2c | Network 切替時に `quinn::Endpoint::rebind()` できるか |
| 5A-2d | rebind で足りない場合 Client Proxy reconnect へ |
| 5A-3 | tsshd wire protocol を Go ソースから fixture 化 |

### Phase 5B: MVP（Spike 確認後）

- `TsshdTransport` task（`TokioTimerRuntime` を tsshd reconnect にも使う）
- Client Proxy 再接続: Go 実装完全追従、nonce/AAD/seq_no 独自拡張禁止
- プロファイル UI: TCP SSH / tsshd QUIC 選択

---

## Phase 6: SSH3 / Remote Terminal over HTTP/3 — 検討の末に断念（記録として残す）

2026-07-01 に検討したが、以下の理由で採用を見送った。詳細な調査記録は `SSH3_PROTOCOL_NOTES.md`（冒頭に
ABANDONED 表記あり）に残してある。

- IETF draft（`draft-michel-remote-terminal-http3`）は個人提案のまま expired。QUIC WG / HTTPBIS WG の
  採用なし、SSHM WG チャーターはスコープ外と明記。標準化トラジェクトリが無い。
- 実装には `hyperium/h3` の `Protocol` enum への独自パッチが必要、TLS Exporter 要件から quiche は不採用
  （quinn を要採用）など、SSH3 互換だけのために背負う実装コストが大きい。
- 代替候補として調べた `draft-bider-ssh-quic` も、仕様はあるが実装が存在しない（2020年で凍結、唯一の
  プロトタイプ `denisbider/QuiSSH` も初日で放棄）ため採用に値しない。
- 代替候補として調べた `oowl/quicssh-rs` も、SSH プロトコルに関与しない汎用 QUIC↔TCP トンネルで
  ライブラリ組み込み不可・ローミング未解決バグありのため、そのまま採用する価値は無いと判断。
  ただし「QUIC トンネル + 素の sshd へ中継」という発想自体は Phase 5 の再設計（下記）に活かす。

**→ Phase 6 は実施しない。SSH3 対応は行わない。**

---

## Phase 7: 自作ヘルパー方式による QUIC 接続耐性（tsshd 非依存、実験的・Phase 5 と共存）

### 位置づけ

Phase 5（tsshd/QUIC）は、サーバー側に **tsshd（trzsz-ssh daemon）が事前インストールされていること**を前提とし、
その Go 実装のワイヤープロトコルに完全追従する設計だった（互換性ドリフトが恒常的なリスクとして残る）。

Phase 7 は、tsshd への依存を断ち切り、**自分たちで書いた最小限の Rust ヘルパーバイナリ**をサーバー側に配置して
同様のローミング耐性を得る方式。ワイヤープロトコルは自分たちで定義するため、外部実装との互換性ドリフトの
心配が無くなる。**Phase 5 を置き換えるのではなく、選択肢として共存させる**（自作ヘルパーが使えない環境
— 例えば `noexec` マウントやセキュリティポリシーが厳しいサーバー — では Phase 5 / 通常 TCP SSH にフォールバック
できる方が堅牢なため）。

### ヘルパーの役割（スコープを小さく保つ）

自作ヘルパーは **賢いことをしない**。正確には「**1 QUIC connection の 1 bidirectional stream を受け取り、
`127.0.0.1:22`（素の、素のままの `sshd`）へ TCP 接続して双方向コピーする**」という stream 単位の薄いプロキシに
徹する（QUIC は単なるバイトストリームではなく stream/datagram/connection の層を持つため、「バイトを中継する」
という曖昧な表現ではなく stream 単位の対応であることを明記する）。SSH の認証・チャネル・PTY 制御は今まで通り
russh がクライアント側で処理する（`run_ssh_channel_loop<T: AsyncRead + AsyncWrite>` は既にトランスポート
非依存な設計になっているため、「TCP の代わりに QUIC 経由でこのヘルパーに繋ぐ」という差し替えだけで済む）。

将来的にポートフォワードや複数チャネルに拡張する場合は、1 QUIC connection に複数の bidirectional stream を
多重化し、各 stream を別々の宛先（`127.0.0.1:22` 以外のポートや control channel）にマッピングする形に
拡張できる。ただし Phase 7 のスコープでは単一 stream のみを実装対象とする。

これにより、tsshd のような「SSH channel 多重化・独自暗号ヘッダ」を自作する必要が無くなり、実装量を最小限に
抑えられる（quicssh-rs が実際にやっていることとほぼ同じだが、ライブラリとして自分たちの Rust コアに直接組み込む）。

### QUIC コネクション維持（ローミング対応）— この設計の核心

**このヘルパーが解決すべき本質的な課題は「クライアントの IP/ポートが変わっても QUIC コネクションを
維持し続けること」であり、tsshd 互換にこだわらなかった最大の理由もここにある。**

- 一次的な機構は **QUIC 自体が持つ Connection Migration（RFC 9000 §9）** に任せる。QUIC は
  「4-tuple（送信元IP・ポート・宛先IP・ポート）」ではなく **Connection ID** でセッションを識別するため、
  Wi-Fi⇔5G 切替でクライアントの IP/ポートが変わっても、**同一の QUIC コネクションのまま**新しい経路
  （path）に自動追従できる。これは tsshd の Go 実装が採用した「独自の Client Proxy 再接続プロトコル
  （nonce/AAD/seq_no を手動管理）」より遥かにシンプルで、しかも QUIC 標準機能なので外部実装への
  追従リスクが無い。
- **クライアント側の要件**: Android の `ConnectivityManager` がネットワーク切替を通知した際、
  `Network.bindSocket(FileDescriptor)` で新しいアクティブネットワークに束縛した UDP ソケットを用意し、
  `quinn::Endpoint::rebind()` を呼ぶ（Phase 5A-2a/2b/2c で既にスパイク済みの手法をそのまま転用できる）。
- **サーバー側（ヘルパー）の要件**: quinn のサーバーエンドポイントが、既に検証済みの Connection ID からの
  path migration（新しい 4-tuple での `PATH_CHALLENGE`/`PATH_RESPONSE`）を正しく受理できることを
  Phase 7-1 のスパイクで確認する（quinn はデフォルトで RFC 9000 準拠の migration をサポートするはずだが、
  明示的な受け入れ確認をテスト項目に含める）。
- **決定的な利点**: ヘルパーが `127.0.0.1:22` の `sshd` と結ぶ TCP コネクションは、クライアントの実ネットワーク
  経路とは完全に切り離された別物であり、クライアントがどれだけ IP/ポートを変えても **一度も再確立されない**。
  つまり `sshd` から見れば、SSH セッションは何の乱れも無く継続し続ける（TCP 接続そのものがローミングを
  意識する必要が無い）。
- **フォールバック（QUIC コネクション自体が失われた場合）**: アイドルタイムアウト超過や migration の
  path validation 失敗など、QUIC connection migration だけでは救えないケース（回線が長時間完全に
  切断された場合など）に備え、直前に発行された認証トークンを使って**新しい QUIC コネクションから
  同じヘルパーセッションに再接続できる**軽量な再接続手順を用意する（SSH 再接続からやり直すより高速）。

### 配置場所（`/tmp` は不採用）

`/tmp` は以下の理由で避ける:
- `noexec` でマウントされているサーバーがあり、実行できないケースがある
- 世界書き込み可能な共有ディレクトリで、多人数利用サーバーでは事故・監査ログ上の見た目が悪い
- 再起動や tmpfs クリアで消える前提を明示的に設計に組み込む必要がある

**→ `~/.local/bin/isekai-helper`（XDG Base Directory 準拠、ユーザーのホーム配下）を既定の設置場所とする。**
ホームディレクトリはほぼ確実に `noexec` ではなく、ユーザーごとに独立し、既存の `$PATH` 慣習（`~/.local/bin` を
`$PATH` に含める設定は一般的）とも整合する。

### 配布方法（複数を用意し、上から順に試す）

```
1. 既存インストール確認: `command -v isekai-helper` または `~/.local/bin/isekai-helper` の存在確認
   → 見つかればそれを使う（バージョン確認して不一致なら再配布を検討）
2. Linuxbrew（linuxbrew/homebrew-core 相当の自前 tap）: `brew install isekai-terminal/tap/isekai-helper`
   → ユーザーが既に linuxbrew を使っている環境では最も摩擦が少ない
3. ブートストラップ自動配布（SSH 経由）:
   a. `uname -m` でアーキテクチャ検出（x86_64 / aarch64 を優先対応）
   b. APK に同梱した対応バイナリ（static musl build）を SSH exec チャネル経由で転送
      （russh の SFTP subsystem が使えるならそちら、無ければ base64 + exec 'cat > file' でも可）
   c. `~/.local/bin/` を作成（無ければ）→ 書き込み → `chmod +x`
   d. 起動し、標準出力から一時ポート番号 + 認証トークンを受け取る
4. 上記すべて失敗した場合: ユーザーに「手動インストールが必要」であることを明示するエラーメッセージを出し、
   手動インストール手順（バイナリ配布ページ）を案内する。通常 SSH（Phase 1-4）へは自動フォールバックする。
```

### セキュリティ

自作ヘルパーの脅威モデルは「個人が自分のサーバーに繋ぐ」スコープに限定し、エンタープライズ的な
ゼロトラストポリシー（GeoIP/ASN ベースの再認証、複雑なローミング状態機械など）は過剰と判断し導入しない。
一方で、以下は実際に効く対策として採用する。

- **証明書ピン留めは SSH 経由で行う**: ヘルパーの自己署名証明書の fingerprint は、QUIC 接続時の
  素の TOFU ではなく、**既に認証済みの SSH チャネル経由**でクライアントに渡す。SSH 接続自体が
  MITM されていない前提が既に成立しているため、通常の「初回接続のみを信頼する」TOFU より強い
  信頼の起点になる（`KnownHost` の仕組みを拡張して転用）。
- **トークンは「送る」のではなく「証明する」**: SSH stdout 経由でクライアントに渡すのは生の
  トークンではなく `session_secret`（ヘルパー起動ごとにランダム生成）。QUIC 接続確立後、
  クライアントは `proof = HMAC(session_secret, quic接続のexport_keying_material)` を送り、
  ヘルパー側で同じ計算をして一致を確認してから初めて TCP 中継を開始する。
  - この設計により、経路上の盗聴者が `proof` を観測できても、それは **その1つの QUIC コネクションにしか
    有効でない**ため、別コネクションへのリプレイができない（SSH3 の JWT `jti` を TLS Exporter に
    束縛する発想と同じ。Phase 6 の調査で確認済みの quinn `export_keying_material()` をそのまま使う）。
  - `session_secret` 自体は物理的な経路（SSH チャネル）を一度も QUIC 上に流さないため、QUIC 側の
    盗聴だけでは `session_secret` を割り出せない。
- **0-RTT 禁止**: `proof` を含む最初のメッセージは 0-RTT（early data）では送らない。1-RTT
  ハンドシェイク完了後にのみ送信・検証する（0-RTT のリプレイ耐性の弱さを回避）。
- **同時アクティブ接続は1本まで**: 同じ `session_secret` に由来する有効な `proof` を持つ新しい
  QUIC 接続が来ても、既存の接続がまだ生きていれば **新規接続を拒否**する（デフォルトは「奪取」ではなく
  「拒否」。正規ユーザーが誤って蹴られる事故より、疑わしい二重接続を弾く方を優先する）。
- **QUIC の path validation は必須のまま**（既存リスク表参照）。ローミング自体への追加の再認証は
  行わない（脅威モデル上、そこまでのリスクは想定しない）。
  - ただし **RFC 9000 §9.3.2（On-Path Address Spoofing）が明記する通り、path validation は
    on-path（既に経路上にいる）攻撃者には効かない**。path validation が証明するのは「その IP に
    パケットが届くこと」だけで、「その経路が信頼できること」ではない。上記の `proof = HMAC(session_secret,
    export_keying_material)` による認証は path validation とは独立した防御であり、on-path 攻撃者が
    正規パケットを忠実に中継して path validation を通過させたとしても、`session_secret` を知らない限り
    有効な `proof` を計算できないため、乗っ取りを防げる。この二層構造（QUIC 標準の path validation ＝
    可用性・整合性、`proof` ＝ アプリ層認可）を明示しておく。
  - サーバー側の `preferred_address`（RFC 9000 §9.6）は**使用しない**。preferred_address を悪用した
    データ持ち出し攻撃（QUIC-Exfil、arXiv:2505.05292、ACM ASIA CCS '25）が実際に報告されており、
    論文自身が「preferred_address の無効化」を対策として挙げている。ヘルパーの quinn サーバー設定で
    明示的に無効化し、意図的な設計判断として記録しておく。
  - PATH_CHALLENGE / NEW_CONNECTION_ID フラッディングによる off-path DoS（メモリ枯渇）が quinn の
    実装依存で起こり得ることが知られている（Marten Seemann の実地報告、2023/2024）。quic-go は
    RFC を意図的に逸脱してキューに上限を設けることで対処した実績がある。Phase 7-1 で quinn の
    デフォルト挙動を確認し、上限が無い場合は自前でレート制限を追加する。
- **セッション終了時の無効化**: ヘルパープロセス終了・SSH 再ブートストラップのたびに `session_secret`
  は使い捨てにし、古いものは再利用不可にする。
- **マイグレーション時に `proof` を再送する必要は無い（実験で確認済み）**: `/home/cuzic/ssh3/rust-quinn-spike/src/bin/migration_exporter_test.rs`
  で、`Endpoint::rebind()` によりクライアントのローカルポートを変更（ローミングを模擬）した前後で
  `export_keying_material()` の出力を比較したところ、**完全に同一の値**になることを実地で確認した。
  - 理由: exporter は TLS ハンドシェイクの `exporter_master_secret` のみに依存し、経路や Connection ID には
    依存しない。したがって同じ `proof = HMAC(session_secret, exporter)` を migration 後に再送しても、
    盗聴者にとって既知の値を繰り返すだけで新しい証拠にならず、追加の防御にはならない。
  - より本質的な理由として、PATH_CHALLENGE/PATH_RESPONSE は暗号化された QUIC フレームとして交換されるため、
    これに正しく応答できる時点でその相手は**既にそのコネクションの暗号鍵を保持している**ことが保証される。
    つまり「migration が成立した」こと自体が「既に接続の当事者である」ことの証明であり、そこに
    application 層の再認証を追加で挟んでも防げる攻撃はもう残っていない。
  - **`proof` の再計算が必要なのは migration ではなく「reconnect」(QUIC コネクション自体が失われ、新しい
    TLS ハンドシェイクで張り直す)の場合のみ**。この場合は新しい `exporter_master_secret` が生成されるため
    `proof` も新しい値になり、既存設計（コネクションごとに `proof` を計算）で自然にカバーされている。
    → migration と reconnect を明確に区別し、reconnect の場合のみ `proof` 再検証を必須とする。
- APK に同梱するヘルパーバイナリはビルド時にチェックサムを記録し、供給チェーンの整合性を確認できるようにする。

### フェーズ分割（レビューを受けて並び替え・詳細化）

外部レビューで「実装より先に CLI/プロトコル契約を固めるべき」「Linuxbrew tap は本筋ではなく優先度を下げるべき」
という指摘を受け、以下の順序に変更した。

| # | 内容 | 成果物 |
|---|------|--------|
| 7-0 | helper の CLI/プロトコル契約を確定（`--target`/`--bind`/`--idle-timeout`/`--max-idle-lifetime`/`--once`/`--log-level`/`--version`、終了コード仕様、起動ハンドシェイク JSON（ファイル権限・flush 契約含む）、HELLO/ACK/REJECT フレーム契約、0-RTT 両側無効化、非ゴールの明記） | ✅ `HELPER_PROTOCOL.md` に確定 |
| 7-1 | helper 最小実装（quinn サーバー、stream 単位の双方向コピー・half-close・timeout・backpressure対応、`proof = HMAC(...)` トークン認証、preferred_address 無効化）+ QUIC connection migration の動作確認 | ✅ `rust-core/isekai-helper/` に実装済み。E2E テスト3件（HELLO/ACK/relay、REJECT_AUTH、REJECT_DUPLICATE）が `cargo test -p isekai-helper` で通過。旧 `rust-core/tsshd/`（無認証の試作、Go実装とは無関係）を isekai-helper にリネーム・作り直した |
| 7-2 | x86_64 / aarch64 musl クロスビルド確認（cargo-ndk 相当の cross ビルド設定）+ `uname -m` → バイナリ選択マッピング + sha256 記録 | ✅ `cargo-zigbuild`（zig を C クロスコンパイラ/リンカに利用、musl-gcc 等のシステムトゥールチェーン不要）で両アーキテクチャの静的バイナリを生成・動作確認済み。`rust-core/scripts/build-isekai-helper-musl.sh` に手順化 |
| 7-3 | SSH 経由のブートストラップ配布ロジック（既存確認→バージョン確認→再利用/置換→転送→チェックサム検証→起動→ポート競合処理→起動確認→失敗時フォールバック） | ✅ `rust-core/src/helper_bootstrap.rs` に実装済み。実 sshd に対する E2E テストで動作確認済み（`HELPER_BOOTSTRAP_TEST_KEY` 環境変数で opt-in、未設定なら自動スキップ）。**重要な実機検証結果**: `cmd & disown` は `setsid` との組み合わせでシェルが子プロセスの終了待ちでハングする不具合があり、`( setsid cmd & )` というサブシェル二重 fork に変更して解消した（詳細は HELPER_PROTOCOL.md 参照） |
| 7-4 | `TransportPreference`（`PlainSsh`/`TsshdQuic`/`IsekaiHelperQuic`/`Auto`）を設計した上で `ActiveSession` へ統合（Phase 5 の `QuicSession` とは責務分離し、並列の `HelperQuicSession` を追加） | ✅ `rust-core/src/helper_quic_transport.rs` + `orchestrator.rs` の `ActiveSession::HelperQuic`/`connect_helper_quic`/`connect_helper_quic_auto` に実装済み。**実 sshd に対するフルスタック E2E テストで、SSH bootstrap → isekai-helper 起動 → QUIC 接続（証明書ピン留め + HMAC 認証）→ russh セッション確立 → 実シェルコマンド実行・出力受信までの全チェーンを確認済み**（`cargo test -p tssh-core --lib helper_quic_transport`）。Kotlin 側 UI（ProfileEditScreen 等への `TransportPreference` 選択肢追加）は未着手、別途フォローアップとする |
| 7-5 | 実機ローミング耐性検証（Wi-Fi⇔5G 切替に加え、alt screen 表示中・大量出力中・入力中・30分アイドル後・画面ロック復帰・helper↔sshd 切断・token 不一致拒否・trzsz 転送中の切替を含む拡充版） | ✅ 自動テスト: `rust-core/src/faulty_udp_socket.rs`（UDP データグラム層でのロス/遅延/完全断シミュレーション + `Endpoint::rebind()` によるネットワーク切替再現）。実機回帰チェックリストは下記「実機検証手順」節 |
| 7-6 | Linuxbrew tap 作成（`cuzic/homebrew-isekai-terminal`、当初案の `isekai-terminal/homebrew-tap` は該当 GitHub org が存在しないため実際のリポジトリ所有者 `cuzic` に合わせて命名変更）— 優先度低、手動インストールしたい上級者向け fallback | ✅ GitHub Release `isekai-helper-v0.2.0`（`cuzic/isekai-terminal`、musl バイナリ x86_64/aarch64 + sha256 添付）と tap リポジトリ `cuzic/homebrew-isekai-terminal`（`Formula/isekai-helper.rb`）を公開済み。`brew tap cuzic/isekai-terminal && brew install isekai-helper` を実際の GitHub 経由で実行し、`isekai-helper --version` の動作まで確認済み。**教訓**: Formula に埋め込む sha256 は「ビルド直後に一度だけ記録した値」を使い回さず、**実際にアップロードした Release アセットを都度ダウンロードして再計算した値**を使うこと（今回、workspace 内の別クレートを再ビルドした影響と見られる非決定的な再リンクで isekai-helper のバイナリが後から変わり、最初に記録した sha256 と食い違って `brew install` が checksum mismatch で失敗する事故が実際に発生した） |
| 7-7 | 「複数経路を同時に温めておき、OS のデフォルトルート変更を待たずに即切替する」設計調査（`noq` 評価・実機マルチパス検証） | ✅ 詳細は下記「### Phase 7-7 詳細」参照。**結論: Tailscale⇔直接アドレスのmultipathは実装コストゼロで実機動作確認済み、正式機能候補。Wi-Fi⇔セルラーの物理同時マルチパスはTailscale稼働中は原理的に不可能と判明したが、Tailscale OFF＋dev boxをIPv6デュアルスタック化した状態では`dualFdMultipath_wifiPlusCellular`が実機で成功（3回中2回、failover含む）。Tailscale併用が前提の現ユーザー像とは相性が悪く優先度は低いまま実験機能扱いとするが、技術的な実現可能性そのものは実証済み** |

対象外: マルチユーザーサーバーでの他ユーザーとの共存考慮の深掘り、Windows/macOS サーバー対応（Linux 前提）、
自動アップデート機構。

**アーキテクチャ上の前提の明確化（レビュー指摘）**: この方式で守られるのは「Android app ↔ helper」間の QUIC
コネクションであり、helper と sshd の間は普通の TCP/SSH セッションである。

```
Android app
  └─ QUIC（migration で経路変更に追従）
      └─ isekai-helper
          └─ TCP 127.0.0.1:22（ここはローミングを意識しない、ずっと同じソケット）
              └─ sshd
```

QUIC connection migration が成功する限り内側の SSH セッションは維持されるが、**QUIC 接続自体が完全に切れて
新規接続（reconnect）になった場合、内側の TCP/SSH セッションも死ぬ**。mosh のような「端末状態同期による
シームレスな再開」ではない点を期待値として明確にしておく。

### リスク

| リスク | 対策 |
|--------|------|
| `~/.local/bin` が `$PATH` に無いサーバーがある | ヘルパー起動はフルパス指定で行うため `$PATH` 依存にしない。`command -v` チェックもフルパス優先 |
| サーバーの CPU アーキテクチャが x86_64/aarch64 以外 | 検出できないアーキテクチャの場合は Phase 5 / 通常 SSH にフォールバック |
| ヘルパープロセスがサーバー再起動・OOM killで消える | 接続の都度「起動確認」を行うロジックを必須にする（tsshd の `ensureServiceStarted()` と同じパターンを踏襲） |
| 任意バイナリの自動転送・実行に対するユーザーの心理的抵抗・セキュリティ方針違反 | ブートストラップは opt-in（プロファイル設定で明示的に有効化）とし、既定は無効。Linuxbrew/手動インストールを優先案内する選択肢も残す |
| バイナリ供給チェーンの改ざんリスク | ビルド時チェックサムを記録し、将来的に署名検証を追加検討 |
| キャリア/企業 NAT・ファイアウォールが QUIC の path migration を弾く（新しい 4-tuple からのパケットを別コネクション扱いする等） | Phase 7-5 で実機の Wi-Fi⇔5G 切替時に実際に migration が成立するか検証。失敗する場合はトークンベースの再接続（フォールバック）に切り替える |
| QUIC アイドルタイムアウト超過で完全にコネクションが失われる（migration では救えない） | 認証トークンを使った軽量な再接続手順を用意し、SSH からのフルリブートストラップより高速な復帰経路を確保する |
| on-path 攻撃者は path validation を通過できてしまう（RFC 9000 §9.3.2、path validation は経路の到達性を証明するだけで信頼性は証明しない） | `proof = HMAC(session_secret, export_keying_material)` によるアプリ層認証を path validation とは独立した防御として設計済み（session_secret を知らない攻撃者は正規パケット中継だけでは乗っ取れない） |
| サーバー側 preferred_address を悪用したデータ持ち出し（QUIC-Exfil、arXiv:2505.05292） | ヘルパーの quinn サーバー設定で preferred_address を明示的に無効化する（そもそも使わない設計） |
| PATH_CHALLENGE / NEW_CONNECTION_ID フラッディングによる off-path DoS（quinn の実装依存、quic-go は RFC 逸脱でキュー上限を設けて対処した前例あり） | Phase 7-1 で quinn のデフォルト挙動を確認し、上限が無ければ自前でレート制限を追加する |
| `cmd & disown`（引数無し・`disown -a` いずれも）が `setsid` との組み合わせで実機ハングする（bash が長時間稼働する子プロセスの終了を暗黙に待ち続ける、実機検証で確認済み） | `( setsid cmd & )` というサブシェル二重 fork に変更して解消（`helper_bootstrap.rs`/`HELPER_PROTOCOL.md` に反映済み）。同様のシェルスクリプトを今後変更する際はこの実測結果を再確認すること |

### 事前検証済み: `faulty_udp_socket.rs` による自動テスト（実機不要）

`faulty_stream.rs`（Phase 5〜7 で使ってきたアプリ層バイトストリームのフォルト注入）は QUIC ストリーム
確立**後**のバイトに遅延を足すだけで、quinn のパス検証・マイグレーション判定には一切影響しない。
そこで `rust-core/src/faulty_udp_socket.rs` を新設し、`quinn::AsyncUdpSocket` を UDP データグラム単位で
ラップしてロス・遅延・完全断（電波圏外相当）を注入できるようにした。さらに quinn 自身のテストスイート
（`quinn/src/tests.rs: rebind_recv`）と同じ手法である `Endpoint::rebind_abstract()` を組み合わせ、
ローカル環境だけで「劣化した状態でネットワークを切り替える」を再現・自動テスト化した
（`cargo test -p tssh-core faulty_udp_socket`、3 テストとも pass 済み）。

- `connects_and_exchanges_data_under_loss_and_latency`: ロス20%・遅延20msの下でも QUIC ハンドシェイク
  と双方向通信が成立することを確認。
- `rebind_to_new_faulty_socket_survives_as_network_switch`: 「ネットワーク A」で疎通後、劣化度合いの
  異なる「ネットワーク B」（別ローカルソケット）へ `rebind_abstract()` で切り替え、切り替え後も同一
  コネクションとして通信が継続し、`export_keying_material()` の出力が切替前後で不変（＝再接続ではなく
  マイグレーションである）ことを確認。
- `cut_causes_connection_to_stall_then_recover_after_restore`: 完全断（電波圏外相当）の間は応答が来ず、
  `restore()` 後に自動復旧することを確認。

この自動テストにより「QUIC migration の仕組みそのものがロス・遅延下でも機能する」という土台は実機
無しで検証済み。ただし以下は自動テストでは代替できず、実機での確認が必須:

- 実際のキャリア網 / Tailscale の NAT・ファイアウォールが新しい 4-tuple からのパケットを別コネクション
  として弾かないか（上記リスク表参照）。
- Android の `ConnectivityManager` がネットワーク切替をどう通知し、アプリ側がそれにどう反応するか
  （現状ソケットは OS のデフォルトルーティングに任せているのみで、明示的な `rebind` 呼び出しは行って
  いない。実機で自然に発生する切替を確認する）。
- 実際の Wi-Fi 圏外・5G ハンドオーバー・Tailscale のリレー経由切替に特有のタイミング・パケットロス
  パターン。

### 実機ライブフォルト注入: `debug_fault.rs`

上記の自動テストはローカル完結だが、実機でも「劣化させながらネットワークを切り替える」を再現したい
という要望を受け、`faulty_udp_socket.rs` を `helper_quic_transport.rs` の QUIC クライアントソケット
構築（`connect_helper_quic_stream` 内）に実際に配線した。既定値（遅延0・ロス0・cut無し）では素通しの
ラッパーとして動作するため、通常利用時の挙動には一切影響しない。

- `rust-core/src/debug_fault.rs`: `UdpFaultInjector` をプロセス内 `OnceLock` で共有し、
  `debug_set_udp_fault_latency_ms` / `debug_set_udp_fault_loss_permille` / `debug_cut_udp_fault` /
  `debug_restore_udp_fault` / `debug_clear_udp_fault` を `#[uniffi::export]` で公開。
- `app/src/debug/kotlin/tools/isekai/terminal/debug/FaultInjectionReceiver.kt`: 上記関数を
  `adb shell am broadcast` から呼び出せる `BroadcastReceiver`。`app/src/debug` ソースセット配下の
  ため **release ビルドには一切含まれない**（Kotlin コード自体が存在しない。Rust 側の `debug_fault`
  関数は release cdylib にも含まれるが、これらの関数を呼ぶコードパスが release ビルドに存在しないため
  到達不能＝実質無効）。
- `rust-core/scripts/phase7-5-roaming-test.sh`: 上記を使い、(A) ライブフォルト注入のみ、(B) 実ネット
  ワーク切替のみ、(C) 劣化させながら切替、の3系統のシナリオ関数を用意したスクリプト。実機は複数の
  Claude セッションで共用しているため、各シナリオ関数を呼ぶ前に毎回ユーザーに確認を取ってから実行する
  運用とする（スクリプト自体は確認なしに一括実行するものではない）。

### 実機検証手順（Phase 7-5）

前提: Tailscale・Wi-Fi・5G の3経路にアクセス可能な実機、`TransportPreference.ISEKAI_HELPER_QUIC`
または `AUTO` を設定したプロファイル。`rust-core/scripts/phase7-5-roaming-test.sh` の `list_scenarios`
に沿って進める。

1. Wi-Fi のみを有効にし、プロファイルへ接続。`isekai-helper` が自動配布・起動され、シェルが使える
   ことを確認する（`connect()` 経由、フォールバック無し設定で接続失敗しないことも兼ねて確認）。
2. 接続を維持したまま `htop` や `yes` など出力し続けるコマンドを実行し、以下の切替パターンをそれぞれ
   試す。切替後 数秒〜十数秒 以内にシェルの応答が復旧すればマイグレーション成功、復旧せず入力も出力も
   止まったままなら失敗（Phase 8 のフルリブートストラップ対象）:
   - Wi-Fi → モバイルデータ（5G）: Wi-Fi をオフ
   - モバイルデータ（5G）→ Wi-Fi: Wi-Fi をオン（自動的に優先経路が切り替わる）
   - Wi-Fi → Tailscale 経由（別ネットワークの Tailscale ノード経由でサーバーに到達する構成にしてから
     Wi-Fi を切替。または Tailscale の relay/direct 切替が発生するタイミングを狙う）
   - 画面ロック → 数分放置 → ロック解除（Doze/App Standby でソケットが殺されないか）
3. 拡充シナリオ（PLAN.md 既存のフェーズ分割表に記載の項目）も同様に切替中に発火させる:
   - alt screen 表示中（`vim`/`htop` 実行中）に切替
   - 大量出力中（`yes` や `find /`）に切替
   - 入力中（キー入力の合間）に切替
   - 30分アイドル後に切替（QUIC keep-alive/idle timeout の境界確認）
   - trzsz 転送中（大きめのファイルを download/upload 中）に切替
4. 意図的な失敗系も確認する:
   - helper プロセスを手動で `kill` してから操作 → エラー表示とフォールバック（`AUTO` なら plain SSH
     へのフォールバック、`ISEKAI_HELPER_QUIC` 単独指定ならエラー表示のみ）を確認
   - session_secret を握っていない別クライアントから同じポートへ接続を試みて `REJECT_AUTH` になる
     ことを確認（意図的な誤り設定で確認可）
5. 各シナリオの結果（成功/失敗、復旧までの体感秒数、ログの異常有無）を記録し、失敗パターンは
   上記リスク表・Phase 8 の設計判断（resume プロトコルが本当に必要か）にフィードバックする。

### 実機検証結果（2026-07-02、Xperia XQ-DQ44 / Android 15）

対象プロファイル: 「Tailscale経由」（`ISEKAI_HELPER_QUIC`、SSH サーバーは Tailscale 越しに到達）。
adb は USB 経由で Windows PC に接続し、`clipwire-exec` で一度 `adb -a start-server` を実行して
adb サーバーを Tailscale 越しに公開（`ADB_SERVER_SOCKET=tcp:<windows-tailscale-ip>:5037`）、そこから
直接操作した。**この経路は USB ベースなのでスマホ自身の WiFi/モバイル切替の影響を受けず、安定して
実機を監視・操作できた**（当初 Linux サンドボックスから直接 `adb connect <phone-tailscale-ip>` して
いたが、これは TCP 接続がスマホの WiFi 切断で切れてしまい使えなかった）。

| シナリオ | 結果 | 備考 |
|---|---|---|
| Wi-Fi → モバイルデータ | ✓ 即座に継続 | `date` 等の応答遅延を体感しないレベル |
| モバイルデータ → Wi-Fi | ✓ 即座に継続 | 同上 |
| 劣化注入（遅延300ms+ロス20%）下での新規接続 | ✓ 成功（約2分半） | SSH は鍵交換・認証・チャネル開設・PTY要求と往復が多く、20%ロスだと各往復で PTO 再送が発生し累積して遅くなる。「繋がらない」のではなく「正しいが極めて遅い」という結果 |
| 劣化注入 + 実際のネットワーク瞬断（約26秒、検証者側で意図せず発生） | ✓ 自己回復（数分後） | 人工的な劣化と実障害が重なり詰まったように見えたが、フォルト解除後に滞留していたデータが一気に流れて復旧。**再接続は不要だった** |
| 完全断（20秒、電波圏外相当）→ 復旧 | ✓ 224ms で復旧 | `debug_fault` の `CUT`/`RESTORE` を使用。再接続不要、キーストロークからの応答が正常なレイテンシに戻った |
| 直接到達（Tailscale 非経由、開発機のグローバルIP宛） | ✗ QUIC ハンドシェイクが30秒でタイムアウト | SSH(TCP:22) は疎通するが `isekai-helper` の動的UDPポートへの着信が届かない。`isekai-helper` 自体は正常に起動・listen していることを `ss -ulnp` で確認済み。クラウド/ルーターのファイアウォールが SSH 以外のポートを塞いでいる可能性が高い。**開発機のインフラ側の課題であり、アプリ側のバグではない**。恒久対応として `--bind` で固定ポートを指定できるようにし、そのポートだけ開けてもらう運用が現実的（`helper_bootstrap.rs` の起動コマンドに `--bind` を渡す変更が必要、未実装） |

**わかったこと・今後への示唆:**
- QUIC migration の仕組み自体（WiFi⇔モバイル切替）は実機で問題なく機能する。ただし今回は
  Tailscale 経由の接続だったため、切替の吸収を Tailscale 自身（WireGuard レイヤー）が行っている
  可能性が高く、`helper_quic_transport.rs` 自身のマイグレーションコードが実際に発火したかは
  今回のログからは断定できていない（`faulty_udp_socket.rs` の自動テストでは `rebind_abstract()`
  により自作コードのマイグレーション発火を直接確認済みなので、機構自体は健全）。直接到達
  経路のファイアウォール問題を解消できれば、この切り分けが実機でも可能になる。
- 遅延・ロスが重なった状態での新規接続や実障害からの回復は「遅いが最終的に成功する」という
  望ましい特性を持つことを確認できた。ユーザー体験としては「繋がらないように見えて実は詰まって
  いるだけ」というケースがあり得るため、将来的には UI 側で「再送中」等のフィードバックを出す
  ことを検討する価値がある（今回はスコープ外）。
- `adb shell am broadcast` は Android 8+ の implicit broadcast 制限で action 指定だけでは
  manifest 登録レシーバーに届かないことがある。`-n <pkg>/.ClassName` で明示的にコンポーネントを
  指定する必要がある（`FaultInjectionReceiver.kt` のコメント・`phase7-5-roaming-test.sh` に反映済み）。

### Phase 7-7 詳細: 複数経路の能動的マルチパス調査（2026-07-02）

「OS のデフォルトルート変更を待たず、複数経路を先に確保・生存確認しておいて即切替したい」という
要望を受けて調査した記録。quinn には現時点でマルチパス QUIC（draft-ietf-quic-multipath）の実装が無い
ため、n0-computer が quinn からフォークして開発している **`noq`**（crates.io、v1.0.1、2026-06-29
リリース、MIT OR Apache-2.0、iroh の高レベル P2P スタック非依存でスタンドアロン利用可能）を評価した。

#### 実機で確認できたこと

1. **Tailscale 経由アドレス ⇔ 直接到達可能な公開アドレス**: 同一 QUIC コネクション内に
   `noq::Connection::open_path()` で 2 本目のパスを開き、両方とも実機（Android 15）で安定して確立・
   双方向通信・failover（片方を close してももう片方だけで通信継続）まで確認できた。**追加実装は不要**
   （tun0 と wlan0 はどちらも常時ルートが張られているため、単純な送信元 IP 切替だけで正しく振り分けら
   れる）。US 東部・GCP 東京リージョンの両方でサーバーを立てて検証し、地理的な差は無いことも確認済み。
2. **Wi-Fi ⇔ セルラーの物理 2 無線同時利用**: `android.net.Network.bindSocket()`（正規の経路選択 API）
   は、Tailscale（VPN）稼働中はこの UID が VPN ロック対象になり `EPERM` で拒否される（netd の fwmark
   サーバーがVPN対象UIDのソケットの物理ネットワークへの直接bindを拒否する仕様。俗称 "Tiny UDP Cannon"
   としてGoogleにも報告済み・"Won't Fix (Infeasible)"）。
   - **回避策として試した「生ローカルIPへの`bind()`」（`Network.bindSocket()`を経由しない、plain kernel
     `bind()`）は`EPERM`は回避できたが、実際には機能しなかった。** `/proc/net/dev`のインターフェース別
     TX/RXカウンタで検証したところ、「セルラー向け」にbindしたはずの通信は実際には一切セルラー無線を
     通っておらず、全てWi-Fi経由で送信されていた（Androidの経路選択はUID/fwmarkベースのポリシー
     ルーティングであり、ソケットの送信元IPをどう指定しても、現在のデフォルトネットワーク（Wi-Fi）
     経由で出て行ってしまう）。
   - この事実が判明する前は「セルラー経由の復路だけ届かない」という非対称な失敗が観測され、一時
     キャリアNAT（SoftBank LTE）側の問題を疑ったが、上記の通り**そもそもセルラーを使っていなかった**
     ことが原因であり、キャリア側の問題ではなかった（US/東京リージョン比較、ポート443へのリダイレクト、
     生UDPでのサイズ別echoテストなど複数の切り分けを行ったが、いずれも「セルラー不使用」という一点で
     説明がつく）。
   - 結論: **Tailscale 稼働中は、Android の正規 API を使っても回避策を使っても、アプリから明示的に
     セルラーを選択することはできない。** これは Android のネットワーク分離の意図した挙動であり、
     root 化しても（`Network.bindSocket()` 自体は root 不要で動くはずの API のため）解決しない制約。
   - **2026-07-02 追記（Tailscale 無効化後の再検証、実施済み）**: USB 接続の adb（Windows PC 経由、
     Windows 自身の Tailscale で中継）はスマホ側の Tailscale 状態と独立しているため、`adb shell am
     force-stop com.tailscale.ipn` でスマホの VPN（`tun0`）だけを落とした状態でも adb 操作を継続できた。
     この状態で `NoqDualFdMultipathSpikeTest#cellularBindSocket_udpEcho`（`cellularNetwork.bindSocket()`
     で bind した UDP ソケットで dev box に echo）を実行したところ **成功**（`EPERM` は発生せず）。
     さらに `/proc/net/dev` の `rmnet_data2`/`wlan0` カウンタを実行前後で比較したところ、
     `rmnet_data2` だけが送受信ともにちょうど 1 パケット（66 バイト）増加し、`wlan0` 側の増分は
     テストと無関係なバックグラウンド通信のみだった。**つまり以前の生 `bind()` ワークアラウンドとは
     異なり、`Network.bindSocket()` は本当にセルラー無線を経由していることを実測で確認できた。**
     Tailscale OFF なら `Network.bindSocket()` は仕様通り機能する、という上記の推測が実証された。
   - **同時に判明した制約**: 実機の Wi-Fi（`wlan0`）が検証時点で IPv4 アドレスを持たず（IPv6 のみ、
     ルーター/DHCP 側の事情）、dev box が IPv4 専用のため `dualFdMultipath_wifiPlusCellular`（noq
     経由で Wi-Fi fd + Cellular fd を同時に張る本命テスト）は `no IPv4 address on <network>` で失敗した。
     これは noq/bindSocket 自体の問題ではなく、検証環境（Wi-Fi の IPv4 未払い出し）に起因する。
   - **2026-07-02 追記（本命テスト完走、実機で成功）**: dev box（本リポジトリの作業環境そのもの、
     公開 IPv4 `204.12.203.210`）に Hurricane Electric Tunnelbroker（`tunnelbroker.net`）で 6in4
     トンネルを張り IPv6（`2001:470:23:47b::2`、ルーテッド /64 は `2001:470:24:47d::/64`）を付与。
     `noq-spike-server` を `0.0.0.0` 単独 bind から `[::]`（このホストは `bindv6only=0` なので
     IPv4/IPv6 両方を単一ソケットで受ける）に変更し、1つの noq `Connection` が IPv4 と IPv6 の
     両方の着信を受けられるようにした。`NoqDualFdMultipathSpikeTest` 側は path0（Wi-Fi）の接続先を
     dev box の IPv6 アドレス、path1（Cellular）はこれまで通り IPv4 アドレスに向くよう変更（Wi-Fi
     ローカル IP の取得も `localIpv4Of` から `localIpv6Of` に変更）。
     - **`dualFdMultipath_wifiPlusCellular` が実機（Tailscale OFF）で成功**: path0（Wi-Fi 実 IPv6
       bindSocket）と path1（Cellular 実 IPv4 bindSocket）が同一 QUIC コネクション内で同時に確立し、
       両方向とも双方向データ送受信を確認。さらに path0（Wi-Fi）を明示的に close しても path1
       （Cellular）単体で通信が継続する failover も確認済み（3 回中 2 回成功、1 回は後述の理由で失敗）。
       これは **Android 実機上で Wi-Fi と セルラーの物理2無線を本当に同時使用する QUIC multipath が
       成立することを示す実証結果**であり、Phase 7-7 冒頭で立てた問いへの結論となる。
     - **3 回中 1 回失敗した内訳**: サーバーログで `PATH_CHALLENGE` は送信したが `PATH_RESPONSE` を
       受信できず、クライアント側の 8 秒タイムアウトで `path validation failed` となった（同一
       セルラー回線上の 1 往復パケットロスと見られ、noq/QUIC 側の設計不備ではない。以前 Phase 7-7 で
       観測した「SoftBank LTE 側のロス」という所見と整合する）。実運用では QUIC の PTO 再送や
       アプリ層のリトライで吸収可能な範囲と考えられるが、本番導入時は `open_path` のタイムアウト・
       リトライ戦略を単発 8 秒より寛容にする必要がある。
     - **使い捨て検証コードへの反映**: `rust-core/noq-multipath-spike/src/bin/server.rs`（dual-stack
       bind）、`app/src/androidTest/kotlin/tools/isekai/terminal/NoqDualFdMultipathSpikeTest.kt`
       （path0 を IPv6 に、`localIpv6Of` 追加）に反映済み。dev box 側の HE トンネル・iptables ICMP
       許可・ip6tables 状態は dev box のセッション環境に残置（本リポジトリのコードではない）。

#### 設計判断

**基本方針（2026-07-02 決定）**: Wi-Fi⇔セルラー物理同時マルチパスは「使える環境なら使う、使えなければ
黙って諦めて既存経路（QUIC migration や Tailscale⇔直接アドレスの multipath）にフォールバックする」
日和見的（opportunistic）機能として位置づける。Tailscale の有無を事前条件として分岐する専用モードは
作らない。上記の「PathState: Validated な経路だけ使う」という path broker の設計方針そのままで良く、
Wi-Fi⇔Cellular はその候補の一つ（`PathCandidate` に `physical_wifi`/`physical_cellular` を将来追加する
イメージ）として扱えば、Tailscale 稼働中で bindSocket が使えない環境では自然に「候補が Validated に
ならない」だけで済み、特別なエラー処理を書く必要がない。

上記より、Phase 7 の本線は以下のように整理する:

```
本線（引き続き Phase 7-1〜7-5 の対象）:
  通常の QUIC connection migration（単一パスの切替）
  Tailscale アドレス ⇔ 直接アドレスの multipath（追加実装ほぼ不要、正式機能候補）

実験機能（デフォルト OFF、将来のサブフェーズ）:
  Wi-Fi ⇔ セルラーの物理同時マルチパス
  → Tailscale 無効時は Network.bindSocket() が仕様通り機能することを実測で確認済み（2026-07-02）
  → Wi-Fi ⇔ Cellular の同時 noq multipath（dualFdMultipath_wifiPlusCellular）も実機で成功
    （dev box を IPv6 デュアルスタック化して解消、3回中2回成功・failoverも確認、2026-07-02）
  → 技術的実現可能性は実証済みだが、Tailscale 併用が前提の現在のユーザー像とは相性が悪く、
    正式機能化の優先度は低いまま（実験機能の位置づけを維持）

将来の fallback（Phase 7-4 拡張候補）:
  MASQUE（RFC 9298 CONNECT-UDP）/ relay-as-a-path
  → UDP directが機能しない環境向け。ポート番号ではなく「確立された信頼される宛先」経由が鍵になる
    可能性があり、実装前に安価な検証（既存のCDN等のH3エンドポイントとの疎通確認）を挟む
```

さらに、今回の一連の調査を通じて「スマホ側が常に通信を開始し、双方向で検証できた経路だけを採用する」
という制御層（**path broker**）の必要性が明確になった。これは QUIC の代替ではなく、QUIC connection
migration / multipath QUIC を前提にした、複数の経路候補（Tailscale・直接アドレス・将来のrelay等）から
「使える経路だけ」を選んで noq/quinn に渡す薄い管理層である:

```
PathCandidate: direct_ipv4 / direct_ipv6 / tailscale / masque_relay（将来）
PathState: Unknown / Probing / Validated / Degraded / Failed / Cooldown
Policy: Validated な経路だけ使う。失敗した経路は Cooldown を置いてから再試行する
```

`TransportPreference`（Phase 7-4 で導入済み）は現状 4 択（`PlainSsh`/`TsshdQuic`/`IsekaiHelperQuic`/
`Auto`）の静的な選択に留まっているが、path broker はこの `Auto` の内部実装を「複数の同時候補から動的に
選ぶ」方向へ発展させるものと位置づけられる。

**Kotlin 側の第一歩は着手済み**: `app/src/main/kotlin/tools/isekai/terminal/session/NetworkPathMonitor.kt`
に `PathId`（`DIRECT`/`TAILSCALE`）・`PathState`（`UNKNOWN`/`PROBING`/`VALIDATED`/`DEGRADED`/`FAILED`/
`COOLDOWN`）と、`ConnectivityManager.NetworkCallback` を使ってネットワークレベルの到達可能性を追跡する
`NetworkPathMonitor` を実装済み。実機不要、Robolectric（`app/src/test/kotlin/tools/isekai/terminal/
session/NetworkPathMonitorTest.kt`、`@Config(sdk = [33])` — Robolectric 4.13 は `targetSdk=36` を直接
サポートしないため既存テストに倣い固定）で `NetworkCallback` の発火を模擬し、状態遷移を検証済み
（4 件とも pass）。**ただし現時点で到達できるのは `VALIDATED`/`FAILED` のみ**（ネットワークインター
フェースの有無を反映するだけ）。`PROBING`/`DEGRADED`/`COOLDOWN` は実際の QUIC 層でのプローブが必要で、
まだ何も駆動していない（型として先取りしているのみ）。

**既存コードへの配線も実施済み**: `TransportPreference`/`ActiveSession`（＝`HelperQuicSession` の接続先
選択）への配線は、noq によるマルチパスをまだ本番導入していないため未着手のまま。一方、現状唯一の
既存の消費者だった `AndroidAppExecutor.registerNetworkCallbacks()` は元々「`NET_CAPABILITY_INTERNET`
を持つネットワークが1つでも増減したら即 `onAvailable`/`onLost`」という単一の粗い判定だったのを、
`NetworkPathMonitor` 経由の direct/Tailscale 別々の状態を使い、**両方とも失われたときだけ**
`TerminalViewModel.onNetworkLost()`（→ `session.notifyNetworkLost()`）を発火するように変更した
（`onAggregateChanged` コールバックで集約）。例えば Wi-Fi が瞬断しても Tailscale 経由の経路がまだ
生きていれば、以前は誤って「ネットワーク喪失」を通知していたのが、今は正しく通知しなくなる。
Robolectric テスト2件を追加し、集約ロジック（最初の1本が来たら true、最後の1本が落ちたら false）を
検証済み。既存の全ユニットテストにも回帰無し。

#### 検証に使った使い捨てコード

`rust-core/noq-multipath-spike/`（独立した workspace member、`noq` 依存はここに隔離）と
`app/src/androidTest/kotlin/tools/isekai/terminal/NoqDualFdMultipathSpikeTest.kt`
（`android.permission.CHANGE_NETWORK_STATE` を `app/src/debug/AndroidManifest.xml` に追加済み、
debug ビルドのみ）に実機検証コードとして残してある。本番機能ではないため、`TransportPreference` に
Tailscale⇔直接アドレスの multipath を正式導入する際に、参考にしつつ書き直すか削除するか判断する。

---

## Phase 8: Opaque SSH byte-stream resume proxy（Phase 7 とは別フェーズ）

### 位置づけ

Phase 7 は「QUIC connection migration が成功する範囲」でしか SSH セッションを守れない。QUIC connection
自体が完全に失われた場合（アイドルタイムアウト超過、長時間の圏外など）、Phase 7 の設計では内側の SSH
セッションも道連れで死ぬ。Phase 8 は、この「QUIC connection が完全に死んだ後」のケースに対応する
追加機能であり、**SSH を終端しない**まま実現できる。

**重要な区別**: これは「SSH セッションの resume」ではなく、「SSH の下にある opaque な双方向バイト列を、
外側の接続が死んだ後も同じ順序・同じ位置から再接続する仕組み」である。SSH の暗号化・MAC・シーケンス番号は
Android 側の SSH client（russh）と `sshd` がそれぞれ保持し続けており、helper はそれらを一切知らない・
関与しない。helper がやるのは「バイト列を欠落・重複・順序入れ替えなく届ける」ことだけ（RFC 4253 の
Transport Layer Protocol は 8-bit clean な binary-transparent transport の上で動作する設計であり、
下位トランスポートの差し替え自体は自然だが、下位トランスポートのエラーは SSH 接続終了に直結する
と明記されている）。

### アーキテクチャ

```
Android SSH client（russh、opaque な SSH バイト列を扱う）
  ↓
[resume stream client]  ← Phase 8 で新設
  ↓ QUIC connection A → （切断）→ QUIC connection B
[isekai-helper 内の resume proxy]  ← Phase 7 の helper を拡張
  ↓ TCP socket（生きたまま）
sshd
```

helper は C→S・S→C それぞれの byte offset を管理するだけで、SSH パケットの中身には一切関与しない。

```
C→S: client_sent_offset / helper_committed_offset
S→C: helper_sent_offset / client_delivered_offset
```

再接続時は `resume(session_id, c2s_offset, s2c_offset)` を送り、双方が確認済みのオフセットより先の
バイト列だけを再送する（未確認分は Android 側 input replay buffer / helper 側 output buffer に保持する）。

### 成立条件 — できる形 / できない形

**できる**: 「Android アプリ（SSH client）プロセス自体は生きている」かつ「helper 側の TCP socket to sshd
も生きている」状態で、その間をつなぐ外側の QUIC connection だけが張り替わるケース。

**できない**: Android アプリプロセスが死んで SSH client の暗号鍵・MAC sequence number・channel state・
rekey state が失われた場合、新しい SSH client を既存の sshd セッションに参加させること。これは
helper が SSH を終端していない以上、原理的に不可能（helper はそれらの状態を知らない）。

### Phase 7（QUIC migration）との違い

| | Phase 7: QUIC migration | Phase 8: byte-stream resume |
|---|---|---|
| 前提 | QUIC connection 自体は生きたまま経路だけ変わる | QUIC connection 自体が一度死に、新しい connection を張り直す |
| 必要な仕組み | QUIC 標準機能（RFC 9000 の path validation）のみ | 自前の session_id / offset / ACK / buffer / replay 制御一式 |
| 実装済み度 | Phase 7 でカバー | 未着手（本フェーズで新設） |

QUIC の stream は ordered byte sequence を保証するが、**別の QUIC connection にまたがって同じ stream を
継続する仕組みまでは提供しない**ため、Phase 8 の resume ロジックは QUIC の外側にアプリケーション層として
実装する必要がある。

### 実装上の難所

- **「どこまで届いたとみなすか」の定義**: QUIC の ACK は QUIC endpoint 間の配送確認であり、Android の
  SSH parser がそのバイトを実際に処理したことまでは意味しない。resume 層で独自に
  `client_delivered_s2c_offset` / `helper_committed_c2s_offset` を管理する必要がある。
- **S→C バッファの肥大化**: 切断中に `sshd` が大量出力すると helper 側バッファが膨らむ。上限に達したら
  TCP socket からの読み込みを止め、backpressure で shell/sshd 側を詰まらせる（正しい挙動だが、長時間切断には弱い）。
- **SSH keepalive の扱い**: helper は SSH keepalive に代理応答できない（SSH を終端していないため）。
  切断中に届いた encrypted keepalive request は他のデータと同様にバッファされ、再接続後にそのまま
  Android 側へ中継されるが、`sshd` 側のタイムアウトが先に来た場合はどうやっても救えない。

### フェーズ分割

| # | 内容 | 成果物 |
|---|------|--------|
| 8-0 | resume プロトコルの契約を確定（session_id、reconnect token、bidirectional byte offset、app-level ACK のワイヤーフォーマット） | ✅ `HELPER_PROTOCOL.md` §7 に確定。2 stream 構成（data stream は Phase 7 のまま raw pipe、新設の control stream で `CONTROL_HELLO`/`CONTROL_ACK`/`APP_ACK`/`RESUME`/`RESUME_ACK` を交換）、4 オフセット（`client_sent_offset`/`helper_committed_offset`/`helper_sent_offset`/`client_delivered_offset`）、`resume_proof = HMAC(session_secret, exporter \|\| session_id)` によるリプレイ防止、`REJECT_UNKNOWN_SESSION`/`REJECT_OFFSET_GONE` を含む拒否応答、Phase 7 helper への後方互換（`max_concurrent_bidi_streams=1` 制限で自然に resume 無効フォールバック）を定義 |
| 8-1 | helper 側 output buffer（上限付き、backpressure 連動）の実装 | ✅ `rust-core/isekai-helper/src/resume.rs`（`OutputBuffer`/`SessionTable`、7 unit tests）+ `main.rs` に control stream（`CONTROL_HELLO`/`CONTROL_ACK`/`APP_ACK`）を配線。`max_concurrent_bidi_streams` を 1→2 に変更。**中継は control stream の accept を待たずに即座に開始する**（当初 accept を先に待つ実装にしていたが、control stream を開かない/開けないクライアントとの疎通が最大 `HELLO_TIMEOUT`(5秒) 遅延するリグレッションを e2e テストで検出し、control stream の accept を背後タスクに切り出して修正）。**実際の reattach（`RESUME`/`RESUME_ACK` 処理・session 再利用）は Phase 8-3 のスコープとして未着手**。`isekai-helper` を v0.2.0 に更新し musl バイナリ再ビルド済み |
| 8-2 | Android 側 input replay buffer の実装 | ✅ `rust-core/src/resume_client.rs`（`ReplayBuffer`/`ClientResumeState`、4 unit tests）。`helper_quic_transport.rs` に client 側 control stream（`CONTROL_HELLO`送信/`CONTROL_ACK`受信/`APP_ACK`送受信）を配線。8-1 と同じ理由で **control stream の確立を待たずに即座に SSH セッションへ stream を渡す**（背後タスクで並行に control stream を確立）。実 SSH 経由の e2e テストで `session_id` 発行までの実疎通を確認済み（当初 data stream を包んでいた `ResumeAwareStream` は Phase 8-3 で `ReattachableStream` に置き換わり、テスト専用として残存） |
| 8-3 | reattach ハンドシェイク（新しい QUIC connection から既存 resume セッションへの再接続）の実装 | ✅ helper 側: 接続の最初の1バイトで `HELLO`（新規）/`RESUME`（reattach）を判別し、`handle_resume_stream` で proof 検証→session 検索→バッファ範囲チェック→`RESUME_ACK`→未確認データ再送→中継再開。`Session::parked_tcp`（data stream 切断時に TCP を退避）+ `parked_since` による自動失効（`sweep_expired_parked`）。client 側: `resume_client::ReattachableStream` が QUIC connection 消失を検知しても **russh にエラーを見せず** 背後で `RESUME` を送って再接続する（指数バックオフ、最大5回）。実際に `debug_fault` で QUIC connection を切断→自動 reattach→同一 SSH セッション継続、を e2e テスト（helper 側・client 側双方）で確認済み |
| 8-4a | 実機不要な reject/失効パスのローカル検証 | ✅ `REJECT_UNKNOWN_SESSION`（未知の session_id / 実は resume 不可能）と `REJECT_OFFSET_GONE`（要求 offset が buffer 範囲外）を実際に発生させる e2e テスト2件を追加。`--idle-timeout` を短く設定して `sweep_expired_parked` が実際に発火し、期限切れセッションへの resume が `REJECT_UNKNOWN_SESSION` になることを確認する e2e テスト1件、および sweep が期限内/アクティブなセッションには触れないことを確認する unit テスト1件を追加。client 側は `ReattachableStream` が `REATTACH_MAX_RETRIES`(5回・指数バックオフ計15秒)を使い切った後に `Poll::Pending` を返し続けず実際の `io::Error` を russh に見せることを、`tokio::time` の仮想時間(`start_paused`)で確認する unit テストを追加（そのため `tokio` の `test-util` feature を dev-dependency に追加）。isekai-helper 15 tests / tssh-core 66 tests 全て pass |
| 8-4a' | Robolectric で検証可能な範囲の追加検証 | ✅ 既存の Robolectric 資産（`NetworkPathMonitorTest`＝shadow ConnectivityManager、`TerminalViewModelTest`＝QUIC 接続時は network-lost で切断しない、Room in-memory、Compose UI）を調査。ギャップだった `app/src/debug/kotlin/.../FaultInjectionReceiver.kt`（実機の `adb shell am broadcast` からフォルト注入する debug 専用 BroadcastReceiver）に `FaultInjectorApi` インターフェースを導入し native FFI 呼び出しを差し替え可能にした上で、`app/src/testDebug/kotlin/.../FaultInjectionReceiverTest.kt` を新設（8 tests: 5 action の intent→FFI 引数マッピング、extra 欠落時のデフォルト値、未知 action/null action で何もしないこと）。`KeystoreKek`/`KeyManager` は Android Keystore が Robolectric で emulate されないため引き続き実機(androidTest)のみ。`testDebugUnitTest` 214 tests 全 pass |
| 8-4b | 実機検証（長時間の圏外、大量出力中の切断、keepalive タイムアウト境界の確認） | ✅ Tailscale 経由 isekai-helper QUIC 接続で `debug_fault` の CUT/RESTORE を使い3シナリオを実施。**シナリオ1(完全断・修正前)**: client が接続喪失を検知するまで実測約43秒（QUIC idle timeout 未設定でサーバー側30秒設定に引きずられ + PTO再送）かかる一方、helper 側は同じ30秒で park セッションを破棄していたため、**reattach が5回とも必ず `REJECT_UNKNOWN_SESSION` になり毎回失敗する**致命的なタイミング不整合を実機でのみ発見（ローカルe2eは `conn.close()` による即時切断検知のため再現しなかった）。**修正**: client 側（`helper_quic_transport.rs`）に `keep_alive_interval`(5秒・NAT UDPマッピング維持)と短い`max_idle_timeout`(15秒)を追加。helper 側は `--idle-timeout`(QUIC transport 生存確認、既定15秒)と `--resume-window`(park セッション保持時間、新設)を分離。**修正後に再検証**: 検知が約19秒に短縮、reattach が2回目の試行で成功、reject 無し。**シナリオ2(大量出力中の切断)**: `seq 1 200000`(約20万行)実行中に CUT→RESTORE。reattach は接続不能な間の4回の試行がそれぞれ `--idle-timeout` と同じ長さ（quinn が handshake タイムアウトとして内部流用）だけブロックすることが判明し、5回全滅する最悪ケースの合計時間は指数バックオフの15秒ではなく**実測で約90秒**（既定 `--resume-window` 90秒とほぼ同値でマージンが薄いと判明）かかることを確認。5回目の試行(RESTORE後)で成功し `helper_committed_offset=3622` から再送・全20万行が最後まで正常に出力完了、その後のコマンドも正常応答。この実測を受けて `--resume-window` の既定値を **120秒**に引き上げ（isekai-helper v0.3.2、musl再ビルド・Androidアプリ再ビルド・実機で再確認済み）。**シナリオ3(keepalive境界)**: フォルト注入なしで100秒アイドル待機し、reattach/disconnect/reject が一切発生せず、待機後もコマンドが即座に応答することを確認。HELPER_PROTOCOL.md §1/§7.5 を全て更新済み。tssh-core 78 tests・isekai-helper 15 tests 全 pass |
| 8-4c | 実機待ちの間のリファクタ | ✅ Rust: `isekai-helper/src/main.rs` の `handle_resume_stream` で4箇所コピペされていた「TCP を park に戻す」処理を `repark()` ヘルパーに抽出。`rust-core/src/helper_quic_transport.rs` で HELLO/RESUME 双方にあった HMAC proof 計算の重複を `compute_proof()` に統一。`rust-core/src/resume_client.rs` の `poll_read`/`poll_write` で重複していた「reattach 起動 + waker 登録」を `begin_reattach_after_io_error()` に抽出。Kotlin: `ProfileListScreen.kt`/`KeyListScreen.kt` の削除確認 `AlertDialog` を `ui/ConfirmDialogs.kt` の `DeleteConfirmDialog` に共通化。`tsshd_port` のデフォルト値 2222 を `ConnectionProfile.DEFAULT_TSSHD_PORT` に定数化（`AppDatabase.kt` の Room migration 内のリテラルは歴史的記録のため意図的に据え置き）。6画面で直書きされていたダークテーマの色 hex を `ui/AppColors.kt` に集約。すべて振る舞い変更なし、tssh-core 66 tests・isekai-helper 15 tests・Android 214 tests 全 pass |
| 8-4d | Kotlin/Rust 境界レビュー: セッション状態の判断ロジックを Rust 側 SSOT に統一 | ✅ `TerminalSession.kt` のコメント「セッション状態の SSOT は Rust 側に持つ」に反し、`notifyNetworkLost()`（ハンドシェイク中/TCP接続中は切断、QUIC接続中は無視、という判断）が Kotlin 側のミラー状態(`_state`)を見て判断していた。`rust-core/src/orchestrator.rs` の `SessionOrchestrator` に `ConnPhase`（Idle/Connecting/Connected）を追加して SSOT を Rust 側に一元化し、`notify_network_lost()` を新設。Kotlin 側は生イベントを転送するだけの1行に縮小し、結果は既存の `onConnectionStateChanged` コールバック経由で反映される。UniFFI Kotlin bindings を再生成（`cargo run -p uniffi-bindgen -- generate --library target/debug/libtssh_core.so --language kotlin`）。`FakeOrchestrator`（テスト用）にも同じ判断ロジックを実装し直して既存テスト(`TerminalSessionTest`)を維持。tssh-core 66 tests・Android 214 tests 全 pass |

対象外: SSH セッションそのものの再生成・代理応答・端末状態同期（mosh 的な state sync はやらない）。

### リスク

| リスク | 対策 |
|--------|------|
| Android アプリプロセスが kill された場合は resume 不可能 | 仕様上の既知の限界として明記し、その場合は Phase 7 の通常ブートストラップからやり直す |
| 長時間切断で helper 側バッファが肥大化・OOM | バッファ上限を設け、上限到達時は TCP backpressure で `sshd`/shell 側を詰まらせる（データロスではなく詰まりで表現する） |
| sshd 側のタイムアウトが resume より先に来る | helper は SSH keepalive に代理応答できないため原理的に防げない。ユーザーへの期待値として明記する |
| resume トークン・offset の改ざん/リプレイ | Phase 7 の `proof = HMAC(session_secret, export_keying_material)` と同様の束縛を reattach ハンドシェイクにも適用する |

### 参照

- RFC 4253（SSH Transport Layer Protocol）: https://datatracker.ietf.org/doc/html/rfc4253
- RFC 9000（QUIC）: https://datatracker.ietf.org/doc/html/rfc9000

---

## Phase 9: 受動的マルチパスフェイルオーバー（Tailscale⇔直接アドレス、第一段）

### 位置づけ

Phase 7-7で実証したnoq（quinnのmultipathフォーク）実機スパイクを基に、Tailscale経由アドレスと
直接アドレスを常時ホットスタンバイさせ、Tailscaleオーバーレイ自体の不調（relay flapping、
coordination server障害、NAT traversal失敗）に対してPhase 8のRESUME往復すら待たずに即座に
フェイルオーバーする機能。QUIC connection migration（Phase 7、無線切替は既に実機で「体感遅延なし」）
とは別の問題を解決する。

9-0〜9-3でまずTailscale⇔直接アドレスの受動フェイルオーバー（二値状態のみ、ヘルスチェック無し）を
実装し、9-5で能動的ヘルスチェック（Degraded検知）、9-4でWi-Fi⇔セルラー物理multipath候補を追加した
（実装順は9-5→9-4、依存関係上こちらが自然だったため）。詳細設計は
`/home/cuzic/.claude/plans/typed-dancing-codd.md` 参照。9-4は実機（Android）が無いセッションで
実装したため**実機検証はまだ**。

### フェーズ分割

| # | 内容 | 成果物 |
|---|------|--------|
| 9-0 | `noq`でHELPER_PROTOCOL.md契約（cert pinning用カスタム`ServerCertVerifier`、`export_keying_material`、HELLO/ACK proof）を再現できるか検証。既存の`quinn`クライアントが`noq`サーバーに接続できるか（後方互換の核心前提）を検証 | ✅ `rust-core/noq-multipath-spike/src/bin/compat_check.rs`。全チェックPASS：①noqサーバー上でHELLO/ACK/proof契約を完全再現、②**無改造の`quinn`クライアントが`noq`サーバーに対してQUICハンドシェイク＋HELLO/ACKを完了**（単一リスナー方式の前提が成立）、③noq multipath（path0+path1+path0 close後のfailover）がこの契約の上でも機能。`noq::TransportConfig`/`ClientConfig`/`ServerConfig`のAPIは`quinn`とほぼ1:1（`max_idle_timeout`/`keep_alive_interval`/`max_concurrent_bidi_streams`/`datagram_receive_buffer_size`/`export_keying_material`/`TryFrom<rustls::ClientConfig>`/`TryFrom<rustls::ServerConfig>`いずれも同名で存在）。→ **9-1は単一リスナー方式（quinn→noq移行、クライアント側Phase 7/8コードは無変更）で進める** |
| 9-1 | `rust-core/isekai-helper/src/main.rs`のQUICリスナーを`quinn`→`noq`に移行。`max_concurrent_multipath_paths(8)`を有効化 | ✅ `quinn::`→`noq::`機械的置換（APIはほぼ1:1）。唯一の非互換点：`Connection::remote_address()`はmultipath化で無くなり、`Connecting`にのみ残存（確立後は`conn.path(PathId::ZERO).remote_address()`で代替、ログ用途のみ影響）。isekai-helper側15テスト（unit 8 + e2e 7）全pass。**tssh-core側66テストも全pass**（うち`helper_bootstrap::bootstraps_and_launches_helper_over_real_ssh`/`helper_quic_transport::full_stack_bootstrap_quic_and_shell_command`/`resume_survives_connection_cut`は実SSH・実sshd相手のopt-inテストで、これらも通過＝**新isekai-helper(noq)に対し無改造の既存quinnクライアントで実際にSSH bootstrap→QUIC接続→shell実行→Phase 8 resumeまでフルスタック疎通を確認**）。v0.3.0としてmuslバイナリ再ビルド・sha256更新済み（`build-isekai-helper-musl.sh`） |
| 9-2 | クライアント新規コード：`PathCandidateId`（Primary/Secondary）・二値`PathState`のbroker、`noq::Connection::open_path()`でpath1確立。`TransportPreference::IsekaiHelperQuicMultipath`として配線 | ✅ `rust-core/src/multipath_transport.rs`新設。**resume/reattach層は無し**——multipathは1コネクション内の複数pathなので、片方が生きている限りコネクション自体が死なず、Phase 8のような明示的再接続機構が不要という設計（PLAN.md本文参照）。`helper_quic_transport.rs`からHELPER_PROTOCOL.md契約（ALPN/フレーム定数/`PinnedCertVerifier`/埋め込みバイナリ/ブートストラップ関数）を`pub(crate)`化して再利用、Phase 7/8のコード自体は無変更。`open_path`は単発8秒待ちではなく3回リトライ+指数バックオフ（2s/4s/8s）。`TransportPreference::IsekaiHelperQuicMultipath`・`ActiveSession::MultipathHelperQuic`・`SessionOrchestrator::connect_multipath_helper_quic`を追加。loopback（127.0.0.1/127.0.0.2、同一noqサーバーへの別経路）でのunit test 3件（path0+path1確立、path1無し、**path0 close後もコネクションが生きてHELLO/ACK往復に応答できる**＝受動的フェイルオーバーの核心）全pass。tssh-core全69テスト（既存66+新規3）pass、無回帰 |
| 9-3 | Kotlin側配線：`ConnectionProfile.directAddress`追加、UI、uniffi再生成 | ✅ uniffi Kotlinバインディング再生成（`cargo run -p uniffi-bindgen`）。`ConnectionProfile.directAddress`（Room migration 4→5、DB version 5）、`ProfileEditScreen`に「自作ヘルパー QUIC（マルチパス）」チップ+直接到達アドレス入力欄、`TerminalViewModel`/`TerminalSession`/`SessionOrchestrator`経由で`connectMultipathHelperQuic`まで配線。`FakeOrchestrator`（test/androidTest 両方）に新メソッド追加。`ConnectionProfileRepositoryTest`にroundtrip確認2件追加。**Gradleでのビルド確認は開発環境のディスク枯渇（`No space left on device`）に一度遭遇したため`cargo clean --profile dev`でRust側の使い捨てビルドキャッシュ約10.7GiBを解放してから実施**。`:app:compileDebugKotlin`/`:app:testDebugUnitTest`とも成功、Kotlin側テスト226件（既存214+新規追加分）全pass、無回帰 |
| 9-5 | 能動的ヘルスチェックによるDegraded検知。独自ping/pongは作らず`noq::Path`のネイティブAPI（`ping()`/`stats()`/`set_status()`）だけで完結させる | ✅ `PathState::Degraded`追加。3秒間隔で`path.ping()`→`path.stats()`→`classify_path_health()`（純粋関数、RTT>800ms・直近ロス率>20%・black hole検出のいずれかでDegraded）→`path.set_status(Backup)`でnoq自身のスケジューリング優先度を下げる（他に健全なpathがあれば自動的にそちらが優先される）。回復は連続2回健全でAvailableに戻す。`PathBroker`は`noq::PathId`→候補IDの明示マッピング方式に変更（旧`PathId::ZERO`ヒューリスティックは廃止、9-4の4候補に対応するため）。unit test 8件（synthetic `PathStats`、非exhaustive構造体のため`Default::default()`後にフィールド代入する形で構築）全pass |
| 9-4 | Wi-Fi⇔セルラー物理multipath候補の追加。JNIブリッジは作らずUniFFIの素の`Int32`でfdを渡す。`MultiUdpSocket`（`noq-multipath-spike`の`DualUdpSocket`を一般化）でdefault 1本+`Network.bindSocket()`済みのbound socket N本を束ねる。実験的機能・既定OFF・プロファイル単位オプトイン | ✅ Rust: `MultipathHelperQuicConfig`に`wifi_fd`/`wifi_local_ip`/`cellular_fd`/`cellular_local_ip`追加、`PathCandidateId::PhysicalWifi`/`PhysicalCellular`、`MultiUdpSocket`/`NamedUdpSocket`実装。loopbackで生fd(127.0.0.3にbind)を使い物理path確立を検証するunit test追加、全10 e2e/unit test pass。Kotlin: `PhysicalPathProvider`新設（`requestNetwork`→`DatagramSocket`→`bindSocket`→`ParcelFileDescriptor.detachFd()`、Tailscale稼働中のEPERM等は例外を握りつぶしnullを返すだけ＝日和見的ポリシー）、`AppExecutor`に`acquirePhysicalMultipathFds`/`releasePhysicalMultipathFds`追加（`AndroidAppExecutor`+test/androidTest両方の`DumbAppExecutor`に実装）、`ConnectionProfile.enablePhysicalMultipath`追加（Room migration 5→6、DB version 6）、`ProfileEditScreen`にチェックボックス+説明文、`TerminalViewModel`で有効時のみ`acquirePhysicalMultipathFds()`を呼びconfigへ詰め、切断時に`releasePhysicalMultipathFds()`。`CHANGE_NETWORK_STATE`権限をmain manifestへ昇格（debug manifestの重複記述は削除）。Kotlinテスト5件追加（`TerminalViewModelTest`3件、`ConnectionProfileRepositoryTest`2件）、全pass。**実機検証は未実施**（このセッションにAndroid実機が接続されていないため。`Network.bindSocket()`/`ConnectivityManager.requestNetwork()`はPhase 7-7で実機検証済みのAPI組み合わせをそのまま踏襲しているため技術的リスクは低いと見ているが、次回実機がある環境で`phase7-5-roaming-test.sh`に類するシナリオ検証が必要） |

対象外（Phase 9の範囲では）: 実機検証（9-4は特に、実機無しでは`Network.bindSocket()`系APIを検証できない）。

### Phase 9-2/9-3 実機検証結果（2026-07-02、Xperia XQ-DQ44 / Android 15）

Windows PC 経由の adb ブリッジ（Tailscale越し、`ADB_SERVER_SOCKET`）で接続し、debug APK
（9-0〜9-4全コード込み）を実機にインストールして検証。プロファイル: ホスト＝dev box の
Tailscale アドレス（`100.100.45.36`、path0）、直接到達アドレス＝dev box の公開IPv4
（`204.12.203.210`、path1）、`TransportPreference.ISEKAI_HELPER_QUIC_MULTIPATH`、
物理マルチパスは今回OFF（Tailscale使用中は効果が無い設計のため）。

- ✅ **path0（Tailscale）が実機で完全に動作**: SSH bootstrap → isekai-helper（noq版）自動配布・
  起動 → `multipath_transport.rs`のnoqクライアントでQUIC接続 → HELLO/ACK → russh session確立 →
  実shellプロンプト表示・`hostname`コマンド実行（`dev`が返る）まで確認。**Phase 9で新設した
  noqベースのマルチパストランスポートが実機で動く最初の確認**（これまではloopbackテストのみ）。
- ✅ **path1（直接アドレス）の失敗と復旧ロジックが設計通り動作**: 3回リトライ
  （1回目は`remoted CIDs exhausted`——path0確立直後すぎてpeerからのConnection ID払い出しが
  間に合わなかったとみられる、2〜3回目は`path validation failed`、既知のdev boxファイアウォール
  制約で動的UDPポートが開いていないため）した末に「3回試行後に諦める」ログが出て、**path0だけで
  セッションは何の影響も無く継続**（受動的フェイルオーバーの「片方が死んでいても全体は壊れない」
  という設計目標を実機で確認）。
- 実機ファイアウォール制約（dev box、Phase 7-5で既知）: `iptables`のINPUT chainがUDP
  45820/45822のみ許可、isekai-helperの動的UDPポートは含まれないため直接到達アドレスへのQUIC
  到達は不可能。この制約を外すには`--bind`固定ポート化（未実装、次フェーズ候補）とファイアウォール
  ルール追加が必要。
- 副次的な学び: 実機の`adb`操作中、同じ端末上で稼働している別の自動化エージェント
  （Pushbullet通知経由の別セッション）のヘッドアップ通知が繰り返しタップ操作を妨害した。
  `settings put global heads_up_notifications_enabled 0`で一時的に無効化して作業し、
  作業後に`1`へ復元した（永続変更なし）。

**結論（初回セッション）**: Phase 9-2/9-3（Tailscale⇔直接アドレスの受動的マルチパス）の中核機能
——noqベースのQUIC接続確立、HELLO/ACK認証、russhセッション、path失敗時のグレースフルデグレード
——は実機で正常動作を確認済み。9-5（ヘルスチェック）と9-4（物理マルチパス）は追ってこのセッション
中に検証した（下記）。

### Phase 9-5 実機検証結果（2026-07-02、同機、追加セッション）

`multipath_transport.rs`に`debug_fault`（既存の`helper_quic_transport.rs`/`faulty_udp_socket.rs`と
同じ`UdpFaultInjector`シングルトン）を配線した上で実施。新しいadb broadcastやUniFFI関数は増やして
いない——既存の`isekai-fault-latency300`等と同じ`FaultInjectionReceiver`がこのトランスポートにも
効くようになっただけ。

- **事前にloopbackでも確認**: `multipath_transport.rs`に`path0_degrades_and_recovers_under_injected_latency`
  テストを追加。900ms一方向遅延では noq の RTT 平滑化（EMA、smoothed = 7/8*old + 1/8*latest）が
  収束するまで798msで頭打ちに近づき閾値(800ms)を超えるのに10サンプル以上（約35秒）かかることが
  判明したため、5秒一方向遅延（往復10秒）に変更し確実に1〜数サンプルで閾値超えするようにした。
  Degraded遷移・Available復帰とも確認、テストは無事pass。
- **実機（Tailscale経由プロファイル）でも確認**: `adb shell am broadcast`で`SET_LATENCY`(3000ms)を
  直接送信し、約26秒後に実ログで
  `multipath_quic: path Primary degraded (rtt=1.689456621s), demoting to Backup`
  を確認。続けて`RESTORE`+`CLEAR`を送信し、約86秒後（EMA復帰も同様に緩やかに収束するため）に
  `multipath_quic: path Primary recovered, marking Available`
  を確認。この間シェルセッションは`uptime`コマンド実行含め一貫して応答し続けた
  （Degraded中もBackupに格下げされるだけでコネクション自体は生きたまま）。
- **結論**: Phase 9-5（能動的ヘルスチェックによるDegraded検知）は実機で完全に動作を確認。

### Phase 9-4 実機検証結果（2026-07-03、同機、Windows PC復旧後の再開セッション）

Windows PC（`dragonflyg4`）のTailscale再オンライン化を確認後、「直接通信」プロファイル
（host=`204.12.203.210`＝直接到達アドレスをそのままpath0にも使用、鍵=`isekai-test-key`、
`ISEKAI_HELPER_QUIC_MULTIPATH`、直接到達アドレス欄=同アドレス、物理マルチパスON）を作成し、
Tailscale OFF状態（`am force-stop com.tailscale.ipn`）で実機検証した。**2件の実バグを発見・修正し、
「直接アドレスでのマルチパス（path0+path1）」は実機で完全動作を確認**。一方「物理Wi-Fi/セルラー
同時利用」はnoqライブラリ側の制約に突き当たり、現状のnoq 1.0.1では実現不可という結論に至った。

**修正1: `PhysicalPathProvider.bindAndDetach()`のIPv4アドレス取得バグ**
`socket.bind(InetSocketAddress(0))`（ワイルドカードbind）だと、この端末のようなデュアルスタック
環境ではIPv6ワイルドカード(`::`)が選ばれ、`socket.localAddress`が`Inet4Address`にならず
「no local IPv4 address」で毎回失敗していた（`dumpsys connectivity`ではWi-Fi/セルラーとも実際は
IPv4アドレスを持っているにもかかわらず）。`ConnectivityManager.getLinkProperties(network)`から
実際のIPv4 `LinkAddress`を取得し、そのアドレスへ明示的にbindするよう修正
（`app/src/main/kotlin/tools/isekai/terminal/session/PhysicalPathProvider.kt`）。

**修正2: direct_host向けisekai-helperの待受ポートを固定化**
`direct_host`はTailscaleの`ts-input`チェーンを経由しないため、外部ファイアウォールで
事前に許可されていないと到達不能——エフェメラルポート（`0.0.0.0:0`、既定）では原理的に
外部ファイアウォールが対応できない。`direct_host`使用時のみisekai-helperを固定ポート
（`DIRECT_MULTIPATH_BIND_PORT = 45823`、`rust-core/src/multipath_transport.rs`）で待受させる
`--bind`引数を追加し、`helper_bootstrap.rs`/`helper_quic_transport.rs`に`bind_port: Option<u16>`を
スレッディング。dev box側で`204.12.203.210`のiptablesにUDP/45823のACCEPTルールを追加した
（本番運用では、direct_host機能を使う全ユーザーがサーバー側でこのポートを開ける必要がある——
現状固定値のハードコードなので、将来的にプロファイル単位で設定可能にする余地を残している）。

**結果: path0（直接アドレス）+ path1（Secondary、OSデフォルトルート）が実機で確立・実shell動作を確認**
`multipath_quic: path Secondary established: id=PathId(...)`のログとともにSSHシェルセッションが
正常に動作。Phase 9-4の「direct_hostへのマルチパスフェイルオーバー」自体は実証済み。

**未解決: 物理Wi-Fi/セルラー個別pathの`open_path()`がnoq側でvalidation failed**
`PhysicalWifi`/`PhysicalCellular`候補（`Network.bindSocket()`で明示的にbindしたfd由来の
ソケット）は3回リトライしてすべて`path validation failed`で失敗する。詳細な診断
（`MultiUdpSocket`の送受信箇所に一時的なログを仕込み、`adb logcat`をリングバッファでなく
ストリーミングキャプチャして確認）の結果:

- fd自体・`Network.bindSocket()`・IPv4明示bindは全て正しく機能している——送信は正しい
  ローカルIP（`192.168.10.80`/`10.116.182.98`）から出ており、応答も正しいソケットで
  受信できている（`dst_ip=Some(192.168.10.80)`等、双方向1200バイトの往復を確認）。
- **にもかかわらずnoqは該当pathを`ValidationFailed`として`abandoned`にする**。しかも
  abandoned後もソケットレベルでは送受信が継続する（noqの内部状態が「もう諦めた」相手との
  やり取りをまだ続けている状態）。
**追加調査1: 「同一remoteアドレスに複数local IP」仮説 → 実機で反証**
「Secondaryは同じremoteへlocal_ip指定無しの単一pathだから成功し、PhysicalWifi/Cellularは
同じremoteに異なるlocal_ip指定で開こうとするから失敗するのでは」という仮説を立てた。dev boxが
IPv4（`204.12.203.210`）に加えてグローバルIPv6アドレス（Hurricane Electricトンネル経由、
`2001:470:23:47b::2/64`）を既に持っており、`noq::Endpoint::server()`はbindアドレスがIPv6なら
内部で`set_only_v6(false)`する（実機untestedのソース読解だけでなく、`nc`/Pythonソケットで
実際にIPv4・IPv6両方から同一ソケットへ到達できることを確認済み）ため、追加インフラ無しで
検証できた。isekai-helperの`--bind`をIPv4ワイルドカード（`0.0.0.0:port`）からIPv6ワイルドカード
（`[::]:port`）に変更（`helper_bootstrap.rs`）し、`MultipathHelperQuicConfig`に
`cellular_remote_host`フィールドを追加してセルラー候補だけIPv6アドレスへ向くようにした
（`ConnectionProfile.cellularRemoteAddress`、Room migration 6→7、UI追加込みで実装）。
**結果: セルラー候補がIPv4→IPv6という完全に別のremoteアドレスを使っても、依然として
`ValidationFailed`で失敗した。**この仮説は反証された。

**追加調査2: 「複数path同時オープン時の競合」仮説 → 実機で反証**
残る手がかりは「Secondary + PhysicalWifi + PhysicalCellularをほぼ同時に`open_path()`している
ことが、CID払い出しやanti-amplification制限を奪い合わせているのでは」という仮説。ループバック
テスト（`physical_path_candidate_establishes_via_multi_udp_socket`）はSecondary + 物理候補1本のみで
毎回成功しており、実機は常にSecondary + 物理候補2本（同時）で失敗していたため、この仮説は筋が
通って見えた。`establish_multipath_connection`を、Secondary/物理候補ごとに`RUNTIME.spawn`していた
並列オープンから、1本ずつ確立（またはリトライ尽き）を待ってから次を開く完全直列オープンに変更した。
**結果: 完全に直列化し、他のpath opening処理が一切並行していない状態でも、PhysicalWifi単独で
依然として`ValidationFailed`になった。**この仮説も反証された。

**追加調査3: noq-proto (MIT/Apache-2.0, GitHub `n0-computer/noq`) をローカルforkして実値ダンプ**

残る共通点は「`local_ip`を明示指定して`open_path()`する」こと自体——remoteアドレスの重複でも、
複数path同時オープンでもない。ソースレベルの静的解析だけでは確定できなかったため、`noq-proto-v1.0.1`
タグ（crates.ioの1.0.1と完全に同一コミット）をローカルにclone、`rust-core/Cargo.toml`に一時的な
`[patch.crates-io]`を追加して差し替え、`ensure_path`/`record_path_challenge_sent`/
`on_path_response_received`にファイル直書き込みの実値ダンプ（`eprintln!`はAndroidのJNI'd .soからは
logcatに出ないため、アプリ private files dirへの直接書き込みに変更）を仕込んで実機再検証した。

**結果（決定的）**: `local_ip=None`のSecondary（path 1）は`record_path_challenge_sent`が1回、
`on_path_response_received`が1回呼ばれ`is_probably_same_path=true`で正常に検証成功する。一方、
`local_ip=Some(192.168.10.80)`の物理WiFi候補は3回リトライ（PathId 2, 3, 4）し、各試行で
`record_path_challenge_sent`が2〜3回呼ばれる（PTOによる正常な再送）にもかかわらず、
**`on_path_response_received`が一度も呼ばれない**——つまり「不一致で拒否される」以前に、
PATH_RESPONSEを処理する関数自体に到達していない。診断ログの生UDPレベルでの双方向1200バイト
往復確認と合わせると、「ソケットでバイト列を受信した後、QUICレベルでPATH_RESPONSEフレームとして
解釈・ディスパッチされるまでの間」のどこかで、noq自身の内部処理（復号・CIDによるpath振り分け等、
アプリ側コードの外側）がこれらのパケットを静かに落としている、という結論に至った。

この診断結果をもとに upstream へissueを提出済み:
**https://github.com/n0-computer/noq/issues/738**
（再現手順・診断ログ全文・除外した仮説を記載。noqメンテナからの応答待ち）

診断用の一時fork（`[patch.crates-io]`によるローカルpath差し替え）はセッション終了時に削除し、
crates.io版の`noq 1.0.1`に戻した——本番ビルドへの影響は無い。

「Tailscale経由⇔直接アドレス」の受動的マルチパス（Phase 9-2/9-3設計の核）と、「direct_host自体の
マルチパス化（path0=path1=direct_host、Secondaryが正常に確立）」は実機で完全動作を確認できた。
一方「物理Wi-Fi/セルラー同時利用」は、Android側の実装（`Network.bindSocket()`・fd受け渡し・
`MultiUdpSocket`の送受信）は全て正しく機能していることを確認済みにもかかわらず、noq 1.0.1
ライブラリ内部でPATH_RESPONSEが`local_ip`明示指定pathのハンドラに到達しないため**現状では実現不可**。
upstreamでの修正（またはこちら側での原因特定・回避策発見）を待つ必要がある——これは当初の
Phase 9-4スコープの一部未達として記録する。「Wi-Fi/セルラー物理無線も同時に使う（実験的）」
チェックボックスはUI上は残るが、有効にしても実質的にpath0/path1のみのフェイルオーバーに留まる
（物理path候補は3回リトライ後にサイレントに諦め、既存のフォールバックへ委ねる設計なのでクラッシュ
等はしない）。`cellular_remote_host`（`ConnectionProfile.cellularRemoteAddress`）は今回の仮説検証
目的で追加した機能だが、根本原因ではなかったとはいえ、同一サーバーの別アドレスをセルラー経路専用に
指定できる一般的な機能として実装は残してある（将来別の用途で有用になる可能性があるため）。

診断中に一時追加した`info!`ログ（送受信のバイト数・src_ip/dst_ipダンプ）と`--log-level debug`は
発見後に削除済み（コードは通常運用時の状態に復元）。`--bind`固定ポート化・IPv6ワイルドカード化・
path直列オープン化・`cellular_remote_host`は恒久的な変更として残した。

### Phase 9-4b: `Endpoint::rebind_abstract()`によるupstream failover（2026-07-03、同日追加実装）

noq issue #738により物理Wi-Fi/セルラー同時保持は断念したが、ユーザーから「WiFiは繋がっているが
upstreamが死んでいる（カフェ等のキャプティブポータル）状況を救いたい」という具体的なユースケースが
提示された。これはPhase 9-2/9-3のTailscale⇔直接アドレス二重化では救えない
（path0/path1どちらもOSデフォルトルート任せの`self.default`ソケットを共有しているため、
物理リンク自体が死んでいる場合は両方とも道連れで死ぬ）。

`open_path()`で新規pathを追加するのではなく、`noq::Endpoint::rebind_abstract()`——
endpointの送受信ソケットを丸ごと差し替える、NATリバインド相当のAPI——を使えば、
issue #738の不具合パターン（`local_ip`明示指定の新規path追加）を踏まずに実現できるという
仮説を立て、実装・検証した。

- **Rust**: `MultipathHelperQuicSession`に`rebind_to_fd(fd, local_ip)`を追加。`establish_multipath_connection`が返す`noq::Endpoint`（`Clone`可能なハンドル）を、専用の`rebind_tx`チャネル経由で待ち受けるバックグラウンドタスクに保持しておき、要求が来たら該当fdを`MultiUdpSocket{default: <fd>, named: [], ..}`でラップして`endpoint.rebind_abstract()`を呼ぶ。`SessionOrchestrator`にも同名の薄いパススルーを追加（マルチパス以外のtransportでは無視）。
- **loopbackテストで実証**: `connection_survives_rebind_to_new_local_address`——path0確立後、127.0.0.1から127.0.0.4への`rebind_abstract()`を行い、rebind前後で新しいbi-directionalストリームによるecho往復が両方とも成功することを確認（0.04秒で完走、noq issue #738のパターンとは異なりPATH_RESPONSE云々を経由しないため一瞬で終わる）。
- **Kotlin**: `UpstreamHealthMonitor`（新規）——`NetworkCapabilities.NET_CAPABILITY_VALIDATED`をWiFiのみ対象に監視。「WiFi接続中だがVALIDATED=false」を検知した瞬間（edge-triggered）に、既存の`PhysicalPathProvider`（Phase 9-4のbindSocket実装をそのまま再利用）でセルラーのfdを取得し、`rebindToFd`を呼ぶ。`ConnectionProfile.enableUpstreamFailover`（Room migration 7→8）でプロファイル単位オプトイン、既定OFF。

**ユーザー提案による設計転換: Android OSのキャプティブポータル検知に頼らず、QUIC自身の
無応答検知で判断する**

ユーザーから「本物のキャプティブポータルが無くても、UDPを丸ごと遮断するfault injection
（既存の`debug_fault`、Phase 9-5で実機検証済みのインフラ）で模擬できるはず。応答が返って
来ないことで判断すればよい」という指摘を受け、設計を転換した。Phase 9-5の能動的ヘルスチェック
（`path.ping()`→`stats()`）を拡張し、「現在Validatedなpathが1本も無い」状態を検知したら
`TransportEvent::NoViablePath`→`OrchestratorCallback.onNoViablePath()`経由でKotlin側に通知、
`UpstreamHealthMonitor`の代わりに（または追加で）これをトリガーとしてセルラーへrebindする。

- **発見1**: `classify_path_health`（既存、RTT/ロス率/black hole判定）だけでは完全な無応答
  （100%ロス）を検知できないことが判明。noqの内部統計自体が輻輳制御的に更新を止めてしまい、
  RTT推定は古い健全値のまま固まる。そこで受信側カウンタ（`udp_rx.datagrams`）を直接見る
  `has_zero_response`を新設——「送ったのに何も受信していない」を直接検知する。
- **発見2（実機で発覚した誤検知バグとその修正）**: `has_zero_response`は単発では実ネットワークの
  ジッタ（応答が`PING_SETTLE_DELAY`内に間に合わないだけ）でも容易に真になる——実機で実際に
  245ms RTT（閾値800msの範囲内）でも単発の「無応答」を観測した。さらに深刻な実機発見: 
  `classify_path_health`（RTT/ロス率ベース）は無応答下でも「健全」と読み続けるため、
  `has_zero_response`によるDegraded降格と、`classify_path_health`による誤ったリカバリ判定が
  競合し、即座にValidatedへ戻ってしまう不整合を実機で確認した。対処として
  `healthy = classify_path_health(..) && !zero_response`とし、両シグナルを一本化。
  さらに`NoViablePath`通知自体は`NO_RESPONSE_CONSECUTIVE_CHECKS=3`回の連続無応答を要求する
  （Backup降格は引き続き単発判定、rebindという重い操作のトリガーだけ厳格化）。
- **loopbackテストで実証**: `no_viable_path_fires_when_udp_fully_cut`——`debug_fault`のCUTだけで
  （本物のキャプティブポータル無しで）`NoViablePath`が正しく発火することを確認。
- **実機検証（完全成功）**: Tailscale OFF、「直接通信」プロファイルで接続後、
  `adb shell am broadcast -a tools.isekai.terminal.debug.CUT`で全UDPを遮断したところ、
  以下が実機ログで確認できた:
  1. `path Secondary got zero responses for N consecutive checks`（N=3で最初のトリガー、
     実際には接続開始直後の一時的な事象と紛れないよう観測を継続）
  2. `no viable path left (all paths degraded/failed)` → `session: no viable path`
  3. Kotlin側で`upstream failover: rebinding to cellular (localIp=10.118.98.216)`
  4. `rebind to local_ip=10.118.98.216 succeeded`
  
  また、意図せず発生した実際のネットワークジッタ（CUT注入前、Secondary pathが実機WiFiの
  非対称な挙動で単発の無応答を示した場面）では、Primary pathが健全だったため
  `any_validated()`がtrueのままとなり、`NoViablePath`は正しく発火**しなかった**——
  誤検知防止ロジックが実際に機能していることも実機で確認できた。
  
  **当初の既知の限界とその解消**: 実機検証時点では`debug_fault`がグローバルなプロセス単位の
  フォルト注入であり、rebind先のセルラーソケットも同じ`shared_injector()`を使うため、CUT状態
  のままだとrebind後の新パスも同じく塞がれてしまい、テスト中は最終的にセッションがタイム
  アウトで切断される、という制約があった。ユーザーから「プロセスグローバルではなく、部分障害
  をエミュレートするように検証手法を改善できないか」と指摘を受け、`UdpFaultInjector`を
  プロセスグローバルな`debug_fault::shared_injector()`ではなく`establish_multipath_connection`/
  `RebindRequest`の引数として明示的に受け渡す設計に変更した（本番経路は引き続き
  `shared_injector()`をデフォルトで使うため挙動は変わらない）。これによりテストごとに独立した
  `UdpFaultInjector::new()`インスタンスを「WiFi用」「セルラー用」に1本ずつ用意できるようになった。
  新設した`session_survives_rebind_when_only_current_path_is_cut`テストでは、WiFi相当のinjector
  だけをCUTして`NoViablePath`発火を確認した後、CUTされていない独立したセルラー相当のinjectorで
  新しいloopbackソケット（127.0.0.6）へ`spawn_rebind_listener`経由でrebindし、rebind後の新しい
  bi-directionalストリームでecho往復が成功することまで、実機の物理的な部分障害無しにloopbackだけ
  で実証した（`cargo test -p tssh-core --lib multipath_transport -- --test-threads=1`で全20件
  pass）。

**結論**: `rebind_abstract()`＋QUIC自身の無応答検知という設計は、loopbackと実機の両方で
「検知→rebind発火」の一気通貫を実証できた。誤検知防止（実際のネットワークジッタでは
発火しない）も実機で確認済み。さらにテスト手法をプロセスグローバルなフォルト注入から
独立injector方式へ改善したことで、「rebind後に本当に生存している別経路へ切り替われば
セッションが継続する」ところまでloopbackで実証できた。残る確認は本物の物理的な部分断
（本物のカフェのWi-Fi等）での最終検証のみ。

**injector-threadingリファクタ後の実機再確認（同日追加）**: テスト手法改善（`UdpFaultInjector`を
引数として明示的に受け渡す設計変更）が本番経路（`try_connect_multipath`/`rebind_to_fd`は
引き続き`shared_injector()`をデフォルト使用）に影響していないことを、同じ実機
（Xperia XQ-DQ44、Tailscale OFF、「直接通信」プロファイル）でCUT broadcastを再度実行して確認した。
`path Secondary got zero responses for 5 consecutive checks` → `no viable path left` →
`session: no viable path` → `upstream failover: rebinding to cellular (localIp=10.116.141.105)` →
`rebind to local_ip=10.116.141.105 succeeded`——リファクタ前と同一の一気通貫をlogcatで再確認できた。
なお今回はCUTしたままリバインド先（セルラー）も同一の`shared_injector()`で塞がれ続けたため
（実機検証固有の既知の制約、上述）、最終的にconnectionが完全に飢餓状態になり切断された
（loopbackの独立injectorテストとは異なり、実機では引き続きこの制約が残る）。CLEAR/RESTORE後、
Tailscale再接続・アプリ強制終了・フォルト注入解除まで実施し、デバイスは元の状態に復帰済み。

**解消済みの既知の課題（androidTest、Phase 9とは無関係の既存不整合）**: `app/src/androidTest/kotlin/tools/isekai/terminal/FakeSshGateway.kt`の`FakeOrchestrator`が`notifyNetworkLost()`を実装しておらず`compileDebugAndroidTestKotlin`が失敗していた問題を解消。`test`側の同名クラスにあったPhase/ConnPhase状態機械の再現をそのまま移植し、`compileDebugAndroidTestKotlin`のBUILD SUCCESSFULを確認済み。

---

## Phase 10: 多段SSH依存からのP2P移行（ProxyJump・STUN方式・relay方式）

### 位置づけ

Phase 7で確立した自作ヘルパー(isekai-helper)経由QUIC接続耐性は、「isekai-helperにどう到達するか」
がSSH経由到達アドレス（Tailscale経由・直接アドレス）に限られており、NAT越え（hole punching）
そのものは行っていなかった。Phase 10ではこれを次の3本柱で拡張した:

1. **ProxyJump（多段SSH、`ssh -J`相当）**: 対象ホストがNAT配下で直接到達できない場合の
   ブートストラップ用フォールバック経路として先に実装（他の2本柱の前提）。
2. **STUN+SSHランデブー方式のP2P**（`TransportPreference::IsekaiStunP2pQuic`）: relay無し。
   両者が自分自身のSTUN観測アドレスを調べ、既存のSSHブートストラップチャネルに相乗りさせて
   交換し、直接のUDP穴あけ（simultaneous open）を試みる。穴あけ不成立時のフォールバックは
   意図的に持たない（ユーザーが別のTransportPreferenceへ手動で切り替える運用）。
3. **MASQUE relay経由のP2P**（`TransportPreference::IsekaiLinkRelayQuic`）: 会社
   （seera-networks）が運用する`ISEKAI-link`のrelay基盤（`axum-masque-rs`の`bound-udp-server`）を
   使う方式。relayが常時経路に残るためNATの種類に依存しないが、relayサーバー・JWTが必要。
   STUN版と完全に独立したトランスポートであり、relay版はMASQUE(RFC 9298系だが実際は
   `bound-udp-server`独自の非標準capsuleプロトコル)を使うのに対し、STUN版はMASQUE/HTTP3/capsule
   を一切使わない単純な自作プロトコルである（両者の違いはユーザーからの質問にも回答済み）。

詳細設計は `/home/cuzic/.claude/plans/cheerful-mixing-summit.md` 参照。

### フェーズ分割

| # | 内容 | 成果物 |
|---|------|--------|
| 10--1a〜d | ProxyJump（多段SSH）: `transport::connect_via_jump_or_direct`（踏み台へ接続・認証→`channel_open_direct_tcpip`→ネストしたSSHハンドシェイク）、`ConnectionProfile`の踏み台設定、`helper_bootstrap.rs`の踏み台対応、`ProfileEditScreen`の踏み台UI | ✅ 全SSHブートストラップ経由トランスポート（`SshConfig`/`HelperQuicConfig`/`MultipathHelperQuicConfig`/`IsekaiStunP2pConfig`/`IsekaiLinkRelayConfig`いずれも）が`jump: Option<JumpConfig>`を持ち、`TSSHD_QUIC`以外の全トランスポートで踏み台を使える |
| 10-0a〜d | STUN+SSHランデブー方式のP2P: 共有`isekai-stun`クレート（RFC 5389 Binding Request/Response、XOR-MAPPED-ADDRESS）、isekai-helperの`--stun-server`/`--punch-peer`、simultaneous open probe送出、`isekai_stun_p2p_transport.rs` | ✅ ローカルsshd+モックSTUNサーバーに対する実e2eテスト（`full_stack_stun_bootstrap_quic_and_shell_command`）でSTUN問い合わせ→SSHブートストラップ→アドレス交換→probe送出→QUIC接続→shell実行までのフルスタック疎通を確認 |
| 10-0b〜d | MASQUE relay経由のP2P: `isekai-link-masque`クレート（`seera-networks/axum-masque-rs`のソースを直接読んで確定した非標準capsuleプロトコル：`0x11`=COMPRESSION_ASSIGN/`0x12`=ACK/`0x13`=CLOSE、`datagram_codec.rs`の`context_id + [ip_version+addr+port]? + payload`フレーミング）、`relay_client.rs`（agent役のCONNECT-UDP-bind確立・単一の非圧縮コンテキスト登録） | ✅ 単体テスト22件 + `tests/relay_e2e.rs`（後述） |
| 10-1 | isekai-helperに`--relay`起動モード追加 | ✅ `--relay`/`--relay-sni`/`--relay-jwt`（`--stun-server`/`--punch-peer`とは併用不可）。relay接続成功時、`--bind`する代わりに`isekai_link_masque::connect_relay_agent()`が返す`RelayUdpSocket`を`noq::Endpoint::server`の抽象ソケットとして使う。relayが割り当てた公開アドレスをハンドシェイクJSONの`relay_public_addr`に含める |
| 10-2a〜c | rust-core側トランスポート追加 | ✅ `isekai_stun_p2p_transport.rs`/`isekai_link_relay_transport.rs`。**relay版で判明した重要な単純化**: isekai-terminal(クライアント役)はMASQUE/HTTP3/capsuleを一切意識しない。relayとCONNECT-UDP-bindトンネルを張るのはisekai-helper(agent役)だけであり、isekai-terminalは`relay_public_addr`（SSHブートストラップのハンドシェイクJSON経由で受け取る、STUN版の`stun_observed_addr`と同じパターン）へ普通にQUIC接続するだけでよい——relayから見ればisekai-helperがその公開アドレスで直接listenしているのと区別が付かない |
| 10-2b | `TransportPreference`に`IsekaiStunP2pQuic`/`IsekaiLinkRelayQuic`追加、`orchestrator.rs`配線 | ✅ `helper_bootstrap.rs`に`HelperP2pMode`(`None`/`Stun`/`Relay`)enumを導入し、3方式が互いに排他であることを型で表現（isekai-helper側`--relay`と`--stun-server`/`--punch-peer`の併用不可というCLI制約と対応） |
| 10-3 | Kotlin側配線 | ✅ UniFFI再生成、`ConnectionProfile`（STUN版: `stunServer`、relay版: `relayAddr`/`relaySni`/`relayJwt`、Room migration 12→13→14）、`ProfileEditScreen`にチップ+設定UI、`TerminalSession`/`TerminalTabsViewModel`配線、`FakeOrchestrator`（test/androidTest両方）更新 |
| 10-4 | JWT発行・配布フローの設計 | 🟡 MVP実装のみ完了（下記参照）。恒久的なフローは未設計・独立着手可能なまま |
| 10-5 | 実機検証・PLAN.md記録 | 🟡 プロトコルレベルのe2e検証（下記）は完了。**物理2ネットワークでの実機検証は未実施**（後述） |

### relay版のローカルe2eテスト（`isekai-link-masque/tests/relay_e2e.rs`）

**本物の`bound-udp-server`のローカルビルドは断念した**: `axum-masque`のデフォルトfeatureが
`msquic-async`(`h3-msquic-async`)を要求し、これは`msquic`(C++、cmake必須)を自動ビルドする。
実際に試みた結果、`msquic`本体のビルドまでは到達したが、そのvendored `quictls`(OpenSSLフォーク)
submoduleが未チェックアウトでビルドが失敗し、これを解消してもさらに深いネイティブビルド依存
（cmake/perl/C++ツールチェーン）が続く見込みだったため、「型チェックが通れば動くはず」ではなく
「実際に動かして検証できる」ことを重視するこのプロジェクトの方針に照らし、**別の検証手段に
切り替える判断をした**（コスト対効果が見合わないと判断——このローカル環境でのネイティブビルドの
成否確認自体に無制限の時間を投じるべきではないという判断）。

代わりに、`axum-masque-rs`のソース（`bound_udp/service.rs`・`main.rs`）を直接読んで
ワイヤー契約（wildcard CONNECT-UDP path、`connect-udp-bind`/`capsule-protocol`/
`proxy-public-address`ヘッダ、capsuleバイト列、datagramフレーミング）を確定させた上で、
**同じ契約を実装するプロトコル正確なモックrelayを`h3-noq`上に構築**し、これに対して
`relay_client.rs`が実際に：

1. CONNECT-UDP-bindリクエストを正しいメソッド/拡張/ヘッダ/JWTで送る
2. capsuleハンドシェイク（COMPRESSION_ASSIGN→ACK）を完了する
3. 実際のQUIC datagram経由でUDPペイロードを転送する
4. その`RelayUdpSocket`を`noq::Endpoint::server`の抽象ソケットとして使い、
   **完全に別プロセス相当の2つ目のnoq clientエンドポイント**（isekai-terminal役）が
   relay越しに実際のQUICハンドシェイク＋双方向ストリーム交換を成功させる

ところまでをe2eテストで確認した（`full_tunnel_round_trips_real_quic_traffic_through_the_relay`）。
この過程で2件の実バグを発見・修正:

- **`h3-noq`自身のバグ**: `RecvStream::recv_id()`が`self.stream.as_ref().unwrap()`していたが、
  `poll_data()`の`read_chunk_fut`実行中は`self.stream`が一時的に`None`になる。h3-datagramの
  stream-id lookupがこの窓で`recv_id()`を呼ぶとpanicする。stream_idをフィールドとして
  別途キャッシュする修正で解消（h3-noqの既存smoke testでは検出されなかった、より複雑な
  「同一stream上でcapsule往復とdatagram往復が並行する」シナリオで顕在化した）。
- **`noq`の`initial_mtu`既定値(1200、QUIC仕様上の最小保証値)では、転送するQUIC Initialパケット
  (それ自体が~1200byteに達する)にこのcrate独自のcontext_id/addrプレフィックスを足すと
  单一QUICパケットに収まらない**。MTU discoveryが十分に上がるまで待たず、`initial_mtu(1500)`
  を明示設定することで解消（`relay_client.rs::uplink_transport_config()`、実運用のネットワークも
  ほぼ全てEthernet MTU相当をサポートするため安全）。

### JWT発行・配布フロー（Phase 10-4、MVP実装のみ）

現時点ではプロダクト/運用判断の要素が大きく恒久的なフローは未設計というオリジナル計画の通り、
**MVPとしてはJWT文字列をプロファイル設定にそのまま貼り付ける方式**にした
（`ConnectionProfile.relayJwt`、`ProfileEditScreen`のテキスト入力欄）。rust-core側
（`isekai-helper`の`--relay-jwt`、`isekai-link-masque`の`connect_relay_agent`）はBearerトークンの
文字列を受け取るだけで、その取得方法には一切関知しない設計にしてあるため、将来どの配布方式を
選んでも rust-core 側の変更は不要——Kotlin側でトークンを取得・更新するロジックを追加し、
`ConnectionProfile.relayJwt`に書き戻すだけで良い。将来検討すべき配布方式（優先順は未確定、
実装時に改めて相談）:

- **デバイスコードフロー**（Auth0の該当エンドポイント使用、ユーザーがブラウザで一度だけ認可）。
  TVアプリ等で一般的、モバイルでもQRコード表示等でUXを補える。
- **アプリ内蔵OAuthクライアント**（Custom TabsでAuth0の認可画面を開き、コールバックURLで
  トークンを受け取る）。よりネイティブなUXだが、Auth0テナント側にモバイルクライアント登録が必要。
- **`isekai-ssh init`相当の初期セットアップコマンド**（`ISEKAI_SSH_DESIGN.md`で新規CLIツール
  `isekai-ssh`向けに検討済みの設計を流用）。
- いずれの方式でも、JWTの有効期限切れ・リフレッシュの扱い（rust-core側は現状リフレッシュを
  一切行わない——`--relay-jwt`は起動時に一度渡すだけ）をどう補うかは併せて設計する必要がある。

### 実機検証（Phase 10-5、未実施）

Phase 9-4と同様の理由（このセッションにAndroid実機が接続されていない）で、以下は未実施のまま:

- STUN版: 異なる2ネットワーク（宅内Wi-Fi NAT配下のisekai-helperホスト、モバイル回線のAndroid端末）
  でのhole punching成立/不成立シナリオの実機確認。
- relay版: 実際の`seera-networks`運用relayに対する接続確認（このセッションでは
  プロトコル正確な自作モックrelayでの検証のみ）。
- 両トランスポートともKotlin側の`ProfileEditScreen`実UI操作での実機確認。

次回実機がある環境で、Phase 7-5/8-4/9-2の実施パターン（`phase7-5-roaming-test.sh`等）を
踏襲した検証が必要。

対象外（Phase 10の範囲では）: relay版のJWT恒久配布フロー実装（Phase 10-4、上述の通り独立着手可能）、
relay版の実機・実relayでの疎通確認。

### Phase 10-5 実機検証結果（2026-07-04、実機（Xperia XQ-DQ44）とWindows PC間のUSBデバッグ接続復旧後）

Windows PC経由でadb USB接続が確立したのを受け、`adb tcpip 5555`→このdev boxのTailscale IP経由
`adb connect`で実機に到達し、直近（quinnからnoqへの移行含む）の変更点を優先順位付けして実機検証した。

**Tier A（quinn→noq移行の回帰確認）**: 既存の「Tailscale経由」プロファイル
（`ISEKAI_HELPER_QUIC_MULTIPATH`）で接続し、logcatで`multipath_quic`関連ログと実際のシェル
往復を確認。quinn依存を除去する一連のコミット（本日付、`d28f1ab`等）後もこのdev box上の
isekai-helper起動・QUIC接続・シェル動作に regression がないことを確認した。

**ProxyJump（多段SSH）実機初検証（成功）**: 「ProxyJump-Test」プロファイル（踏み台ホスト＝
target hostと同一の`100.100.45.36`を指定する「同一ホスト踏み台トリック」——2台目のSSHサーバーを
用意せずにトンネル機構そのものを検証する手法）を`ProfileEditScreen`から実際にUI操作で作成し接続。
logcatで踏み台向け・target向けの2回のSSHハンドシェイク/ホスト鍵検証/鍵復号イベントを確認し、
`echo proxyjump-tunnel-ok`の実インタラクティブ往復が成功した。ProxyJump機能の実機での動作は
これが初めての確認。

**STUN+SSHランデブーP2P実機初検証（成功、実際のNAT hole punchingを確認）**: 「StunP2p-Test」
プロファイル（`ISEKAI_STUN_P2P_QUIC`、STUNサーバー欄は空欄＝`DEFAULT_STUN_SERVER`
（`stun.l.google.com:19302`）使用）を作成・接続。実機側logcatとdev box側(`~/.cache/isekai-terminal/helper.log`)
の両方で以下を確認した:

- 実機: `isekai_stun_p2p: our observed address is 122.209.120.129:44913 (via 74.125.250.129:19302)`
  （実機はこの時点でWi-Fi＝自宅ルーター経由がTRANSPORT_PRIMARYであり、STUNクエリはこの経路の
  ISP側NAT公開アドレスを返した。実機は同時にモバイル回線(LTE)も接続していたが、デフォルト
  ルートはWi-Fi側だったため今回の観測アドレスはWi-Fi経由のもの）
- 実機: `isekai_stun_p2p: peer observed address is 204.12.203.210:54471` → `HELLO/ACK ok — handing off to SSH`
  → `control stream established (resume support enabled), session_id=1fc4153532e8f0d583c783cecb8adf1a`
- dev box: `punch: sending hole-punch probes to 122.209.120.129:44913` →
  `QUIC connection established from Some(122.209.120.129:44913)` →
  `control stream established, session_id=1fc4153532e8f0d583c783cecb8adf1a`（実機側と完全一致）
- 実際のインタラクティブシェルで`echo stun-p2p-hole-punch-ok`往復が成功。

実機・dev box双方のログでsession_idが一致し、実際のUDP hole punchingが成立してQUIC接続が
確立したことを確認できた（relayを一切介さない直接P2P接続の実機初検証）。なお今回はWi-Fi経由
（自宅ルーターNAT配下）での検証であり、「モバイル回線（セルラー）経由での穴あけ」は未検証のまま
（実機のWi-Fiを完全にOFFにする必要があり、この検証に使っていたadb接続自体がTailscale
（Wi-Fi経由）に依存していたため、セッション中断のリスクを避けて見送った）。

**ポートフォワード回帰確認（成功）**: 「StunP2p-Test」プロファイルを一時的に`通常SSH`
トランスポートへ切り替え、ローカルフォワード（待受127.0.0.1:28080 → 転送先127.0.0.1:18080）を
`ProfileEditScreen`から追加して接続。dev box側に`python3 -m http.server 18080 --bind 127.0.0.1`を
起動し、`adb forward tcp:29999 tcp:28080`でこのdev boxからも実機のフォワード待受ポートに到達
できるようにした上で`curl http://127.0.0.1:29999/index.html`を実行したところ、実機の
SSHポートフォワード経由でdev boxのHTTPサーバーからレスポンス（`port-forward-test-ok`）を
正しく受信できることを確認した。

**未実施のまま残った項目**:
- relay版（`ISEKAI_LINK_RELAY_QUIC`）: 実relay・JWTへのアクセスが無いため今回も未検証のまま。
- STUN版のモバイル回線（セルラー）経由での穴あけ検証: 上述の理由（adb接続経路への影響回避）で見送り。
- Phase 9-4（物理Wi-Fi/セルラー同時マルチパス）・9-4b（rebindによるupstream failover）は、
  前回セッション（2026-07-03）で既に実機検証済み・結論確定済み（9-4はnoq側の不具合
  [issue #738](https://github.com/n0-computer/noq/issues/738)により現状実現不可、9-4bは
  rebind成功を2度実機確認済み）のため、今回は再検証していない。

検証に使用した一時的な鍵・プロファイルは検証後にクリーンアップ（dev box側`~/.ssh/authorized_keys`
を検証開始前の状態に復元）。実機側に作成したテスト用プロファイル（ProxyJump-Test/StunP2p-Test/
Tailscale経由/直接通信）はユーザーの今後の実機検証の参考用にそのまま残してある。

### Phase 10 完了後の外部レビュー（ChatGPT相談、2026-07-03）

直近24時間の変更（quinn→noq移行、SSH agent forwarding、port forward、マルチタブ、Phase 10の
ProxyJump/STUN P2P/MASQUE relay）をまとめてChatGPTに相談した。総評は「個別実装は健全だが、
Phase 10時点で『トランスポート実験が同時に増えすぎて境界が曖昧になり始めている』」というもの。
noq issue #738（`local_ip`明示指定パスでのPATH_RESPONSE未達によるvalidation failed）は
Web検索で実在・現状Needs Triageであることを確認済み——Phase 9-4ブロッカーとして扱って良い。

次フェーズ着手前に反映すべき指摘（実装はまだ行っていない。次セッションでの着手候補として記録）:

**P0（Phase 11で着手予定）**
1. ✅ `relay_jwt`平文保存を「MVP限定TODO」ではなく明示的なsecurity debt issueとして扱う
   → [issue #1](https://github.com/cuzic/isekai-terminal/issues/1)を起票し、`ConnectionProfile.kt`の
   コメントから参照するよう変更（Phase 12でのKeystoreKekベースvault移行までcloseしない）
2. ✅ STUN/Relayの「フォールバックなし」を内部実装のままにしつつ、ユーザー向けには
   Strict Isekai Link / Smart Connect / Plain SSHのような接続ポリシーとして明示する案
   → `ProfileEditScreen.kt`の「接続方式」セクションを、実際のRust側フォールバック挙動
   （`TransportPreference`各バリアントのdocコメント）に基づき3グループに再編:
   Plain SSH（通常SSH/tsshd QUIC、フォールバック概念自体が無い）／
   Smart Connect（推奨）（`Auto`のみ、ヘルパーQUIC失敗時に実際にplain SSHへ自動フォールバック
   する唯一の方式）／Strict Isekai Link（実験的・フォールバックなし）（自作ヘルパーQUIC・
   マルチパス・STUN P2P・relay P2Pの4方式、すべて明示的にフォールバック無し）。
   各グループの直下にポリシーレベルのキャプションを追加し、個別方式の詳細キャプションは維持。
3. ✅ noq #738により物理マルチパスがunavailableであることをUI/docs/testsに反映
   → 調査の結果、影響範囲は「自作ヘルパー QUIC（マルチパス）」チップ全体
   （Tailscale⇔直接アドレスのpath0/path1、`open_path(local_ip=None)`）ではなく、
   その内側の「Wi-Fi/セルラー物理無線も同時に使う」チェックボックス（Phase 9-4、
   `open_path(local_ip=Some(..))`）だけが noq #738 の対象と判明したため、後者のみ
   ラベルを「（現在利用不可）」に変更しキャプションを状態/原因/フォールバック先を
   明示する形に書き換えた（`ProfileEditScreen.kt`）。チップ自体の改名は不要と判断
   （path0/path1のTailscale⇔直接アドレス切替は#738の対象外で実際に動作するため）。
4. ✅ 非ループバックport forward bindをRust側（`SshConfig`に`allow_non_loopback_forward_bind: bool`
   のような明示許可フラグ）でも制御できるようにする（現状Kotlin UI警告のみでコア側allowlistなし）
   → `SshConfig.allow_non_loopback_forward_bind: bool`（既定false）を追加し、唯一の実体験装
   （`transport.rs::run_ssh_channel_loop`の`AddLocalForward`ハンドラ、全transportがここを
   共有）でbind_addressがループバック（127.0.0.0/8・::1・"localhost"）でない場合に
   `ForwardState::Failed`で拒否するよう変更（Rust SSOTルール準拠）。QUIC系5トランスポート
   （quic/multipath/stun_p2p/link_relay/helper_quic）はagent_forwardと同様にfalse固定
   （Config構造体がforwards自体を持たないため）。Kotlin側は`ConnectionProfile`に同名列を追加
   （Room migration 14→15）、`ProfileEditScreen`に「同一Wi-Fi/LAN上の他端末からの待受を許可する」
   チェックボックスを追加し、OFF時は拒否される旨を警告文言に反映。Rust側新規ユニットテスト2件
   （ループバック判定・非ループバックbind拒否のe2e）、Kotlin側新規migrationテスト1件を追加。
5. ✅ Room migration番号の並行worktree衝突（この24hで3件のfixupコミット発生）に対し、
   migration予約ファイル+CI重複チェックのような仕組みを検討
   → `app/migration_registry.toml`（現行版数`current`+未マージの予約`[[reserved]]`一覧）、
   `scripts/reserve-room-migration.sh <owner-slug>`（次の版数を予約してファイルへ追記）、
   `scripts/check-room-migrations.sh`（`AppDatabase.kt`の版数とregistryの`current`一致・
   `Migration(X, Y)`チェーンが1→currentまで欠番/重複無し・マージ後の`[[reserved]]`消し忘れ、
   の3点を検証）を追加。CI(`.github/workflows/room-migration-check.yml`)で
   `AppDatabase.kt`/`migration_registry.toml`変更時に自動実行。`CLAUDE.md`にも運用ルールを追記し、
   今後の並行作業(他エージェント含む)が予約せず直接番号を使わないよう周知した。

**P1（設計候補、Phase 12以降）**
- `h3-noq`/`isekai-link-masque`/noqの型を上位に漏らさない`isekai-transport` trait層の切り出し
  （`isekai-protocol`/`isekai-trust`との境界分離も含む、**他エージェントが作業中のため着手しない**）
- ✅ relay_jwtの平文保存自体は解消（issue #1 close）: `RelayCredentialVault`
  （`KeystoreKek`＝秘密鍵と同じAndroid Keystore由来AES/GCMを再利用）でRoomには暗号化(Base64)
  して保存し、`AppExecutor.decryptRelayJwt`(接続直前)・`ProfileEditScreen`の
  `encryptRelayJwt`/`decryptRelayJwt`引数(既定は`RelayCredentialVault`、テストは恒等関数に
  差し替え。`AndroidKeyStore`はRobolectricで使えないため`applyTerminalTheme`と同じ注入
  パターンを採用)でのみ平文化する。`toIsekaiLinkRelayConfig`自体は暗号化を意識しない純粋な
  マッピング関数のまま維持。実機テスト`RelayCredentialVaultTest`を追加。
  残作業(未着手、P1として継続): access_jwt短命化・メモリのみ保持、refresh/device token
  発行・revoke/rotateという、relay認可サーバーの実装を前提とした本格的なcredential vault設計。
- ✅ Phase 9-4を正式機能ではなくexperimental feature flagへ格下げ
  → `app/build.gradle.kts`に`BuildConfig.ENABLE_EXPERIMENTAL_PHYSICAL_MULTIPATH`を追加
  （defaultConfigでtrue・releaseビルドでfalseに上書き）。`ProfileEditScreen`の
  「Wi-Fi/セルラー物理無線への同時マルチパス」チェックボックス・キャプション・
  セルラー用別リモートアドレス欄一式をこのフラグでガードし、一般ユーザー向けの
  リリースビルドでは非表示に、debugビルド(開発・実機検証用)では引き続き表示するように
  した。noq #738の影響を受けない「自作ヘルパー QUIC（マルチパス）」チップ本体（path0/
  path1のTailscale⇔直接アドレス切替）と、Phase 9-4bのupstream failover機能
  （`enableUpstreamFailover`、rebind_abstract経由で実機確認済みの別機能）は対象外。

**P2（ユーザー優先度判断: 1→2→3→4の順）**

1. ✅ per-session/per-hostのterminal theme（現状グローバルstatic THEME）
   → `ThemeDefinition`(グローバル/DB管理)・`ThemeResolved`(session/tabごとのimmutable
   snapshot)・ANSI→RGB解決はRust側`SessionOrchestrator`配下、という設計方針の通りに実装。
   Rust側: `Terminal`が`theme: Theme`フィールドを持ち構築時に受け取る(グローバルを都度
   読みには行かない)。`SessionCore`が`current_theme`(接続=タブ作成時にグローバル既定を
   スナップショット)を保持し、`set_theme()`で`SessionCmd::SetTheme`経由でTerminalへ
   反映(以降にパースされるSGRにのみ反映、既存の制約を維持)。全6トランスポート(Ssh/
   Quic/HelperQuic/MultipathHelperQuic/IsekaiStunP2p/IsekaiLinkRelay)に同じ委譲
   メソッドを追加し、`SessionOrchestrator::set_session_theme(ansi16, defaultFg,
   defaultBg)`として統一公開。
   Kotlin側: `ConnectionProfile.themeName`(Room migration 15→16、null=グローバル既定に
   従う)を追加。`TerminalTabsViewModel.TabState`が`currentTheme`(StateFlow)・
   `isThemeOverridden`を持ち、Global default → Profile default → Tab/session override
   の3段階を解決。`applyGlobalThemeToNonOverriddenTabs`でグローバル変更を非上書きタブに
   だけ伝播(MainActivityのProfileListScreen呼び出し経由)。`TerminalHostScreen`のタブ
   ラベルに🎨ボタンを追加しタブ個別上書きが可能。`ProfileEditScreen`にプロファイル
   単位のテーマ選択(FilterChip、「既定に従う」+プリセット4種)を追加。
   Rust新規テスト2件(構築時テーマ・set_theme後方影響のみ)、Kotlin新規テスト7件
   (ViewModel4件・ProfileEditScreen3件)を追加。
2. Remote/Dynamic port forward（plain SSH限定、LocalForwardRunner/RemoteForwardRunner/
   DynamicSocksForwardRunnerに責務分離、クライアント側は非ループバックbindを既定拒否）
3. ✅ QUIC系トランスポートでのagent forwarding対応は設計のみ固めて実装は後回し
   （実装は着手しない。以下は将来着手する際の設計メモ）

   **現状**: agent forwardingは`rust-core/src/agent_forward.rs`にrusshのchannelベースで
   自前実装した最小サブセット(`REQUEST_IDENTITIES`/`SIGN_REQUEST`のみ)で、plain TCP SSH
   トランスポート(`run_russh_transport`)のみが対応。5種類のQUIC系トランスポート
   (自作ヘルパーQUIC・マルチパス・STUN P2P・MASQUE relay・tsshd QUIC)は全てConfig構造体
   自体に`agent_forward`フィールドが無く、常に無効(Phase 11 P0-4 委譲メソッド追加時に
   確認済み)。

   **拡張ポイント**: `HELPER_PROTOCOL.md`にはPhase 7時点で「1 QUIC connectionにつき
   data stream 1本のみ、それ以外のstreamは将来のポートフォワード拡張用に予約」という
   記述があり、Phase 8でこれを実際に「data stream + control stream」の2本構成へ拡張した
   前例がある。agent forwardingもこの前例に倣い、新しい**agent forward stream**を
   isekai-helperプロトコルに追加し、`max_concurrent_bidi_streams`をさらに1つ増やす形で
   拡張するのが自然。ワイヤーフォーマットは`agent_forward.rs`の既存の最小サブセット
   (`REQUEST_IDENTITIES`/`SIGN_REQUEST`)をそのままこのstream用に再利用できる見込み
   (russh固有のchannel機構ではなく、isekai-helperの独自フレーミングに載せ替えるだけ)。

   **セキュリティ方針**: agent forwardingは秘密鍵そのものは送らないが、リモート側に
   「署名オラクル」を渡す機能である点は既存のplain SSH実装でも変わらない。既定OFF・
   プロファイル単位opt-inという既存方針を維持しつつ、経路ごとに差をつける:
   - 自作ヘルパーQUIC(直結、relayを介さない): 将来的にopt-in拡張候補。
   - STUN P2P / MASQUE relay: 第三者が運用するrelayサーバーが経路に常時介在する
     (relay版)、またはNAT越え成立に依存する不安定な経路である(STUN版)ため、
     署名オラクルを転送する対象としては既定OFF・当面対象外のままとする。将来的に
     許可する場合も、通常より強い警告文言を必須にする。

   **優先度**: 実装コスト(HELPER_PROTOCOL改版・5 Config構造体拡張・UniFFI再生成・
   UI警告)に対して利用頻度が限定的と見込まれるため、当面は設計メモのみでよい。
4. ✅ msquic/channel-masque sidecar実験は今は着手しない（parking lot、実装は行わない）

   **現状で十分な理由**: 自前relay(`isekai-link-masque`)とのMASQUE通信は既にh3-noq
   (noq上に自前実装したHTTP/3+MASQUE)で完結しており、外部のmsquic/channel-masqueベースの
   relay実装と相互運用する必要が今のところ無い。実際、Phase 10のrelay版e2eテスト
   （`isekai-link-masque/tests/relay_e2e.rs`）を書く際、本物の`bound-udp-server`
   （msquic依存の`axum-masque`）のローカルビルドはcmake/quictls(OpenSSLフォーク)submodule
   未チェックアウト等のネイティブビルド依存が深く、コスト対効果が見合わないと判断して
   断念した経緯がある（上記「relay版のローカルe2eテスト」参照）。同じ理由が
   msquic/channel-masque sidecarの新規実装にもそのまま当てはまる。

   **方針**: noq単一スタック継続で合意済み(前回外部レビューでも一致)。msquic/channel-masque
   が必要になっても同一プロセス共存は避け、必要になった時点でsidecarプロセスとして
   隔離する方針自体は維持するが、具体的な実装(IPC設計・isekai-helperのbootstrap配布
   フローとの共存)は着手条件が発生するまで行わない。

   **着手条件**(いずれか発生時に再検討):
   - 外部の(自作でない)MASQUEサーバー実装との相互運用要件が発生した
   - noq/h3-noqでは仕様上どうしても実現できないMASQUE機能が必要になった
   - 商用relay提供者・パートナーがmsquic前提の実装を要求してきた

   **着手する場合のIPC方針**: まずUnix domain socketで隔離する(共有メモリはIPC設計が
   固まった後の性能最適化段階の話であり、最初のsidecar境界には時期尚早)。

判断が割れなかった点（両者一致）: noq単一スタック方針の継続は妥当、フォールバックなし設計自体は
セキュリティ的に正しい、Room migration番号体系（線形バージョニング）自体は変えない。

---

## Phase X: SSH agent forwarding — DESIGN.md の「やらないこと」から方針転換（ユーザー承認済み）

`DESIGN.md` は当初エージェント転送を対象外としていたが、ユーザー承認のもと追加した。

- 秘密鍵は既存の認証フロー（`KeystoreKek` で復号 → `SshAuth::PublicKey{private_key_pem}` →
  `PrivateKey::from_openssh()`）で得られる `PrivateKey` をそのまま共有する。鍵の追加受け渡しは不要。
- russh 0.48 はトランスポート配管をビルトインで持つ（クライアント側 `channel.agent_forward(true)`、
  サーバーが開き返す転送チャネルは `Handler::server_channel_open_agent_forward`）。
- agent ワイヤープロトコル（`REQUEST_IDENTITIES`/`SIGN_REQUEST` のみの最小サブセット）は
  `rust-core/src/agent_forward.rs` に自前実装した。`russh_keys::agent::server::serve()` は
  空の `KeyStore` 前提で「常に1本の既存鍵だけを提供する」用途に合わないため流用していない。
- セキュリティ設計: 既定 OFF・プロファイル単位 opt-in（`ConnectionProfile.enableAgentForward`）。
  署名要求（SIGN_REQUEST）ごとに必ずユーザー確認を挟む
  （`TransportEvent::AgentSignRequest` → `SessionCallback`/`OrchestratorCallback` の
  `on_agent_sign_request` → Kotlin 側は `TerminalSession.onAgentSignRequest()` が
  `RealHostKeyChecker.check()` と同じ「呼び出し元スレッドをブロックして待つ」パターンで
  ダイアログの応答を待つ）。タイムアウト（Rust 側 30 秒／Kotlin 側 25 秒）は拒否扱い。
- 現時点では plain TCP SSH transport（`run_russh_transport`）のみ対応。QUIC 系 transport
  （`quic_transport.rs` の `QuicConfig` 経由）はまだ `agent_forward` フィールドを持たず、
  常に無効（今後 QUIC 系プロファイルにも広げる場合は `QuicConfig` に同フィールドを追加する）。

---

## 実装順序

```
✅ Phase 0A: TransportCommand/Event/Handle + RusshTransport task 化
✅ Phase 4A-0: trzsz detector golden test（TrzszFilter ベース、12 tests pass）

【次】Phase 4A: TrzszTransferFsm
  - timed-fsm 依存追加
  - TrzszTransferFsm 実装（TimedStateMachine）
  - golden test を TrzszTransferFsm ベースに書き直す
  - TokioTimerRuntime / TrzszEffectExecutor
  - SessionCmd チャンネル追加、session_event_loop に統合
  - UniFFI メソッド / SessionCallback コールバック追加
  - バインディング再生成

Phase 4B: upload MVP（4A 完了後）
Phase 4C: download MVP（4A 完了後、4B と並行可）
Phase 4D: 実機回帰

[判断ゲート: TCP SSH 5G/WiFi 実測]

Phase 5A: Spike
Phase 5B: tsshd MVP

[Phase 6: SSH3 / Remote Terminal over HTTP/3 は検討の末に不採用（詳細は上記 Phase 6 節）]

Phase 7-0: helper の CLI/プロトコル契約を確定
Phase 7-1: 自作ヘルパーバイナリ最小実装（quinn サーバー + stream単位中継、HMAC認証）✅
Phase 7-2: x86_64/aarch64 musl クロスビルド ✅
Phase 7-3: SSH 経由ブートストラップ配布ロジック（起動管理・fallback含む）✅
Phase 7-4: TransportPreference 設計 + ActiveSession 統合 ✅（Rust 側。Kotlin UI 統合は別途）
Phase 7-5: 実機ローミング耐性検証（拡充版）
Phase 7-6: Linuxbrew tap 作成（低優先度）

[Phase 7 完了後、必要性を再評価してから着手]

Phase 8-0: resume プロトコル契約の確定
Phase 8-1: helper 側 output buffer 実装
Phase 8-2: Android 側 input replay buffer 実装
Phase 8-3: reattach ハンドシェイク実装
Phase 8-4: 実機検証（長時間圏外・大量出力中切断・keepalive境界）
```

---

## リスク表

| リスク | 対策 |
|--------|------|
| trzsz 検出の誤認識 / chunk 境界 | golden test（8 ケース、12 tests pass）|
| 転送タイムアウトで端末が壊れる | timed-fsm の Transfer timer → Recovering、VTE 自然回復 |
| OOM（大容量ファイル） | SAF chunk stream、Rust 側も一括保持しない |
| 古い BottomSheet からの chunk 混入 | transfer_id を FSM 側で生成、Kotlin は受け取るだけ |
| Transport 書き込み失敗・再接続中 | TransportCommand channel でバッファリング |
| timed-fsm の StepCoro を誤用 | 使わない。TimedStateMachine trait のみ |
| tsshd が仕様変更 | tag 固定、互換テスト |
| tsshd 暗号パケット独自解釈 | Go 実装完全追従 |

---

## 参照

- HELPER_PROTOCOL.md: isekai-helper の CLI / ワイヤープロトコル契約（Phase 7-0 の成果物）
- cargo-zigbuild: https://github.com/rust-cross/cargo-zigbuild（zig を C クロスコンパイラ/リンカに使い、musl-gcc 等のシステムトゥールチェーン無しで musl static バイナリをビルドできる。`brew install zig && cargo install cargo-zigbuild` で導入）
- rust-core/scripts/build-isekai-helper-musl.sh: x86_64/aarch64 musl バイナリのビルド + sha256 記録スクリプト（Phase 7-2 成果物）
- seera-networks/ISEKAI-link（参考、Phase 7 には不採用）: https://github.com/seera-networks/ISEKAI-link
  — 同じ命名系統の別プロジェクト。msquic + HTTP/3 + MASQUE(CONNECT-UDP) + JWT 認証 + P2P/リレー
  自動フォールバックによる NAT 配下デバイスのリモート制御基盤。`h3::ext::Protocol::CONNECT_UDP`
  （h3 の標準サポート値）を使っており、Phase 6 で断念したカスタム `:protocol` パッチが不要な設計。
  Phase 7 は「自分の SSH サーバーに直接 QUIC 接続する」前提でスコープが大きく異なるため今は不採用だが、
  将来「NAT 配下の到達不能なサーバーにも接続したい」となった場合の技術候補として記録しておく。
- timed-fsm: `/home/cuzic/rust-nicola/crates/timed-fsm`（MIT, pure std）
- tsshd: https://github.com/trzsz/tsshd（MIT）
- trzsz-go: https://github.com/trzsz/trzsz-go（MIT）
  - detector: `trzsz/comm.go` の `LastIndex(output, "::TRZSZ:TRANSFER:")`
- quinn: https://github.com/quinn-rs/quinn
- Android Network.bindSocket: API 22+（FileDescriptor: 23+）
- SSH3_PROTOCOL_NOTES.md: SSH3 / Remote Terminal over HTTP/3 の調査記録（ABANDONED、Phase 6 不採用の経緯）
- oowl/quicssh-rs（参考、不採用）: https://github.com/oowl/quicssh-rs（QUIC↔TCP汎用トンネル。「素の sshd へ中継」という発想のみ Phase 7 に活用）
- russh-sftp: crates.io の `russh-sftp`（2.3.0、SFTP subsystem for russh）— Phase 7-3 のブートストラップ転送で使用検討
- Homebrew Formula Cookbook: https://docs.brew.sh/Formula-Cookbook（Phase 7-6 の linuxbrew tap 作成時に参照）
- XDG Base Directory Specification: https://specifications.freedesktop.org/basedir-spec/latest/（`~/.local/bin` 配置の根拠）
- RFC 9000 §9（Connection Migration）、§9.3.1-9.3.2（Peer/On-Path Address Spoofing）、§9.6（Server's Preferred Address）、§21.5（Request Forgery Attacks）: https://www.rfc-editor.org/rfc/rfc9000.html
- QUIC-Exfil 論文: arXiv:2505.05292（ACM ASIA CCS '25、DOI 10.1145/3708821.3733872）
- Marten Seemann, "Exploiting QUIC's Path Validation"（2023）: https://seemann.io/posts/2023-12-18---exploiting-quics-path-validation/
- Marten Seemann, "Exploiting QUIC's Connection ID Management"（2024）: https://seemann.io/posts/2024-03-19---exploiting-quics-connection-id-management/
- migration/exporter 実証実験コード: `/home/cuzic/ssh3/rust-quinn-spike/src/bin/migration_exporter_test.rs`（`cargo run --bin migration_exporter_test` で再現可）
- rust-core/src/faulty_udp_socket.rs: UDP データグラム層でのロス/遅延/完全断シミュレーション + `Endpoint::rebind()` によるネットワーク切替の自動テスト（Phase 7-5 成果物）。本番の `helper_quic_transport.rs` にも配線済みだが、既定値では素通しで挙動に影響しない
- rust-core/src/debug_fault.rs + app/src/debug/kotlin/.../FaultInjectionReceiver.kt: 実機での上記フォルト注入をライブに有効化する adb 経由のデバッグフック（release ビルドには含まれない）
- rust-core/scripts/phase7-5-roaming-test.sh: 実機ローミング耐性検証のシナリオ関数集（ライブフォルト注入 / 実ネットワーク切替 / 組み合わせ）
- quinn の rebind 前例: `quinn-0.11.11/src/endpoint.rs` の `Endpoint::rebind`/`rebind_abstract`、および quinn 自身のテスト `quinn-0.11.11/src/tests.rs: rebind_recv`（同じ手法の公式前例）
