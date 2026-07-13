use std::sync::Arc;
use std::time::Duration;
use parking_lot::Mutex;

use crate::{
    CellData, ClipboardPayload, ConnectionPublicState, ForwardState, OrchestratorCallback, ScreenUpdate,
    SessionCallback, SshConfig, SshError, TrzszPublicState, RUNTIME,
};
use crate::net_health_policy;
use crate::quic_transport::{QuicConfig, QuicSession};
use crate::isekai_pipe_quic_transport::{IsekaiPipeQuicConfig, IsekaiPipeQuicSession};
use crate::multipath_transport::{MultipathIsekaiPipeQuicConfig, MultipathIsekaiPipeQuicSession};
use crate::isekai_stun_p2p_transport::{IsekaiStunP2pConfig, IsekaiStunP2pSession};
use crate::isekai_link_relay_transport::{IsekaiLinkRelayConfig, IsekaiLinkRelaySession};

// ── Active session ────────────────────────────────────────

enum ActiveSession {
    Ssh(Arc<crate::SshSession>),
    Quic(Arc<QuicSession>),
    IsekaiPipeQuic(Arc<IsekaiPipeQuicSession>),
    MultipathIsekaiPipeQuic(Arc<MultipathIsekaiPipeQuicSession>),
    IsekaiStunP2p(Arc<IsekaiStunP2pSession>),
    IsekaiLinkRelay(Arc<IsekaiLinkRelaySession>),
}

/// `ActiveSession`の全バリアントに同じメソッド呼び出しを委譲するだけのmatchを
/// 展開する。6トランスポートすべてが同じ`SessionCore`委譲メソッドを持つため
/// （各transportモジュール参照）、ここは常に「アームごとの分岐ロジックが無い」
/// 純粋な委譲にのみ使う。`add_local_forward`/`remove_forward`のように一部の
/// トランスポートで挙動が違うメソッドは対象外とし、手書きのmatchのままにする。
macro_rules! dispatch_all {
    ($self:expr, $method:ident $(, $arg:expr)*) => {
        match $self {
            Self::Ssh(s) => s.$method($($arg),*),
            Self::Quic(s) => s.$method($($arg),*),
            Self::IsekaiPipeQuic(s) => s.$method($($arg),*),
            Self::MultipathIsekaiPipeQuic(s) => s.$method($($arg),*),
            Self::IsekaiStunP2p(s) => s.$method($($arg),*),
            Self::IsekaiLinkRelay(s) => s.$method($($arg),*),
        }
    };
}

impl ActiveSession {
    fn send(&self, data: Vec<u8>) {
        dispatch_all!(self, send, data)
    }
    fn resize(&self, cols: u32, rows: u32) {
        dispatch_all!(self, resize, cols, rows)
    }
    fn disconnect(&self) {
        dispatch_all!(self, disconnect)
    }
    /// マルチパス以外のセッションでは意味を持たないため何もしない
    /// （呼び出し側は「そのとき使っているtransportがマルチパスかどうか」を
    /// 意識せず日和見的に呼べばよい）。
    fn rebind_to_fd(&self, fd: i32, local_ip: String) {
        if let Self::MultipathIsekaiPipeQuic(s) = self {
            s.rebind_to_fd(fd, local_ip);
        }
    }
    /// #11: ユーザーが「今すぐWiFiに戻す」を要求した。マルチパス以外のtransportでは
    /// 何もしない(rebind_to_fdと同じ理由)。
    fn force_return_to_wifi(&self) {
        if let Self::MultipathIsekaiPipeQuic(s) = self {
            s.force_return_to_wifi();
        }
    }
    /// trzsz転送中(WaitingUser含む)かどうかをRebindManager(#22のDriver)の
    /// 静けさ判定の補助シグナルとして伝える。マルチパス以外では意味を持たないため
    /// `rebind_to_fd`と同じくno-op委譲。
    fn set_interactive_busy(&self, busy: bool) {
        if let Self::MultipathIsekaiPipeQuic(s) = self {
            s.set_interactive_busy(busy);
        }
    }
    fn scrollback_len(&self) -> u32 {
        dispatch_all!(self, scrollback_len)
    }
    fn scrollback_cells(&self, offset: u32, rows: u32) -> Vec<CellData> {
        dispatch_all!(self, scrollback_cells, offset, rows)
    }
    fn trzsz_accept_upload(&self, transfer_id: String, file_name: String, file_size: u64, mode: u32) {
        dispatch_all!(self, trzsz_accept_upload, transfer_id, file_name, file_size, mode)
    }
    fn trzsz_send_chunk(&self, transfer_id: String, data: Vec<u8>, is_last: bool) {
        dispatch_all!(self, trzsz_send_chunk, transfer_id, data, is_last)
    }
    fn trzsz_accept_download(&self, transfer_id: String) {
        dispatch_all!(self, trzsz_accept_download, transfer_id)
    }
    fn trzsz_cancel(&self, transfer_id: String) {
        dispatch_all!(self, trzsz_cancel, transfer_id)
    }
    fn add_local_forward(&self, id: String, bind_address: String, bind_port: u16, remote_host: String, remote_port: u16) {
        match self {
            Self::Ssh(s) => s.add_local_forward(id, bind_address, bind_port, remote_host, remote_port),
            Self::Quic(s) => s.add_local_forward(id, bind_address, bind_port, remote_host, remote_port),
            // ポートフォワードは MVP スコープ上プレーン SSH / tsshd QUIC のみ対応。
            // isekai-helper 経由の QUIC 系トランスポートは未対応（対象外）。
            Self::IsekaiPipeQuic(_) | Self::MultipathIsekaiPipeQuic(_) | Self::IsekaiStunP2p(_) | Self::IsekaiLinkRelay(_) => {
                log::warn!("add_local_forward: not supported over helper-QUIC transports");
            }
        }
    }
    fn remove_forward(&self, id: String) {
        match self {
            Self::Ssh(s) => s.remove_forward(id),
            Self::Quic(s) => s.remove_forward(id),
            Self::IsekaiPipeQuic(_) | Self::MultipathIsekaiPipeQuic(_) | Self::IsekaiStunP2p(_) | Self::IsekaiLinkRelay(_) => {
                log::warn!("remove_forward: not supported over helper-QUIC transports");
            }
        }
    }
    /// Phase 12: per-session theme。全トランスポート共通(`Terminal`/`SessionCore`は
    /// トランスポート非依存)なので、`add_local_forward`と違い対象外の分岐は無い。
    fn set_theme(&self, theme: crate::theme::Theme) {
        dispatch_all!(self, set_theme, theme)
    }
}

// ── Shared internal state ─────────────────────────────────

/// 接続状態の SSOT。`ConnectionPublicState` の Connecting/Connected の別を
/// Rust 側でも保持し、`notify_network_path_changed` がミラー無しで判断できるようにする。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnPhase {
    Idle,
    Connecting,
    Connected,
}

/// 直前に成功した(あるいは試みた)`connect_*`の種類とConfigを保持し、予期しない
/// 切断時に同じ接続を自動的に張り直せるようにする(tssh のUDPモード reconnect相当)。
/// 全Configは既に`Clone`実装済みなので、そのまま複製して再利用できる。
/// `IsekaiPipeQuic`と`IsekaiPipeQuicAuto`は同じ`IsekaiPipeQuicConfig`/セッション型を
/// 使うが呼ぶメソッド(`connect` vs `connect_auto`)が違うため、別バリアントとして区別する
/// (`connect_auto`はQUICブートストラップ失敗時に自動でTCP SSHへフォールバックする挙動を持つ)。
#[derive(Clone)]
enum LastConnectAttempt {
    Ssh(SshConfig),
    Quic(QuicConfig),
    IsekaiPipeQuic(IsekaiPipeQuicConfig),
    IsekaiPipeQuicAuto(IsekaiPipeQuicConfig),
    MultipathIsekaiPipeQuic(MultipathIsekaiPipeQuicConfig),
    IsekaiStunP2p(IsekaiStunP2pConfig),
    IsekaiLinkRelay(IsekaiLinkRelayConfig),
}

impl LastConnectAttempt {
    fn host_port_is_quic(&self) -> (String, u16, bool) {
        match self {
            Self::Ssh(c) => (c.host.clone(), c.port, false),
            Self::Quic(c) => (c.ssh_host.clone(), c.ssh_port, true),
            Self::IsekaiPipeQuic(c) | Self::IsekaiPipeQuicAuto(c) => (c.ssh_host.clone(), c.ssh_port, true),
            Self::MultipathIsekaiPipeQuic(c) => (c.ssh_host.clone(), c.ssh_port, true),
            Self::IsekaiStunP2p(c) => (c.ssh_host.clone(), c.ssh_port, true),
            Self::IsekaiLinkRelay(c) => (c.ssh_host.clone(), c.ssh_port, true),
        }
    }
}

/// 自動再接続ループのタイミング定数。`net_health_policy::NetPathPolicy`と同じ理由
/// (テストで短い値に差し替えられるようにする)で構造体化する。既定値はMVPとして
/// ハードコード(設定UIは作らない): tssh の `aliveTimeout` 相当が60秒。
#[derive(Debug, Clone, Copy)]
struct ReconnectPolicy {
    /// UIへライブ通知する間隔。
    tick: Duration,
    /// 実際に`connect_via`を試みる間隔(tickの整数倍)。
    retry_interval: Duration,
    /// これを超えて再接続できなければギブアップする。
    timeout: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(1),
            retry_interval: Duration::from_secs(3),
            timeout: Duration::from_secs(60),
        }
    }
}

/// trzsz ダウンロードの累積バッファに設ける上限(#60)。trzsz プロトコルの
/// `SIZE`(申告値)はサーバー側の自己申告に過ぎず強制されないため、悪意ある/
/// 壊れたサーバーが巨大な SIZE を申告して DATA を送り続けると `download_buf` が
/// 無制限に肥大化し端末が OOM でクラッシュし得る。実際に受信したバイト数の実測値
/// (`download_buf.len() + 今回のchunk長`)がこの上限を超えたら転送を中断する。
const MAX_DOWNLOAD_BUF_BYTES: usize = 2 * 1024 * 1024 * 1024; // 2 GiB

struct OrchestratorState {
    current_host: Option<String>,
    current_port: u16,
    is_quic: bool,
    phase: ConnPhase,
    /// Active transfer ID set by on_trzsz_request; used to route trzsz commands without exposing ID to Kotlin
    current_transfer_id: Option<String>,
    /// "upload" / "download" set on on_trzsz_request; used to detect download accumulation
    trzsz_mode: Option<String>,
    /// Accumulates bytes from on_trzsz_download_chunk; drained on on_trzsz_finished
    download_buf: Vec<u8>,
    /// #60: `MAX_DOWNLOAD_BUF_BYTES` を超えてローカルに中断した転送のID。
    /// `trzsz_cancel` は非同期(セッションイベントループへのコマンド送信)なので、
    /// 実際の `on_trzsz_finished`(success=false, message="Cancelled" 等の汎用文言)が
    /// 届くのは少し後になる。その届いた際にこのIDが一致すれば、汎用文言ではなく
    /// ユーザーに分かりやすい「大きすぎる」メッセージへ差し替える。
    size_limit_exceeded_for: Option<String>,

    // ── 自動再接続(tssh風reconnect) ──────────────────────
    /// セッションオブジェクト1つ生成するごとにインクリメントする世代カウンタ。
    /// `OrchestratorAdapter`は生成時にこの値をキャプチャし、`SessionCallback`の
    /// 各メソッド呼び出し時にこの値と現在値が一致するかを確認する。不一致なら
    /// 「既に見捨てられた古いセッションからの遅延コールバック」なので無視する
    /// (新しい手動接続/再接続試行が、古いセッションの遅延イベントに状態を
    /// 巻き戻されないようにするための独立した仕組み。`reconnect_epoch`とは別物)。
    session_generation: u64,
    /// 自動再接続ループ自身の生存確認用epoch。新しい`connect_*`呼び出し・
    /// `cancel_reconnect()`・再接続成功のいずれかでインクリメントされ、
    /// ループは次のtickで自分のepochが古いと分かれば静かに終了する。
    reconnect_epoch: u64,
    /// 自動再接続ループが現在動作中かどうか。`on_disconnected`が二重にループを
    /// 起動しない・二重に`Disconnected`を通知しないための判定に使う。
    reconnect_loop_active: bool,
    /// ループが`connect_via`を発火してから、その試行の結果(generation一致の
    /// `on_connected`/`on_disconnected`)を観測するまでの間true。次のtickで
    /// 新しい試行を重ねて発火しないためのガード(ホスト鍵確認プロンプトの
    /// 多重発生を防ぐ)。
    retry_attempt_in_flight: bool,
    /// `SessionOrchestrator::disconnect()`が呼ばれた際に立てる。ユーザーが
    /// 明示的に切断した場合は自動再接続しない(tsshの「唯一の例外」と同じ)。
    /// 読み取った直後にfalseへ戻す一度きりのフラグ。
    user_initiated_disconnect: bool,
    /// 直前に成功した(あるいは試みた)`connect_*`。予期しない切断時にこれを
    /// 使って自動的に再接続を試みる。
    last_connect_attempt: Option<LastConnectAttempt>,
    /// 再接続ループのタイミング。テストでは短い値に差し替える。
    reconnect_policy: ReconnectPolicy,
}

/// 1回の再接続試行を実行する処理の型。既定は`connect_via`(実際にセッションを
/// 生成して接続する)。テストでは実ネットワークに触れないフェイクへ差し替え、
/// 呼び出し回数・cadenceだけを検証する — `connect()`自体が非同期fire-and-forget
/// なので、実際に接続できたかどうかまではこの粒度の単体テストでは検証しない
/// (Codexレビュー指摘、実ネットワーク越しの成功パスは実機確認でカバーする)。
type ReconnectAttemptFn = dyn Fn(&Arc<OrchestratorShared>, LastConnectAttempt) -> Result<(), SshError> + Send + Sync;

pub(crate) struct OrchestratorShared {
    state: Mutex<OrchestratorState>,
    callback: Arc<dyn OrchestratorCallback>,
    session: Mutex<Option<ActiveSession>>,
    /// `notify_network_path_changed`のdebounce/epoch状態。`Connected && !is_quic`の
    /// ケースだけがこれを実際に使う([`crate::net_health_policy`]参照)。
    path_observer: Mutex<crate::net_health_policy::PathObserver>,
    reconnect_attempt: Box<ReconnectAttemptFn>,
}

// ── OrchestratorAdapter ───────────────────────────────────
// Translates old SessionCallback events → structured OrchestratorCallback

pub(crate) struct OrchestratorAdapter {
    pub(crate) shared: Arc<OrchestratorShared>,
    /// 生成時にキャプチャした`session_generation`。`is_current()`参照。
    generation: u64,
}

impl OrchestratorAdapter {
    /// 新しいセッションを1つ作るたびに呼ぶ。`session_generation`をインクリメントし、
    /// その値をこのアダプタ自身にキャプチャする(このアダプタ経由のコールバックが
    /// 「今まさに有効なセッションからのものか」を後から判定できるようにする)。
    fn new(shared: Arc<OrchestratorShared>) -> Self {
        let generation = {
            let mut s = shared.state.lock();
            s.session_generation += 1;
            s.session_generation
        };
        Self { shared, generation }
    }

    /// このアダプタが今も「現行の」セッションのものかどうか。古い(既に見捨てられた)
    /// セッションからの遅延コールバックはこれがfalseになり、呼び出し元は無視する。
    fn is_current(&self) -> bool {
        self.shared.state.lock().session_generation == self.generation
    }
}

impl SessionCallback for OrchestratorAdapter {
    fn on_data(&self, data: Vec<u8>) {
        if !self.is_current() { return; }
        self.shared.callback.on_data(data);
    }

    fn on_host_key(&self, fingerprint: String) -> bool {
        if !self.is_current() { return false; }
        let (host, port) = {
            let s = self.shared.state.lock();
            (s.current_host.clone().unwrap_or_default(), s.current_port)
        };
        self.shared.callback.on_host_key(host, port, fingerprint)
    }

    fn on_connected(&self) {
        if !self.is_current() { return; }
        let host = {
            let mut s = self.shared.state.lock();
            s.phase = ConnPhase::Connected;
            // 再接続ループが動いていたなら、成功したのでここで止める。
            s.reconnect_epoch += 1;
            s.reconnect_loop_active = false;
            s.retry_attempt_in_flight = false;
            s.current_host.clone().unwrap_or_default()
        };
        self.shared.callback.on_connection_state_changed(
            ConnectionPublicState::Connected { host }
        );
    }

    fn on_disconnected(&self, reason: Option<String>) {
        if !self.is_current() { return; }
        handle_unexpected_disconnect(&self.shared, reason);
    }

    fn on_screen_update(&self, update: ScreenUpdate) {
        if !self.is_current() { return; }
        self.shared.callback.on_screen_update(update);
    }

    fn on_trzsz_request(
        &self, transfer_id: String, mode: String,
        suggested_name: Option<String>, expected_size: Option<u64>,
    ) {
        if !self.is_current() { return; }
        {
            let mut s = self.shared.state.lock();
            s.current_transfer_id = Some(transfer_id.clone());
            s.trzsz_mode = Some(mode.clone());
            s.download_buf.clear();
            s.size_limit_exceeded_for = None;
        }
        if let Some(session) = self.shared.session.lock().as_ref() {
            session.set_interactive_busy(true);
        }
        self.shared.callback.on_trzsz_state_changed(
            TrzszPublicState::WaitingUser { transfer_id, mode, suggested_name, expected_size }
        );
    }

    /// #60: trzsz の `SIZE` 申告値はサーバーの自己申告に過ぎず強制されないため、
    /// 実際に受信したバイト数(累積 `download_buf` 長)を都度 `MAX_DOWNLOAD_BUF_BYTES`
    /// と比較する。超過したら OOM する前に `download_buf` を捨て、転送そのものも
    /// `trzsz_cancel` で中断させる(FSM側は非同期に `on_trzsz_finished` を返してくる
    /// ので、そちらで success=false・分かりやすいメッセージに揃える)。
    fn on_trzsz_download_chunk(&self, transfer_id: String, data: Vec<u8>, _is_last: bool) {
        if !self.is_current() { return; }
        let exceeded = {
            let mut s = self.shared.state.lock();
            let would_be_len = s.download_buf.len().saturating_add(data.len());
            if would_be_len > MAX_DOWNLOAD_BUF_BYTES {
                log::warn!(
                    "trzsz: download {} exceeds {} byte cap (would reach {}), aborting to avoid OOM",
                    transfer_id, MAX_DOWNLOAD_BUF_BYTES, would_be_len
                );
                s.download_buf.clear();
                s.size_limit_exceeded_for = Some(transfer_id.clone());
                true
            } else {
                s.download_buf.extend_from_slice(&data);
                false
            }
        };
        if exceeded {
            if let Some(session) = self.shared.session.lock().as_ref() {
                session.trzsz_cancel(transfer_id);
            }
        }
    }

    fn on_trzsz_progress(&self, transfer_id: String, transferred: u64, total: Option<u64>) {
        if !self.is_current() { return; }
        let mode = self.shared.state.lock()
            .trzsz_mode.clone()
            .unwrap_or_else(|| "download".to_string());
        self.shared.callback.on_trzsz_state_changed(
            TrzszPublicState::InProgress {
                transfer_id, mode, file_name: None, transferred, total
            }
        );
    }

    fn on_trzsz_finished(&self, transfer_id: String, success: bool, message: Option<String>) {
        if !self.is_current() { return; }
        let (data, is_download, success, message) = {
            let mut s = self.shared.state.lock();
            s.current_transfer_id = None;
            let size_limit_hit = s.size_limit_exceeded_for.take().as_deref() == Some(transfer_id.as_str());
            let data = std::mem::take(&mut s.download_buf);
            let is_download = s.trzsz_mode.as_deref() == Some("download");
            if size_limit_hit {
                // #60: on_trzsz_download_chunk側で既に中断済み。trzsz_cancel経由の
                // 汎用的な message(例: "Cancelled")を、ユーザーに分かりやすい文言へ
                // 差し替える。success も常にfalseにする(万一cancel競合でtrueが
                // 届いても、上限超過を成功扱いにしてはいけない)。
                (data, is_download, false, Some("ファイルが大きすぎるため転送を中断しました".to_string()))
            } else {
                (data, is_download, success, message)
            }
        };
        if let Some(session) = self.shared.session.lock().as_ref() {
            session.set_interactive_busy(false);
        }
        if success && is_download && !data.is_empty() {
            self.shared.callback.on_download_complete(None, data);
        }
        self.shared.callback.on_trzsz_state_changed(
            TrzszPublicState::Done { transfer_id, success, message }
        );
    }

    fn on_no_viable_path(&self) {
        if !self.is_current() { return; }
        self.shared.callback.on_no_viable_path();
    }

    fn on_forward_state_changed(&self, id: String, state: ForwardState) {
        if !self.is_current() { return; }
        self.shared.callback.on_forward_state_changed(id, state);
    }

    fn on_agent_sign_request(&self, key_fingerprint: String) -> bool {
        if !self.is_current() { return false; }
        self.shared.callback.on_agent_sign_request(key_fingerprint)
    }

    fn on_clipboard_write(&self, payload: ClipboardPayload) {
        if !self.is_current() { return; }
        self.shared.callback.on_clipboard_write(payload);
    }

    fn on_clipboard_pull_request(&self) -> Option<ClipboardPayload> {
        if !self.is_current() { return None; }
        self.shared.callback.on_clipboard_pull_request()
    }

    fn on_request_wifi_fd(&self) -> Option<crate::PlatformFd> {
        if !self.is_current() { return None; }
        self.shared.callback.on_request_wifi_fd()
    }

    fn on_request_cellular_fd(&self) -> Option<crate::PlatformFd> {
        if !self.is_current() { return None; }
        self.shared.callback.on_request_cellular_fd()
    }

    fn on_rebind_state_changed(&self, state: crate::rebind_manager::RebindPublicState) {
        if !self.is_current() { return; }
        self.shared.callback.on_rebind_state_changed(state);
    }
}

/// `notify_network_path_changed`の実際の切断処理。`&Arc<OrchestratorShared>`だけを
/// 取る自由関数にしてあるのは、debounce後の発火が`SessionOrchestrator`自身ではなく
/// `RUNTIME.spawn`されたtokio task(`Arc<OrchestratorShared>`のcloneしか持たない)から
/// 呼ばれるため — `SessionOrchestrator::disconnect`(セッションを切るだけの2行)と
/// 中身は同じだが、`&self`経由ではなく`shared`に対して直接操作する。
///
/// [[always-connects.md]]の実インシデント(網断debounce発火の経路だけが自動復旧の
/// 対象外になっていた)と同じ見落としを繰り返さないよう、`OrchestratorAdapter::
/// on_disconnected`と同じ`handle_unexpected_disconnect`を経由させる —
/// 個別に「phase=Idle + Disconnected通知」を書かない。
/// `apply_network_lost`が`handle_unexpected_disconnect`へ渡す合成理由文字列。
/// [`DisconnectKind::classify`]がこの定数を直接比較するので、二重に書かないよう
/// 定数化してある。
const NETWORK_LOST_REASON: &str = "network lost";

fn apply_network_lost(shared: &Arc<OrchestratorShared>) {
    if let Some(s) = shared.session.lock().as_ref() {
        s.disconnect();
    }
    handle_unexpected_disconnect(shared, Some(NETWORK_LOST_REASON.to_string()));
}

/// `handle_unexpected_disconnect`が受け取る`reason`文字列のRust内部用分類。
///
/// `SessionCallback::on_disconnected(reason: Option<String>)`には現状「切断理由の
/// 種別」を運ぶ専用フィールドが無く、`reason`文字列に頼っている ── この
/// trait(`SessionCallback`)には本番用の`OrchestratorAdapter`以外にテスト専用の
/// 実装が4箇所あり、シグネチャ変更・UniFFI経由でKotlin側に公開される文字列の
/// 変更はそれら全ての更新を要する大きめの変更になるため見送っている。この型は
/// あくまで`reason`文字列を読んだ*後*にRustのプロセス内だけで使う分類であり、
/// `on_disconnected`のシグネチャにも公開文字列そのものにも影響しない。
/// 分類ロジックを`handle_unexpected_disconnect`一箇所に一元化することで、
/// `starts_with`/文字列比較が呼び出し側に増殖するのを防ぐ(rust-ssot.mdの
/// 「判断ロジックをRust側に一元化する」原則そのもの)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisconnectKind {
    /// `transport::ssh_handler::run_ssh_channel_loop`の`ChannelMsg::ExitStatus`
    /// (リモートプロセスの正常終了、例: ユーザーがシェルで`exit`した)由来の切断。
    /// ネットワーク/トランスポート障害ではないので、tssh風の自動再接続の対象に
    /// しない(勝手に新しいシェルを張り直すのは意図しない挙動)。
    GracefulRemoteExit,
    /// `apply_network_lost`が合成する、OS側のネットワークパス消失由来の切断。
    /// トランスポート層自体は特に何も報告していない(自動再接続の対象)。
    NetworkLost,
    /// 上記以外 ── russh/QUICエラー・認証失敗・PTY/shellリクエスト失敗・
    /// `reason: None`(ピア/ローカルからの切断)等。自動再接続の対象。
    TransportError,
}

impl DisconnectKind {
    fn classify(reason: &Option<String>) -> Self {
        match reason.as_deref() {
            Some(r) if r.starts_with("remote process exited") => Self::GracefulRemoteExit,
            Some(r) if r == NETWORK_LOST_REASON => Self::NetworkLost,
            _ => Self::TransportError,
        }
    }
}

/// 予期しない切断(`OrchestratorAdapter::on_disconnected`・`apply_network_lost`の
/// 両方から呼ばれる)の共通処理。一度`Connected`になっていて・ユーザーが明示的に
/// 切断したのでなく・リモートプロセスの正常終了でもなく・直前の接続設定が分かって
/// いれば自動再接続ループを起動する。既にループが動作中の切断(＝1回のリトライ
/// 試行自体の失敗)は、二重にループを起動せず・連続で`Disconnected`を通知もせず、
/// ループ自身のtickに任せる。
fn handle_unexpected_disconnect(shared: &Arc<OrchestratorShared>, reason: Option<String>) {
    enum Action {
        Suppress,
        StartLoop(LastConnectAttempt, u64),
        NotifyDisconnected,
    }

    let action = {
        let mut s = shared.state.lock();
        let was_connected = s.phase == ConnPhase::Connected;
        let user_initiated = s.user_initiated_disconnect;
        let graceful_exit = DisconnectKind::classify(&reason) == DisconnectKind::GracefulRemoteExit;
        s.user_initiated_disconnect = false;
        s.phase = ConnPhase::Idle;
        s.retry_attempt_in_flight = false;

        if s.reconnect_loop_active {
            Action::Suppress
        } else if was_connected && !user_initiated && !graceful_exit {
            match s.last_connect_attempt.clone() {
                Some(attempt) => {
                    s.reconnect_loop_active = true;
                    s.reconnect_epoch += 1;
                    Action::StartLoop(attempt, s.reconnect_epoch)
                }
                None => Action::NotifyDisconnected,
            }
        } else {
            Action::NotifyDisconnected
        }
    };

    match action {
        Action::Suppress => {}
        Action::StartLoop(attempt, epoch) => {
            spawn_reconnect_loop(shared.clone(), attempt, reason, epoch);
        }
        Action::NotifyDisconnected => {
            shared.callback.on_connection_state_changed(
                ConnectionPublicState::Disconnected { reason }
            );
        }
    }
}

/// リトライ専用のセッション生成。`begin_connect()`(手動接続の開始、`Connecting`通知・
/// `reconnect_epoch`無効化を伴う)とは別関数にしてある — リトライのたびに`begin_connect()`
/// を呼ぶと、リトライループ自身の`reconnect_epoch`を無効化してしまい自己終了してしまう。
fn connect_via(shared: &Arc<OrchestratorShared>, attempt: LastConnectAttempt) -> Result<(), SshError> {
    let (host, port, is_quic) = attempt.host_port_is_quic();
    {
        let mut s = shared.state.lock();
        s.current_host = Some(host);
        s.current_port = port;
        s.is_quic = is_quic;
        s.phase = ConnPhase::Connecting;
    }
    let adapter = OrchestratorAdapter::new(shared.clone());
    let session = match attempt {
        LastConnectAttempt::Ssh(config) => {
            let session = crate::create_ssh_session(config);
            session.connect(Box::new(adapter))?;
            ActiveSession::Ssh(session)
        }
        LastConnectAttempt::Quic(config) => {
            let session = crate::quic_transport::create_quic_session(config);
            session.connect(Box::new(adapter))?;
            ActiveSession::Quic(session)
        }
        LastConnectAttempt::IsekaiPipeQuic(config) => {
            let session = crate::isekai_pipe_quic_transport::create_isekai_pipe_quic_session(config);
            session.connect(Box::new(adapter))?;
            ActiveSession::IsekaiPipeQuic(session)
        }
        LastConnectAttempt::IsekaiPipeQuicAuto(config) => {
            let session = crate::isekai_pipe_quic_transport::create_isekai_pipe_quic_session(config);
            session.connect_auto(Box::new(adapter))?;
            ActiveSession::IsekaiPipeQuic(session)
        }
        LastConnectAttempt::MultipathIsekaiPipeQuic(config) => {
            let session = crate::multipath_transport::create_multipath_isekai_pipe_quic_session(config);
            session.connect(Box::new(adapter))?;
            ActiveSession::MultipathIsekaiPipeQuic(session)
        }
        LastConnectAttempt::IsekaiStunP2p(config) => {
            let session = crate::isekai_stun_p2p_transport::create_isekai_stun_p2p_session(config);
            session.connect(Box::new(adapter))?;
            ActiveSession::IsekaiStunP2p(session)
        }
        LastConnectAttempt::IsekaiLinkRelay(config) => {
            let session = crate::isekai_link_relay_transport::create_isekai_link_relay_session(config);
            session.connect(Box::new(adapter))?;
            ActiveSession::IsekaiLinkRelay(session)
        }
    };
    *shared.session.lock() = Some(session);
    Ok(())
}

/// 自動再接続ループ本体。`RUNTIME.spawn`されたtokio task。tsshのUDPモード
/// reconnectと同じく、1秒ごとに`Reconnecting`をライブ通知しつつ、
/// `retry_interval`ごとに実際の再接続(`connect_via`)を試みる。
/// `retry_attempt_in_flight`により、1回の試行の結果(成功/失敗)が判明するまで
/// 次の試行を重ねて発火しない(ホスト鍵確認プロンプトの多重発生を防ぐ)。
fn spawn_reconnect_loop(
    shared: Arc<OrchestratorShared>,
    attempt: LastConnectAttempt,
    reason: Option<String>,
    epoch: u64,
) {
    RUNTIME.spawn(async move {
        let policy = shared.state.lock().reconnect_policy;
        let timeout_secs = policy.timeout.as_secs() as u32;
        // tickの整数倍でretry_intervalを表す(「何tickごとに1回試みるか」)。
        // 経過時間を`.as_secs()`で秒に丸めてから割り算すると、テスト用の
        // サブ秒ポリシー(tick=10msなど)で常に0になり判定が壊れるため、
        // tick単位のカウンタで比較する。
        let ticks_per_retry = (policy.retry_interval.as_nanos() / policy.tick.as_nanos().max(1)).max(1);
        let mut elapsed = Duration::ZERO;
        let mut tick_count: u128 = 0;

        if shared.state.lock().reconnect_epoch != epoch {
            // spawnされてから最初のtickに至るまでの間に、既に別の何か(即座の
            // 手動再接続・cancel_reconnect等)に主導権が移っていた場合、初回の
            // Reconnecting通知すら出さずに静かに終了する。
            return;
        }
        shared.callback.on_connection_state_changed(ConnectionPublicState::Reconnecting {
            elapsed_secs: 0,
            timeout_secs,
            reason: reason.clone(),
        });

        loop {
            tokio::time::sleep(policy.tick).await;
            elapsed = elapsed.saturating_add(policy.tick);
            tick_count += 1;

            if shared.state.lock().reconnect_epoch != epoch {
                // 別の何か(新しい手動接続・cancel_reconnect・再接続成功)に
                // 主導権が移った。静かに終了する。
                return;
            }

            if elapsed >= policy.timeout {
                let mut s = shared.state.lock();
                if s.reconnect_epoch == epoch {
                    s.reconnect_loop_active = false;
                    s.retry_attempt_in_flight = false;
                }
                drop(s);
                log::warn!("orchestrator: reconnect loop gave up after {timeout_secs}s");
                shared.callback.on_connection_state_changed(ConnectionPublicState::Disconnected {
                    reason: Some(format!(
                        "reconnect timed out after {timeout_secs}s (last: {})",
                        reason.clone().unwrap_or_else(|| "unknown".to_string())
                    )),
                });
                return;
            }

            shared.callback.on_connection_state_changed(ConnectionPublicState::Reconnecting {
                elapsed_secs: elapsed.as_secs() as u32,
                timeout_secs,
                reason: reason.clone(),
            });

            let should_attempt = {
                let mut s = shared.state.lock();
                let due = tick_count % ticks_per_retry == 0;
                if s.reconnect_epoch == epoch && !s.retry_attempt_in_flight && due {
                    s.retry_attempt_in_flight = true;
                    true
                } else {
                    false
                }
            };
            if should_attempt {
                if let Err(e) = (shared.reconnect_attempt)(&shared, attempt.clone()) {
                    log::warn!("orchestrator: reconnect attempt failed synchronously: {e:?}");
                    let mut s = shared.state.lock();
                    if s.reconnect_epoch == epoch {
                        s.retry_attempt_in_flight = false;
                    }
                }
            }
        }
    });
}

// ── SessionOrchestrator ───────────────────────────────────

#[derive(uniffi::Object)]
pub struct SessionOrchestrator {
    shared: Arc<OrchestratorShared>,
}

#[uniffi::export]
pub fn create_session_orchestrator(callback: Box<dyn OrchestratorCallback>) -> Arc<SessionOrchestrator> {
    crate::init_logger();
    let shared = Arc::new(OrchestratorShared {
        state: Mutex::new(OrchestratorState {
            current_host: None,
            current_port: 22,
            is_quic: false,
            phase: ConnPhase::Idle,
            current_transfer_id: None,
            trzsz_mode: None,
            download_buf: Vec::new(),
            size_limit_exceeded_for: None,
            session_generation: 0,
            reconnect_epoch: 0,
            reconnect_loop_active: false,
            retry_attempt_in_flight: false,
            user_initiated_disconnect: false,
            last_connect_attempt: None,
            reconnect_policy: ReconnectPolicy::default(),
        }),
        callback: Arc::from(callback),
        session: Mutex::new(None),
        path_observer: Mutex::new(crate::net_health_policy::PathObserver::default()),
        reconnect_attempt: Box::new(connect_via),
    });
    Arc::new(SessionOrchestrator { shared })
}

impl SessionOrchestrator {
    /// 各`connect_*`が共通で行う「state更新→Connecting通知→adapter生成」を
    /// 一箇所にまとめる。session生成・接続・`ActiveSession`格納は呼び出し側が
    /// トランスポートごとに行う（`connect`のエラー型/セッション型がそれぞれ違うため）。
    fn begin_connect(&self, host: String, port: u16, is_quic: bool) -> OrchestratorAdapter {
        {
            let mut s = self.shared.state.lock();
            s.current_host = Some(host);
            s.current_port = port;
            s.is_quic = is_quic;
            s.phase = ConnPhase::Connecting;
            // 新しい手動接続が始まった以上、直前のdisconnect()由来のフラグや
            // 実行中だったかもしれない自動再接続ループは無関係になる。
            s.user_initiated_disconnect = false;
            s.reconnect_epoch += 1;
            s.reconnect_loop_active = false;
            s.retry_attempt_in_flight = false;
        }
        // 新しい接続試行が始まった時点で、直前のセッションに対して保留中だった
        // network-path debounceは無効化する。そうしないと、瞬断のdebounce待機中に
        // 手動で切断/別transportへ再接続した場合、無関係な新しいセッションを
        // 誤って切断してしまう(レビューで指摘された実際の不具合)。
        self.shared.path_observer.lock().invalidate();
        self.shared.callback.on_connection_state_changed(ConnectionPublicState::Connecting);
        OrchestratorAdapter::new(self.shared.clone())
    }
}

#[uniffi::export]
impl SessionOrchestrator {
    pub fn connect(&self, config: SshConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.host.clone(), config.port, false);
        self.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::Ssh(config.clone()));
        let session = crate::create_ssh_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::Ssh(session));
        Ok(())
    }

    pub fn connect_quic(&self, config: QuicConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true);
        self.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::Quic(config.clone()));
        let session = crate::quic_transport::create_quic_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::Quic(session));
        Ok(())
    }

    /// Phase 7: 自作ヘルパー（isekai-helper）経由の QUIC 接続。フォールバック無し
    /// （`TransportPreference::IsekaiPipeQuic` 相当、明示選択時に使う）。
    pub fn connect_isekai_pipe_quic(&self, config: IsekaiPipeQuicConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true);
        self.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::IsekaiPipeQuic(config.clone()));
        let session = crate::isekai_pipe_quic_transport::create_isekai_pipe_quic_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::IsekaiPipeQuic(session));
        Ok(())
    }

    /// Phase 7: `TransportPreference::Auto` 相当。自作ヘルパー経由 QUIC のブートストラップ/
    /// 接続に失敗した場合、内部で自動的に通常の TCP SSH にフォールバックする。
    pub fn connect_isekai_pipe_quic_auto(&self, config: IsekaiPipeQuicConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true);
        self.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::IsekaiPipeQuicAuto(config.clone()));
        let session = crate::isekai_pipe_quic_transport::create_isekai_pipe_quic_session(config);
        session.connect_auto(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::IsekaiPipeQuic(session));
        Ok(())
    }

    /// Phase 9: `TransportPreference::IsekaiPipeQuicMultipath` 相当。フォールバック無し。
    /// `config.direct_host` が設定されていれば path0（`ssh_host`）+ path1（`direct_host`）の
    /// 受動的マルチパスで接続する。
    pub fn connect_multipath_isekai_pipe_quic(&self, config: MultipathIsekaiPipeQuicConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true);
        self.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::MultipathIsekaiPipeQuic(config.clone()));
        let session = crate::multipath_transport::create_multipath_isekai_pipe_quic_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::MultipathIsekaiPipeQuic(session));
        Ok(())
    }

    /// Phase 10: `TransportPreference::IsekaiStunP2pQuic` 相当。relay 無し・
    /// STUN+SSH rendezvousによる直接 P2P QUIC。フォールバック無し（穴あけ不成立時は
    /// 接続失敗として扱う。`isekai_stun_p2p_transport.rs` 参照）。
    pub fn connect_isekai_stun_p2p(&self, config: IsekaiStunP2pConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true);
        self.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::IsekaiStunP2p(config.clone()));
        let session = crate::isekai_stun_p2p_transport::create_isekai_stun_p2p_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::IsekaiStunP2p(session));
        Ok(())
    }

    /// Phase 10: `TransportPreference::IsekaiLinkRelayQuic` 相当。MASQUE relay 経由の
    /// P2P QUIC。フォールバック無し（`isekai_link_relay_transport.rs` 参照）。
    pub fn connect_isekai_link_relay(&self, config: IsekaiLinkRelayConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true);
        self.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::IsekaiLinkRelay(config.clone()));
        let session = crate::isekai_link_relay_transport::create_isekai_link_relay_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::IsekaiLinkRelay(session));
        Ok(())
    }

    pub fn disconnect(&self) {
        // 「これから来る`on_disconnected`はユーザー操作起因」の印を先に立てておく
        // (実際の切断はこの後`s.disconnect()`が非同期にコールバックを発火させる)。
        self.shared.state.lock().user_initiated_disconnect = true;
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.disconnect();
        }
    }

    /// 自動再接続ループを中止する。ループが動作中だった場合のみ`Disconnected`を
    /// 通知する(動いていない時に呼ばれても無音、UIは`isReconnecting`の間だけ
    /// 「中止」操作を出す想定)。
    pub fn cancel_reconnect(&self) {
        let was_active = {
            let mut s = self.shared.state.lock();
            let was_active = s.reconnect_loop_active;
            s.reconnect_epoch += 1;
            s.reconnect_loop_active = false;
            s.retry_attempt_in_flight = false;
            was_active
        };
        if was_active {
            self.shared.callback.on_connection_state_changed(
                ConnectionPublicState::Disconnected {
                    reason: Some("reconnect cancelled by user".to_string()),
                }
            );
        }
    }

    /// 「WiFiは繋がっているがupstreamが死んでいる」等をKotlin側で検知した際に呼ぶ。
    /// `fd`は`Network.bindSocket()`済み・`ParcelFileDescriptor.detachFd()`済みの生fd
    /// （所有権はこちらに移る）。マルチパス以外のtransportや未接続時は何もしない。
    pub fn rebind_to_fd(&self, fd: i32, local_ip: String) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.rebind_to_fd(fd, local_ip);
        }
    }

    /// #11: ユーザーが「今すぐWiFiに戻す」操作を行った(セルラーにフェイルオーバー中、
    /// ダウンロード中などで静けさ待ちを待たずに即座に戻したい場合)。疎通確認だけは
    /// 省略されない(`RebindManager::handle_manual_force_return`参照)。マルチパス以外の
    /// transportや未接続時は何もしない。
    pub fn force_return_to_wifi(&self) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.force_return_to_wifi();
        }
    }

    pub fn send(&self, data: Vec<u8>) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.send(data);
        }
    }

    pub fn resize(&self, cols: u32, rows: u32) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.resize(cols, rows);
        }
    }

    pub fn scrollback_len(&self) -> u32 {
        self.shared.session.lock().as_ref().map_or(0, |s| s.scrollback_len())
    }

    pub fn scrollback_cells(&self, offset: u32, rows: u32) -> Vec<CellData> {
        self.shared.session.lock().as_ref()
            .map_or_else(Vec::new, |s| s.scrollback_cells(offset, rows))
    }

    pub fn trzsz_accept_download(&self) {
        let tid = self.shared.state.lock().current_transfer_id.clone();
        if let Some(tid) = tid {
            if let Some(s) = self.shared.session.lock().as_ref() {
                s.trzsz_accept_download(tid);
            }
        }
    }

    pub fn trzsz_accept_upload(&self, file_name: String, file_size: u64, mode: u32) {
        let tid = self.shared.state.lock().current_transfer_id.clone();
        if let Some(tid) = tid {
            if let Some(s) = self.shared.session.lock().as_ref() {
                s.trzsz_accept_upload(tid, file_name, file_size, mode);
            }
        }
    }

    pub fn trzsz_send_chunk(&self, data: Vec<u8>, is_last: bool) {
        let tid = self.shared.state.lock().current_transfer_id.clone();
        if let Some(tid) = tid {
            if let Some(s) = self.shared.session.lock().as_ref() {
                s.trzsz_send_chunk(tid, data, is_last);
            }
        }
    }

    pub fn trzsz_cancel(&self) {
        let tid = self.shared.state.lock().current_transfer_id.take();
        if let Some(tid) = tid {
            if let Some(s) = self.shared.session.lock().as_ref() {
                s.trzsz_cancel(tid);
                s.set_interactive_busy(false);
            }
        }
    }

    pub fn trzsz_dismiss(&self) {
        let mut s = self.shared.state.lock();
        s.trzsz_mode = None;
        s.current_transfer_id = None;
        drop(s);
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.set_interactive_busy(false);
        }
        self.shared.callback.on_trzsz_state_changed(TrzszPublicState::Idle);
    }

    pub fn is_quic(&self) -> bool {
        self.shared.state.lock().is_quic
    }

    /// OS からネットワーク断（Wi-Fi/セルラー消失等）を通知された時の対応を決める。
    /// QUIC 接続はパス変更に自前で耐えられるため無視し、ハンドシェイク中や
    /// OS からのネットワークpath変化(`ConnectivityManager`/`NWPathMonitor`)をそのまま
    /// 転送してもらい、判断はここ(Rust側のSSOT)で行う。Kotlin/Swift側はイベントを
    /// そのまま転送するだけでよい。
    ///
    /// `Idle`/`Connecting`/`Connected && is_quic`は既存の即時判断ロジックのまま
    /// (ハンドシェイク中は自前の耐性がまだ無いので即abort、QUIC系は自前で耐えるので
    /// 何もしない)。`Connected && !is_quic`(プレーンTCP SSH)だけが新たに
    /// [`crate::net_health_policy`]のdebounceの対象になる — OS通知の瞬断で
    /// 即切断されていた実バグの唯一の発生源だったため。
    pub fn notify_network_path_changed(&self, is_satisfied: bool) {
        let (phase, is_quic) = {
            let s = self.shared.state.lock();
            (s.phase, s.is_quic)
        };
        match phase {
            ConnPhase::Idle => {}
            ConnPhase::Connecting => {
                if !is_satisfied {
                    log::warn!("orchestrator: network lost during handshake — aborting");
                    apply_network_lost(&self.shared);
                }
            }
            ConnPhase::Connected if is_quic => {
                log::info!("orchestrator: network path changed — QUIC session, letting transport handle it");
            }
            ConnPhase::Connected => {
                let (epoch, decision) = self.shared.path_observer.lock().handle_update(is_satisfied);
                match decision {
                    net_health_policy::Decision::Ignore => {}
                    net_health_policy::Decision::NotifyAfterDebounce(dur) => {
                        let shared = self.shared.clone();
                        RUNTIME.spawn(async move {
                            tokio::time::sleep(dur).await;
                            if shared.path_observer.lock().is_current(epoch) {
                                log::warn!(
                                    "orchestrator: network still lost after debounce — disconnecting TCP session"
                                );
                                apply_network_lost(&shared);
                            }
                        });
                    }
                }
            }
        }
    }

    /// 接続中にローカルポートフォワード(-L)を動的に追加する。
    /// MVP の UI は接続前に `SshConfig.forwards` へまとめて設定するだけなので現状未使用だが、
    /// 将来「接続したまま転送を足す」UI を追加する際の入り口として用意している。
    pub fn add_local_forward(&self, id: String, bind_address: String, bind_port: u16, remote_host: String, remote_port: u16) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.add_local_forward(id, bind_address, bind_port, remote_host, remote_port);
        }
    }

    pub fn remove_forward(&self, id: String) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.remove_forward(id);
        }
    }

    /// Phase 12: このセッション(タブ)だけの配色テーマを差し替える(per-session theme)。
    /// アプリ全体の既定テーマ(`set_terminal_theme`)とは独立しており、以降このタブが
    /// 解決する SGR にのみ反映される(既に画面/scrollbackに積まれたセルは遡って
    /// 再着色されない、`set_terminal_theme`と同じ制約)。
    ///
    /// `ansi16`/`default_fg`/`default_bg`は`set_terminal_theme`と同じ形式。呼び出し側
    /// (Kotlin `TerminalTabsViewModel`)が「Global default → Profile default →
    /// Tab/session override」の解決を行い、結果をここへ渡す。
    pub fn set_session_theme(&self, ansi16: Vec<u32>, default_fg: u32, default_bg: u32) {
        let theme = crate::theme::from_raw(ansi16, default_fg, default_bg);
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.set_theme(theme);
        }
    }

    pub fn notify_error(&self, message: String) {
        self.shared.callback.on_connection_state_changed(
            ConnectionPublicState::Error { message }
        );
    }
}

// ── Tests ──────────────────────────────────────────────────
//
// この模块の状態遷移(`ConnPhase`の分岐、`OrchestratorAdapter`のtrzsz状態集約)は
// 実SSH/QUIC接続を一切必要としない純粋なロジックであり、本来実機は不要だったにも
// 関わらず`orchestrator.rs`にはテストが1つも無かった。`rust-ssot.md`が「Rust側の
// SSOTである」ことの根拠として挙げている`notify_network_lost()`自体が無テストだった
// ため、ここで最初にカバーする。`ActiveSession`は具体的なtransportセッション型しか
// 保持できない(trait objectではない)ため、`session: Mutex::new(None)`のまま
// (未接続として)テストする — `notify_network_lost`/`disconnect`は`None`の場合
// no-opになるよう書かれているので、これで分岐ロジックの検証は完結する。
//
// #60: `on_trzsz_download_chunk`が上限超過時に呼ぶ`session.trzsz_cancel(..)`も
// 同様に`None`の場合no-opになるよう書かれているので、trzszバッファ上限のロジック
// (実SSH/QUIC不要)もここで検証できる。
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn disconnect_kind_classifies_graceful_remote_exit_by_prefix() {
        let reason = Some("remote process exited (status 0)".to_string());
        assert_eq!(DisconnectKind::classify(&reason), DisconnectKind::GracefulRemoteExit);
    }

    #[test]
    fn disconnect_kind_classifies_the_network_lost_literal() {
        let reason = Some(NETWORK_LOST_REASON.to_string());
        assert_eq!(DisconnectKind::classify(&reason), DisconnectKind::NetworkLost);
    }

    #[test]
    fn disconnect_kind_defaults_to_transport_error_for_anything_else() {
        assert_eq!(DisconnectKind::classify(&None), DisconnectKind::TransportError);
        assert_eq!(
            DisconnectKind::classify(&Some("PTY/shell request failed".to_string())),
            DisconnectKind::TransportError
        );
        // A reason that merely mentions "network lost" mid-string (not the
        // exact synthesized literal `apply_network_lost` sends) must not be
        // misclassified — only the precise, orchestrator-synthesized value
        // counts as `NetworkLost`.
        assert_eq!(
            DisconnectKind::classify(&Some("something about network lost here".to_string())),
            DisconnectKind::TransportError
        );
    }

    #[derive(Default)]
    struct RecordingCallback {
        connection_states: StdMutex<Vec<ConnectionPublicState>>,
        trzsz_states: StdMutex<Vec<TrzszPublicState>>,
        downloads: StdMutex<Vec<(Option<String>, Vec<u8>)>>,
    }

    impl OrchestratorCallback for RecordingCallback {
        fn on_connection_state_changed(&self, state: ConnectionPublicState) {
            self.connection_states.lock().unwrap().push(state);
        }
        fn on_screen_update(&self, _update: ScreenUpdate) {}
        fn on_host_key(&self, _host: String, _port: u16, _fingerprint: String) -> bool {
            true
        }
        fn on_data(&self, _data: Vec<u8>) {}
        fn on_trzsz_state_changed(&self, state: TrzszPublicState) {
            self.trzsz_states.lock().unwrap().push(state);
        }
        fn on_download_complete(&self, file_name: Option<String>, data: Vec<u8>) {
            self.downloads.lock().unwrap().push((file_name, data));
        }
        fn on_no_viable_path(&self) {}
        fn on_forward_state_changed(&self, _id: String, _state: ForwardState) {}
        fn on_agent_sign_request(&self, _key_fingerprint: String) -> bool {
            true
        }
        fn on_clipboard_write(&self, _payload: ClipboardPayload) {}
        fn on_clipboard_pull_request(&self) -> Option<ClipboardPayload> { None }
        fn on_request_wifi_fd(&self) -> Option<crate::PlatformFd> { None }
        fn on_request_cellular_fd(&self) -> Option<crate::PlatformFd> { None }
        fn on_rebind_state_changed(&self, _state: crate::rebind_manager::RebindPublicState) {}
    }

    fn shared_with_phase(phase: ConnPhase, is_quic: bool) -> (Arc<OrchestratorShared>, Arc<RecordingCallback>) {
        let callback = Arc::new(RecordingCallback::default());
        let shared = Arc::new(OrchestratorShared {
            state: Mutex::new(OrchestratorState {
                current_host: Some("example.com".to_string()),
                current_port: 22,
                is_quic,
                phase,
                current_transfer_id: None,
                trzsz_mode: None,
                download_buf: Vec::new(),
                size_limit_exceeded_for: None,
                session_generation: 0,
                reconnect_epoch: 0,
                reconnect_loop_active: false,
                retry_attempt_in_flight: false,
                user_initiated_disconnect: false,
                last_connect_attempt: None,
                reconnect_policy: ReconnectPolicy::default(),
            }),
            callback: callback.clone(),
            session: Mutex::new(None),
            path_observer: Mutex::new(net_health_policy::PathObserver::default()),
            reconnect_attempt: Box::new(connect_via),
        });
        (shared, callback)
    }

    fn orchestrator_with_phase(phase: ConnPhase, is_quic: bool) -> (SessionOrchestrator, Arc<RecordingCallback>) {
        let (shared, callback) = shared_with_phase(phase, is_quic);
        (SessionOrchestrator { shared }, callback)
    }

    /// `Connected && !is_quic`のdebounceを検証するテスト用に、debounce時間を短く
    /// 差し替えたオーケストレータを作る。
    fn orchestrator_connected_tcp_with_debounce(
        debounce: std::time::Duration,
    ) -> (SessionOrchestrator, Arc<RecordingCallback>) {
        let (shared, callback) = shared_with_phase(ConnPhase::Connected, false);
        *shared.path_observer.lock() =
            net_health_policy::PathObserver::new(net_health_policy::NetPathPolicy { debounce });
        (SessionOrchestrator { shared }, callback)
    }

    /// 自動再接続ループを検証するためのオーケストレータ。`Connected`かつ
    /// `last_connect_attempt`が設定済み(再接続可能)で、tick/retry_interval/timeoutを
    /// テスト用に短く差し替えてある。`connect_via`(実ネットワーク)は使わず、
    /// 呼び出し回数を記録するだけのフェイクに差し替えてある — `connect()`は
    /// 非同期fire-and-forgetで実際の接続結果は検証できないため、この粒度の
    /// 単体テストでは「正しいcadenceで試行が発火したか」だけを見る。
    fn orchestrator_connected_with_reconnect_policy(
        policy: ReconnectPolicy,
    ) -> (SessionOrchestrator, Arc<RecordingCallback>, Arc<std::sync::atomic::AtomicUsize>) {
        let callback = Arc::new(RecordingCallback::default());
        let attempt_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempt_count.clone();
        let shared = Arc::new(OrchestratorShared {
            state: Mutex::new(OrchestratorState {
                current_host: Some("example.com".to_string()),
                current_port: 22,
                is_quic: false,
                phase: ConnPhase::Connected,
                current_transfer_id: None,
                trzsz_mode: None,
                download_buf: Vec::new(),
                size_limit_exceeded_for: None,
                session_generation: 0,
                reconnect_epoch: 0,
                reconnect_loop_active: false,
                retry_attempt_in_flight: false,
                user_initiated_disconnect: false,
                last_connect_attempt: Some(LastConnectAttempt::Ssh(test_ssh_config())),
                reconnect_policy: policy,
            }),
            callback: callback.clone(),
            session: Mutex::new(None),
            path_observer: Mutex::new(net_health_policy::PathObserver::default()),
            reconnect_attempt: Box::new(move |_shared, _attempt| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }),
        });
        (SessionOrchestrator { shared }, callback, attempt_count)
    }

    fn test_ssh_config() -> SshConfig {
        SshConfig {
            host: "example.com".to_string(),
            port: 22,
            username: "tester".to_string(),
            auth: crate::SshAuth::Password { password: "unused".to_string() },
            cols: 80,
            rows: 24,
            forwards: Vec::new(),
            agent_forward: false,
            jump: None,
            allow_non_loopback_forward_bind: false,
        }
    }

    // ── notify_network_path_changed ──────────────────────────

    #[test]
    fn notify_network_path_changed_does_nothing_when_idle() {
        let (orch, cb) = orchestrator_with_phase(ConnPhase::Idle, false);
        orch.notify_network_path_changed(false);
        assert!(cb.connection_states.lock().unwrap().is_empty());
        assert!(orch.shared.state.lock().phase == ConnPhase::Idle);
    }

    #[test]
    fn notify_network_path_changed_aborts_and_reports_disconnected_during_handshake() {
        let (orch, cb) = orchestrator_with_phase(ConnPhase::Connecting, false);
        orch.notify_network_path_changed(false);
        let events = cb.connection_states.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ConnectionPublicState::Disconnected { reason: Some(r) } if r == "network lost"
        ));
        assert!(orch.shared.state.lock().phase == ConnPhase::Idle);
    }

    #[test]
    fn notify_network_path_changed_ignores_satisfied_updates_during_handshake() {
        // Connecting中は瞬断debounceの対象外 — 既存の即時abort挙動を維持する一方、
        // is_satisfied=trueはそもそも「断ではない」ので何もしないままで良い。
        let (orch, cb) = orchestrator_with_phase(ConnPhase::Connecting, false);
        orch.notify_network_path_changed(true);
        assert!(cb.connection_states.lock().unwrap().is_empty());
        assert!(orch.shared.state.lock().phase == ConnPhase::Connecting);
    }

    #[test]
    fn notify_network_path_changed_ignores_quic_when_connected() {
        let (orch, cb) = orchestrator_with_phase(ConnPhase::Connected, true);
        orch.notify_network_path_changed(false);
        // QUICは経路変更に自前で耐えるため、切断扱いにせずphaseもConnectedのまま維持する。
        assert!(cb.connection_states.lock().unwrap().is_empty());
        assert!(orch.shared.state.lock().phase == ConnPhase::Connected);
    }

    #[test]
    fn notify_network_path_changed_disconnects_plain_tcp_after_debounce_elapses() {
        let (orch, cb) = orchestrator_connected_tcp_with_debounce(std::time::Duration::from_millis(30));
        orch.notify_network_path_changed(false);
        assert!(
            cb.connection_states.lock().unwrap().is_empty(),
            "debounce前は即座に切断されないはず"
        );

        std::thread::sleep(std::time::Duration::from_millis(200));

        let events = cb.connection_states.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ConnectionPublicState::Disconnected { .. }));
        assert!(orch.shared.state.lock().phase == ConnPhase::Idle);
    }

    #[test]
    fn notify_network_path_changed_does_not_disconnect_plain_tcp_if_recovered_before_debounce_elapses() {
        let (orch, cb) = orchestrator_connected_tcp_with_debounce(std::time::Duration::from_millis(30));
        orch.notify_network_path_changed(false);
        orch.notify_network_path_changed(true); // 瞬断から復旧 — 保留中のdebounceをキャンセルする

        std::thread::sleep(std::time::Duration::from_millis(200));

        assert!(
            cb.connection_states.lock().unwrap().is_empty(),
            "debounce中に復旧したので切断されないはず"
        );
        assert!(orch.shared.state.lock().phase == ConnPhase::Connected);
    }

    #[test]
    fn notify_network_path_changed_pending_debounce_is_cancelled_by_a_new_connect_attempt() {
        // レビューで指摘された不具合の再現: プレーンTCP接続中に瞬断でdebounceが
        // 保留中の間、手動で別のセッションへ再接続しても、古いdebounceの発火で
        // 新しいセッションを誤って切断してはいけない。
        let (orch, cb) = orchestrator_connected_tcp_with_debounce(std::time::Duration::from_millis(30));
        orch.notify_network_path_changed(false);
        orch.begin_connect("other.example.com".to_string(), 22, false);

        std::thread::sleep(std::time::Duration::from_millis(200));

        let events = cb.connection_states.lock().unwrap();
        assert!(
            events.iter().all(|e| !matches!(e, ConnectionPublicState::Disconnected { .. })),
            "新しい接続試行後は、古いdebounce発火由来のDisconnectedが飛んではいけない, got: {events:?}"
        );
        assert!(
            orch.shared.state.lock().phase == ConnPhase::Connecting,
            "古いdebounce発火でphaseがIdleへ巻き戻されてはいけない"
        );
    }

    // ── OrchestratorAdapter (SessionCallback実装) ────────────

    fn adapter_with_phase(phase: ConnPhase, is_quic: bool) -> (OrchestratorAdapter, Arc<OrchestratorShared>, Arc<RecordingCallback>) {
        let (shared, callback) = shared_with_phase(phase, is_quic);
        (OrchestratorAdapter::new(shared.clone()), shared, callback)
    }

    #[test]
    fn on_connected_sets_phase_connected_and_reports_current_host() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connecting, false);
        adapter.on_connected();
        assert!(shared.state.lock().phase == ConnPhase::Connected);
        let events = cb.connection_states.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ConnectionPublicState::Connected { host } if host == "example.com"
        ));
    }

    #[test]
    fn on_disconnected_sets_phase_idle_and_forwards_reason() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(shared.state.lock().phase == ConnPhase::Idle);
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(
            &events[0],
            ConnectionPublicState::Disconnected { reason: Some(r) } if r == "peer closed"
        ));
    }

    #[test]
    fn on_host_key_reports_current_host_and_port_from_state() {
        let (adapter, _shared, _cb) = adapter_with_phase(ConnPhase::Connecting, false);
        // RecordingCallback::on_host_key always returns true; verifying it forwards
        // without panicking exercises the host/port read out of shared state.
        assert!(adapter.on_host_key("aa:bb:cc".to_string()));
    }

    #[test]
    fn on_trzsz_request_records_transfer_and_clears_download_buf() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().download_buf = vec![1, 2, 3];
        shared.state.lock().size_limit_exceeded_for = Some("stale".to_string());
        adapter.on_trzsz_request(
            "t1".to_string(), "download".to_string(), Some("file.txt".to_string()), Some(100),
        );
        {
            let s = shared.state.lock();
            assert_eq!(s.current_transfer_id.as_deref(), Some("t1"));
            assert_eq!(s.trzsz_mode.as_deref(), Some("download"));
            assert!(s.download_buf.is_empty());
            assert!(s.size_limit_exceeded_for.is_none(), "新しい転送開始時に前回の状態を持ち越さない");
        }
        let events = cb.trzsz_states.lock().unwrap();
        assert!(matches!(&events[0], TrzszPublicState::WaitingUser { transfer_id, .. } if transfer_id == "t1"));
    }

    #[test]
    fn on_trzsz_download_chunk_accumulates_bytes_across_calls() {
        let (adapter, shared, _cb) = adapter_with_phase(ConnPhase::Connected, false);
        adapter.on_trzsz_download_chunk("t1".to_string(), vec![1, 2], false);
        adapter.on_trzsz_download_chunk("t1".to_string(), vec![3, 4], true);
        assert_eq!(shared.state.lock().download_buf, vec![1, 2, 3, 4]);
    }

    // #60: 上限超過時にOOMせず転送を中断し、download_bufを破棄することを確認する。
    // `vec![0u8; MAX_DOWNLOAD_BUF_BYTES]`はLinux上ではゼロページの遅延確保のため
    // 実メモリをほぼ消費せず高速(かつ本テストはそれ以上書き込まない)。
    #[test]
    fn on_trzsz_download_chunk_clears_buffer_and_marks_size_limit_when_cap_exceeded() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().current_transfer_id = Some("t1".to_string());
        shared.state.lock().trzsz_mode = Some("download".to_string());
        shared.state.lock().download_buf = vec![0u8; MAX_DOWNLOAD_BUF_BYTES];

        adapter.on_trzsz_download_chunk("t1".to_string(), vec![1], false);

        let s = shared.state.lock();
        assert!(s.download_buf.is_empty(), "上限超過時はOOM回避のためdownload_bufを破棄する");
        assert_eq!(s.size_limit_exceeded_for.as_deref(), Some("t1"));
        drop(s);
        // まだon_trzsz_finishedが来ていないので、この時点ではDoneはまだ出ていない
        assert!(cb.trzsz_states.lock().unwrap().is_empty());
    }

    #[test]
    fn on_trzsz_download_chunk_stays_under_cap_does_not_mark_size_limit() {
        let (adapter, shared, _cb) = adapter_with_phase(ConnPhase::Connected, false);
        adapter.on_trzsz_download_chunk("t1".to_string(), vec![1, 2, 3], false);
        let s = shared.state.lock();
        assert_eq!(s.download_buf, vec![1, 2, 3]);
        assert!(s.size_limit_exceeded_for.is_none());
    }

    // #60: 上限超過後、非同期のtrzsz_cancel往復で本物のon_trzsz_finishedが
    // (success=false, message="Cancelled"等の汎用文言で)届いた際に、ユーザーへ
    // 分かりやすい「大きすぎる」メッセージへ差し替えて伝えることを確認する。
    #[test]
    fn on_trzsz_finished_overrides_message_when_size_limit_was_exceeded() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().current_transfer_id = Some("t1".to_string());
        shared.state.lock().trzsz_mode = Some("download".to_string());
        shared.state.lock().download_buf = vec![0u8; MAX_DOWNLOAD_BUF_BYTES];
        adapter.on_trzsz_download_chunk("t1".to_string(), vec![1], false);

        // 実際のFSMはtrzsz_cancel経由で非同期に success=false, message="Cancelled" を
        // 返してくる。ここではそれをシミュレートする。
        adapter.on_trzsz_finished("t1".to_string(), false, Some("Cancelled".to_string()));

        assert!(cb.downloads.lock().unwrap().is_empty(), "中断された転送でdownload_completeを呼んではいけない");
        let events = cb.trzsz_states.lock().unwrap();
        assert!(matches!(
            &events[0],
            TrzszPublicState::Done { success: false, message: Some(m), .. } if m.contains("大きすぎる")
        ));
        assert!(shared.state.lock().size_limit_exceeded_for.is_none(), "一度使ったフラグは消費してクリアする");
    }

    // #60: 万一cancelが競合してsuccess=trueが返ってきても、上限超過を検知していた
    // 転送は成功扱いにしない(かつ空のdownload_bufをon_download_completeへ渡さない)。
    #[test]
    fn on_trzsz_finished_forces_failure_when_size_limit_was_exceeded_even_if_reported_success() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().current_transfer_id = Some("t1".to_string());
        shared.state.lock().trzsz_mode = Some("download".to_string());
        shared.state.lock().size_limit_exceeded_for = Some("t1".to_string());

        adapter.on_trzsz_finished("t1".to_string(), true, None);

        assert!(cb.downloads.lock().unwrap().is_empty());
        let events = cb.trzsz_states.lock().unwrap();
        assert!(matches!(&events[0], TrzszPublicState::Done { success: false, .. }));
    }

    #[test]
    fn on_trzsz_finished_download_success_emits_download_complete_with_accumulated_bytes() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().trzsz_mode = Some("download".to_string());
        adapter.on_trzsz_download_chunk("t1".to_string(), vec![9, 9, 9], true);
        adapter.on_trzsz_finished("t1".to_string(), true, None);
        let downloads = cb.downloads.lock().unwrap();
        assert_eq!(downloads.len(), 1);
        assert_eq!(downloads[0].1, vec![9, 9, 9]);
        // 完了後はtransfer_id/download_bufをクリアし、次の転送に持ち越さない。
        assert!(shared.state.lock().current_transfer_id.is_none());
        assert!(shared.state.lock().download_buf.is_empty());
    }

    #[test]
    fn on_trzsz_finished_failure_does_not_emit_download_complete() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().trzsz_mode = Some("download".to_string());
        adapter.on_trzsz_download_chunk("t1".to_string(), vec![9, 9, 9], true);
        adapter.on_trzsz_finished("t1".to_string(), false, Some("connection lost".to_string()));
        assert!(cb.downloads.lock().unwrap().is_empty());
        let events = cb.trzsz_states.lock().unwrap();
        assert!(matches!(&events[0], TrzszPublicState::Done { success: false, .. }));
    }

    #[test]
    fn on_trzsz_finished_upload_does_not_emit_download_complete_even_with_buffered_bytes() {
        // upload完了時にはdownload_bufは本来空のはずだが、万一何か残っていても
        // is_download判定がfalseならon_download_completeを呼んではいけない。
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().trzsz_mode = Some("upload".to_string());
        shared.state.lock().download_buf = vec![1, 2, 3];
        adapter.on_trzsz_finished("t1".to_string(), true, None);
        assert!(cb.downloads.lock().unwrap().is_empty());
    }

    #[test]
    fn on_trzsz_progress_defaults_mode_to_download_when_unset() {
        let (adapter, _shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        adapter.on_trzsz_progress("t1".to_string(), 50, Some(100));
        let events = cb.trzsz_states.lock().unwrap();
        assert!(matches!(
            &events[0],
            TrzszPublicState::InProgress { mode, transferred: 50, total: Some(100), .. } if mode == "download"
        ));
    }

    // ── session_generation(古いセッションからの遅延コールバックを無視) ──

    #[test]
    fn stale_adapter_callbacks_are_ignored_after_a_newer_session_starts() {
        let (shared, cb) = shared_with_phase(ConnPhase::Connecting, false);
        let stale = OrchestratorAdapter::new(shared.clone());
        // 新しいセッションが生成された(session_generationが進む)状況を模す。
        let _fresh = OrchestratorAdapter::new(shared.clone());

        stale.on_connected();
        assert!(
            cb.connection_states.lock().unwrap().is_empty(),
            "古いgenerationのon_connectedはphase/通知に一切影響してはいけない"
        );
        assert!(shared.state.lock().phase == ConnPhase::Connecting, "phaseも書き換わってはいけない");

        stale.on_disconnected(Some("stale".to_string()));
        assert!(
            cb.connection_states.lock().unwrap().is_empty(),
            "古いgenerationのon_disconnectedも無視されるはず"
        );
    }

    #[test]
    fn on_host_key_returns_false_for_stale_generation() {
        let (shared, _cb) = shared_with_phase(ConnPhase::Connecting, false);
        let stale = OrchestratorAdapter::new(shared.clone());
        let _fresh = OrchestratorAdapter::new(shared.clone());
        assert!(!stale.on_host_key("aa:bb:cc".to_string()));
    }

    // ── 自動再接続ループ ──────────────────────────────────

    fn fast_test_policy() -> ReconnectPolicy {
        ReconnectPolicy {
            tick: Duration::from_millis(15),
            retry_interval: Duration::from_millis(30),
            timeout: Duration::from_millis(200),
        }
    }

    #[test]
    fn unexpected_disconnect_after_connected_starts_reconnect_loop_and_attempts_retry() {
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        let adapter = OrchestratorAdapter::new(orch.shared.clone());

        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect_loop_active, "ループが起動しているはず");

        std::thread::sleep(Duration::from_millis(80));

        let events = cb.connection_states.lock().unwrap();
        assert!(
            events.iter().any(|e| matches!(e, ConnectionPublicState::Reconnecting { .. })),
            "Reconnectingがライブ通知されるはず, got: {events:?}"
        );
        assert!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "retry_interval経過後に再接続が試みられるはず"
        );
    }

    #[test]
    fn user_initiated_disconnect_does_not_start_reconnect_loop() {
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.disconnect(); // session が None なので実際の切断処理は起きないが、フラグは立つ
        let adapter = OrchestratorAdapter::new(orch.shared.clone());

        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(!orch.shared.state.lock().reconnect_loop_active);

        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(&events[0], ConnectionPublicState::Disconnected { .. }));
    }

    #[test]
    fn disconnect_without_last_connect_attempt_does_not_start_reconnect_loop() {
        let (shared, cb) = shared_with_phase(ConnPhase::Connected, false);
        // last_connect_attemptは未設定(初回接続の失敗などを模す)。
        let adapter = OrchestratorAdapter::new(shared.clone());
        adapter.on_disconnected(Some("handshake failed".to_string()));
        assert!(!shared.state.lock().reconnect_loop_active);
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(&events[0], ConnectionPublicState::Disconnected { .. }));
    }

    #[test]
    fn graceful_remote_exit_does_not_start_reconnect_loop() {
        // リモートシェルの正常終了(`run_ssh_channel_loop`の`ChannelMsg::ExitStatus`)は
        // ネットワーク障害ではないので自動再接続してはいけない
        // (実際にこの区別が無かったことで`transport::pooling_e2e_tests::
        // one_tab_remote_exit_does_not_disconnect_sibling_tabs`が壊れた)。
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("remote process exited (status 0)".to_string()));

        assert!(!orch.shared.state.lock().reconnect_loop_active);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(&events[0], ConnectionPublicState::Disconnected { .. }));
    }

    #[test]
    fn reconnect_loop_gives_up_after_timeout_and_notifies_disconnected() {
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            // retry_intervalをtimeoutより長くして、試行を一切発火させずに
            // タイムアウトだけを検証する(実接続の副作用を避ける)。
            retry_interval: Duration::from_secs(60),
            timeout: Duration::from_millis(40),
        };
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));

        std::thread::sleep(Duration::from_millis(200));

        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!orch.shared.state.lock().reconnect_loop_active, "タイムアウト後はループが終了しているはず");
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(
            events.last(),
            Some(ConnectionPublicState::Disconnected { reason: Some(r) }) if r.contains("timed out")
        ), "ギブアップ後は理由付きでDisconnectedが通知されるはず, got: {events:?}");
    }

    #[test]
    fn cancel_reconnect_stops_loop_and_notifies_disconnected() {
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            retry_interval: Duration::from_secs(60),
            timeout: Duration::from_secs(60),
        };
        let (orch, cb, _attempt_count) = orchestrator_connected_with_reconnect_policy(policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect_loop_active);

        orch.cancel_reconnect();

        assert!(!orch.shared.state.lock().reconnect_loop_active);
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(
            events.last(),
            Some(ConnectionPublicState::Disconnected { reason: Some(r) }) if r.contains("cancelled")
        ));

        // ループ自体もepoch不一致で自然終了するはず(次tickでretryが発火しない)。
        drop(events);
        std::thread::sleep(Duration::from_millis(60));
        // cancel_reconnect後に新規の接続試行は発火しない。
    }

    #[test]
    fn a_new_manual_connect_invalidates_a_pending_reconnect_loop() {
        // レビューで指摘された既存の`notify_network_path_changed`パターンと同型:
        // 再接続ループが動いている最中に手動で新しい接続を始めたら、古いループの
        // 通知/試行が新しいセッションを誤って巻き戻してはいけない。
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            retry_interval: Duration::from_millis(20),
            timeout: Duration::from_secs(60),
        };
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect_loop_active);

        // 手動で新しい接続を開始(begin_connect相当)。
        let _new_adapter = orch.begin_connect("other.example.com".to_string(), 22, false);
        assert!(!orch.shared.state.lock().reconnect_loop_active, "新しい手動接続でループは無効化されるはず");

        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0,
            "無効化された古いループはconnect_via相当を発火してはいけない"
        );
        let events = cb.connection_states.lock().unwrap();
        assert!(
            events.iter().all(|e| !matches!(e, ConnectionPublicState::Disconnected { .. })),
            "古いループ由来のDisconnectedが飛んではいけない, got: {events:?}"
        );
    }

    #[test]
    fn apply_network_lost_on_connected_tcp_session_also_starts_reconnect_loop() {
        // always-connects.mdの実インシデント(網断debounce経路だけが自動復旧の
        // 対象外だった)の再発防止: apply_network_lost経由でも同じ
        // handle_unexpected_disconnectを通ることを確認する。
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        apply_network_lost(&orch.shared);
        assert!(orch.shared.state.lock().reconnect_loop_active);

        std::thread::sleep(Duration::from_millis(80));
        assert!(attempt_count.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        let events = cb.connection_states.lock().unwrap();
        assert!(events.iter().any(|e| matches!(e, ConnectionPublicState::Reconnecting { .. })));
    }

    #[test]
    fn reconnect_success_stops_the_loop() {
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            retry_interval: Duration::from_millis(500), // このテストでは試行が発火する前に成功させる
            timeout: Duration::from_secs(60),
        };
        let (orch, cb, _attempt_count) = orchestrator_connected_with_reconnect_policy(policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect_loop_active);

        // 別経路で再接続が成功した(例: 手動再接続やconnect_via経由の新しいセッション)ことを模す。
        let success_adapter = OrchestratorAdapter::new(orch.shared.clone());
        success_adapter.on_connected();
        // ループ自身の初回通知(spawn直後の非同期タスク)とこの成功呼び出しは別スレッドで
        // 走るため、"Connected"より前に1回だけ"Reconnecting"が紛れ込む可能性はあるが、
        // それは無害(UIは直後にConnectedへ収束する)。ここで決定的に検証できる/すべき
        // 性質は「ループ自身が停止すること」と「成功後に(タイムアウト由来の)Disconnectedが
        // 絶対に飛ばないこと」の2つ。
        assert!(!orch.shared.state.lock().reconnect_loop_active);

        std::thread::sleep(Duration::from_millis(60));
        let events = cb.connection_states.lock().unwrap();
        assert!(
            events.iter().any(|e| matches!(e, ConnectionPublicState::Connected { .. })),
            "Connectedが通知されるはず, got: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, ConnectionPublicState::Disconnected { .. })),
            "成功後にギブアップのDisconnectedが飛んではいけない, got: {events:?}"
        );
    }
}
