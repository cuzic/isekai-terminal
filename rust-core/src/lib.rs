uniffi::setup_scaffolding!("tssh_core");

pub mod trzsz;
pub mod quic_transport;
pub(crate) mod agent_forward;
pub(crate) mod terminal;
pub(crate) mod theme;
pub(crate) mod transport;
pub(crate) mod session_state;
pub(crate) mod session;
pub mod orchestrator;
pub(crate) mod helper_bootstrap;
pub mod helper_quic_transport;
pub mod multipath_transport;
pub mod isekai_stun_p2p_transport;
pub mod isekai_link_relay_transport;
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
use russh::client;

use crate::session::SessionCore;
use crate::transport::{TransportCommand, TransportEvent, run_ssh_channel_loop};

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

// ── ターミナル配色テーマ ──────────────────────────────────
// 配色パレット自体（ANSI 16色・デフォルト fg/bg）は `theme` モジュールが
// プロセス全体で共有するグローバル状態として保持する（`theme::Theme` 参照）。
// ここではその差し替え用の UniFFI エントリポイントのみを公開する。

/// ターミナルの配色テーマを差し替える（プロファイル毎ではなくグローバル設定）。
///
/// `ansi16` は SGR が参照する 16 色を ARGB（`0xAARRGGBB`）で `[normal 8色, bright 8色]`
/// の順に渡す。16 個に満たない場合は残りを既定テーマの値で埋め、16 個を超える分は無視する。
/// 呼び出し以降にパースされる SGR にのみ反映され、既に scrollback に積まれた行は
/// 遡って再着色されない（既知の制約）。
#[uniffi::export]
pub fn set_terminal_theme(ansi16: Vec<u32>, default_fg: u32, default_bg: u32) {
    let mut table = theme::Theme::default().ansi16;
    for (slot, v) in table.iter_mut().zip(ansi16.into_iter()) {
        *slot = v;
    }
    theme::set(theme::Theme { ansi16: table, default_fg, default_bg });
}

// ── 公開型 ──────────────────────────────────────────────

#[derive(Debug, Clone, uniffi::Record)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
    pub cols: u32,
    pub rows: u32,
    /// ローカルポートフォワード(-L)の一覧。接続確立後に自動で待受を開始する。
    pub forwards: Vec<PortForward>,
    /// SSH agent forwarding。既定 OFF・プロファイル単位 opt-in。
    /// 有効でも公開鍵認証以外（パスワード認証）の場合は転送しない。
    /// 有効な場合、サーバー側からの署名要求は毎回ユーザー確認を必須とする
    /// （`OrchestratorCallback::on_agent_sign_request` / `SessionCallback::on_agent_sign_request`）。
    pub agent_forward: bool,
    /// 設定されていれば、`host:port` へ直接ではなく、まずこの踏み台ホストへ
    /// SSH接続・認証し、そこから `channel_open_direct_tcpip` で `host:port` への
    /// チャネルを開いた上にネストしたSSHセッションを張る（`ssh -J` 相当）。
    /// 対象ホストがNAT配下で直接到達できない場合の唯一の到達経路になる。
    pub jump: Option<JumpConfig>,
    /// `forwards` の `bind_address` が非ループバック（127.0.0.0/8・::1・localhost以外）の
    /// 場合に、それを許可するかどうか。既定 false。Kotlin側UI警告だけに頼らずコア側でも
    /// 強制する（Rust SSOTルール、外部レビュー指摘対応）。false時に非ループバックbindが
    /// 指定された場合、そのforwardは`ForwardState::Failed`として拒否される
    /// （セッション自体は切断されない。他のforwardには影響しない）。
    pub allow_non_loopback_forward_bind: bool,
}

/// ProxyJump（多段SSH）の踏み台ホストへの接続情報。`SshConfig::jump` 参照。
#[derive(Debug, Clone, uniffi::Record)]
pub struct JumpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
}

// ── ポートフォワード(-L のみ、MVP) ───────────────────────

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ForwardType {
    /// `ssh -L bind:remote_host:remote_port` 相当。Dynamic/Remote は将来拡張。
    Local,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PortForward {
    pub forward_type: ForwardType,
    /// 待受アドレス。既定は "127.0.0.1"("0.0.0.0" 等にすると同一 LAN 上の
    /// 第三者からアクセスされ得るため UI 側で警告する)。
    pub bind_address: String,
    pub bind_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
}

/// ポートフォワード待受の状態。`OrchestratorCallback::on_forward_state_changed` で通知される。
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ForwardState {
    Listening,
    Failed { reason: String },
    Stopped,
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
    /// STUN+SSH rendezvous による直接 P2P QUIC（Phase 10、オプトイン。relay 無し・
    /// 穴あけ不成立時のフォールバック無し）。`isekai_stun_p2p_transport.rs` 参照。
    /// relay 経由の MASQUE ベース P2P（`IsekaiLinkRelayQuic`）とは独立したトランスポート。
    IsekaiStunP2pQuic,
    /// MASQUE relay 経由の P2P QUIC（Phase 10、オプトイン。フォールバック無し）。
    /// `isekai_link_relay_transport.rs` 参照。`IsekaiStunP2pQuic` と異なり relay が常時
    /// 経路に残るため NAT の種類に左右されないが、relay サーバー・JWT が必要。
    IsekaiLinkRelayQuic,
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
    fn on_forward_state_changed(&self, id: String, state: ForwardState);
    /// SSH agent forwarding: 転送された鍵での署名要求を、要求ごとにユーザーへ確認する。
    /// `true` を返すと署名を実行し、`false` なら拒否する。呼び出し元は host key 確認と
    /// 同じ同期ブロッキング方式（Rust 側の `spawn_blocking` から呼ばれる）を使うため、
    /// この実装は呼び出し元スレッドをブロックしてユーザー操作を待ってよい
    /// （実装例は `TerminalSession.kt` の `onAgentSignRequest` を参照）。
    fn on_agent_sign_request(&self, key_fingerprint: String) -> bool;
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
    fn on_forward_state_changed(&self, id: String, state: ForwardState);
    fn on_agent_sign_request(&self, key_fingerprint: String) -> bool;
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
        // config.forwards はコマンドチャネル経由で "AddLocalForward" として投入する。
        // run_ssh_channel_loop がシェル起動後に select ループへ入った時点で消費され、
        // 待受タスクが起動する(Kotlin から動的に追加/削除する将来の拡張と同じ経路)。
        if let Some(tx) = self.core.command_sender() {
            for (i, pf) in config.forwards.iter().enumerate() {
                let cmd = TransportCommand::AddLocalForward {
                    id: format!("lf-{i}"),
                    bind_addr: pf.bind_address.clone(),
                    bind_port: pf.bind_port,
                    remote_host: pf.remote_host.clone(),
                    remote_port: pf.remote_port,
                };
                if tx.try_send(cmd).is_err() {
                    log::warn!("ssh: failed to queue initial forward #{i} (channel full?)");
                }
            }
        }
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

// ── ポートフォワードの動的追加/削除 ───────────────────────
// SessionOrchestrator からのみ呼ばれる内部 API(uniffi には直接は出さない)。
// MVP の ProfileEditScreen は接続時に forwards をまとめて適用するだけだが、
// 将来 Kotlin から接続中に動的に追加/削除する UI を足すときはここを export すればよい。
impl SshSession {
    pub(crate) fn add_local_forward(
        &self, id: String, bind_address: String, bind_port: u16, remote_host: String, remote_port: u16,
    ) {
        if let Some(tx) = self.core.command_sender() {
            let cmd = TransportCommand::AddLocalForward { id, bind_addr: bind_address, bind_port, remote_host, remote_port };
            if tx.try_send(cmd).is_err() {
                log::warn!("ssh: add_local_forward command dropped (channel full)");
            }
        }
    }

    pub(crate) fn remove_forward(&self, id: String) {
        if let Some(tx) = self.core.command_sender() {
            if tx.try_send(TransportCommand::RemoveForward { id }).is_err() {
                log::warn!("ssh: remove_forward command dropped (channel full)");
            }
        }
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

    // `established.jump_handle`(Some の場合)は、これが保持する接続の上に
    // トンネルされた `established.handle` が乗っているため、`run_ssh_channel_loop`
    // が終わるまで(＝このスコープの終わりまで)drop してはならない。
    let established = match transport::connect_via_jump_or_direct(
        &config.jump, russh_config, &config.host, config.port, event_tx.clone(),
    ).await {
        Ok(e) => e,
        Err(msg) => {
            log::warn!("ssh: {msg}");
            event_tx.send(TransportEvent::Disconnected { reason: Some(msg) }).await.ok();
            return;
        }
    };
    let session = established.handle;
    let agent_key = established.agent_key;

    run_ssh_channel_loop(
        &config.username, &config.auth, config.cols, config.rows,
        config.agent_forward, agent_key, config.allow_non_loopback_forward_bind,
        session, cmd_rx, event_tx,
    ).await;
}
