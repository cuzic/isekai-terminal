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

- timed-fsm: `/home/cuzic/rust-nicola/crates/timed-fsm`（MIT, pure std）
- tsshd: https://github.com/trzsz/tsshd（MIT）
- trzsz-go: https://github.com/trzsz/trzsz-go（MIT）
  - detector: `trzsz/comm.go` の `LastIndex(output, "::TRZSZ:TRANSFER:")`
- quinn: https://github.com/quinn-rs/quinn
- Android Network.bindSocket: API 22+（FileDescriptor: 23+）
