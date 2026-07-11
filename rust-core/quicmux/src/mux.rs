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
///
/// Only the `noq` backend has a variant today — this crate has no
/// `qmux`-backed listener/accept implementation yet (see [`AnyMuxConnection::accept_bi`]'s
/// docs), so `AnyMuxListener` is currently `noq`-only the same way
/// [`AnyMuxRebinder`] is, and for the same structural reason (a build with
/// only the `qmux` feature enabled has zero variants of this type; no value
/// of it can exist, matching the exhaustiveness-checker pattern
/// [`AnyMuxRebinder`]'s methods already use).
pub enum AnyMuxListener {
    #[cfg(feature = "noq")]
    Noq(crate::noq_backend::NoqListener),
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

    /// Waits for the next inbound connection candidate. Returns `None` once
    /// the listener has been closed and has no more incoming connections to
    /// deliver.
    pub async fn accept(&self) -> Option<AnyMuxIncoming> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(listener) => listener.accept().await.map(AnyMuxIncoming::Noq),
        }
    }

    pub fn local_addr(&self) -> Result<std::net::SocketAddr, MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(listener) => listener.local_addr(),
        }
    }

    /// Requests that the listener (and every connection it produced) be
    /// closed. Best-effort — does not wait for peers to acknowledge; see
    /// [`AnyMuxListener::wait_idle`] for that.
    pub fn close(&self) {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(listener) => listener.close(),
        }
    }

    /// Waits until every connection this listener produced has finished
    /// closing (after a prior [`AnyMuxListener::close`]).
    pub async fn wait_idle(&self) {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(listener) => listener.wait_idle().await,
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
}

impl AnyMuxIncoming {
    /// Awaits handshake completion, yielding the established connection.
    pub async fn accept(self) -> Result<AnyMuxConnection, MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(incoming) => Ok(AnyMuxConnection::Noq(incoming.accept().await?)),
        }
    }
}

/// An established mux connection.
pub enum AnyMuxConnection {
    #[cfg(feature = "noq")]
    Noq(crate::noq_backend::NoqConnection),
    #[cfg(feature = "qmux")]
    Qmux(crate::qmux_backend::QmuxConnection),
}

impl AnyMuxConnection {
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
    /// opened, once the connection itself is established.
    ///
    /// The `qmux` backend does not support this yet — a `Qmux` connection is
    /// only ever produced today by [`AnyMuxEndpoint::connect`] (dialing out),
    /// and this crate has no `qmux`-backed listener/accept path yet (tracked
    /// separately from the `noq` listener support added alongside this
    /// method) — so this fails with [`MuxError::Unsupported`] for that
    /// backend rather than compiling out the method entirely, since a caller
    /// generic over `AnyMuxConnection` should get a runtime error it can
    /// react to, not a build failure that only shows up when `qmux` is the
    /// enabled backend.
    pub async fn accept_bi(&self) -> Result<AnyByteStream, MuxError> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(conn) => Ok(AnyByteStream::Noq(conn.accept_bi().await?)),
            #[cfg(feature = "qmux")]
            Self::Qmux(_) => Err(MuxError::Unsupported {
                operation: "accept_bi",
                reason: "the qmux backend has no server/listener-side implementation yet",
            }),
        }
    }

    /// Best-effort remote address of the peer — `None` if the backend has no
    /// stable single address to report. This covers two different cases: a
    /// `noq` connection with multipath enabled may have a different address
    /// per path (this reports path 0's, which always exists, but that is
    /// still not necessarily "the" address once other paths are live), and
    /// the `qmux` backend does not currently capture/retain the peer's TCP
    /// address on [`AnyMuxConnection`] at all (nothing needs it yet — add it
    /// if a caller does).
    pub fn remote_addr(&self) -> Option<std::net::SocketAddr> {
        match self {
            #[cfg(feature = "noq")]
            Self::Noq(conn) => conn.remote_addr(),
            #[cfg(feature = "qmux")]
            Self::Qmux(_) => None,
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
}
