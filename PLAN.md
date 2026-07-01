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
| 7-5 | 実機ローミング耐性検証（Wi-Fi⇔5G 切替に加え、alt screen 表示中・大量出力中・入力中・30分アイドル後・画面ロック復帰・helper↔sshd 切断・token 不一致拒否・trzsz 転送中の切替を含む拡充版） | 実機回帰チェックリスト |
| 7-6 | Linuxbrew tap 作成（`isekai-terminal/homebrew-tap`）— 優先度低、手動インストールしたい上級者向け fallback | `brew install` で導入可能 |

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
| 8-0 | resume プロトコルの契約を確定（session_id、reconnect token、bidirectional byte offset、app-level ACK のワイヤーフォーマット） | 設計ドキュメント |
| 8-1 | helper 側 output buffer（上限付き、backpressure 連動）の実装 | S→C 方向の resume が成立 |
| 8-2 | Android 側 input replay buffer の実装 | C→S 方向の resume が成立 |
| 8-3 | reattach ハンドシェイク（新しい QUIC connection から既存 resume セッションへの再接続）の実装 | QUIC connection 完全消失後も SSH セッションが継続する |
| 8-4 | 実機検証（長時間の圏外、大量出力中の切断、keepalive タイムアウト境界の確認） | 実機回帰チェックリスト |

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
