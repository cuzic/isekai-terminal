//! [`MuxError`]: the error type every `quicmux` operation (connection
//! establishment, stream I/O, rebind) fails with — deliberately richer than
//! a bare `std::io::Error`, because callers layered on top of this crate
//! (e.g. `isekai-transport`'s resume logic) need to tell "the peer explicitly
//! closed this session" apart from "the transport just died" to decide
//! whether auto-resuming is safe. Collapsing that distinction into a single
//! opaque error would push callers back to string-matching, which is exactly
//! what a typed error type exists to avoid.

use std::net::SocketAddr;

/// Errors surfaced by `quicmux`'s connection-establishment, stream I/O, and
/// rebind operations. Deliberately backend-agnostic — nothing here names
/// `noq` or `qmux` types directly, so a caller that matches on a specific
/// variant never needs to know (or `#[cfg]`-gate on) which backend produced
/// it.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    /// Failed to bind a fresh local UDP socket. UDP-specific (only the `noq`
    /// backend ever produces this — `qmux` runs over TCP and has no
    /// equivalent bind step of its own kind), but kept as a variant on the
    /// shared error type rather than a backend-specific one so callers that
    /// don't care which backend they're running against can still match on
    /// it uniformly.
    #[error("failed to bind local UDP socket at {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    /// Failed to prepare an already-bound socket for reuse by this mux
    /// backend (e.g. `tokio::net::UdpSocket::into_std` failing). Distinct
    /// from [`MuxError::Bind`], which is specifically about the initial
    /// `bind()` syscall, not a later reuse of an already-bound socket.
    #[error("failed to prepare an already-bound socket for mux use: {0}")]
    SocketSetup(String),

    /// Failed to configure the underlying mux endpoint (e.g. a malformed
    /// `EndpointConfig`).
    #[error("failed to configure the mux endpoint: {0}")]
    EndpointSetup(String),

    /// Failed to build the TLS client configuration (cipher suite/ALPN/
    /// certificate-verifier setup) before a connection attempt even reached
    /// the network.
    #[error("failed to configure TLS: {0}")]
    TlsConfig(String),

    /// Setting up the connection attempt itself failed, before any bytes
    /// reached the network (e.g. the backend rejected the destination
    /// address).
    #[error("connect setup failed: {0}")]
    ConnectSetup(String),

    /// The TLS/mux handshake failed for a reason other than a pinned-cert
    /// mismatch (see [`MuxError::CertPinMismatch`], which is reported
    /// separately since callers often want to react to it differently —
    /// e.g. treat it as a signal that cached trust material is stale).
    #[error("handshake failed: {0}")]
    Handshake(String),

    /// The peer's presented certificate did not match the pinned SHA-256
    /// fingerprint the caller expected. Reported as its own variant (not
    /// folded into [`MuxError::Handshake`]) because callers commonly want to
    /// react to this specifically — e.g. treat a mismatch as a high-
    /// confidence signal that previously cached trust material (the
    /// fingerprint itself) is stale, which a generic handshake failure is
    /// not.
    #[error("peer certificate did not match the pinned fingerprint: expected {expected} got {got}")]
    CertPinMismatch { expected: String, got: String },

    /// The peer rejected this connection attempt at the authentication layer
    /// (as opposed to the transport/TLS layer) — e.g. an application-level
    /// credential the caller presented after the handshake was rejected.
    /// `quicmux` itself never produces this (it has no authentication layer
    /// of its own); it exists for callers that layer their own
    /// authentication over a `quicmux` connection and want to report the
    /// failure through this same error type instead of inventing another
    /// one.
    #[error("authentication failed: {0}")]
    AuthenticationFailed(String),

    /// Failed to open a new stream on an otherwise-live connection.
    #[error("failed to open a stream: {0}")]
    OpenStream(String),

    /// A stream read/write/shutdown operation failed for a reason not
    /// covered by a more specific variant (e.g. [`MuxError::StreamReset`],
    /// [`MuxError::TransportLost`]).
    #[error("stream I/O failed: {0}")]
    StreamIo(String),

    /// This connection was closed by a local call to `close()` — not by the
    /// peer, and not because the transport died. Distinct from
    /// [`MuxError::PeerClosed`]/[`MuxError::TransportLost`] specifically so a
    /// caller that closed the connection itself doesn't misclassify its own
    /// action as a signal to retry/resume.
    #[error("connection was closed locally")]
    LocallyClosed,

    /// The peer explicitly closed the connection (an application-level
    /// close, or the peer's own transport reporting a clean/aborted close it
    /// initiated) — as opposed to the connection simply going silent
    /// ([`MuxError::TransportLost`]). This is the "the other side told us it
    /// is done" case: a caller like `isekai-transport`'s resume logic must
    /// **not** auto-resume a session the peer explicitly closed, but safely
    /// may retry/resume one where the transport merely died underneath it.
    #[error("peer closed the connection (code={code}): {reason}")]
    PeerClosed { code: u64, reason: String },

    /// The peer reset (aborted) one specific stream — distinct from
    /// [`MuxError::PeerClosed`], which is a whole-connection close. Note:
    /// `quicmux`'s [`crate::AnyByteStream`] deliberately does not expose an
    /// API to *trigger* a reset (no `finish`/`reset`/`stop` split — see that
    /// type's docs); this variant exists because a stream can still be
    /// reset *by the peer* even though this crate's own API surface never
    /// asks for one.
    #[error("stream was reset by the peer (code={code})")]
    StreamReset { code: u64 },

    /// The connection is gone for a reason that isn't an explicit peer
    /// close and isn't a stream-level event — e.g. an idle timeout, a
    /// network-level reset, or the peer's transport stack aborting without
    /// an application-level reason. `retryable` says whether re-dialing is
    /// plausibly worth attempting (e.g. an idle timeout: yes; a version
    /// mismatch discovered mid-connection: no) — callers building a resume/
    /// retry loop on top of `quicmux` should consult this instead of
    /// guessing from the message text.
    #[error("transport lost: {reason}")]
    TransportLost { reason: String, retryable: bool },

    /// The peer violated the underlying protocol (QUIC transport-level
    /// error, or a QMux framing/protocol error) in a way that isn't
    /// meaningfully retryable — redialing the same peer would very likely
    /// hit the same violation again.
    #[error("protocol violation: {0}")]
    ProtocolViolation(String),

    /// Failed to export keying material from the live TLS session.
    #[error("failed to export keying material: {0}")]
    ExportKeyingMaterial(String),

    /// A rebind (switching an already-live connection's endpoint to a new
    /// local socket, without a fresh handshake) failed — either the
    /// replacement socket couldn't be bound, or the backend rejected the
    /// switch itself. On failure, the connection keeps using whatever
    /// socket it had before the attempt (see [`crate::AnyMuxRebinder`]'s
    /// docs for the exact guarantee, which is backend-specific).
    #[error("failed to rebind to a new local socket: {0}")]
    Rebind(String),

    /// This backend has no meaningful way to perform the requested
    /// operation at all (as opposed to attempting it and failing) — e.g.
    /// calling `wrap_bound_socket` against the `qmux` backend, which runs
    /// over TCP and has no UDP socket concept to wrap in the first place.
    #[error("{operation} is not supported by this backend: {reason}")]
    Unsupported { operation: &'static str, reason: &'static str },
}

impl MuxError {
    /// Whether this specific failure is a [`MuxError::TransportLost`] that
    /// was itself reported as retryable. Convenience for callers that just
    /// want a `bool` without matching the variant themselves — does *not*
    /// attempt to guess retryability for any other variant (e.g.
    /// [`MuxError::Bind`] failures aren't covered; callers with an opinion on
    /// those should classify them separately, the same way this crate's own
    /// callers already did before this type existed).
    pub fn is_retryable_transport_loss(&self) -> bool {
        matches!(self, MuxError::TransportLost { retryable: true, .. })
    }
}
