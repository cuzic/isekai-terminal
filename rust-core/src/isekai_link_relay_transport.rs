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
//! 同じパターン）だけである。そこから先は`helper_quic_transport.rs`と全く同じ
//! HELLO/proof/ACK クライアントロジックで接続を確立する。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use log::{info, warn};
use russh::client;

use crate::helper_bootstrap::{self, BootstrapError, HelperBinaries, HelperHandshake, HelperP2pMode};
use crate::helper_quic_transport::{
    self, compute_proof, establish_quic_connection_with_socket, open_control_stream,
    spawn_app_ack_tasks, spawn_bootstrap_host_key_forwarder, FRAME_ACK, FRAME_HELLO,
    FRAME_REJECT_AUTH, FRAME_REJECT_DUPLICATE, FRAME_REJECT_TARGET, FRAME_REJECT_UNSUPPORTED,
    HELPER_BIN_AARCH64, HELPER_BIN_X86_64, HELPER_VERSION,
};
use crate::resume_client::{self, ClientResumeState};
use crate::transport::{
    authenticate_session, connect_via_jump_or_direct, run_ssh_channel_loop, RusshEventHandler,
    TransportCommand, TransportEvent,
};
use crate::{init_logger, CellData, JumpConfig, SessionCallback, SshAuth, SshError, RUNTIME};
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

#[derive(uniffi::Object)]
pub struct IsekaiLinkRelaySession {
    config: IsekaiLinkRelayConfig,
    core: SessionCore,
}

#[uniffi::export]
pub fn create_isekai_link_relay_session(config: IsekaiLinkRelayConfig) -> Arc<IsekaiLinkRelaySession> {
    init_logger();
    Arc::new(IsekaiLinkRelaySession { config, core: SessionCore::new() })
}

#[uniffi::export]
impl IsekaiLinkRelaySession {
    /// MASQUE relay経由の直接P2P QUICのみを試す。フォールバック無し
    /// （relayへの到達・認証・トンネル確立のいずれかが失敗すれば接続失敗として扱う）。
    pub fn connect(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
        let config = self.config.clone();
        let (cmd_rx, event_tx) = self.core.start(config.cols, config.rows, callback);
        // ブートストラップ用SSHのホスト鍵検証を本セッションのcallbackに委譲する
        // (`helper_quic_transport::bootstrap_helper_via_ssh`のNOTE参照)。
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

    /// Phase 1C(#26): OSからネットワーク断を通知された時の対応(`SessionCore`が
    /// 判断、詳細は`session.rs`の`should_abort_on_network_lost`参照)。QUICは
    /// `is_quic=true`固定 — 接続済みならtransport自身のtransparent resumeを信頼し
    /// 何もしない。
    pub fn notify_network_lost(&self) {
        self.core.notify_network_lost(true);
    }
}

// SessionOrchestrator からのみ呼ばれる内部API(uniffi には直接は出さない)。
impl IsekaiLinkRelaySession {
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
        .relay_public_addr
        .as_deref()
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
) -> Result<HelperHandshake, String> {
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

    let binaries = HelperBinaries { x86_64: HELPER_BIN_X86_64, aarch64: HELPER_BIN_AARCH64 };
    let p2p_mode = HelperP2pMode::Relay {
        relay_addr,
        relay_sni: config.relay_sni.clone(),
        relay_jwt: config.relay_jwt.clone(),
    };
    helper_bootstrap::ensure_helper_running(
        &mut established.handle,
        &binaries,
        HELPER_VERSION,
        "127.0.0.1:22",
        None,
        &p2p_mode,
    )
    .await
    .map_err(|e: BootstrapError| format!("bootstrap failed: {e}"))
}

// ── QUIC 接続（HELLO/ACK ハンドシェイク） ───────────────
// `helper_quic_transport.rs`と同じワイヤー契約(HELPER_PROTOCOL.md)を再利用する。
// STUN版と異なり、穴あけ用ソケットの使い回しは不要（relayは直接到達可能なので、
// 通常の新規エフェメラルソケットで問題ない）。

async fn connect_relay_stream(
    helper_addr: SocketAddr,
    handshake: &HelperHandshake,
) -> Result<resume_client::ReattachableStream, String> {
    let cert_sha256_hex = handshake.cert_sha256.clone();
    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&handshake.session_secret)
        .map_err(|e| format!("invalid session_secret encoding: {e}"))?;

    let socket = crate::faulty_udp_socket::bind_faulty_udp_socket(
        "0.0.0.0:0".parse().unwrap(),
        crate::debug_fault::shared_injector(),
    )
    .map_err(|e| format!("endpoint bind failed: {e}"))?;
    let conn = establish_quic_connection_with_socket(socket, helper_addr, &cert_sha256_hex).await?;

    let proof = compute_proof(&conn, &session_secret, b"")?;

    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("open_bi failed: {e}"))?;
    let mut hello = Vec::with_capacity(33);
    hello.push(FRAME_HELLO);
    hello.extend_from_slice(&proof);
    send.write_all(&hello).await.map_err(|e| format!("HELLO write failed: {e}"))?;

    let mut resp = [0u8; 1];
    recv.read_exact(&mut resp).await.map_err(|e| format!("ACK read failed: {e}"))?;
    match resp[0] {
        FRAME_ACK => {}
        FRAME_REJECT_AUTH => return Err("isekai-helper rejected: auth (proof mismatch)".to_string()),
        FRAME_REJECT_DUPLICATE => {
            return Err("isekai-helper rejected: duplicate active connection".to_string())
        }
        FRAME_REJECT_TARGET => return Err("isekai-helper rejected: target unreachable".to_string()),
        FRAME_REJECT_UNSUPPORTED => return Err("isekai-helper rejected: unsupported frame".to_string()),
        other => return Err(format!("isekai-helper: unexpected response byte {other:#x}")),
    }
    info!("isekai_link_relay: HELLO/ACK ok — handing off to SSH");

    let resume_state = Arc::new(std::sync::Mutex::new(ClientResumeState::new(
        DEFAULT_RESUME_BUFFER_SIZE,
    )));

    {
        let conn = conn.clone();
        let proof = proof.to_vec();
        let resume_state = resume_state.clone();
        RUNTIME.spawn(async move {
            match tokio::time::timeout(CONTROL_STREAM_TIMEOUT, open_control_stream(&conn, &proof)).await {
                Ok(Ok((csend, crecv, session_id))) => {
                    info!(
                        "isekai_link_relay: control stream established (resume support enabled), session_id={}",
                        session_id.iter().map(|b| format!("{b:02x}")).collect::<String>()
                    );
                    resume_state.lock().unwrap().session_id = Some(session_id);
                    spawn_app_ack_tasks(csend, crecv, resume_state);
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
    let reattach_fn: resume_client::ReattachFn = Arc::new(move |session_id, client_sent_offset, client_delivered_offset| {
        let cert_sha256_hex = cert_sha256_hex.clone();
        let session_secret = session_secret.clone();
        Box::pin(async move {
            let conn = helper_quic_transport::establish_quic_connection_with_socket(
                crate::faulty_udp_socket::bind_faulty_udp_socket(
                    "0.0.0.0:0".parse().unwrap(),
                    crate::debug_fault::shared_injector(),
                )
                .map_err(|e| format!("endpoint bind failed: {e}"))?,
                helper_addr,
                &cert_sha256_hex,
            )
            .await?;
            let resume_proof = compute_proof(&conn, &session_secret, &session_id)?;

            let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("open_bi (resume) failed: {e}"))?;
            let mut frame = vec![resume_client::RESUME];
            frame.extend_from_slice(&session_id);
            frame.extend_from_slice(&resume_proof);
            frame.extend_from_slice(&client_sent_offset.to_be_bytes());
            frame.extend_from_slice(&client_delivered_offset.to_be_bytes());
            send.write_all(&frame).await.map_err(|e| format!("RESUME write failed: {e}"))?;

            let mut resp = [0u8; 1];
            recv.read_exact(&mut resp).await.map_err(|e| format!("RESUME_ACK read failed: {e}"))?;
            if resp[0] != resume_client::RESUME_ACK {
                return Err(format!("isekai-helper rejected resume: {:#x}", resp[0]));
            }
            let mut rest = [0u8; 16];
            recv.read_exact(&mut rest).await.map_err(|e| format!("RESUME_ACK body read failed: {e}"))?;
            let helper_committed_offset = u64::from_be_bytes(rest[0..8].try_into().unwrap());
            info!("isekai_link_relay: resume succeeded, helper_committed_offset={helper_committed_offset}");
            Ok(resume_client::ReattachResult { send, recv, helper_committed_offset })
        })
    });

    Ok(resume_client::ReattachableStream::new(send, recv, resume_state, reattach_fn))
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
    let handler = RusshEventHandler::new(event_tx.clone());
    let agent_key = handler.agent_key.clone();
    let remote_forwards = handler.remote_forwards.clone();

    let session = match client::connect_stream(russh_config, stream, handler).await {
        Ok(s) => s,
        Err(e) => {
            event_tx.send(TransportEvent::Disconnected { reason: Some(e.to_string()) }).await.ok();
            return;
        }
    };

    // IsekaiLinkRelayConfig は agent forwarding 未対応（`HelperQuicConfig` と同様）。
    run_ssh_channel_loop(
        &config.username, &mut config.auth, config.cols, config.rows,
        false, agent_key, false, remote_forwards,
        session, cmd_rx, event_tx,
    ).await;
}
