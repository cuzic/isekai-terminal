//! `SystemQuicEndpointFactory`: the CLI's concrete `QuicEndpointFactory`,
//! built directly on `noq` + `rustls` + a plain `tokio::net::UdpSocket`
//! (`archive/ISEKAI_SSH_DESIGN.md` "実装方針": "中身はnoqとrustlsを直接使い、
//! tokio::net::UdpSocketをbindしてnoq::Endpointのクライアントとして使う").
//!
//! Deliberately must never reference `FaultyUdpSocket`, UniFFI, or any other
//! Android/`isekai-terminal-core`-specific type — this crate is also linked into
//! `isekai-ssh`, a plain CLI binary with no Android runtime.
//!
//! The certificate-pinning logic (`PinnedCertVerifier`) and QUIC transport
//! tuning (idle timeout / keepalive interval) are copied verbatim from
//! `isekai_pipe_quic_transport.rs::establish_quic_connection_with_socket` and its
//! `PinnedCertVerifier`, minus the `FaultyUdpSocket` parameter — this crate
//! binds a real `tokio::net::UdpSocket` instead.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use isekai_protocol::hello::ALPN;
use log::info;
use noq::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use sha2::{Digest, Sha256};

use crate::error::TransportError;
use crate::traits::{
    ByteStream, ByteStreamReadHalf, ByteStreamWriteHalf, QuicConnection, QuicEndpoint, QuicEndpointFactory,
    QuicEndpointRebinder,
};
use crate::types::{BindSpec, RemoteSpec};

/// QUIC connection is declared dead after this much silence. Matches
/// `isekai_pipe_quic_transport.rs::CLIENT_MAX_IDLE_TIMEOUT` — see that file's
/// comment on the Phase 8-4b timing bug this specific value avoids (must be
/// short enough that a dead connection is detected before isekai-helper's
/// parked-session TTL expires).
const CLIENT_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// PING interval to keep NAT UDP mappings alive. Matches
/// `isekai_pipe_quic_transport.rs::CLIENT_KEEP_ALIVE_INTERVAL` (kept at 1/3 of the
/// idle timeout so a handful of lost PINGs can be tolerated).
const CLIENT_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// Verifies the server's leaf certificate against a pinned SHA-256
/// fingerprint instead of a CA chain — copied from
/// `isekai_pipe_quic_transport.rs::PinnedCertVerifier`. isekai-helper presents an
/// ephemeral self-signed cert, so ordinary CA validation cannot apply; the
/// fingerprint itself is the trust root, delivered out-of-band over the
/// bootstrap SSH channel (`HandshakeJson::cert_sha256`).
/// `pub(crate)` (not private) so `qmux_relay.rs`'s manually-driven TLS
/// handshake (`qmux::Session::connect` needs a raw `tokio_rustls::TlsStream`,
/// not a `noq::ClientConfig`, so it can't go through [`client_config_for`])
/// can reuse the exact same cert-pinning logic instead of duplicating a
/// security-sensitive verifier.
#[derive(Debug)]
pub(crate) struct PinnedCertVerifier {
    expected_sha256_hex: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
    /// Set right before returning `Err` on a mismatch, since
    /// `ServerCertVerifier::verify_server_cert`'s return type is fixed to
    /// `Result<_, rustls::Error>` and can't carry a typed
    /// `TransportError::CertPinMismatch` directly. `SystemQuicEndpoint::connect`
    /// checks this slot after a handshake failure to recover the structured
    /// reason (`ISEKAI_PIPE_DESIGN.md` §8 Epic N) — the TLS behavior itself
    /// (still returning `Err`, aborting the handshake) is unchanged.
    mismatch: Arc<Mutex<Option<(String, String)>>>,
}

impl PinnedCertVerifier {
    /// `pub(crate)` constructor — see the struct's own doc comment for why
    /// `qmux_relay.rs` needs this instead of going through
    /// [`client_config_for`].
    pub(crate) fn new(
        expected_sha256_hex: String,
        provider: Arc<rustls::crypto::CryptoProvider>,
        mismatch: Arc<Mutex<Option<(String, String)>>>,
    ) -> Self {
        Self { expected_sha256_hex, provider, mismatch }
    }
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
            *self.mismatch.lock().unwrap() = Some((self.expected_sha256_hex.clone(), got.clone()));
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

/// Builds a `noq::ClientConfig` pinned to `cert_sha256_hex` (see
/// [`PinnedCertVerifier`]) with this crate's fixed idle-timeout/keepalive/
/// stream-limit tuning. `pub` (not crate-private) so `isekai-terminal-core`'s
/// own `QuicEndpoint` adapter (`rust-core/src/android_quic_endpoint.rs`) can
/// reuse the exact same TLS/transport config instead of keeping its own
/// near-identical copy (isekai-terminal-core/isekai-transport crate共有化
/// Phase 1b) — this function has no Android-specific or CLI-specific
/// dependency, only `noq`/`rustls`, so widening its visibility doesn't cross
/// the "no Android/UniFFI types in this crate" boundary the module docs
/// describe.
///
/// `multipath`: whether to advertise noq's multipath extension
/// (`TransportConfig::max_concurrent_multipath_paths`) — required on *both*
/// sides of a connection before `noq::Connection::open_path` will do
/// anything but fail with "multipath extension not negotiated" (confirmed
/// via `multipath::connect_multipath`'s own e2e test). This is *also* a
/// prerequisite for `noq::Endpoint::rebind`'s own connection-migration
/// (PATH_CHALLENGE/PATH_RESPONSE) validation to succeed rather than hang
/// (confirmed via `rebind_e2e.rs` — without it, rebinding a live connection
/// onto a new socket silently breaks it) — so `SystemQuicEndpoint::connect`
/// passes `true` unconditionally: any connection it makes might later go
/// through its own `rebinder()` (`isekai-pipe`'s `--experimental-network-
/// rebind`), and `isekai-pipe serve`'s own server-side `TransportConfig`
/// (`isekai-pipe/src/engine/mod.rs`) already negotiates this unconditionally
/// too, for the identical reason ("既存clientはopen_path()を呼ばないため影響なし",
/// Phase 9-1) — a client that doesn't negotiate it back was the one actual
/// asymmetry, not a deliberate opt-in. `android_quic_endpoint.rs` is the one
/// caller that still passes `false`: `AndroidQuicEndpoint::rebinder()` is
/// unconditionally `None` (Android's own `multipath_transport.rs` handles
/// physical-interface failover through its own mechanism, not this trait —
/// see that type's docs), so there is nothing on the Android side that could
/// ever call `rebind()` through this path to make multipath negotiation
/// worth the (harmless, but non-zero) extra transport parameter.
///
/// Also returns a `mismatch` slot (`ISEKAI_PIPE_DESIGN.md` §8 Epic N):
/// `PinnedCertVerifier` records `(expected, got)` there right before
/// returning `Err` on a cert-pin mismatch, since `ServerCertVerifier`'s
/// trait-fixed return type can't carry a typed `TransportError` directly —
/// `SystemQuicEndpoint::connect` checks this slot after a handshake failure
/// to recover `TransportError::CertPinMismatch`.
pub fn client_config_for(
    cert_sha256_hex: &str,
    multipath: bool,
) -> Result<(noq::ClientConfig, Arc<Mutex<Option<(String, String)>>>), TransportError> {
    let mismatch = Arc::new(Mutex::new(None));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| TransportError::TlsConfig(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
            expected_sha256_hex: cert_sha256_hex.to_string(),
            provider,
            mismatch: mismatch.clone(),
        }))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    // 0-RTT is never used client-side either (`archive/HELPER_PROTOCOL.md`: "0-RTT は
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
    if multipath {
        // Matches `multipath_transport.rs::build_pinned_client_config`'s value —
        // no product requirement drove "8" specifically, just "more than the
        // 1 primary + a small number of secondaries this crate opens".
        transport.max_concurrent_multipath_paths(8);
    }

    let mut client_config = noq::ClientConfig::new(Arc::new(quic_crypto));
    client_config.transport_config(Arc::new(transport));
    Ok((client_config, mismatch))
}

/// Wraps an already-bound `std::net::UdpSocket` as a `noq`-backed
/// `QuicEndpoint`. Shared by `SystemQuicEndpointFactory::create_endpoint`
/// (which binds a fresh socket) and `stun_p2p::connect_stun_p2p` (which
/// reuses a socket that already performed a STUN query and sent hole-punch
/// probes on it — `isekai_stun_p2p_transport.rs`'s comment on why the STUN/
/// probe step must happen *before* handing the socket to `noq::Endpoint`, to
/// avoid a race between noq's internal `poll_recv` and raw reads on the same
/// socket) — this is the "既存の生ソケットをQUICエンドポイントにラップする"
/// logic `archive/ISEKAI_SSH_DESIGN.md` calls out as needing exactly one
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

    async fn wrap_bound_socket(&self, socket: tokio::net::UdpSocket) -> Result<Box<dyn QuicEndpoint>, TransportError> {
        let std_socket = socket.into_std().map_err(|e| TransportError::SocketSetup(e.to_string()))?;
        quic_endpoint_from_std_socket(std_socket)
    }
}

struct SystemQuicEndpoint {
    endpoint: noq::Endpoint,
}

#[async_trait]
impl QuicEndpoint for SystemQuicEndpoint {
    async fn connect(&self, remote: RemoteSpec) -> Result<Box<dyn QuicConnection>, TransportError> {
        let (client_config, mismatch) = client_config_for(&remote.cert_sha256_hex, true)?;
        info!("isekai-transport: connecting to {}", remote.addr);
        let conn = self
            .endpoint
            .connect_with(client_config, remote.addr, &remote.server_name)
            .map_err(|e| TransportError::ConnectSetup(e.to_string()))?
            .await
            .map_err(|e| match mismatch.lock().unwrap().take() {
                Some((expected, got)) => TransportError::CertPinMismatch { expected, got },
                None => TransportError::Handshake(e.to_string()),
            })?;
        info!("isekai-transport: QUIC handshake ok rtt={:?}", conn.rtt(noq::PathId::ZERO));
        Ok(Box::new(SystemQuicConnection { conn }))
    }

    fn rebinder(&self) -> Option<Box<dyn QuicEndpointRebinder>> {
        // `noq::Endpoint` is a cheap, `Clone`-able handle onto shared
        // internal state ("May be cloned to obtain another handle to the
        // same endpoint" — its own doc comment), not the owner of a
        // background task that dies with this particular value, so cloning
        // it here and handing the clone to an independently-held rebinder
        // is exactly the intended usage (mirrors `multipath_transport.rs`'s
        // `spawn_rebind_listener`, which keeps its own `noq::Endpoint` value
        // for the same purpose, entirely separate from wherever the
        // `noq::Connection` it produced lives).
        Some(Box::new(SystemQuicEndpointRebinder { endpoint: self.endpoint.clone() }))
    }
}

/// [`QuicEndpointRebinder`] for the CLI's `noq`-backed endpoint —
/// [`noq::Endpoint::rebind`], the same operation
/// `multipath_transport.rs`'s Android code exercises (both on real hardware
/// and in loopback tests) as `Endpoint::rebind_abstract()`. This uses the
/// plain `rebind()` overload (a `std::net::UdpSocket`, not a custom
/// `AsyncUdpSocket` impl) since all this needs is "hand it an already-bound
/// socket" — the caller (`QuicEndpointRebinder::rebind_socket`'s caller)
/// decides how that socket got bound (a plain ephemeral port via
/// [`QuicEndpointRebinder::rebind`]'s default impl, or a specific physical
/// interface via `physical_interface::bind_physical_interface`).
///
/// `noq::Endpoint::rebind`'s own doc comment: "On error, the old UDP socket
/// is retained" — a failed [`QuicEndpointRebinder::rebind_socket`] call
/// through this type never leaves the endpoint in a half-switched state;
/// the connection keeps using whatever socket it had before the attempt.
struct SystemQuicEndpointRebinder {
    endpoint: noq::Endpoint,
}

#[async_trait]
impl QuicEndpointRebinder for SystemQuicEndpointRebinder {
    async fn rebind_socket(&self, socket: std::net::UdpSocket) -> Result<(), TransportError> {
        self.endpoint.rebind(socket).map_err(|e| TransportError::Rebind(e.to_string()))
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
