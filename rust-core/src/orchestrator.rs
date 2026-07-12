use std::sync::Arc;
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
}

pub(crate) struct OrchestratorShared {
    state: Mutex<OrchestratorState>,
    callback: Arc<dyn OrchestratorCallback>,
    session: Mutex<Option<ActiveSession>>,
    /// `notify_network_path_changed`のdebounce/epoch状態。`Connected && !is_quic`の
    /// ケースだけがこれを実際に使う([`crate::net_health_policy`]参照)。
    path_observer: Mutex<crate::net_health_policy::PathObserver>,
}

// ── OrchestratorAdapter ───────────────────────────────────
// Translates old SessionCallback events → structured OrchestratorCallback

pub(crate) struct OrchestratorAdapter {
    pub(crate) shared: Arc<OrchestratorShared>,
}

impl SessionCallback for OrchestratorAdapter {
    fn on_data(&self, data: Vec<u8>) {
        self.shared.callback.on_data(data);
    }

    fn on_host_key(&self, fingerprint: String) -> bool {
        let (host, port) = {
            let s = self.shared.state.lock();
            (s.current_host.clone().unwrap_or_default(), s.current_port)
        };
        self.shared.callback.on_host_key(host, port, fingerprint)
    }

    fn on_connected(&self) {
        let host = {
            let mut s = self.shared.state.lock();
            s.phase = ConnPhase::Connected;
            s.current_host.clone().unwrap_or_default()
        };
        self.shared.callback.on_connection_state_changed(
            ConnectionPublicState::Connected { host }
        );
    }

    fn on_disconnected(&self, reason: Option<String>) {
        self.shared.state.lock().phase = ConnPhase::Idle;
        self.shared.callback.on_connection_state_changed(
            ConnectionPublicState::Disconnected { reason }
        );
    }

    fn on_screen_update(&self, update: ScreenUpdate) {
        self.shared.callback.on_screen_update(update);
    }

    fn on_trzsz_request(
        &self, transfer_id: String, mode: String,
        suggested_name: Option<String>, expected_size: Option<u64>,
    ) {
        {
            let mut s = self.shared.state.lock();
            s.current_transfer_id = Some(transfer_id.clone());
            s.trzsz_mode = Some(mode.clone());
            s.download_buf.clear();
            s.size_limit_exceeded_for = None;
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
        if success && is_download && !data.is_empty() {
            self.shared.callback.on_download_complete(None, data);
        }
        self.shared.callback.on_trzsz_state_changed(
            TrzszPublicState::Done { transfer_id, success, message }
        );
    }

    fn on_no_viable_path(&self) {
        self.shared.callback.on_no_viable_path();
    }

    fn on_forward_state_changed(&self, id: String, state: ForwardState) {
        self.shared.callback.on_forward_state_changed(id, state);
    }

    fn on_agent_sign_request(&self, key_fingerprint: String) -> bool {
        self.shared.callback.on_agent_sign_request(key_fingerprint)
    }

    fn on_clipboard_write(&self, payload: ClipboardPayload) {
        self.shared.callback.on_clipboard_write(payload);
    }

    fn on_clipboard_pull_request(&self) -> Option<ClipboardPayload> {
        self.shared.callback.on_clipboard_pull_request()
    }
}

/// `notify_network_path_changed`の実際の切断処理。`&Arc<OrchestratorShared>`だけを
/// 取る自由関数にしてあるのは、debounce後の発火が`SessionOrchestrator`自身ではなく
/// `RUNTIME.spawn`されたtokio task(`Arc<OrchestratorShared>`のcloneしか持たない)から
/// 呼ばれるため — `SessionOrchestrator::disconnect`(セッションを切るだけの2行)と
/// 中身は同じだが、`&self`経由ではなく`shared`に対して直接操作する。
fn apply_network_lost(shared: &Arc<OrchestratorShared>) {
    if let Some(s) = shared.session.lock().as_ref() {
        s.disconnect();
    }
    shared.state.lock().phase = ConnPhase::Idle;
    shared.callback.on_connection_state_changed(
        ConnectionPublicState::Disconnected { reason: Some("network lost".to_string()) }
    );
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
        }),
        callback: Arc::from(callback),
        session: Mutex::new(None),
        path_observer: Mutex::new(crate::net_health_policy::PathObserver::default()),
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
        }
        // 新しい接続試行が始まった時点で、直前のセッションに対して保留中だった
        // network-path debounceは無効化する。そうしないと、瞬断のdebounce待機中に
        // 手動で切断/別transportへ再接続した場合、無関係な新しいセッションを
        // 誤って切断してしまう(レビューで指摘された実際の不具合)。
        self.shared.path_observer.lock().invalidate();
        self.shared.callback.on_connection_state_changed(ConnectionPublicState::Connecting);
        OrchestratorAdapter { shared: self.shared.clone() }
    }
}

#[uniffi::export]
impl SessionOrchestrator {
    pub fn connect(&self, config: SshConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.host.clone(), config.port, false);
        let session = crate::create_ssh_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::Ssh(session));
        Ok(())
    }

    pub fn connect_quic(&self, config: QuicConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true);
        let session = crate::quic_transport::create_quic_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::Quic(session));
        Ok(())
    }

    /// Phase 7: 自作ヘルパー（isekai-helper）経由の QUIC 接続。フォールバック無し
    /// （`TransportPreference::IsekaiPipeQuic` 相当、明示選択時に使う）。
    pub fn connect_isekai_pipe_quic(&self, config: IsekaiPipeQuicConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true);
        let session = crate::isekai_pipe_quic_transport::create_isekai_pipe_quic_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::IsekaiPipeQuic(session));
        Ok(())
    }

    /// Phase 7: `TransportPreference::Auto` 相当。自作ヘルパー経由 QUIC のブートストラップ/
    /// 接続に失敗した場合、内部で自動的に通常の TCP SSH にフォールバックする。
    pub fn connect_isekai_pipe_quic_auto(&self, config: IsekaiPipeQuicConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true);
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
        let session = crate::isekai_stun_p2p_transport::create_isekai_stun_p2p_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::IsekaiStunP2p(session));
        Ok(())
    }

    /// Phase 10: `TransportPreference::IsekaiLinkRelayQuic` 相当。MASQUE relay 経由の
    /// P2P QUIC。フォールバック無し（`isekai_link_relay_transport.rs` 参照）。
    pub fn connect_isekai_link_relay(&self, config: IsekaiLinkRelayConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true);
        let session = crate::isekai_link_relay_transport::create_isekai_link_relay_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::IsekaiLinkRelay(session));
        Ok(())
    }

    pub fn disconnect(&self) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.disconnect();
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
            }
        }
    }

    pub fn trzsz_dismiss(&self) {
        let mut s = self.shared.state.lock();
        s.trzsz_mode = None;
        s.current_transfer_id = None;
        drop(s);
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
            }),
            callback: callback.clone(),
            session: Mutex::new(None),
            path_observer: Mutex::new(net_health_policy::PathObserver::default()),
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
        (OrchestratorAdapter { shared: shared.clone() }, shared, callback)
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
}
