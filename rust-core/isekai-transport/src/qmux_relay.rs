//! `QmuxQuicEndpointFactory`: a `QuicEndpointFactory` backed by `qmux`
//! (draft-ietf-quic-qmux — QUIC's stream API polyfilled over
//! TLS-over-TCP instead of raw UDP+QUIC packets), for reaching a
//! relay-assigned `isekai-helper` endpoint from a network that blocks
//! outbound UDP (`#qmux-leg1`). Opt-in via the `qmux-relay` feature (off by
//! default — CLAUDE.md's opportunistic/opt-in-by-default principle, and
//! `qmux`'s pre-1.0 API churn risk).
//!
//! Mirrors [`system::SystemQuicEndpointFactory`]'s structure and reuses its
//! [`system::PinnedCertVerifier`] (same cert-pinning contract: `isekai-helper`
//! presents an ephemeral self-signed cert, verified by SHA-256 fingerprint,
//! not a CA chain) — the notable differences accounted for here:
//!
//! - `qmux::Session::connect` takes ownership of (and internally
//!   `tokio::io::split`s) whatever `Transport` it's given, so there is no
//!   way to retrieve a live handle to the underlying `rustls` connection
//!   *after* handing it off. `compute_proof` (`proof.rs`) only ever calls
//!   [`traits::QuicConnection::export_keying_material`] with one fixed
//!   `(label, context)` pair (`EXPORTER_LABEL`, `b""`) — confirmed by
//!   reading every call site in this workspace — so this drives the TLS
//!   handshake manually (`tokio_rustls::TlsConnector`, not `qmux::tls::Client`)
//!   and captures that single export immediately after the handshake
//!   completes, before handing the stream to `qmux::transport::Stream::new`.
//!   [`QmuxQuicConnection::export_keying_material`] serves that cached value
//!   for the one pair it was captured for and fails closed
//!   ([`TransportError::Unsupported`]) for any other — there is no way to
//!   honor a different request after the fact, so this makes that limitation
//!   loud rather than silently wrong.
//! - **ALPN is unverified against the real relay.** `H3_QMUX_ALPN` in
//!   `isekai-link-masque`'s `relay_client.rs` (`#qmux-leg2`) is this leg's
//!   own H3-carrying counterpart, negotiated for the relay's own MASQUE
//!   registration channel — this leg is a plain ATTACH-protocol connection
//!   (no H3 at all), so it cannot reuse that token. [`QMUX_ALPN`] is a
//!   placeholder derived from `isekai_protocol::hello::ALPN`
//!   (`"isekai-pipe/1"`) following draft-ietf-quic-qmux §8.1's own rule that
//!   each application-protocol mapping needs a distinct ALPN when carried
//!   over QMux — pin the `qmux` crate version and confirm this against
//!   `isekai-link-server`'s actual QMux ingress before relying on it in
//!   production (mirrors the same caveat already recorded for `#qmux-leg2`).
//! - No [`traits::QuicEndpointRebinder`] support — `rebinder()` stays the
//!   trait's default `None`. Rebinding a *TCP* connection onto a new local
//!   address isn't the same operation `system::SystemQuicEndpointRebinder`
//!   performs (that's a `noq::Endpoint::rebind` UDP-socket swap on an
//!   otherwise-unbroken QUIC connection); a QMux/TCP equivalent would mean
//!   tearing down and re-establishing the TCP+TLS+QMux session entirely,
//!   which is just a fresh `connect()` call, not a distinct primitive.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use isekai_protocol::hello::EXPORTER_LABEL;

use crate::error::TransportError;
use crate::system::PinnedCertVerifier;
use crate::traits::{
    ByteStream, ByteStreamReadHalf, ByteStreamWriteHalf, QuicConnection, QuicEndpoint, QuicEndpointFactory,
};
use crate::types::{BindSpec, RemoteSpec};

/// See this module's docs — unverified against the real relay.
pub const QMUX_ALPN: &[u8] = b"isekai-pipe/1+qmux01";

/// A `QuicEndpointFactory` backed by `qmux`. Stateless (like
/// `system::SystemQuicEndpointFactory`) — every `create_endpoint` call just
/// records the requested local bind address; the actual TCP connect + TLS
/// handshake + QMux session happens lazily in
/// [`QmuxQuicEndpoint::connect`].
#[derive(Debug, Default, Clone, Copy)]
pub struct QmuxQuicEndpointFactory;

#[async_trait]
impl QuicEndpointFactory for QmuxQuicEndpointFactory {
    async fn create_endpoint(&self, bind: BindSpec) -> Result<Box<dyn QuicEndpoint>, TransportError> {
        Ok(Box::new(QmuxQuicEndpoint { local_addr: bind.local_addr }))
    }

    async fn wrap_bound_socket(&self, _socket: tokio::net::UdpSocket) -> Result<Box<dyn QuicEndpoint>, TransportError> {
        Err(TransportError::Unsupported {
            operation: "wrap_bound_socket",
            reason: "QMux runs over TCP, not UDP — there is no bound UDP socket for it to wrap",
        })
    }
}

struct QmuxQuicEndpoint {
    local_addr: std::net::SocketAddr,
}

#[async_trait]
impl QuicEndpoint for QmuxQuicEndpoint {
    async fn connect(&self, remote: RemoteSpec) -> Result<Box<dyn QuicConnection>, TransportError> {
        let socket = tokio::net::TcpSocket::new_v4()
            .map_err(|source| TransportError::Bind { addr: self.local_addr, source })?;
        socket
            .bind(self.local_addr)
            .map_err(|source| TransportError::Bind { addr: self.local_addr, source })?;
        let tcp = socket
            .connect(remote.addr)
            .await
            .map_err(|e| TransportError::ConnectSetup(e.to_string()))?;

        let mismatch = Arc::new(Mutex::new(None));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| TransportError::TlsConfig(e.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier::new(
                remote.cert_sha256_hex.clone(),
                provider,
                mismatch.clone(),
            )))
            .with_no_client_auth();
        crypto.alpn_protocols = vec![QMUX_ALPN.to_vec()];
        // 0-RTT never used (matches `system::client_config_for`'s contract) —
        // not calling anything 0-RTT-related here is what implements that.

        let connector = tokio_rustls::TlsConnector::from(Arc::new(crypto));
        let server_name = rustls::pki_types::ServerName::try_from(remote.server_name.clone())
            .map_err(|e| TransportError::TlsConfig(format!("invalid server_name: {e}")))?;
        let mut tls_stream = connector.connect(server_name, tcp).await.map_err(|e| {
            match mismatch.lock().unwrap().take() {
                Some((expected, got)) => TransportError::CertPinMismatch { expected, got },
                None => TransportError::Handshake(e.to_string()),
            }
        })?;

        // See the module docs: this is the one and only export this
        // connection will ever perform, captured now because
        // `qmux::Session::connect` (below) takes ownership of `tls_stream`.
        let mut exporter = [0u8; 32];
        tls_stream
            .get_mut()
            .1
            .export_keying_material(&mut exporter, EXPORTER_LABEL, None)
            .map_err(|e| TransportError::ExportKeyingMaterial(e.to_string()))?;

        let config = qmux::Config::new(qmux::Version::QMux01);
        let transport = qmux::transport::Stream::new(tls_stream, config.version, config.max_record_size);
        let session = qmux::Session::connect(transport, config)
            .await
            .map_err(|e| TransportError::Handshake(format!("QMux handshake failed: {e}")))?;

        Ok(Box::new(QmuxQuicConnection { session, exporter_label: EXPORTER_LABEL, exporter }))
    }
}

struct QmuxQuicConnection {
    session: qmux::Session,
    /// The `label` [`export_keying_material`] was actually captured for —
    /// see the module docs for why only this exact `(label, context=b"")`
    /// pair can ever be served.
    exporter_label: &'static [u8],
    exporter: [u8; 32],
}

#[async_trait]
impl QuicConnection for QmuxQuicConnection {
    async fn open_bi(&self) -> Result<Box<dyn ByteStream>, TransportError> {
        use web_transport_trait::Session as _;
        let (send, recv) = self.session.open_bi().await.map_err(|e| TransportError::OpenStream(e.to_string()))?;
        // `qmux::Session` closes the whole connection once its *last handle*
        // is dropped ("Closes the connection once the last Session handle is
        // dropped" — its own doc comment); `SendStream`/`RecvStream` don't
        // independently keep it alive. `relay.rs::connect_via_relay` drops
        // its `QuicConnection` (this struct) as soon as it has the returned
        // `ByteStream` in hand, so without holding a clone here the
        // connection would tear itself down out from under an otherwise
        // still-open, still-in-use stream.
        Ok(Box::new(QmuxByteStream { send, recv, _session: self.session.clone() }))
    }

    async fn close(&self) {
        use web_transport_trait::Session as _;
        self.session.close(0, "");
    }

    async fn export_keying_material(&self, label: &[u8], context: &[u8]) -> Result<[u8; 32], TransportError> {
        keying_material_for(self.exporter_label, self.exporter, label, context)
    }
}

/// The fail-closed decision behind [`QmuxQuicConnection::export_keying_material`]:
/// only the exact `(label, context)` pair captured at connect time can ever be
/// served (see the module docs for why `qmux::Session::connect` makes any
/// other export impossible after the fact). Split out from the trait method
/// so it can be unit-tested without a live `qmux::Session`.
fn keying_material_for(
    captured_label: &'static [u8],
    captured_material: [u8; 32],
    requested_label: &[u8],
    requested_context: &[u8],
) -> Result<[u8; 32], TransportError> {
    if requested_label == captured_label && requested_context.is_empty() {
        Ok(captured_material)
    } else {
        Err(TransportError::Unsupported {
            operation: "export_keying_material",
            reason: "qmux_relay only supports the single (label, context) pair captured at connect time \
                      (qmux::Session::connect takes ownership of the TLS stream, so no later export is possible)",
        })
    }
}

struct QmuxByteStream {
    send: qmux::SendStream,
    recv: qmux::RecvStream,
    /// Keeps the underlying `qmux::Session` alive — see `open_bi`'s comment.
    _session: qmux::Session,
}

#[async_trait]
impl ByteStream for QmuxByteStream {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        use web_transport_trait::RecvStream as _;
        match self.recv.read(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))? {
            Some(n) => Ok(n),
            None => Ok(0), // stream finished cleanly (EOF)
        }
    }

    async fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError> {
        use web_transport_trait::SendStream as _;
        self.send.write_all(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))
    }

    async fn shutdown(&mut self) -> Result<(), TransportError> {
        use web_transport_trait::SendStream as _;
        self.send.finish().map_err(|e| TransportError::StreamIo(e.to_string()))
    }

    fn split(self: Box<Self>) -> (Box<dyn ByteStreamReadHalf>, Box<dyn ByteStreamWriteHalf>) {
        let QmuxByteStream { send, recv, _session } = *self;
        (
            Box::new(QmuxByteStreamReadHalf { recv, _session: _session.clone() }),
            Box::new(QmuxByteStreamWriteHalf { send, _session }),
        )
    }
}

struct QmuxByteStreamReadHalf {
    recv: qmux::RecvStream,
    /// Keeps the underlying `qmux::Session` alive — see `QmuxQuicConnection::open_bi`'s comment.
    _session: qmux::Session,
}

#[async_trait]
impl ByteStreamReadHalf for QmuxByteStreamReadHalf {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        use web_transport_trait::RecvStream as _;
        match self.recv.read(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))? {
            Some(n) => Ok(n),
            None => Ok(0),
        }
    }
}

struct QmuxByteStreamWriteHalf {
    send: qmux::SendStream,
    /// Keeps the underlying `qmux::Session` alive — see `QmuxQuicConnection::open_bi`'s comment.
    _session: qmux::Session,
}

#[async_trait]
impl ByteStreamWriteHalf for QmuxByteStreamWriteHalf {
    async fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError> {
        use web_transport_trait::SendStream as _;
        self.send.write_all(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))
    }

    async fn shutdown(&mut self) -> Result<(), TransportError> {
        use web_transport_trait::SendStream as _;
        self.send.finish().map_err(|e| TransportError::StreamIo(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURED_MATERIAL: [u8; 32] = [7u8; 32];

    #[test]
    fn returns_the_captured_material_for_the_exact_label_and_empty_context() {
        let result = keying_material_for(EXPORTER_LABEL, CAPTURED_MATERIAL, EXPORTER_LABEL, b"");

        assert_eq!(result.unwrap(), CAPTURED_MATERIAL);
    }

    #[test]
    fn fails_closed_on_a_different_label() {
        let result = keying_material_for(EXPORTER_LABEL, CAPTURED_MATERIAL, b"some-other-label", b"");

        assert!(matches!(result, Err(TransportError::Unsupported { operation: "export_keying_material", .. })));
    }

    #[test]
    fn fails_closed_on_a_non_empty_context() {
        let result = keying_material_for(EXPORTER_LABEL, CAPTURED_MATERIAL, EXPORTER_LABEL, b"nonempty");

        assert!(matches!(result, Err(TransportError::Unsupported { operation: "export_keying_material", .. })));
    }

    #[test]
    fn fails_closed_on_both_a_different_label_and_a_non_empty_context() {
        let result = keying_material_for(EXPORTER_LABEL, CAPTURED_MATERIAL, b"some-other-label", b"nonempty");

        assert!(matches!(result, Err(TransportError::Unsupported { operation: "export_keying_material", .. })));
    }
}
