//! Phase 10: MASQUE relay(`seera-networks/axum-masque-rs`の`bound-udp-server`)経由の
//! P2P QUIC トランスポート。
//!
//! `isekai_stun_p2p_transport.rs`(relay無し・穴あけ不成立時のフォールバック無し)とは
//! 対照的に、こちらは relay が常時経路に残るため NAT の種類に関わらず接続できる
//! （relay到達不可時のフォールバックは無いが、そもそも「relayへの到達性」は一般の
//! インターネットホストへのHTTPS到達性と同程度に安定している前提）。
//!
//! isekai-terminal 自身は MASQUE/HTTP/3/capsule を一切意識しない。実際に relay と
//! CONNECT-UDP-bind トンネルを張る（`isekai-link-masque`クレートの「agent役」API を使う）のは
//! isekai-helper 側（`isekai-helper/src/main.rs`の`--relay`モード）であり、isekai-terminal
//! が知る必要があるのは relay が割り当てた公開アドレス（SSH ブートストラップの
//! ハンドシェイク JSON 経由で受け取る、`isekai_stun_p2p_transport.rs`の`stun_observed_addr`と
//! 同じパターン）だけである。そこから先は`isekai_pipe_quic_transport.rs`と全く同じ
//! HELLO/proof/ACK クライアントロジックで接続を確立する。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use log::{info, warn};
use russh::client;

use crate::helper_bootstrap::{self, BootstrapError, IsekaiPipeBinaries, IsekaiPipeHandshake, IsekaiPipeP2pMode};
use crate::isekai_pipe_quic_transport::{
    self, spawn_bootstrap_host_key_forwarder, ISEKAI_PIPE_BIN_AARCH64, ISEKAI_PIPE_BIN_X86_64, ISEKAI_PIPE_VERSION,
};
use crate::resume_client::{self, ClientResumeState};
use crate::transport::{
    authenticate_session, connect_via_jump_or_direct, run_ssh_channel_loop,
    TransportCommand, TransportEvent,
};
use crate::{init_logger, CellData, JumpConfig, ScrollbackSearchMatch, SessionCallback, SshAuth, SshError, RUNTIME};
use crate::session::SessionCore;

const DEFAULT_RESUME_BUFFER_SIZE: usize = 4 * 1024 * 1024;
const CONTROL_STREAM_TIMEOUT: Duration = Duration::from_secs(5);
/// relayへのブートストラップ + QUIC接続確立全体のタイムアウト。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

// ── 公開型 ──────────────────────────────────────────────

#[derive(Debug, Clone, uniffi::Record)]
pub struct IsekaiLinkRelayConfig {
    pub ssh_host: String,
    pub ssh_port: u16,
    pub username: String,
    pub auth: SshAuth,
    pub cols: u32,
    pub rows: u32,
    /// ブートストラップ用SSH接続の踏み台(ProxyJump)。`SshConfig::jump`参照。
    pub jump: Option<JumpConfig>,
    /// MASQUE relay(`bound-udp-server`)のアドレス(`host:port`)。
    pub relay_addr: String,
    /// `relay_addr`のTLS SNI/HTTPオーソリティ。
    pub relay_sni: String,
    /// `relay_addr`への認証に使うBearerトークン(JWT)。取得・更新はKotlin側の責務
    /// （PLAN.md Phase 10-4、JWT発行・配布フロー参照）——rust-core側はトークン文字列を
    /// 受け取るだけで、その取得方法には関知しない。
    pub relay_jwt: String,
}

// `SessionOrchestrator`(orchestrator.rs)がActiveSession::IsekaiLinkRelayとして
// 内部的に使う実装。両OSともSessionOrchestrator/OrchestratorCallbackへ移行済みのため
// (2026-07-11)、UniFFIへの公開はやめてクレート内部専用にした。
pub(crate) struct IsekaiLinkRelaySession {
    config: IsekaiLinkRelayConfig,
    core: SessionCore,
}

pub(crate) fn create_isekai_link_relay_session(config: IsekaiLinkRelayConfig) -> Arc<IsekaiLinkRelaySession> {
    init_logger();
    Arc::new(IsekaiLinkRelaySession { config, core: SessionCore::new() })
}

impl IsekaiLinkRelaySession {
    /// MASQUE relay経由の直接P2P QUICのみを試す。フォールバック無し
    /// （relayへの到達・認証・トンネル確立のいずれかが失敗すれば接続失敗として扱う）。
    pub(crate) fn connect(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
        let config = self.config.clone();
        let (cmd_rx, event_tx) = self.core.start(config.cols, config.rows, callback);
        // ブートストラップ用SSHのホスト鍵検証を本セッションのcallbackに委譲する
        // (`isekai_pipe_quic_transport::bootstrap_helper_via_ssh`のNOTE参照)。
        let host_key_callback = self.core.callback();
        RUNTIME.spawn(async move {
            match tokio::time::timeout(
                CONNECT_TIMEOUT,
                try_connect_isekai_link_relay(&config, host_key_callback),
            )
            .await
            {
                Ok(Ok(stream)) => run_over_stream(config, stream, cmd_rx, event_tx).await,
                Ok(Err(e)) => {
                    warn!("isekai_link_relay: connect failed: {e}");
                    event_tx.send(TransportEvent::Disconnected { reason: Some(e) }).await.ok();
                }
                Err(_) => {
                    warn!("isekai_link_relay: connect timed out");
                    event_tx
                        .send(TransportEvent::Disconnected {
                            reason: Some("timed out establishing relay P2P connection".to_string()),
                        })
                        .await
                        .ok();
                }
            }
        });
        Ok(())
    }

    pub(crate) fn scrollback_len(&self) -> u32 { self.core.scrollback_len() }

    pub(crate) fn scrollback_cells(&self, offset: u32, rows: u32) -> Vec<CellData> {
        self.core.scrollback_cells(offset, rows)
    }

    pub(crate) fn search_scrollback(&self, query: String, case_sensitive: bool) -> Vec<ScrollbackSearchMatch> {
        self.core.search_scrollback(&query, case_sensitive)
    }

    pub(crate) fn send(&self, data: Vec<u8>) { self.core.send(data); }

    pub(crate) fn resize(&self, cols: u32, rows: u32) { self.core.resize(cols, rows); }

    pub(crate) fn disconnect(&self) { self.core.disconnect(); }

    pub(crate) fn trzsz_accept_upload(&self, transfer_id: String, file_name: String,
                               file_size: u64, mode: u32) {
        self.core.trzsz_accept_upload(transfer_id, file_name, file_size, mode);
    }

    pub(crate) fn trzsz_send_chunk(&self, transfer_id: String, data: Vec<u8>, is_last: bool) {
        self.core.trzsz_send_chunk(transfer_id, data, is_last);
    }

    pub(crate) fn trzsz_accept_download(&self, transfer_id: String) {
        self.core.trzsz_accept_download(transfer_id);
    }

    pub(crate) fn trzsz_cancel(&self, transfer_id: String) {
        self.core.trzsz_cancel(transfer_id);
    }

    /// Phase 12: per-session theme。
    pub(crate) fn set_theme(&self, theme: crate::theme::Theme) {
        self.core.set_theme(theme);
    }
}

// ── ブートストラップ ─────────────────────────────────────

async fn try_connect_isekai_link_relay(
    config: &IsekaiLinkRelayConfig,
    host_key_callback: Option<Arc<dyn SessionCallback>>,
) -> Result<resume_client::ReattachableStream, String> {
    let relay_addr: SocketAddr = tokio::net::lookup_host(&config.relay_addr)
        .await
        .map_err(|e| format!("relayのDNS解決に失敗: {e}"))?
        .next()
        .ok_or_else(|| "relayのアドレスが解決できません".to_string())?;

    let handshake = bootstrap_via_ssh_with_relay(config, relay_addr, host_key_callback).await?;

    let helper_addr: SocketAddr = handshake
        .relay_public_addr()
        .ok_or_else(|| {
            "isekai-helper がrelay公開アドレスを報告しませんでした（--relay指定なのに \
             relay_public_addrが無い——helper_bootstrap.rsとisekai-helperのバージョン不一致の可能性）"
                .to_string()
        })?
        .parse()
        .map_err(|e| format!("isekai-helper が返したrelay公開アドレスが不正: {e}"))?;
    info!("isekai_link_relay: relay-assigned public address is {helper_addr}");

    connect_relay_stream(helper_addr, &handshake).await
}

/// ProxyJump対応のSSH接続を張り、`--relay`/`--relay-sni`/`--relay-jwt`付きでisekai-helperを
/// ブートストラップ起動する。`isekai_stun_p2p_transport.rs::bootstrap_via_ssh_with_punch`と
/// 同じ理由でコード共有はせず複製している。ホスト鍵検証ループ
/// (`spawn_bootstrap_host_key_forwarder`)自体は共有する。
async fn bootstrap_via_ssh_with_relay(
    config: &IsekaiLinkRelayConfig,
    relay_addr: SocketAddr,
    host_key_callback: Option<Arc<dyn SessionCallback>>,
) -> Result<IsekaiPipeHandshake, String> {
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
    spawn_bootstrap_host_key_forwarder(event_rx, host_key_callback);

    let russh_config = Arc::new(client::Config::default());
    let mut established = connect_via_jump_or_direct(
        &config.jump, russh_config, &config.ssh_host, config.ssh_port, event_tx,
    )
    .await
    .map_err(|e| format!("bootstrap SSH connect failed: {e}"))?;

    let (authenticated, _) =
        authenticate_session(&mut established.handle, &config.username, &config.auth).await;
    if !authenticated {
        return Err("bootstrap SSH authentication failed".to_string());
    }

    let binaries = IsekaiPipeBinaries { x86_64: ISEKAI_PIPE_BIN_X86_64, aarch64: ISEKAI_PIPE_BIN_AARCH64 };
    let p2p_mode = IsekaiPipeP2pMode::Relay {
        relay_addr,
        relay_sni: config.relay_sni.clone(),
        relay_jwt: config.relay_jwt.clone(),
    };
    // relay経路にはSTUNサーバー設定が無いため常に空スライス
    // (isekai-terminal-core/isekai-ssh crate共有化 Phase 2c)。
    helper_bootstrap::ensure_helper_running(
        &mut established.handle,
        &binaries,
        ISEKAI_PIPE_VERSION,
        "127.0.0.1:22",
        None,
        &p2p_mode,
        &[],
    )
    .await
    .map_err(|e: BootstrapError| format!("bootstrap failed: {e}"))
}

// ── QUIC 接続（HELLO/ACK ハンドシェイク） ───────────────
// `isekai_pipe_quic_transport.rs`と同じワイヤー契約(HELPER_PROTOCOL.md)を再利用する。
// STUN版と異なり、穴あけ用ソケットの使い回しは不要（relayは直接到達可能なので、
// 通常の新規エフェメラルソケットで問題ない）。

async fn connect_relay_stream(
    helper_addr: SocketAddr,
    handshake: &IsekaiPipeHandshake,
) -> Result<resume_client::ReattachableStream, String> {
    let cert_sha256_hex = handshake.cert_sha256().to_string();
    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&handshake.session_secret)
        .map_err(|e| format!("invalid session_secret encoding: {e}"))?;

    let target = isekai_transport::RelayTarget {
        helper_addr,
        server_name: isekai_pipe_quic_transport::QUIC_SERVER_NAME.to_string(),
        cert_sha256_hex,
        session_secret,
        // No local-port-range restriction on Android today (see
        // `isekai_pipe_quic_transport.rs`'s equivalent site).
        local_bind_port_range: None,
    };
    let factory = crate::android_quic_endpoint::factory();
    let (conn, data_stream, proof) = isekai_transport::connect_via_relay_with_connection(&factory, &target)
        .await
        .map_err(|e| e.to_string())?;
    info!("isekai_link_relay: ATTACH ok — handing off to SSH");

    let resume_state = Arc::new(std::sync::Mutex::new(ClientResumeState::new(
        DEFAULT_RESUME_BUFFER_SIZE,
    )));

    {
        let resume_state = resume_state.clone();
        RUNTIME.spawn(async move {
            match tokio::time::timeout(
                CONTROL_STREAM_TIMEOUT,
                isekai_transport::resume::open_control_stream(&conn, &proof),
            )
            .await
            {
                Ok(Ok(control)) => {
                    let session_id = *control.session_id.as_bytes();
                    info!(
                        "isekai_link_relay: control stream established (resume support enabled), session_id={}",
                        session_id.iter().map(|b| format!("{b:02x}")).collect::<String>()
                    );
                    resume_state.lock().unwrap().session_id = Some(session_id);
                    let counters = Arc::new(isekai_transport::resume::AppAckCounters::new());
                    isekai_transport::resume::spawn_app_ack_tasks(control.stream, counters.clone());
                    isekai_pipe_quic_transport::spawn_app_ack_bridge(resume_state, counters);
                }
                Ok(Err(e)) => {
                    info!("isekai_link_relay: control stream handshake failed ({e}), continuing without resume support");
                }
                Err(_) => {
                    info!("isekai_link_relay: control stream not accepted within timeout, continuing without resume support");
                }
            }
        });
    }

    // reattach(RESUME)はrelay公開アドレスへ新規エフェメラルソケットから繋ぎ直すだけ。
    // relayは常時経路に残る(常にトンネルを維持している)ため、STUN版のような
    // 「NATマッピングが失われて復旧不能」という制約は無い——relay自体への到達性が
    // 保たれている限り、何度でも同じアドレスへ繋ぎ直せる。
    let reattach_fn: resume_client::ReattachFn<quicmux::AnyByteStreamReadHalf, quicmux::AnyByteStreamWriteHalf> = Arc::new({
        let resume_state = resume_state.clone();
        move |session_id, client_sent_offset, client_delivered_offset| {
            let factory = crate::android_quic_endpoint::factory();
            let target = target.clone();
            let resume_state = resume_state.clone();
            Box::pin(async move {
                let outcome = isekai_transport::resume::reconnect_and_resume(
                    &factory,
                    &target,
                    isekai_transport::SessionId::from_bytes(session_id),
                    isekai_transport::C2hSentOffset::new(client_sent_offset),
                    isekai_transport::H2cClientDeliveredOffset::new(client_delivered_offset),
                )
                .await
                .map_err(|e| e.to_string())?;
                info!("isekai_link_relay: resume succeeded, helper_committed_offset={}", outcome.helper_committed_offset);
                isekai_pipe_quic_transport::spawn_control_stream_reestablishment_after_resume(
                    "isekai_link_relay",
                    outcome.connection.clone(),
                    target.session_secret.clone(),
                    resume_state,
                );
                let (read, write) = outcome.data_stream.split();
                Ok(resume_client::ReattachResult { read, write, helper_committed_offset: outcome.helper_committed_offset.get() })
            })
        }
    });

    let (data_read, data_write) = data_stream.split();
    Ok(resume_client::ReattachableStream::new(data_read, data_write, resume_state, reattach_fn))
}

async fn run_over_stream(
    mut config: IsekaiLinkRelayConfig,
    stream: resume_client::ReattachableStream,
    cmd_rx: tokio::sync::mpsc::Receiver<TransportCommand>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    let russh_config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(60)),
        keepalive_max: 3,
        ..client::Config::default()
    });

    // link relay(実験的opt-in機能)はSSH接続プーリング(`archive/ISEKAI_SSH_DESIGN.md`
    // 「今後の課題」参照)のスコープ外。タブごとに毎回新規のQUIC接続・ネストしたSSH認証を
    // 行う、これまでと同じ挙動のまま。
    let pooled = match crate::transport::establish_ssh_handle_over_stream(
        russh_config, stream, &config.username, &mut config.auth, false, &event_tx,
    ).await {
        Ok(p) => p,
        Err(msg) => {
            event_tx.send(TransportEvent::Disconnected { reason: Some(msg) }).await.ok();
            return;
        }
    };

    // IsekaiLinkRelayConfig は agent forwarding 未対応（`IsekaiPipeQuicConfig` と同様）。
    run_ssh_channel_loop(&pooled, config.cols, config.rows, false, false, cmd_rx, event_tx).await;
}
