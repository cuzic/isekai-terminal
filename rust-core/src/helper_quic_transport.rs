//! Phase 7-4: 自作ヘルパー（isekai-helper）経由の QUIC トランスポート。
//!
//! Phase 5B の tsshd/`QuicSession` と異なり、サーバー側に事前インストールされたデーモンを
//! 前提とせず、SSH 経由で isekai-helper を自動ブートストラップ（Phase 7-3）してから
//! QUIC 接続する。ワイヤー契約の詳細は `/HELPER_PROTOCOL.md` を参照。
//!
//! ビルド前提: `rust-core/scripts/build-isekai-helper-musl.sh` を先に実行し、
//! `target/{x86_64,aarch64}-unknown-linux-musl/release/isekai-helper` が存在すること
//! （`include_bytes!` でこの crate の Android ビルドに埋め込む）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use hmac::{Hmac, Mac};
use log::{info, warn};
use noq::crypto::rustls::QuicClientConfig;
use russh::client;
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use sha2::{Digest, Sha256};

use crate::helper_bootstrap::{self, BootstrapError, HelperBinaries, HelperHandshake};
use crate::resume_client::{self, ClientResumeState};
use crate::transport::{
    authenticate_session, connect_via_jump_or_direct, run_ssh_channel_loop, RusshEventHandler,
    TransportCommand, TransportEvent,
};
use crate::{init_logger, CellData, JumpConfig, SessionCallback, SshAuth, SshError, RUNTIME};
use crate::session::SessionCore;

type HmacSha256 = Hmac<Sha256>;

/// C→S input replay buffer の既定上限（helper 側 `DEFAULT_RESUME_BUFFER_SIZE` と揃える）。
const DEFAULT_RESUME_BUFFER_SIZE: usize = 4 * 1024 * 1024;
/// control stream を開く/CONTROL_ACK を待つタイムアウト（helper 側 `HELLO_TIMEOUT` と揃える）。
const CONTROL_STREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// QUIC connection が本当に死んだことを検知するまでの時間。実機検証（Phase 8-4b）で、
/// この値が未設定（quinn のデフォルト任せ）だと検知に 40 秒以上かかり、helper 側の
/// park セッション破棄（`isekai-helper::DEFAULT_PARKED_SESSION_TTL`）より遅くなって
/// reattach が必ず `REJECT_UNKNOWN_SESSION` になる、という致命的なタイミング不整合が
/// 見つかった。検知を速くしつつ、NAT の UDP マッピング（通常 30 秒）が切れて偽陽性の
/// タイムアウトが起きないよう `CLIENT_KEEP_ALIVE_INTERVAL` で PING を送り続ける。
const CLIENT_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// NAT マッピング維持のための PING 間隔。`CLIENT_MAX_IDLE_TIMEOUT` の 1/3 以下にして、
/// 数回分の PING ロスを許容できるようにする。
const CLIENT_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(5);

// Phase 9-2 (multipath_transport.rs) もこのワイヤー契約・埋め込みバイナリを共有する
// ため pub(crate) にしてある（HELPER_PROTOCOL.md の契約は接続経路が単一パスか
// multipath かによらず同一）。
pub(crate) const EXPORTER_LABEL: &[u8] = b"isekai-helper-auth-v1";
pub(crate) const ALPN: &[u8] = b"isekai-helper/1";
pub(crate) const FRAME_HELLO: u8 = 0x01;
pub(crate) const FRAME_ACK: u8 = 0x02;
pub(crate) const FRAME_REJECT_TARGET: u8 = 0xFC;
pub(crate) const FRAME_REJECT_UNSUPPORTED: u8 = 0xFD;
pub(crate) const FRAME_REJECT_DUPLICATE: u8 = 0xFE;
pub(crate) const FRAME_REJECT_AUTH: u8 = 0xFF;

/// isekai-helper/Cargo.toml の version と一致させる。バージョン不一致時は
/// helper_bootstrap::ensure_helper_running が再配布する。
pub(crate) const HELPER_VERSION: &str = "0.3.2";

pub(crate) const HELPER_BIN_X86_64: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/target/x86_64-unknown-linux-musl/release/isekai-helper"
));
pub(crate) const HELPER_BIN_AARCH64: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/target/aarch64-unknown-linux-musl/release/isekai-helper"
));

// ── 公開型 ──────────────────────────────────────────────

#[derive(Debug, Clone, uniffi::Record)]
pub struct HelperQuicConfig {
    pub ssh_host: String,
    pub ssh_port: u16,
    pub username: String,
    pub auth: SshAuth,
    pub cols: u32,
    pub rows: u32,
    /// ブートストラップ用SSH接続の踏み台(ProxyJump)。`SshConfig::jump`参照。
    pub jump: Option<JumpConfig>,
}

#[derive(uniffi::Object)]
pub struct HelperQuicSession {
    config: HelperQuicConfig,
    core: SessionCore,
}

#[uniffi::export]
pub fn create_helper_quic_session(config: HelperQuicConfig) -> Arc<HelperQuicSession> {
    init_logger();
    Arc::new(HelperQuicSession { config, core: SessionCore::new() })
}

#[uniffi::export]
impl HelperQuicSession {
    /// 明示的にヘルパー経由 QUIC のみを試す（フォールバック無し）。
    pub fn connect(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
        let config = self.config.clone();
        let (cmd_rx, event_tx) = self.core.start(config.cols, config.rows, callback);
        RUNTIME.spawn(async move {
            match try_connect_helper_quic(&config).await {
                Ok(stream) => run_over_stream(config, stream, cmd_rx, event_tx).await,
                Err(e) => {
                    warn!("helper_quic: connect failed: {e}");
                    event_tx.send(TransportEvent::Disconnected { reason: Some(e) }).await.ok();
                }
            }
        });
        Ok(())
    }

    /// `TransportPreference::Auto` 相当: ヘルパー経由 QUIC を試し、失敗したら
    /// 通常の TCP SSH（Phase 1-4）にフォールバックする。
    pub fn connect_auto(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
        let config = self.config.clone();
        let (cmd_rx, event_tx) = self.core.start(config.cols, config.rows, callback);
        RUNTIME.spawn(async move {
            run_helper_quic_transport_auto(config, cmd_rx, event_tx).await;
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

// ── 証明書ピン留め ───────────────────────────────────────
// handshake JSON の cert_sha256（SSH チャネル経由で受け渡し済み、TOFU より強い信頼の起点）
// とだけ照合する。通常の CA チェーン検証は行わない（自己署名 ephemeral cert のため）。

#[derive(Debug)]
pub(crate) struct PinnedCertVerifier {
    pub(crate) expected_sha256_hex: String,
    pub(crate) provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let mut hasher = Sha256::new();
        hasher.update(end_entity.as_ref());
        let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got == self.expected_sha256_hex {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "isekai-helper cert pin mismatch: expected {} got {}",
                self.expected_sha256_hex, got
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message, cert, dss, &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message, cert, dss, &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

// ── ブートストラップ（Phase 7-3 呼び出し） ───────────────

async fn bootstrap_via_ssh(config: &HelperQuicConfig) -> Result<HelperHandshake, String> {
    bootstrap_helper_via_ssh(
        &config.ssh_host, config.ssh_port, &config.username, &config.auth, &config.jump, None,
    ).await
}

/// SSH でログインし、isekai-helper をブートストラップして起動ハンドシェイクを得る。
/// `HelperQuicConfig`（Phase 7/8、フォールバック無し/Auto）と `MultipathHelperQuicConfig`
/// （Phase 9-2、パス冗長化）は候補アドレスの持ち方が異なるだけで、SSH ブートストラップ自体は
/// 共通なのでここに切り出してある。
///
/// `jump`: 設定されていれば `ssh_host:ssh_port` へ直接ではなく踏み台経由で接続する
/// （`SshConfig::jump`・`transport::connect_via_jump_or_direct` 参照）。対象ホストが
/// NAT配下で直接到達できない場合、初回のisekai-helper配布・起動にはこれが唯一の経路になる。
///
/// `bind_port`: `None` なら isekai-helper は既定通りエフェメラルポートで待ち受ける
/// （Tailscale経由のpath0はこれで十分、`ts-input`チェーンが素通しなため）。
/// `Some(port)` を渡すと、direct_host（外部到達アドレス）向けにファイアウォールで
/// 個別に許可した固定ポートで待ち受けさせる（Phase 9-4、`multipath_transport.rs`から使用）。
pub(crate) async fn bootstrap_helper_via_ssh(
    ssh_host: &str,
    ssh_port: u16,
    username: &str,
    auth: &SshAuth,
    jump: &Option<JumpConfig>,
    bind_port: Option<u16>,
) -> Result<HelperHandshake, String> {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);
    // NOTE: このブートストラップ用 SSH 接続(踏み台込み)のホスト鍵は、本セッションと
    // 同じサーバー・同じ known_hosts エントリを検証すべきだが、Phase 7-4 のスコープでは
    // 簡略化し常に承認する。TODO: 既存の KnownHost チェック（HostKeyChecker）と統合する。
    tokio::spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            if let TransportEvent::HostKey(_, reply) = ev {
                let _ = reply.send(true);
            }
        }
    });

    let russh_config = Arc::new(client::Config::default());
    let mut established = connect_via_jump_or_direct(jump, russh_config, ssh_host, ssh_port, event_tx)
        .await
        .map_err(|e| format!("bootstrap SSH connect failed: {e}"))?;

    let (authenticated, _) = authenticate_session(&mut established.handle, username, auth).await;
    if !authenticated {
        return Err("bootstrap SSH authentication failed".to_string());
    }

    let binaries = HelperBinaries { x86_64: HELPER_BIN_X86_64, aarch64: HELPER_BIN_AARCH64 };
    helper_bootstrap::ensure_helper_running(
        &mut established.handle, &binaries, HELPER_VERSION, "127.0.0.1:22", bind_port,
    )
        .await
        .map_err(|e: BootstrapError| format!("bootstrap failed: {e}"))
}

// ── QUIC 接続（HELLO/ACK ハンドシェイク） ───────────────

/// `cert_sha256_hex` にピン留めした QUIC connection を確立する。初回接続・
/// reattach（`RESUME`）のどちらからも呼ばれる共通処理。
async fn establish_quic_connection(remote: SocketAddr, cert_sha256_hex: &str) -> Result<noq::Connection, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| "TLS config failed".to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
            expected_sha256_hex: cert_sha256_hex.to_string(),
            provider,
        }))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    // 0-RTT はクライアント側でも使わない（HELPER_PROTOCOL.md「0-RTT はクライアント・
    // サーバー双方で完全に無効化する」契約）。noq::Connecting::into_0rtt() を呼ばず
    // 通常のハンドシェイク完了を待つのがクライアント側の対応。

    let quic_crypto = QuicClientConfig::try_from(crypto).map_err(|_| "QUIC crypto config failed".to_string())?;

    let mut transport = noq::TransportConfig::default();
    transport.max_concurrent_bidi_streams(noq::VarInt::from_u32(1));
    transport.max_concurrent_uni_streams(noq::VarInt::from_u32(0));
    transport.max_idle_timeout(Some(
        noq::IdleTimeout::try_from(CLIENT_MAX_IDLE_TIMEOUT).expect("valid idle timeout"),
    ));
    transport.keep_alive_interval(Some(CLIENT_KEEP_ALIVE_INTERVAL));

    let mut client_config = noq::ClientConfig::new(Arc::new(quic_crypto));
    client_config.transport_config(Arc::new(transport));

    // 実機検証 (Phase 7-5) 用: `debug_fault` 経由で有効化されない限り
    // `FaultyUdpSocket` は素通しで、通常利用時の挙動には影響しない。
    let socket = crate::faulty_udp_socket::bind_faulty_udp_socket(
        "0.0.0.0:0".parse().unwrap(),
        crate::debug_fault::shared_injector(),
    )
    .map_err(|e| format!("endpoint bind failed: {e}"))?;
    let endpoint = noq::Endpoint::new_with_abstract_socket(
        noq::EndpointConfig::default(),
        None,
        Box::new(socket),
        Arc::new(noq::TokioRuntime),
    )
    .map_err(|e| format!("endpoint bind failed: {e}"))?;
    endpoint.set_default_client_config(client_config);

    info!("helper_quic: connecting to {remote}");
    let conn = endpoint
        .connect(remote, "isekai-helper.local")
        .map_err(|e| format!("connect setup failed: {e}"))?
        .await
        .map_err(|e| format!("QUIC handshake failed: {e}"))?;
    info!("helper_quic: QUIC handshake ok rtt={:?}", conn.rtt(noq::PathId::ZERO));
    Ok(conn)
}

/// `session_secret` と QUIC connection の exporter から proof を計算する
/// （HELLO と RESUME で共通のロジック。RESUME は `extra` に session_id を渡す）。
fn compute_proof(conn: &noq::Connection, session_secret: &[u8], extra: &[u8]) -> Result<[u8; 32], String> {
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"")
        .map_err(|e| format!("export_keying_material failed: {e:?}"))?;
    let mut mac = HmacSha256::new_from_slice(session_secret).expect("HMAC accepts any key length");
    mac.update(&exporter);
    if !extra.is_empty() {
        mac.update(extra);
    }
    Ok(mac.finalize().into_bytes().into())
}

async fn connect_helper_quic_stream(
    ssh_host: &str,
    handshake: &HelperHandshake,
) -> Result<resume_client::ReattachableStream, String> {
    let remote: SocketAddr = tokio::net::lookup_host((ssh_host, handshake.listen_port))
        .await
        .map_err(|e| format!("DNS lookup failed: {e}"))?
        .next()
        .ok_or_else(|| "no address resolved for isekai-helper host".to_string())?;
    let cert_sha256_hex = handshake.cert_sha256.clone();
    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&handshake.session_secret)
        .map_err(|e| format!("invalid session_secret encoding: {e}"))?;

    let conn = establish_quic_connection(remote, &cert_sha256_hex).await?;

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
    info!("helper_quic: HELLO/ACK ok — handing off to SSH");

    let resume_state = Arc::new(std::sync::Mutex::new(ClientResumeState::new(
        DEFAULT_RESUME_BUFFER_SIZE,
    )));

    // control stream の確立を待たずに即座に SSH セッションへ渡す。
    // noq の open_bi() は相手の stream credit が尽きていると（旧 helper 等）
    // 即座にエラーを返さず MAX_STREAMS を待って内部でブロックし得るため、
    // ここで await してしまうと旧 helper 相手に毎回 CONTROL_STREAM_TIMEOUT 分
    // シェル開始が遅延する（isekai-helper 側の e2e テストで実際に踏んだ
    // リグレッションと同種の問題）。control stream の確立は背後タスクとして
    // 並行に進める。
    {
        let conn = conn.clone();
        let proof = proof.to_vec();
        let resume_state = resume_state.clone();
        RUNTIME.spawn(async move {
            match tokio::time::timeout(CONTROL_STREAM_TIMEOUT, open_control_stream(&conn, &proof)).await {
                Ok(Ok((csend, crecv, session_id))) => {
                    info!(
                        "helper_quic: control stream established (resume support enabled), session_id={}",
                        session_id.iter().map(|b| format!("{b:02x}")).collect::<String>()
                    );
                    resume_state.lock().unwrap().session_id = Some(session_id);
                    spawn_app_ack_tasks(csend, crecv, resume_state);
                }
                Ok(Err(e)) => {
                    info!("helper_quic: control stream handshake failed ({e}), continuing without resume support");
                }
                Err(_) => {
                    info!("helper_quic: control stream not accepted within timeout, continuing without resume support");
                }
            }
        });
    }

    // Phase 8-3: QUIC connection が失われても RESUME で reattach する
    // クロージャ。`remote`/`cert_sha256_hex`/`session_secret` を捕捉する。
    let reattach_fn: resume_client::ReattachFn = Arc::new(move |session_id, client_sent_offset, client_delivered_offset| {
        let cert_sha256_hex = cert_sha256_hex.clone();
        let session_secret = session_secret.clone();
        Box::pin(async move {
            let conn = establish_quic_connection(remote, &cert_sha256_hex).await?;
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
            info!("helper_quic: resume succeeded, helper_committed_offset={helper_committed_offset}");
            Ok(resume_client::ReattachResult { send, recv, helper_committed_offset })
        })
    });

    Ok(resume_client::ReattachableStream::new(send, recv, resume_state, reattach_fn))
}

/// control stream を開き、`CONTROL_HELLO` を送って `CONTROL_ACK` を待つ。
/// data stream の HELLO と同じ `proof` を再利用する（同一 QUIC connection の
/// exporter から計算されるため同じ値になる、HELPER_PROTOCOL.md §7.3）。
async fn open_control_stream(
    conn: &noq::Connection,
    proof: &[u8],
) -> Result<(noq::SendStream, noq::RecvStream, resume_client::SessionId), String> {
    let (mut csend, mut crecv) = conn.open_bi().await.map_err(|e| format!("open_bi (control) failed: {e}"))?;
    let mut hello = Vec::with_capacity(33);
    hello.push(resume_client::CONTROL_HELLO);
    hello.extend_from_slice(proof);
    csend
        .write_all(&hello)
        .await
        .map_err(|e| format!("CONTROL_HELLO write failed: {e}"))?;

    let mut ack = [0u8; 17];
    crecv
        .read_exact(&mut ack)
        .await
        .map_err(|e| format!("CONTROL_ACK read failed: {e}"))?;
    if ack[0] != resume_client::CONTROL_ACK {
        return Err(format!("unexpected control response byte {:#x}", ack[0]));
    }
    let mut session_id = [0u8; 16];
    session_id.copy_from_slice(&ack[1..17]);
    Ok((csend, crecv, session_id))
}

/// APP_ACK の送受信を行う背後タスクを spawn する。data stream が閉じた後も
/// これらのタスクは自然に終了しない（control stream 側の read/write が
/// エラーになった時点でループを抜ける、ベストエフォート設計）。
fn spawn_app_ack_tasks(
    mut csend: noq::SendStream,
    mut crecv: noq::RecvStream,
    state: Arc<std::sync::Mutex<ClientResumeState>>,
) {
    // APP_ACK 受信: helper からの helper_committed_offset を受け取り、
    // input replay buffer の破棄範囲を進める。
    {
        let state = state.clone();
        RUNTIME.spawn(async move {
            loop {
                let mut frame = [0u8; 9];
                match crecv.read_exact(&mut frame).await {
                    Ok(()) if frame[0] == resume_client::APP_ACK => {
                        let offset = u64::from_be_bytes(frame[1..9].try_into().unwrap());
                        state.lock().unwrap().replay_buffer.advance_start(offset);
                    }
                    _ => break,
                }
            }
        });
    }

    // APP_ACK 送信: client_delivered_offset（S→C の受信確認）を 200ms ごとに送る。
    RUNTIME.spawn(async move {
        let mut last_sent = 0u64;
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let current = state.lock().unwrap().client_delivered_offset;
            if current == last_sent {
                continue;
            }
            let mut frame = Vec::with_capacity(9);
            frame.push(resume_client::APP_ACK);
            frame.extend_from_slice(&current.to_be_bytes());
            if csend.write_all(&frame).await.is_err() {
                break;
            }
            last_sent = current;
        }
    });
}

async fn try_connect_helper_quic(
    config: &HelperQuicConfig,
) -> Result<resume_client::ReattachableStream, String> {
    let handshake = bootstrap_via_ssh(config).await?;
    connect_helper_quic_stream(&config.ssh_host, &handshake).await
}

async fn run_over_stream(
    config: HelperQuicConfig,
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

    let session = match client::connect_stream(russh_config, stream, handler).await {
        Ok(s) => s,
        Err(e) => {
            event_tx.send(TransportEvent::Disconnected { reason: Some(e.to_string()) }).await.ok();
            return;
        }
    };

    // HelperQuicConfig は agent forwarding 未対応（プロファイルの `SshConfig.agent_forward`
    // 相当のフィールドをまだ持たない）。
    run_ssh_channel_loop(
        &config.username, &config.auth, config.cols, config.rows,
        false, agent_key,
        session, cmd_rx, event_tx,
    ).await;
}

/// `TransportPreference::Auto`: ヘルパー経由 QUIC を試し、ブートストラップ/QUIC 接続の
/// 時点で失敗したら通常の TCP SSH にフォールバックする。一度 russh セッションが確立した
/// 後の切断はフォールバック対象にしない（正常な切断イベントとして扱う）。
async fn run_helper_quic_transport_auto(
    config: HelperQuicConfig,
    cmd_rx: tokio::sync::mpsc::Receiver<TransportCommand>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    match try_connect_helper_quic(&config).await {
        Ok(stream) => run_over_stream(config, stream, cmd_rx, event_tx).await,
        Err(e) => {
            warn!("helper_quic auto: falling back to plain SSH after: {e}");
            let ssh_config = crate::SshConfig {
                host: config.ssh_host,
                port: config.ssh_port,
                username: config.username,
                auth: config.auth,
                cols: config.cols,
                rows: config.rows,
                // HelperQuicConfig にはポートフォワード設定・agent forwarding 設定が無いため、
                // フォールバック時はどちらも無効なプレーン SSH として接続する。
                forwards: Vec::new(),
                agent_forward: false,
                // HelperQuicConfig には踏み台(jump host)設定も無いため、フォールバック時は
                // 対象ホストへ直接SSH接続する前提のまま(SshConfig::jump 参照)。
                jump: None,
            };
            crate::run_russh_transport(ssh_config, cmd_rx, event_tx).await;
        }
    }
}

#[cfg(test)]
mod tests {
    //! `HelperQuicSession` を orchestrator レベルではなく直接使い、SSH ブートストラップ →
    //! isekai-helper QUIC → russh チャネル → 実シェルコマンド実行までを通しで検証する。
    //! 実 sshd（127.0.0.1:22）+ `HELPER_BOOTSTRAP_TEST_KEY` が必要な opt-in テスト。
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Notify;

    struct TestCallback {
        buf: Arc<StdMutex<Vec<u8>>>,
        notify: Arc<Notify>,
    }

    impl SessionCallback for TestCallback {
        fn on_data(&self, data: Vec<u8>) {
            self.buf.lock().unwrap().extend_from_slice(&data);
            self.notify.notify_one();
        }
        fn on_host_key(&self, _fingerprint: String) -> bool { true }
        fn on_connected(&self) {}
        fn on_disconnected(&self, reason: Option<String>) {
            eprintln!("test: disconnected: {reason:?}");
        }
        fn on_screen_update(&self, _update: crate::ScreenUpdate) {}
        fn on_trzsz_request(&self, _t: String, _m: String, _n: Option<String>, _s: Option<u64>) {}
        fn on_trzsz_download_chunk(&self, _t: String, _d: Vec<u8>, _l: bool) {}
        fn on_trzsz_progress(&self, _t: String, _tr: u64, _to: Option<u64>) {}
        fn on_trzsz_finished(&self, _t: String, _s: bool, _m: Option<String>) {}
        fn on_no_viable_path(&self) {}
        fn on_forward_state_changed(&self, _id: String, _state: crate::ForwardState) {}
        fn on_agent_sign_request(&self, _key_fingerprint: String) -> bool { true }
    }

    #[tokio::test]
    async fn full_stack_bootstrap_quic_and_shell_command() {
        let Ok(key_path) = std::env::var("HELPER_BOOTSTRAP_TEST_KEY") else {
            eprintln!("skipping: HELPER_BOOTSTRAP_TEST_KEY not set");
            return;
        };
        let key_pem = std::fs::read_to_string(&key_path).unwrap();
        // 秘密鍵の実体はテストなので PEM のまま渡す（本番の SshAuth::PublicKey と同じ形）。
        let config = HelperQuicConfig {
            ssh_host: "127.0.0.1".to_string(),
            ssh_port: 22,
            username: std::env::var("USER").unwrap_or_else(|_| "root".to_string()),
            auth: SshAuth::PublicKey { private_key_pem: key_pem.into_bytes() },
            cols: 80,
            rows: 24,
            jump: None,
        };

        let session = create_helper_quic_session(config);
        let buf = Arc::new(StdMutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let callback = TestCallback { buf: buf.clone(), notify: notify.clone() };
        session.connect(Box::new(callback)).expect("connect() call failed");

        // シェルプロンプトが出るまで少し待ってからコマンドを送る。
        tokio::time::sleep(Duration::from_millis(800)).await;
        session.send(b"echo full-stack-ok\n".to_vec());

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            {
                let b = buf.lock().unwrap();
                if String::from_utf8_lossy(&b).contains("full-stack-ok") {
                    break;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "timed out waiting for echo output; got so far: {:?}",
                    String::from_utf8_lossy(&buf.lock().unwrap())
                );
            }
            tokio::time::timeout(Duration::from_millis(200), notify.notified())
                .await
                .ok();
        }

        session.disconnect();
    }

    /// Phase 8-3: 実際に QUIC connection を切断してから復旧させ、
    /// `ReattachableStream` が russh にエラーを見せずに同じ SSH セッションを
    /// 継続させることを検証する。`debug_fault::shared_injector()` はプロセス
    /// グローバルな状態なので、**このテストは他のテストと同時実行しないこと**
    /// （`cargo test --lib helper_quic_transport::tests::resume_survives_connection_cut`
    /// のように単独実行する）。
    #[tokio::test]
    async fn resume_survives_connection_cut() {
        let Ok(key_path) = std::env::var("HELPER_BOOTSTRAP_TEST_KEY") else {
            eprintln!("skipping: HELPER_BOOTSTRAP_TEST_KEY not set");
            return;
        };
        let key_pem = std::fs::read_to_string(&key_path).unwrap();
        let config = HelperQuicConfig {
            ssh_host: "127.0.0.1".to_string(),
            ssh_port: 22,
            username: std::env::var("USER").unwrap_or_else(|_| "root".to_string()),
            auth: SshAuth::PublicKey { private_key_pem: key_pem.into_bytes() },
            cols: 80,
            rows: 24,
            jump: None,
        };

        crate::debug_fault::shared_injector().restore();
        crate::debug_fault::shared_injector().set_latency(Duration::ZERO);
        crate::debug_fault::shared_injector().set_loss_rate(0.0);

        let session = create_helper_quic_session(config);
        let buf = Arc::new(StdMutex::new(Vec::new()));
        let notify = Arc::new(Notify::new());
        let callback = TestCallback { buf: buf.clone(), notify: notify.clone() };
        session.connect(Box::new(callback)).expect("connect() call failed");

        tokio::time::sleep(Duration::from_millis(800)).await;
        session.send(b"echo before-cut\n".to_vec());
        wait_for_output(&buf, &notify, "before-cut", Duration::from_secs(10)).await;

        // control stream が確立し session_id が発行されるまで少し待ってから切断する
        // （control stream 未確立のまま切ると resume できず Failed になるのは仕様通り）。
        tokio::time::sleep(Duration::from_millis(500)).await;

        eprintln!("test: cutting connection");
        crate::debug_fault::shared_injector().cut();
        // ReattachableStream が失敗を検知し、reattach タスクが 1 回目の
        // リトライを試みる程度の時間だけ切断したままにする。
        tokio::time::sleep(Duration::from_millis(1500)).await;
        eprintln!("test: restoring connection");
        crate::debug_fault::shared_injector().restore();

        // reattach が完了して同じセッションで応答が返ってくることを確認する。
        session.send(b"echo after-resume\n".to_vec());
        wait_for_output(&buf, &notify, "after-resume", Duration::from_secs(20)).await;

        session.disconnect();
        crate::debug_fault::shared_injector().restore();
    }

    async fn wait_for_output(
        buf: &Arc<StdMutex<Vec<u8>>>,
        notify: &Arc<Notify>,
        needle: &str,
        timeout: Duration,
    ) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let b = buf.lock().unwrap();
                if String::from_utf8_lossy(&b).contains(needle) {
                    return;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {needle:?}; got so far: {:?}",
                    String::from_utf8_lossy(&buf.lock().unwrap())
                );
            }
            tokio::time::timeout(Duration::from_millis(200), notify.notified())
                .await
                .ok();
        }
    }
}
