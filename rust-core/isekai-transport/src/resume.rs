//! Control stream (`CONTROL_HELLO`/`CONTROL_ACK`/`APP_ACK`) and `RESUME`
//! reconnection support (`archive/ISEKAI_SSH_DESIGN.md` Phase S-4c), ported from
//! `rust-core/src/isekai_pipe_quic_transport.rs`'s `open_control_stream` /
//! `spawn_app_ack_tasks` and `rust-core/src/isekai_link_relay_transport.rs`'s
//! `reattach_fn` closure — minus anything that touches `noq` directly,
//! `FaultyUdpSocket`, or `isekai-terminal-core`'s UniFFI types. The control
//! stream's wire format (`CONTROL_HELLO`/`CONTROL_ACK`/`APP_ACK`) matches
//! `archive/HELPER_PROTOCOL.md` §7.4 byte-for-byte (confirmed against the real
//! `isekai-helper` implementation, `isekai-helper/src/main.rs` +
//! `isekai-helper/src/resume.rs`, which is the actual interop target — not
//! just the design doc's prose). `RESUME` itself (`reconnect_and_resume`)
//! moved onto `quicmux::resume`'s generic wire framing
//! (quicmux-server-resume Stage B) — no longer the §7.3 byte layout; see
//! that function's docs.
//!
//! Deliberately **not** ported from `isekai-terminal-core`: the `ReattachableStream`
//! `AsyncRead`/`AsyncWrite` wrapper. That type exists on the Android side
//! purely to present a single object russh can keep driving across a
//! reconnect. `isekai-ssh` has no russh in the loop — it drives its own
//! stdin/stdout pump loops directly — so its reconnect orchestration
//! (replay buffer, backpressure, give-up-after-window) lives in
//! `isekai-ssh` itself and calls the functions here directly rather than
//! going through an `AsyncRead`/`AsyncWrite` facade
//! (`archive/ISEKAI_SSH_DESIGN.md` "進め方": "過度に複雑にしないこと").
//!
//! One deliberate behavioral simplification versus `isekai_pipe_quic_transport.rs`
//! (documented there as an Android-only latency optimization): this module
//! opens the control stream **sequentially**, after the data stream's
//! HELLO/ACK completes, rather than racing it in a background task. Android
//! does that to avoid delaying the SSH handshake hand-off by up to
//! `CONTROL_STREAM_TIMEOUT`; `isekai-ssh` has no such downstream consumer
//! waiting on the data stream alone, so the extra round trip is an
//! acceptable, much simpler trade.
//!
//! Reopening a control stream after a successful `RESUME` is deliberately
//! **not** this module's job: `reconnect_and_resume` performs a single
//! resume attempt and hands back the resumed connection/data stream, but
//! stays agnostic about anything past that — it's each caller's own
//! responsibility to reopen a control stream on the returned connection
//! (`compute_proof` + `open_control_stream` + `spawn_app_ack_tasks`) if it
//! wants `APP_ACK`-based buffer trimming to keep working past the first
//! resume. This used to be a real, previously-undiscovered gap in every
//! caller (`isekai-pipe connect`'s `run_resume_loop`, and
//! `isekai-terminal-core`'s three Android transports) — APP_ACK trimming
//! silently stopped forever after the first resume in each of them — found
//! and fixed in all four places (`quicmux-server-resume`, codex review).
//! `finish_via_resume` below, in this same module, is the one caller that
//! has always gotten this right, and is the reference implementation the
//! other callers' fixes were modeled on.

mod app_ack;

pub use app_ack::{AppAckCounters, AppAckTasks, spawn_app_ack_tasks};

use isekai_protocol::attach::ConnectionGeneration;
use isekai_protocol::hello::Proof;
use isekai_protocol::offset::{C2hHelperCommittedOffset, C2hSentOffset, H2cClientDeliveredOffset, H2cSentOffset};
use isekai_protocol::session_id::{decode_session_id, SessionId, SESSION_ID_LEN};
use log::info;

use quicmux::{AnyByteStream, AnyMuxConnection, AnyMuxFactory, AnyMuxRebinder, RemoteSpec};

use crate::error::TransportError;
use crate::proof::compute_proof;
use crate::relay::{connect_and_handshake, random_session_id, read_exact, RelayTarget};

/// `archive/HELPER_PROTOCOL.md` §7.3 control-stream frame markers. `RESUME`/
/// `RESUME_ACK` themselves now live in `quicmux::resume`
/// (`quicmux-server-resume` Stage B); these three are only used on the
/// control stream itself and never overlap with the data stream's HELLO/ACK
/// vocabulary, so — unlike `RESUME`/`RESUME_ACK` — they didn't need a
/// pure-crate home ahead of time and are defined here, matching
/// `rust-core/src/resume_client.rs`'s `pub(crate)` constants of the
/// same names/values byte-for-byte.
pub const CONTROL_HELLO: u8 = 0x10;
pub const CONTROL_ACK: u8 = 0x11;
/// Used by the `app_ack` submodule's `spawn_app_ack_tasks`, not by anything
/// in this file directly — declared here anyway, alongside its two control-
/// stream siblings, so the trio's shared doc comment above still applies to
/// all three at their one declaration site.
pub const APP_ACK: u8 = 0x12;

const CONTROL_HELLO_FRAME_LEN: usize = 1 + isekai_protocol::hello::PROOF_LEN;
const CONTROL_ACK_FRAME_LEN: usize = 1 + SESSION_ID_LEN;

/// A successfully-established control stream (`archive/ISEKAI_SSH_DESIGN.md`
/// "接続確立順序" step 2), plus the `session_id` isekai-helper echoed back
/// (`#18-4`: the client itself generated this value before ever connecting
/// and already sent it in `ATTACH_HELLO`; `CONTROL_ACK` merely confirms the
/// server recorded the same one).
pub struct ControlStream {
    pub stream: AnyByteStream,
    pub session_id: SessionId,
}

/// Opens a new bidirectional stream on `conn` and performs the
/// `CONTROL_HELLO`/`CONTROL_ACK` exchange (`archive/HELPER_PROTOCOL.md` §7.3),
/// reusing the same `proof` the data stream's `HELLO` already sent — both are
/// computed from the same connection's exporter with an empty `extra`, so
/// they are always equal; recomputing would just waste an HMAC call
/// (`isekai_pipe_quic_transport.rs::open_control_stream`'s same shortcut).
pub async fn open_control_stream(conn: &AnyMuxConnection, proof: &Proof) -> Result<ControlStream, TransportError> {
    let mut stream = conn.open_bi().await.map_err(TransportError::Mux)?;

    let mut hello = Vec::with_capacity(CONTROL_HELLO_FRAME_LEN);
    hello.push(CONTROL_HELLO);
    hello.extend_from_slice(proof.as_bytes());
    stream.write_all(&hello).await.map_err(TransportError::Mux)?;

    let mut ack = [0u8; CONTROL_ACK_FRAME_LEN];
    read_exact(&mut stream, &mut ack).await?;
    if ack[0] != CONTROL_ACK {
        return Err(TransportError::ControlHandshake(format!(
            "unexpected control response byte {:#x}",
            ack[0]
        )));
    }
    let session_id = decode_session_id(&ack[1..CONTROL_ACK_FRAME_LEN])?;
    Ok(ControlStream { stream, session_id })
}

/// The result of establishing a brand-new (non-resumed) relay connection with
/// resume support wired up: the data stream (HELLO/ACK'd, ready for raw
/// pass-through), the control stream (`CONTROL_HELLO`/`CONTROL_ACK`'d, ready
/// for `spawn_app_ack_tasks`), and the `session_id` the caller needs to hold
/// onto for a future `reconnect_and_resume` call. `connection` is also
/// returned so a caller that wants to explicitly `close()` it (e.g. to
/// deliberately simulate a disconnect in a test, or a graceful shutdown) can
/// — the data/control streams keep the connection alive on their own even if
/// this handle is dropped (mirrors `connect_via_relay`'s existing behavior of
/// dropping its own connection handle immediately).
pub struct ResumableRelaySession {
    pub connection: AnyMuxConnection,
    pub data_stream: AnyByteStream,
    pub control_stream: AnyByteStream,
    pub session_id: SessionId,
    /// The resume-grace period the server actually granted
    /// (`min(requested_resume_grace_secs, server's own configured max)`, or
    /// the server's own default when `requested_resume_grace_secs` was `0`)
    /// — callers should bound their own give-up-and-stop-retrying deadline by
    /// this value, not by whatever they originally requested, since the
    /// server will have already discarded the parked session past this point
    /// regardless (`ISEKAI_PIPE_DESIGN.md`).
    pub effective_resume_grace_secs: u32,
    /// A handle to switch this session's underlying mux endpoint to a new
    /// local socket without a full RESUME reconnect, if the endpoint that
    /// produced `connection` supports it (`AnyMuxEndpoint::rebinder`) —
    /// `None` for any backend that doesn't (every one today except `noq`,
    /// see `quicmux::AnyMuxRebinder`'s docs). Tied to *this* connection's
    /// endpoint specifically: after a `reconnect_and_resume` call replaces
    /// `connection`, the caller must take this field's fresh value from that
    /// call's `ResumeAckOutcome`, not keep reusing an old one pointing at an
    /// endpoint that may no longer be driving anything.
    pub network_rebinder: Option<AnyMuxRebinder>,
}

/// Like `relay::connect_via_relay`, but additionally opens the control stream
/// and returns the `session_id` needed to resume later
/// (`archive/ISEKAI_SSH_DESIGN.md` Phase S-4c). Used for the *first* connection to a
/// given isekai-helper instance; `reconnect_and_resume` is used for every
/// subsequent reconnection after a disconnect.
pub async fn connect_via_relay_resumable(
    factory: &AnyMuxFactory,
    target: &RelayTarget,
    requested_resume_grace_secs: u32,
    identity: crate::telemetry::CandidateIdentity<'_>,
) -> Result<ResumableRelaySession, TransportError> {
    let endpoint = factory.create_endpoint(quicmux::BindSpec::any_ipv4().with_port_range(target.local_bind_port_range)).await.map_err(TransportError::Mux)?;
    let (conn, data_stream, proof, effective_resume_grace_secs) = connect_and_handshake(
        &endpoint,
        RemoteSpec {
            addr: target.helper_addr,
            server_name: target.server_name.clone(),
            cert_sha256_hex: target.cert_sha256_hex.clone(),
        },
        &target.session_secret,
        random_session_id(),
        ConnectionGeneration::INITIAL,
        requested_resume_grace_secs,
        identity,
    )
    .await?;

    // Taken before `endpoint` goes out of scope at the end of this function
    // — `AnyMuxEndpoint::rebinder()`'s returned handle clones the underlying
    // engine endpoint (`noq::Endpoint` today), which stays independently
    // usable afterward.
    let network_rebinder = endpoint.rebinder();

    let control = open_control_stream(&conn, &proof).await?;
    info!("isekai-transport: control stream established, session_id={}", control.session_id);

    Ok(ResumableRelaySession {
        connection: conn,
        data_stream,
        control_stream: control.stream,
        session_id: control.session_id,
        effective_resume_grace_secs,
        network_rebinder,
    })
}

/// One relay candidate as `connect_via_relay_resumable_with_fallback` needs
/// it: a dialable target plus the id telemetry logs it under. All candidates
/// passed to one fallback call are assumed to be `CandidateOriginKind::ConfigRelay`
/// (`#12`'s scope: relay-endpoint fallback specifically) — `identity.kind`/
/// `identity.source`/`identity.provider` are therefore fixed to
/// `"relay"`/`"config-relay"`/`"config-relay"` for every candidate in the
/// slice; only `candidate_id` varies.
#[derive(Debug, Clone)]
pub struct SequentialRelayCandidate {
    pub target: RelayTarget,
    pub candidate_id: String,
}

/// One candidate's contribution to an eventual [`SequentialConnectError::AllCandidatesFailed`].
#[derive(Debug)]
pub struct SequentialFailure {
    pub candidate_id: String,
    pub failure: crate::attempt::AttemptFailure,
}

#[derive(Debug)]
pub enum SequentialConnectError {
    /// `connect_via_relay_resumable_with_fallback` was called with an empty
    /// candidate list — a caller bug (the caller must not invoke this with
    /// nothing to try), not a connectivity failure.
    NoCandidates,
    /// Every candidate failed with a pre-attach (or definitively
    /// non-retryable) reason; every one was tried.
    AllCandidatesFailed { failures: Vec<SequentialFailure> },
    /// A candidate's failure was ambiguous or terminal
    /// (`AttemptFailure::may_retry_pre_fencing() == false`) — stopped
    /// immediately rather than silently trying the next candidate (see
    /// `AttemptFailure`'s module docs for why this is the safe behavior
    /// before `#18`'s fencing exists; candidates after this one in the list
    /// were never attempted).
    StoppedEarly { candidate_id: String, failure: crate::attempt::AttemptFailure },
    /// The data stream's `HELLO`/`ACK` succeeded for this candidate (it is
    /// genuinely attached server-side) but opening the control stream
    /// afterward failed. Mirrors `connect_via_relay_resumable`'s existing
    /// all-or-nothing behavior exactly (that function also fails the whole
    /// attempt via `?` in this situation) — trying a *different* candidate
    /// here would abandon a connection that is already live, which is worse
    /// than just surfacing the error; a future task could choose to return
    /// the plain (non-resumable) data stream instead, but that's out of
    /// scope for `#12`.
    AttachedButControlStreamFailed { candidate_id: String, source: TransportError },
    /// `#25-3`: a later candidate was told `ATTACH_ALREADY_ESTABLISHED`
    /// (`AttemptFailure::MustResume`), implying an earlier ambiguous
    /// candidate's attach actually succeeded server-side — but the
    /// subsequent `RESUME` attempt against that earlier candidate's target
    /// itself failed (or its own control-stream open failed).
    MustResumeButResumeFailed { candidate_id: String, source: TransportError },
    /// `#25-2`: an `AmbiguousAfterAttach`/`StaleAttempt` failure kept
    /// recurring until [`crate::generation_coordinator::GenerationCoordinator`]'s
    /// retry budget was exhausted — every generation advance this round was
    /// allowed still ended up ambiguous or stale. Distinct from
    /// `AllCandidatesFailed` (every candidate cleanly failed pre-attach in a
    /// *single* round) — this means the ATTACH control path itself looks
    /// unstable, not that the candidates are simply unreachable.
    GaveUpAfterGenerationRetries {
        failures: Vec<SequentialFailure>,
        budget: crate::generation_coordinator::AdvanceGenerationError,
    },
}

impl SequentialConnectError {
    /// Whether *any* underlying failure captured here looks like stale
    /// cached trust material (`ISEKAI_PIPE_DESIGN.md` §8 Epic N) — any-of
    /// semantics across the failure list, since a single high-confidence
    /// stale-trust signal from any candidate is reason enough for the
    /// caller to consider a re-bootstrap worthwhile.
    pub fn is_stale_trust_signal(&self) -> bool {
        match self {
            Self::NoCandidates => false,
            Self::AllCandidatesFailed { failures } | Self::GaveUpAfterGenerationRetries { failures, .. } => {
                failures.iter().any(|f| f.failure.is_stale_trust_signal())
            }
            Self::StoppedEarly { failure, .. } => failure.is_stale_trust_signal(),
            Self::AttachedButControlStreamFailed { source, .. } | Self::MustResumeButResumeFailed { source, .. } => {
                source.is_stale_trust_signal()
            }
        }
    }

    /// Whether this failure is exactly a `BUSY_OTHER_SESSION` rejection
    /// (`TransportError::is_busy_other_session`'s docs) — the one shape a
    /// `Terminal` `AttemptFailure` here can take, since every candidate in
    /// one round shares the same `session_id` (module docs above): if the
    /// helper is busy with a different one, every candidate reaching it
    /// would fail identically, so this always surfaces as `StoppedEarly`
    /// on the first candidate that reaches it, never `AllCandidatesFailed`.
    /// `isekai-pipe connect`'s initial-connect entry points use this to
    /// decide whether retrying the whole fallback scan (a fresh
    /// `session_id`/round each time) is worth it.
    pub fn is_busy_other_session(&self) -> bool {
        match self {
            Self::StoppedEarly { failure: crate::attempt::AttemptFailure::Terminal { source }, .. } => {
                source.is_busy_other_session()
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for SequentialConnectError {
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
            Self::AttachedButControlStreamFailed { candidate_id, source } => {
                write!(f, "candidate {candidate_id:?} attached but its control stream failed: {source}")
            }
            Self::MustResumeButResumeFailed { candidate_id, source } => {
                write!(
                    f,
                    "candidate {candidate_id:?} reported ATTACH_ALREADY_ESTABLISHED, but resuming the earlier \
                     ambiguous attempt failed: {source}"
                )
            }
            Self::GaveUpAfterGenerationRetries { failures, budget } => {
                write!(f, "gave up after generation retries were exhausted ({budget}); {} pre-attach failure(s) along the way:", failures.len())?;
                for failure in failures {
                    write!(f, " [{}: {}]", failure.candidate_id, failure.failure)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for SequentialConnectError {}

/// Like [`connect_via_relay_resumable`], but tries each of `candidates` in
/// order and falls back to the next one when a candidate fails in a way
/// that's provably safe to retry within the same generation
/// (`AttemptFailure::may_retry_pre_fencing`, `ISEKAI_PIPE_DESIGN.md` task
/// `#12`) — i.e. nothing was ever sent to that candidate's server, so trying
/// a different candidate cannot cause a double-attach.
///
/// An `AmbiguousAfterAttach`/`StaleAttempt` failure no longer stops the whole
/// attempt (`#25-2`, now that `#18`'s fencing exists): the
/// [`crate::generation_coordinator::GenerationCoordinator`] advances to a new
/// generation — safely superseding the earlier, not-yet-`Established`
/// attempt server-side (`AttachArbiter::ClosingForSupersede`, `#18`) — and a
/// fresh round starts from the *next* candidate after the one that went
/// ambiguous, wrapping around the candidate list (ring rotation) rather than
/// giving up on that candidate forever or fixating on retrying it
/// immediately. A `DefinitiveRejectNotRetryable`/`Terminal`/`LostRace` failure
/// still stops the whole attempt immediately — see `AttemptFailure`'s module
/// docs. `MustResume` also still stops here for now (`#25-3` wires it into an
/// actual resume attempt).
///
/// Returns the winning candidate's `RelayTarget` alongside the session — a
/// later disconnect must be resumed (`reconnect_and_resume`) against that
/// *same* candidate specifically, not by re-running the whole fallback scan
/// (resume is scoped to one already-established `session_id` on one specific
/// helper connection, not a fresh candidate search).
pub async fn connect_via_relay_resumable_with_fallback(
    factory: &AnyMuxFactory,
    candidates: &[SequentialRelayCandidate],
    requested_resume_grace_secs: u32,
) -> Result<(ResumableRelaySession, RelayTarget), SequentialConnectError> {
    if candidates.is_empty() {
        return Err(SequentialConnectError::NoCandidates);
    }

    // `#18-5`/`#25-1`: every candidate in one round shares the same
    // `session_id`/`generation` (`GenerationCoordinator`'s `current_round()`)
    // so the server can tell that a fallback attempt to a *different*
    // candidate is still logically the same attach round as the one before
    // it (`AttachArbiter`'s winner rule, `#18`). Only the coordinator is
    // allowed to advance `generation`, and only after an
    // `AmbiguousAfterAttach`/`StaleAttempt` failure — never for a
    // `RetryablePreAttach` one.
    let mut coordinator = crate::generation_coordinator::GenerationCoordinator::new(random_session_id());
    let mut start_index = 0usize;
    let mut failures = Vec::new();
    // `#25-3`: the most recent candidate whose attach went ambiguous — if a
    // *later* candidate is told `ATTACH_ALREADY_ESTABLISHED`, this is the
    // candidate whose earlier, unconfirmed attempt most plausibly actually
    // succeeded server-side (the client just never found out), so this is
    // who a resume attempt should target.
    let mut last_ambiguous_target: Option<RelayTarget> = None;

    'rounds: loop {
        let round = coordinator.current_round();
        for offset in 0..candidates.len() {
            let idx = (start_index + offset) % candidates.len();
            let candidate = &candidates[idx];
            let identity = crate::telemetry::CandidateIdentity {
                kind: "relay",
                source: "config-relay",
                provider: "config-relay",
                id: &candidate.candidate_id,
            };

            let endpoint = match factory.create_endpoint(quicmux::BindSpec::any_ipv4().with_port_range(candidate.target.local_bind_port_range)).await {
                Ok(endpoint) => endpoint,
                Err(source) => {
                    // Binding our own local socket never touches the remote
                    // server at all — unconditionally safe to move on.
                    failures.push(SequentialFailure {
                        candidate_id: candidate.candidate_id.clone(),
                        failure: crate::attempt::AttemptFailure::RetryablePreAttach { source: TransportError::Mux(source) },
                    });
                    continue;
                }
            };

            let attempt = connect_and_handshake(
                &endpoint,
                RemoteSpec {
                    addr: candidate.target.helper_addr,
                    server_name: candidate.target.server_name.clone(),
                    cert_sha256_hex: candidate.target.cert_sha256_hex.clone(),
                },
                &candidate.target.session_secret,
                round.session_id,
                round.generation,
                requested_resume_grace_secs,
                identity,
            )
            .await;

            let (conn, data_stream, proof, effective_resume_grace_secs) = match attempt {
                Ok(ok) => ok,
                Err(attempt_err) => {
                    let failure = crate::attempt::AttemptFailure::from(attempt_err);
                    match &failure {
                        crate::attempt::AttemptFailure::RetryablePreAttach { .. } => {
                            failures.push(SequentialFailure { candidate_id: candidate.candidate_id.clone(), failure });
                            continue;
                        }
                        crate::attempt::AttemptFailure::AmbiguousAfterAttach { .. } => {
                            last_ambiguous_target = Some(candidate.target.clone());
                            let server_floor = None;
                            match coordinator.advance_generation(server_floor) {
                                Ok(new_round) => {
                                    crate::telemetry::log_generation_advance(
                                        round.session_id,
                                        round.generation,
                                        new_round.generation,
                                        "ambiguous",
                                        coordinator.generation_advances(),
                                        coordinator.max_generation_advances(),
                                    );
                                    start_index = (idx + 1) % candidates.len();
                                    continue 'rounds;
                                }
                                Err(budget) => {
                                    return Err(SequentialConnectError::GaveUpAfterGenerationRetries {
                                        failures,
                                        budget,
                                    });
                                }
                            }
                        }
                        crate::attempt::AttemptFailure::StaleAttempt { current_generation, .. } => {
                            match coordinator.advance_generation(Some(*current_generation)) {
                                Ok(new_round) => {
                                    crate::telemetry::log_generation_advance(
                                        round.session_id,
                                        round.generation,
                                        new_round.generation,
                                        "stale",
                                        coordinator.generation_advances(),
                                        coordinator.max_generation_advances(),
                                    );
                                    start_index = (idx + 1) % candidates.len();
                                    continue 'rounds;
                                }
                                Err(budget) => {
                                    return Err(SequentialConnectError::GaveUpAfterGenerationRetries {
                                        failures,
                                        budget,
                                    });
                                }
                            }
                        }
                        crate::attempt::AttemptFailure::MustResume { .. } => {
                            let Some(resume_target) = last_ambiguous_target.clone() else {
                                // No prior ambiguous candidate in this round
                                // plausibly caused this — nothing sensible to
                                // resume against.
                                return Err(SequentialConnectError::StoppedEarly {
                                    candidate_id: candidate.candidate_id.clone(),
                                    failure,
                                });
                            };
                            crate::telemetry::log_must_resume_convergence(round.session_id, &candidate.candidate_id);
                            return finish_via_resume(
                                factory,
                                &resume_target,
                                round.session_id,
                                candidate.candidate_id.clone(),
                                requested_resume_grace_secs,
                            )
                            .await;
                        }
                        _ => {
                            return Err(SequentialConnectError::StoppedEarly {
                                candidate_id: candidate.candidate_id.clone(),
                                failure,
                            });
                        }
                    }
                }
            };

            // Taken before `endpoint` goes out of scope at the end of this
            // loop iteration — see `connect_via_relay_resumable`'s identical
            // comment on why that's safe.
            let network_rebinder = endpoint.rebinder();

            let control = match open_control_stream(&conn, &proof).await {
                Ok(control) => control,
                Err(source) => {
                    return Err(SequentialConnectError::AttachedButControlStreamFailed {
                        candidate_id: candidate.candidate_id.clone(),
                        source,
                    });
                }
            };
            info!(
                "isekai-transport: control stream established, session_id={}, candidate_id={}",
                control.session_id, candidate.candidate_id
            );

            let session = ResumableRelaySession {
                connection: conn,
                data_stream,
                control_stream: control.stream,
                session_id: control.session_id,
                effective_resume_grace_secs,
                network_rebinder,
            };
            return Ok((session, candidate.target.clone()));
        }

        // Every candidate in this round failed `RetryablePreAttach` — no
        // ambiguous/stale failure occurred, so there's no reason to advance
        // the generation and try again.
        return Err(SequentialConnectError::AllCandidatesFailed { failures });
    }
}

/// `#25-3`: the `MustResume` convergence path. `resume_target` is the
/// earlier candidate whose ambiguous attach most plausibly actually
/// succeeded server-side; dials it fresh and issues `RESUME` for
/// `session_id` with zero offsets (this client never got far enough into
/// that earlier attempt to have sent or received any application data —
/// only the ambiguous attach itself may have completed), then opens a
/// control stream on the resumed connection so the returned
/// `ResumableRelaySession` is just as resume-capable as a freshly attached
/// one going forward.
async fn finish_via_resume(
    factory: &AnyMuxFactory,
    resume_target: &RelayTarget,
    session_id: SessionId,
    candidate_id: String,
    requested_resume_grace_secs: u32,
) -> Result<(ResumableRelaySession, RelayTarget), SequentialConnectError> {
    let resumed = reconnect_and_resume(factory, resume_target, session_id, C2hSentOffset::ZERO, H2cClientDeliveredOffset::ZERO)
        .await
        .map_err(|source| SequentialConnectError::MustResumeButResumeFailed { candidate_id: candidate_id.clone(), source })?;

    let proof = compute_proof(&resumed.connection, &resume_target.session_secret, b"")
        .await
        .map_err(|source| SequentialConnectError::MustResumeButResumeFailed { candidate_id: candidate_id.clone(), source })?;
    let control = open_control_stream(&resumed.connection, &proof)
        .await
        .map_err(|source| SequentialConnectError::MustResumeButResumeFailed { candidate_id, source })?;

    info!(
        "isekai-transport: resumed after MustResume convergence, session_id={}, helper_committed_offset={}",
        control.session_id, resumed.helper_committed_offset
    );

    let session = ResumableRelaySession {
        connection: resumed.connection,
        data_stream: resumed.data_stream,
        control_stream: control.stream,
        session_id: control.session_id,
        // No fresh negotiation happens on a bare RESUME/RESUME_ACK exchange
        // (unlike the initial ATTACH_HELLO/AttachReadyV2), so the server-
        // granted value from the original (ambiguous) attach was never
        // observed by this client — falling back to `requested_resume_grace_secs`
        // (the caller's own original request) rather than `0`: callers like
        // `isekai-pipe connect`'s `run_resume_loop` treat this field as a
        // literal deadline in seconds, not a "0 means unknown" sentinel
        // (`ISEKAI_PIPE_DESIGN.md`), so `0` here previously made any session
        // that reached this convergence path give up on its very next
        // disconnect instead of ever attempting to resume (codex review,
        // quicmux-server-resume). The server may have actually granted less
        // than what was requested (it clamps to its own configured max), so
        // this is still only an approximation — but a strictly better one
        // than a hardcoded `0`.
        effective_resume_grace_secs: requested_resume_grace_secs,
        network_rebinder: resumed.network_rebinder,
    };
    Ok((session, resume_target.clone()))
}

/// The result of a successful `RESUME` (`archive/HELPER_PROTOCOL.md` §7.3): a fresh
/// QUIC connection and its first (and, per this module's simplification, only
/// — see module docs) bidirectional stream, now a raw data-stream
/// pass-through exactly like a fresh `HELLO`/`ACK`'d connection, plus the
/// offsets isekai-helper reports so the caller knows what it may safely
/// discard from its own C2H replay buffer (`helper_committed_offset`) and,
/// for diagnostics/consistency checking, how much it already sent
/// (`helper_sent_offset`).
pub struct ResumeAckOutcome {
    pub connection: AnyMuxConnection,
    pub data_stream: AnyByteStream,
    pub helper_committed_offset: C2hHelperCommittedOffset,
    pub helper_sent_offset: H2cSentOffset,
    /// See `ResumableRelaySession::network_rebinder` — this reconnect made a
    /// brand-new endpoint, so this is a fresh handle onto *that* one, not a
    /// stale reference to whatever endpoint the previous connection used.
    pub network_rebinder: Option<AnyMuxRebinder>,
}

/// Maps `quicmux::ResumeRejectReason`(意味論としては解釈しない、ワイヤ上の
/// 型でしかない)を、このcrate自身の`TransportError::ResumeRejected`が
/// 使い続けている`isekai_protocol::resume::ResumeRejectReason`(3値とも
/// 1:1で対応)に変換する。quicmux-server-resume Stage Bでワイヤフォーマット
/// 自体はquicmux::resumeへ移行したが、呼び出し側(`isekai-ssh`等)から見た
/// このcrateの公開エラー型は変えない。`pub(crate)`: `warm_standby.rs`も
/// standby連接の直接resumeで同じ変換が必要なため。
pub(crate) fn map_reject_reason(reason: quicmux::ResumeRejectReason) -> isekai_protocol::resume::ResumeRejectReason {
    match reason {
        quicmux::ResumeRejectReason::Auth => isekai_protocol::resume::ResumeRejectReason::Auth,
        quicmux::ResumeRejectReason::UnknownToken => isekai_protocol::resume::ResumeRejectReason::UnknownSession,
        quicmux::ResumeRejectReason::OffsetGone => isekai_protocol::resume::ResumeRejectReason::OffsetGone,
    }
}

/// Dials a brand-new QUIC connection to `target.helper_addr` and issues a
/// `quicmux::resume` RESUME request on its first bidirectional stream
/// (quicmux-server-resume Stage B — previously isekai's own `archive/
/// HELPER_PROTOCOL.md` §7.3 byte layout; see this module's docs). Despite
/// the historical "control stream 先頭" naming in the design doc, this is
/// the *first* stream opened on the new connection, not a stream opened
/// alongside/after a fresh HELLO; the real `isekai-helper` implementation
/// treats whichever frame type arrives first on the first stream as either a
/// new-session `ATTACH_HELLO` or a `quicmux::resume::FRAME_RESUME`, see
/// `isekai-pipe`'s `engine/mod.rs::handle_connection`).
///
/// `client_sent_offset`/`client_delivered_offset` must be the caller's
/// current C2H-sent / H2C-delivered offsets (`archive/ISEKAI_SSH_DESIGN.md`'s
/// naming) — the caller (`isekai-ssh`) owns that bookkeeping; this function
/// only knows how to put them on the wire and parse the response.
pub async fn reconnect_and_resume(
    factory: &AnyMuxFactory,
    target: &RelayTarget,
    session_id: SessionId,
    client_sent_offset: C2hSentOffset,
    client_delivered_offset: H2cClientDeliveredOffset,
) -> Result<ResumeAckOutcome, TransportError> {
    let endpoint = factory.create_endpoint(quicmux::BindSpec::any_ipv4().with_port_range(target.local_bind_port_range)).await.map_err(TransportError::Mux)?;
    let conn = endpoint
        .connect(RemoteSpec {
            addr: target.helper_addr,
            server_name: target.server_name.clone(),
            cert_sha256_hex: target.cert_sha256_hex.clone(),
        })
        .await
        .map_err(TransportError::Mux)?;
    // Taken before `endpoint` goes out of scope at the end of this function
    // — see `connect_via_relay_resumable`'s identical comment.
    let network_rebinder = endpoint.rebinder();

    // `resume_proof = HMAC-SHA256(session_secret, exporter || session_id)`
    // (`archive/HELPER_PROTOCOL.md` §7.3の計算式は不変 — 変わったのは
    // これをquicmux::resumeの`token`/`auth_blob`という完全にopaqueな
    // bytesに載せて送る、というワイヤ上の運び方だけ)。`compute_proof`の
    // `extra`パラメータは、実際の`isekai-helper`(`isekai-pipe serve`)実装が
    // 初回`HELLO`と`RESUME`/`CONTROL_HELLO`の両方で同じexporter labelを
    // 使う(`isekai-pipe/src/engine/mod.rs`のEXPORTER_LABEL)ことに対称に
    // 揃えたもの。
    let resume_proof_bytes = compute_proof(&conn, &target.session_secret, session_id.as_bytes()).await?;

    let outcome = quicmux::request_resume(
        &conn,
        session_id.as_bytes(),
        resume_proof_bytes.as_bytes(),
        client_sent_offset.get(),
        client_delivered_offset.get(),
    )
    .await
    .map_err(|e| match e {
        quicmux::ResumeRequestError::Mux(mux_err) => TransportError::Mux(mux_err),
        quicmux::ResumeRequestError::Rejected(reason) => TransportError::ResumeRejected(map_reject_reason(reason)),
    })?;

    info!(
        "isekai-transport: resume succeeded, session_id={session_id}, helper_committed_offset={}",
        outcome.committed_offset
    );
    Ok(ResumeAckOutcome {
        connection: conn,
        data_stream: outcome.stream,
        helper_committed_offset: C2hHelperCommittedOffset::new(outcome.committed_offset),
        helper_sent_offset: H2cSentOffset::new(outcome.sent_offset),
        network_rebinder,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attempt::AttemptFailure;

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
        assert!(!SequentialConnectError::NoCandidates.is_stale_trust_signal());
    }

    #[test]
    fn all_candidates_failed_is_stale_if_any_failure_is() {
        assert!(SequentialConnectError::AllCandidatesFailed { failures: vec![not_stale_failure(), stale_failure()] }.is_stale_trust_signal());
        assert!(!SequentialConnectError::AllCandidatesFailed { failures: vec![not_stale_failure()] }.is_stale_trust_signal());
    }

    #[test]
    fn stopped_early_delegates_to_its_failure() {
        assert!(SequentialConnectError::StoppedEarly { candidate_id: "c1".to_string(), failure: stale_failure().failure }
            .is_stale_trust_signal());
        assert!(!SequentialConnectError::StoppedEarly { candidate_id: "c2".to_string(), failure: not_stale_failure().failure }
            .is_stale_trust_signal());
    }

    #[test]
    fn attached_but_control_stream_failed_delegates_to_source() {
        assert!(SequentialConnectError::AttachedButControlStreamFailed {
            candidate_id: "c1".to_string(),
            source: TransportError::Rejected(isekai_protocol::attach::AttachRejectReason::Auth),
        }
        .is_stale_trust_signal());
        assert!(!SequentialConnectError::AttachedButControlStreamFailed {
            candidate_id: "c1".to_string(),
            source: TransportError::UnexpectedEof,
        }
        .is_stale_trust_signal());
    }

    #[test]
    fn must_resume_but_resume_failed_delegates_to_source() {
        assert!(SequentialConnectError::MustResumeButResumeFailed {
            candidate_id: "c1".to_string(),
            source: TransportError::Mux(quicmux::MuxError::CertPinMismatch { expected: "a".to_string(), got: "b".to_string() }),
        }
        .is_stale_trust_signal());
    }

    #[test]
    fn gave_up_after_generation_retries_is_stale_if_any_failure_is() {
        assert!(SequentialConnectError::GaveUpAfterGenerationRetries {
            failures: vec![stale_failure()],
            budget: crate::generation_coordinator::AdvanceGenerationError::RetryBudgetExceeded { advances: 3, max: 3 },
        }
        .is_stale_trust_signal());
        assert!(!SequentialConnectError::GaveUpAfterGenerationRetries {
            failures: vec![not_stale_failure()],
            budget: crate::generation_coordinator::AdvanceGenerationError::RetryBudgetExceeded { advances: 3, max: 3 },
        }
        .is_stale_trust_signal());
    }

    fn busy_other_session_failure() -> AttemptFailure {
        AttemptFailure::Terminal { source: TransportError::Rejected(isekai_protocol::attach::AttachRejectReason::BusyOtherSession) }
    }

    #[test]
    fn stopped_early_with_busy_other_session_is_recognized() {
        assert!(SequentialConnectError::StoppedEarly { candidate_id: "c1".to_string(), failure: busy_other_session_failure() }
            .is_busy_other_session());
    }

    #[test]
    fn stopped_early_with_other_terminal_failures_is_not_busy_other_session() {
        assert!(!SequentialConnectError::StoppedEarly { candidate_id: "c1".to_string(), failure: stale_failure().failure }
            .is_busy_other_session());
        assert!(!SequentialConnectError::StoppedEarly { candidate_id: "c2".to_string(), failure: not_stale_failure().failure }
            .is_busy_other_session());
    }

    #[test]
    fn non_stopped_early_variants_are_never_busy_other_session() {
        assert!(!SequentialConnectError::NoCandidates.is_busy_other_session());
        assert!(!SequentialConnectError::AllCandidatesFailed { failures: vec![stale_failure()] }.is_busy_other_session());
    }
}
