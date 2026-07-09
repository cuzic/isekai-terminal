//! `QuicEndpointFactory` / `QuicEndpoint` / `QuicConnection` / `ByteStream`
//! traits (`archive/ISEKAI_SSH_DESIGN.md` "実装方針", "`FaultyUdpSocket`（Android専用
//! フォルト注入ソケット）の扱い" 節).
//!
//! These exist so that `isekai-transport`'s relay-connection logic
//! (`relay.rs`) never has to know whether it is running against a real
//! `tokio::net::UdpSocket` (`system::SystemQuicEndpointFactory`, used by the
//! CLI) or an Android-specific instrumented socket (`isekai-terminal-core`'s
//! debug-only fault-injection factory, kept out of this crate entirely).
//! Only *connection establishment and stream opening* lives behind this
//! boundary — HELLO/proof/ACK protocol logic is layered on top in
//! `relay.rs`/`proof.rs` using `isekai_protocol`, not baked into these
//! traits (mirrors the split already proven out by
//! `isekai_pipe_quic_transport.rs`'s `establish_quic_connection_with_socket` vs.
//! the HELLO/ACK code that calls it).
//!
//! Async trait methods are made object-safe via `async-trait`, the same
//! crate `isekai-terminal-core` already depends on — no new dependency introduced here.

use async_trait::async_trait;

use crate::error::TransportError;
use crate::types::{BindSpec, RemoteSpec};

/// Creates QUIC endpoints bound to a given local address. Implementations
/// own the concrete UDP socket type; callers of `connect_via_relay` only
/// ever see this trait object.
#[async_trait]
pub trait QuicEndpointFactory: Send + Sync {
    async fn create_endpoint(&self, bind: BindSpec) -> Result<Box<dyn QuicEndpoint>, TransportError>;

    /// Wraps an already-bound `tokio::net::UdpSocket` as a QUIC endpoint,
    /// instead of binding a fresh one via [`QuicEndpointFactory::create_endpoint`]
    /// (isekai-terminal-core/isekai-transport crate共有化 Phase 1c). For
    /// `stun_p2p::connect_stun_p2p`, which must do its own raw STUN-query/
    /// hole-punch-probe I/O on a specific socket *before* handing it to
    /// `noq` — that raw phase can't go through `create_endpoint`'s
    /// bind-a-fresh-socket contract, but the *implementation-specific* part
    /// (which concrete `noq::AsyncUdpSocket` wraps it — plain for
    /// `system::SystemQuicEndpointFactory`, fault-injectable for
    /// `isekai-terminal-core`'s own factory) still needs to be pluggable the
    /// same way `create_endpoint` already is, so this crate's STUN P2P logic
    /// never has to know which concrete socket type it's running against.
    async fn wrap_bound_socket(&self, socket: tokio::net::UdpSocket) -> Result<Box<dyn QuicEndpoint>, TransportError>;
}

/// A bound QUIC endpoint, capable of initiating outbound connections.
#[async_trait]
pub trait QuicEndpoint: Send + Sync {
    async fn connect(&self, remote: RemoteSpec) -> Result<Box<dyn QuicConnection>, TransportError>;

    /// Returns a handle that can later switch this endpoint's local UDP
    /// socket without tearing down any connection made through it — for a
    /// caller that wants to react to an OS-reported network change faster
    /// than falling all the way back to a brand-new connection + RESUME
    /// handshake (`isekai-pipe`'s `--experimental-network-rebind`; see
    /// `system::SystemQuicEndpointRebinder`'s docs for the underlying
    /// mechanism and its caveats).
    ///
    /// `None` by default — this is deliberately *not* a required method,
    /// because "switch the local socket of an already-connected endpoint"
    /// is an engine-specific capability with no meaningful generic
    /// implementation (unlike `connect`/`open_bi`/etc., which every QUIC
    /// engine this trait could plausibly be backed by supports in some
    /// form). Only `system::SystemQuicEndpoint` overrides this today.
    fn rebinder(&self) -> Option<Box<dyn QuicEndpointRebinder>> {
        None
    }
}

/// A handle that can switch its [`QuicEndpoint`]'s local UDP socket in
/// place, without disturbing any connection already established through it
/// — see [`QuicEndpoint::rebinder`] for why this is a separate, optional
/// trait rather than a method on `QuicEndpoint`/`QuicConnection` directly.
#[async_trait]
pub trait QuicEndpointRebinder: Send + Sync {
    /// Switches the endpoint to `socket` directly, instead of binding a
    /// fresh one itself — the primitive [`QuicEndpointRebinder::rebind`] is
    /// built from. This is what a caller who needs to rebind onto a
    /// *specific physical network interface* uses: bind `socket` themselves
    /// first (e.g. via `quicsock::bind_udp` on the CLI/PC, or via Android's
    /// `Network.bindSocket()` on the Kotlin/JNI side, then import the fd),
    /// and hand the already-bound result here — this trait has no opinion
    /// on how `socket` got bound, only that it's ready to use.
    ///
    /// A successful return means the switch itself succeeded — it does
    /// **not** mean the new socket can actually reach the peer (that can
    /// only be learned by observing whether the connection keeps working
    /// afterward). On failure, the endpoint keeps using its previous socket
    /// (whatever guarantee the underlying engine's own rebind operation
    /// gives here — `system::SystemQuicEndpointRebinder`'s docs cite noq's).
    async fn rebind_socket(&self, socket: std::net::UdpSocket) -> Result<(), TransportError>;

    /// Binds a fresh local UDP socket at `bind` (an ordinary, OS-routed
    /// bind — no specific physical interface) and switches to it via
    /// [`QuicEndpointRebinder::rebind_socket`]. The common case (e.g.
    /// reacting to an OS-reported IP change without caring which interface
    /// it came from); reach for `rebind_socket` directly for the
    /// physical-interface case.
    async fn rebind(&self, bind: BindSpec) -> Result<(), TransportError> {
        let socket = std::net::UdpSocket::bind(bind.local_addr)
            .map_err(|source| TransportError::Bind { addr: bind.local_addr, source })?;
        self.rebind_socket(socket).await
    }
}

/// An established QUIC connection.
#[async_trait]
pub trait QuicConnection: Send + Sync {
    /// Opens a new bidirectional stream on this connection.
    async fn open_bi(&self) -> Result<Box<dyn ByteStream>, TransportError>;

    /// Requests that the connection be closed. Best-effort — mirrors
    /// `noq::Connection::close`, which does not wait for the peer to
    /// acknowledge.
    async fn close(&self);

    /// Exports keying material from the live TLS session
    /// (`isekai_pipe_quic_transport.rs::compute_proof`'s
    /// `conn.export_keying_material` call, generalized behind this trait so
    /// `isekai-transport`'s proof computation (`proof.rs`) never touches a
    /// concrete `noq::Connection`). Always returns exactly 32 bytes, which is
    /// all `compute_proof` needs.
    async fn export_keying_material(&self, label: &[u8], context: &[u8]) -> Result<[u8; 32], TransportError>;
}

/// One direction-agnostic byte stream — a QUIC bidirectional stream, from
/// the caller's point of view.
#[async_trait]
pub trait ByteStream: Send {
    /// Reads into `buf`, returning the number of bytes read, or `0` on EOF
    /// (the stream's peer finished it) — the same convention as
    /// `tokio::io::AsyncRead`/`std::io::Read`.
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;

    async fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError>;

    /// Signals that no more data will be written (finishes the send side).
    async fn shutdown(&mut self) -> Result<(), TransportError>;

    /// Splits this stream into independently-owned read/write halves so a
    /// caller can drive "read from A, write to this stream" and "read from
    /// this stream, write to B" as two separately `tokio::spawn`ed tasks
    /// without any shared lock between them (`isekai-ssh`'s stdin/stdout
    /// relay is exactly this — see `archive/ISEKAI_SSH_DESIGN.md`'s note that
    /// `tokio::io::copy_bidirectional` doesn't fit because stdin/stdout are
    /// two separate handles, not one duplex object; the QUIC side has the
    /// same "two separate handles" shape once split).
    ///
    /// Every concrete `ByteStream` already keeps its send/recv sides as
    /// physically separate fields under the hood (a QUIC bidi stream *is*
    /// two independent objects, one per direction), so implementations
    /// should return this as a cheap move/reinterpretation — never a
    /// runtime-synchronized wrapper (a `Mutex`-guarded single object would
    /// let a stalled write block an otherwise-ready read, or vice versa,
    /// defeating the point of splitting in the first place).
    fn split(self: Box<Self>) -> (Box<dyn ByteStreamReadHalf>, Box<dyn ByteStreamWriteHalf>);
}

/// The read half of a `ByteStream` after `ByteStream::split`.
#[async_trait]
pub trait ByteStreamReadHalf: Send {
    /// Same contract as `ByteStream::read`.
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;
}

/// The write half of a `ByteStream` after `ByteStream::split`.
#[async_trait]
pub trait ByteStreamWriteHalf: Send {
    /// Same contract as `ByteStream::write_all`.
    async fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError>;

    /// Same contract as `ByteStream::shutdown`.
    async fn shutdown(&mut self) -> Result<(), TransportError>;
}
