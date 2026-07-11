//! `qmux` (draft-ietf-quic-qmux — QUIC's stream API polyfilled over
//! TLS-over-TCP instead of raw UDP+QUIC packets) backend for `quicmux`'s mux
//! abstraction, for reaching a peer from a network that blocks outbound UDP.
//! Mirrors `isekai-transport`'s original `qmux_relay::QmuxQuicEndpointFactory`/
//! `QmuxQuicEndpoint`/`QmuxQuicConnection`/`QmuxByteStream` before they moved
//! here — the notable differences accounted for here (all inherited from that
//! original module's own docs):
//!
//! - `qmux::Session::connect` takes ownership of (and internally
//!   `tokio::io::split`s) whatever `Transport` it's given, so there is no way
//!   to retrieve a live handle to the underlying `rustls` connection *after*
//!   handing it off. This drives the TLS handshake manually
//!   (`tokio_rustls::TlsConnector`, not `qmux::tls::Client`) and captures the
//!   `config.exporter_label` export immediately after the handshake
//!   completes, before handing the stream to `qmux::transport::Stream::new`.
//!   [`QmuxConnection::export_keying_material`] can only ever serve that one
//!   cached `(label, context=b"")` pair; any other request fails closed with
//!   [`MuxError::Unsupported`] rather than silently returning something
//!   wrong.
//! - No rebind support — rebinding a *TCP* connection onto a new local
//!   address isn't the same operation the `noq` backend's rebinder performs
//!   (a UDP-socket swap on an otherwise-unbroken QUIC connection); a TCP
//!   equivalent would mean tearing down and re-establishing the whole
//!   session, which is just a fresh `connect()` call, not a distinct
//!   primitive. [`crate::AnyMuxEndpoint::rebinder`] always returns `None` for
//!   this backend.
//! - [`QmuxFactory::wrap_bound_socket`] structurally cannot succeed — `qmux`
//!   runs over TCP, so there is no bound UDP socket for it to wrap.
//! - [`QmuxListener`] has no `noq::Endpoint`-like centralized connection
//!   tracking: every [`QmuxConnection`] it produces keeps itself alive
//!   independently via its own `qmux::Session` clone (see
//!   [`QmuxConnection::open_bi`]'s comment), with no shared owning structure
//!   the listener could enumerate or wait on. [`QmuxListener::close`] only
//!   stops *accepting new* connections; it does not touch already-accepted
//!   ones. [`QmuxListener::wait_idle`] is a no-op for the same reason — there
//!   is nothing for it to wait on; a caller that needs to know when a
//!   specific accepted connection is done must track that connection's own
//!   lifetime itself, not go through the listener.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::cert::PinnedCertVerifier;
use crate::config::{MuxClientConfig, MuxServerConfig};
use crate::error::MuxError;
use crate::types::{BindSpec, RemoteSpec};

/// Maps a `qmux::Error` onto this crate's backend-agnostic [`MuxError`].
fn map_qmux_error(e: qmux::Error) -> MuxError {
    match e {
        qmux::Error::ConnectionClosed { code, reason } => MuxError::PeerClosed { code: u64::from(code), reason },
        qmux::Error::StreamReset(code) | qmux::Error::StreamStop(code) => MuxError::StreamReset { code: u64::from(code) },
        qmux::Error::Closed | qmux::Error::IdleTimeout => {
            MuxError::TransportLost { reason: e.to_string(), retryable: true }
        }
        qmux::Error::HandshakeTimeout => MuxError::Handshake(e.to_string()),
        qmux::Error::InvalidFrameType(_)
        | qmux::Error::InvalidStreamId
        | qmux::Error::StreamClosed
        | qmux::Error::FrameTooLarge
        | qmux::Error::FlowControlError
        | qmux::Error::FrameEncoding
        | qmux::Error::ProtocolViolation
        | qmux::Error::TransportParameter
        | qmux::Error::StreamLimitExceeded
        | qmux::Error::DuplicateParam(_)
        | qmux::Error::Short
        | qmux::Error::InvalidProtocol(_)
        | qmux::Error::UnexpectedProtocols
        | qmux::Error::InvalidServerName => MuxError::ProtocolViolation(e.to_string()),
        qmux::Error::Http(status) => MuxError::AuthenticationFailed(format!("http status {status}")),
        qmux::Error::Io(io_err) => MuxError::StreamIo(io_err.to_string()),
        other => MuxError::StreamIo(other.to_string()),
    }
}

/// A `qmux`-backed [`crate::AnyMuxFactory`] variant. Stateless — every
/// [`QmuxFactory::create_endpoint`] call just records the requested local
/// bind address; the actual TCP connect + TLS handshake + QMux session
/// happens lazily in [`QmuxEndpoint::connect`].
#[derive(Debug, Clone)]
pub struct QmuxFactory {
    config: MuxClientConfig,
}

impl QmuxFactory {
    pub fn new(config: MuxClientConfig) -> Self {
        Self { config }
    }

    pub(crate) async fn create_endpoint(&self, bind: crate::types::BindSpec) -> Result<QmuxEndpoint, MuxError> {
        Ok(QmuxEndpoint { local_addr: bind.local_addr, config: self.config.clone() })
    }

    pub(crate) async fn wrap_bound_socket(&self, _socket: tokio::net::UdpSocket) -> Result<QmuxEndpoint, MuxError> {
        Err(MuxError::Unsupported {
            operation: "wrap_bound_socket",
            reason: "QMux runs over TCP, not UDP — there is no bound UDP socket for it to wrap",
        })
    }
}

pub struct QmuxEndpoint {
    local_addr: std::net::SocketAddr,
    config: MuxClientConfig,
}

impl QmuxEndpoint {
    pub(crate) async fn connect(&self, remote: RemoteSpec) -> Result<QmuxConnection, MuxError> {
        let socket = tokio::net::TcpSocket::new_v4().map_err(|source| MuxError::Bind { addr: self.local_addr, source })?;
        socket.bind(self.local_addr).map_err(|source| MuxError::Bind { addr: self.local_addr, source })?;
        let tcp = socket.connect(remote.addr).await.map_err(|e| MuxError::ConnectSetup(e.to_string()))?;

        let mismatch = Arc::new(Mutex::new(None));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| MuxError::TlsConfig(e.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier::new(remote.cert_sha256_hex.clone(), provider, mismatch.clone())))
            .with_no_client_auth();
        crypto.alpn_protocols = vec![self.config.alpn.clone()];
        // 0-RTT never used (matches the `noq` backend's contract) — not
        // calling anything 0-RTT-related here is what implements that.

        let connector = tokio_rustls::TlsConnector::from(Arc::new(crypto));
        let server_name =
            rustls::pki_types::ServerName::try_from(remote.server_name.clone()).map_err(|e| MuxError::TlsConfig(format!("invalid server_name: {e}")))?;
        let mut tls_stream = connector.connect(server_name, tcp).await.map_err(|e| match mismatch.lock().unwrap().take() {
            Some((expected, got)) => MuxError::CertPinMismatch { expected, got },
            None => MuxError::Handshake(e.to_string()),
        })?;

        // See this module's docs: this is the one and only export this
        // connection will ever perform, captured now because
        // `qmux::Session::connect` (below) takes ownership of `tls_stream`.
        let mut exporter = [0u8; 32];
        tls_stream
            .get_mut()
            .1
            .export_keying_material(&mut exporter, &self.config.exporter_label, None)
            .map_err(|e| MuxError::ExportKeyingMaterial(e.to_string()))?;

        let config = qmux::Config::new(qmux::Version::QMux01);
        let transport = qmux::transport::Stream::new(tls_stream, config.version, config.max_record_size);
        let session = qmux::Session::connect(transport, config).await.map_err(|e| MuxError::Handshake(format!("QMux handshake failed: {e}")))?;

        Ok(QmuxConnection { session, exporter_label: self.config.exporter_label.clone(), exporter, remote_addr: Some(remote.addr) })
    }
}

/// A `qmux`-backed [`crate::AnyMuxListener`] variant — a bound TCP listener
/// that TLS-accepts each inbound connection and establishes a `qmux::Session`
/// over it. See this module's docs for what `close`/`wait_idle` can and
/// cannot do here (no centralized connection tracking, unlike `noq`).
pub struct QmuxListener {
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    exporter_label: Vec<u8>,
    /// `accept()` checks this before/while waiting on the next TCP
    /// connection — a `tokio::net::TcpListener` has no `close()`/`shutdown()`
    /// of its own to call from a `&self` method, so this plus `close_notify`
    /// (to wake a call already blocked in `accept()`) is this backend's
    /// stand-in for that.
    closed: Arc<AtomicBool>,
    close_notify: Arc<tokio::sync::Notify>,
}

impl QmuxListener {
    pub(crate) async fn bind(config: MuxServerConfig, bind: BindSpec) -> Result<Self, MuxError> {
        let listener = tokio::net::TcpListener::bind(bind.local_addr).await.map_err(|source| MuxError::Bind { addr: bind.local_addr, source })?;

        // Explicit provider (not the bare `rustls::ServerConfig::builder()`,
        // which relies on a process-wide default having been installed via
        // `CryptoProvider::install_default()` somewhere else) — matches
        // `QmuxEndpoint::connect`'s identical choice on the client side, so
        // this backend never depends on caller/process global state.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut server_crypto = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| MuxError::TlsConfig(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(config.cert_chain.clone(), config.private_key.clone_key())
            .map_err(|e| MuxError::TlsConfig(e.to_string()))?;
        server_crypto.alpn_protocols = vec![config.alpn.clone()];
        // See `MuxClientConfig`'s docs: 0-RTT is never used by this crate.
        server_crypto.max_early_data_size = 0;
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_crypto));

        Ok(Self {
            listener,
            acceptor,
            exporter_label: config.exporter_label,
            closed: Arc::new(AtomicBool::new(false)),
            close_notify: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Waits for the next inbound TCP connection. Unlike [`crate::noq_backend::NoqListener::accept`],
    /// this does not separately expose "candidate received" vs. "handshake
    /// complete" — a `noq::Incoming` distinguishes those because `noq`'s
    /// QUIC handshake happens inside the transport itself before the
    /// `Connection` is usable, whereas here the TLS+QMux handshake is this
    /// module's own manually-driven code (same shape client-side, see
    /// [`QmuxEndpoint::connect`]), so [`QmuxIncoming::accept`] is where all
    /// of it happens, with no intermediate "accepted, not yet
    /// handshaken" state worth exposing.
    pub(crate) async fn accept(&self) -> Option<QmuxIncoming> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        tokio::select! {
            _ = self.close_notify.notified() => None,
            result = self.listener.accept() => match result {
                Ok((tcp, peer_addr)) => Some(QmuxIncoming {
                    tcp,
                    peer_addr,
                    acceptor: self.acceptor.clone(),
                    exporter_label: self.exporter_label.clone(),
                }),
                Err(_) => None,
            },
        }
    }

    pub(crate) fn local_addr(&self) -> Result<SocketAddr, MuxError> {
        self.listener.local_addr().map_err(|e| MuxError::EndpointSetup(e.to_string()))
    }

    /// Stops accepting new connections. Does **not** close any connection
    /// already produced by a prior [`QmuxListener::accept`]/[`QmuxIncoming::accept`]
    /// — see this module's docs.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.close_notify.notify_waiters();
    }

    /// No-op — see this module's docs for why this backend has nothing to
    /// wait on here.
    pub(crate) async fn wait_idle(&self) {}
}

/// A TCP connection [`QmuxListener::accept`] received, not yet TLS/QMux-
/// handshaken.
pub struct QmuxIncoming {
    tcp: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    acceptor: tokio_rustls::TlsAcceptor,
    exporter_label: Vec<u8>,
}

impl QmuxIncoming {
    pub(crate) async fn accept(self) -> Result<QmuxConnection, MuxError> {
        let mut tls_stream = self.acceptor.accept(self.tcp).await.map_err(|e| MuxError::Handshake(e.to_string()))?;

        // Captured now, symmetric to the client side — see this module's
        // docs for why this must happen before `qmux::Session::accept`
        // takes ownership of `tls_stream`.
        let mut exporter = [0u8; 32];
        tls_stream
            .get_mut()
            .1
            .export_keying_material(&mut exporter, &self.exporter_label, None)
            .map_err(|e| MuxError::ExportKeyingMaterial(e.to_string()))?;

        let config = qmux::Config::new(qmux::Version::QMux01);
        let transport = qmux::transport::Stream::new(tls_stream, config.version, config.max_record_size);
        let session = qmux::Session::accept(transport, config).await.map_err(|e| MuxError::Handshake(format!("QMux handshake failed: {e}")))?;

        Ok(QmuxConnection { session, exporter_label: self.exporter_label, exporter, remote_addr: Some(self.peer_addr) })
    }
}

pub struct QmuxConnection {
    session: qmux::Session,
    /// The `label` `export_keying_material` was actually captured for — see
    /// this module's docs for why only this exact `(label, context=b"")`
    /// pair can ever be served. An owned `Vec<u8>` (not `&'static [u8]`,
    /// unlike this backend's pre-`quicmux` ancestor): the label is now a
    /// runtime-supplied [`MuxClientConfig`] field rather than a fixed
    /// program constant, so it can't be borrowed with a `'static` lifetime
    /// any more.
    exporter_label: Vec<u8>,
    exporter: [u8; 32],
    /// The peer's TCP address, known unconditionally at both connect time
    /// (the caller-supplied [`RemoteSpec::addr`]) and accept time
    /// (`TcpListener::accept`'s own return value) — always `Some` in
    /// practice; `Option` only because a future construction path might not
    /// have one to hand, not because either existing path can fail to.
    remote_addr: Option<SocketAddr>,
}

impl QmuxConnection {
    pub(crate) async fn open_bi(&self) -> Result<QmuxByteStream, MuxError> {
        use web_transport_trait::Session as _;
        let (send, recv) = self.session.open_bi().await.map_err(map_qmux_error)?;
        // `qmux::Session` closes the whole connection once its *last handle*
        // is dropped ("Closes the connection once the last Session handle is
        // dropped" — its own doc comment); `SendStream`/`RecvStream` don't
        // independently keep it alive, so every stream keeps its own clone.
        Ok(QmuxByteStream { send, recv, _session: self.session.clone() })
    }

    /// Accepts a new bidirectional stream the peer opened. Symmetric with
    /// [`QmuxConnection::open_bi`] — `qmux::Session` itself has no client/
    /// server distinction once the handshake is done, matching
    /// [`crate::noq_backend::NoqConnection::accept_bi`]'s identical framing.
    pub(crate) async fn accept_bi(&self) -> Result<QmuxByteStream, MuxError> {
        use web_transport_trait::Session as _;
        let (send, recv) = self.session.accept_bi().await.map_err(map_qmux_error)?;
        Ok(QmuxByteStream { send, recv, _session: self.session.clone() })
    }

    pub(crate) async fn close(&self) {
        use web_transport_trait::Session as _;
        self.session.close(0, "");
    }

    pub(crate) async fn export_keying_material(&self, label: &[u8], context: &[u8]) -> Result<[u8; 32], MuxError> {
        keying_material_for(&self.exporter_label, self.exporter, label, context)
    }

    pub(crate) fn remote_addr(&self) -> Option<SocketAddr> {
        self.remote_addr
    }
}

/// The fail-closed decision behind [`QmuxConnection::export_keying_material`]:
/// only the exact `(label, context)` pair captured at connect time can ever be
/// served (see this module's docs for why `qmux::Session::connect` makes any
/// other export impossible after the fact). Split out from the method so it
/// can be unit-tested without a live `qmux::Session`.
fn keying_material_for(
    captured_label: &[u8],
    captured_material: [u8; 32],
    requested_label: &[u8],
    requested_context: &[u8],
) -> Result<[u8; 32], MuxError> {
    if requested_label == captured_label && requested_context.is_empty() {
        Ok(captured_material)
    } else {
        Err(MuxError::Unsupported {
            operation: "export_keying_material",
            reason: "the qmux backend only supports the single (label, context) pair captured at connect time \
                      (qmux::Session::connect takes ownership of the TLS stream, so no later export is possible)",
        })
    }
}

#[cfg(test)]
mod keying_material_tests {
    use super::*;

    const CAPTURED_MATERIAL: [u8; 32] = [7u8; 32];
    const CAPTURED_LABEL: &[u8] = b"captured-label";

    #[test]
    fn returns_the_captured_material_for_the_exact_label_and_empty_context() {
        let result = keying_material_for(CAPTURED_LABEL, CAPTURED_MATERIAL, CAPTURED_LABEL, b"");

        assert_eq!(result.unwrap(), CAPTURED_MATERIAL);
    }

    #[test]
    fn fails_closed_on_a_different_label() {
        let result = keying_material_for(CAPTURED_LABEL, CAPTURED_MATERIAL, b"some-other-label", b"");

        assert!(matches!(result, Err(MuxError::Unsupported { operation: "export_keying_material", .. })));
    }

    #[test]
    fn fails_closed_on_a_non_empty_context() {
        let result = keying_material_for(CAPTURED_LABEL, CAPTURED_MATERIAL, CAPTURED_LABEL, b"nonempty");

        assert!(matches!(result, Err(MuxError::Unsupported { operation: "export_keying_material", .. })));
    }

    #[test]
    fn fails_closed_on_both_a_different_label_and_a_non_empty_context() {
        let result = keying_material_for(CAPTURED_LABEL, CAPTURED_MATERIAL, b"some-other-label", b"nonempty");

        assert!(matches!(result, Err(MuxError::Unsupported { operation: "export_keying_material", .. })));
    }
}

pub struct QmuxByteStream {
    send: qmux::SendStream,
    recv: qmux::RecvStream,
    /// Keeps the underlying `qmux::Session` alive — see `open_bi`'s comment.
    _session: qmux::Session,
}

impl QmuxByteStream {
    pub(crate) async fn read(&mut self, buf: &mut [u8]) -> Result<usize, MuxError> {
        use web_transport_trait::RecvStream as _;
        match self.recv.read(buf).await.map_err(map_qmux_error)? {
            Some(n) => Ok(n),
            None => Ok(0), // stream finished cleanly (EOF)
        }
    }

    pub(crate) async fn write_all(&mut self, buf: &[u8]) -> Result<(), MuxError> {
        use web_transport_trait::SendStream as _;
        self.send.write_all(buf).await.map_err(map_qmux_error)
    }

    pub(crate) async fn shutdown(&mut self) -> Result<(), MuxError> {
        use web_transport_trait::SendStream as _;
        self.send.finish().map_err(map_qmux_error)
    }

    pub(crate) fn split(self) -> (QmuxByteStreamReadHalf, QmuxByteStreamWriteHalf) {
        let QmuxByteStream { send, recv, _session } = self;
        (QmuxByteStreamReadHalf { recv, _session: _session.clone() }, QmuxByteStreamWriteHalf { send, _session })
    }
}

pub struct QmuxByteStreamReadHalf {
    recv: qmux::RecvStream,
    /// Keeps the underlying `qmux::Session` alive — see `QmuxConnection::open_bi`'s comment.
    _session: qmux::Session,
}

impl QmuxByteStreamReadHalf {
    pub(crate) async fn read(&mut self, buf: &mut [u8]) -> Result<usize, MuxError> {
        use web_transport_trait::RecvStream as _;
        match self.recv.read(buf).await.map_err(map_qmux_error)? {
            Some(n) => Ok(n),
            None => Ok(0),
        }
    }
}

pub struct QmuxByteStreamWriteHalf {
    send: qmux::SendStream,
    /// Keeps the underlying `qmux::Session` alive — see `QmuxConnection::open_bi`'s comment.
    _session: qmux::Session,
}

impl QmuxByteStreamWriteHalf {
    pub(crate) async fn write_all(&mut self, buf: &[u8]) -> Result<(), MuxError> {
        use web_transport_trait::SendStream as _;
        self.send.write_all(buf).await.map_err(map_qmux_error)
    }

    pub(crate) async fn shutdown(&mut self) -> Result<(), MuxError> {
        use web_transport_trait::SendStream as _;
        self.send.finish().map_err(map_qmux_error)
    }
}

#[cfg(test)]
mod listener_tests {
    use super::*;
    use crate::types::BindSpec;

    fn test_client_config() -> MuxClientConfig {
        MuxClientConfig {
            alpn: b"quicmux-test/1".to_vec(),
            exporter_label: b"quicmux-test-exporter".to_vec(),
            max_idle_timeout: std::time::Duration::from_secs(15),
            keep_alive_interval: std::time::Duration::from_secs(5),
            max_concurrent_bidi_streams: 2,
            max_concurrent_uni_streams: 0,
            multipath: false,
        }
    }

    fn test_server_config() -> (MuxServerConfig, String) {
        let cert = rcgen::generate_simple_self_signed(vec!["quicmux-test.local".to_string()]).unwrap();
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().clone());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
        let cert_sha256_hex = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(cert_der.as_ref());
            hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let config = MuxServerConfig {
            alpn: test_client_config().alpn,
            exporter_label: test_client_config().exporter_label,
            max_idle_timeout: std::time::Duration::from_secs(15),
            keep_alive_interval: std::time::Duration::from_secs(5),
            max_concurrent_bidi_streams: 2,
            max_concurrent_uni_streams: 0,
            multipath: false,
            cert_chain: vec![cert_der],
            private_key: key_der,
        };
        (config, cert_sha256_hex)
    }

    #[tokio::test]
    async fn listener_bind_accept_bi_write_and_read_roundtrip() {
        let (server_config, cert_sha256_hex) = test_server_config();
        let listener = QmuxListener::bind(server_config, BindSpec::any_ipv4()).await.expect("listener bind failed");
        let server_addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let Some(incoming) = listener.accept().await else { return };
            let Ok(conn) = incoming.accept().await else { return };
            let Ok(mut stream) = conn.accept_bi().await else { return };
            let mut buf = [0u8; 64];
            if let Ok(n) = stream.read(&mut buf).await {
                let _ = stream.write_all(&buf[..n]).await;
            }
            let _ = stream.shutdown().await;
            // See `noq_backend`'s identical comment: keep `conn` alive until
            // the client itself closes, instead of dropping it (and the
            // whole listener) the instant the echo is written.
            let _ = conn.accept_bi().await;
        });

        let factory = QmuxFactory::new(test_client_config());
        let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await.expect("create_endpoint failed");
        let conn = endpoint
            .connect(RemoteSpec { addr: server_addr, server_name: "quicmux-test.local".to_string(), cert_sha256_hex })
            .await
            .expect("connect failed");
        assert_eq!(conn.remote_addr(), Some(server_addr));

        let mut stream = conn.open_bi().await.expect("open_bi failed");
        stream.write_all(b"hello quicmux qmux listener").await.expect("write failed");
        stream.shutdown().await.expect("shutdown failed");

        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.expect("read failed");
        assert_eq!(&buf[..n], b"hello quicmux qmux listener");

        conn.close().await;
    }
}
