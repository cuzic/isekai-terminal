use std::sync::Arc;
use parking_lot::Mutex;

use crate::{
    CellData, ConnectionPublicState, ForwardState, OrchestratorCallback, ScreenUpdate,
    SessionCallback, SshConfig, SshError, TrzszPublicState,
};
use crate::quic_transport::{QuicConfig, QuicSession};
use crate::helper_quic_transport::{HelperQuicConfig, HelperQuicSession};
use crate::multipath_transport::{MultipathHelperQuicConfig, MultipathHelperQuicSession};
use crate::isekai_stun_p2p_transport::{IsekaiStunP2pConfig, IsekaiStunP2pSession};

// ── Active session ────────────────────────────────────────

enum ActiveSession {
    Ssh(Arc<crate::SshSession>),
    Quic(Arc<QuicSession>),
    HelperQuic(Arc<HelperQuicSession>),
    MultipathHelperQuic(Arc<MultipathHelperQuicSession>),
    IsekaiStunP2p(Arc<IsekaiStunP2pSession>),
}

impl ActiveSession {
    fn send(&self, data: Vec<u8>) {
        match self {
            Self::Ssh(s) => s.send(data),
            Self::Quic(s) => s.send(data),
            Self::HelperQuic(s) => s.send(data),
            Self::MultipathHelperQuic(s) => s.send(data),
            Self::IsekaiStunP2p(s) => s.send(data),
        }
    }
    fn resize(&self, cols: u32, rows: u32) {
        match self {
            Self::Ssh(s) => s.resize(cols, rows),
            Self::Quic(s) => s.resize(cols, rows),
            Self::HelperQuic(s) => s.resize(cols, rows),
            Self::MultipathHelperQuic(s) => s.resize(cols, rows),
            Self::IsekaiStunP2p(s) => s.resize(cols, rows),
        }
    }
    fn disconnect(&self) {
        match self {
            Self::Ssh(s) => s.disconnect(),
            Self::Quic(s) => s.disconnect(),
            Self::HelperQuic(s) => s.disconnect(),
            Self::MultipathHelperQuic(s) => s.disconnect(),
            Self::IsekaiStunP2p(s) => s.disconnect(),
        }
    }
    /// マルチパス以外のセッションでは意味を持たないため何もしない
    /// （呼び出し側は「そのとき使っているtransportがマルチパスかどうか」を
    /// 意識せず日和見的に呼べばよい）。
    fn rebind_to_fd(&self, fd: i32, local_ip: String) {
        if let Self::MultipathHelperQuic(s) = self {
            s.rebind_to_fd(fd, local_ip);
        }
    }
    fn scrollback_len(&self) -> u32 {
        match self {
            Self::Ssh(s) => s.scrollback_len(),
            Self::Quic(s) => s.scrollback_len(),
            Self::HelperQuic(s) => s.scrollback_len(),
            Self::MultipathHelperQuic(s) => s.scrollback_len(),
            Self::IsekaiStunP2p(s) => s.scrollback_len(),
        }
    }
    fn scrollback_cells(&self, offset: u32, rows: u32) -> Vec<CellData> {
        match self {
            Self::Ssh(s) => s.scrollback_cells(offset, rows),
            Self::Quic(s) => s.scrollback_cells(offset, rows),
            Self::HelperQuic(s) => s.scrollback_cells(offset, rows),
            Self::MultipathHelperQuic(s) => s.scrollback_cells(offset, rows),
            Self::IsekaiStunP2p(s) => s.scrollback_cells(offset, rows),
        }
    }
    fn trzsz_accept_upload(&self, transfer_id: String, file_name: String, file_size: u64, mode: u32) {
        match self {
            Self::Ssh(s) => s.trzsz_accept_upload(transfer_id, file_name, file_size, mode),
            Self::Quic(s) => s.trzsz_accept_upload(transfer_id, file_name, file_size, mode),
            Self::HelperQuic(s) => s.trzsz_accept_upload(transfer_id, file_name, file_size, mode),
            Self::MultipathHelperQuic(s) => s.trzsz_accept_upload(transfer_id, file_name, file_size, mode),
            Self::IsekaiStunP2p(s) => s.trzsz_accept_upload(transfer_id, file_name, file_size, mode),
        }
    }
    fn trzsz_send_chunk(&self, transfer_id: String, data: Vec<u8>, is_last: bool) {
        match self {
            Self::Ssh(s) => s.trzsz_send_chunk(transfer_id, data, is_last),
            Self::Quic(s) => s.trzsz_send_chunk(transfer_id, data, is_last),
            Self::HelperQuic(s) => s.trzsz_send_chunk(transfer_id, data, is_last),
            Self::MultipathHelperQuic(s) => s.trzsz_send_chunk(transfer_id, data, is_last),
            Self::IsekaiStunP2p(s) => s.trzsz_send_chunk(transfer_id, data, is_last),
        }
    }
    fn trzsz_accept_download(&self, transfer_id: String) {
        match self {
            Self::Ssh(s) => s.trzsz_accept_download(transfer_id),
            Self::Quic(s) => s.trzsz_accept_download(transfer_id),
            Self::HelperQuic(s) => s.trzsz_accept_download(transfer_id),
            Self::MultipathHelperQuic(s) => s.trzsz_accept_download(transfer_id),
            Self::IsekaiStunP2p(s) => s.trzsz_accept_download(transfer_id),
        }
    }
    fn trzsz_cancel(&self, transfer_id: String) {
        match self {
            Self::Ssh(s) => s.trzsz_cancel(transfer_id),
            Self::Quic(s) => s.trzsz_cancel(transfer_id),
            Self::HelperQuic(s) => s.trzsz_cancel(transfer_id),
            Self::MultipathHelperQuic(s) => s.trzsz_cancel(transfer_id),
            Self::IsekaiStunP2p(s) => s.trzsz_cancel(transfer_id),
        }
    }
    fn add_local_forward(&self, id: String, bind_address: String, bind_port: u16, remote_host: String, remote_port: u16) {
        match self {
            Self::Ssh(s) => s.add_local_forward(id, bind_address, bind_port, remote_host, remote_port),
            Self::Quic(s) => s.add_local_forward(id, bind_address, bind_port, remote_host, remote_port),
            // ポートフォワードは MVP スコープ上プレーン SSH / tsshd QUIC のみ対応。
            // isekai-helper 経由の QUIC 系トランスポートは未対応（対象外）。
            Self::HelperQuic(_) | Self::MultipathHelperQuic(_) | Self::IsekaiStunP2p(_) => {
                log::warn!("add_local_forward: not supported over helper-QUIC transports");
            }
        }
    }
    fn remove_forward(&self, id: String) {
        match self {
            Self::Ssh(s) => s.remove_forward(id),
            Self::Quic(s) => s.remove_forward(id),
            Self::HelperQuic(_) | Self::MultipathHelperQuic(_) | Self::IsekaiStunP2p(_) => {
                log::warn!("remove_forward: not supported over helper-QUIC transports");
            }
        }
    }
}

// ── Shared internal state ─────────────────────────────────

/// 接続状態の SSOT。`ConnectionPublicState` の Connecting/Connected の別を
/// Rust 側でも保持し、`notify_network_lost` がミラー無しで判断できるようにする。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnPhase {
    Idle,
    Connecting,
    Connected,
}

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
}

pub(crate) struct OrchestratorShared {
    state: Mutex<OrchestratorState>,
    callback: Arc<dyn OrchestratorCallback>,
    session: Mutex<Option<ActiveSession>>,
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
        }
        self.shared.callback.on_trzsz_state_changed(
            TrzszPublicState::WaitingUser { transfer_id, mode, suggested_name, expected_size }
        );
    }

    fn on_trzsz_download_chunk(&self, _transfer_id: String, data: Vec<u8>, _is_last: bool) {
        self.shared.state.lock().download_buf.extend_from_slice(&data);
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
        let (data, is_download) = {
            let mut s = self.shared.state.lock();
            s.current_transfer_id = None;
            (std::mem::take(&mut s.download_buf),
             s.trzsz_mode.as_deref() == Some("download"))
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
        }),
        callback: Arc::from(callback),
        session: Mutex::new(None),
    });
    Arc::new(SessionOrchestrator { shared })
}

#[uniffi::export]
impl SessionOrchestrator {
    pub fn connect(&self, config: SshConfig) -> Result<(), SshError> {
        {
            let mut s = self.shared.state.lock();
            s.current_host = Some(config.host.clone());
            s.current_port = config.port;
            s.is_quic = false;
            s.phase = ConnPhase::Connecting;
        }
        self.shared.callback.on_connection_state_changed(ConnectionPublicState::Connecting);
        let adapter = OrchestratorAdapter { shared: self.shared.clone() };
        let session = crate::create_ssh_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::Ssh(session));
        Ok(())
    }

    pub fn connect_quic(&self, config: QuicConfig) -> Result<(), SshError> {
        {
            let mut s = self.shared.state.lock();
            s.current_host = Some(config.ssh_host.clone());
            s.current_port = config.ssh_port;
            s.is_quic = true;
            s.phase = ConnPhase::Connecting;
        }
        self.shared.callback.on_connection_state_changed(ConnectionPublicState::Connecting);
        let adapter = OrchestratorAdapter { shared: self.shared.clone() };
        let session = crate::quic_transport::create_quic_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::Quic(session));
        Ok(())
    }

    /// Phase 7: 自作ヘルパー（isekai-helper）経由の QUIC 接続。フォールバック無し
    /// （`TransportPreference::IsekaiHelperQuic` 相当、明示選択時に使う）。
    pub fn connect_helper_quic(&self, config: HelperQuicConfig) -> Result<(), SshError> {
        {
            let mut s = self.shared.state.lock();
            s.current_host = Some(config.ssh_host.clone());
            s.current_port = config.ssh_port;
            s.is_quic = true;
            s.phase = ConnPhase::Connecting;
        }
        self.shared.callback.on_connection_state_changed(ConnectionPublicState::Connecting);
        let adapter = OrchestratorAdapter { shared: self.shared.clone() };
        let session = crate::helper_quic_transport::create_helper_quic_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::HelperQuic(session));
        Ok(())
    }

    /// Phase 7: `TransportPreference::Auto` 相当。自作ヘルパー経由 QUIC のブートストラップ/
    /// 接続に失敗した場合、内部で自動的に通常の TCP SSH にフォールバックする。
    pub fn connect_helper_quic_auto(&self, config: HelperQuicConfig) -> Result<(), SshError> {
        {
            let mut s = self.shared.state.lock();
            s.current_host = Some(config.ssh_host.clone());
            s.current_port = config.ssh_port;
            s.is_quic = true;
            s.phase = ConnPhase::Connecting;
        }
        self.shared.callback.on_connection_state_changed(ConnectionPublicState::Connecting);
        let adapter = OrchestratorAdapter { shared: self.shared.clone() };
        let session = crate::helper_quic_transport::create_helper_quic_session(config);
        session.connect_auto(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::HelperQuic(session));
        Ok(())
    }

    /// Phase 9: `TransportPreference::IsekaiHelperQuicMultipath` 相当。フォールバック無し。
    /// `config.direct_host` が設定されていれば path0（`ssh_host`）+ path1（`direct_host`）の
    /// 受動的マルチパスで接続する。
    pub fn connect_multipath_helper_quic(&self, config: MultipathHelperQuicConfig) -> Result<(), SshError> {
        {
            let mut s = self.shared.state.lock();
            s.current_host = Some(config.ssh_host.clone());
            s.current_port = config.ssh_port;
            s.is_quic = true;
            s.phase = ConnPhase::Connecting;
        }
        self.shared.callback.on_connection_state_changed(ConnectionPublicState::Connecting);
        let adapter = OrchestratorAdapter { shared: self.shared.clone() };
        let session = crate::multipath_transport::create_multipath_helper_quic_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::MultipathHelperQuic(session));
        Ok(())
    }

    /// Phase 10: `TransportPreference::IsekaiStunP2pQuic` 相当。relay 無し・
    /// STUN+SSH rendezvousによる直接 P2P QUIC。フォールバック無し（穴あけ不成立時は
    /// 接続失敗として扱う。`isekai_stun_p2p_transport.rs` 参照）。
    pub fn connect_isekai_stun_p2p(&self, config: IsekaiStunP2pConfig) -> Result<(), SshError> {
        {
            let mut s = self.shared.state.lock();
            s.current_host = Some(config.ssh_host.clone());
            s.current_port = config.ssh_port;
            s.is_quic = true;
            s.phase = ConnPhase::Connecting;
        }
        self.shared.callback.on_connection_state_changed(ConnectionPublicState::Connecting);
        let adapter = OrchestratorAdapter { shared: self.shared.clone() };
        let session = crate::isekai_stun_p2p_transport::create_isekai_stun_p2p_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::IsekaiStunP2p(session));
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
    /// プレーン TCP SSH 接続中は切断扱いにする（判断は Rust 側の SSOT で行う。
    /// Kotlin 側はイベントをそのまま転送するだけ）。
    pub fn notify_network_lost(&self) {
        let (phase, is_quic) = {
            let s = self.shared.state.lock();
            (s.phase, s.is_quic)
        };
        match phase {
            ConnPhase::Idle => {}
            ConnPhase::Connecting => {
                log::warn!("orchestrator: network lost during handshake — aborting");
                self.disconnect();
                self.shared.state.lock().phase = ConnPhase::Idle;
                self.shared.callback.on_connection_state_changed(
                    ConnectionPublicState::Disconnected { reason: Some("network lost".to_string()) }
                );
            }
            ConnPhase::Connected if !is_quic => {
                log::warn!("orchestrator: network lost while connected — disconnecting TCP session");
                self.disconnect();
                self.shared.state.lock().phase = ConnPhase::Idle;
                self.shared.callback.on_connection_state_changed(
                    ConnectionPublicState::Disconnected { reason: Some("network lost".to_string()) }
                );
            }
            ConnPhase::Connected => {
                log::info!("orchestrator: network lost — QUIC session, letting transport handle it");
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

    pub fn notify_error(&self, message: String) {
        self.shared.callback.on_connection_state_changed(
            ConnectionPublicState::Error { message }
        );
    }
}
