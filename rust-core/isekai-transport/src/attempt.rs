//! Classifies what happened to one `connect_and_handshake` attempt
//! (`ISEKAI_PIPE_DESIGN.md` task #12, ChatGPT second-opinion review
//! 2026-07-08), for callers that juggle multiple candidates and need to
//! decide whether it's safe to try a different one next.
//!
//! The safety property this exists to encode: whether moving on to another
//! candidate is safe depends on whether the server could plausibly have
//! already committed this attempt's session-attach
//! (`isekai-pipe serve`'s `AttachArbiter`, `#18`). If the client sent
//! `ATTACH_HELLO`/`AttachActivate` and then lost the ability to observe the
//! outcome (write failure, read failure, no response before giving up), the
//! server may have already moved this attempt into `PendingActivation` or
//! `Established` — a *different* candidate that reaches the *same*
//! underlying helper (e.g. another relay endpoint for it, `#12`'s own scope)
//! would then either collide (`ALREADY_ATTACHED`) or, before `#25` exists,
//! leave the first attempt's session orphaned with nothing to reconcile it.
//! `#25` (post-`#18`) adds that reconciliation (advancing
//! `ConnectionGeneration` to safely supersede an ambiguous earlier attempt);
//! this crate doesn't retry past it yet, so the only safe answer today is
//! "fail closed" for anything ambiguous.

use isekai_protocol::attach::{AttachRejectReason, ConnectionGeneration};

use crate::error::TransportError;

/// Which phase of the `ATTACH_HELLO`/proof/`AttachReadyV2`/`AttachActivate`
/// handshake a `connect_and_handshake` attempt failed in. `pub(crate)` —
/// only `relay.rs` constructs these; the public classification callers
/// branch on is [`AttemptFailure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectAttemptStage {
    QuicConnect,
    ComputeProof,
    OpenStream,
    HelloWrite,
    /// No `AttachReadyV2`/reject byte arrived before the read itself failed
    /// (I/O error, unexpected EOF) — distinct from receiving an explicit
    /// reject, which is [`ConnectAttemptStage::Rejected`] instead.
    AckRead,
    /// The server sent `AttachReadyV2`, but writing `AttachActivate` back
    /// failed — exactly as ambiguous as `HelloWrite`/`AckRead`: the server
    /// may or may not have received it before moving on
    /// (`AttachArbiter::PendingActivation`'s own timeout is what eventually
    /// reclaims the slot server-side if it never arrives).
    ActivateWrite,
    /// The server responded with an explicit reject byte (not silence, not
    /// an I/O failure) — the client *does* know the outcome for certain.
    Rejected(RejectReason),
}

/// Mirrors `isekai_protocol::attach::AttachRejectReason`, minus the payload
/// on `StaleGeneration` (kept alongside instead, since [`AttemptFailure`]'s
/// `StaleAttempt` variant is where callers actually want it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RejectReason {
    Auth,
    Target,
    Unsupported,
    AlreadyAttached,
    StaleGeneration { current_generation: ConnectionGeneration },
    BusyOtherSession,
    AttachAlreadyEstablished,
}

impl RejectReason {
    pub(crate) fn from_attach_reject(reason: AttachRejectReason) -> Self {
        match reason {
            AttachRejectReason::Auth => Self::Auth,
            AttachRejectReason::Target => Self::Target,
            AttachRejectReason::Unsupported => Self::Unsupported,
            AttachRejectReason::AlreadyAttached => Self::AlreadyAttached,
            AttachRejectReason::StaleGeneration { current_generation } => {
                Self::StaleGeneration { current_generation }
            }
            AttachRejectReason::BusyOtherSession => Self::BusyOtherSession,
            AttachRejectReason::AttachAlreadyEstablished => Self::AttachAlreadyEstablished,
        }
    }

    #[cfg(test)]
    fn as_attach_reject(self) -> AttachRejectReason {
        match self {
            Self::Auth => AttachRejectReason::Auth,
            Self::Target => AttachRejectReason::Target,
            Self::Unsupported => AttachRejectReason::Unsupported,
            Self::AlreadyAttached => AttachRejectReason::AlreadyAttached,
            Self::StaleGeneration { current_generation } => {
                AttachRejectReason::StaleGeneration { current_generation }
            }
            Self::BusyOtherSession => AttachRejectReason::BusyOtherSession,
            Self::AttachAlreadyEstablished => AttachRejectReason::AttachAlreadyEstablished,
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
    /// before opening the QUIC stream, or before finishing the
    /// `ATTACH_HELLO` write) — trying the next candidate is always safe.
    RetryablePreAttach { source: TransportError },
    /// The server rejected this attempt for a reason that would recur
    /// identically against this *same* underlying helper regardless of which
    /// candidate reached it (`Target`: the helper's own configured target is
    /// unreachable) — not a security concern, but retrying a different
    /// candidate *of the same helper* cannot help either.
    DefinitiveRejectNotRetryable { source: TransportError },
    /// `ATTACH_HELLO`/`AttachActivate` was sent and then the outcome became
    /// unobservable (read/write failure, no response before giving up)
    /// before a definitive response arrived — the server may or may not
    /// have already committed this attempt's session-attach. Must not be
    /// silently followed by trying another candidate before `#25` exists
    /// (`#12`'s own scope stops here; `#25` extends past it once it can
    /// safely supersede via a new `ConnectionGeneration`).
    AmbiguousAfterAttach { source: TransportError },
    /// `ALREADY_ATTACHED`: a *different* attempt already won this exact
    /// `(session_id, generation)` — this candidate lost a race (`#19`), not
    /// a terminal failure for the session as a whole.
    LostRace { source: TransportError },
    /// `STALE_GENERATION`: this attempt's generation is behind the server's
    /// current one for this session — carries the server's
    /// `current_generation` so a future caller (`#25`) knows exactly how far
    /// to jump rather than guessing.
    StaleAttempt { source: TransportError, current_generation: ConnectionGeneration },
    /// `ATTACH_ALREADY_ESTABLISHED`: this session already completed its
    /// initial attach — a new attach round shouldn't be started for it,
    /// `RESUME` should be used instead.
    MustResume { source: TransportError },
    /// A reason to stop entirely, not just skip this candidate — an auth
    /// rejection (wrong session secret, or a MITM/replay), a protocol
    /// version mismatch, or a signal that some other logical session is
    /// occupying the server right now (`BUSY_OTHER_SESSION`). Worth
    /// surfacing loudly, not quietly retrying past.
    Terminal { source: TransportError },
}

impl std::fmt::Display for AttemptFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RetryablePreAttach { source }
            | Self::DefinitiveRejectNotRetryable { source }
            | Self::AmbiguousAfterAttach { source }
            | Self::LostRace { source }
            | Self::StaleAttempt { source, .. }
            | Self::MustResume { source }
            | Self::Terminal { source } => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for AttemptFailure {}

impl AttemptFailure {
    /// Whether a sequential fallback connector may safely try a different
    /// candidate after this failure, *without* `#25`'s generation-advancing
    /// machinery. `#25` will need its own, less conservative check for the
    /// `AmbiguousAfterAttach`/`LostRace` cases.
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
            | Self::LostRace { source }
            | Self::StaleAttempt { source, .. }
            | Self::MustResume { source }
            | Self::Terminal { source } => source,
        }
    }

    /// Delegates to the wrapped `TransportError::is_stale_trust_signal`
    /// (`ISEKAI_PIPE_DESIGN.md` §8 Epic N) — the sequential-fallback
    /// classification this type adds is orthogonal to whether the
    /// *underlying* failure looks like stale cached trust material.
    pub fn is_stale_trust_signal(&self) -> bool {
        match self {
            Self::RetryablePreAttach { source }
            | Self::DefinitiveRejectNotRetryable { source }
            | Self::AmbiguousAfterAttach { source }
            | Self::LostRace { source }
            | Self::StaleAttempt { source, .. }
            | Self::MustResume { source }
            | Self::Terminal { source } => source.is_stale_trust_signal(),
        }
    }
}

impl From<ConnectAttemptError> for AttemptFailure {
    fn from(e: ConnectAttemptError) -> Self {
        match e.stage {
            ConnectAttemptStage::QuicConnect | ConnectAttemptStage::ComputeProof | ConnectAttemptStage::OpenStream => {
                AttemptFailure::RetryablePreAttach { source: e.source }
            }
            // A write/read failure anywhere from the HELLO write through the
            // Activate write is exactly as ambiguous: in every case we
            // cannot tell whether the server ever saw (and acted on) the
            // frame in question.
            ConnectAttemptStage::HelloWrite | ConnectAttemptStage::AckRead | ConnectAttemptStage::ActivateWrite => {
                AttemptFailure::AmbiguousAfterAttach { source: e.source }
            }
            ConnectAttemptStage::Rejected(RejectReason::Target) => {
                AttemptFailure::DefinitiveRejectNotRetryable { source: e.source }
            }
            ConnectAttemptStage::Rejected(RejectReason::AlreadyAttached) => {
                AttemptFailure::LostRace { source: e.source }
            }
            ConnectAttemptStage::Rejected(RejectReason::StaleGeneration { current_generation }) => {
                AttemptFailure::StaleAttempt { source: e.source, current_generation }
            }
            ConnectAttemptStage::Rejected(RejectReason::AttachAlreadyEstablished) => {
                AttemptFailure::MustResume { source: e.source }
            }
            ConnectAttemptStage::Rejected(RejectReason::Auth)
            | ConnectAttemptStage::Rejected(RejectReason::Unsupported)
            | ConnectAttemptStage::Rejected(RejectReason::BusyOtherSession) => {
                AttemptFailure::Terminal { source: e.source }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(stage: ConnectAttemptStage) -> ConnectAttemptError {
        let reason = match stage {
            ConnectAttemptStage::Rejected(r) => Some(r.as_attach_reject()),
            _ => None,
        };
        ConnectAttemptError {
            stage,
            source: match reason {
                Some(reason) => TransportError::Rejected(reason),
                None => TransportError::UnexpectedEof,
            },
        }
    }

    #[test]
    fn pre_attach_stages_classify_as_retryable() {
        for stage in [ConnectAttemptStage::QuicConnect, ConnectAttemptStage::ComputeProof, ConnectAttemptStage::OpenStream]
        {
            let failure = AttemptFailure::from(err(stage));
            assert!(matches!(failure, AttemptFailure::RetryablePreAttach { .. }));
            assert!(failure.may_retry_pre_fencing());
        }
    }

    #[test]
    fn hello_write_ack_read_and_activate_write_classify_as_ambiguous() {
        for stage in [ConnectAttemptStage::HelloWrite, ConnectAttemptStage::AckRead, ConnectAttemptStage::ActivateWrite] {
            let failure = AttemptFailure::from(err(stage));
            assert!(matches!(failure, AttemptFailure::AmbiguousAfterAttach { .. }));
            assert!(!failure.may_retry_pre_fencing(), "ambiguous failures must not be retried before #25 exists");
        }
    }

    #[test]
    fn reject_target_classifies_as_definitive_not_retryable() {
        let failure = AttemptFailure::from(err(ConnectAttemptStage::Rejected(RejectReason::Target)));
        assert!(matches!(failure, AttemptFailure::DefinitiveRejectNotRetryable { .. }));
        assert!(!failure.may_retry_pre_fencing());
    }

    #[test]
    fn reject_already_attached_classifies_as_lost_race() {
        let failure = AttemptFailure::from(err(ConnectAttemptStage::Rejected(RejectReason::AlreadyAttached)));
        assert!(matches!(failure, AttemptFailure::LostRace { .. }));
        assert!(!failure.may_retry_pre_fencing());
    }

    #[test]
    fn reject_stale_generation_classifies_as_stale_attempt_with_current_generation() {
        let failure = AttemptFailure::from(err(ConnectAttemptStage::Rejected(RejectReason::StaleGeneration {
            current_generation: ConnectionGeneration::new(7),
        })));
        match failure {
            AttemptFailure::StaleAttempt { current_generation, .. } => {
                assert_eq!(current_generation, ConnectionGeneration::new(7));
            }
            other => panic!("expected StaleAttempt, got {other:?}"),
        }
    }

    #[test]
    fn reject_attach_already_established_classifies_as_must_resume() {
        let failure = AttemptFailure::from(err(ConnectAttemptStage::Rejected(RejectReason::AttachAlreadyEstablished)));
        assert!(matches!(failure, AttemptFailure::MustResume { .. }));
        assert!(!failure.may_retry_pre_fencing());
    }

    #[test]
    fn reject_auth_unsupported_and_busy_other_session_classify_as_terminal() {
        for reason in [RejectReason::Auth, RejectReason::Unsupported, RejectReason::BusyOtherSession] {
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

    #[test]
    fn attempt_failure_delegates_stale_trust_signal_to_its_source() {
        let stale = AttemptFailure::from(err(ConnectAttemptStage::Rejected(RejectReason::Auth)));
        assert!(stale.is_stale_trust_signal());

        let not_stale = AttemptFailure::from(err(ConnectAttemptStage::QuicConnect));
        assert!(!not_stale.is_stale_trust_signal());
    }
}
