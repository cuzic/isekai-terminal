//! `SystemQuicEndpointFactory`: the CLI's concrete `QuicEndpointFactory`,
//! built directly on `noq` + `rustls` + a plain `tokio::net::UdpSocket`
//! (`ISEKAI_SSH_DESIGN.md` "実装方針": "中身はnoqとrustlsを直接使い、
//! tokio::net::UdpSocketをbindしてnoq::Endpointのクライアントとして使う").
//!
//! Deliberately must never reference `FaultyUdpSocket`, UniFFI, or any other
//! Android/`tssh-core`-specific type — this crate is also linked into
//! `isekai-ssh`, a plain CLI binary with no Android runtime.
//!
//! The certificate-pinning logic (`PinnedCertVerifier`) and QUIC transport
//! tuning (idle timeout / keepalive interval) are copied verbatim from
//! `helper_quic_transport.rs::establish_quic_connection_with_socket` and its
//! `PinnedCertVerifier`, minus the `FaultyUdpSocket` parameter — this crate
//! binds a real `tokio::net::UdpSocket` instead.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use isekai_protocol::hello::ALPN;
use log::info;
use noq::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use sha2::{Digest, Sha256};

use crate::error::TransportError;
use crate::traits::{ByteStream, ByteStreamReadHalf, ByteStreamWriteHalf, QuicConnection, QuicEndpoint, QuicEndpointFactory};
use crate::types::{BindSpec, RemoteSpec};

/// QUIC connection is declared dead after this much silence. Matches
/// `helper_quic_transport.rs::CLIENT_MAX_IDLE_TIMEOUT` — see that file's
/// comment on the Phase 8-4b timing bug this specific value avoids (must be
/// short enough that a dead connection is detected before isekai-helper's
/// parked-session TTL expires).
const CLIENT_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// PING interval to keep NAT UDP mappings alive. Matches
/// `helper_quic_transport.rs::CLIENT_KEEP_ALIVE_INTERVAL` (kept at 1/3 of the
/// idle timeout so a handful of lost PINGs can be tolerated).
const CLIENT_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// Verifies the server's leaf certificate against a pinned SHA-256
/// fingerprint instead of a CA chain — copied from
/// `helper_quic_transport.rs::PinnedCertVerifier`. isekai-helper presents an
/// ephemeral self-signed cert, so ordinary CA validation cannot apply; the
/// fingerprint itself is the trust root, delivered out-of-band over the
/// bootstrap SSH channel (`HandshakeJson::cert_sha256`).
#[derive(Debug)]
struct PinnedCertVerifier {
    expected_sha256_hex: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
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

fn client_config_for(cert_sha256_hex: &str) -> Result<noq::ClientConfig, TransportError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| TransportError::TlsConfig(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
            expected_sha256_hex: cert_sha256_hex.to_string(),
            provider,
        }))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    // 0-RTT is never used client-side either (`HELPER_PROTOCOL.md`: "0-RTT は
    // クライアント・サーバー双方で完全に無効化する"). Not calling
    // `Connecting::into_0rtt()` anywhere in this module is what implements
    // that half of the contract.

    let quic_crypto = QuicClientConfig::try_from(crypto)
        .map_err(|_| TransportError::TlsConfig("QUIC crypto config failed".to_string()))?;

    let mut transport = noq::TransportConfig::default();
    transport.max_concurrent_bidi_streams(noq::VarInt::from_u32(1));
    transport.max_concurrent_uni_streams(noq::VarInt::from_u32(0));
    transport.max_idle_timeout(Some(
        noq::IdleTimeout::try_from(CLIENT_MAX_IDLE_TIMEOUT).expect("valid idle timeout"),
    ));
    transport.keep_alive_interval(Some(CLIENT_KEEP_ALIVE_INTERVAL));

    let mut client_config = noq::ClientConfig::new(Arc::new(quic_crypto));
    client_config.transport_config(Arc::new(transport));
    Ok(client_config)
}

/// Wraps an already-bound `std::net::UdpSocket` as a `noq`-backed
/// `QuicEndpoint`. Shared by `SystemQuicEndpointFactory::create_endpoint`
/// (which binds a fresh socket) and `stun_p2p::connect_stun_p2p` (which
/// reuses a socket that already performed a STUN query and sent hole-punch
/// probes on it — `isekai_stun_p2p_transport.rs`'s comment on why the STUN/
/// probe step must happen *before* handing the socket to `noq::Endpoint`, to
/// avoid a race between noq's internal `poll_recv` and raw reads on the same
/// socket) — this is the "既存の生ソケットをQUICエンドポイントにラップする"
/// logic `ISEKAI_SSH_DESIGN.md` calls out as needing exactly one
/// implementation shared by both call sites.
pub(crate) fn quic_endpoint_from_std_socket(
    std_socket: std::net::UdpSocket,
) -> Result<Box<dyn QuicEndpoint>, TransportError> {
    let endpoint = noq::Endpoint::new(
        noq::EndpointConfig::default(),
        None,
        std_socket,
        Arc::new(noq::TokioRuntime),
    )
    .map_err(|e| TransportError::EndpointSetup(e.to_string()))?;

    Ok(Box::new(SystemQuicEndpoint { endpoint }))
}

/// The CLI's concrete `QuicEndpointFactory`. Stateless — every
/// `create_endpoint` call binds a fresh UDP socket.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemQuicEndpointFactory;

#[async_trait]
impl QuicEndpointFactory for SystemQuicEndpointFactory {
    async fn create_endpoint(&self, bind: BindSpec) -> Result<Box<dyn QuicEndpoint>, TransportError> {
        let socket = tokio::net::UdpSocket::bind(bind.local_addr)
            .await
            .map_err(|source| TransportError::Bind { addr: bind.local_addr, source })?;
        let std_socket = socket
            .into_std()
            .map_err(|source| TransportError::Bind { addr: bind.local_addr, source })?;

        quic_endpoint_from_std_socket(std_socket)
    }
}

struct SystemQuicEndpoint {
    endpoint: noq::Endpoint,
}

#[async_trait]
impl QuicEndpoint for SystemQuicEndpoint {
    async fn connect(&self, remote: RemoteSpec) -> Result<Box<dyn QuicConnection>, TransportError> {
        let client_config = client_config_for(&remote.cert_sha256_hex)?;
        info!("isekai-transport: connecting to {}", remote.addr);
        let conn = self
            .endpoint
            .connect_with(client_config, remote.addr, &remote.server_name)
            .map_err(|e| TransportError::ConnectSetup(e.to_string()))?
            .await
            .map_err(|e| TransportError::Handshake(e.to_string()))?;
        info!("isekai-transport: QUIC handshake ok rtt={:?}", conn.rtt(noq::PathId::ZERO));
        Ok(Box::new(SystemQuicConnection { conn }))
    }
}

struct SystemQuicConnection {
    conn: noq::Connection,
}

#[async_trait]
impl QuicConnection for SystemQuicConnection {
    async fn open_bi(&self) -> Result<Box<dyn ByteStream>, TransportError> {
        let (send, recv) =
            self.conn.open_bi().await.map_err(|e| TransportError::OpenStream(e.to_string()))?;
        Ok(Box::new(SystemByteStream { send, recv }))
    }

    async fn close(&self) {
        self.conn.close(noq::VarInt::from_u32(0), b"");
    }

    async fn export_keying_material(&self, label: &[u8], context: &[u8]) -> Result<[u8; 32], TransportError> {
        let mut out = [0u8; 32];
        self.conn
            .export_keying_material(&mut out, label, context)
            .map_err(|e| TransportError::ExportKeyingMaterial(format!("{e:?}")))?;
        Ok(out)
    }
}

struct SystemByteStream {
    send: noq::SendStream,
    recv: noq::RecvStream,
}

#[async_trait]
impl ByteStream for SystemByteStream {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self.recv.read(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))? {
            Some(n) => Ok(n),
            None => Ok(0), // stream finished cleanly (EOF)
        }
    }

    async fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError> {
        self.send.write_all(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))
    }

    async fn shutdown(&mut self) -> Result<(), TransportError> {
        self.send.finish().map_err(|e| TransportError::StreamIo(e.to_string()))
    }

    fn split(self: Box<Self>) -> (Box<dyn ByteStreamReadHalf>, Box<dyn ByteStreamWriteHalf>) {
        let SystemByteStream { send, recv } = *self;
        (Box::new(SystemByteStreamReadHalf { recv }), Box::new(SystemByteStreamWriteHalf { send }))
    }
}

struct SystemByteStreamReadHalf {
    recv: noq::RecvStream,
}

#[async_trait]
impl ByteStreamReadHalf for SystemByteStreamReadHalf {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self.recv.read(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))? {
            Some(n) => Ok(n),
            None => Ok(0), // stream finished cleanly (EOF)
        }
    }
}

struct SystemByteStreamWriteHalf {
    send: noq::SendStream,
}

#[async_trait]
impl ByteStreamWriteHalf for SystemByteStreamWriteHalf {
    async fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError> {
        self.send.write_all(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))
    }

    async fn shutdown(&mut self) -> Result<(), TransportError> {
        self.send.finish().map_err(|e| TransportError::StreamIo(e.to_string()))
    }
}
