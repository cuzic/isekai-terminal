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

use std::sync::{Arc, Mutex};

use crate::cert::PinnedCertVerifier;
use crate::config::MuxClientConfig;
use crate::error::MuxError;
use crate::types::RemoteSpec;

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

        Ok(QmuxConnection { session, exporter_label: self.config.exporter_label.clone(), exporter })
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

    pub(crate) async fn close(&self) {
        use web_transport_trait::Session as _;
        self.session.close(0, "");
    }

    pub(crate) async fn export_keying_material(&self, label: &[u8], context: &[u8]) -> Result<[u8; 32], MuxError> {
        keying_material_for(&self.exporter_label, self.exporter, label, context)
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
