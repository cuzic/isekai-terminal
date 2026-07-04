//! Errors surfaced by `isekai-transport`'s connection-establishment and
//! relay-handshake logic (`ISEKAI_SSH_DESIGN.md` phase S-0d-1).

use isekai_protocol::hello::AckResponse;
use isekai_protocol::resume::ResumeRejectReason;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("failed to bind local UDP socket at {addr}: {source}")]
    Bind {
        addr: std::net::SocketAddr,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to configure QUIC endpoint: {0}")]
    EndpointSetup(String),

    #[error("failed to configure TLS client config: {0}")]
    TlsConfig(String),

    #[error("QUIC connect setup failed: {0}")]
    ConnectSetup(String),

    #[error("QUIC handshake failed: {0}")]
    Handshake(String),

    #[error("failed to open a bidirectional QUIC stream: {0}")]
    OpenStream(String),

    #[error("QUIC stream I/O failed: {0}")]
    StreamIo(String),

    #[error("failed to export keying material from the QUIC connection: {0}")]
    ExportKeyingMaterial(String),

    /// isekai-helper responded to `HELLO` with something other than `ACK`
    /// (`HELPER_PROTOCOL.md` §4).
    #[error("isekai-helper rejected the connection: {0:?}")]
    Rejected(AckResponse),

    /// A frame from `isekai_protocol` failed to decode (e.g. an unexpected
    /// response byte on the HELLO/ACK stream).
    #[error(transparent)]
    Protocol(#[from] isekai_protocol::ProtocolError),

    #[error("peer closed the stream before sending a complete response")]
    UnexpectedEof,

    /// Failed to learn this socket's own STUN-observed address
    /// (`stun_p2p::connect_stun_p2p`'s self-observation step,
    /// `isekai_stun_p2p_transport.rs`'s equivalent call to
    /// `isekai_stun::query_stun`).
    #[error("STUN query failed: {0}")]
    Stun(#[from] isekai_stun::StunError),

    /// Failed to prepare an already-bound UDP socket for reuse as a QUIC
    /// endpoint (e.g. `tokio::net::UdpSocket::into_std` failing) — distinct
    /// from `Bind`, which is specifically about the initial `bind()` syscall.
    #[error("failed to prepare UDP socket for QUIC use: {0}")]
    SocketSetup(String),

    /// The control stream handshake (`CONTROL_HELLO`/`CONTROL_ACK`,
    /// `HELPER_PROTOCOL.md` §7.3) got a response byte other than
    /// `CONTROL_ACK` (`resume::open_control_stream`).
    #[error("isekai-helper control stream handshake failed: {0}")]
    ControlHandshake(String),

    /// isekai-helper rejected a `RESUME` request (`HELPER_PROTOCOL.md` §7.3
    /// "RESUME の拒否応答"). `UnknownSession`/`OffsetGone` both mean resume is
    /// not possible for this `session_id` any more — the caller must fall
    /// back to a fresh (non-resuming) connection.
    #[error("isekai-helper rejected RESUME: {0:?}")]
    ResumeRejected(ResumeRejectReason),
}
