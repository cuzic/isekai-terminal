//! `noq` (quinn's multipath fork, UDP-based) backend for `quicmux`'s mux
//! abstraction. Built directly on `noq` + `rustls`, mirroring
//! `isekai-transport`'s original `system::SystemQuicEndpointFactory`/
//! `system::SystemQuicEndpoint`/`system::SystemQuicConnection`/
//! `system::SystemByteStream` before they moved here.

use std::sync::{Arc, Mutex};

use noq::crypto::rustls::QuicClientConfig;

use crate::cert::{CertMismatchSlot, PinnedCertVerifier};
use crate::config::MuxClientConfig;
use crate::error::MuxError;
use crate::types::{BindSpec, RemoteSpec};

/// Adapts an already-bound `std::net::UdpSocket` into whatever concrete
/// `noq::AsyncUdpSocket` a [`NoqFactory`] should actually hand to
/// `noq::Endpoint::new_with_abstract_socket`. This is the one seam this
/// backend exposes for a caller that needs a non-default socket
/// implementation (e.g. a fault-injectable one for testing network
/// conditions) without `quicmux` itself ever needing to know such a thing
/// exists — see [`NoqFactory::with_socket_adapter`].
pub type AsyncUdpSocketAdapter =
    Arc<dyn Fn(std::net::UdpSocket) -> std::io::Result<Box<dyn noq::AsyncUdpSocket>> + Send + Sync>;

fn default_socket_adapter() -> AsyncUdpSocketAdapter {
    Arc::new(|std_socket| {
        use noq::Runtime as _;
        noq::TokioRuntime.wrap_udp_socket(std_socket)
    })
}

/// Builds a `noq::ClientConfig` pinned to `cert_sha256_hex` (see
/// [`PinnedCertVerifier`]) using `config`'s ALPN/idle-timeout/keepalive/
/// stream-limit/multipath tuning. `pub` so a caller that drives its own
/// `noq::Endpoint` directly (e.g. `isekai-transport::multipath`, which needs
/// `set_default_client_config` rather than going through
/// [`NoqEndpoint::connect`]) can still get the identical TLS/transport setup
/// instead of keeping its own near-identical copy.
pub fn noq_client_config(cert_sha256_hex: &str, config: &MuxClientConfig) -> Result<(noq::ClientConfig, CertMismatchSlot), MuxError> {
    let mismatch = Arc::new(Mutex::new(None));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| MuxError::TlsConfig(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier::new(
            cert_sha256_hex.to_string(),
            provider,
            mismatch.clone(),
        )))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![config.alpn.clone()];
    // 0-RTT is never used client-side — not calling `Connecting::
    // into_0rtt()` anywhere in this module is what implements that.

    let quic_crypto =
        QuicClientConfig::try_from(crypto).map_err(|_| MuxError::TlsConfig("QUIC crypto config failed".to_string()))?;

    let mut transport = noq::TransportConfig::default();
    transport.max_concurrent_bidi_streams(noq::VarInt::from_u32(config.max_concurrent_bidi_streams));
    transport.max_concurrent_uni_streams(noq::VarInt::from_u32(config.max_concurrent_uni_streams));
    transport.max_idle_timeout(Some(
        noq::IdleTimeout::try_from(config.max_idle_timeout).map_err(|e| MuxError::TlsConfig(e.to_string()))?,
    ));
    transport.keep_alive_interval(Some(config.keep_alive_interval));
    if config.multipath {
        // Matches `isekai-terminal-core`'s `multipath_transport.rs::
        // build_pinned_client_config`'s value — no product requirement
        // drove "8" specifically, just "more than the 1 primary + a small
        // number of secondaries a typical caller opens".
        transport.max_concurrent_multipath_paths(8);
    }

    let mut client_config = noq::ClientConfig::new(Arc::new(quic_crypto));
    client_config.transport_config(Arc::new(transport));
    Ok((client_config, mismatch))
}

/// Maps a `noq::ConnectionError` (whole-connection failure) onto this
/// crate's backend-agnostic [`MuxError`].
fn map_connection_error(e: noq::ConnectionError) -> MuxError {
    use noq::ConnectionError::*;
    match e {
        LocallyClosed => MuxError::LocallyClosed,
        ApplicationClosed(close) => {
            MuxError::PeerClosed { code: u64::from(close.error_code), reason: String::from_utf8_lossy(&close.reason).into_owned() }
        }
        ConnectionClosed(close) => MuxError::PeerClosed { code: 0, reason: close.to_string() },
        Reset => MuxError::TransportLost { reason: "reset by peer".to_string(), retryable: true },
        TimedOut => MuxError::TransportLost { reason: "idle timeout".to_string(), retryable: true },
        VersionMismatch => MuxError::ProtocolViolation("peer doesn't implement any supported version".to_string()),
        TransportError(e) => MuxError::ProtocolViolation(e.to_string()),
        CidsExhausted => MuxError::TransportLost { reason: "connection IDs exhausted".to_string(), retryable: false },
    }
}

/// Maps a `noq::WriteError` onto [`MuxError`].
fn map_write_error(e: noq::WriteError) -> MuxError {
    match e {
        noq::WriteError::Stopped(code) => MuxError::StreamReset { code: u64::from(code) },
        noq::WriteError::ConnectionLost(e) => map_connection_error(e),
        noq::WriteError::ClosedStream => MuxError::StreamIo("stream already finished or reset".to_string()),
        noq::WriteError::ZeroRttRejected => {
            MuxError::Unsupported { operation: "0-rtt", reason: "0-RTT is never used by this backend" }
        }
    }
}

/// Maps a `noq::ReadError` onto [`MuxError`].
fn map_read_error(e: noq::ReadError) -> MuxError {
    match e {
        noq::ReadError::Reset(code) => MuxError::StreamReset { code: u64::from(code) },
        noq::ReadError::ConnectionLost(e) => map_connection_error(e),
        noq::ReadError::ClosedStream => MuxError::StreamIo("stream already finished or reset".to_string()),
        noq::ReadError::ZeroRttRejected => {
            MuxError::Unsupported { operation: "0-rtt", reason: "0-RTT is never used by this backend" }
        }
    }
}

/// A `noq`-backed [`crate::AnyMuxFactory`] variant. Builds endpoints from
/// either a fresh local bind ([`NoqFactory::create_endpoint`], via
/// [`crate::AnyMuxFactory`]) or an already-bound socket
/// ([`NoqFactory::wrap_bound_socket`]) — both paths funnel through the same
/// `adapter` closure so a caller that needs a non-default socket
/// implementation only has to supply it once.
#[derive(Clone)]
pub struct NoqFactory {
    config: MuxClientConfig,
    adapter: AsyncUdpSocketAdapter,
}

impl NoqFactory {
    /// Builds a factory that binds/wraps plain `tokio::net::UdpSocket`s —
    /// the common case (CLI/PC callers with no need to intercept the
    /// underlying socket).
    pub fn new(config: MuxClientConfig) -> Self {
        Self { config, adapter: default_socket_adapter() }
    }

    /// Builds a factory that adapts every socket it binds/wraps through
    /// `adapter` before handing it to `noq::Endpoint::new_with_abstract_socket`
    /// — for a caller that needs a custom `noq::AsyncUdpSocket`
    /// implementation (e.g. a fault-injectable one for testing network
    /// conditions) without `quicmux` itself needing to know that such a
    /// thing exists.
    pub fn with_socket_adapter(config: MuxClientConfig, adapter: AsyncUdpSocketAdapter) -> Self {
        Self { config, adapter }
    }

    pub(crate) async fn create_endpoint(&self, bind: BindSpec) -> Result<NoqEndpoint, MuxError> {
        let socket = tokio::net::UdpSocket::bind(bind.local_addr)
            .await
            .map_err(|source| MuxError::Bind { addr: bind.local_addr, source })?;
        let std_socket = socket.into_std().map_err(|source| MuxError::Bind { addr: bind.local_addr, source })?;
        self.endpoint_from_std_socket(std_socket)
    }

    pub(crate) async fn wrap_bound_socket(&self, socket: tokio::net::UdpSocket) -> Result<NoqEndpoint, MuxError> {
        let std_socket = socket.into_std().map_err(|e| MuxError::SocketSetup(e.to_string()))?;
        self.endpoint_from_std_socket(std_socket)
    }

    fn endpoint_from_std_socket(&self, std_socket: std::net::UdpSocket) -> Result<NoqEndpoint, MuxError> {
        let async_socket = (self.adapter)(std_socket).map_err(|e| MuxError::SocketSetup(e.to_string()))?;
        endpoint_from_abstract_socket(async_socket, self.config.clone())
    }
}

/// Wraps an already-adapted `Box<dyn noq::AsyncUdpSocket>` as a
/// [`NoqEndpoint`] directly, bypassing [`NoqFactory`] entirely — the entry
/// point for a caller that builds its own custom `noq::AsyncUdpSocket`
/// implementation up front (rather than adapting a `tokio::net::UdpSocket`
/// on demand via [`NoqFactory::with_socket_adapter`]'s closure), e.g.
/// Android's fault-injectable socket, which binds and wraps itself in one
/// step with no intermediate `tokio::net::UdpSocket` ever created.
pub fn endpoint_from_abstract_socket(socket: Box<dyn noq::AsyncUdpSocket>, config: MuxClientConfig) -> Result<NoqEndpoint, MuxError> {
    let endpoint = noq::Endpoint::new_with_abstract_socket(noq::EndpointConfig::default(), None, socket, Arc::new(noq::TokioRuntime))
        .map_err(|e| MuxError::EndpointSetup(e.to_string()))?;
    Ok(NoqEndpoint { endpoint, config })
}

pub struct NoqEndpoint {
    endpoint: noq::Endpoint,
    config: MuxClientConfig,
}

impl NoqEndpoint {
    pub(crate) async fn connect(&self, remote: RemoteSpec) -> Result<NoqConnection, MuxError> {
        let (client_config, mismatch) = noq_client_config(&remote.cert_sha256_hex, &self.config)?;
        log::info!("quicmux(noq): connecting to {}", remote.addr);
        let conn = self
            .endpoint
            .connect_with(client_config, remote.addr, &remote.server_name)
            .map_err(|e| MuxError::ConnectSetup(e.to_string()))?
            .await
            .map_err(|e| match mismatch.lock().unwrap().take() {
                Some((expected, got)) => MuxError::CertPinMismatch { expected, got },
                None => map_connection_error(e),
            })?;
        log::info!("quicmux(noq): handshake ok rtt={:?}", conn.rtt(noq::PathId::ZERO));
        Ok(NoqConnection { conn })
    }

    pub(crate) fn rebinder(&self) -> NoqRebinder {
        // `noq::Endpoint` is a cheap, `Clone`-able handle onto shared
        // internal state ("May be cloned to obtain another handle to the
        // same endpoint" — its own doc comment), not the owner of a
        // background task that dies with this particular value, so cloning
        // it here and handing the clone to an independently-held rebinder is
        // exactly the intended usage.
        NoqRebinder { endpoint: self.endpoint.clone() }
    }
}

/// [`crate::AnyMuxRebinder`]'s `noq`-backed implementation —
/// `noq::Endpoint::rebind`. On error, the old UDP socket is retained (the
/// endpoint keeps using whatever socket it had before the attempt).
pub struct NoqRebinder {
    endpoint: noq::Endpoint,
}

impl NoqRebinder {
    pub(crate) async fn rebind_socket(&self, socket: std::net::UdpSocket) -> Result<(), MuxError> {
        self.endpoint.rebind(socket).map_err(|e| MuxError::Rebind(e.to_string()))
    }

    pub(crate) async fn rebind(&self, bind: BindSpec) -> Result<(), MuxError> {
        let socket = std::net::UdpSocket::bind(bind.local_addr).map_err(|source| MuxError::Bind { addr: bind.local_addr, source })?;
        self.rebind_socket(socket).await
    }
}

pub struct NoqConnection {
    conn: noq::Connection,
}

impl NoqConnection {
    pub(crate) async fn open_bi(&self) -> Result<NoqByteStream, MuxError> {
        let (send, recv) = self.conn.open_bi().await.map_err(map_connection_error)?;
        Ok(NoqByteStream { send, recv })
    }

    pub(crate) async fn close(&self) {
        self.conn.close(noq::VarInt::from_u32(0), b"");
    }

    pub(crate) async fn export_keying_material(&self, label: &[u8], context: &[u8]) -> Result<[u8; 32], MuxError> {
        let mut out = [0u8; 32];
        self.conn.export_keying_material(&mut out, label, context).map_err(|e| MuxError::ExportKeyingMaterial(format!("{e:?}")))?;
        Ok(out)
    }
}

pub struct NoqByteStream {
    send: noq::SendStream,
    recv: noq::RecvStream,
}

impl NoqByteStream {
    pub(crate) async fn read(&mut self, buf: &mut [u8]) -> Result<usize, MuxError> {
        match self.recv.read(buf).await.map_err(map_read_error)? {
            Some(n) => Ok(n),
            None => Ok(0), // stream finished cleanly (EOF)
        }
    }

    pub(crate) async fn write_all(&mut self, buf: &[u8]) -> Result<(), MuxError> {
        self.send.write_all(buf).await.map_err(map_write_error)
    }

    pub(crate) async fn shutdown(&mut self) -> Result<(), MuxError> {
        self.send.finish().map_err(|_| MuxError::StreamIo("stream already finished or reset".to_string()))
    }

    pub(crate) fn split(self) -> (NoqByteStreamReadHalf, NoqByteStreamWriteHalf) {
        (NoqByteStreamReadHalf { recv: self.recv }, NoqByteStreamWriteHalf { send: self.send })
    }
}

pub struct NoqByteStreamReadHalf {
    recv: noq::RecvStream,
}

impl NoqByteStreamReadHalf {
    pub(crate) async fn read(&mut self, buf: &mut [u8]) -> Result<usize, MuxError> {
        match self.recv.read(buf).await.map_err(map_read_error)? {
            Some(n) => Ok(n),
            None => Ok(0),
        }
    }
}

pub struct NoqByteStreamWriteHalf {
    send: noq::SendStream,
}

impl NoqByteStreamWriteHalf {
    pub(crate) async fn write_all(&mut self, buf: &[u8]) -> Result<(), MuxError> {
        self.send.write_all(buf).await.map_err(map_write_error)
    }

    pub(crate) async fn shutdown(&mut self) -> Result<(), MuxError> {
        self.send.finish().map_err(|_| MuxError::StreamIo("stream already finished or reset".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn test_config() -> MuxClientConfig {
        MuxClientConfig {
            alpn: b"quicmux-test/1".to_vec(),
            exporter_label: b"quicmux-test-exporter".to_vec(),
            max_idle_timeout: std::time::Duration::from_secs(15),
            keep_alive_interval: std::time::Duration::from_secs(5),
            max_concurrent_bidi_streams: 1,
            max_concurrent_uni_streams: 0,
            multipath: false,
        }
    }

    async fn start_echo_server() -> (SocketAddr, String) {
        let cert = rcgen::generate_simple_self_signed(vec!["quicmux-test.local".to_string()]).unwrap();
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().clone());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
        let cert_sha256_hex = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(cert_der.as_ref());
            hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
        };

        let mut server_crypto = rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(vec![cert_der], key_der).unwrap();
        server_crypto.alpn_protocols = vec![test_config().alpn];
        server_crypto.max_early_data_size = 0;
        let quic_crypto = noq::crypto::rustls::QuicServerConfig::try_from(server_crypto).unwrap();
        let server_config = noq::ServerConfig::with_crypto(Arc::new(quic_crypto));

        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        let endpoint = noq::Endpoint::server(server_config, bind_addr).unwrap();
        let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), endpoint.local_addr().unwrap().port());

        tokio::spawn(async move {
            let Some(incoming) = endpoint.accept().await else { return };
            let Ok(conn) = incoming.await else { return };
            while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                let mut buf = [0u8; 64];
                if let Ok(Some(n)) = recv.read(&mut buf).await {
                    let _ = send.write_all(&buf[..n]).await;
                }
                let _ = send.finish();
            }
        });

        (local_addr, cert_sha256_hex)
    }

    #[tokio::test]
    async fn connect_open_bi_write_and_read_roundtrip() {
        let (server_addr, cert_sha256_hex) = start_echo_server().await;

        let factory = NoqFactory::new(test_config());
        let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await.expect("create_endpoint failed");
        let conn = endpoint
            .connect(RemoteSpec { addr: server_addr, server_name: "quicmux-test.local".to_string(), cert_sha256_hex })
            .await
            .expect("connect failed");

        let mut stream = conn.open_bi().await.expect("open_bi failed");
        stream.write_all(b"hello quicmux noq backend").await.expect("write failed");
        stream.shutdown().await.expect("shutdown failed");

        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.expect("read failed");
        assert_eq!(&buf[..n], b"hello quicmux noq backend");

        let keying = conn.export_keying_material(b"test-label", b"").await.expect("export_keying_material failed");
        assert_eq!(keying.len(), 32);

        conn.close().await;
    }

    #[tokio::test]
    async fn connect_fails_on_cert_pin_mismatch() {
        let (server_addr, _correct_cert_sha256_hex) = start_echo_server().await;

        let factory = NoqFactory::new(test_config());
        let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await.expect("create_endpoint failed");
        let result = endpoint
            .connect(RemoteSpec { addr: server_addr, server_name: "quicmux-test.local".to_string(), cert_sha256_hex: "0".repeat(64) })
            .await;

        match result {
            Err(MuxError::CertPinMismatch { .. }) => {}
            Err(other) => panic!("expected CertPinMismatch, got {other:?}"),
            Ok(_) => panic!("expected CertPinMismatch, connect unexpectedly succeeded"),
        }
    }
}
