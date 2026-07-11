//! Errors surfaced by `isekai-transport`'s connection-establishment and
//! relay-handshake logic (`archive/ISEKAI_SSH_DESIGN.md` phase S-0d-1).

use isekai_protocol::attach::AttachRejectReason;
use isekai_protocol::resume::ResumeRejectReason;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Everything about dialing/maintaining the underlying mux connection
    /// itself (bind/connect/handshake/cert-pin/stream-I/O/rebind failures)
    /// is `quicmux`'s concern, not this crate's — see [`quicmux::MuxError`]
    /// for the full breakdown. This variant is how those failures surface
    /// through this crate's own error type without this crate re-declaring
    /// the same set of cases under different names.
    #[error(transparent)]
    Mux(#[from] quicmux::MuxError),

    /// isekai-helper responded to `ATTACH_HELLO` with a reject rather than
    /// `AttachReadyV2` (`#18`, ATTACH v2).
    #[error("isekai-helper rejected the connection: {0:?}")]
    Rejected(AttachRejectReason),

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

    /// The control stream handshake (`CONTROL_HELLO`/`CONTROL_ACK`,
    /// `archive/HELPER_PROTOCOL.md` §7.3) got a response byte other than
    /// `CONTROL_ACK` (`resume::open_control_stream`).
    #[error("isekai-helper control stream handshake failed: {0}")]
    ControlHandshake(String),

    /// isekai-helper rejected a `RESUME` request (`archive/HELPER_PROTOCOL.md` §7.3
    /// "RESUME の拒否応答"). `UnknownSession`/`OffsetGone` both mean resume is
    /// not possible for this `session_id` any more — the caller must fall
    /// back to a fresh (non-resuming) connection.
    #[error("isekai-helper rejected RESUME: {0:?}")]
    ResumeRejected(ResumeRejectReason),
}

impl TransportError {
    /// Whether this failure is a high-confidence signal that a
    /// `PersistentProfile`'s cached trust material (session_secret/cert
    /// pin) is stale — most commonly because the deployed `isekai-pipe
    /// serve` process restarted and regenerated both (`engine/mod.rs`
    /// generates fresh ephemeral values on every launch, never persisting
    /// them). Deliberately narrow: only a cert-pin mismatch (from
    /// [`quicmux::MuxError::CertPinMismatch`]) and an explicit server-side
    /// auth reject qualify. Plain connectivity failures (timeout, connection
    /// refused, DNS, etc.) do *not* — those are indistinguishable from an
    /// ordinary transient network problem over a single attempt, and
    /// treating them as "stale" would trigger a wasted SSH re-deploy attempt
    /// on every blip (`ISEKAI_PIPE_DESIGN.md` §8 Epic N).
    pub fn is_stale_trust_signal(&self) -> bool {
        matches!(self, Self::Mux(quicmux::MuxError::CertPinMismatch { .. }) | Self::Rejected(AttachRejectReason::Auth))
    }
}

/// Attached via `anyhow::Error::context` at the point a connect-time error
/// classified by `is_stale_trust_signal()` is converted to `anyhow::Error`
/// (`isekai-pipe/src/main.rs`), so `isekai-pipe connect` can later
/// `downcast_ref` it off the top-level error to decide whether to write a
/// `ConnectOutcome::StaleTrust` side-channel file for the `isekai-ssh`
/// wrapper to notice (`ISEKAI_PIPE_DESIGN.md` §8 Epic N). Mirrors
/// `isekai-bootstrap-plan::BootstrapFailure`'s own
/// attach-at-the-source/downcast-at-the-top shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleTrustSignal;

impl std::fmt::Display for StaleTrustSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cached trust looks stale (cert pin mismatch or session-secret rejected)")
    }
}

impl std::error::Error for StaleTrustSignal {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_pin_mismatch_is_a_stale_trust_signal() {
        assert!(TransportError::Mux(quicmux::MuxError::CertPinMismatch { expected: "a".to_string(), got: "b".to_string() })
            .is_stale_trust_signal());
    }

    #[test]
    fn rejected_auth_is_a_stale_trust_signal() {
        assert!(TransportError::Rejected(AttachRejectReason::Auth).is_stale_trust_signal());
    }

    #[test]
    fn other_rejected_reasons_are_not_stale_trust_signals() {
        for reason in [
            AttachRejectReason::Target,
            AttachRejectReason::Unsupported,
            AttachRejectReason::AlreadyAttached,
            AttachRejectReason::StaleGeneration { current_generation: isekai_protocol::attach::ConnectionGeneration::INITIAL },
            AttachRejectReason::BusyOtherSession,
            AttachRejectReason::AttachAlreadyEstablished,
        ] {
            assert!(!TransportError::Rejected(reason).is_stale_trust_signal());
        }
    }

    #[test]
    fn plain_connectivity_failures_are_not_stale_trust_signals() {
        assert!(!TransportError::Mux(quicmux::MuxError::Bind { addr: "127.0.0.1:0".parse().unwrap(), source: std::io::Error::other("x") })
            .is_stale_trust_signal());
        assert!(!TransportError::Mux(quicmux::MuxError::EndpointSetup("x".to_string())).is_stale_trust_signal());
        assert!(!TransportError::Mux(quicmux::MuxError::TlsConfig("x".to_string())).is_stale_trust_signal());
        assert!(!TransportError::Mux(quicmux::MuxError::ConnectSetup("x".to_string())).is_stale_trust_signal());
        assert!(!TransportError::Mux(quicmux::MuxError::Handshake("x".to_string())).is_stale_trust_signal());
        assert!(!TransportError::Mux(quicmux::MuxError::OpenStream("x".to_string())).is_stale_trust_signal());
        assert!(!TransportError::Mux(quicmux::MuxError::StreamIo("x".to_string())).is_stale_trust_signal());
        assert!(!TransportError::Mux(quicmux::MuxError::ExportKeyingMaterial("x".to_string())).is_stale_trust_signal());
        assert!(!TransportError::UnexpectedEof.is_stale_trust_signal());
        assert!(!TransportError::Mux(quicmux::MuxError::SocketSetup("x".to_string())).is_stale_trust_signal());
        assert!(!TransportError::Mux(quicmux::MuxError::Rebind("x".to_string())).is_stale_trust_signal());
        assert!(!TransportError::ControlHandshake("x".to_string()).is_stale_trust_signal());
        assert!(!TransportError::ResumeRejected(ResumeRejectReason::UnknownSession).is_stale_trust_signal());
    }
}
