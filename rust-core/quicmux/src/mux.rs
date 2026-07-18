//! The backend-agnostic mux abstraction itself: [`AnyMuxFactory`] →
//! [`AnyMuxEndpoint`] → [`AnyMuxConnection`] → [`AnyByteStream`], plus
//! [`AnyMuxRebinder`]. Each is a plain enum over the compiled-in backends
//! (`noq`/`qmux`, both cargo features), not a trait object — callers match
//! on or call inherent methods on a concrete, sized type instead of going
//! through a vtable. See this crate's top-level docs for why: the whole
//! point of extracting `quicmux` was to stop `isekai-transport` from having
//! to hand-roll backend selection behind `dyn Trait`, and an enum with a
//! fixed, small set of variants is simpler than an object-safe trait
//! hierarchy for a "exactly N backends, chosen once at startup" shape like
//! this one.
//!
//! Each enum's variants are `#[cfg(feature = "...")]`-gated on the
//! corresponding backend feature, so a build with only one backend enabled
//! never even compiles the other's variant — not just skips constructing it.

use crate::config::{MuxClientConfig, MuxServerConfig};
use crate::error::MuxError;
use crate::types::{BindSpec, RemoteSpec};

/// Creates [`AnyMuxEndpoint`]s. The `noq`/`qmux`-backed constructors
/// ([`AnyMuxFactory::noq`]/[`AnyMuxFactory::noq_with_socket_adapter`]/
/// [`AnyMuxFactory::qmux`]) are the only way to obtain one — there is no
/// "default" backend at this layer; the caller picks.
#[derive(Clone)]
pub enum AnyMuxFactory {
    #[cfg(feature = "noq")]
    Noq(crate::noq_backend::NoqFactory),
    #[cfg(feature = "qmux")]
    Qmux(crate::qmux_backend::QmuxFactory),
}

impl AnyMuxFactory {
    /// A factory backed by the `noq` engine, binding/wrapping plain
    /// `tokio::net::UdpSocket`s.
    #[cfg(feature = "noq")]
    pub fn noq(config: MuxClientConfig) -> Self {
        Self::Noq(crate::noq_backend::NoqFactory::new(config))
    }

    /// A factory backed by the `noq` engine, adapting every socket it
    /// binds/wraps through `adapter` before handing it to the underlying
    /// engine — see [`crate::noq_backend::NoqFactory::with_socket_adapter`].
    #[cfg(feature = "noq")]
    pub fn noq_with_socket_adapter(config: MuxClientConfig, adapter: crate::noq_backend::AsyncUdpSocketAdapter) -> Self {
        Self::Noq(crate::noq_backend::NoqFactory::with_socket_adapter(config, adapter))
    }

    /// A factory backed by the `qmux` engine (QUIC-over-TLS-over-TCP).
    #[cfg(feature = "qmux")]
    pub fn qmux(config: MuxClientConfig) -> Self {
        Self::Qmux(crate::qmux_backend::QmuxFactory::new(config))
    }

    /// Binds a fresh local socket at `bind` and returns an endpoint capable
    /// of dialing outbound connections through it.
    pub async fn create_endpoint(&self, bind: BindSpec) -> Result<AnyMuxEndpoint, MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(factory) => Ok(AnyMuxEndpoint::Noq(factory.create_endpoint(bind).await?)),
            #[cfg(feature = "qmux")]
            Self::Qmux(factory) => Ok(AnyMuxEndpoint::Qmux(factory.create_endpoint(bind).await?)),
        }
    }

    /// Wraps an already-bound `tokio::net::UdpSocket` as an endpoint,
    /// instead of binding a fresh one via
    /// [`AnyMuxFactory::create_endpoint`] — for a caller that must perform
    /// its own raw I/O on a specific socket (e.g. a STUN query and
    /// hole-punch probes) *before* handing it to this crate. Fails with
    /// [`MuxError::Unsupported`] on a backend with no UDP-socket concept
    /// (`qmux`).
    pub async fn wrap_bound_socket(&self, socket: tokio::net::UdpSocket) -> Result<AnyMuxEndpoint, MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(factory) => Ok(AnyMuxEndpoint::Noq(factory.wrap_bound_socket(socket).await?)),
            #[cfg(feature = "qmux")]
            Self::Qmux(factory) => Ok(AnyMuxEndpoint::Qmux(factory.wrap_bound_socket(socket).await?)),
        }
    }
}

/// A bound mux endpoint, capable of initiating outbound connections.
pub enum AnyMuxEndpoint {
    #[cfg(feature = "noq")]
    Noq(crate::noq_backend::NoqEndpoint),
    #[cfg(feature = "qmux")]
    Qmux(crate::qmux_backend::QmuxEndpoint),
}

impl AnyMuxEndpoint {
    pub async fn connect(&self, remote: RemoteSpec) -> Result<AnyMuxConnection, MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(endpoint) => Ok(AnyMuxConnection::Noq(endpoint.connect(remote).await?)),
            #[cfg(feature = "qmux")]
            Self::Qmux(endpoint) => Ok(AnyMuxConnection::Qmux(endpoint.connect(remote).await?)),
        }
    }

    /// Returns a handle that can later switch this endpoint's local socket
    /// without tearing down any connection made through it, if the backend
    /// supports that (only `noq` does today — see [`AnyMuxRebinder`]'s
    /// docs). `None` for a backend with no such capability, not an error:
    /// unlike `connect`/`open_bi`/etc., rebinding is deliberately not
    /// something every backend needs to support.
    pub fn rebinder(&self) -> Option<AnyMuxRebinder> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(endpoint) => Some(AnyMuxRebinder::Noq(endpoint.rebinder())),
            #[cfg(feature = "qmux")]
            Self::Qmux(_) => None,
        }
    }
}

/// A handle that can switch its [`AnyMuxEndpoint`]'s local socket in place,
/// without disturbing any connection already established through it. Only
/// the `noq` backend ever produces one (see [`AnyMuxEndpoint::rebinder`]) —
/// rebinding a TCP-based connection (`qmux`) isn't the same kind of
/// operation; there is nothing to migrate in place, only a fresh `connect()`.
pub enum AnyMuxRebinder {
    #[cfg(feature = "noq")]
    Noq(crate::noq_backend::NoqRebinder),
}

impl AnyMuxRebinder {
    /// Switches the endpoint to `socket` directly, instead of binding a
    /// fresh one itself. A successful return means the switch itself
    /// succeeded — it does **not** mean the new socket can actually reach
    /// the peer (that can only be learned by observing whether the
    /// connection keeps working afterward). On failure, the endpoint keeps
    /// using its previous socket.
    pub async fn rebind_socket(&self, socket: std::net::UdpSocket) -> Result<(), MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(rebinder) => rebinder.rebind_socket(socket).await,
            // `AnyMuxRebinder` currently has no variant besides `Noq` — this
            // arm only exists so the match stays exhaustive in a build where
            // the `noq` feature is disabled (in which case `AnyMuxRebinder`
            // has zero variants and no value of this type can ever actually
            // reach here; `&AnyMuxRebinder` is still "inhabited" from the
            // exhaustiveness checker's point of view, since a reference to
            // an uninhabited type is not itself uninhabited).
            #[allow(unreachable_patterns)]
            _ => unreachable!("AnyMuxRebinder can only be constructed by a compiled-in backend that supports rebinding"),
        }
    }

    /// Binds a fresh local socket at `bind` (an ordinary, OS-routed bind —
    /// no specific physical interface) and switches to it via
    /// [`AnyMuxRebinder::rebind_socket`]. The common case; reach for
    /// `rebind_socket` directly for the physical-interface case (bind the
    /// socket to a specific interface yourself first).
    pub async fn rebind(&self, bind: BindSpec) -> Result<(), MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(rebinder) => rebinder.rebind(bind).await,
            #[allow(unreachable_patterns)]
            _ => unreachable!("AnyMuxRebinder can only be constructed by a compiled-in backend that supports rebinding"),
        }
    }
}

/// A bound mux listener, capable of accepting inbound connections — the
/// server-side counterpart to [`AnyMuxEndpoint`].
pub enum AnyMuxListener {
    #[cfg(feature = "noq")]
    Noq(crate::noq_backend::NoqListener),
    #[cfg(feature = "qmux")]
    Qmux(crate::qmux_backend::QmuxListener),
}

impl AnyMuxListener {
    /// Binds a fresh local UDP socket at `bind` and listens for inbound
    /// `noq` connections.
    #[cfg(feature = "noq")]
    pub async fn bind_noq(config: MuxServerConfig, bind: BindSpec) -> Result<Self, MuxError> {
        Ok(Self::Noq(crate::noq_backend::NoqListener::bind(config, bind).await?))
    }

    /// Wraps an already-bound `tokio::net::UdpSocket` as a `noq`-backed
    /// listener, instead of binding a fresh one via [`AnyMuxListener::bind_noq`]
    /// — for a caller that must perform its own raw I/O on a specific socket
    /// (a STUN query and hole-punch probes, or an inbound relay tunnel
    /// socket) before handing it to this crate, mirroring
    /// [`AnyMuxFactory::wrap_bound_socket`]'s client-side equivalent.
    #[cfg(feature = "noq")]
    pub async fn wrap_bound_socket_noq(config: MuxServerConfig, socket: tokio::net::UdpSocket) -> Result<Self, MuxError> {
        Ok(Self::Noq(crate::noq_backend::NoqListener::wrap_bound_socket(config, socket).await?))
    }

    /// Wraps an already-adapted `Box<dyn noq::AsyncUdpSocket>` as a listener
    /// directly, for a caller whose socket isn't a plain
    /// `tokio::net::UdpSocket` at all — see
    /// [`crate::noq_backend::NoqListener::from_abstract_socket`]'s docs
    /// (e.g. `isekai-pipe serve`'s `--relay` MASQUE-tunnel socket).
    #[cfg(feature = "noq")]
    pub fn from_abstract_socket_noq(config: MuxServerConfig, socket: Box<dyn noq::AsyncUdpSocket>) -> Result<Self, MuxError> {
        Ok(Self::Noq(crate::noq_backend::NoqListener::from_abstract_socket(config, socket)?))
    }

    /// Binds a fresh local TCP socket at `bind` and listens for inbound
    /// `qmux` connections. No `wrap_bound_socket`-style counterpart — unlike
    /// `noq`'s UDP-socket-then-STUN-then-hand-off pattern, nothing in this
    /// crate's callers needs to run raw I/O on the listening TCP socket
    /// before this crate takes it over.
    #[cfg(feature = "qmux")]
    pub async fn bind_qmux(config: MuxServerConfig, bind: BindSpec) -> Result<Self, MuxError> {
        Ok(Self::Qmux(crate::qmux_backend::QmuxListener::bind(config, bind).await?))
    }

    /// Waits for the next inbound connection candidate. Returns `None` once
    /// the listener has been closed and has no more incoming connections to
    /// deliver.
    pub async fn accept(&self) -> Option<AnyMuxIncoming> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(listener) => listener.accept().await.map(AnyMuxIncoming::Noq),
            #[cfg(feature = "qmux")]
            Self::Qmux(listener) => listener.accept().await.map(AnyMuxIncoming::Qmux),
        }
    }

    pub fn local_addr(&self) -> Result<std::net::SocketAddr, MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(listener) => listener.local_addr(),
            #[cfg(feature = "qmux")]
            Self::Qmux(listener) => listener.local_addr(),
        }
    }

    /// Requests that the listener stop accepting new connections. Best-
    /// effort, and backend-dependent in scope: the `noq` backend also closes
    /// every connection it already produced, sending `reason` as the
    /// application-level close reason (does not wait for peers to
    /// acknowledge; see [`AnyMuxListener::wait_idle`] for that), while the
    /// `qmux` backend only stops accepting new TCP connections (`reason` is
    /// unused there — a bare TCP listener close has no application-level
    /// close-reason concept) — it has no centralized tracking of connections
    /// it already produced to close (see `qmux_backend`'s module docs).
    pub fn close(&self, reason: &[u8]) {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(listener) => listener.close(reason),
            #[cfg(feature = "qmux")]
            Self::Qmux(listener) => listener.close(),
        }
    }

    /// Waits until every connection this listener produced has finished
    /// closing (after a prior [`AnyMuxListener::close`]) — a no-op for the
    /// `qmux` backend (see [`AnyMuxListener::close`]'s docs on why).
    pub async fn wait_idle(&self) {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(listener) => listener.wait_idle().await,
            #[cfg(feature = "qmux")]
            Self::Qmux(listener) => listener.wait_idle().await,
        }
    }
}

/// A connection candidate [`AnyMuxListener::accept`] received, whose
/// handshake has not necessarily completed yet — split out from `accept`
/// itself (instead of `accept` awaiting completion directly) so a caller
/// that needs to synchronously wait for one *specific* accepted candidate's
/// handshake before doing anything else (e.g. `isekai-pipe serve`'s `--once`
/// flag, which must not close the listener until the one connection it
/// decided to accept has actually finished handshaking — closing right after
/// `accept()` returns instead would race the listener's own shutdown against
/// the still-pending handshake) can do so without an extra channel/task.
pub enum AnyMuxIncoming {
    #[cfg(feature = "noq")]
    Noq(crate::noq_backend::NoqIncoming),
    #[cfg(feature = "qmux")]
    Qmux(crate::qmux_backend::QmuxIncoming),
}

impl AnyMuxIncoming {
    /// Awaits handshake completion, yielding the established connection.
    pub async fn accept(self) -> Result<AnyMuxConnection, MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(incoming) => Ok(AnyMuxConnection::Noq(incoming.accept().await?)),
            #[cfg(feature = "qmux")]
            Self::Qmux(incoming) => Ok(AnyMuxConnection::Qmux(incoming.accept().await?)),
        }
    }
}

/// An established mux connection. `Clone` — both backing types
/// (`noq::Connection`/`qmux::Session`) are themselves cheap `Clone` handles
/// onto shared state, not owners of a background task that dies with one
/// particular value (see each's own doc comment), so a caller that needs to
/// hand a second handle to a spawned task (e.g. to open a control stream
/// concurrently with driving the main data stream) can just clone this
/// rather than needing an `Arc<AnyMuxConnection>` wrapper of its own.
#[derive(Clone)]
pub enum AnyMuxConnection {
    #[cfg(feature = "noq")]
    Noq(crate::noq_backend::NoqConnection),
    #[cfg(feature = "qmux")]
    Qmux(crate::qmux_backend::QmuxConnection),
}

impl AnyMuxConnection {
    /// Wraps an already-established `noq::Connection` a caller obtained
    /// through its own connect/accept path — e.g. one that drove
    /// `noq::Endpoint`/`noq::Connection::open_path` directly instead of
    /// going through this crate's [`AnyMuxFactory`]/[`AnyMuxEndpoint`]/
    /// [`AnyMuxListener`] (see
    /// [`crate::noq_backend::NoqConnection::from_connection`]'s docs for a
    /// concrete example). Lets that connection be driven the same way as any
    /// other [`AnyMuxConnection`] — including its datagram plane
    /// (`send_datagram`/`recv_datagram`/etc.) — without the caller needing
    /// two separate APIs depending on where the connection came from.
    #[cfg(feature = "noq")]
    pub fn from_noq_connection(conn: noq::Connection) -> Self {
        Self::Noq(crate::noq_backend::NoqConnection::from_connection(conn))
    }

    /// Opens a new bidirectional stream on this connection.
    pub async fn open_bi(&self) -> Result<AnyByteStream, MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(conn) => Ok(AnyByteStream::Noq(conn.open_bi().await?)),
            #[cfg(feature = "qmux")]
            Self::Qmux(conn) => Ok(AnyByteStream::Qmux(conn.open_bi().await?)),
        }
    }

    /// Accepts a new bidirectional stream the peer opened — the
    /// server-accepting-a-connection counterpart to [`AnyMuxConnection::open_bi`].
    /// Not restricted to server-produced connections: nothing about "which
    /// side dialed" stops either peer from accepting a stream the other
    /// opened, once the connection itself is established — a `noq::Connection`
    /// and a `qmux::Session` both work this way, so a client-dialed
    /// [`AnyMuxConnection`] can call this too (e.g. to accept a control
    /// stream the server opens back).
    pub async fn accept_bi(&self) -> Result<AnyByteStream, MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(conn) => Ok(AnyByteStream::Noq(conn.accept_bi().await?)),
            #[cfg(feature = "qmux")]
            Self::Qmux(conn) => Ok(AnyByteStream::Qmux(conn.accept_bi().await?)),
        }
    }

    /// Best-effort remote address of the peer — `None` if the backend has no
    /// stable single address to report. A `noq` connection with multipath
    /// enabled may have a different address per path (this reports path 0's,
    /// which always exists, but that is still not necessarily "the" address
    /// once other paths are live).
    pub fn remote_addr(&self) -> Option<std::net::SocketAddr> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(conn) => conn.remote_addr(),
            #[cfg(feature = "qmux")]
            Self::Qmux(conn) => conn.remote_addr(),
        }
    }

    /// Requests that the connection be closed. Best-effort — does not wait
    /// for the peer to acknowledge.
    pub async fn close(&self) {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(conn) => conn.close().await,
            #[cfg(feature = "qmux")]
            Self::Qmux(conn) => conn.close().await,
        }
    }

    /// Exports keying material from the live TLS session. Always returns
    /// exactly 32 bytes on success.
    pub async fn export_keying_material(&self, label: &[u8], context: &[u8]) -> Result<[u8; 32], MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(conn) => conn.export_keying_material(label, context).await,
            #[cfg(feature = "qmux")]
            Self::Qmux(conn) => conn.export_keying_material(label, context).await,
        }
    }

    /// Sends one datagram (RFC 9221 — unreliable, unordered, never
    /// retransmitted by this crate; see [`crate::MuxClientConfig::datagram_send_buffer_size`]
    /// to enable/tune this per connection). Non-blocking: if the outbound
    /// buffer is full, this crate does not fail the call, but *which*
    /// datagram gets shed differs by backend — `noq` drops its own oldest
    /// still-unsent datagram to make room for this one (newest-wins), while
    /// `qmux` instead drops *this* call's payload and still returns `Ok(())`
    /// (drop-newest). Both are legitimate under RFC 9221 (no datagram is
    /// guaranteed delivery regardless of which one got shed), but a caller
    /// that cares which policy it gets should not assume they match across
    /// backends. Use [`AnyMuxConnection::send_datagram_wait`] for traffic
    /// that must never be silently shed this way.
    pub fn send_datagram(&self, data: bytes::Bytes) -> Result<(), MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(conn) => conn.send_datagram(data),
            #[cfg(feature = "qmux")]
            Self::Qmux(conn) => conn.send_datagram(data),
        }
    }

    /// Backpressure-aware version of [`AnyMuxConnection::send_datagram`]:
    /// waits for outbound buffer space instead of shedding an older datagram
    /// to make room. Only the `noq` backend can actually wait this way —
    /// `qmux`'s underlying `web_transport_trait::Session` has no equivalent
    /// API (see `qmux_backend`'s module docs). On the `qmux` backend this
    /// falls back to the same non-waiting [`AnyMuxConnection::send_datagram`]
    /// and returns immediately, inheriting `qmux`'s drop-newest behavior
    /// instead of ever blocking — a caller whose traffic class truly cannot
    /// tolerate that on `qmux` needs its own higher-level ack/retry, not this
    /// method (see this crate's design notes on datagram resume semantics:
    /// datagrams are never replayed across a resume/reconnect either).
    pub async fn send_datagram_wait(&self, data: bytes::Bytes) -> Result<(), MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(conn) => conn.send_datagram_wait(data).await,
            #[cfg(feature = "qmux")]
            Self::Qmux(conn) => conn.send_datagram(data),
        }
    }

    /// Receives one datagram, waiting for the next one to arrive.
    pub async fn recv_datagram(&self) -> Result<bytes::Bytes, MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(conn) => conn.recv_datagram().await,
            #[cfg(feature = "qmux")]
            Self::Qmux(conn) => conn.recv_datagram().await,
        }
    }

    /// The largest single datagram payload this connection can currently
    /// send, or `None` if datagrams are disabled for this connection
    /// (locally, via [`crate::MuxClientConfig::datagram_send_buffer_size`]
    /// being `None`) or the peer hasn't negotiated willingness to receive
    /// them. Can shrink over the connection's lifetime (path MTU
    /// re-estimation on the `noq` backend) — check before every send rather
    /// than caching.
    pub fn max_datagram_size(&self) -> Option<usize> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(conn) => conn.max_datagram_size(),
            #[cfg(feature = "qmux")]
            Self::Qmux(conn) => conn.max_datagram_size(),
        }
    }

    /// Local outbound datagram buffer space currently free, in bytes. Not a
    /// general network-congestion signal — it only reflects this backend's
    /// own send queue, which can have room even while the path itself is
    /// congested (see this crate's design notes). Always `0` on the `qmux`
    /// backend, which has no equivalent byte-sized buffer of its own (its
    /// outbound queue is bounded by frame count internally — see
    /// `qmux_backend`'s module docs) — not a signal that `qmux`'s buffer is
    /// full, just that it has nothing comparable to report.
    pub fn datagram_send_buffer_space(&self) -> usize {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(conn) => conn.datagram_send_buffer_space(),
            #[cfg(feature = "qmux")]
            Self::Qmux(conn) => conn.datagram_send_buffer_space(),
        }
    }
}

/// One direction-agnostic byte stream — a mux bidirectional stream, from the
/// caller's point of view. Deliberately keeps the same combined read/write/
/// shutdown/split() shape its trait-based ancestor had — not split into
/// separate send/recv types with `finish`/`reset`/`stop` — because nothing
/// in any current caller's protocol uses stream reset today, and that
/// finer-grained API would be speculative generality.
pub enum AnyByteStream {
    #[cfg(feature = "noq")]
    Noq(crate::noq_backend::NoqByteStream),
    #[cfg(feature = "qmux")]
    Qmux(crate::qmux_backend::QmuxByteStream),
}

impl AnyByteStream {
    /// Reads into `buf`, returning the number of bytes read, or `0` on EOF
    /// (the stream's peer finished it) — the same convention as
    /// `tokio::io::AsyncRead`/`std::io::Read`.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(stream) => stream.read(buf).await,
            #[cfg(feature = "qmux")]
            Self::Qmux(stream) => stream.read(buf).await,
        }
    }

    pub async fn write_all(&mut self, buf: &[u8]) -> Result<(), MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(stream) => stream.write_all(buf).await,
            #[cfg(feature = "qmux")]
            Self::Qmux(stream) => stream.write_all(buf).await,
        }
    }

    /// Signals that no more data will be written (finishes the send side).
    pub async fn shutdown(&mut self) -> Result<(), MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(stream) => stream.shutdown().await,
            #[cfg(feature = "qmux")]
            Self::Qmux(stream) => stream.shutdown().await,
        }
    }

    /// Waits until the peer has either received everything this stream sent
    /// (acknowledged a prior [`AnyByteStream::shutdown`]) or explicitly
    /// stopped reading it. A caller that writes a final message and then
    /// immediately drops/closes the whole connection can race the peer
    /// actually receiving that message — `isekai-pipe serve`'s `reject()`
    /// hit exactly this (documented there as "実測で確認済みのバグ": the
    /// QUIC connection closing before the reject reason reached the client)
    /// and this crate's own listener tests independently reproduced the
    /// same class of race (`noq_backend`/`qmux_backend`'s `PeerClosed`
    /// gotcha in their listener echo tests) — call this after
    /// [`AnyByteStream::shutdown`] and before closing the connection
    /// whenever the caller needs the peer to have actually seen the last
    /// write, not just that the local call to send it returned `Ok`.
    pub async fn wait_for_close(&mut self) -> Result<(), MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(stream) => stream.wait_for_close().await,
            #[cfg(feature = "qmux")]
            Self::Qmux(stream) => stream.wait_for_close().await,
        }
    }

    /// Splits this stream into independently-owned read/write halves so a
    /// caller can drive "read from A, write to this stream" and "read from
    /// this stream, write to B" as two separately `tokio::spawn`ed tasks
    /// without any shared lock between them. Every concrete backend already
    /// keeps its send/recv sides as physically separate fields under the
    /// hood, so this is a cheap move — never a runtime-synchronized wrapper.
    pub fn split(self) -> (AnyByteStreamReadHalf, AnyByteStreamWriteHalf) {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(stream) => {
                let (read, write) = stream.split();
                (AnyByteStreamReadHalf::Noq(read), AnyByteStreamWriteHalf::Noq(write))
            }
            #[cfg(feature = "qmux")]
            Self::Qmux(stream) => {
                let (read, write) = stream.split();
                (AnyByteStreamReadHalf::Qmux(read), AnyByteStreamWriteHalf::Qmux(write))
            }
        }
    }

    /// The inverse of [`AnyByteStream::split`] — recombines a previously
    /// split pair back into one stream a caller can hand off to code that
    /// expects the combined shape (e.g. a resume/reconnect flow that only
    /// needed split halves transiently, to write a request and read a
    /// response sequentially, but whose caller ultimately wants the same
    /// `AnyByteStream` shape a fresh connection's `open_bi()` would have
    /// produced). `read`/`write` must come from the same prior `split()`
    /// call — mixing halves from two different streams, or from different
    /// backends, panics rather than silently producing a stream that reads
    /// from one connection and writes to another.
    pub fn unsplit(read: AnyByteStreamReadHalf, write: AnyByteStreamWriteHalf) -> Self {
        match (read, write) {
            #[cfg(feature = "noq")]
            (AnyByteStreamReadHalf::Noq(read), AnyByteStreamWriteHalf::Noq(write)) => {
                Self::Noq(crate::noq_backend::NoqByteStream::unsplit(read, write))
            }
            #[cfg(feature = "qmux")]
            (AnyByteStreamReadHalf::Qmux(read), AnyByteStreamWriteHalf::Qmux(write)) => {
                Self::Qmux(crate::qmux_backend::QmuxByteStream::unsplit(read, write))
            }
            #[cfg(all(feature = "noq", feature = "qmux"))]
            _ => panic!("AnyByteStream::unsplit: read and write halves came from different backends"),
        }
    }
}

/// The read half of an [`AnyByteStream`] after [`AnyByteStream::split`].
pub enum AnyByteStreamReadHalf {
    #[cfg(feature = "noq")]
    Noq(crate::noq_backend::NoqByteStreamReadHalf),
    #[cfg(feature = "qmux")]
    Qmux(crate::qmux_backend::QmuxByteStreamReadHalf),
}

impl AnyByteStreamReadHalf {
    /// Same contract as [`AnyByteStream::read`].
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(half) => half.read(buf).await,
            #[cfg(feature = "qmux")]
            Self::Qmux(half) => half.read(buf).await,
        }
    }
}

/// The write half of an [`AnyByteStream`] after [`AnyByteStream::split`].
pub enum AnyByteStreamWriteHalf {
    #[cfg(feature = "noq")]
    Noq(crate::noq_backend::NoqByteStreamWriteHalf),
    #[cfg(feature = "qmux")]
    Qmux(crate::qmux_backend::QmuxByteStreamWriteHalf),
}

impl AnyByteStreamWriteHalf {
    /// Same contract as [`AnyByteStream::write_all`].
    pub async fn write_all(&mut self, buf: &[u8]) -> Result<(), MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(half) => half.write_all(buf).await,
            #[cfg(feature = "qmux")]
            Self::Qmux(half) => half.write_all(buf).await,
        }
    }

    /// Same contract as [`AnyByteStream::shutdown`].
    pub async fn shutdown(&mut self) -> Result<(), MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(half) => half.shutdown().await,
            #[cfg(feature = "qmux")]
            Self::Qmux(half) => half.shutdown().await,
        }
    }

    /// Same contract as [`AnyByteStream::wait_for_close`].
    pub async fn wait_for_close(&mut self) -> Result<(), MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(half) => half.wait_for_close().await,
            #[cfg(feature = "qmux")]
            Self::Qmux(half) => half.wait_for_close().await,
        }
    }

    /// Abandons this send stream with an abrupt reset (`code`) instead of a
    /// graceful finish — see `NoqByteStreamWriteHalf::reset`'s docs for why
    /// this distinction matters to a peer deciding whether the stream just
    /// finished cleanly or was abandoned outright. Infallible: both backends
    /// treat "already finished/reset" as a no-op, not an error a caller
    /// needs to react to.
    pub fn reset(&mut self, code: u32) {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(half) => half.reset(code),
            #[cfg(feature = "qmux")]
            Self::Qmux(half) => half.reset(code),
        }
    }
}
