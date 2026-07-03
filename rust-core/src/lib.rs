uniffi::setup_scaffolding!("tssh_core");

pub mod trzsz;
pub mod quic_transport;
pub(crate) mod terminal;
pub(crate) mod transport;
pub(crate) mod session_state;
pub(crate) mod session;
pub mod orchestrator;
pub(crate) mod helper_bootstrap;
pub mod helper_quic_transport;
pub mod multipath_transport;
#[cfg(test)]
pub(crate) mod faulty_stream;
pub(crate) mod faulty_udp_socket;
pub mod debug_fault;
pub(crate) mod resume_client;

pub use quic_transport::{create_quic_session, QuicConfig, QuicSession};
pub use orchestrator::{create_session_orchestrator, SessionOrchestrator};

use std::sync::Arc;
use std::sync::LazyLock;
use tokio::runtime::Runtime;
use log::info;
use russh::client;

use crate::session::SessionCore;
use crate::transport::{RusshEventHandler, TransportCommand, TransportEvent, run_ssh_channel_loop};

pub(crate) static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    Runtime::new().expect("Failed to create Tokio runtime")
});

#[cfg(target_os = "android")]
pub(crate) fn init_logger() {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_tag("tssh-core"),
    );
}

#[cfg(not(target_os = "android"))]
pub(crate) fn init_logger() {}

// ── 定数 ────────────────────────────────────────────────

pub(crate) const DEFAULT_FG: u32 = 0xFFCCCCCC;
pub(crate) const DEFAULT_BG: u32 = 0xFF000000;

// ── 公開型 ──────────────────────────────────────────────

#[derive(Debug, Clone, uniffi::Record)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
    pub cols: u32,
    pub rows: u32,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum SshAuth {
    Password { password: String },
    PublicKey { private_key_pem: Vec<u8> },
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SshError {
    #[error("Connection failed")]
    ConnectionFailed,
    #[error("Authentication failed")]
    AuthFailed,
    #[error("Host key rejected")]
    HostKeyRejected,
    #[error("IO error")]
    IoError,
    #[error("Disconnected")]
    Disconnected,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct CellData {
    pub ch: String,
    pub fg: u32,
    pub bg: u32,
    pub bold: bool,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ScreenUpdate {
    pub cols: u32,
    pub rows: u32,
    pub cells: Vec<CellData>,
    pub cursor_row: u32,
    pub cursor_col: u32,
    pub title: Option<String>,
    pub application_cursor_mode: bool,
    pub bracketed_paste_mode: bool,
}

// ── New orchestrator public types ────────────────────────

/// Phase 7-4: プロファイルが選択するトランスポート戦略。実際のディスパッチは
/// Kotlin 側でこの値に応じて `SessionOrchestrator::connect` /
/// `connect_quic`（tsshd） / `connect_helper_quic` / `connect_helper_quic_auto`
/// のいずれかを呼び分ける（設定の意図を表す列挙型であり、単一の万能 connect API
/// を意図したものではない。既存の transport ごとに別メソッドを持つ設計を踏襲する）。
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum TransportPreference {
    /// 通常の TCP SSH（Phase 1-4）。
    PlainSsh,
    /// tsshd 互換 QUIC（Phase 5、サーバー側に事前インストールされた tsshd/isekai-helper
    /// 前身を前提とする旧経路）。
    TsshdQuic,
    /// 自作ヘルパー経由 QUIC、フォールバック無し（Phase 7、明示選択時）。
    IsekaiHelperQuic,
    /// 自作ヘルパー経由 QUIC を試し、失敗したら通常の TCP SSH にフォールバックする
    /// （Phase 7、既定推奨）。
    Auto,
    /// 自作ヘルパー経由 QUIC + Tailscale⇔直接アドレスの受動的マルチパスフェイルオーバー
    /// （Phase 9、オプトイン。フォールバック無し）。`direct_host` 未設定なら
    /// `IsekaiHelperQuic` と同等（path0 のみ）。
    IsekaiHelperQuicMultipath,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ConnectionPublicState {
    Disconnected { reason: Option<String> },
    Connecting,
    Connected { host: String },
    Error { message: String },
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum TrzszPublicState {
    Idle,
    WaitingUser {
        transfer_id: String,
        mode: String,
        suggested_name: Option<String>,
        expected_size: Option<u64>,
    },
    InProgress {
        transfer_id: String,
        mode: String,
        file_name: Option<String>,
        transferred: u64,
        total: Option<u64>,
    },
    Done {
        transfer_id: String,
        success: bool,
        message: Option<String>,
    },
}

#[uniffi::export(callback_interface)]
pub trait OrchestratorCallback: Send + Sync {
    fn on_connection_state_changed(&self, state: ConnectionPublicState);
    fn on_screen_update(&self, update: ScreenUpdate);
    fn on_host_key(&self, host: String, port: u16, fingerprint: String) -> bool;
    fn on_data(&self, data: Vec<u8>);
    fn on_trzsz_state_changed(&self, state: TrzszPublicState);
    fn on_download_complete(&self, file_name: Option<String>, data: Vec<u8>);
    /// マルチパスtransportで、現在Validatedなpathが1本も無くなった（＝手元のQUIC
    /// コネクション視点で「応答が一切返ってこない」）ことを検知した際に呼ばれる。
    /// キャプティブポータル等はQUICから見ればこれと区別が付かない（100%ロス）ため、
    /// Android OSのキャプティブポータル検知APIより先にこちらで直接検知できる。
    /// マルチパス以外のtransportでは呼ばれない。
    fn on_no_viable_path(&self);
}

// ── Old callback interface (kept for binary compatibility) ──

#[uniffi::export(callback_interface)]
pub trait SessionCallback: Send + Sync {
    fn on_data(&self, data: Vec<u8>);
    fn on_host_key(&self, fingerprint: String) -> bool;
    fn on_connected(&self);
    fn on_disconnected(&self, reason: Option<String>);
    fn on_screen_update(&self, update: ScreenUpdate);
    fn on_trzsz_request(&self, transfer_id: String, mode: String,
                        suggested_name: Option<String>, expected_size: Option<u64>);
    fn on_trzsz_download_chunk(&self, transfer_id: String, data: Vec<u8>, is_last: bool);
    fn on_trzsz_progress(&self, transfer_id: String, transferred: u64, total: Option<u64>);
    fn on_trzsz_finished(&self, transfer_id: String, success: bool, message: Option<String>);
    fn on_no_viable_path(&self);
}

// ── SshSession ──────────────────────────────────────────

#[derive(uniffi::Object)]
pub struct SshSession {
    config: SshConfig,
    core: SessionCore,
}

#[uniffi::export]
pub fn create_ssh_session(config: SshConfig) -> Arc<SshSession> {
    init_logger();
    Arc::new(SshSession { config, core: SessionCore::new() })
}

#[uniffi::export]
impl SshSession {
    pub fn connect(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
        let config = self.config.clone();
        let (cmd_rx, event_tx) = self.core.start(config.cols, config.rows, callback);
        RUNTIME.spawn(async move {
            run_russh_transport(config, cmd_rx, event_tx).await;
        });
        Ok(())
    }

    pub fn scrollback_len(&self) -> u32 { self.core.scrollback_len() }

    pub fn scrollback_cells(&self, offset: u32, rows: u32) -> Vec<CellData> {
        self.core.scrollback_cells(offset, rows)
    }

    pub fn send(&self, data: Vec<u8>) { self.core.send(data); }

    pub fn resize(&self, cols: u32, rows: u32) { self.core.resize(cols, rows); }

    pub fn disconnect(&self) { self.core.disconnect(); }

    pub fn trzsz_accept_upload(&self, transfer_id: String, file_name: String,
                               file_size: u64, mode: u32) {
        self.core.trzsz_accept_upload(transfer_id, file_name, file_size, mode);
    }

    pub fn trzsz_send_chunk(&self, transfer_id: String, data: Vec<u8>, is_last: bool) {
        self.core.trzsz_send_chunk(transfer_id, data, is_last);
    }

    pub fn trzsz_accept_download(&self, transfer_id: String) {
        self.core.trzsz_accept_download(transfer_id);
    }

    pub fn trzsz_cancel(&self, transfer_id: String) {
        self.core.trzsz_cancel(transfer_id);
    }
}

// ── TCP transport task ───────────────────────────────────

pub(crate) async fn run_russh_transport(
    config: SshConfig,
    cmd_rx: tokio::sync::mpsc::Receiver<TransportCommand>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    let russh_config = Arc::new(client::Config {
        keepalive_interval: Some(std::time::Duration::from_secs(60)),
        keepalive_max: 3,
        ..client::Config::default()
    });
    let handler = RusshEventHandler { event_tx: event_tx.clone() };

    let addr = format!("{}:{}", config.host, config.port);
    info!("ssh: TCP connecting to {}", addr);
    let session = match client::connect(russh_config, addr.as_str(), handler).await {
        Ok(s) => { info!("ssh: TCP connected to {}", addr); s }
        Err(e) => {
            log::warn!("ssh: TCP connect failed to {}: {}", addr, e);
            event_tx.send(TransportEvent::Disconnected { reason: Some(e.to_string()) }).await.ok();
            return;
        }
    };

    run_ssh_channel_loop(
        &config.username, &config.auth, config.cols, config.rows,
        session, cmd_rx, event_tx,
    ).await;
}
