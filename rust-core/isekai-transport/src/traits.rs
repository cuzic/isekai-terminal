//! `QuicEndpointFactory` / `QuicEndpoint` / `QuicConnection` / `ByteStream`
//! traits (`ISEKAI_SSH_DESIGN.md` "実装方針", "`FaultyUdpSocket`（Android専用
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
//! `helper_quic_transport.rs`'s `establish_quic_connection_with_socket` vs.
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
}

/// A bound QUIC endpoint, capable of initiating outbound connections.
#[async_trait]
pub trait QuicEndpoint: Send + Sync {
    async fn connect(&self, remote: RemoteSpec) -> Result<Box<dyn QuicConnection>, TransportError>;
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
    /// (`helper_quic_transport.rs::compute_proof`'s
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
    /// relay is exactly this — see `ISEKAI_SSH_DESIGN.md`'s note that
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
