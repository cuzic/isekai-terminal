//! STUN+SSH rendezvous P2P QUIC connection establishment
//! (`archive/ISEKAI_SSH_DESIGN.md` phase S-0d-2), extracted from `isekai-terminal-core`'s
//! `isekai_stun_p2p_transport.rs`.
//!
//! Scope of this module (mirrors `try_connect_isekai_stun_p2p` /
//! `connect_stun_p2p_stream`, **minus** the parts out of scope for this
//! phase):
//! - Bind a fresh UDP socket, query a STUN server for this socket's own
//!   observed address on it (`isekai_stun::query_stun`).
//! - Send hole-punch probes to the peer's already-known observed address
//!   (simultaneous open).
//! - Reuse that *same* socket as a QUIC endpoint
//!   (`system::quic_endpoint_from_std_socket`) and perform the
//!   HELLO/proof/ACK handshake against the peer
//!   (`relay::connect_and_handshake`, shared with `connect_via_relay`).
//!
//! Explicitly **out of scope** here (`archive/ISEKAI_SSH_DESIGN.md`'s task
//! description for this phase):
//! - The SSH-bootstrap step that actually exchanges `our_observed_addr`/
//!   `peer_addr` out-of-band between the two sides
//!   (`bootstrap_via_ssh_with_punch` on the Android side). Callers of
//!   `connect_stun_p2p` must already know `target.peer_addr` by whatever
//!   means (a future `isekai-bootstrap`/`isekai-ssh` wiring, S-6) — this
//!   crate does not know how to reach a bootstrap channel.
//! - `resume_client::ClientResumeState`/`reattach_fn`/the control stream —
//!   resume support lands in S-4a onward.

use std::net::SocketAddr;
use std::time::Duration;

use log::info;

use isekai_protocol::attach::ConnectionGeneration;
use isekai_protocol::session_id::SessionId;

use crate::attempt::AttemptFailure;
use crate::error::TransportError;
use crate::relay::{connect_and_handshake, random_session_id};
use crate::system::quic_endpoint_from_std_socket;
use crate::traits::ByteStream;
use crate::types::{BindSpec, RemoteSpec};

/// Number of hole-punch probe datagrams sent to the peer's observed address
/// before attempting the QUIC handshake. Matches
/// `isekai_stun_p2p_transport.rs::PUNCH_PROBE_COUNT`.
const PUNCH_PROBE_COUNT: u32 = 5;
/// Interval between hole-punch probes. Matches
/// `isekai_stun_p2p_transport.rs::PUNCH_PROBE_INTERVAL`.
const PUNCH_PROBE_INTERVAL: Duration = Duration::from_millis(150);
/// Payload of each hole-punch probe datagram. The content is never parsed by
/// either side — it exists purely to prime a NAT mapping / trigger
/// simultaneous open — so any fixed byte string works
/// (`isekai_stun_p2p_transport.rs` uses the same literal).
const PUNCH_PROBE_PAYLOAD: &[u8] = b"isekai-punch";

/// Everything `connect_stun_p2p` needs to know about the remote isekai-helper
/// instance reached directly (peer-to-peer, no relay). Mirrors the subset of
/// `isekai_stun_p2p_transport.rs::connect_stun_p2p_stream`'s inputs this
/// crate is responsible for.
#[derive(Debug, Clone)]
pub struct StunP2pTarget {
    /// The peer's (isekai-helper's) own STUN-observed address
    /// (`IsekaiPipeHandshake::stun_observed_addr` on the Android side), obtained
    /// out-of-band by the caller. Exchanging this value is explicitly out of
    /// scope for this crate (`archive/ISEKAI_SSH_DESIGN.md` S-6: a future
    /// `isekai-bootstrap`/`isekai-ssh` concern).
    pub peer_addr: SocketAddr,
    /// TLS SNI / QUIC server name (`RemoteSpec::server_name`'s docs: ignored
    /// by isekai-helper, but required by rustls's API).
    pub server_name: String,
    /// `HandshakeJson::cert_sha256` (already validated by
    /// `isekai_protocol::handshake::decode_handshake_json`).
    pub cert_sha256_hex: String,
    /// Already base64-decoded `HandshakeJson::session_secret`.
    pub session_secret: Vec<u8>,
}

/// Result of a successful `connect_stun_p2p` call: the HELLO/ACK'd byte
/// stream, plus this side's own STUN-observed address — in case the caller
/// still needs to hand it to a signaling/bootstrap channel. Producing that
/// value is this crate's job; wiring it anywhere is not
/// (`archive/ISEKAI_SSH_DESIGN.md` S-6).
pub struct StunP2pConnection {
    pub our_observed_addr: SocketAddr,
    pub stream: Box<dyn ByteStream>,
}

/// Binds a fresh UDP socket, queries `stun_server` for this socket's own
/// observed address, sends hole-punch probes to `target.peer_addr`
/// (simultaneous open — the peer is assumed to be probing this side's
/// observed address at roughly the same time, by whatever out-of-band
/// exchange got `target.peer_addr` to this caller in the first place), then
/// reuses the *same* socket as a QUIC endpoint to perform the HELLO/proof/ACK
/// handshake against `target.peer_addr`.
///
/// Mirrors `isekai_stun_p2p_transport.rs::try_connect_isekai_stun_p2p` +
/// `connect_stun_p2p_stream`'s connection-establishment portion; the
/// SSH-bootstrap step that exchanges observed addresses out-of-band is the
/// caller's responsibility here, not this function's (see module docs).
pub async fn connect_stun_p2p(
    stun_server: SocketAddr,
    target: &StunP2pTarget,
    identity: crate::telemetry::CandidateIdentity<'_>,
) -> Result<StunP2pConnection, TransportError> {
    connect_stun_p2p_with_round(stun_server, target, random_session_id(), ConnectionGeneration::INITIAL, identity)
        .await
        .map_err(AttemptFailure::into_source)
}

/// Like [`connect_stun_p2p`], but takes an externally-provided
/// `session_id`/`generation` instead of generating its own — for `#19`'s
/// direct/relay race, where both candidates must share one round's fencing
/// identity (`AttachArbiter`'s winner rule, `#18`). Classifies failures via
/// [`AttemptFailure`] instead of the plain `TransportError` so a race runner
/// can distinguish pre-attach failures (safe to just let the other candidate
/// win) from ambiguous/terminal ones the same way the sequential fallback
/// connector does (`#25`).
/// One STUN-server candidate as [`connect_stun_p2p_with_fallback`] needs it:
/// which STUN server to query plus the id telemetry logs it under. Every
/// candidate passed to one fallback call dials the *same* `StunP2pTarget`
/// (same peer, same session secret) — only `stun_server` (and therefore this
/// side's own observed address) varies, matching
/// `isekai-pipe-core::candidate::CandidateRoute::StunP2p`'s dedup-identity
/// docs ("same peer, different STUN server" is a different candidate, not a
/// duplicate).
#[derive(Debug, Clone)]
pub struct SequentialStunCandidate {
    pub stun_server: SocketAddr,
    pub candidate_id: String,
}

#[derive(Debug)]
pub enum SequentialStunConnectError {
    /// [`connect_stun_p2p_with_fallback`] was called with an empty candidate
    /// list — a caller bug, not a connectivity failure.
    NoCandidates,
    /// Every candidate failed with a pre-attach reason
    /// (`AttemptFailure::may_retry_pre_fencing() == true`); every one was
    /// tried.
    AllCandidatesFailed { failures: Vec<crate::resume::SequentialFailure> },
    /// A candidate's failure was not safely pre-attach-retryable — stopped
    /// immediately rather than trying the next candidate, exactly like the
    /// original (pre-`#25`) relay fallback's `StoppedEarly` behavior. STUN
    /// P2P has no resume/control-stream concept to converge an ambiguous
    /// failure through (unlike relay's `#25`), so this stays intentionally
    /// simple until real-world STUN failure-mode telemetry (`#13b`) shows a
    /// generation-retry-aware version is actually needed.
    StoppedEarly { candidate_id: String, failure: crate::attempt::AttemptFailure },
}

impl SequentialStunConnectError {
    /// Same any-of semantics as `SequentialConnectError::is_stale_trust_signal`
    /// (`ISEKAI_PIPE_DESIGN.md` §8 Epic N).
    pub fn is_stale_trust_signal(&self) -> bool {
        match self {
            Self::NoCandidates => false,
            Self::AllCandidatesFailed { failures } => failures.iter().any(|f| f.failure.is_stale_trust_signal()),
            Self::StoppedEarly { failure, .. } => failure.is_stale_trust_signal(),
        }
    }
}

impl std::fmt::Display for SequentialStunConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCandidates => write!(f, "no candidates were provided to try"),
            Self::AllCandidatesFailed { failures } => {
                write!(f, "all {} candidate(s) failed:", failures.len())?;
                for failure in failures {
                    write!(f, " [{}: {}]", failure.candidate_id, failure.failure)?;
                }
                Ok(())
            }
            Self::StoppedEarly { candidate_id, failure } => {
                write!(f, "stopped after candidate {candidate_id:?} failed ambiguously or terminally: {failure}")
            }
        }
    }
}

impl std::error::Error for SequentialStunConnectError {}

/// Like [`connect_stun_p2p`], but tries each of `candidates` (each a
/// different STUN server, same `target`) in order and falls back to the next
/// one when a candidate fails in a way that's provably safe to retry
/// (`AttemptFailure::may_retry_pre_fencing`) — mirrors
/// `resume::connect_via_relay_resumable_with_fallback`'s original (`#12`)
/// simplicity rather than its later (`#25`) generation-retry/`MustResume`
/// convergence machinery, since that machinery exists specifically to
/// recover a relay session via `RESUME`, and STUN P2P has no such resume path
/// (module docs).
///
/// Every candidate in one call shares the same `session_id`/
/// `ConnectionGeneration::INITIAL` (`#18-5`'s fencing identity) so the peer
/// can tell a fallback attempt to a different STUN server is still logically
/// the same attach round, not a second concurrent session.
pub async fn connect_stun_p2p_with_fallback(
    target: &StunP2pTarget,
    candidates: &[SequentialStunCandidate],
) -> Result<(StunP2pConnection, SocketAddr), SequentialStunConnectError> {
    if candidates.is_empty() {
        return Err(SequentialStunConnectError::NoCandidates);
    }

    let session_id = random_session_id();
    let mut failures = Vec::new();

    for candidate in candidates {
        let identity = crate::telemetry::CandidateIdentity {
            kind: "stun-p2p",
            source: "config-stun",
            provider: "config-stun",
            id: &candidate.candidate_id,
        };
        match connect_stun_p2p_with_round(candidate.stun_server, target, session_id, ConnectionGeneration::INITIAL, identity)
            .await
        {
            Ok(conn) => return Ok((conn, candidate.stun_server)),
            Err(failure) => {
                if failure.may_retry_pre_fencing() {
                    failures.push(crate::resume::SequentialFailure { candidate_id: candidate.candidate_id.clone(), failure });
                    continue;
                }
                return Err(SequentialStunConnectError::StoppedEarly { candidate_id: candidate.candidate_id.clone(), failure });
            }
        }
    }

    Err(SequentialStunConnectError::AllCandidatesFailed { failures })
}

pub(crate) async fn connect_stun_p2p_with_round(
    stun_server: SocketAddr,
    target: &StunP2pTarget,
    session_id: SessionId,
    generation: ConnectionGeneration,
    identity: crate::telemetry::CandidateIdentity<'_>,
) -> Result<StunP2pConnection, AttemptFailure> {
    let bind_addr = BindSpec::any_ipv4().local_addr;
    let socket = tokio::net::UdpSocket::bind(bind_addr).await.map_err(|source| AttemptFailure::RetryablePreAttach {
        source: TransportError::Bind { addr: bind_addr, source },
    })?;

    let our_observed_addr = isekai_stun::query_stun(&socket, stun_server)
        .await
        .map_err(|source| AttemptFailure::RetryablePreAttach { source: source.into() })?;
    info!("isekai-transport: our STUN-observed address is {our_observed_addr} (via {stun_server})");

    // Simultaneous open: fire a handful of probes at the peer's observed
    // address before attempting the QUIC handshake so both sides' NAT
    // mappings are primed at roughly the same time
    // (`isekai_stun_p2p_transport.rs`'s comment on why this needs to happen
    // on the *same* socket that will become the QUIC endpoint).
    for _ in 0..PUNCH_PROBE_COUNT {
        let _ = socket.send_to(PUNCH_PROBE_PAYLOAD, target.peer_addr).await;
        tokio::time::sleep(PUNCH_PROBE_INTERVAL).await;
    }

    let std_socket = socket.into_std().map_err(|e| AttemptFailure::RetryablePreAttach {
        source: TransportError::SocketSetup(e.to_string()),
    })?;
    let endpoint = quic_endpoint_from_std_socket(std_socket)
        .map_err(|source| AttemptFailure::RetryablePreAttach { source })?;

    let remote = RemoteSpec {
        addr: target.peer_addr,
        server_name: target.server_name.clone(),
        cert_sha256_hex: target.cert_sha256_hex.clone(),
    };
    // No resume support on this path (module docs), so there is no grace
    // period to request — `0` ("no preference").
    let (_conn, stream, _proof, _effective_resume_grace_secs) =
        connect_and_handshake(endpoint.as_ref(), remote, &target.session_secret, session_id, generation, 0, identity)
            .await
            .map_err(AttemptFailure::from)?;

    Ok(StunP2pConnection { our_observed_addr, stream })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resume::SequentialFailure;

    fn stale_failure() -> SequentialFailure {
        SequentialFailure {
            candidate_id: "c1".to_string(),
            failure: AttemptFailure::Terminal {
                source: TransportError::Rejected(isekai_protocol::attach::AttachRejectReason::Auth),
            },
        }
    }

    fn not_stale_failure() -> SequentialFailure {
        SequentialFailure { candidate_id: "c2".to_string(), failure: AttemptFailure::RetryablePreAttach { source: TransportError::UnexpectedEof } }
    }

    #[test]
    fn no_candidates_is_never_a_stale_trust_signal() {
        assert!(!SequentialStunConnectError::NoCandidates.is_stale_trust_signal());
    }

    #[test]
    fn all_candidates_failed_is_stale_if_any_failure_is() {
        assert!(SequentialStunConnectError::AllCandidatesFailed { failures: vec![not_stale_failure(), stale_failure()] }
            .is_stale_trust_signal());
        assert!(!SequentialStunConnectError::AllCandidatesFailed { failures: vec![not_stale_failure()] }.is_stale_trust_signal());
    }

    #[test]
    fn stopped_early_delegates_to_its_failure() {
        assert!(SequentialStunConnectError::StoppedEarly { candidate_id: "c1".to_string(), failure: stale_failure().failure }
            .is_stale_trust_signal());
        assert!(!SequentialStunConnectError::StoppedEarly { candidate_id: "c2".to_string(), failure: not_stale_failure().failure }
            .is_stale_trust_signal());
    }
}
