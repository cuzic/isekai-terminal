//! `noq` (quinn's multipath fork, UDP-based) backend for `quicmux`'s mux
//! abstraction. Built directly on `noq` + `rustls`, mirroring
//! `isekai-transport`'s original `system::SystemQuicEndpointFactory`/
//! `system::SystemQuicEndpoint`/`system::SystemQuicConnection`/
//! `system::SystemByteStream` before they moved here.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use noq::crypto::rustls::{QuicClientConfig, QuicServerConfig};

use crate::cert::{CertMismatchSlot, PinnedCertVerifier};
use crate::config::{MuxClientConfig, MuxServerConfig};
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

/// Builds a `noq::ServerConfig` from `config`'s certificate/ALPN/idle-timeout/
/// keepalive/stream-limit/multipath tuning — the server-side counterpart to
/// [`noq_client_config`]. `pub` for the same reason: a caller driving its own
/// `noq::Endpoint` directly (`isekai-pipe serve`'s STUN/relay-socket setup,
/// which must construct the endpoint itself to get at the raw socket first)
/// can still get the identical TLS/transport setup instead of keeping its
/// own near-identical copy.
pub fn noq_server_config(config: &MuxServerConfig) -> Result<noq::ServerConfig, MuxError> {
    // Explicit provider — matches `noq_client_config`'s identical choice, so
    // this function never depends on a process-wide default having been
    // installed via `CryptoProvider::install_default()` somewhere else in
    // the caller's binary (isekai-pipe's original `serve` command, which
    // this was ported from, happened to rely on such an ambient install; a
    // standalone caller of this function should not have to replicate that).
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server_crypto = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| MuxError::TlsConfig(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(config.cert_chain.clone(), config.private_key.clone_key())
        .map_err(|e| MuxError::TlsConfig(e.to_string()))?;
    server_crypto.alpn_protocols = vec![config.alpn.clone()];
    // 0-RTT / early data is never used by this crate on either side — see
    // `MuxClientConfig`'s docs. Explicit here (not just "absence of opt-in")
    // because `rustls::ServerConfig` otherwise leaves this at its own
    // default, and a future rustls version changing that default should not
    // silently change this crate's behavior.
    server_crypto.max_early_data_size = 0;

    let quic_crypto =
        QuicServerConfig::try_from(server_crypto).map_err(|e| MuxError::TlsConfig(format!("QUIC server crypto config failed: {e}")))?;

    let mut transport = noq::TransportConfig::default();
    transport.max_concurrent_bidi_streams(noq::VarInt::from_u32(config.max_concurrent_bidi_streams));
    transport.max_concurrent_uni_streams(noq::VarInt::from_u32(config.max_concurrent_uni_streams));
    transport.max_idle_timeout(Some(
        noq::IdleTimeout::try_from(config.max_idle_timeout).map_err(|e| MuxError::TlsConfig(e.to_string()))?,
    ));
    transport.keep_alive_interval(Some(config.keep_alive_interval));
    if config.multipath {
        transport.max_concurrent_multipath_paths(8);
    }

    let mut server_config = noq::ServerConfig::with_crypto(Arc::new(quic_crypto));
    server_config.transport_config(Arc::new(transport));
    Ok(server_config)
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

/// A `noq`-backed [`crate::AnyMuxListener`] variant — a bound endpoint
/// accepting inbound connections, built from either a fresh local bind
/// ([`NoqListener::bind`]) or an already-bound socket
/// ([`NoqListener::wrap_bound_socket`], for a caller that must perform its
/// own raw I/O on a specific socket — a STUN query and hole-punch probes, or
/// an inbound relay tunnel socket — before handing it to this crate, exactly
/// like [`NoqFactory::wrap_bound_socket`]'s client-side equivalent).
pub struct NoqListener {
    endpoint: noq::Endpoint,
}

impl NoqListener {
    pub(crate) async fn bind(config: MuxServerConfig, bind: BindSpec) -> Result<Self, MuxError> {
        let server_config = noq_server_config(&config)?;
        let endpoint = noq::Endpoint::server(server_config, bind.local_addr).map_err(|e| MuxError::EndpointSetup(e.to_string()))?;
        Ok(Self { endpoint })
    }

    pub(crate) async fn wrap_bound_socket(config: MuxServerConfig, socket: tokio::net::UdpSocket) -> Result<Self, MuxError> {
        let std_socket = socket.into_std().map_err(|e| MuxError::SocketSetup(e.to_string()))?;
        let async_socket = default_socket_adapter()(std_socket).map_err(|e| MuxError::SocketSetup(e.to_string()))?;
        Self::from_abstract_socket(config, async_socket)
    }

    /// Wraps an already-adapted `Box<dyn noq::AsyncUdpSocket>` as a listener
    /// directly, bypassing the plain-`tokio::net::UdpSocket` path entirely —
    /// the listener-side counterpart to
    /// [`crate::noq_backend::endpoint_from_abstract_socket`] (client side),
    /// for a caller whose socket isn't a plain UDP socket at all (e.g.
    /// `isekai-pipe serve`'s `--relay` mode, which tunnels through a MASQUE
    /// relay via its own custom `noq::AsyncUdpSocket` implementation — there
    /// is no `tokio::net::UdpSocket` anywhere in that path to convert).
    pub(crate) fn from_abstract_socket(config: MuxServerConfig, socket: Box<dyn noq::AsyncUdpSocket>) -> Result<Self, MuxError> {
        let server_config = noq_server_config(&config)?;
        let endpoint = noq::Endpoint::new_with_abstract_socket(
            noq::EndpointConfig::default(),
            Some(server_config),
            socket,
            Arc::new(noq::TokioRuntime),
        )
        .map_err(|e| MuxError::EndpointSetup(e.to_string()))?;
        Ok(Self { endpoint })
    }

    /// Waits for the next inbound connection candidate. Returns `None` once
    /// the endpoint has been closed and has no more incoming connections to
    /// deliver — the same "listener is done" signal `noq::Endpoint::accept`
    /// itself returns.
    ///
    /// Deliberately returns [`NoqIncoming`] (a *pending* handshake) rather
    /// than awaiting completion itself: `isekai-pipe serve`'s `--once` relies
    /// on this split (`engine/mod.rs`'s `handle_incoming`/`once` handling) to
    /// synchronously await the *specific* handshake it just decided to
    /// accept before closing the listener — closing right after this method
    /// returns, instead of after the caller awaits the returned
    /// [`NoqIncoming`], would race the listener's own close against the
    /// still-pending handshake and could drop the very connection `--once`
    /// meant to serve.
    pub(crate) async fn accept(&self) -> Option<NoqIncoming> {
        self.endpoint.accept().await.map(|incoming| NoqIncoming { incoming })
    }

    pub(crate) fn local_addr(&self) -> Result<SocketAddr, MuxError> {
        self.endpoint.local_addr().map_err(|e| MuxError::EndpointSetup(e.to_string()))
    }

    /// Requests that the listener (and every connection it produced) be
    /// closed, with `reason` as the application-level close reason each
    /// connected peer observes. Best-effort — does not wait for peers to
    /// acknowledge; see [`NoqListener::wait_idle`] for that.
    pub(crate) fn close(&self, reason: &[u8]) {
        self.endpoint.close(noq::VarInt::from_u32(0), reason);
    }

    /// Waits until every connection this listener produced has finished
    /// closing (after a prior [`NoqListener::close`]) — `isekai-pipe serve`
    /// calls this right before process exit so it doesn't tear down the
    /// process out from under a connection that's still draining its close
    /// handshake.
    pub(crate) async fn wait_idle(&self) {
        self.endpoint.wait_idle().await;
    }
}

/// A connection candidate [`NoqListener::accept`] received, whose handshake
/// has not necessarily completed yet — see that method's docs for why this
/// split (instead of awaiting completion inside `accept` itself) matters.
pub struct NoqIncoming {
    incoming: noq::Incoming,
}

impl NoqIncoming {
    pub(crate) async fn accept(self) -> Result<NoqConnection, MuxError> {
        let conn = self.incoming.await.map_err(map_connection_error)?;
        Ok(NoqConnection { conn })
    }
}

#[derive(Clone)]
pub struct NoqConnection {
    conn: noq::Connection,
}

impl NoqConnection {
    pub(crate) async fn open_bi(&self) -> Result<NoqByteStream, MuxError> {
        let (send, recv) = self.conn.open_bi().await.map_err(map_connection_error)?;
        Ok(NoqByteStream { send, recv })
    }

    /// Accepts a new bidirectional stream the peer opened. Server-side
    /// counterpart to [`NoqConnection::open_bi`] — but symmetric, not
    /// direction-restricted: a client-side `NoqConnection` can call this too
    /// (e.g. to accept a control stream the server opened back), exactly
    /// like `noq::Connection::accept_bi` itself has no client/server
    /// distinction once the handshake is done.
    pub(crate) async fn accept_bi(&self) -> Result<NoqByteStream, MuxError> {
        let (send, recv) = self.conn.accept_bi().await.map_err(map_connection_error)?;
        Ok(NoqByteStream { send, recv })
    }

    pub(crate) async fn close(&self) {
        self.conn.close(noq::VarInt::from_u32(0), b"");
    }

    /// Best-effort remote address, read from path 0 (`noq::PathId::ZERO`,
    /// which always exists — see `isekai-pipe/src/engine/mod.rs`'s identical
    /// comment on why path 0 specifically is the right choice once
    /// multipath may be in play: with multipath enabled, later paths can
    /// each have their own distinct remote address, so there is no single
    /// "the" remote address to report in general, but path 0 is always
    /// present and is the one a log line or diagnostic wants).
    pub(crate) fn remote_addr(&self) -> Option<SocketAddr> {
        self.conn.path(noq::PathId::ZERO).and_then(|p| p.remote_address().ok())
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

    /// Waits until the peer has either fully received this stream's data
    /// (acknowledged the `finish()`) or explicitly stopped reading it — see
    /// [`crate::AnyByteStream::wait_for_close`]'s docs for why a caller needs
    /// this. `noq::SendStream::stopped()` actually distinguishes those two
    /// outcomes (`Some(code)` for an explicit stop, `None` for a plain
    /// finish-ack) and callers of `isekai-pipe`'s original `reject()` never
    /// used that distinction — collapsed to `()` here to match the coarser
    /// guarantee `qmux`'s `SendStream::closed()` can actually make.
    pub(crate) async fn wait_for_close(&self) -> Result<(), MuxError> {
        self.send.stopped().await.map(|_| ()).map_err(|e| map_write_error(e.into()))
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

    /// See [`NoqByteStream::wait_for_close`].
    pub(crate) async fn wait_for_close(&self) -> Result<(), MuxError> {
        self.send.stopped().await.map(|_| ()).map_err(|e| map_write_error(e.into()))
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

    /// Built on [`NoqListener`] (this crate's own production accept API,
    /// not a hand-rolled `noq::Endpoint::server` copy) so this helper can't
    /// drift from what real callers actually use. Previously duplicated the
    /// setup with a bare `rustls::ServerConfig::builder()` (no explicit
    /// provider) — harmless when only the `noq` feature was compiled in
    /// (a single crypto provider is unambiguous), but panicked with
    /// "Could not automatically determine the process-level CryptoProvider"
    /// once `qmux`'s transitive deps (which pull in `aws-lc-rs` alongside
    /// this crate's `ring`) were linked into the same test binary too
    /// (`cargo test -p quicmux --features noq,qmux`) — reusing
    /// [`test_server_config`]'s explicit-provider [`noq_server_config`] path
    /// avoids that class of bug entirely, here and in any future caller.
    async fn start_echo_server() -> (SocketAddr, String) {
        let (server_config, cert_sha256_hex) = test_server_config();
        let listener = NoqListener::bind(server_config, BindSpec::any_ipv4()).await.unwrap();
        let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listener.local_addr().unwrap().port());

        tokio::spawn(async move {
            let Some(incoming) = listener.accept().await else { return };
            let Ok(conn) = incoming.accept().await else { return };
            while let Ok(mut stream) = conn.accept_bi().await {
                let mut buf = [0u8; 64];
                if let Ok(n) = stream.read(&mut buf).await {
                    let _ = stream.write_all(&buf[..n]).await;
                }
                let _ = stream.shutdown().await;
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

    /// A fresh self-signed cert/key pair plus a [`MuxServerConfig`] built
    /// from it, and the cert's SHA-256 fingerprint for the client side to
    /// pin against — everything [`NoqListener`]-based tests need.
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
            alpn: test_config().alpn,
            exporter_label: test_config().exporter_label,
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

    /// Runs one accept→echo round on `listener` and then returns — mirrors
    /// `isekai-pipe serve`'s `handle_incoming`/`accept_bi` loop shape closely
    /// enough to exercise the same API surface (`accept`/`NoqIncoming::accept`/
    /// `accept_bi`) without pulling in any isekai-specific framing.
    async fn spawn_echo_once(listener: NoqListener) {
        tokio::spawn(async move {
            let Some(incoming) = listener.accept().await else { return };
            let Ok(conn) = incoming.accept().await else { return };
            let Ok(mut stream) = conn.accept_bi().await else { return };
            let mut buf = [0u8; 64];
            if let Ok(n) = stream.read(&mut buf).await {
                let _ = stream.write_all(&buf[..n]).await;
            }
            let _ = stream.shutdown().await;
            // `wait_for_close()` — not an immediate drop of `stream`/`conn`/
            // `listener` — is this crate's real fix for the race this test
            // originally caught (see `AnyByteStream::wait_for_close`'s
            // docs): dropping the sole `noq::Connection` handle right after
            // a stream `finish()`, before the peer has actually *received*
            // the data (only requested it), tears the whole connection down
            // mid-flight and can race the client's `read()` into seeing
            // `PeerClosed` instead of the echoed bytes (reproduced directly
            // here before this call was added). `isekai-pipe serve`'s real
            // `reject()` hits the identical race and fixes it the same way.
            let _ = stream.wait_for_close().await;
        });
    }

    #[tokio::test]
    async fn listener_bind_accept_bi_write_and_read_roundtrip() {
        let (server_config, cert_sha256_hex) = test_server_config();
        let listener = NoqListener::bind(server_config, BindSpec::any_ipv4()).await.expect("listener bind failed");
        let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listener.local_addr().unwrap().port());
        spawn_echo_once(listener).await;

        let factory = NoqFactory::new(test_config());
        let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await.expect("create_endpoint failed");
        let conn = endpoint
            .connect(RemoteSpec { addr: server_addr, server_name: "quicmux-test.local".to_string(), cert_sha256_hex })
            .await
            .expect("connect failed");

        let mut stream = conn.open_bi().await.expect("open_bi failed");
        stream.write_all(b"hello quicmux noq listener").await.expect("write failed");
        stream.shutdown().await.expect("shutdown failed");

        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.expect("read failed");
        assert_eq!(&buf[..n], b"hello quicmux noq listener");

        conn.close().await;
    }

    #[tokio::test]
    async fn listener_wrap_bound_socket_roundtrip() {
        // Mirrors `isekai-pipe serve`'s STUN/hole-punch path: bind a raw
        // `std::net::UdpSocket` first (where a caller would run its own I/O
        // on it — a STUN query, punch probes — before this crate ever sees
        // it), then hand it to the listener instead of letting it bind fresh.
        let std_socket = std::net::UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)).unwrap();
        std_socket.set_nonblocking(true).unwrap();
        let raw_socket = tokio::net::UdpSocket::from_std(std_socket).unwrap();

        let (server_config, cert_sha256_hex) = test_server_config();
        let listener = NoqListener::wrap_bound_socket(server_config, raw_socket).await.expect("wrap_bound_socket failed");
        let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listener.local_addr().unwrap().port());
        spawn_echo_once(listener).await;

        let factory = NoqFactory::new(test_config());
        let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await.expect("create_endpoint failed");
        let conn = endpoint
            .connect(RemoteSpec { addr: server_addr, server_name: "quicmux-test.local".to_string(), cert_sha256_hex })
            .await
            .expect("connect failed");

        let mut stream = conn.open_bi().await.expect("open_bi failed");
        stream.write_all(b"hello via wrapped socket").await.expect("write failed");
        stream.shutdown().await.expect("shutdown failed");

        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.expect("read failed");
        assert_eq!(&buf[..n], b"hello via wrapped socket");

        conn.close().await;
    }

    #[tokio::test]
    async fn connection_remote_addr_matches_client_local_addr() {
        let (server_config, cert_sha256_hex) = test_server_config();
        let listener = NoqListener::bind(server_config, BindSpec::any_ipv4()).await.expect("listener bind failed");
        let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listener.local_addr().unwrap().port());

        let (server_remote_addr_tx, server_remote_addr_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Some(incoming) = listener.accept().await else { return };
            let Ok(conn) = incoming.accept().await else { return };
            let _ = server_remote_addr_tx.send(conn.remote_addr());
        });

        let factory = NoqFactory::new(test_config());
        let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await.expect("create_endpoint failed");
        let client_local_port = endpoint.endpoint.local_addr().unwrap().port();
        let conn = endpoint
            .connect(RemoteSpec { addr: server_addr, server_name: "quicmux-test.local".to_string(), cert_sha256_hex })
            .await
            .expect("connect failed");

        let server_observed = server_remote_addr_rx.await.expect("server task didn't report remote_addr").expect("remote_addr was None");
        assert_eq!(server_observed.port(), client_local_port);
        conn.close().await;
    }
}
