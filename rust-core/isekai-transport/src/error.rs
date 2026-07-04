//! Errors surfaced by `isekai-transport`'s connection-establishment and
//! relay-handshake logic (`ISEKAI_SSH_DESIGN.md` phase S-0d-1).

use isekai_protocol::hello::AckResponse;

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
}
