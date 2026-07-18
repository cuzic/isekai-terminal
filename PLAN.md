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
| 7-4 | `TransportPreference`（`PlainSsh`/`TsshdQuic`/`IsekaiHelperQuic`/`Auto`）を設計した上で `ActiveSession` へ統合（Phase 5 の `QuicSession` とは責務分離し、並列の `HelperQuicSession` を追加） | ✅ `rust-core/src/helper_quic_transport.rs` + `orchestrator.rs` の `ActiveSession::HelperQuic`/`connect_helper_quic`/`connect_helper_quic_auto` に実装済み。**実 sshd に対するフルスタック E2E テストで、SSH bootstrap → isekai-helper 起動 → QUIC 接続（証明書ピン留め + HMAC 認証）→ russh セッション確立 → 実シェルコマンド実行・出力受信までの全チェーンを確認済み**（`cargo test -p isekai-terminal-core --lib helper_quic_transport`）。Kotlin 側 UI（ProfileEditScreen 等への `TransportPreference` 選択肢追加）は未着手、別途フォローアップとする |
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
（`cargo test -p isekai-terminal-core faulty_udp_socket`、3 テストとも pass 済み）。

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
- `android/src/debug/kotlin/tools/isekai/terminal/debug/FaultInjectionReceiver.kt`: 上記関数を
  `adb shell am broadcast` から呼び出せる `BroadcastReceiver`。`android/src/debug` ソースセット配下の
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
       bind）、`android/src/androidTest/kotlin/tools/isekai/terminal/NoqDualFdMultipathSpikeTest.kt`
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

**Kotlin 側の第一歩は着手済み**: `android/src/main/kotlin/tools/isekai/terminal/session/NetworkPathMonitor.kt`
に `PathId`（`DIRECT`/`TAILSCALE`）・`PathState`（`UNKNOWN`/`PROBING`/`VALIDATED`/`DEGRADED`/`FAILED`/
`COOLDOWN`）と、`ConnectivityManager.NetworkCallback` を使ってネットワークレベルの到達可能性を追跡する
`NetworkPathMonitor` を実装済み。実機不要、Robolectric（`android/src/test/kotlin/tools/isekai/terminal/
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
`android/src/androidTest/kotlin/tools/isekai/terminal/NoqDualFdMultipathSpikeTest.kt`
（`android.permission.CHANGE_NETWORK_STATE` を `android/src/debug/AndroidManifest.xml` に追加済み、
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
| 8-4a | 実機不要な reject/失効パスのローカル検証 | ✅ `REJECT_UNKNOWN_SESSION`（未知の session_id / 実は resume 不可能）と `REJECT_OFFSET_GONE`（要求 offset が buffer 範囲外）を実際に発生させる e2e テスト2件を追加。`--idle-timeout` を短く設定して `sweep_expired_parked` が実際に発火し、期限切れセッションへの resume が `REJECT_UNKNOWN_SESSION` になることを確認する e2e テスト1件、および sweep が期限内/アクティブなセッションには触れないことを確認する unit テスト1件を追加。client 側は `ReattachableStream` が `REATTACH_MAX_RETRIES`(5回・指数バックオフ計15秒)を使い切った後に `Poll::Pending` を返し続けず実際の `io::Error` を russh に見せることを、`tokio::time` の仮想時間(`start_paused`)で確認する unit テストを追加（そのため `tokio` の `test-util` feature を dev-dependency に追加）。isekai-helper 15 tests / isekai-terminal-core 66 tests 全て pass |
| 8-4a' | Robolectric で検証可能な範囲の追加検証 | ✅ 既存の Robolectric 資産（`NetworkPathMonitorTest`＝shadow ConnectivityManager、`TerminalViewModelTest`＝QUIC 接続時は network-lost で切断しない、Room in-memory、Compose UI）を調査。ギャップだった `android/src/debug/kotlin/.../FaultInjectionReceiver.kt`（実機の `adb shell am broadcast` からフォルト注入する debug 専用 BroadcastReceiver）に `FaultInjectorApi` インターフェースを導入し native FFI 呼び出しを差し替え可能にした上で、`android/src/testDebug/kotlin/.../FaultInjectionReceiverTest.kt` を新設（8 tests: 5 action の intent→FFI 引数マッピング、extra 欠落時のデフォルト値、未知 action/null action で何もしないこと）。`KeystoreKek`/`KeyManager` は Android Keystore が Robolectric で emulate されないため引き続き実機(androidTest)のみ。`testDebugUnitTest` 214 tests 全 pass |
| 8-4b | 実機検証（長時間の圏外、大量出力中の切断、keepalive タイムアウト境界の確認） | ✅ Tailscale 経由 isekai-helper QUIC 接続で `debug_fault` の CUT/RESTORE を使い3シナリオを実施。**シナリオ1(完全断・修正前)**: client が接続喪失を検知するまで実測約43秒（QUIC idle timeout 未設定でサーバー側30秒設定に引きずられ + PTO再送）かかる一方、helper 側は同じ30秒で park セッションを破棄していたため、**reattach が5回とも必ず `REJECT_UNKNOWN_SESSION` になり毎回失敗する**致命的なタイミング不整合を実機でのみ発見（ローカルe2eは `conn.close()` による即時切断検知のため再現しなかった）。**修正**: client 側（`helper_quic_transport.rs`）に `keep_alive_interval`(5秒・NAT UDPマッピング維持)と短い`max_idle_timeout`(15秒)を追加。helper 側は `--idle-timeout`(QUIC transport 生存確認、既定15秒)と `--resume-window`(park セッション保持時間、新設)を分離。**修正後に再検証**: 検知が約19秒に短縮、reattach が2回目の試行で成功、reject 無し。**シナリオ2(大量出力中の切断)**: `seq 1 200000`(約20万行)実行中に CUT→RESTORE。reattach は接続不能な間の4回の試行がそれぞれ `--idle-timeout` と同じ長さ（quinn が handshake タイムアウトとして内部流用）だけブロックすることが判明し、5回全滅する最悪ケースの合計時間は指数バックオフの15秒ではなく**実測で約90秒**（既定 `--resume-window` 90秒とほぼ同値でマージンが薄いと判明）かかることを確認。5回目の試行(RESTORE後)で成功し `helper_committed_offset=3622` から再送・全20万行が最後まで正常に出力完了、その後のコマンドも正常応答。この実測を受けて `--resume-window` の既定値を **120秒**に引き上げ（isekai-helper v0.3.2、musl再ビルド・Androidアプリ再ビルド・実機で再確認済み）。**シナリオ3(keepalive境界)**: フォルト注入なしで100秒アイドル待機し、reattach/disconnect/reject が一切発生せず、待機後もコマンドが即座に応答することを確認。HELPER_PROTOCOL.md §1/§7.5 を全て更新済み。isekai-terminal-core 78 tests・isekai-helper 15 tests 全 pass |
| 8-4c | 実機待ちの間のリファクタ | ✅ Rust: `isekai-helper/src/main.rs` の `handle_resume_stream` で4箇所コピペされていた「TCP を park に戻す」処理を `repark()` ヘルパーに抽出。`rust-core/src/helper_quic_transport.rs` で HELLO/RESUME 双方にあった HMAC proof 計算の重複を `compute_proof()` に統一。`rust-core/src/resume_client.rs` の `poll_read`/`poll_write` で重複していた「reattach 起動 + waker 登録」を `begin_reattach_after_io_error()` に抽出。Kotlin: `ProfileListScreen.kt`/`KeyListScreen.kt` の削除確認 `AlertDialog` を `ui/ConfirmDialogs.kt` の `DeleteConfirmDialog` に共通化。`tsshd_port` のデフォルト値 2222 を `ConnectionProfile.DEFAULT_TSSHD_PORT` に定数化（`AppDatabase.kt` の Room migration 内のリテラルは歴史的記録のため意図的に据え置き）。6画面で直書きされていたダークテーマの色 hex を `ui/AppColors.kt` に集約。すべて振る舞い変更なし、isekai-terminal-core 66 tests・isekai-helper 15 tests・Android 214 tests 全 pass |
| 8-4d | Kotlin/Rust 境界レビュー: セッション状態の判断ロジックを Rust 側 SSOT に統一 | ✅ `TerminalSession.kt` のコメント「セッション状態の SSOT は Rust 側に持つ」に反し、`notifyNetworkLost()`（ハンドシェイク中/TCP接続中は切断、QUIC接続中は無視、という判断）が Kotlin 側のミラー状態(`_state`)を見て判断していた。`rust-core/src/orchestrator.rs` の `SessionOrchestrator` に `ConnPhase`（Idle/Connecting/Connected）を追加して SSOT を Rust 側に一元化し、`notify_network_lost()` を新設。Kotlin 側は生イベントを転送するだけの1行に縮小し、結果は既存の `onConnectionStateChanged` コールバック経由で反映される。UniFFI Kotlin bindings を再生成（`cargo run -p uniffi-bindgen -- generate --library target/debug/libisekai_terminal_core.so --language kotlin`）。`FakeOrchestrator`（テスト用）にも同じ判断ロジックを実装し直して既存テスト(`TerminalSessionTest`)を維持。isekai-terminal-core 66 tests・Android 214 tests 全 pass |

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
| 9-1 | `rust-core/isekai-helper/src/main.rs`のQUICリスナーを`quinn`→`noq`に移行。`max_concurrent_multipath_paths(8)`を有効化 | ✅ `quinn::`→`noq::`機械的置換（APIはほぼ1:1）。唯一の非互換点：`Connection::remote_address()`はmultipath化で無くなり、`Connecting`にのみ残存（確立後は`conn.path(PathId::ZERO).remote_address()`で代替、ログ用途のみ影響）。isekai-helper側15テスト（unit 8 + e2e 7）全pass。**isekai-terminal-core側66テストも全pass**（うち`helper_bootstrap::bootstraps_and_launches_helper_over_real_ssh`/`helper_quic_transport::full_stack_bootstrap_quic_and_shell_command`/`resume_survives_connection_cut`は実SSH・実sshd相手のopt-inテストで、これらも通過＝**新isekai-helper(noq)に対し無改造の既存quinnクライアントで実際にSSH bootstrap→QUIC接続→shell実行→Phase 8 resumeまでフルスタック疎通を確認**）。v0.3.0としてmuslバイナリ再ビルド・sha256更新済み（`build-isekai-helper-musl.sh`） |
| 9-2 | クライアント新規コード：`PathCandidateId`（Primary/Secondary）・二値`PathState`のbroker、`noq::Connection::open_path()`でpath1確立。`TransportPreference::IsekaiHelperQuicMultipath`として配線 | ✅ `rust-core/src/multipath_transport.rs`新設。**resume/reattach層は無し**——multipathは1コネクション内の複数pathなので、片方が生きている限りコネクション自体が死なず、Phase 8のような明示的再接続機構が不要という設計（PLAN.md本文参照）。`helper_quic_transport.rs`からHELPER_PROTOCOL.md契約（ALPN/フレーム定数/`PinnedCertVerifier`/埋め込みバイナリ/ブートストラップ関数）を`pub(crate)`化して再利用、Phase 7/8のコード自体は無変更。`open_path`は単発8秒待ちではなく3回リトライ+指数バックオフ（2s/4s/8s）。`TransportPreference::IsekaiHelperQuicMultipath`・`ActiveSession::MultipathHelperQuic`・`SessionOrchestrator::connect_multipath_helper_quic`を追加。loopback（127.0.0.1/127.0.0.2、同一noqサーバーへの別経路）でのunit test 3件（path0+path1確立、path1無し、**path0 close後もコネクションが生きてHELLO/ACK往復に応答できる**＝受動的フェイルオーバーの核心）全pass。isekai-terminal-core全69テスト（既存66+新規3）pass、無回帰 |
| 9-3 | Kotlin側配線：`ConnectionProfile.directAddress`追加、UI、uniffi再生成 | ✅ uniffi Kotlinバインディング再生成（`cargo run -p uniffi-bindgen`）。`ConnectionProfile.directAddress`（Room migration 4→5、DB version 5）、`ProfileEditScreen`に「自作ヘルパー QUIC（マルチパス）」チップ+直接到達アドレス入力欄、`TerminalViewModel`/`TerminalSession`/`SessionOrchestrator`経由で`connectMultipathHelperQuic`まで配線。`FakeOrchestrator`（test/androidTest 両方）に新メソッド追加。`ConnectionProfileRepositoryTest`にroundtrip確認2件追加。**Gradleでのビルド確認は開発環境のディスク枯渇（`No space left on device`）に一度遭遇したため`cargo clean --profile dev`でRust側の使い捨てビルドキャッシュ約10.7GiBを解放してから実施**。`:android:compileDebugKotlin`/`:android:testDebugUnitTest`とも成功、Kotlin側テスト226件（既存214+新規追加分）全pass、無回帰 |
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
（`android/src/main/kotlin/tools/isekai/terminal/session/PhysicalPathProvider.kt`）。

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
  で実証した（`cargo test -p isekai-terminal-core --lib multipath_transport -- --test-threads=1`で全20件
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

**解消済みの既知の課題（androidTest、Phase 9とは無関係の既存不整合）**: `android/src/androidTest/kotlin/tools/isekai/terminal/FakeSshGateway.kt`の`FakeOrchestrator`が`notifyNetworkLost()`を実装しておらず`compileDebugAndroidTestKotlin`が失敗していた問題を解消。`test`側の同名クラスにあったPhase/ConnPhase状態機械の再現をそのまま移植し、`compileDebugAndroidTestKotlin`のBUILD SUCCESSFULを確認済み。

### Phase 9-6: セルラー→WiFi自動復帰（RebindManager、2026-07-11）

Phase 9-4bまでで「WiFiは繋がっているがupstreamが死んでいる」検知→セルラーへの片方向
フェイルオーバーは実装・実機検証済みだったが、「WiFiの上流が復活したら自動的にWiFiへ
戻る」機能が無く、`AndroidAppExecutor.kt`の`onWifiUpstreamRecovered = {}`がno-opの
ままだった。カフェのWiFi（1時間で上流だけサイレントに死ぬ）でセルラーへ切り替わった後、
WiFiに戻ったら黙って戻ってきてほしい、というユーザー要望を受けて実装した。

**設計方針（ユーザー・Codexとの相談を経て確定）**:
- 切断側（WiFi→セルラー）は既存どおり即座（3回連続無応答、約9〜10秒）。静けさ待ちは行わない
  （繋がらなくなっている以上、待つ理由が無い）。
- 復帰側（セルラー→WiFi）は非対称に慎重にする: WiFi-bound一時Endpointでの疎通確認 →
  5回連続成功+15秒安定+セルラー最小滞在60秒のヒステリシス → 通信量が少ない「静けさ」を
  待ってから初めて実際にrebindする（trzsz転送中やターミナル出力が多い最中の瞬断を避けるため）。
  静けさが来ないまま最大60〜120秒待っても来なければ、今回は諦めて2分/5分/10分のバックオフへ。
- ユーザーが「今すぐWiFiに戻す」を明示的に要求した場合（ダウンロード中でも待てない等の
  レアケース）は、疎通確認だけは省略しないが、上記ヒステリシス・静けさ待ちは全てバイパスする。
- fd所有権ポリシー: 疎通確認に使ったfdをそのまま本番rebindに使い回すと所有権が競合するため、
  疎通確認用と本番rebind用は毎回別々に新規取得する。

**pure core / effectful shell の分離**: 判断ロジック（`RebindManager`、`rust-core/src/
rebind_manager.rs`）は、このリポジトリで既に`trzsz.rs`の`TrzszTransferFsm`が使っている
`timed_fsm::TimedStateMachine`にそのまま乗せた——`on_event`/`on_timeout`が`Response`
（アクション+タイマー命令+consumedフラグ）を宣言的に返すだけで、tokio/実fd/実I/Oに一切
触れない完全に同期的な実装。ヒステリシス・静けさ待ち・バックオフの全ロジックがここに
閉じ込められており、`RebindEvent`/`RebindTimer`を手で送るだけで（fakeもmockも実ネットワーク
も不要に）全状態遷移をユニットテストできる（`rebind_manager.rs`のtestモジュール、18ケース）。

I/O(疎通確認の実行・実際のrebind実行・WiFi/セルラーfdの取得)は`rebind_ports.rs`で
trait(`WifiProbeExecutor`/`RebindExecutor`/`PlatformFdSource`)として抽象化し、実装は
`rebind_driver.rs`(非同期実行層、`session.rs`の`TokioTimerRuntime`と同じパターンで
`RebindTimer`のSet/Killをtokioタスクのspawn/abortに変換する)と
`multipath_transport.rs`の`Real*`構造体(`RealWifiProbeExecutor`は本番と同じ相手へ実際に
QUICハンドシェイクを試みる一時Endpoint、`RealRebindExecutor`は既存の`rebind_to_fd`と
同じ`RebindRequest`チャネルを再利用、`RealPlatformFdSource`は`SessionCallback`経由で
Kotlin/Swiftへ`spawn_blocking`越しに同期的に要求する)に分離した。`rebind_driver.rs`は
fakeトレイト実装による配線テスト4本を持つ（noqを使わない、高速・非フレーキー）。

**UniFFI**: `OrchestratorCallback`に`on_request_wifi_fd`/`on_request_cellular_fd`
（`PlatformFd { fd, local_ip }`を返す、`PlatformFdSource`の実体）と
`on_rebind_state_changed(state: RebindPublicState)`（`OnWifi`/`FailedOverToCellular`/
`WaitingQuietToReturn`の3値、UI表示用）を追加。`SessionOrchestrator`に
`force_return_to_wifi()`（手動即時切替）を追加。

**Android実装**: `PhysicalPathProvider.acquireWifiOnly()`（既存`acquireCellularOnly()`の
WiFi版）、`AppExecutor.acquireWifiFd()`、`TerminalSession`に
`acquireWifiFd`/`acquireCellularFd`コールバックパラメータと
`onRequestWifiFd`/`onRequestCellularFd`/`onRebindStateChanged`のcallback実装、
`forceReturnToWifi()`の薄い委譲を追加。`TerminalUiState.rebindState`を追加し、
「今すぐWiFiに戻す」ボタン（`TerminalScreen.kt`）をセルラーへフェイルオーバー中/
WiFi復帰の静けさ待ち中だけ表示する（判定は`RebindPublicState`だけを見る、Kotlin側で
推測状態は持たない）。

設計判断として、`UpstreamHealthMonitor`の`onWifiUpstreamRecovered`
（`NET_CAPABILITY_VALIDATED`起点）は配線しなかった——`RebindManager`自身が
`ProbeCadence`タイマーで10秒毎に自発的にWiFi疎通確認を再試行するため、Kotlin側からの
「復帰の可能性」ヒント転送が無くても正しく動作する（無くても最大10秒の検知遅延が
増えるだけ）。将来的なレイテンシ最適化の余地として残す。

**テスト状況**: `rust-core`側は純粋状態機械18テスト＋Driver配線4テスト＋
`multipath_transport.rs`の既存e2eループバックテスト（`no_viable_path_fires_when_udp_fully_cut`
等）が新しいDriverを実際に組み込んだ状態でも無回帰であることを確認済み（全283テストgreen）。
Android側は`compileDebugKotlin`/`compileDebugUnitTestKotlin`/`compileDebugAndroidTestKotlin`
とJVMユニットテスト全体の無回帰を確認済み。

**既知の残作業**:
- **実機検証未実施**: 本物のカフェWiFi等での「上流サイレント死→セルラー切替→WiFi復帰→
  静けさ待ち→自動復帰」の一気通貫は、Phase 9-4bと同様まだloopbackテストのみで、
  実機・実ネットワークでの最終検証が残っている。

**Phase 9-6追記: iOS実装（Task #15/#16、macOS GitHub Actionsランナー上での実装）**

この開発環境（Linuxサンドボックス、Swiftツールチェーン無し）から直接iOSコードを
コンパイル確認することはできないため、Android版と1:1対応する構造で実装したうえで
macOS GitHub Actionsランナー（`ios-app-build-check.yml`/`ios-rust-core-check.yml`、
共に`macos-26`）でのビルド・テストに検証を委ねる方針にした。

- 新規`ios/Sources/IsekaiTerminalCore/PhysicalPathProvider.swift`（#15）: Android版
  `PhysicalPathProvider.acquireWifiOnly()`/`acquireCellularOnly()`のiOS版。
  `NWPathMonitor.availableInterfaces`からWiFi/セルラーそれぞれの`NWInterface`
  （`.index`/`.name`）を取得し、`setsockopt(IPPROTO_IP, IP_BOUND_IF)`でUDPソケットを
  明示的にそのインターフェースへバインドしたうえで、`getifaddrs(3)`で取得した
  実際のIPv4アドレスへ`bind(2)`する（Android版がワイルドカードbindを避けて
  `LinkProperties`から実IPv4アドレスを取得するのと同じ理由）。Android版の
  `ConnectivityManager.requestNetwork`のタイムアウト付き非同期待機と対称に、
  呼び出しごとに使い捨ての`NWPathMonitor`をタイムアウト付きで待つ設計にした
  （長命の監視状態を持ち回さない）。IPv4のみ対応（Android版と同じ制約、IPv6は
  将来課題）。
- `TerminalSessionController.swift`（#16）: `onRequestWifiFd`/`onRequestCellularFd`の
  暫定スタブ（コミット`1513132`）を`physicalPathProvider`呼び出しに置き換え、
  `onRebindStateChanged`を`TerminalUIState.rebindState`（新規`@Published`）へ反映する
  実装に、`forceReturnToWifi()`（`orchestrator.forceReturnToWifi()`への薄い委譲）を
  追加した。
- `TerminalView.swift`: Android版`TerminalScreen.kt`の「今すぐWiFiに戻す」ボタンと
  同じ表示条件（`connected && rebindState != nil && rebindState != .onWifi`、
  `RebindPublicState`だけを見て判定しSwift側でミラー状態は持たない）で
  `forceReturnToWifiButton`を追加。
- `ios/Tests/IsekaiTerminalCoreTests/PhysicalPathProviderTests.swift`を新規追加。
  Simulator環境では物理WiFi/セルラーインターフェースが実機と同じ形で存在するとは
  限らない（特にセルラーはSimulatorに無いことが多い）ため、「取得できればfdが
  有効であること」「取得できなければクラッシュせずnilを返すこと」の両方を許容する
  設計にした。
- **macOS CIでの検証結果**: PR #11でmacOS GitHub Actionsランナー（`macos-26`）の
  `ios-rust-core-check.yml`（`xcodebuild test -scheme IsekaiTerminalCore-Package`、
  新規`PhysicalPathProviderTests`含む）・`ios-app-build-check.yml`（実機Simulator
  アプリビルド+テスト）・`ios-ssh-vertical-slice-check.yml`が全てgreenであることを
  確認した(2026-07-14)。初回pushでは`PhysicalPathProvider.swift`が`Foundation`の
  import漏れで`TimeInterval`を解決できずビルド失敗していた（`Darwin`/`Network`だけ
  ではFoundationは暗黙にimportされない）ため、`import Foundation`を追加して修正。
  `ios-app-build-check.yml`は1回目の実行がちょうど`timeout-minutes: 30`の境界で
  `cancelled`になった(全ステップは`success`表示で実質的な失敗ではなかった)ため
  再実行してgreenを確認した。
- **Simulator制約（Task #15のサブタスク）**: SimulatorはmacOSホストのネットワークを
  仮想化しているため、`IP_BOUND_IF`によるインターフェース分離が実機と同じように
  機能するとは限らない。特に物理セルラーインターフェースはSimulatorに存在しない
  ことが多く、`acquireCellularFd()`はCI上では常に`nil`を返す可能性が高い
  （設計上は正常系として許容される）。実機での動作確認は#17が担う。

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
   → `android/migration_registry.toml`（現行版数`current`+未マージの予約`[[reserved]]`一覧）、
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
  → `android/build.gradle.kts`に`BuildConfig.ENABLE_EXPERIMENTAL_PHYSICAL_MULTIPATH`を追加
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
2. ✅ Remote/Dynamic port forward（plain SSH限定、LocalForwardRunner/RemoteForwardRunner/
   DynamicSocksForwardRunnerに責務分離、クライアント側は非ループバックbindを既定拒否）
   → Rust側: `ForwardType`に`Remote`/`Dynamic`を追加(既存`Local`と合わせ3種)。
   `RusshEventHandler`に`client::Handler::server_channel_open_forwarded_tcpip`を実装し、
   `-R`相当のリモート側listen(`tcpip_forward`/`cancel_tcpip_forward`、いずれも`&mut self`の
   ためclient handleを`Arc<tokio::sync::Mutex<_>>`化)からのforwarded接続をこちら側の
   ローカルターゲットへ中継。`-D`相当は新規`socks.rs`(SOCKS4/4a/5サーバー実装、独立
   ユニットテスト5件)を使う`run_dynamic_forward`で実装。`active_forwards`を
   `HashMap<String, ActiveForward>`(`Task`/`Remote{bind_addr,bound_port}`の2バリアント)に
   拡張し、`teardown_forward()`で種別ごとの後始末(task abort vs
   `cancel_tcpip_forward`+remote_forwardsマップからの除去)を共通化。非ループバックbind
   拒否(`reject_non_loopback_bind`)はLocal/Remote/Dynamic全種で共通適用。5種のQUIC系
   トランスポート(自作ヘルパーQUIC・マルチパス・STUN P2P・MASQUE relay含む)は
   `run_ssh_channel_loop`呼び出しの配線更新のみで同じロジックを共有。e2eテスト2件
   (`remote_forward_relays_bytes_end_to_end`・`dynamic_forward_socks5_relays_bytes_end_to_end`)
   を`local_forward_e2e_tests`に追加。
   Kotlin側: `ProfileEditScreen`のフォワード編集行にLocal/Remote/Dynamicの3種
   `FilterChip`を追加。`ForwardDraft`に`forwardType`を追加し、Remoteでは
   フィールドラベルを「ローカルターゲットホスト/ポート」に、Dynamicでは
   remoteHost/remotePort欄自体を非表示にしてSOCKS動作の説明文に置き換え。
   **見つかった実装済みバグ**: `PortForwardListConverter`(Room TypeConverter)と
   `PortForwardParceler`(`@Parcelize`用)が、LocalのみだったMVP時代の実装のまま
   `forwardType`を保存せず常に`ForwardType.LOCAL`固定で読み書きしていたため、
   Remote/Dynamicで保存してもDBラウンドトリップ後にLocalへ化けていた
   (instrumented testで発覚)。両方とも`forwardType`を実際にシリアライズ/
   デシリアライズするよう修正。
   Kotlinテスト2件追加(`addingRemoteForward_andSaving_persistsRemoteForward`・
   `addingDynamicForward_andSaving_persistsDynamicForwardWithoutTarget`)。
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

## Phase Y: iOS対応（Rust + Swift）— DESIGN.md の「やらないこと」から方針転換（ユーザー承認済み）

`DESIGN.md`（24行目）は当初 iOS 対応を「やらないこと（将来フェーズ）」としていたが、そこには元々
「Swift UI + 同 Rust コア共有。UniFFI が Swift バインディングを生成するため UI 層のみ別実装で済む」
という構想が明記されていた。2026-07-04、この構想を実行に移すことをユーザー承認のもと決定した。

### 方針（2026-07-04 決定）
- リポジトリ構成: 別リポジトリを切らず、この isekai-terminal リポジトリ内に `ios/` を追加する形で進める。
- 最初のマイルストーン: Android 版の主要機能フル移植（trzsz・QUIC/isekai-helper連携・鍵管理まで含む）を目標に据える。
- iOS 最低バージョン: 特にこだわらず、UniFFI/SwiftPM の標準的なサポート範囲（概ね iOS 15+ 目安）に委ねる。

### 調査で判明した技術的前提（2026-07-04）
- `rust-core`（crate `isekai-terminal-core`）は `[lib] crate-type = ["cdylib", "staticlib"]` で既に
  `staticlib` を含み、iOS 向けに転用しやすい。Android 依存は `#[cfg(target_os = "android")]` の
  android_logger 初期化のみで極めて薄い。`isekai-helper`（サーバー側常駐バイナリ）も Android
  依存ゼロで無改修のまま使える。
- UniFFI は UDL 不使用の proc-macro ベースで、`uniffi-bindgen/src/main.rs` は
  `uniffi::uniffi_bindgen_main()` の汎用呼び出しのみ。`--language swift` を指定すれば同じ仕組みで
  Swift バインディング生成できる見込み（未検証、Phase 0 で確認する）。
- **Swift 側で作り直しが必要（コード共有不可）な部分**: `NetworkPathMonitor.kt`/
  `PhysicalPathProvider.kt`（Android `ConnectivityManager` 直叩きの物理 Wi-Fi/セルラー同時
  マルチパス。iOS に同等 API なし）、`KeystoreKek.kt`（→ iOS Keychain/Secure Enclave）、
  `TerminalSessionService.kt`（Android Foreground Service → iOS のバックグラウンド実行制約は
  ずっと強い）、`input/` 配下（IME）、`ui/` 配下（Compose → SwiftUI 書き直し）。
- rust-core は `LazyLock<Runtime>` でプロセス全体に単一 tokio ランタイムを持つ設計。iOS の
  バックグラウンド時プロセス一時停止/終了との相互作用は未検証（Phase 0 で確認する）。

### 外部レビュー（ChatGPT相談、2026-07-04）

実装着手前に、上記の技術的前提を踏まえて次の4論点を ChatGPT に相談した。実装はまだ行っておらず、
次フェーズ（Phase 0: 技術検証スパイク）着手時の設計方針として記録する。

1. **バックグラウンド接続維持**: 「無期限にソケットを生かす」ことを iOS 版の仕様として約束しない。
   `Active → Quiescing → Suspended → Resuming` という明示的なセッション FSM を Rust 側に追加し、
   正（SSOT）は「iOS 上の QUIC connection の生存」ではなく「helper 側の論理セッション（session
   lease・H2C リングバッファ・offset）」に置く。既存の resume 機構（Phase 8）の延長で対応可能と
   いう見立て。`beginBackgroundTask` は短時間の後始末用、`BGAppRefreshTask`/`BGProcessingTask` は
   常時接続に使えない。Live Activities は v1 必須ではなく実験機能枠。Network Extension
   （`NEAppPushProvider`）や PushKit の流用はプライバシー説明・審査リスクの観点で非推奨。
2. **物理 Wi-Fi/セルラー同時マルチパス**: v1 スコープ外にする。`NWParameters.multipathServiceType`
   は MPTCP 向けで QUIC multipath には使えない。代替として論理マルチパス（Tailscale/直接/relay、
   Phase 9 相当）+ QUIC connection migration + `NWPathMonitor` 駆動の高速再接続を提供する。
3. **単一 tokio ランタイム**: 維持してよいが、`SessionSupervisor` + 明示的セッション FSM
   （`Disconnected/Connecting/Active/Quiescing/Suspended/Resuming/Closing/Closed`）を挟む設計を
   推奨。UniFFI 越しに `prepare_for_background`/`resume_from_background`/
   `application_will_terminate`/`memory_warning` 等のライフサイクル API を追加する。UniFFI の
   async は Swift Task cancellation を自動伝播しないため、セッション固有の `cancel()` を別 API
   として用意する必要がある。ターミナル再描画は1セル単位で callback せず、snapshot pull 型にする。
4. **XCFramework/SPM 配布**: XCFramework には Rust 静的ライブラリ + C ヘッダー/modulemap のみを
   格納し、UniFFI 生成 Swift コードは別途 Swift Package の source target として配布する（バイナリ
   に焼き込まない。Mozilla 自身もこの構成を採用）。ビルドターゲットは `aarch64-apple-ios`/
   `aarch64-apple-ios-sim`/`x86_64-apple-ios` の3つ。バージョンは Swift Package version / Rust
   crate version / wire protocol version の3つを分離管理する。

**まだ決めていない点**: 上記を正式な各 Phase 実装（サブフェーズ分割・番号）に落とし込む詳細設計は、
Phase 0（技術検証スパイク）の結果を見てから確定する。PLAN.md への Phase 記録を今後もこの Phase Y
配下に追記していくか、別ファイルに分けるかも Phase 0 着手時点で判断する。

### Phase 0: 技術検証スパイク（2026-07-04 着手）

開発機が Linux（macOS + Xcode 不在）であるため、Phase 0-0/0-1（mac 不要）はこのセッションで
実装・検証済み、Phase 0-2〜0-6（mac 必須）はスクリプト/手順書の用意のみで実行はユーザーが
macOS 環境で行う。

- ✅ **Phase 0-0（前提整備）**: `.cargo/config.toml`/`Cargo.toml` は変更不要と判断（iOS クロス
  ビルドは Xcode 同梱の cc/ar で完結し、Android のような `linker =` 上書きは原則不要という想定）。
- ✅ **Phase 0-1（Swiftバインディング生成の検証、最大のリスク項目）**: 新規
  `rust-core/scripts/generate-swift-bindings.sh`（ホストのデフォルトターゲットで cdylib を
  ビルドし `uniffi-bindgen -- generate --library ... --language swift` を実行。iOS クロス
  コンパイル環境不要）を実際に実行し、**UniFFI 0.31.2 で `--language swift` が全22箇所の
  エクスポート面を問題なく生成できることを確認した**。特に懸念していた
  `callback_interface OrchestratorCallback`（9メソッド）は `public protocol OrchestratorCallback:
  AnyObject, Sendable { ... }` として、`SessionCallback`（旧API）も同様に、日本語ドキュメント
  コメントを保持したまま生成された。`SshError`（`uniffi::Error`）も
  `enum SshError: Swift.Error, Equatable, Hashable, Foundation.LocalizedError` として正しく
  生成された。生成物一式（`isekai_terminal_core.swift`/`isekai_terminal_coreFFI.h`/`isekai_terminal_coreFFI.modulemap` と
  各 `.sha256`）は `ios/Sources/IsekaiTerminalCore/generated/` にコミット（Kotlin 側
  `android/src/main/kotlin/uniffi/isekai_terminal_core/isekai_terminal_core.kt` の運用と対称）。
  診断用の round-trip 検証関数 `core_version() -> String`（`rust-core/src/lib.rs`、
  `env!("CARGO_PKG_VERSION")` を返すだけの純粋関数、Rust SSOT 原則には抵触しない）を追加し、
  再生成後に Swift 側へ `coreVersion()` として生成されることも確認済み。
  スクリプト実装上の注意点（見つかった不具合と修正）: sha256 サイドカーファイル生成ループが
  `$OUT_DIR/*` を素朴に回すと、再実行時に既存の `*.sha256` まで再ハッシュして
  `*.sha256.sha256` が増殖するバグがあったため、生成のたびに `OUT_DIR` を `rm -rf` してから
  生成し直す方式に修正済み。
- ✅ **Phase 0-2（iOS 3ターゲットへのクロスコンパイル）**・**Phase 0-3（XCFramework化 +
  SwiftPMパッケージ雛形）**・**Phase 0-4（最小round-trip検証）**: 当初は mac 必須のため
  手順書（`ios/README.md`）の用意のみに留めていたが、後述のCI導入判断により
  **GitHub Actions（`macos-26`ランナー）上で実際に検証し、2026-07-04に全項目green
  で完了した**（実行ログ: run 28706001145、`build-and-test` ジョブ 11m28s）。
  途中で1点、事前に把握していなかったビルド前提が判明した: `isekai-terminal-core`
  （`helper_quic_transport.rs`）は`isekai-helper`のx86_64/aarch64 musl静的バイナリを
  `include_bytes!`で埋め込む設計（iOS固有ではなく、isekai-terminal-coreをどのホスト/ターゲット向けに
  ビルドする場合でも必要な一般的な前提。ローカル開発機ではPhase 0-1実行時点で既に
  `target/`に残っていたため気づかなかった）。CI上でcargo-zigbuild経由の
  `build-isekai-helper-musl.sh`を先に実行するステップを追加して解消した。
  シミュレータ上のround-tripテスト（`CoreVersionRoundTripTests.
  testCoreVersionMatchesCargoPackageVersion`）も実際にpassした。
  `rust-core/scripts/build-ios-xcframework.sh`（`uname` が `Darwin` でなければ即エラー終了。
  Phase 0-1 で確認した生成物のファイル名 `isekai_terminal_coreFFI.modulemap` を、
  `xcodebuild -create-xcframework -headers` が期待する `module.modulemap` という名前へ
  コピーしてから渡す処理を組み込み済み。module 宣言自体は `isekai_terminal_coreFFI` のまま変更しない）、
  `ios/Package.swift`（`IsekaiTerminalCoreFFIBinary` という binaryTarget名で XCFramework を参照、
  生成された Swift コードの `import isekai_terminal_coreFFI` はモジュール名なので target 名とは別物）、
  `ios/Tests/IsekaiTerminalCoreTests/CoreVersionRoundTripTests.swift`（`coreVersion()` が
  `Cargo.toml` の `version = "0.1.0"` と一致することを確認する1テスト）を作成済み。
  実行結果は下記 Phase 0-6 の CI 実行結果を参照。
- **Phase 0-5（バックグラウンドライフサイクルの手がかり）**: 机上調査の結論のみ記録。
  `LazyLock<Runtime>` のワーカースレッドは iOS サスペンド中に停止するが壁時計時間は経過し
  続けるため、QUIC の idle timeout/keepalive はサスペンド復帰時に stale 化しうる。これは
  上記の外部レビュー方針（「iOS 上の QUIC connection の生存」ではなく「helper 側の論理
  セッション」を SSOT にする）の妥当性を裏付ける。新規ライフサイクル API は実装していない
  （Phase 1 以降のマター）。実機（iPhone）でのサスペンド/jetsam実挙動の検証は、Phase 9-4の
  「実機検証は未実施」という記録パターンに倣い、明示的に次フェーズへ送る。
- **Phase 0-6（CI、方針転換: GitHub Actions を採用）**: 当初は「試行錯誤前提のスパイクのため
  時期尚早」としてPhase 0には含めない判断だったが、次の理由でCI導入に方針転換した。

  1. **開発機がLinuxでmacOS実機が無い制約を、CIが直接解消できる**: このリポジトリ
     （`cuzic/isekai-terminal`）は**公開リポジトリ**であり、GitHub-hosted の標準ランナー
     （macOS含む）は公開リポジトリでは無課金で使える。つまりユーザーが自分のMacで
     `ios/README.md` の手順を実行するのを待たずに、CI上でPhase 0-2〜0-4を実際に検証できる。
  2. **外部レビュー（ChatGPT相談、2026-07-04）**: 「Phase 0（ライブラリのクロスビルド+
     シミュレータテストのみ）にはコード署名が一切不要なため、Codemagicの強み（証明書管理・
     TestFlight/App Store配信の簡便さ）は現段階では効いてこない。既存CI
     （`fdroid-build-check.yml`/`room-migration-check.yml`）が両方GitHub Actionsである
     一貫性、公開リポジトリならmacOSランナーも無課金である点から、GitHub Actionsを推す。
     Codemagicは実際に配布可能なiOSアプリ本体をビルドする段階（TestFlight/App Store提出、
     証明書・Provisioning Profile管理）になってから、配布専用CIとして追加検討すればよい」
     という回答を得た。ビルドロジックをCIのYAMLに埋めずリポジトリ内のシェルスクリプトに
     置く（`rust-core/scripts/*.sh`）という既存方針とも整合する。
  3. **`macos-14` は使わない**: 2026-07-06から非推奨化開始・2026-11-02サポート終了予定のため
     （WebSearchで確認済み）、新規ワークフローは `macos-26`（2026-02-26 GA、Apple Silicon
     ネイティブ）を使う。`macos-latest` は移行期間中（2026-06-15〜07-15）で挙動が不安定な
     ため使わず、明示的に `macos-26` を指定する。

  新規 `.github/workflows/ios-rust-core-check.yml`（`runs-on: macos-26`、
  `rust-core/**`/`ios/**` 変更時 + `workflow_dispatch`）を作成。内容:
  Rust iOSターゲット追加 → cargoキャッシュ → Swiftバインディング再生成+
  コミット済み生成物とのdiffチェック（Rust公開APIを変更してもSwift生成物の再生成を
  忘れるケースをPRで検出）→ `build-ios-xcframework.sh` 実行 → `lipo -archs` によるXCFramework
  スライス検証 → シミュレータ動的選択（`xcrun simctl list devices available`から
  iPhone系デバイスを1つ選ぶ）→ `xcodebuild test`（`CODE_SIGNING_ALLOWED=NO`等で署名無効化）
  → XCFrameworkをartifactとしてアップロード。

  将来、実際に配布可能なiOSアプリのビルド（Archive・署名・IPA生成・TestFlight配信）を
  行うフェーズになったら、Codemagicを「配布専用CI」として追加することを検討する
  （ライブラリ検証はGitHub Actionsのまま維持）。

**Phase 0 完了（2026-07-04）**: 上記の通り Phase 0-1〜0-4 は GitHub Actions
（`.github/workflows/ios-rust-core-check.yml`、`macos-26`ランナー）上で全てgreenを確認した。
Phase 0-5（バックグラウンドライフサイクル）・0-6（CI、GitHub Actions採用）も含め、
「Phase 0: 技術検証スパイク」のゴールは達成された。

**次にやること**: 本格的な iOS 版 UI 実装（SwiftUI・Keychain・trzsz UI 等）の Phase 番号を確定し、
着手する。

### Phase 1 タスク分解への外部レビュー（ChatGPT相談、2026-07-04・2ラウンド）

Phase 1（本格実装）の作業をタスク分解した最初の案（Xcode雛形・Keychain/Secure Enclave・
プロファイル管理・ターミナル描画/IME・trzsz UI・QUIC接続・バックグラウンド配線・CI・実機確認の
10項目、番号順）をChatGPTに相談し、2ラウンドの指摘を経て以下の内容に確定した
（実装はまだ行っていない。以後のPhase 1実装はこのリポジトリの `main` ではなく
`.claude/worktrees/ios-phase1`（ブランチ `worktree-ios-phase1`）で行う）。

**ラウンド1の指摘**: 「iOS技術スパイク・最小SSHクライアント・製品機能・バックグラウンド耐性の
4段階が混在しており、番号順にそのまま進めるべきではない」。

**ラウンド2の指摘**（ラウンド1の再構成案に対するさらなるレビュー）:
1. Xcode雛形タスクと最小SSH縦切りタスクの受け入れ条件が重複していた → Xcode雛形は
   「Rustの同期/非同期関数を呼べる・テスト用callbackを受信できる・Rustオブジェクトを
   明示的に破棄できる」までに限定し、実SSH接続は縦切りタスクだけに持たせる。
2. SSH/helper接続テスト用fixtureは「フェーズ非依存」ではなく、CI統合テスト・最小縦切り・
   ネットワーク変化テスト・resume試験の前提であるため、Phase 1A前半に移動。CI fixture
   （GitHub Actions macOSランナー内、pinned host key、外部ネットワーク非依存）と実機fixture
   （LAN上の開発機、Local Network Privacy確認用、Wi-Fi切断/helper再起動可能）に分ける。
3. **「Swift Actorで順序保証する」という設計は誤り**: Swift Actorは内部状態への同時アクセスは
   防ぐが、複数RustスレッドからのcallbackをそれぞれTask化した場合、Actorへ到達する順序が
   元のcallback発生順である保証はない（Swift Task実行順は決定的FIFOではない）。代わりに
   Rust側に単一の順序付きEventQueue（`session_id`/`generation`/`sequence`を持つ
   `EventEnvelope`）を置き、Swiftはwake通知を受けて`drain_events(after_sequence, max_count)`
   で能動的に取得する設計に変更。イベントは「lossless（接続状態・認証要求・ホスト鍵確認・
   切断理由・trzsz開始/完了・エラー・バックグラウンドライフサイクル）」と
   「latest-wins（画面Damage・カーソル点滅・転送進捗中間値）」の2系統に分離する。
4. 画面更新のUniFFI境界データ形式を具体化: `TerminalFrameBatch`
   （`session_id`/`screen_generation`/`frame_sequence`/`rows: Vec<PackedRow>`/
   `dirty_top`/`dirty_bottom`/`cursor`/`title`/`bell`）。`PackedRow`はセルオブジェクト配列
   ではなくUTF-8テキストバッファ+セル幅配列+属性run+色テーブルにまとめる。
5. 日本語IMEスパイクを2段階に分割: (a)完全に独立したハーネスでの単体スパイク（固定位置の
   `UITextView`。完全に透明/ゼロサイズ/画面外は候補位置検証に適さないため避ける）→
   (b)画面更新ブリッジ完成後にターミナルカーソル位置への統合。
6. 最小縦切りをplain SSH（#20a）とisekai-helper/QUIC（#20b、任意）に分割し、原因分離しやすく
   する。キャンセル可能な接続・タイムアウト・二重connect拒否・close冪等性などを最初から含める。
7. **新規タスク: SSH/helper信頼ストアとホスト鍵確認UI**（秘密鍵管理はあったが接続先サーバーを
   信頼する仕組みが抜けていた）。初回接続でfingerprint表示・承認、再接続は一致すれば自動許可、
   変更時は自動許可せず明示的警告。
8. 鍵管理タスクを「CredentialVault」に拡張（SSH秘密鍵だけでなくパスワード・passphrase・
   helper認証情報・将来のrelay tokenまで対象）。nonce再利用禁止・AAD・atomic write・
   rollback/リカバリ・orphan blob清掃・鍵ローテーションなどを受け入れ条件に追加。
9. NWPathMonitorの通知ポリシーをSessionStateに応じて変える（Active時は短時間coalesce、
   Degraded/Reconnecting時はsatisfied変化を即時通知、unsatisfiedでも即座に切断とは
   判断せず実transport errorを待つ）。
10. バックグラウンドAPIの`deadline_ms`を`budget_ms`に変更（SwiftとRustで基準時計を共有して
    いないため目安に過ぎない。実際の終了判断はSwift側background task expiration callbackが正）。
11. バックグラウンド遷移対応に「SceneLifecycleReporter→AppExecutionCoordinator（複数Scene集約）
    →Rust SessionSupervisor」という層を追加（iPhone単一Scene限定でも将来の複数Scene対応に
    備えて最初から入れる）。
12. 総合回帰テストからSecure Enclave試験を除外（Phase 1で必須にしていないため矛盾する）。
    「メモリ圧迫によるプロセス破棄」は再現性が低いため、`simctl terminate`や
    cold launch（terminateコールバックなし）からのresume検証に置き換える。

**推奨実装順序（確定）**:

```
Phase 1A（iOSで成立するかを早期に証明するフェーズ）
  1. Xcodeアプリ雛形（実SSH接続は含めない）
  2. アプリビルドCI
  3. SSH/helper fixture（CI用・実機用）
  4. Rust側連番付きEventQueue + Swift CallbackIngress
  5. 日本語IME単体スパイク
  6. Rust→Swift画面更新ブリッジ + 最小レンダラー
  7. 日本語IMEとターミナルカーソルの統合
  8. plain SSH最小縦切り（Phase 1A完了条件）
  9. isekai-helper/QUIC最小縦切り（任意）

Phase 1B（安全に日常利用できる最小SSHクライアント）
  10. CredentialVault（Keychain保護）
  11. SSH/helper信頼ストア・ホスト鍵確認UI
  12. GRDB + プロファイルCRUD UI
  13. ターミナル特殊キー操作 + フル機能レンダリング
  14. NWPathMonitor + Local Network Privacy対応

Phase 1C（isekai-terminal固有の耐障害性を完成させるフェーズ）
  15. SessionSupervisor（2軸FSM: SessionState / ExecutionMode）
  16. バックグラウンド遷移対応（複数Scene集約層込み）
  17. trzszファイル転送 + ファイルサンドボックス橋渡し
  18. isekai-helper再接続・resume対応
  19. 実機での総合回帰テスト
```

実機検証は各機能タスクの受け入れ条件に前倒しし、最終回帰テストには集約しない方針。
タスクの詳細はセッションのTaskList（`[Phase 1A-N]`等のタグ付き）を参照。

### Phase 1A 実装進捗（2026-07-04、`worktree-ios-phase1`ブランチ、PR #2）

Phase 1Aの1〜4を実装し、いずれもGitHub Actions（`macos-26`）で実際にgreenを確認した。

- ✅ **1. Xcodeアプリ雛形**: `ios/App/IsekaiTerminalApp.xcodeproj`（手書き。開発機にmacOS/Xcodeが
  無いため、Xcode GUIを使わず直接pbxprojを記述した）+ `ContentView`が`coreVersion()`
  （sync）・`corePing()`（async）・`DiagnosticCallback`経由のcallback受信・
  `DiagnosticHandle`の明示的破棄を一通り実行する。**既知の落とし穴**: 当初
  `ios/`直下に`.xcodeproj`を置いたところ、同ディレクトリの`Package.swift`と
  競合し、既存の`ios-rust-core-check.yml`が使う`xcodebuild test -scheme IsekaiTerminalCore`
  が「Scheme IsekaiTerminalCore is not currently configured for the test action」で
  壊れることをCIで発見した。`ios/App/`サブディレクトリへ分離して解消
  （ローカルパッケージ参照の相対パスは`..`）。
- ✅ **2. アプリビルドCI**: `.github/workflows/ios-app-build-check.yml`
  （`macos-26`、`xcodebuild build-for-testing`、署名無効化）。
- ✅ **3. SSH/helper fixture（CI用）**: `rust-core/scripts/ios-fixture/
  start-sshd-fixture.sh`/`stop-sshd-fixture.sh`。実行のたびに使い捨てのホスト鍵・
  ユーザー鍵を生成し127.0.0.1の高番ポートでsshdを起動する。ローカル(Linux)・
  CI(`macos-26`、`.github/workflows/ios-fixture-check.yml`)双方でSSH round-trip・
  停止確認を確認済み。実機fixture（LAN上の開発機）は#20a/#20b着手時に用意する。
- ✅ **4. Rust側連番付きEventQueue + Swift CallbackIngress**: `DiagnosticEventQueue`
  （`sequence`発行をMutexで直列化するSSOT）+ `EventWakeListener`（wake通知のみ）+
  Swift `CallbackIngress` actor（wake受信→`drainEvents()`能動呼び出し）。
  Rust単体テスト3件、Swift XCTest 2件（sequence順で全件受信・drain冪等性）を
  iOS Simulator上で確認済み。実際の`OrchestratorCallback`統合（ControlEventQueue/
  RenderMailbox分離含む）はPhase 1Cへ持ち越し。

**副産物として発見した問題（iOS対応とは無関係、別セッションで対応予定）**:
`fdroid/tools.isekai.terminal.yml`（F-Droid提出用レシピ）の`sudo:`ブロックは
Android向けrustupセットアップのみを行い、`isekai-terminal-core`が`include_bytes!`で要求する
`isekai-helper`のmusl静的バイナリを事前ビルドしていない。既存の`fdroid-build-check.yml`
がこの副PRのCIで失敗して発覚（`main`でも同じ理由で既に失敗していることを確認済み）。
実際のF-Droidビルドサーバーでも同じ理由でビルドが失敗する可能性が高い。CI側だけを
回避的に直すと問題を覆い隠すことになるため、修正は`fdroid/tools.isekai.terminal.yml`
本体に対して行う必要がある（詳細はmemory参照）。

続けて5・6も実装し、いずれもGitHub Actions（`macos-26`）でgreenを確認した。

- ✅ **5. 日本語IME単体スパイク**: `XCUIApplication().typeText()`はソフトウェア
  キーボード/IMEを経由せずテキストを直接挿入するだけで変換ロジックを検証できない
  ため、`UITextInput`プロトコル（`setMarkedText`/`unmarkText`/`insertText`等）を
  実装した`TerminalIMEInputView`を用意し、実際のIMEが呼ぶのと同じメソッドを
  XCTestから直接呼び出す方式でCI上に落とし込んだ（ChatGPTとの相談で確認した方針）。
  ローマ字変換→変換中のBackspace→確定、変換キャンセル、絵文字直接入力、複数行
  ペーストの7シナリオがiOS Simulator上でpass。候補ウィンドウの見た目そのものは
  CIでは検証できないため実機/シミュレータでの目視確認は別途行う。
- ✅ **6. Rust→Swift画面更新ブリッジ + 最小レンダラー**: UniFFI境界のデータ形式を
  具体化（`TerminalFrameBatch`/`PackedRow`/`AttributeRun`/`CursorState`。セル
  オブジェクト配列ではなくUTF-8テキストバッファ+セル幅配列+属性runにまとめた）。
  `DiagnosticFrameMailbox`（Rust側、latest-wins、`screen_generation`/
  `frame_sequence`に基づくstale frame破棄のSSOT）+ `TerminalFrameRenderer`
  （Core Graphics/Core Textの初期レンダラー）+ `FrameIngress`（Swift actor、
  wake通知を受けて30fps相当にレート制限しつつdrain、UIView操作は`@MainActor`へ
  明示的にhop）。Rust単体テスト4件・Swift XCTest 2件で検証。

**次にやること**: Phase 1A-7（IMEとターミナルカーソルの統合）・1A-8（plain SSH
最小縦切り）・1A-9（isekai-helper/QUIC最小縦切り、任意）へ進む。1A-8以降は
実際のSSH接続(`SshSession`/`SshConfig`)を使う本格的な統合になるため、CIでの
自動検証に加えて実機/シミュレータでの対話的な動作確認も併用する。

### ✅ Phase 1A完了条件を達成（2026-07-04、1A-8）

- ✅ **8. plain SSH最小縦切り**: Android版と共通のRust実装
  （`createSshSession`/`SshConfig`/`SessionCallback`、変更なし）を使い、
  `rust-core/scripts/ios-fixture/start-sshd-fixture.sh`が起動する使い捨てsshdへ
  実際に接続。公開鍵認証→PTYシェル起動→日本語を含む`echo`コマンド送受信→切断が
  iOS Simulator上で2.4秒で成功した（`.github/workflows/
  ios-ssh-vertical-slice-check.yml`、`SshVerticalSliceTests`）。**Android版で
  実装済みのSSHクライアントコードがiOSから初めて実際に動作したことを実証**。
  ホスト鍵は`onHostKey`で常に受理（信頼ストア#31はPhase 1Bで実装）。
  fixture不在時はXCTSkipし既存テストスイートには影響しない設計。

これにより「Phase 1A: iOSで成立するかを早期に証明するフェーズ」の主要ゴール
（Xcode雛形・アプリCI・fixture・EventQueue・IME・画面更新ブリッジ・実SSH接続）を
全て達成した。残りはPhase 1A-7（IME/カーソル統合）・1A-9（QUIC/helper縦切り、任意）。
これらは実機での対話的な確認が中心になるため、実機が不要な範囲でPhase 1B
（CredentialVault・GRDB・信頼ストア等）へ先に進むことにした。

### Phase 1B 実装進捗（2026-07-04）

- ✅ **CredentialVault（Keychain保護）**: AES-GCM暗号化 + Keychain保管のKEK
  （`kSecAttrAccessibleWhenUnlockedThisDeviceOnly`）という構成で実装。
  暗号化形式のversion・AAD（key_id/key_type/public_keyですり替え検知）・
  atomic write・key_idのSHA256ハッシュからのパス導出・Keychain追加成功/blob
  保存失敗時のrollback・鍵ローテーション・orphan blob清掃・端末ロック時の
  エラー区別を実装し、XCTest 7件で検証。

  **重要な落とし穴（Keychainテストの配置場所）**: 素のSwiftPMパッケージ
  （`IsekaiTerminalCore`）のXCTestバンドルは実アプリでホストされないため、Keychain APIが
  `errSecMissingEntitlement`（-34018）で失敗することをCIで発見した（未署名/
  非ホストのプロセスはOSがどのアプリのKeychainか判定できないため）。
  `CODE_SIGNING_ALLOWED=NO`を外してXcodeの自動ローカル署名に任せるだけでは
  解消せず、根本原因は「テストが実アプリにホストされていないこと」だった。
  `IsekaiTerminalApp.xcodeproj`に新規ユニットテストターゲット
  `IsekaiTerminalAppTests`（`TEST_HOST`/`BUNDLE_LOADER`で`IsekaiTerminalApp`に
  ホストされる、明示的な共有scheme付き）を追加し、`CredentialVaultTests`を
  そこへ移動して解消した。**Keychain・生体認証・Local Network Privacyなど
  実アプリのentitlementコンテキストが必要なテストは、今後も`IsekaiTerminalAppTests`
  （素のSwiftPMパッケージの`IsekaiTerminalCoreTests`ではなく）に置くこと。**

- ✅ **SSH/helper信頼ストア**: `SshHostTrustStore`(JSONファイル永続化、GRDB統合前提の
  暫定実装)。SSH host key/isekai-helper identity/踏み台ホスト鍵を種別ごとに
  独立した識別子(`TrustIdentifierKind`)で管理。初回接続は`unknownHost`、
  fingerprint変更時は`mismatch`を返し自動上書きしない(明示的な`trust()`呼び出しの
  みが上書きできる)。XCTest 7件で検証(entitlement不要、素の`IsekaiTerminalCoreTests`で実行)。

- ✅ **接続プロファイル管理DB(GRDB)**: `GRDB.swift`を依存に追加。`KeyEntry`
  (CredentialVaultの`key_id`+表示名/鍵種別/公開鍵/認証ポリシーのみ、秘密材料は
  DBに保存しない)と`ConnectionProfile`(host/port/username/keyEntryId)の2テーブル、
  `keyEntryId`外部キーは`onDelete(.setNull)`。XCTest 8件で検証
  (マイグレーション適用・冪等性・CRUD・ソート・外部キーのNULL化)。

  **落とし穴（Dateの往復精度）**: GRDBがSQLiteへ`Date`を保存する際にミリ秒精度へ
  丸めるため、`Date()`由来のサブミリ秒精度を持つ値をそのまま厳密等価比較すると
  CIで失敗した。テストでは秒単位のDateを明示的に使うことで回避。

- ✅ **NWPathMonitor連携とLocal Network Privacy対応**: `NetworkPathPolicy`
  （判断ロジック単体、Phase 1CのSessionState導入前提の縮小版
  `ConnectionHealthHint`を受け取る疎結合設計）+ `NetworkPathObserver`
  （`network_epoch`発行・debounce・古いepochのキャンセル）。healthy時は
  短時間debounce、degraded/reconnecting時はsatisfiedへの変化を即時通知、
  unsatisfiedのままでも切断と断定しない。Info.plistへ
  `NSLocalNetworkUsageDescription`を追加(`NSBonjourServices`はBonjour未使用
  のため追加せず)。`LocalNetworkPermissionGuide`で設定アプリへの誘導導線を
  提供(拒否状態の事前問い合わせAPIが無いため、検知は実際の接続エラーを
  トリガーに呼び出し側が判断)。XCTest 8件で検証(実ネットワーク切替は
  CIで再現できないため判断ロジックのみ)。

- ✅ **ターミナル特殊キー→制御シーケンス変換（実機不要範囲）**:
  `TerminalKeyMapper`。Ctrl+英字の制御バイト・Esc/Tab/Backspace/Delete・
  矢印キー・Home/End/PageUp/PageDown・F1〜F12(xterm互換)をXCTest 8件で検証。
  **キーボードアクセサリバーの見た目・レイアウトや選択/コピー/ペーストのUI・
  Dynamic Typeとは独立したフォントサイズ設定UIは実機/シミュレータでの目視確認が
  必要なため、このタスクの実機不要範囲には含めていない(実機確認可能になった
  時点で着手する)。**

**Phase 1B のまとめ（2026-07-04）**: 5項目全てに着手し、実機不要な範囲は
全て実装・CI検証済み(CredentialVault・SSH/helper信頼ストア・GRDB接続
プロファイル管理・NWPathMonitor通知ポリシー・ターミナルキー変換ロジック)。
残っているのは実機/シミュレータでの対話的な確認が必要な部分(Keychainの
生体認証モード等の追加検証、ターミナルの実際の見た目・操作感、Local Network
Privacyの実際の許可ダイアログ挙動)のみ。次はPhase 1C(SessionSupervisor・
バックグラウンド配線・trzsz・resume・実機総合回帰)、または残っている
Phase 1A(1A-7 IME/カーソル統合・1A-9 QUIC/helper縦切り)に進む。

### Phase 1B 追記: ターミナルキー変換ロジックのAndroid/iOS共通化（2026-07-04）

「androidと共通化できるところは共通化しよう」という方針に基づき、iOSの
`TerminalKeyMapper.swift`が独自実装していたキー→制御シーケンス変換ロジックを
rust-core側へ統合した。

- **Rust側実装**: `rust-core/src/lib.rs`に`TerminalSpecialKey`(uniffi::Enum)・
  `terminal_special_key_bytes()`・`terminal_unicode_char_bytes()`・
  `terminal_ctrl_byte()`・`terminal_commit_text_bytes()`を追加。Android版
  `TerminalKeyEncoder.kt`の挙動を1:1で移植し、Rust側テスト31件
  (`terminal_key_mapping_tests`、Android golden testの28件+新規3件)で
  `cargo test -p isekai-terminal-core --lib`実行・全件pass確認済み。
- **追加した新規バリアント**: F1〜F12(xterm互換、Android版には無かった機能。
  iOS版`TerminalKeyMapper`由来)と`ForwardDelete`(前方削除、`ESC[3~`。iOS版の
  `.delete`ケースに対応、Android版`KEYCODE_DEL`＝バックスペース相当の
  `TerminalSpecialKey::Delete`(0x7F)とは別物)。
- **iOS側**: `TerminalKeyMapper.swift`を、既存の公開Swift API
  (`controlByte(for:)`/`SpecialKey`/`bytes(for:)`)はそのまま維持しつつ、内部実装を
  生成済みSwiftバインディング(`terminalCtrlByte`/`terminalSpecialKeyBytes`)への
  委譲に置き換えた薄いラッパーへ書き換えた。
  **副作用として`controlByte(for:)`の挙動がAndroid版と揃って拡張された**:
  従来のiOS版はアルファベットのみ対応(空白・記号は`nil`)だったが、Rust統合後は
  Android版と同じく`@ [ \ ] ^ _ ? space`もCtrl+<記号>として変換されるようになった
  (例: space→0x00, `[`→ESC(0x1B), `?`→0x7F)。`TerminalKeyMapperTests.swift`を
  この拡張された挙動に合わせて更新(space等をnilと期待するテストを削除し、
  Android paritryを検証するテストを追加)。
- **Android側は意図的に変更しなかった**: `TerminalKeyEncoder.kt`をRust側の
  UniFFI関数へ委譲する案も検討したが、**JVM/Robolectric単体テストはホストJVM上で
  ネイティブライブラリを解決できない**(`cargoBuildRustCore`はarm64-v8a向けの`.so`
  しかビルドしない)という既存の制約(`TerminalThemeTest.kt`のコメントで既に
  文書化されていた同じ問題)に阻まれる。Android本番コードから直接UniFFI関数を
  呼ぶ形に書き換えると、`TerminalKeyEncoderTest.kt`の既存28件が
  `UnsatisfiedLinkError`等で実行できなくなり、「実機不要な範囲でCI/Robolectricで
  検証する」という開発フローそのものが壊れる。そのため`TerminalKeyEncoder.kt`は
  従来通りのプレーンKotlin実装を維持し(golden testで両実装の等価性を継続担保)、
  ソースにその理由を明記するコメントのみ追加した。
- **Kotlinバインディング(`android/src/main/kotlin/uniffi/isekai_terminal_core/isekai_terminal_core.kt`)は
  再生成した**(実際にはAndroid本番コードから新規関数を呼ばないため機能上は
  不要だが、Phase 0以降追加した`DiagnosticCallback`/`EventWakeListener`/
  `core_version`等がAndroid側では一度も再生成されておらずファイルが大きく
  ドリフトしていたため、この機会に最新のRust APIサーフェスへ同期した)。
- **結論**: 実質的な「共通化」は、iOSのSwift独自実装をRust側の共通実装へ
  委譲する形で達成された(Swift側の重複コード削除)。Android側は元から
  Rust移植の一次ソースだったため変更不要。Rust実装とKotlin実装は今後も
  golden testで相互検証しながら並行して保守する非対称な構成になる。

## Phase 1D: iOS本体UI画面実装(2026-07-04、「先に画面の実装もすすめて」指示を受けて着手)

Phase 1B完了時点でiOS側には実際のプロダクト画面(プロファイル一覧・鍵管理・
ターミナル本体)が無く、Phase 1A-1の診断用`ContentView.swift`しか存在しなかった。
「xcodeシミュレーターでテストできる範囲を網羅的に」という要望への対応として
画面実装から着手する方針にユーザーが合意し、Android版の対応画面
(`ProfileListScreen.kt`/`ProfileEditScreen.kt`/`KeyListScreen.kt`/`KeyImportScreen.kt`)の
MVPスコープ部分をSwiftUIへ移植した。

- **`KeyManager.swift`**: Android版`KeyManager.kt`(ed25519鍵生成+OpenSSH private key
  PEMエンコード)をCryptoKitの`Curve25519.Signing.PrivateKey`で移植。
  **移植時に発見した実装差異**: Android版のOpenSSH `AUTH_MAGIC`が仕様上
  `"openssh-key-v1"` + 終端NUL(15バイト)であるべきところ、末尾が半角スペースに
  なっている(仕様不一致の可能性がある既存バグ)。iOS版では仕様通りNULバイトで実装し、
  実際のsshd(CI fixture)への認証成功をXCTestで検証して裏付けを取った
  (Android版の修正は本タスクのスコープ外、別途確認が必要)。
- **`AppServices.swift`**: `ProfileDatabase`/`CredentialVault`のアプリ全体シングルトン
  (Android版`data.Repositories`相当)。実ファイルパスは`.applicationSupportDirectory`
  配下。
- **`ProfileListView`/`ProfileEditView`/`KeyListView`/`KeyImportView`**: いずれも
  `ios/Sources/IsekaiTerminalCore/`(SwiftPMパッケージ、`ios/App/`のxcodeprojではなく)に配置。
  これによりXcodeプロジェクトファイル(pbxproj)の手書き編集が一切不要になった
  (SwiftPMがディレクトリ内の`.swift`ファイルを自動検出するため)。App target
  (`IsekaiTerminalApp.swift`)からはこれらを`import IsekaiTerminalCore`で参照するだけでよい。
- **スコープ**: iOS版`ConnectionProfile`(GRDB)は現時点でlabel/host/port/username/
  keyEntryIdのみのMVPスキーマのため、踏み台(ProxyJump)・relay・multipath・
  ポートフォワード等、Android版がPhase 7〜10で段階的に追加した高度な機能は
  この一次実装のスコープに含めない(後続タスクで追加)。ターミナル本画面
  (SSH接続+レンダリング+IME統合)も別タスクとして後続に切り出し、接続タップ時は
  `TerminalPlaceholderView`(未実装であることを明示するプレースホルダー)へ遷移する。
- **`NavigationStack`/`.navigationDestination(for:)`の採用に伴いiOS最低バージョンを
  15→16に引き上げた**(`Package.swift`の`platforms`、pbxprojの
  `IPHONEOS_DEPLOYMENT_TARGET`両方)。プログラム的な配列ベースpath
  (`NavigationStack(path: $path)`、`AppRoute: Hashable`)を採用。
- **テスト**: Keychainに触れない`ProfileListModel`/`ProfileEditModel`は素の
  `IsekaiTerminalCoreTests`(非ホスト)で検証。`KeyManager`は仕様準拠の構造チェックに加えて、
  CI fixtureのsshdへ実際に認証させるE2Eテストで検証(`KeyManagerTests.swift`)。
  fixtureの`authorized_keys`はsshd起動後も接続の都度再読込されるため、
  テスト実行時に動的に追記できることを利用した。既存の`SshVerticalSliceTests.swift`の
  fixture読込ロジックは`SshFixtureConfig.swift`へ共通化。
- **未着手(次のタスク)**: ターミナル本画面の実装。

### `IsekaiTerminalAppUITests`(XCUITest)新設(2026-07-04)

ユーザーからの「xcodeシミュレーターでテストできる範囲を網羅的に」という要望に対応。
これまでの`IsekaiTerminalCoreTests`/`IsekaiTerminalAppTests`はいずれも「XCTestCaseから対象の
メソッド/モデルを直接呼び出す」ユニットテストスタイルで、`XCUIApplication`で実際に
アプリを起動しタップ・文字入力・スワイプ・メニュー操作・システムアラート確認を行う
「真のUI駆動テスト」は一つも無かった。`IsekaiTerminalApp.xcodeproj`に
`com.apple.product-type.bundle.ui-testing`型の新規ターゲット`IsekaiTerminalAppUITests`
を(`IsekaiTerminalAppTests`追加時と同じ手法で)手書きのpbxproj編集で追加し、
既存の共有scheme(`IsekaiTerminalApp.xcscheme`)にもBuildActionEntry/TestableReferenceを
追加した(`ios-app-build-check.yml`が実行する`xcodebuild test -scheme IsekaiTerminalApp`が
自動的にこの新規ターゲットも実行するため、新規CI workflowは不要)。

`AppLaunchUITests.swift`として以下7件を追加、全てCI(iOS Simulator)で実際にpass
(初回実行で7件とも一発green、SwiftUIの`Menu`タップ・`.swipeActions`・
`TextField(axis: .vertical)`の要素種別不定・アラート確認など、事前に動作を
確信できていなかった操作を含む):
- アプリ起動→接続先一覧画面が表示されることの確認(スクリーンショット添付)
- 「+」→フォーム入力→保存→一覧に新しい行が現れることの確認
- 行のスワイプ削除→削除確認アラート→行が消えることの確認
- 行のスワイプ編集→ラベル変更→保存→新ラベルに置き換わることの確認
- メニュー→鍵管理→鍵生成→生成完了アラート→一覧に新しい鍵が現れることの確認
- メニュー→鍵管理→インポート→貼り付け→保存→一覧に新しい鍵が現れることの確認
- パスワード認証プロファイルをタップ→パスワードプロンプトが表示されることの確認
  (ターミナル本画面は未実装のためキャンセルのみ確認)

**未実施(次の候補)**: Local Network Privacyの実際の許可ダイアログがSimulatorで
再現するかは今回試していない(実LAN上の別ホストへの接続でのみ発火する可能性が高く、
127.0.0.1へのループバック接続では発火しないと見られるため、実装コストの割に
検証価値が不確実と判断し見送った)。ターミナル本画面が実装されSSH接続フローが
組み込まれた際に、実際の接続試行と合わせて検証するのが良い。

**既知の制約**: `AppServices.shared`は実ファイル(GRDB DB・Keychain)を使う
シングルトンでテスト間でリセットされないため、各テストは`UUID`ベースのユニークな
ラベルで新規行を識別する設計にした(既存データの有無を前提にしない)。

**CIで発見・修正した2件の不具合(2026-07-04)**:
1. **アクター分離エラー**: `ProfileListView`/`KeyListView`/`KeyImportView`の`init`が
   `model: XxxModel = XxxModel()`という形でデフォルト引数を持たせていたが、
   デフォルト引数式は呼び出し側の非isolatedなコンテキストで評価されるため、
   `@MainActor`なモデルのinitを呼べずコンパイルエラーになった
   (`ProfileEditView`は`StateObject(wrappedValue:)`のautoclosureに包まれた形で
   init本体内に構築していたため問題が出なかった)。デフォルト値を廃止し、
   呼び出し側(`body`、MainActor)で明示的に構築するよう修正。
2. **KeyManagerの実sshd認証テストが素通りしていた**: `ios-ssh-vertical-slice-check.yml`
   は`-only-testing:IsekaiTerminalCoreTests/SshVerticalSliceTests`で絞り込んでいたため、
   fixtureを使う`KeyManagerTests.testGeneratedKeyAuthenticatesAgainstRealSshd`は
   一度も実行されず(`ios-rust-core-check.yml`側はfixtureが無く常にXCTSkip)、
   「生成した鍵が実際にsshdで認証できる」という最重要の検証が実質未実施だった。
   `-only-testing`にこのテストを追加して修正し、実行後に実際にpass
   (0.475秒、実接続)することを確認した。これによりAndroid版の
   `AUTH_MAGIC`仕様不一致の修正が正しいことも実証された。

## ターミナル本画面の実装(#18b、2026-07-04、「ターミナル本画面の実装に進んで」指示)

Phase 1D最後の主要ピース。SSH接続・VTE画面描画・日本語IME統合・特殊キーの
アクセサリバーを1画面にまとめた`TerminalView`を実装した。

- **`TerminalSessionController`/`TerminalUIState`**: `SessionCallback`
  (Android版の新しい`OrchestratorCallback`ではなく、iOS版がPhase 1A-8で使った
  従来の`createSshSession`/`SessionCallback`API)を実装する接続コントローラ。
  `SessionCallback`のメソッドはRustのtokioワーカースレッドから直接呼ばれ、
  かつ`onHostKey`は同期的に`Bool`を返す必要があるため、コントローラ自体は
  `@MainActor`にせず(`@unchecked Sendable`の素のclass)、UIへ反映する
  `@Published`状態だけを別クラス`TerminalUIState`(`@MainActor`)に分離し
  `Task { @MainActor in }`で明示的に受け渡す設計にした。
- **描画は`ScreenUpdate`/`CellData`を直接消費**: Phase 1A-6で作った
  `TerminalFrameBatch`/`DiagnosticFrameMailbox`/`FrameIngress`(診断用の並行表現)は
  一切使わず、Android版`ui/SshTerminalCanvas.kt`と対称に`CellData.fg`/`bg`を
  ARGBパックのUInt32として直接解釈する`TerminalScreenView`(Core Graphics)を
  新規に実装した(PLAN.mdで以前から指摘していた「実際の統合時はScreenUpdate/
  CellDataを直接使うべき」という方針をここで実行した)。
- **ホスト鍵確認はAndroid版と同じTOFU方式に決定**: `SshHostTrustStore`自体の
  設計コメントは「対話的な確認UIを前提」だが、`onHostKey`がRustスレッドから
  同期的にBoolを返す必要がある制約(接続処理をブロックしてまでUI確認を待つ
  設計は複雑さに見合わない)を踏まえ、Android版`TerminalSession.kt`の
  `onHostKey`と同じTrust On First Use方式(初回は自動信頼して記録、
  fingerprint変化時のみ拒否)を採用した。対話的な確認UIへの格上げは将来の
  改善候補として明記。
- **`TerminalIMEInputView`にターミナル統合用フックを追加**(#18bで担う予定だった
  部分): `onSendBytes`/`bracketedPasteMode`/`ctrlArmed`を追加。`insertText`/
  `unmarkText`経由の確定テキストは`terminal_commit_text_bytes`で、Backspaceは
  固定の0x7Fバイトで送信バイトを計算する。**Backspaceの送信は内部bufferの
  空/非空に関係なく常に発行する**ように修正した(このviewの`buffer`は
  UITextInputプロトコル用の内部トラッキングに過ぎず実際のターミナル画面内容とは
  独立しているため、元の「bufferが空ならBackspaceを無視する」ガードは
  実運用では誤りだった)。
- **アクセサリバー**: Ctrl(トグル式、次の1文字をCtrl制御バイトに変換)・
  Esc・Tab・矢印・Home/End/PageUp/PageDownを実装。矢印キーは当初`TerminalKeyMapper`
  (Swift版)に`applicationCursorMode`切り替えのAPIが無いため常にCSI形式だったが、
  タスク#63(#31で`TerminalKeyMapper.bytes`にmodifiers/applicationCursorMode引数が
  追加された後)で`controller.uiState.latestScreenUpdate?.applicationCursorMode`
  (新しいミラー状態を作らずRust側の値をそのまま読む、`TerminalSessionController
  .sendKeySequence`と同じパターン)を配線し、DECCKMを考慮するよう解消した。
- **cols/rowsは固定(80x24)**: 実際のview sizeに応じた動的リサイズ
  (`SshSession.resize(cols:rows:)`は既に存在する)は後続の改善候補。
- **テスト**: `TerminalSessionControllerTests`(TOFUロジックを実接続なしで検証)・
  `TerminalScreenViewTests`(`ScreenUpdate`適用+`draw(_:)`直接呼び出しの
  スモークテスト。`layer.render(in:)`はキャッシュ済みcontentsの再生でしかなく
  `draw(_:)`を保証しないため、`UIGraphicsImageRenderer`のコンテキスト内で
  `draw(_:)`を直接呼ぶ方式にした)・`TerminalIMEInputViewTests`に新規フックの
  検証を追加(commit/backspace/ctrlArmedの各経路)。実際のSSH接続を伴う
  ターミナル画面自体のXCUITestはまだ追加していない(実sshdへの接続待ちを伴う
  ため、CI fixtureとの組み合わせは次の課題)。
- **`TerminalSessionControllerE2ETests`(実sshd接続の統合テスト)**:
  鍵認証プロファイル(CredentialVault経由の秘密鍵解決)でCI fixtureへ実際に
  接続し、`onConnected`/`echo`コマンド送信/`onScreenUpdate`受信/`onDisconnected`が
  一通り動くことを検証する。**CredentialVault(Keychain)に触れるため、
  最初は素の`IsekaiTerminalCoreTests`に置いてしまい`errSecMissingEntitlement`(-34018)で
  CI red化した**(`CredentialVaultTests.swift`で既に文書化されていた制約を
  また踏んだ)。アプリホスト型の`IsekaiTerminalAppTests`へ移動し(fixtureを
  共有できないモジュール境界のため、`SshFixtureConfig`相当を最小限複製)、
  `ios-app-build-check.yml`側でもCI fixtureを起動するよう追加した
  (`ios-ssh-vertical-slice-check.yml`とはポート2298/2299で分離)。
- **CIで発見・修正した追加の3件の不具合(ターミナル本画面実装時)**:
  1. `TerminalUIState()`を`TerminalSessionController`(非isolated)のstored
     property初期値として構築しようとし、以前のView初期化子デフォルト引数と
     同種のactor分離エラーになった → `TerminalUIState.init()`を`nonisolated`に。
  2. `TerminalAccessoryBar`の`inputView`プロパティが`UIResponder.inputView`と
     名前が衝突し「'strong'プロパティを'weak'でオーバーライドできない」エラー
     になった → `imeInputView`へリネーム。
  3. `TerminalIMEInputView`から`inputAccessoryView`を外部から設定しようとしたが
     `UIResponder.inputAccessoryView`は既定でget-onlyだった → overrideして
     get/set可能にした。

**Phase 1Dのまとめ**: プロファイル管理・鍵管理・ターミナル本画面という
iOS版の主要な画面が一通り実装され、全てCI(iOS Simulator)で検証済み。
真のUI駆動テスト(XCUITest)基盤も新設した。残る主要タスクはPhase 1C
(SessionSupervisor・バックグラウンド配線・trzsz・resume・実機総合回帰)。

## Android/iOS機能パリティのgap分析(2026-07-04、「androidで実装済機能でまだなものを洗い出して」指示)

Android側の実装済み機能を網羅的に調査し(Explore agent活用)、iOS側との機能差分を
洗い出してPhase 1E〜1Gとしてタスク化した(タスク#40〜#54)。詳細はタスクリスト参照、
ここには要点のみ記録する。

**Phase 1E(トランスポート/接続方式、影響最大)**: iOSは現状プレーンSSH直接接続のみ。
`ConnectionProfile`にjump host/転送方式/forwards/agent forward等のフィールドが
そもそも無い(#40でスキーマ拡張がまず必要)。ProxyJump(#41)・ポートフォワード
-L/-R/-D(#42)・SSH agent forwarding(#43)・STUN+SSHランデブーP2P(#44)・
MASQUE relay P2P(#45)・Tailscale⇔直接アドレスマルチパス(#46)・物理Wi-Fi/
セルラーマルチパス(#47、実験的・低優先)。**Rust側(rust-core)はこれら全て
既に実装済み(Android側が使用中)**なので、iOS側はUI/データモデルの配線が
不足しているだけ。

**Phase 1F(ターミナルUI polish)**: 選択/コピー・ペーストUI(#48)・フォントサイズ
ピンチズーム(#49)・配色テーマプリセット選択UI(#50、Rust側`setTerminalTheme`は
既存)・スクロールバックスワイプUI(#51、`scrollbackCells`/`scrollbackLen`は
既存)・アクセサリバー拡充(^D/^Z/ペースト/定型文シート、#52)。

**Phase 1G(その他)**: 定型文(Snippets)管理画面(#53)・複数タブ/複数セッション
対応(#54、現状iOSは1画面1セッションのみ)。

**パリティが取れている項目**(参考、gap分析結果): Ed25519鍵生成・パスワード/鍵
認証・CredentialVault・ホスト鍵TOFU方式・VT100/VTEレンダリング・IME統合。

**How to apply**: 優先度はユーザーとの相談次第だが、影響が最も大きいのは
Phase 1E(トランスポート層)。ただしAndroid側でもSTUN P2P/relay P2P/物理
マルチパスは実機未検証の実験的機能のままなので、iOS版でもこれらを「完了」の
基準にする必要はない(Android自身がそう扱っている)。

### Phase 1E-1〜1E-4実装メモ(2026-07-04、#40〜#43完了)

`ProfileDatabase.swift`に`StoredPortForward`/`StoredTransportPreference`という
GRDB永続化専用のミラー型を追加した(UniFFI生成型`PortForward`/`TransportPreference`
自体は`Equatable, Hashable`のみで`Codable`ではなく、別ファイルでの再帰的な
`Codable`合成もSwiftでは出来ないため)。ハマった点:

- `StoredPortForward.Kind`に`Equatable`/`Hashable`を付け忘れ、外側の
  `StoredPortForward`自体の合成コンパイルが通らなかった(自己レビューで発見、
  別コミットで修正)。
- `StoredTransportPreference`に`DatabaseValueConvertible`を付けないと、GRDBの
  Codable-record機構がJSON文字列としてダブルクォート付きで保存してしまい、
  v2 migrationの`ALTER TABLE ... DEFAULT`の素の文字列リテラルと表現が食い違う
  ところだった(生カラム値を直接読むテストで確認)。

`onAgentSignRequest`(#43、Rustスレッドから同期的にBoolを返す必要がある)は
`DispatchSemaphore` + `Task { @MainActor in }`でUI確認を橋渡しする方式にした
(30秒タイムアウト、Android版と同じ方針)。

### Phase 1A-9(#30)実装メモ(2026-07-05、isekai-helper/QUIC最小縦切り)

**アーキテクチャ上の発見**: `createHelperQuicSession`/`HelperQuicSession.connect`・
`connectAuto`だけでなく、`MultipathHelperQuicSession`・`IsekaiStunP2pSession`・
`IsekaiLinkRelaySession`(#44〜#46が使う予定)も**全て既存の`SessionCallback`
プロトコルを使う**ことを確認した(`OrchestratorCallback`/`SessionOrchestrator`への
移行は不要)。Android側は`SessionOrchestrator`経由で全トランスポートを統一的に
扱っているが(`TerminalTabsViewModel.connectTab`のトランスポート別分岐を参照)、
iOS側は当面この直接セッション方式のままで#44〜#47まで実装を進められる。
`SessionOrchestrator`への移行が必要かどうかは#24(Rust側`SessionSupervisor`実装)の
スコープと合わせて改めて判断する。

`TerminalSessionController.connect()`に`profile.transportPreference`による分岐を
追加し(Android版`TerminalTabsViewModel.connectTab`の`when`式と同じ構造)、
`.plainSsh`→`createSshSession`、`.isekaiHelperQuic`→`HelperQuicSession.connect`、
`.auto`→`.connectAuto`(フォールバック付き)とし、未実装の残り4方式
(`.tsshdQuic`/`.isekaiHelperQuicMultipath`/`.isekaiStunP2pQuic`/
`.isekaiLinkRelayQuic`)は明示的に`.failed`にする。`SshSession`/`HelperQuicSession`は
共通の親プロトコルを持たないため、`send`/`resize`/`disconnect`だけを要求する
`ActiveTerminalSession`という薄いプロトコルへ両クラスを同一モジュール内で
事後適合(`extension SshSession: ActiveTerminalSession {}`)させ、
`TerminalSessionController.session`の型をそれに統一した。config構築ロジック
(`makeSshConfig`/`makeHelperQuicConfig`、Android版`ConnectionProfile.toSshConfig`/
`toHelperQuicConfig`相当)はネットワーク呼び出しから分離し、`internal`スコープで
テストから直接呼べるようにした。

**E2E検証範囲の意図的な限定(重要)**: `rust-core/src/helper_bootstrap.rs`を調査した
結果、isekai-helperのブートストラップは**Linux musl static バイナリ**
(`x86_64-unknown-linux-musl`/`aarch64-unknown-linux-musl`)をSSH経由でリモートに
配置・実行する前提であることを確認した。一方、既存のiOS CI fixture
(`rust-core/scripts/ios-fixture/start-sshd-fixture.sh`)は**macOS CIランナー自身の
上で**sshdをループバック起動する方式のため、そこにLinuxバイナリを配置しても
実行できない(macOSはLinux ELFをネイティブ実行できない)。この制約は`isekai-helper`
クレート自体にLinux専用コードが無い(cross-compile自体は可能かもしれない)としても
変わらない — 本番の埋め込み(`include_bytes!`、rust-core側)はLinux musl向けに
固定されており、これを差し替えるにはrust-core本体の変更が必要になる。

これはRust側`helper_bootstrap.rs`自身のe2eテストも実機/リモートLinuxサーバーを
前提にした**opt-in扱い(通常のCIでは実行されない)**という既存の前例と同じ制約
であり、iOS版だけの新しいギャップではない。したがって#30のCI検証範囲は
「configのマッピングと`transportPreference`分岐が正しいこと」(ネットワーク非依存の
unit test、`TerminalSessionControllerTests.swift`)に限定し、**実際のQUIC
接続性・isekai-helperブートストラップの成否はこのセッションではCI検証しない**。
将来これを検証したい場合は、(a) macOSランナーから疎通できる実Linuxサーバーを
別途用意する、(b) 実機(iPhone)でユーザーが手動検証する、のいずれかが必要になる
(Android版のSTUN P2P/relay P2P/物理マルチパスが実機未検証のままな理由と同種)。

### Phase 1E-5(#44)実装メモ(2026-07-05、STUN+SSHランデブーP2P)

#30と全く同じパターンで実装: `TerminalSessionController`に
`makeIsekaiStunP2pConfig`(config構築、ネットワーク非依存でunit testable)+
`connectIsekaiStunP2p`(`createIsekaiStunP2pSession`呼び出し)を追加し、
`IsekaiStunP2pSession`も`ActiveTerminalSession`へ事後適合させた。
`ProfileEditView`にも「接続方式」Pickerへ選択肢を追加し、選択時のみ
`stunServer`(host:port)入力欄を表示する(空欄ならAndroid版と同じ既定STUN
サーバー`stun.l.google.com:19302`にフォールバック、Android版
`ConnectionProfile.DEFAULT_STUN_SERVER`と同じ値)。E2E検証の限界は#30と同一
(STUN穴あけ+isekai-helperブートストラップの実接続性はmacOS CI fixtureでは
検証できない)ため、テストはconfig構築ロジックのみに限定した。

残り#45(MASQUE relay)・#46(マルチパス)・#47(物理マルチパス、低優先)も
同じパターンで実装できる見込み。

### Phase 1E-6(#45)実装メモ(2026-07-05、MASQUE relay P2P)

`IsekaiLinkRelayConfig`は`relayAddr`/`relaySni`/`relayJwt`(全て`String`必須、
Optionalではない)を要求する。`ConnectionProfile.relayJwt`はPhase 1E-1の時点で
「暗号化して保存すること(現時点ではまだ平文格納のプレースホルダー)」と明記
されていたため、このタスクでAndroid版`RelayCredentialVault`+`KeystoreKek`相当を
Swift側にも実装した:

- 新規`ios/Sources/IsekaiTerminalCore/RelayCredentialVault.swift`: `CredentialVault.swift`に
  既にある`KeychainKEKStore`(Keychain由来のAES-GCM対称鍵ストア)を、秘密鍵ごとの
  鍵ではなく固定1鍵(`"relay-jwt-kek"`)で再利用する薄いラッパー。
  `encrypt(String) throws -> String`/`decrypt(String) throws -> String`のみ。
- `AppServices.shared.relayVault`として公開し、`ProfileEditModel`(保存時に暗号化・
  編集読込時に復号、Android版`encryptRelayJwt`/`decryptRelayJwt`と同じタイミング)と
  `TerminalSessionController`(接続直前に復号、Android版`connectTab`内の
  `decryptRelayJwt`呼び出しと同じタイミング)の両方に注入した。
- **Keychainテスト配置の罠(4回目)**: `RelayCredentialVault.encrypt/decrypt`を
  実際に呼ぶテストは素の`IsekaiTerminalCoreTests`では`errSecMissingEntitlement`になるため、
  最初からアプリホスト型の`IsekaiTerminalAppTests`(既存の
  `TerminalSessionControllerE2ETests.swift`に追記、新規ファイルにすると
  pbxproj手編集が要るため既存ファイルへの追記で済ませた)に置いた。
  `Tests/IsekaiTerminalCoreTests`側には「relayJwt未設定でnilを返す」経路(Keychainに触れない)
  だけを残した。

### Phase 1E-7(#46)実装メモ(2026-07-05、Tailscale⇔直接アドレスのマルチパス)

`MultipathHelperQuicConfig`は`directHost`/`cellularRemoteHost`(Tailscale⇔直接
アドレス切替、このタスクの対象)と`wifiFd`/`wifiLocalIp`/`cellularFd`/
`cellularLocalIp`(物理Wi-Fi/セルラー無線への同時バインド、#47の対象)が同じ構造体に
混在している。Android版`ProfileEditScreen.kt`を読んだところ、**Android自身の物理
マルチパスも現状noq側の既知バグ(noq issue #738、`open_path()`にlocal_ip明示指定した
経路でPATH_RESPONSEが届かずvalidation failedになる)により事実上no-op**であることが
判明した(`BuildConfig.ENABLE_EXPERIMENTAL_PHYSICAL_MULTIPATH`でdebugビルドのみ
表示、リリースビルドは非表示)。このため#47を「低優先」とした当初の判断は妥当であり、
iOS版もこのタスクでは`wifiFd`等を常に`nil`にした(#30/#44/#45と同じ、config構築を
ネットワーク呼び出しから分離してunit testableにするパターンを踏襲)。

`TransportPreference.tsshdQuic`はAndroid版では対応済みだが(`tsshd`バイナリ経由の
別実装、Phase 5B)、#40〜#54のAndroid/iOS機能パリティ調査ではisekai-helper系を優先
したため対象外にしてあり、タスク番号が無い。iOS版`TerminalSessionController.connect()`
では`.tsshdQuic`のみ引き続き「未対応」の`.failed`にしている(意図的な既知ギャップ、
新しいタスクではない)。

**#47(物理Wi-Fi/セルラーマルチパス)は今回スキップした**: Android版自身が
noq issue #738により事実上no-opという上記の発見を踏まえ、動作しない機能をiOS側に
新規実装する価値が無いと判断し、Phase 1F(ターミナルUI polish)へ進んだ。
noq側の修正が入り次第、Android/iOS両方で改めて着手する。

### Phase 1F-1(#48)実装メモ(2026-07-05、ターミナル選択/コピー・ペーストUI)

Android版`ui/TerminalSelection.kt`(純粋ロジック)+`ui/SshTerminalCanvas.kt`
(ハイライト描画)+`TerminalScreen.kt`(ジェスチャ+クリップボード配線)を調査した上で
(Explore agentで事前調査)、Swift側へ1:1移植した:

- 新規`ios/Sources/IsekaiTerminalCore/TerminalSelection.swift`: `CellPos`/`SelectionRange`
  (行単位選択、Android版と同じMVP制約)/`offsetToCellPos`/
  `reconstructSelectionText`。選択状態はスクロール位置と同じ「UI表示だけに閉じた
  状態」(`.claude/rules/rust-ssot.md`の例外)として扱い、Rust側には一切持たせない
  (Rust-core調査済み、選択/クリップボード関連のAPIは元々存在しない)。
- `TerminalScreenView`(UIKit)に`UILongPressGestureRecognizer`を追加。
  `.began`後も`.changed`で位置更新され続けるため、Android版のように別途pan
  gestureを組み合わせる必要はなかった(UIKit側の方がシンプルに書けた点)。
- 選択ハイライトの描画順序がAndroid版と異なる: Android版はセル背景の前に
  半透明色を敷くが(`bg == テーマ既定色`の場合は背景描画自体を省略しているため
  それで見える)、iOS版は各セルの背景を無条件に不透明で塗る既存実装のため、
  選択ハイライトはセル描画の**後**にオーバーレイとして重ねる方式にした
  (視覚的な見た目はほぼ同じ)。
- `TerminalView.swift`にフローティングコピー/キャンセルツールバーを追加
  (`UIPasteboard.general.string`でクリップボードへ書き込み)。
- タスク名に「ペースト」も含まれていたため、`TerminalAccessoryBar`に「貼付」
  ボタンも追加した(Android版のCtrl行の同等ボタンに相当、`terminalCommitTextBytes`
  でbracketed paste modeを考慮、既存UniFFI関数をそのまま利用)。Android版では
  この貼付ボタン自体は将来のアクセサリバー拡充タスク(#52)側にあるが、#48の
  タスク名が「選択/コピー・ペースト」を明記しているため、ここで最小限の
  貼付導線だけ先に用意した(#52では^D/^Z/定型文シート等を追加する)。

config構築ロジックと同様、`offsetToCellPos`/`reconstructSelectionText`は
ネットワーク非依存の純粋関数としてテストした(`TerminalSelectionTests.swift`、
`Tests/IsekaiTerminalCoreTests`、Keychain等に触れないため素のターゲットで動く)。

### Phase 1F-2/1F-3/1F-4(#49/#50/#51)実装メモ(2026-07-05)

3タスクまとめて実装した(いずれもAndroid版`TerminalScreen.kt`/`ui/TerminalTheme.kt`の
既存機能をSwiftへ1:1移植する作業で、新規のRust側変更は不要だった)。

- **#49(ピンチズーム)**: `TerminalScreenView`に`UIPinchGestureRecognizer`を追加。
  クランプ計算(`clampedFontScale`、0.5〜3.0)を`@objc`ハンドラから分離した純粋関数に
  してテスト容易にした。永続化はAndroid版`SharedPreferences`の`"font_scale"`キーと
  対称の`UserDefaults`キーへ`@AppStorage`経由で行う。
- **#50(配色テーマ)**: 新規`TerminalThemes.swift`(Default Dark/Solarized Dark/
  Dracula/Nordの4プリセット、Android版`ui/TerminalTheme.kt`と同じ値)。
  `setTerminalTheme(ansi16:defaultFg:defaultBg:)`はセッション単位ではなくRust側の
  グローバル状態への設定関数だったため、`TerminalSessionController.connect()`の
  冒頭で`resolveTheme().apply()`を呼ぶだけで済んだ(Android版のような
  タブ毎の`pushThemeToSession`配線は現時点でiOSがシングルタブのため不要、
  複数タブ対応(#54)時に再検討)。Global default(`ProfileListView`の配色テーマ
  選択、`UserDefaults`)→ Profile default(`ProfileEditView`の`themeName`上書き)の
  解決順はAndroid版`TerminalTabsViewModel.openTab`と同じ。
- **#51(スクロールバックのスワイプ)**: `TerminalScreenView`に`UIPanGestureRecognizer`
  (`maximumNumberOfTouches = 1`)を追加。ライブの`ScreenUpdate`とスクロールバックの
  行から表示用updateを合成するロジックを`synthesizeDisplayUpdate`(新規
  `TerminalScrollback.swift`)として純粋関数に分離しテストした。長押し(選択)・
  ピンチ(ズーム)・pan(スクロール)の3ジェスチャが同一Viewに共存するが、UIKitの
  既定動作(同一View上の複数`UIGestureRecognizer`の同時認識は明示的に許可しない限り
  OFF)により、Android版がCompose側で手動分岐していたのと同じ排他制御が
  追加コード無しで得られた(気づきとして記録)。選択中にスクロールバックへ入っている
  場合、コピー対象もスクロールバック側の内容になるよう`TerminalView`の
  コピー処理を修正した(Android版`reconstructSelectionText(displayUpdate, sel)`と
  同じ)。

### Phase 1F-5/1G-1(#52/#53)実装メモ(2026-07-05、アクセサリバー拡充+定型コマンド)

#52(^D/^Z/定型文シート)は#53(定型コマンド管理画面)のデータモデルに依存するため、
まとめて実装した。Android版`data/Snippet.kt`(Room `@Entity`)+`SnippetCommands.kt`+
`SnippetListScreen.kt`/`SnippetEditScreen.kt`/`SnippetListViewModel.kt`/
`SnippetEditViewModel.kt`をSwiftへ1:1移植した。

- `ProfileDatabase.swift`に`Snippet`(GRDBレコード)+`v3_create_snippets`
  migrationを追加(`connection_profile`と違い、Android版もFK制約を付けていない
  ため、iOS版も`profileId`カラムに明示的なFK制約を付けない — プロファイル削除で
  スニペットが孤立しても実害が無いため)。`fetchSnippets(forProfileId:)`は
  Android版`SnippetDao.getForProfile`(`WHERE profile_id IS NULL OR
  profile_id = :profileId`)と同じクエリで、全プロファイル共通+指定
  プロファイル専用の両方を返す。
- `SnippetCommands.toBytes(command:appendNewline:)`はAndroid版と同じ正規化
  ロジック(`\r\n`/`\n`を`\r`に統一、`appendNewline`時は末尾に`\r`を追加)の
  純粋関数として移植・テストした。
- 新規`SnippetListView.swift`/`SnippetEditView.swift`(Android版の一覧/編集画面と
  同じUI)。`ProfileListView`のメニューに「定型コマンド」項目を追加し、
  `IsekaiTerminalApp.swift`のナビゲーションに`.snippetList`/`.snippetEdit`を追加した。
- `TerminalAccessoryBar`に^C/^D/^Zの制御バイト直接送信ボタン(Android版が
  トグル式Ctrlボタンとは別に持つ即時送信ショートカット)と「定型」ボタン
  (SwiftUI側の`showSnippetSheet`をトリガーするクロージャ経由)を追加した。
  定型コマンド選択シート(`SnippetPickerSheet`)は現在のプロファイルIDで
  `fetchSnippets(forProfileId:)`を呼び、選択したスニペットを
  `SnippetCommands.toBytes(snippet:)`で送信する。

### Phase 1G-2(#54)実装メモ(2026-07-05、複数タブ/複数セッション対応)

Explore agentでAndroid版`TerminalTabsViewModel.kt`(`TabState`/タブ追加・切替・
クローズ・`watchTab`による非アクティブタブの継続動作・単一FGS共有)と
`TerminalHostScreen.kt`(タブバーUI)を事前調査した上で実装した。

**スコープ限定(意図的)**: Android版のマルチセッション生存は共有Foreground
Serviceに支えられているが、iOSにFGS相当の仕組みは無い。このタスクは
「アプリがフォアグラウンドの間、複数セッションを同時に維持する」ことだけを
スコープとし、バックグラウンドでの生存は#14(バックグラウンド遷移対応)に
委ねる(Explore agentの調査結果を踏まえた判断)。

**Android版からの意図的なUX変更(プラットフォーム制約への適応)**: Android版は
タブ追加用の「+」を持たず、プロファイル一覧へ戻って再接続することで新規タブを
開く(`tabsVm`がActivity scopeで生き続けるため可能)。iOSの`NavigationStack`は
ポップされたdestinationを保持しない(破棄される)ため、同じ手段を採ると
タブ一覧画面から一度離れただけで全セッションが切断されてしまう。そのため
iOS版はタブバーに明示的な「+」ボタンを持たせ、タブ一覧画面(`TerminalTabsHostView`)
から離れずに新しいタブを開けるようにした。`.navigationBarBackButtonHidden(true)`
も設定し、システムの戻るジェスチャで誤って全セッションを切断しないようにした
(Android版の「全タブを閉じたときだけ一覧へ戻る」という設計と実質的に同じ効果)。

**実装**:
- `TerminalTabsModel`(新規`TerminalTabsHostView.swift`): `[Tab]`(`profile`+
  `controller`)+`activeTabId`を持つ。`openTab`はAndroid版`openTab`と同じく
  タブを開いた瞬間に`controller.connect()`を呼ぶ(Viewのマウントタイミングに
  依存しない)。
- `TerminalView`のinitを「profileから内部でcontrollerを構築する」方式から
  「外部(`TerminalTabsModel`)が構築したcontrollerを受け取る」方式へ変更した。
  `.onAppear`での`connect()`呼び出しは削除した(`openTab`が既に呼んでいるため、
  二重呼び出しを避ける)。
- 複数タブの`TerminalView`を`ZStack`に**同時にマウントしたまま**、非アクティブな
  ものは`.opacity(0)`+`.allowsHitTesting(false)`にする方式にした(Android版が
  全タブのComposableをコンポジションに残したままゼロサイズにするのと同じ狙い:
  スクロール位置・選択範囲・IME状態をタブ切替時も保持する)。
- `isActive`フラグを`TerminalView`→`TerminalInputRepresentable`まで通し、
  アクティブなタブだけがIMEのfirst responderを持つようにした(非アクティブに
  なったら`resignFirstResponder()`)。これが無いと複数タブ同時マウント時に
  どのタブがキーボード入力を受け取るか不定になる。
- タブバー(`TerminalTabsHostView.tabBar`)の状態ドット(緑=接続済み/黄=接続中/
  灰=それ以外)は各タブの`controller.uiState`を`@ObservedObject`で直接観測する
  (`TabChip`)。テーマ切替🎨相当の機能はこのタスクのスコープ外。

**テスト**: `TerminalTabsModel`の状態遷移(`openTab`/`setActiveTab`/`closeTab`)を
ネットワーク非依存でテストした(`TerminalTabsModelTests.swift`、存在しない
`keyEntryId`を使い`resolveAuth`が即座に失敗して`connect()`がネットワークに
触れる前に終わることを利用、既存の`TerminalSessionControllerTests`と同じ手法)。

### iOS Linux CI: IsekaiTerminalCoreLogicの切り出し(2026-07-05、「Linux側テストを充実させて」指示を受けて着手)

これまでiOS側の自動テストはすべて`macos-26`ランナー(`ios-rust-core-check.yml`/
`ios-app-build-check.yml`/`ios-ssh-vertical-slice-check.yml`)経由でしか動かせず、
`ios/Package.swift`が`platforms: [.iOS(.v16)]`+XCFramework(`.binaryTarget`、Apple専用
パッケージング形式)+GRDBに依存していたため、Swift側のロジックをLinux上で
`swift test`することは原理的に不可能だった。Mozillaの`rust-components-swift`の方針
(「Rust coreは`cargo test`で分厚く、Swift境界は薄い契約テストに絞る」)を参考に、
`ios/Sources/IsekaiTerminalCore`から UIKit/SwiftUI/GRDB/Keychain に依存しない部分を
`ios/Sources/IsekaiTerminalCoreLogic`という新ターゲットへ切り出した。

**切り出した内容**(いずれも実機/シミュレータ不要、Rust側とのFFI境界を含む):
`TerminalKeyMapper`(キー→制御シーケンス変換、Rust `terminal_ctrl_byte`等への薄いラッパー)、
`TerminalScrollback`(`synthesizeDisplayUpdate`)、`TerminalSelection`
(`offsetToCellPos`/`reconstructSelectionText`。引数を`CGFloat`→`Double`に変更、
`CoreGraphics`はLinuxに無いため)、`NetworkPathPolicy`/`NetworkPathObserver`、
`SshHostTrustStore`、`CallbackIngress`、`KeyManager`(ed25519生成+OpenSSH PEM
エンコード。`CryptoKit`の代わりにLinuxでは`#if canImport(CryptoKit)`分岐で
swift-cryptoの`Crypto`モジュールを使う。両モジュールとも`Curve25519.Signing.PrivateKey`の
APIが同一のため実装は無変更)、`SnippetCommands.toBytes(command:appendNewline:)`
(GRDBの`Snippet`型に依存するオーバーロードだけは`IsekaiTerminalCore`側の`extension`として残した)。
UniFFI生成物(`generated/`)も`IsekaiTerminalCoreLogic`側へ移した(内容はホストOS非依存で
Linux/macOSどちらで生成しても同一と確認済み)。

**Linux上でのFFIリンク方式**: XCFrameworkはApple専用パッケージング形式でLinuxでは
使えないため、`ios/Sources/IsekaiTerminalCoreFFILinux`という`systemLibrary`ターゲットを新設し、
`rust-core/scripts/build-linux-swift-ffi.sh`がネイティブビルドした`libisekai_terminal_core.so`
(`cargo build -p isekai-terminal-core`、クロスコンパイル不要)へ直接リンクする。uniffi-bindgenが
生成するmodulemapは常に`use "Darwin"`を含む(ホストOSに関係ない固定テンプレート)ため、
Linux版はこの行を除いたコピーを使う。

**`swift test`が全ターゲットを1つのテストプロダクトへ束ねる問題**: `swift test`は
(`--filter`を使っても)宣言されている全`testTarget`を1つの実行ファイルへリンクしようと
するため、`IsekaiTerminalCore`/`IsekaiTerminalCoreTests`(UIKit/GRDB/CryptoKit依存)がマニフェストに
存在するだけでLinux上の`swift test`がそれらのコンパイルに巻き込まれて失敗する。
`ios/Package.swift`全体を`#if os(Linux)`で分岐させ、Linux上では`IsekaiTerminalCoreFFIBinary`
(`.binaryTarget`)・`IsekaiTerminalCore`・`IsekaiTerminalCoreTests`自体をマニフェストから丸ごと除外する
ことで解決した(このディレクティブはクロスコンパイル先ではなく`swift build`/
`swift test`を実行しているホストOSで評価されるため、Xcode/macOS側の解決には影響しない)。

**検証**: Debian 12の開発機にSwift 6.3.3公式Linuxツールチェーンを導入し、実際に
`cd ios && swift test`を実行して50 tests中49 pass・1 skip(sshd fixture不在による
意図的スキップ)を確認した。CIは新設の`.github/workflows/ios-logic-linux-check.yml`
(`ubuntu-24.04`ランナー、`ios/.build`を`Package.resolved`のハッシュでキャッシュ)。
swift-cryptoの`CCryptoBoringSSL`(BoringSSLのC移植、約370ファイル)は初回ビルドが
数分かかるため、キャッシュヒット時の高速化を優先してこの構成にした。

**やらないこと(意図的にiOS専用のまま残した部分)**: `ProfileDatabase`(GRDB本体、
`ConnectionProfile`/`Snippet`レコード型含む)・`CredentialVault`/`RelayCredentialVault`
(Keychain依存)・`TerminalSessionController`(`ObservableObject`/`@Published`が
LinuxにないCombineに依存)・UIKit/SwiftUIの各View/ViewModel。実iOSアプリの
Simulatorビルド・実機相当の検証は引き続き既存のmacOSランナー3ワークフローが担当する
(このLinuxレーンは、それらより大幅に安く速い「ロジック層の一次ゲート」という位置づけ)。

**並行編集の記録**: この作業中、同一worktree(`worktree-ios-phase1`)で並行して
Phase 1G-2(#54)が実装され(`46d3881`→`1322c12`)、未コミットの`import IsekaiTerminalCoreLogic`
挿入を「紛れ込んだ不要import」と誤解してその2ファイル(`TerminalTabsHostView.swift`/
`TerminalView.swift`)から一旦削除するfixコミットが入った。`IsekaiTerminalCoreLogic`が正式な
依存になった時点で両ファイルとも実際に`SshHostTrustStore`/`SelectionRange`を使うため
importを再度追加した。`Tests/IsekaiTerminalCoreTests/TerminalTabsModelTests.swift`
(`SshHostTrustStore`を直接使用)にも同様に追加が必要だった。

**分離直後にCIで発覚した3件の不整合(このコミットで修正)**:
1. `ios-rust-core-check.yml`/`ios-ssh-vertical-slice-check.yml`が使う
   `xcodebuild test -scheme IsekaiTerminalCore`が「Scheme IsekaiTerminalCore is not currently
   configured for the test action」で壊れた。`IsekaiTerminalCoreLogic`分離により
   自動生成スキームが`GRDB-Package`/`IsekaiTerminalCore`/`IsekaiTerminalCore-Package`/`IsekaiTerminalCoreLogic`
   の4つになり、個別ターゲットスキーム(`IsekaiTerminalCore`)にはテストアクションが
   構成されなくなったため。全ターゲットのテストを束ねる`IsekaiTerminalCore-Package`へ
   両ワークフローの`-scheme`を変更した(`ios/README.md`の該当箇所も追随)。
2. 同じ理由で`ios-ssh-vertical-slice-check.yml`の
   `-only-testing:IsekaiTerminalCoreTests/KeyManagerTests/...`が誤り(`KeyManagerTests`は
   `IsekaiTerminalCoreLogicTests`へ移動済み)だったため修正。
3. `ios/App/IsekaiTerminalApp/ContentView.swift`/`DiagnosticCallbackBridge.swift`
   が`DiagnosticCallback`(生成UniFFIバインディング、`IsekaiTerminalCoreLogic`へ移動済み)を
   使うにもかかわらず`import IsekaiTerminalCore`のみで`import IsekaiTerminalCoreLogic`が無く、
   「cannot find type 'DiagnosticCallback' in scope」でビルドが壊れていた
   (`IsekaiTerminalCore`は`IsekaiTerminalCoreLogic`を`@_exported import`していないため、依存先の
   publicな型は自動では見えない)。

### Phase 1C(#14)実装メモ(2026-07-05、バックグラウンド遷移対応)

**スコープ**: iOS版のバックグラウンド遷移対応のうち、Swiftだけで完結する範囲
(OSライフサイクルの中継+フォアグラウンド復帰時の再接続)に限定した。Rust側の
`SessionSupervisor`/2軸FSM(`SessionState`/`ExecutionMode`)によるバックグラウンド専用
UniFFI APIの追加は#24のスコープとし、このタスクでは行わない(既存の
`.claude/rules/rust-ssot.md`が要求する「セッション状態の判断はRust側」という原則には、
現時点でRust側にバックグラウンド遷移という概念自体が無いため踏み込めず、#24でその
概念を導入した後にこのSwift側実装を作り直す前提)。

**実装内容**:
- `TerminalTabsModel`(`TerminalTabsHostView.swift`)が`UIApplication.didEnterBackgroundNotification`/
  `.willEnterForegroundNotification`をクロージャベースの
  `NotificationCenter.addObserver(forName:object:queue:using:)`で購読する
  (このクラスは`NSObject`を継承しないプレーンな`ObservableObject`のため、
  `#selector`/`@objc`方式は使えない)。バックグラウンド遷移時は
  `UIApplication.beginBackgroundTask(withName:expirationHandler:)`で約30秒の猶予を得る
  (AndroidのForeground Service相当の仕組みがiOSに無いため、あくまでベストエフォート。
  この猶予中も実際のセッション生存はhelper側の論理セッション/QUIC idle timeoutに
  委ねる方針で、`ios/README.md`のPhase 0-5の机上調査結論と一貫させている)。
  フォアグラウンド復帰時は全タブに対し`controller.reconnect()`を呼ぶ。
- `TerminalSessionController.reconnect()`(新規): `.connecting`/`.connected`中は
  二重接続防止のため無視し、`.disconnected`/`.failed`の場合のみ`connect()`を
  最後に使ったcols/rowsで呼び直す(既存セッションはRust側にresumeの概念が無いため
  単純に破棄して作り直す)。
- `TerminalView`の切断/エラーオーバーレイに「再接続」ボタンを追加
  (自動復帰後もなお繋がらない場合の手動リトライ手段)。

**やらないこと**: trzsz転送中のバックグラウンド遷移時の扱い(#25)、
isekai-helperの実際のresumeハンドシェイク(#26、現状の`reconnect()`は
「新規セッションとして繋ぎ直す」だけで、Android版のresumeトークンによる
連続性維持とは異なる)。

### Phase 1C(#24)実装メモ(2026-07-05、SessionSupervisorと2軸FSM)

**スコープ決定**: 外部レビュー(2026-07-04、本ファイル1689行目以降)が提案した
`SessionState`(8状態: Disconnected/Connecting/Active/Quiescing/Suspended/
Resuming/Closing/Closed)×`ExecutionMode`(Foreground/Background)の2軸FSMを、
新規`rust-core/src/session_supervisor.rs`の`SessionSupervisor`(UniFFI Object)
として実装した。既存`SessionOrchestrator`(`orchestrator.rs`)の`ConnPhase`
(Idle/Connecting/Connectedの3状態、Androidが使用中)を置き換えるか、iOS側を
低レベルAPI(`SshSession`等、`SessionCallback`直接実装)から`SessionOrchestrator`
経由に移行するかは、#30実装メモで「#24のスコープと合わせて改めて判断する」と
していた通り本タスクの検討事項だったが、次の理由で**両者とも据え置き、
`SessionSupervisor`は独立した新規オブジェクトとして追加するに留めた**:

- Android/iOSとも現行の接続コード(`SessionOrchestrator`/低レベルAPI)は
  #20〜#54で実機/CI検証済みで実際に動いている。`ConnPhase`を8状態FSMへ
  置き換える、あるいはiOSを`SessionOrchestrator`経由に移行するのは、
  どちらもUniFFI境界・コールバック配線・既存テスト一式に影響する大きめの
  リファクタで、実機検証環境が無い(Linux開発機+CI simulatorのみ)状況では
  リスクに見合わない。
- 一方で「Rust側にSessionState/ExecutionMode FSMを実装する」というタスクの
  核心(判断ロジックをRust SSOTへ置く、`.claude/rules/rust-ssot.md`)自体は、
  既存の接続コードと結合させなくても単体で満たせる。`SessionSupervisor`を
  独立オブジェクトにしたことで、実transportに一切触れない純粋な状態機械として
  ハードウェア/ネットワーク不要で網羅的にテストできた(13テスト、全遷移パターン
  カバー、`rust-core-test-coverage-audit`の方針と同じ「ハードウェア不要な
  Rustロジックはまず単体テストで固める」を踏襲)。
- `SessionOrchestrator`への統合(`ConnPhase`を`SessionState`へ差し替え、
  `prepare_for_background`等を`SessionOrchestrator`のメソッドとして生やす)や、
  iOS`TerminalSessionController`が`SessionSupervisor`を実際に参照して
  `#14`の`reconnect()`をより賢く(例: `Suspended`時のみ再接続、`Quiescing`中は
  何もしない、等)する統合作業は、明示的に**未実施のまま次フェーズ以降へ持ち越す**
  (現時点でこの新APIを呼び出すKotlin/Swiftコードは無い)。

**実装内容**:
- `rust-core/src/session_supervisor.rs`(新規): `SessionState`/`ExecutionMode`
  (いずれも`#[derive(uniffi::Enum)]`)、`SessionSupervisor`(`#[derive(uniffi::Object)]`、
  `create_session_supervisor()`で生成)。メソッド: `session_state()`/
  `execution_mode()`(getter)、`on_connect_requested()`/`on_connected()`/
  `on_connect_failed()`/`on_disconnected()`(接続ライフサイクル)、
  `prepare_for_background(budget_ms)`/`resume_from_foreground()`/
  `mark_suspended()`/`memory_warning()`(バックグラウンド遷移、外部レビュー論点1・3・10
  相当)、`application_will_terminate()`/`on_terminated()`(終了)。
- 遷移の要点: `prepare_for_background`は`Active`の場合のみ`Quiescing`へ遷移する
  (`Disconnected`/`Connecting`中の場合はそもそも維持すべきセッションが無いため
  状態を変えない)。`Quiescing`中に`resume_from_foreground`が来れば猶予内に
  復帰できたとみなし`Active`へ戻すが、`mark_suspended`(猶予切れ)や
  `memory_warning`(iOSの`didReceiveMemoryWarning`相当、保守的に早期`Suspended`扱い)
  を経由していた場合は`Resuming`にし、呼び出し側が実際に再接続してから
  `on_connected()`を呼ぶまで`Active`に戻さない。`budget_ms`自体はRust側でタイマー
  管理せず記録もしない(外部レビュー論点10: Swift/Rustで基準時計を共有していない
  ため、実際の期限判断はSwift側`beginBackgroundTask`失効コールバックが正という
  既存方針をそのまま踏襲)。
- `lib.rs`に`pub mod session_supervisor;`と
  `pub use session_supervisor::{create_session_supervisor, ExecutionMode, SessionState, SessionSupervisor};`
  を追加。Kotlinバインディング(`android/src/main/kotlin/uniffi/isekai_terminal_core/isekai_terminal_core.kt`)
  を`generate-swift-bindings.sh`と対の手順(`uniffi-bindgen -- generate --library
  target/debug/libisekai_terminal_core.so --language kotlin`)で再生成し直コミット
  (CLAUDE.mdの「Rust の public API を変更したら Kotlin バインディング再生成が必須」
  に従う。新規追加のみで既存生成物への変更は無いことを確認済み)。iOS向けSwift
  バインディング(`ios/Sources/IsekaiTerminalCoreLogic/generated/`)はCI
  (`ios-rust-core-check.yml`の「Generate Swift bindings and check for drift」)が
  次回実行時に自動生成・差分チェックする。

**命名の注意**: `crate::session_state::SessionState`(1セッション分のVTE/trzsz
パーサー状態、`pub(crate)`でUniFFI非公開)と`session_supervisor::SessionState`
(このタスクの8状態FSM、UniFFI公開)は同名だが別モジュールの別型。前者は
クレート内部でのみ使われ、`pub use`で再エクスポートされていないため名前衝突は
発生しない(`session_supervisor.rs`冒頭に両者の関係を明記するdocコメントを残した)。

**やらないこと(次フェーズ以降へ持ち越し)**: `SessionOrchestrator`/`ConnPhase`との
統合、iOS`TerminalSessionController`/Android`TerminalTabsViewModel`からの実際の
呼び出し配線、`budget_ms`の実際の算出ロジック(現状#14のiOS実装は固定値を渡す
想定すらしておらず、Swift側`beginBackgroundTask`のexpirationHandlerで検知するのみ)。

### Phase 1C(#25)実装メモ(2026-07-05、trzszファイル転送とサンドボックス橋渡し)

**アーキテクチャ調査(Explore agent)の要点**: iOS側`TerminalSessionController`は
低レベルの`SessionCallback`を直接実装しており(#24で`SessionOrchestrator`統合は
見送り済み)、trzsz関連の4コールバック(`onTrzszRequest`/`onTrzszDownloadChunk`/
`onTrzszProgress`/`onTrzszFinished`)は空スタブのままだった。Android版は
`SessionOrchestrator`経由の高レベルAPI(`onTrzszStateChanged`+ダウンロード完了時に
まとめて全バイト列を受け取る`onDownloadComplete`)に移行済みだが、iOS側は生の
チャンク単位コールバックをそのまま受ける必要がある(`on_trzsz_download_chunk`が
プロトコルのDATAフレーム単位で逐次発火する、Rust側にはAndroidのような
「ダウンロード全体をバッファしてから渡す」機構が無い)。

**実装内容**:
- `TrzszUiState`(新規`TrzszTransfer.swift`): Android版`TrzszUiState`(sealed class)
  と対称の3ケースenum(`waitingUser`/`inProgress`/`done`)。`TerminalUIState`に
  `@Published trzszState: TrzszUiState?`と`completedDownloadURL: URL?`を追加。
- `ActiveTerminalSession`プロトコルに`trzszAcceptUpload`/`trzszSendChunk`/
  `trzszAcceptDownload`/`trzszCancel`を追加(生成済みUniFFIバインディングの
  6セッション型全てが既に同名メソッドを持つため、`extension X: ActiveTerminalSession {}`
  の事後適合だけで済んだ)。
- アップロード: `trzszStartUpload(url:)`が`.fileImporter`で選択されたsecurity-scoped
  URLを受け取り、バックグラウンドキューで`startAccessingSecurityScopedResource`の
  スコープ内で読み込む。Android版`TerminalTabsViewModel.trzszStartUpload`と同じ
  64KBチャンク+「1チャンク先読みしてisLastを判定」方式。この読み出しループ自体は
  `TerminalSessionController.trzszSendChunked(readNext:send:)`という純粋関数
  (`Data`のクロージャベース)に切り出し、実ファイルI/Oなしで境界条件
  (0バイトファイル・ちょうどchunkSize境界)を単体テストできるようにした
  (`makeSshConfig`等、#30以来のパターンの踏襲)。
- ダウンロード: `trzszStartDownload()`が`transferId`単位の一時ディレクトリ
  (`FileManager.default.temporaryDirectory/trzsz-<transferId>/`)に書き込み用
  `FileHandle`を開いてから`trzszAcceptDownload`を呼ぶ。`onTrzszDownloadChunk`は
  そのハンドルへ逐次`write`する(Rustスレッドから直接呼ばれるためMainActorへは
  ホップしない)。`transferId`でnamespaceしているのは、同じ`suggestedName`を持つ
  別タブ/別転送が同じ一時パスへ衝突するのを避けるため。
- `onTrzszFinished`(成功かつdownloadモードの場合のみ)で`completedDownloadURL`を
  設定し、UI側は`.fileMover(isPresented:file:)`(iOS 16+、既存ファイルURLをそのまま
  ユーザー選択の保存先へコピーできる)でFilesアプリ等への保存を提供する。
  `.fileExporter`(`FileDocument`が必要)ではなく`.fileMover`を選んだのは、
  既に一時ファイルとしてディスク上に存在するものをそのまま渡せるため
  (メモリに載せ直す必要がない)。
- `TrzszTransferSheet`(新規、Android版`TrzszTransferSheet.kt`のModalBottomSheetと
  対称の3状態表示)。完了画面はダウンロード成功時のみ「保存」ボタンを出し、それが
  ローカルな`@State showTrzszFileMover`(`uiState.completedDownloadURL`を直接
  isPresentedに使わなかったのは、ユーザーが保存をキャンセルした場合に正しく
  閉じられる必要があるため)経由で`.fileMover`を開く。
- `trzszDismiss()`(クライアント側のみ、Rust API呼び出しなし): `trzszState`/
  `completedDownloadURL`をクリアし、一時ディレクトリを削除する(`.fileMover`で
  既に書き出し済みでも、コピー先とは別物の一時ファイルなのでどのみち不要)。

**やらないこと**: ディレクトリ転送(`mode == "dir"`)のUI対応(Android版も未対応、
`TrzszUiState.mode`は文字列のまま素通しし、"upload"/"download"以外は現状
`waitingUserView`の`else`分岐(ダウンロード扱い)に落ちる。実害があれば別タスクで
分岐追加)。バックグラウンド遷移中の転送継続保証(#24の`SessionSupervisor`との
統合が前提になるため、#24の「やらないこと」と同じ理由で見送り)。

### Phase 1C(#26)実装メモ(2026-07-05、isekai-helper再接続・resume対応)

**アーキテクチャ調査(Explore agent)の最重要な発見**: isekai-helper経由のresume
(セッション断からの再接続)は**Rust側で既に完全に透過的に動作しており、この
タスクで新規に追加すべきresume APIは存在しない**。`rust-core/src/resume_client.rs`
の`ReattachableStream`が、`HelperQuicSession`/`MultipathHelperQuicSession`/
`IsekaiStunP2pSession`/`IsekaiLinkRelaySession`のQUIC接続断をI/Oエラーとして
russh/transportに見せる前に検知し、内部で新しいQUIC接続を張り直して
`RESUME`ハンドシェイク(`session_id`+送受信オフセット)を行い、再送すべき
C→Sバイトをreplayしてから元のストリームへ差し替える。この一連の処理は
セッションオブジェクト(タスク)が生きている限り完全に自動で、UniFFI越しに
明示的な`resume()`呼び出しは一切不要(生成済みSwiftバインディングにも
`resume`/`reattach`に相当するメソッドは存在しないことを確認済み)。iOS側の
`TerminalSessionController.reconnect()`(#14)は`.disconnected`/`.failed`の場合の
みフレッシュ接続を張り直すが、これは`ReattachableStream`が(既定
`REATTACH_MAX_RETRIES`到達後)本当に諦めた後にしか`.disconnected`にならないため、
既存実装のままで矛盾なく動作する。

**実際に見つかったギャップ**: #23で作った`NetworkPathPolicy`/`NetworkPathObserver`
(NWPathMonitorの生イベントをdebounce/coalesceしてRustへ通知するタイミングだけを
決める判断層、実際の接続可否判断はRust側に委ねる設計で元々書かれていた)が、
実機の`NWPathMonitor`と一切結線されておらず「宙に浮いていた」。Android版は
`SessionOrchestrator::notify_network_lost()`(`ConnPhase`に応じてQUICなら無視/
非QUICや接続中なら中断)を持つが、iOS側は`SessionOrchestrator`を経由しない
低レベルセッションを直接使う(#24決定通り)ため、この判断ロジックを呼べる
Rust APIが存在しなかった。

**実装内容**:
- `rust-core/src/session.rs`の`SessionCore`に`connected: Arc<AtomicBool>`を追加
  (`start()`でfalseにリセット、`TransportEvent::Connected`受信時にtrueへ、
  `Disconnected`/チャネルクローズ時にfalseへ戻す)。
- `SessionCore::notify_network_lost(is_quic: bool)`を追加。判断ロジック本体は
  `should_abort_on_network_lost(has_session, connected, is_quic) -> bool`という
  純粋関数に切り出し(Idleなら無視/ハンドシェイク中は中断/接続済みQUICは
  transport自身のtransparent resumeを信頼して無視/接続済み非QUICは中断、の
  4パターンを実チャネル・tokioタスク無しで単体テストした、4 tests)。
  実際の中断は既存の`disconnect()`をそのまま呼ぶ(新しいteardown経路を書かず、
  既に使われている安全な経路に乗せる)。
- `SshSession`(`is_quic=false`)、`HelperQuicSession`/`MultipathHelperQuicSession`/
  `IsekaiStunP2pSession`/`IsekaiLinkRelaySession`(いずれも`is_quic=true`)に
  `notify_network_lost()`を追加。UniFFI経由でSwift/Kotlin双方のバインディングを
  再生成した(Androidは`SessionOrchestrator`経由のままなので、この新APIを
  呼び出すKotlinコードは無い)。
- iOS側`TerminalSessionController`に実`NWPathMonitor`を追加し、`init`で
  `startNetworkPathMonitoring()`を呼んで生イベントを`networkPathObserver`
  (既存#23の判断層)へ転送するだけにする(生イベントの中継のみ、判断は一切
  しない、`.claude/rules/rust-ssot.md`)。`networkPathObserver`のonNotifyが
  実際に呼ばれた場合、`isSatisfied == false`(断)の時だけ
  `session?.notifyNetworkLost()`を呼ぶ(`isSatisfied == true`(復帰)に対応する
  Rust APIは無いため何もしない — 復帰の検知は既存のtransport再試行/#14の
  フォアグラウンド復帰時`reconnect()`に委ねる)。`deinit`で`cancel()`する。

**やらないこと**: `SessionOrchestrator`側への同種統合(Android版は`ConnPhase`
ベースの独自実装を既に持っており、影響なし)。QUICのpath recovery通知
(`isSatisfied == true`)に対応するRust APIの新設(現時点で必要性が無いため)。
実機でのローミング/圏外復帰の実地検証(開発機にmacOS/実機が無いため、
Simulatorでは`NWPathMonitor`の実経路変化を再現できない。既存のPhase 9-4等と
同じ理由で実機検証は次フェーズ(#28)へ持ち越し)。

---

## Phase S-7: isekai-ssh CLI — musl静的バイナリ配布・実機検証（2026-07-04）

`isekai-ssh`（`rust-core/isekai-ssh/`、`ISEKAI_SSH_DESIGN.md`参照。`isekai-terminal-core`とは独立した、
`ssh`のProxyCommandとして使う単体CLIツール）の実装フェーズ分割案のS-7に対応する。

### muslビルド（このセッションのサンドボックス環境で実施・確認済み）

`rust-core/isekai-helper/`向けの`build-isekai-helper-musl.sh`（Phase 7-2）と同じ手法
（`cargo-zigbuild`でzigをCクロスコンパイラ/リンカに使い、musl-gcc等のシステムトゥールチェーン
不要）を転用し、`rust-core/scripts/build-isekai-ssh-musl.sh`を新設した。**配布用ビルドでは
`--dev-insecure-*`系フラグを有効化する`dev-insecure` feature（`ISEKAI_SSH_DESIGN.md`「実装方針」
節参照）を明示的に有効化しない**（デフォルトfeatureのみでビルド）。

このセッションには実ネットワークデバイスは無いが、ビルド自体はこの環境で実際に実行できたため、
以下を実機（このマシン上でのx86_64ネイティブ実行）で確認した:

- `cargo zigbuild --release -p isekai-ssh --target x86_64-unknown-linux-musl` /
  `--target aarch64-unknown-linux-musl` の両方がビルド成功。
- `file`で両バイナリとも `ELF 64-bit LSB executable, ..., statically linked, stripped`
  （x86_64: 5,797,520 bytes、aarch64: 5,003,464 bytes）であることを確認。
- x86_64バイナリを実際に実行し、トップレベル・`connect`/`init`/`login`/`logout`各サブコマンドの
  `--help`出力に`--dev-insecure-*`系フラグが一切出現しないことを確認（`isekai-ssh/tests/
  help_purity.rs`が検証している不変条件と同じ）。同テストをデフォルトfeatureのみで実際に実行し
  `release_build_connect_help_never_mentions_dev_insecure_flags ... ok`のpassを確認した。
- 両バイナリのsha256を記録（`.sha256`ファイル、helper版と同じ運用）。

### 配布方法

`isekai-ssh`は個人が`~/.ssh/config`に`ProxyCommand isekai-ssh connect %h`として置く手元用
CLIツールであり、サーバー側常駐の`isekai-helper`向けに構築したLinuxbrew tap（Phase 7-6、
`cuzic/homebrew-isekai-terminal`）ほどの配布インフラは現時点の利用規模では不要と判断した。
GitHub Releaseにx86_64/aarch64両musl静的バイナリ+sha256を添付し、ユーザーが手動ダウンロード→
sha256照合→配置→`~/.ssh/config`設定、という軽量な配布に留める方針を`ISEKAI_SSH_DESIGN.md`の
「S-7実施結果」節に記録した（詳細はそちらを参照）。Homebrew tap等のパッケージマネージャー配布は
将来ニーズが顕在化してから検討する。

### ローカルでのrelay/resume疎通確認（本フェーズ以前に完了済み、参考情報として整理）

複数ネットワーク環境をまたぐ実機検証とは別に、ローカル環境での relay/resume 疎通自体は
Phase S-4（resume本実装のサブフェーズ、`ISEKAI_SSH_DESIGN.md`参照）のe2eテスト群
（`isekai-ssh/tests/resume_reconnect_e2e.rs`・`resume_window_exceeded_e2e.rs`・
`resume_multi_disconnect_e2e.rs`）で、実バイナリ・実UDPブラックホールプロキシによる
フォルト注入を使い既に厳密に確認済みである。今回のS-7は「配布用バイナリのビルド手順の
再現性」の検証が主眼であり、relay/resumeのプロトコルレベルの正しさそのものはS-4完了時点で
担保されている。

### 実機ネットワーク検証（未実施、フォローアップが必要）

複数ネットワーク環境（宅内Wi-Fi NAT配下ホスト・モバイル回線クライアント）をまたいだ
実relay疎通・resume・ローミング挙動の実機検証は、**このサンドボックス環境（単一マシン、
実ネットワークデバイス無し）の制約上、今回は実行不可能であり未実施**。これはPhase 9-4・
Phase 10-5で`isekai-terminal-core`/Android側の実機検証が同じ理由（セッションに実ネットワークデバイスを
持つ端末が接続されていない）で保留されているのと全く同じ扱いであり、疑似的な検証で
代替することはしていない。次回、実際のWi-Fi NAT配下ホスト＋モバイル回線クライアントの
組み合わせを用意できる環境で、`phase7-5-roaming-test.sh`相当の手動検証を行う必要がある。

対象外（本フェーズの範囲では）: Homebrew tap等の追加配布インフラの新設、署名検証の導入
（`ISEKAI_SSH_DESIGN.md`「オープンな課題」参照、未着手のまま）。

---

## tty完全実装タスク（#18〜）で対象外(won't do)と判断した機能

### Kitty graphics画像プロトコル対応（タスク#53）

Sixel対応（タスク#42）とは独立に検討したが、**現行の `vte` crate（0.13.1、`Cargo.lock`で固定）の
標準的な`Perform`実装だけではAPCシーケンスを受信できない**と判断し、対象外(won't do)とする。

**確認した制約（`~/.cargo/registry/.../vte-0.13.1/src/table.rs` と `src/lib.rs` を実際に読んで検証済み）**:

- Kitty graphicsはAPC（`ESC _ ... ST`）ベースのプロトコル。
- vte 0.13.1のステートマシンは`Escape`状態で`0x5f`（`_`）を受けると`SosPmApcString`状態
  （APC/PM/SOS共通）に遷移するが、`table.rs`の`SosPmApcString`エントリでは
  ペイロードバイト（`0x20..=0x7f`）に対するアクションが**すべて`Ignore`**であり、
  DCS（`DcsPassthrough`）のような`Put`アクションが一切割り当てられていない。
- さらに`lib.rs::perform_state_change`の状態離脱時の特別処理（`Unhook`/`OscEnd`呼び出し）は
  `State::DcsPassthrough`と`State::OscString`のみをハンドリングしており、
  `State::SosPmApcString`はこの分岐から漏れている。つまりAPCの開始・ペイロード・
  APCとしての終了通知は`Perform`トレイトへ一切コールバックされず、中身を読み捨てる
  どころか存在自体が`Perform`実装側から観測不能（8-bit ST `0x9c`で終端する場合は
  遷移アクションも`None`。7-bit ST `ESC \`で終端する場合のみ、`\`が通常の`esc_dispatch`
  として見えることがあるが、これはAPC終了の通知ではなく単なる無関係なエスケープ
  シーケンスとしての解釈であり、ペイロード内容の取得には繋がらない）。
- `Perform`トレイト自体にもAPC専用メソッドが無く（`hook`/`put`/`unhook`はdocstring上も
  DCS専用）、vteをforkするか前段でAPCシーケンスを抜き出す独自フィルタを挟まない限り
  受信すらできない。

**判断根拠**:

- 上記の通りvte forkまたは独自プリプロセッサの実装が必須であり、当初のタスク規模見積もりを
  大きく超える。
- Kitty graphics固有のquery（`a=q`等）にはdevice→host応答が必要（タスク#38のDA/DSR応答経路が
  前提）だが、それを満たしてもvte側の制約は解消しない。
- 画像表示という実利についてはSixel対応（タスク#42）でほぼ充足する。
- 上記理由により、Kitty graphics対応は実装せず対象外(won't do)としてタスク#53を完了扱いとする。
  将来vteの置き換え（別parser crateへの移行、または独自fork）を行う機会があれば再検討する。

### alt-screenでのwheel→矢印キー変換（xterm `?1007` Alternate Scroll Mode相当、タスク#50の範囲外）

タスク#50（Android側マウスレポーティング配線）のFableレビュー2次で、「マウスレポーティングが
Offの間、alt-screen（pager/vim等）表示中のホイールスクロールを上下矢印キーへ変換して送るか
（xterm `?1007` Alternate Scroll Mode相当）」を明示するよう求められた。検討の上、**この
サブ機能はタスク#50の範囲に含めず対象外(won't do、ただし将来のrust-core側タスクで再検討可能)**
と判断した。

**確認した制約**: `rust-core`（タスク#36）は`?1000`/`?1002`/`?1003`（マウスレポーティング
モード）と`?1006`（SGR拡張）は`Terminal`に状態として保持し`ScreenUpdate::mouse_reporting_mode`/
`sgr_mouse_mode`経由で公開しているが、**`?1007`自体の状態は一切保持しておらず、
`ScreenUpdate`は「現在alt screenかどうか」も公開していない**（`rust-core/src/lib.rs`の
`ScreenUpdate`定義、`grep -n "1007\|alt_screen" rust-core/src/lib.rs rust-core/src/terminal.rs`
で確認済み、ヒット無し）。

**判断根拠**:

- `.claude/rules/rust-ssot.md`の原則により、「今alt screenかどうか」「`?1007`が有効かどうか」
  というターミナル状態の判断ロジックはRust側（`Terminal`/`ScreenUpdate`）に置くべきで、
  Kotlin側で代替判定（例えばESCシーケンスの目視パースや別経路の状態推測）を持つのは
  避けるべきである。
- 上記の通りRust側に必要な状態（`?1007`保持・alt screen可視性の公開）が無いため、正しく
  実装するにはまず rust-core 側の変更（新しいタスク）が必要であり、UI配線のみを対象とする
  タスク#50の範囲を超える。
- 実利は「マウスレポーティングOffのままpager/vim等でホイールスクロールしたい」という
  限定的なケースであり、マウスレポーティング自体が有効な間のホイール処理（wheel up/down、
  タスク#50で実装済み）で大半のユースケース（アプリ側が明示的にマウスを要求している場合）
  はカバーされる。
- 将来 rust-core 側に`?1007`状態保持とalt screen可視性の`ScreenUpdate`公開を追加する機会が
  あれば、Android/iOS双方のUI配線を別タスクとして起票し再検討する。

### マウスレポーティング有効時、タッチユーザーの単一指スクロール/選択が使えない（タスク#50/#74、対象外・未実装のまま保留）

Fableレビュー（グループD、タスク#74）で指摘。`android/src/main/kotlin/tools/isekai/terminal/TerminalScreen.kt`の
`awaitEachGesture`内`mouseModeActive`分岐（およびiOS版`ios/Sources/IsekaiTerminalCore/TerminalScreenView.swift`の
`gestureRecognizer(_:shouldReceive:)`）は、マウスレポーティング（`?1000`/`?1002`/`?1003`）が有効かつ
`scrollOffset == 0`（スクロールバック表示中でない）の間、単一指タッチの press/drag/release を全て
`MouseButton.LEFT`のマウスイベントとしてRustへ転送する。これ自体はタスク#50/#51で意図した挙動（アプリが
マウスを要求している間はタッチをそのままマウス操作に転用する）だが、`less --mouse`や`tmux set -g mouse on`
のようにマウスレポーティングを有効化しつつ通常のタッチスクロール/選択も期待するアプリでは、単一指での
スクロールバックパン・長押しテキスト選択の手段が失われる。ただし完全にスクロール手段が無いわけではなく、
Androidは2本目の指が触れた時点でmouseドラッグからタスク#80の`runPinchAndPan()`へ引き継ぐため、2本指
ピンチ+パンでマウスモード中もスクロールバックへ入れる。iOSは`gestureRecognizer(_:shouldReceive:)`が
`panGestureRecognizer`自体を`isPointerReportingActive`中は拒否するため、タッチ由来の2本指パンは
`handlePan`に届かないが、`UIPinchGestureRecognizer`（`handlePinch`）はこのガードの対象外なので2本指
ピンチズームは維持される（2本指パンでのスクロールバック移動は不可）。いずれのプラットフォームでも
影響が及ぶのは主に単一指ジェスチャである。

**検討した対応案**:

- (A) 単一指の縦スワイプを wheel up/down（button 64/65、iOS版の間接[トラックパッド]scrollでタスク#81が
  既に行っている変換と同種）へ変換する。ただし、マウスドラッグ（vim visual modeやtmuxのマウス選択等）も
  正当なユースケースであるため、指の移動が「スクロール意図」か「ドラッグ選択意図」かを区別する閾値/
  ヒューリスティックの新規設計が必要。
- (B) 長押し等の一時バイパスでマウスモードの捕捉を抜け、通常の選択/スクロールへ切り替える。ただし
  「今はマウスモードの長押しではなく通常の長押しとして扱われている」ことをユーザーに伝える視覚的
  フィードバックが無いままでは誤操作を招きやすく、UX設計が別途必要。

**判断根拠**:

- (A)(B)いずれもジェスチャー閾値のチューニングや新規UXフィードバックの設計を要し、タスク#50/#80/#81の
  範囲（マウスレポーティング配線・ピンチ/ホイール対応）を超える。
- タスク#74のCodexレビューでも「実装するなら『wheel変換』と『一時バイパス/ドキュメント化』は別タスクに
  分けるべき」と指摘されており、単一タスクでの拙速な実装は避けるべきと判断した。
- タスク#87（マウスUI裁定ロジックのテストが0件）が既に別途起票されており、（A）（B）いずれを実装するに
  してもジェスチャーロジックへのテスト整備が前提になる。
- 上記の理由により、(A)(B)ともに実装せず対象外（保留）のまま残す。将来実装する場合は、(A)と(B)を
  それぞれ別タスクとして起票し、実機でのジェスチャー閾値チューニング・UXレビューを経てから着手する。

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
- rust-core/src/debug_fault.rs + android/src/debug/kotlin/.../FaultInjectionReceiver.kt: 実機での上記フォルト注入をライブに有効化する adb 経由のデバッグフック（release ビルドには含まれない）
- rust-core/scripts/phase7-5-roaming-test.sh: 実機ローミング耐性検証のシナリオ関数集（ライブフォルト注入 / 実ネットワーク切替 / 組み合わせ）
- quinn の rebind 前例: `quinn-0.11.11/src/endpoint.rs` の `Endpoint::rebind`/`rebind_abstract`、および quinn 自身のテスト `quinn-0.11.11/src/tests.rs: rebind_recv`（同じ手法の公式前例）
