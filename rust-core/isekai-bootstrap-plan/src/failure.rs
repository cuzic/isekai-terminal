//! [`BootstrapFailure`]: why a [`crate::BootstrapPlan`] attempt failed,
//! classified so a caller can decide what to do next without string-matching
//! an error message. `ISEKAI_PIPE_DESIGN.md` §8 Epic A calls out four
//! decisions this classification must support without guessing from text:
//! retry the same route, redirect to `isekai-ssh login`, redirect to
//! `isekai-ssh init`, or fall back from direct/STUN to relay. Modeled after
//! `isekai-transport::attempt::AttemptFailure`'s classification-wrapping-a-
//! source-error shape, one layer up (whole-bootstrap-attempt failures, not
//! one QUIC candidate's).

#[derive(Debug, thiserror::Error)]
pub enum BootstrapFailure {
    /// No usable SSH credential exists for a hop in the chain (no agent
    /// identity, no working key file). Recoverable only by the user
    /// setting up a credential — `isekai-ssh init` walks through that
    /// interactively.
    #[error("no usable SSH credential for this host")]
    AuthenticationRequired,
    /// A relay-scoped token (`CredentialSource::RelayToken`) was supplied
    /// but the relay rejected it as expired.
    #[error("relay token expired")]
    TokenExpired,
    /// `ssh(1)` refused to proceed because the remote host key didn't match
    /// (or wasn't yet) an accepted key. Never auto-retried or silently
    /// routed around — this is a trust decision, not a connectivity one.
    #[error("remote host key rejected")]
    HostKeyRejected,
    /// A hop in the jump chain (or the destination itself, for a 0-hop
    /// plan) could not be reached at the TCP/SSH level.
    #[error("jump host unreachable")]
    JumpHostUnreachable,
    /// `isekai-pipe serve` is not present on the remote host and no upload
    /// step is part of this plan (or the upload step itself is disabled).
    #[error("remote isekai-pipe binary missing")]
    RemoteBinaryMissing,
    /// A remote `isekai-pipe` binary exists but failed signature/digest
    /// verification (Epic D) — must not be executed.
    #[error("remote isekai-pipe binary failed trust verification")]
    RemoteBinaryUntrusted,
    /// The SSH-bootstrap-mediated candidate exchange (STUN route) didn't
    /// complete — e.g. the remote side never reported its observed address
    /// before the candidate-gathering budget ran out.
    #[error("candidate exchange over SSH bootstrap failed")]
    CandidateExchangeFailed,
    /// A STUN server used for address discovery was unreachable or didn't
    /// respond.
    #[error("STUN server unreachable")]
    StunUnreachable,
    /// The relay rejected this session for a reason other than an expired
    /// token (e.g. scope/audience mismatch).
    #[error("relay rejected this session")]
    RelayUnauthorized,
    /// The relay itself could not be reached or is not accepting new
    /// sessions.
    #[error("relay unavailable")]
    RelayUnavailable,
    /// The QUIC handshake to a resolved candidate address failed.
    #[error("QUIC handshake failed")]
    QuicHandshakeFailed,
    /// A QUIC handshake completed but the remote's presented identity
    /// (cert pin) didn't match what this plan expected — always
    /// security-sensitive, never silently retried or routed around.
    #[error("helper identity does not match the expected certificate pin")]
    HelperIdentityMismatch,
    /// The QUIC connection was established but the service target (e.g.
    /// `127.0.0.1:22` on the remote) refused or reset the relayed
    /// connection.
    #[error("service target unreachable from the remote helper")]
    TargetUnreachable,
    /// Writing this plan's confirmed candidate(s) to persistent storage
    /// failed (disk full, permission denied, concurrent writer). The
    /// bootstrap itself may have otherwise succeeded.
    #[error("failed to persist bootstrap result: {0}")]
    PersistenceFailed(String),
}

impl BootstrapFailure {
    /// Whether retrying the *same* route (no route change, no new
    /// credential) has a chance of succeeding — true only for failures that
    /// plausibly reflect transient network/timing conditions rather than a
    /// configuration, trust, or auth problem that a blind retry cannot fix.
    pub fn may_retry(&self) -> bool {
        matches!(
            self,
            Self::JumpHostUnreachable
                | Self::CandidateExchangeFailed
                | Self::StunUnreachable
                | Self::RelayUnavailable
                | Self::QuicHandshakeFailed
                | Self::TargetUnreachable
        )
    }

    /// Whether the caller should prompt the user to run `isekai-ssh login`
    /// (refresh/obtain a relay credential) before trying again.
    pub fn should_redirect_to_login(&self) -> bool {
        matches!(self, Self::TokenExpired | Self::RelayUnauthorized)
    }

    /// Whether the caller should prompt the user to run `isekai-ssh init`
    /// (interactive trust/credential setup) rather than retry
    /// automatically.
    pub fn should_redirect_to_init(&self) -> bool {
        matches!(
            self,
            Self::AuthenticationRequired | Self::HostKeyRejected | Self::RemoteBinaryMissing | Self::RemoteBinaryUntrusted
        )
    }

    /// Whether a direct/STUN-route failure of this kind justifies falling
    /// back to the relay route within the same plan (rather than treating
    /// the whole bootstrap attempt as failed). `false` for failures that
    /// would recur identically on a relay attempt too (an unreachable jump
    /// host blocks deploying `isekai-pipe serve` regardless of which route
    /// is later dialed) or that are security-sensitive.
    pub fn should_fallback_to_relay(&self) -> bool {
        matches!(self, Self::StunUnreachable | Self::TargetUnreachable | Self::QuicHandshakeFailed | Self::CandidateExchangeFailed)
    }
}

/// Classifies an `isekai_bootstrap::BootstrapBackend::install_and_start`
/// failure into a [`BootstrapFailure`], per `ISEKAI_PIPE_DESIGN.md` §8 Epic I
/// ("BootstrapFailure分類をwrapper.rsに配線"). `BootstrapError` doesn't carry
/// enough structure to distinguish every real-world cause (`ssh(1)` reports
/// most of it as opaque stderr text, and this crate deliberately never
/// string-matches error messages — see the module doc), so a couple of
/// choices below are a documented best fit rather than a certainty:
///
/// - `Io`/`HandshakeMissing`: no confirmed response from the remote at all,
///   treated as connectivity (`JumpHostUnreachable`) even though `Io` can
///   also mean `ssh(1)` itself is missing locally — retrying is harmless
///   either way and is the more actionable default.
/// - `UploadFailed`: whatever the root cause, the direct consequence is that
///   `isekai-pipe` never landed on the remote, matching `RemoteBinaryMissing`
///   (which already routes to `isekai-ssh init`, an interactive flow better
///   equipped to surface the underlying `ssh(1)` stderr than a wrapper retry
///   loop).
/// - `UnexpectedStdout`/`HandshakeParse`: some response was received but
///   couldn't be trusted (stdout-purity violation or schema mismatch) —
///   bucketed with `RemoteBinaryUntrusted`, which is never auto-retried or
///   routed around, matching the "don't execute what you can't validate"
///   contract those two error variants exist to enforce.
///
/// Returns `None` for `InvalidRelayParam`/`InvalidRemotePath`/
/// `InvalidRemoteLogLevel`: those are local argument-validation failures
/// caught before ever contacting the remote host, i.e. a plan/config problem
/// rather than a bootstrap *attempt* failure — the thing this classification
/// exists to describe (honest gap, not silently mapped to the nearest
/// variant).
pub fn classify_bootstrap_error(err: &isekai_bootstrap::BootstrapError) -> Option<BootstrapFailure> {
    use isekai_bootstrap::BootstrapError as E;
    match err {
        E::Io(_) => Some(BootstrapFailure::JumpHostUnreachable),
        E::UploadFailed { .. } => Some(BootstrapFailure::RemoteBinaryMissing),
        E::HandshakeMissing { .. } => Some(BootstrapFailure::JumpHostUnreachable),
        E::UnexpectedStdout { .. } => Some(BootstrapFailure::RemoteBinaryUntrusted),
        E::HandshakeParse(_) => Some(BootstrapFailure::RemoteBinaryUntrusted),
        // `uname -m` (a supporting probe, not the upload/launch step itself)
        // failed to run at all — a connectivity-shaped failure, same bucket
        // as `HandshakeMissing`.
        E::RemoteCommandFailed { .. } => Some(BootstrapFailure::JumpHostUnreachable),
        // No pre-built `isekai-pipe` exists for this remote's architecture —
        // there is no upload step this plan can perform, matching
        // `RemoteBinaryMissing`'s own doc comment.
        E::UnsupportedArch(_) => Some(BootstrapFailure::RemoteBinaryMissing),
        E::InvalidRelayParam(_) | E::InvalidRemotePath(_) | E::InvalidRemoteLogLevel(_) => None,

        // ── `RusshBackend`-only variants below (`fancy-humming-pnueli.md` M3) ──
        // `UnsupportedViaChain`/`ConfigResolve`/`TrustStorePath` are local
        // plan/environment problems caught before (or instead of) ever
        // attempting an SSH connection — same rationale as
        // `InvalidRelayParam` et al. above, not a bootstrap *attempt*
        // failure.
        E::UnsupportedViaChain { .. } | E::ConfigResolve { .. } | E::TrustStorePath(_) => None,
        // No username/credential/home-dir resolvable for a hop — the exact
        // "no usable SSH credential" case `AuthenticationRequired`'s own doc
        // comment describes.
        E::NoUsername { .. } | E::NoCredential { .. } | E::NoHomeDir => Some(BootstrapFailure::AuthenticationRequired),
        E::Session(session_err) => classify_session_error(session_err),
    }
}

/// Sub-classifies `russh_stream_session::SessionError` (`RusshBackend`'s
/// connect/authenticate/channel failures) into the same `BootstrapFailure`
/// buckets `classify_bootstrap_error` uses for `OpenSshBackend`'s `ssh(1)`-
/// shaped failures, so a caller (`isekai-ssh`'s auto-bootstrap recovery,
/// `always-connects.md`) doesn't need to know which backend actually ran.
fn classify_session_error(err: &russh_stream_session::SessionError) -> Option<BootstrapFailure> {
    use russh_stream_session::SessionError as S;
    match err {
        // `FileBackedHostKeyVerifier::verify` returning `false` (an unknown
        // key the user declined, or a mismatched/changed key) makes russh
        // fail the handshake with `Error::UnknownKey`, which surfaces here
        // wrapped in `Connect` (direct) or `JumpHandshake` (via a jump host).
        // That is a trust decision (potential MITM, or a legitimate
        // redeploy), not a connectivity blip: route it to `HostKeyRejected`
        // (`may_retry=false`, → `isekai-ssh init`) instead of blindly
        // auto-retrying it as an unreachable host.
        S::Connect { source, .. } | S::JumpHandshake { source, .. } if matches!(source, russh::Error::UnknownKey) => {
            Some(BootstrapFailure::HostKeyRejected)
        }
        S::Connect { .. } | S::JumpTunnel { .. } | S::JumpHandshake { .. } | S::Handshake(_) | S::Channel(_) => {
            Some(BootstrapFailure::JumpHostUnreachable)
        }
        S::JumpAuthFailed { .. } | S::Auth(_) | S::AgentAuth(_) | S::InvalidPrivateKey(_) => {
            Some(BootstrapFailure::AuthenticationRequired)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_token_and_relay_unauthorized_redirect_to_login_only() {
        for f in [BootstrapFailure::TokenExpired, BootstrapFailure::RelayUnauthorized] {
            assert!(f.should_redirect_to_login(), "{f:?}");
            assert!(!f.should_redirect_to_init(), "{f:?}");
            assert!(!f.may_retry(), "{f:?}");
        }
    }

    #[test]
    fn trust_and_deployment_failures_redirect_to_init_only() {
        for f in [
            BootstrapFailure::AuthenticationRequired,
            BootstrapFailure::HostKeyRejected,
            BootstrapFailure::RemoteBinaryMissing,
            BootstrapFailure::RemoteBinaryUntrusted,
        ] {
            assert!(f.should_redirect_to_init(), "{f:?}");
            assert!(!f.should_redirect_to_login(), "{f:?}");
            assert!(!f.may_retry(), "{f:?}");
            assert!(!f.should_fallback_to_relay(), "{f:?}");
        }
    }

    #[test]
    fn connectivity_failures_may_retry() {
        for f in [
            BootstrapFailure::JumpHostUnreachable,
            BootstrapFailure::CandidateExchangeFailed,
            BootstrapFailure::StunUnreachable,
            BootstrapFailure::RelayUnavailable,
            BootstrapFailure::QuicHandshakeFailed,
            BootstrapFailure::TargetUnreachable,
        ] {
            assert!(f.may_retry(), "{f:?}");
        }
    }

    #[test]
    fn only_direct_stun_connectivity_failures_suggest_relay_fallback() {
        assert!(BootstrapFailure::StunUnreachable.should_fallback_to_relay());
        assert!(BootstrapFailure::TargetUnreachable.should_fallback_to_relay());
        assert!(BootstrapFailure::QuicHandshakeFailed.should_fallback_to_relay());
        assert!(BootstrapFailure::CandidateExchangeFailed.should_fallback_to_relay());

        // An unreachable jump host blocks deploying isekai-pipe serve at
        // all, so a relay attempt would fail the exact same way — no
        // fallback should be suggested.
        assert!(!BootstrapFailure::JumpHostUnreachable.should_fallback_to_relay());
        // Already a relay-route failure; there is no further route to fall
        // back to.
        assert!(!BootstrapFailure::RelayUnavailable.should_fallback_to_relay());
        assert!(!BootstrapFailure::RelayUnauthorized.should_fallback_to_relay());
    }

    #[test]
    fn helper_identity_mismatch_is_never_auto_actionable() {
        let f = BootstrapFailure::HelperIdentityMismatch;
        assert!(!f.may_retry());
        assert!(!f.should_redirect_to_login());
        assert!(!f.should_redirect_to_init());
        assert!(!f.should_fallback_to_relay());
    }

    #[test]
    fn classifies_bootstrap_error_variants() {
        use isekai_bootstrap::BootstrapError;

        let io = BootstrapError::Io(std::io::Error::other("broken pipe"));
        assert!(matches!(classify_bootstrap_error(&io), Some(BootstrapFailure::JumpHostUnreachable)));

        let upload_failed = BootstrapError::UploadFailed { status: Some(1), stderr: "permission denied".to_string() };
        assert!(matches!(classify_bootstrap_error(&upload_failed), Some(BootstrapFailure::RemoteBinaryMissing)));

        let handshake_missing = BootstrapError::HandshakeMissing { status: None, stderr: String::new() };
        assert!(matches!(classify_bootstrap_error(&handshake_missing), Some(BootstrapFailure::JumpHostUnreachable)));

        let unexpected_stdout = BootstrapError::UnexpectedStdout { extra_lines: 2 };
        assert!(matches!(classify_bootstrap_error(&unexpected_stdout), Some(BootstrapFailure::RemoteBinaryUntrusted)));

        let handshake_parse = BootstrapError::HandshakeParse(isekai_protocol::ProtocolError::UnknownFrameType(0xff));
        assert!(matches!(classify_bootstrap_error(&handshake_parse), Some(BootstrapFailure::RemoteBinaryUntrusted)));

        let invalid_relay = BootstrapError::InvalidRelayParam("bad sni".to_string());
        assert!(classify_bootstrap_error(&invalid_relay).is_none());

        let invalid_path = BootstrapError::InvalidRemotePath("bad path".to_string());
        assert!(classify_bootstrap_error(&invalid_path).is_none());

        let remote_command_failed =
            BootstrapError::RemoteCommandFailed { command: "uname -m".to_string(), status: Some(1), stderr: String::new() };
        assert!(matches!(classify_bootstrap_error(&remote_command_failed), Some(BootstrapFailure::JumpHostUnreachable)));

        let unsupported_arch = BootstrapError::UnsupportedArch("riscv64".to_string());
        assert!(matches!(classify_bootstrap_error(&unsupported_arch), Some(BootstrapFailure::RemoteBinaryMissing)));
    }

    #[test]
    fn host_key_rejection_is_classified_as_host_key_rejected_not_unreachable() {
        use russh_stream_session::SessionError;

        // A host-key rejection on the direct path (russh raises
        // `Error::UnknownKey` when the verifier returns false) must be a
        // trust failure routed to `isekai-ssh init`, never an auto-retried
        // "unreachable jump host".
        let direct = SessionError::Connect { addr: "example.com:22".to_string(), source: russh::Error::UnknownKey };
        assert!(matches!(classify_session_error(&direct), Some(BootstrapFailure::HostKeyRejected)));

        // Same rejection reached via a jump host surfaces as `JumpHandshake`.
        let via_jump =
            SessionError::JumpHandshake { host: "example.com".to_string(), port: 22, source: russh::Error::UnknownKey };
        assert!(matches!(classify_session_error(&via_jump), Some(BootstrapFailure::HostKeyRejected)));

        // A genuine connectivity failure (not a host-key rejection) on the
        // same variants stays `JumpHostUnreachable` — it may be auto-retried.
        let unreachable = SessionError::Connect { addr: "example.com:22".to_string(), source: russh::Error::Disconnect };
        assert!(matches!(classify_session_error(&unreachable), Some(BootstrapFailure::JumpHostUnreachable)));

        let jump_unreachable =
            SessionError::JumpHandshake { host: "example.com".to_string(), port: 22, source: russh::Error::Disconnect };
        assert!(matches!(classify_session_error(&jump_unreachable), Some(BootstrapFailure::JumpHostUnreachable)));
    }

    #[test]
    fn persistence_failure_is_not_auto_actionable_either() {
        let f = BootstrapFailure::PersistenceFailed("disk full".to_string());
        assert!(!f.may_retry());
        assert!(!f.should_redirect_to_login());
        assert!(!f.should_redirect_to_init());
        assert!(!f.should_fallback_to_relay());
        assert_eq!(f.to_string(), "failed to persist bootstrap result: disk full");
    }
}
