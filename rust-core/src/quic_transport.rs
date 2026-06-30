//! Phase 5B: QUIC transport client.
//!
//! `QuicSession` connects to a `tsshd` QUIC proxy, which in turn opens a TCP
//! connection to the real SSH server and bidirectionally forwards bytes. From
//! russh's point of view the QUIC bi-stream is just an `AsyncRead + AsyncWrite`
//! transport, so we reuse the same `session_event_loop` (VTE parser + trzsz FSM)
//! as the plain-TCP `SshSession`.

use std::sync::Arc;

use log::{debug, info, warn};
use russh::client;
use tokio::io::AsyncReadExt;

use crate::{
    init_logger, CellData, SessionCallback, SshAuth, SshError, RUNTIME,
};
use crate::session::SessionCore;
use crate::transport::{RusshEventHandler, TransportCommand, TransportEvent, run_ssh_channel_loop};

// ── 公開型 ──────────────────────────────────────────────

#[derive(Debug, Clone, uniffi::Record)]
pub struct QuicConfig {
    /// tsshd の QUIC エンドポイント (e.g. "100.100.45.36")
    pub tsshd_host: String,
    pub tsshd_port: u16,
    /// tsshd がこのアドレスで SSH サーバーに接続する
    pub ssh_host: String,
    pub ssh_port: u16,
    pub username: String,
    pub auth: SshAuth,
    pub cols: u32,
    pub rows: u32,
    /// スパイク用: TLS 証明書検証をスキップ
    pub skip_cert_verify: bool,
}

#[derive(uniffi::Object)]
pub struct QuicSession {
    config: QuicConfig,
    core: SessionCore,
}

#[uniffi::export]
pub fn create_quic_session(config: QuicConfig) -> Arc<QuicSession> {
    init_logger();
    Arc::new(QuicSession { config, core: SessionCore::new() })
}

#[uniffi::export]
impl QuicSession {
    pub fn connect(&self, callback: Box<dyn SessionCallback>) -> Result<(), SshError> {
        let config = self.config.clone();
        let (cmd_rx, event_tx) = self.core.start(config.cols, config.rows, callback);
        RUNTIME.spawn(async move {
            run_quic_transport(config, cmd_rx, event_tx).await;
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

// ── 証明書検証スキップ (スパイク用) ──────────────────────

#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message, cert, dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message, cert, dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn build_client_config(skip_cert_verify: bool) -> Result<quinn::ClientConfig, SshError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let mut crypto = if skip_cert_verify {
        rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|_| SshError::ConnectionFailed)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification(provider)))
            .with_no_client_auth()
    } else {
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|_| SshError::ConnectionFailed)?
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth()
    };
    crypto.alpn_protocols = vec![b"tsshd".to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .map_err(|_| SshError::ConnectionFailed)?;

    let mut transport = quinn::TransportConfig::default();
    // NAT の UDP マッピング (通常 30 秒) を維持するため 20 秒ごとに QUIC PING を送る
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(20)));
    // アイドルタイムアウトを 5 分に延長
    transport.max_idle_timeout(Some(
        std::time::Duration::from_secs(300).try_into().unwrap()
    ));

    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_crypto));
    client_config.transport_config(Arc::new(transport));
    Ok(client_config)
}

/// tsshd への QUIC bi-stream を確立し、`OK\n` ハンドシェイク完了後に
/// `recv`/`send` を結合した stream を返す。
async fn open_proxy_stream(
    config: &QuicConfig,
) -> Result<tokio::io::Join<quinn::RecvStream, quinn::SendStream>, String> {
    info!("quic: DNS lookup {}:{}", config.tsshd_host, config.tsshd_port);
    let remote = tokio::net::lookup_host((config.tsshd_host.as_str(), config.tsshd_port))
        .await
        .map_err(|e| format!("DNS lookup failed: {e}"))?
        .next()
        .ok_or_else(|| "no address resolved for tsshd host".to_string())?;
    info!("quic: resolved {}", remote);

    let bind_addr: std::net::SocketAddr = if remote.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };
    debug!("quic: binding UDP {}", bind_addr);

    let mut endpoint =
        quinn::Endpoint::client(bind_addr).map_err(|e| format!("endpoint bind failed: {e}"))?;
    endpoint.set_default_client_config(
        build_client_config(config.skip_cert_verify).map_err(|_| "TLS config failed".to_string())?,
    );

    info!("quic: connecting to {} (skip_cert_verify={})", remote, config.skip_cert_verify);
    let conn = endpoint
        .connect(remote, &config.tsshd_host)
        .map_err(|e| format!("connect setup failed: {e}"))?
        .await
        .map_err(|e| {
            warn!("quic: QUIC handshake failed: {}", e);
            format!("QUIC handshake failed: {e}")
        })?;
    info!("quic: QUIC handshake ok rtt={:?}", conn.rtt());

    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| format!("open_bi failed: {e}"))?;
    debug!("quic: bi-stream opened");

    let handshake = format!(
        "{{\"ssh_host\":\"{}\",\"ssh_port\":{},\"cols\":{},\"rows\":{}}}\n",
        config.ssh_host, config.ssh_port, config.cols, config.rows
    );
    info!("quic: sending handshake → ssh://{}:{}", config.ssh_host, config.ssh_port);
    send.write_all(handshake.as_bytes())
        .await
        .map_err(|e| format!("handshake write failed: {e}"))?;

    // tsshd が "OK\n" を返すまで 1 バイトずつ読む。'\n' の手前で止めることで
    // SSH バナーを読み飛ばさず russh に引き継げる。
    let mut line = Vec::with_capacity(8);
    loop {
        let b = recv
            .read_u8()
            .await
            .map_err(|e| format!("handshake read failed: {e}"))?;
        if b == b'\n' {
            break;
        }
        line.push(b);
        if line.len() > 4096 {
            return Err("handshake response too long".to_string());
        }
    }
    if line != b"OK" {
        let resp = String::from_utf8_lossy(&line).to_string();
        warn!("quic: tsshd rejected handshake: {}", resp);
        return Err(format!("tsshd rejected handshake: {resp}"));
    }
    info!("quic: tsshd handshake ok — handing off to SSH");

    // recv/send は内部で ConnectionRef を保持しており、stream が生きている間は
    // 接続も生存する。接続が生きていれば endpoint の driver task も終了しないため、
    // ここで endpoint/conn ハンドルをドロップしても安全。
    Ok(tokio::io::join(recv, send))
}

async fn run_quic_transport(
    config: QuicConfig,
    cmd_rx: tokio::sync::mpsc::Receiver<TransportCommand>,
    event_tx: tokio::sync::mpsc::Sender<TransportEvent>,
) {
    let stream = match open_proxy_stream(&config).await {
        Ok(s) => { info!("quic: proxy stream ready"); s }
        Err(e) => {
            warn!("quic: open_proxy_stream failed: {}", e);
            event_tx.send(TransportEvent::Disconnected { reason: Some(e) }).await.ok();
            return;
        }
    };

    let russh_config = Arc::new(client::Config {
        keepalive_interval: Some(std::time::Duration::from_secs(60)),
        keepalive_max: 3,
        ..client::Config::default()
    });
    let handler = RusshEventHandler { event_tx: event_tx.clone() };

    let session = match client::connect_stream(russh_config, stream, handler).await {
        Ok(s) => s,
        Err(e) => {
            event_tx.send(TransportEvent::Disconnected { reason: Some(e.to_string()) }).await.ok();
            return;
        }
    };

    run_ssh_channel_loop(
        &config.username, &config.auth, config.cols, config.rows,
        session, cmd_rx, event_tx,
    ).await;
}
