//! Classifies what happened to one `connect_and_handshake` attempt
//! (`ISEKAI_PIPE_DESIGN.md` task #12, ChatGPT second-opinion review
//! 2026-07-08), for callers that juggle multiple candidates and need to
//! decide whether it's safe to try a different one next.
//!
//! The safety property this exists to encode: whether moving on to another
//! candidate is safe depends on whether the server could plausibly have
//! already committed this attempt's session-attach
//! (`isekai-pipe serve`'s `active: AtomicBool` compare-exchange,
//! `engine/mod.rs::handle_stream`). If the client sent `HELLO` and then lost
//! the ability to observe the outcome (write failure, read failure, no
//! response before giving up), the server may have already flipped `active`
//! to `true` and be mid-`TcpStream::connect` to the target — a *different*
//! candidate that reaches the *same* underlying helper (e.g. another relay
//! endpoint for it, `#12`'s own scope) would then either collide
//! (`FRAME_REJECT_DUPLICATE`) or, once per-candidate independent sessions
//! exist, leave the first attempt's session orphaned with nothing to
//! reconcile it. `#18`/`#25` add that reconciliation (`session_id`/
//! `connection_generation`/`fencing_token`-based winner determination); this
//! crate doesn't have it yet, so the only safe answer today is "fail closed"
//! for anything ambiguous.

use isekai_protocol::hello::AckResponse;

use crate::error::TransportError;

/// Which phase of the HELLO/proof/ACK handshake a `connect_and_handshake`
/// attempt failed in. `pub(crate)` — only `relay.rs` constructs these; the
/// public classification callers branch on is [`AttemptFailure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectAttemptStage {
    QuicConnect,
    ComputeProof,
    OpenStream,
    HelloWrite,
    /// No `ACK`/reject byte arrived before the read itself failed (I/O error,
    /// unexpected EOF) — distinct from receiving an explicit reject byte,
    /// which is [`ConnectAttemptStage::Rejected`] instead.
    AckRead,
    /// The server responded with an explicit reject byte (not silence, not
    /// an I/O failure) — the client *does* know the outcome for certain.
    Rejected(RejectReason),
}

/// The four ACK-reject reasons `isekai_protocol::hello::AckResponse` can
/// carry, minus the success case (`Ack`, which never reaches this type —
/// `connect_and_handshake` only tags a stage as `Rejected` on its `other =>`
/// match arm, which by construction excludes `Ack`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RejectReason {
    Auth,
    Duplicate,
    Target,
    Unsupported,
}

impl RejectReason {
    /// Panics on `AckResponse::Ack` — callers must only invoke this from the
    /// reject branch of an ACK match, never the success branch.
    pub(crate) fn from_ack_response(resp: AckResponse) -> Self {
        match resp {
            AckResponse::RejectAuth => Self::Auth,
            AckResponse::RejectDuplicate => Self::Duplicate,
            AckResponse::RejectTarget => Self::Target,
            AckResponse::RejectUnsupported => Self::Unsupported,
            AckResponse::Ack { .. } => {
                unreachable!("RejectReason is only constructed from the reject arm of an ACK match")
            }
        }
    }
}

/// `connect_and_handshake`'s internal error type: the underlying
/// `TransportError` (unchanged from before this classification existed —
/// still what every existing caller sees via `?`, thanks to the `From` impl
/// below) tagged with which stage produced it. `pub(crate)` — this crate's
/// public error type stays plain `TransportError`; only a multi-candidate
/// connector (`resume::connect_via_relay_resumable_with_fallback`) needs the
/// richer [`AttemptFailure`] classification derived from it.
#[derive(Debug)]
pub(crate) struct ConnectAttemptError {
    pub stage: ConnectAttemptStage,
    pub source: TransportError,
}

impl std::fmt::Display for ConnectAttemptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.source)
    }
}

impl std::error::Error for ConnectAttemptError {}

/// Lets every existing single-candidate caller (`connect_via_relay`,
/// `connect_via_relay_resumable`, `connect_stun_p2p`) keep using `?` against
/// their own `Result<_, TransportError>` return types unchanged — the stage
/// tag is simply dropped for them. Only code that explicitly wants
/// `AttemptFailure` needs to intercept `ConnectAttemptError` before this
/// conversion happens.
impl From<ConnectAttemptError> for TransportError {
    fn from(e: ConnectAttemptError) -> Self {
        e.source
    }
}

/// How a multi-candidate connector should react to one candidate's failure.
/// See the module docs for the safety property this encodes.
#[derive(Debug)]
pub enum AttemptFailure {
    /// Nothing was ever sent to the server for this candidate (failed at or
    /// before opening the QUIC stream, or before finishing the `HELLO`
    /// write) — trying the next candidate is always safe.
    RetryablePreAttach { source: TransportError },
    /// The server rejected this attempt for a reason that would recur
    /// identically against this *same* underlying helper regardless of which
    /// candidate reached it (`REJECT_TARGET`: the helper's own configured
    /// target is unreachable) — not a security concern, but retrying a
    /// different candidate *of the same helper* cannot help either.
    DefinitiveRejectNotRetryable { source: TransportError },
    /// `HELLO` was sent and then the outcome became unobservable (read
    /// failure, no response before giving up) before any `ACK`/reject byte
    /// arrived — the server may or may not have already committed this
    /// attempt's session-attach. Must not be silently followed by trying
    /// another candidate before `#18`'s fencing exists (`#12`'s own scope
    /// stops here; `#25` extends past it once fencing exists).
    AmbiguousAfterAttach { source: TransportError },
    /// A reason to stop entirely, not just skip this candidate — an auth
    /// rejection (wrong session secret, or a MITM/replay), a protocol
    /// version mismatch, or (`REJECT_DUPLICATE`) a signal that some other
    /// connection may already be attached to this helper right now. Worth
    /// surfacing loudly, not quietly retrying past.
    Terminal { source: TransportError },
}

impl std::fmt::Display for AttemptFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RetryablePreAttach { source }
            | Self::DefinitiveRejectNotRetryable { source }
            | Self::AmbiguousAfterAttach { source }
            | Self::Terminal { source } => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for AttemptFailure {}

impl AttemptFailure {
    /// Whether a sequential fallback connector may safely try a different
    /// candidate after this failure, *without* `#18`'s fencing machinery.
    /// `#25` (post-`#18`) will need its own, less conservative check for the
    /// `AmbiguousAfterAttach` case.
    pub fn may_retry_pre_fencing(&self) -> bool {
        matches!(self, Self::RetryablePreAttach { .. })
    }

    /// Unwraps back to the plain `TransportError` every existing caller
    /// already understands (e.g. for logging/`anyhow::Context`) — the
    /// classification is additive information, not a replacement.
    pub fn into_source(self) -> TransportError {
        match self {
            Self::RetryablePreAttach { source }
            | Self::DefinitiveRejectNotRetryable { source }
            | Self::AmbiguousAfterAttach { source }
            | Self::Terminal { source } => source,
        }
    }
}

impl From<ConnectAttemptError> for AttemptFailure {
    fn from(e: ConnectAttemptError) -> Self {
        match e.stage {
            ConnectAttemptStage::QuicConnect | ConnectAttemptStage::ComputeProof | ConnectAttemptStage::OpenStream => {
                AttemptFailure::RetryablePreAttach { source: e.source }
            }
            // A write failure mid-HELLO is exactly as ambiguous as a read
            // failure waiting for the ACK: in both cases we cannot tell
            // whether the server ever saw (and acted on) the HELLO.
            ConnectAttemptStage::HelloWrite | ConnectAttemptStage::AckRead => {
                AttemptFailure::AmbiguousAfterAttach { source: e.source }
            }
            ConnectAttemptStage::Rejected(RejectReason::Target) => {
                AttemptFailure::DefinitiveRejectNotRetryable { source: e.source }
            }
            ConnectAttemptStage::Rejected(RejectReason::Auth)
            | ConnectAttemptStage::Rejected(RejectReason::Unsupported)
            | ConnectAttemptStage::Rejected(RejectReason::Duplicate) => {
                AttemptFailure::Terminal { source: e.source }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(stage: ConnectAttemptStage) -> ConnectAttemptError {
        ConnectAttemptError { stage, source: TransportError::UnexpectedEof }
    }

    #[test]
    fn pre_attach_stages_classify_as_retryable() {
        for stage in [ConnectAttemptStage::QuicConnect, ConnectAttemptStage::ComputeProof, ConnectAttemptStage::OpenStream] {
            let failure = AttemptFailure::from(err(stage));
            assert!(matches!(failure, AttemptFailure::RetryablePreAttach { .. }));
            assert!(failure.may_retry_pre_fencing());
        }
    }

    #[test]
    fn hello_write_and_ack_read_classify_as_ambiguous() {
        for stage in [ConnectAttemptStage::HelloWrite, ConnectAttemptStage::AckRead] {
            let failure = AttemptFailure::from(err(stage));
            assert!(matches!(failure, AttemptFailure::AmbiguousAfterAttach { .. }));
            assert!(!failure.may_retry_pre_fencing(), "ambiguous failures must not be retried before #18 exists");
        }
    }

    #[test]
    fn reject_target_classifies_as_definitive_not_retryable() {
        let failure = AttemptFailure::from(err(ConnectAttemptStage::Rejected(RejectReason::Target)));
        assert!(matches!(failure, AttemptFailure::DefinitiveRejectNotRetryable { .. }));
        assert!(!failure.may_retry_pre_fencing());
    }

    #[test]
    fn reject_auth_duplicate_and_unsupported_classify_as_terminal() {
        for reason in [RejectReason::Auth, RejectReason::Duplicate, RejectReason::Unsupported] {
            let failure = AttemptFailure::from(err(ConnectAttemptStage::Rejected(reason)));
            assert!(matches!(failure, AttemptFailure::Terminal { .. }), "{reason:?} should be Terminal");
            assert!(!failure.may_retry_pre_fencing());
        }
    }

    #[test]
    fn connect_attempt_error_converts_back_to_plain_transport_error() {
        let attempt_err = err(ConnectAttemptStage::QuicConnect);
        let transport_err: TransportError = attempt_err.into();
        assert!(matches!(transport_err, TransportError::UnexpectedEof));
    }

    #[test]
    fn attempt_failure_into_source_recovers_the_transport_error() {
        let failure = AttemptFailure::from(err(ConnectAttemptStage::AckRead));
        assert!(matches!(failure.into_source(), TransportError::UnexpectedEof));
    }
}
