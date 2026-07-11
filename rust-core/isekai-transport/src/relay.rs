//! Relay-only connection establishment: HELLO/proof/ACK against the
//! relay-assigned public address of a remote isekai-helper
//! (`archive/ISEKAI_SSH_DESIGN.md` phase S-0d-1). Mirrors
//! `isekai_link_relay_transport.rs::connect_relay_stream`, minus what is out
//! of scope for this phase: no `resume_client::ReattachableStream` hand-off
//! and no control stream (`RESUME`/session table land in S-4a onward).

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use isekai_protocol::attach::{
    attach_hello_proof_transcript, decode_attach_response, encode_attach_activate, encode_attach_hello, AttachActivate,
    AttachHello, AttachProof, AttachResponse, AttemptId, ConnectionGeneration, ATTACH_READY_FRAME_LEN,
    ATTEMPT_ID_LEN, FRAME_ATTACH_READY, FRAME_REJECT_STALE_GENERATION, STALE_GENERATION_REJECT_FRAME_LEN,
};
use isekai_protocol::hello::Proof;
use isekai_protocol::session_id::{SessionId, SESSION_ID_LEN};
use log::info;
use quicmux::{AnyByteStream, AnyMuxConnection, AnyMuxEndpoint, AnyMuxFactory, RemoteSpec};
use rand::RngCore;

use crate::attempt::{ConnectAttemptError, ConnectAttemptStage, RejectReason};
use crate::error::TransportError;
use crate::proof::compute_proof;
use crate::telemetry::{log_candidate_attempt, CandidateAttempt, CandidateIdentity, CandidateOutcome};

/// Generates a fresh, random `SessionId` for a brand-new logical session
/// (`#18-4`: the client picks `session_id` before ever connecting, rather
/// than the server assigning one after the fact via `CONTROL_HELLO`/
/// `CONTROL_ACK`). Every candidate that participates in the *same* round of
/// connection attempts (`resume::connect_via_relay_resumable_with_fallback`)
/// must share the one `SessionId` generated for that round — this helper is
/// meant to be called once per round, not once per candidate.
pub(crate) fn random_session_id() -> SessionId {
    let mut bytes = [0u8; SESSION_ID_LEN];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    SessionId::from_bytes(bytes)
}

fn random_attempt_id() -> AttemptId {
    let mut bytes = [0u8; ATTEMPT_ID_LEN];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    AttemptId::from_bytes(bytes)
}

/// Everything `connect_via_relay` needs to know about one specific
/// isekai-helper instance's relay-assigned endpoint. Mirrors the subset of
/// `isekai_link_relay_transport.rs::IsekaiLinkRelayConfig` /
/// `helper_bootstrap::IsekaiPipeHandshake` this crate actually consumes — SSH
/// bootstrap and handshake-JSON parsing are the caller's responsibility
/// (`isekai_protocol::handshake`), not this crate's.
#[derive(Debug, Clone)]
pub struct RelayTarget {
    /// The relay-assigned public address of the remote isekai-helper
    /// (`HandshakeJson::relay_public_addr`), *not* the relay server itself —
    /// by the time this crate is called, the relay's MASQUE tunnel has
    /// already been set up on isekai-helper's side
    /// (`archive/ISEKAI_SSH_DESIGN.md` "isekai-helper・isekai-sshの統合方針").
    pub helper_addr: SocketAddr,
    /// SNI presented during the QUIC handshake. isekai-helper ignores it
    /// (see `quicmux::RemoteSpec::server_name`'s docs); kept configurable
    /// rather than hardcoded so a future non-isekai-helper QUIC endpoint
    /// could reuse this function.
    pub server_name: String,
    /// `HandshakeJson::cert_sha256` (already validated by
    /// `isekai_protocol::handshake::decode_handshake_json`).
    pub cert_sha256_hex: String,
    /// Already base64-decoded `HandshakeJson::session_secret`.
    pub session_secret: Vec<u8>,
}

/// Establishes a fresh QUIC connection to `target.helper_addr`, pinned to
/// `target.cert_sha256_hex`, then performs the HELLO/proof/ACK handshake
/// (`archive/HELPER_PROTOCOL.md` §4) using `isekai_protocol::hello`. On success,
/// returns the already-open bidirectional QUIC stream: from this point on it
/// is a raw byte pass-through to isekai-helper's target TCP connection.
///
/// Deliberately does *not* open a control stream or return a resume-capable
/// handle — `archive/ISEKAI_SSH_DESIGN.md`'s S-0d-1 scope is "HELLO/proof/ACKまでの
/// 接続確立だけでよい"; resume support lands in S-4a.
pub async fn connect_via_relay(factory: &AnyMuxFactory, target: &RelayTarget) -> Result<AnyByteStream, TransportError> {
    let endpoint = factory.create_endpoint(quicmux::BindSpec::any_ipv4()).await?;
    // No resume support on this path (module docs), so there is no grace
    // period to request — `0` ("no preference").
    let (_conn, stream, _proof, _effective_resume_grace_secs) = connect_and_handshake(
        &endpoint,
        RemoteSpec {
            addr: target.helper_addr,
            server_name: target.server_name.clone(),
            cert_sha256_hex: target.cert_sha256_hex.clone(),
        },
        &target.session_secret,
        random_session_id(),
        ConnectionGeneration::INITIAL,
        0,
        CandidateIdentity { kind: "relay", source: "n/a", provider: "n/a", id: &target.helper_addr.to_string() },
    )
    .await?;
    Ok(stream)
}

/// Like [`connect_via_relay`], but also returns the underlying connection
/// (and the plain HELLO proof) instead of dropping them — for a caller that
/// needs to open additional streams on the *same* connection afterward
/// without going through [`resume::connect_via_relay_resumable`]'s bundled,
/// synchronous control-stream establishment (isekai-terminal-core/
/// isekai-transport crate共有化 Phase 1c: `isekai-terminal-core`'s Android
/// transport opens its control stream in a backgrounded, timeout-bounded
/// task instead, specifically to avoid delaying the SSH hand-off when the
/// remote is slow to accept a second stream — a real regression it hit once
/// already, see `rust-core/src/isekai_pipe_quic_transport.rs`'s
/// `connect_isekai_pipe_quic_stream` doc comment). Does **not** open a
/// control stream itself — that remains entirely the caller's job, using
/// [`resume::open_control_stream`] on the returned connection whenever (and
/// however) it wants.
pub async fn connect_via_relay_with_connection(
    factory: &AnyMuxFactory,
    target: &RelayTarget,
) -> Result<(AnyMuxConnection, AnyByteStream, Proof), TransportError> {
    let endpoint = factory.create_endpoint(quicmux::BindSpec::any_ipv4()).await?;
    // No resume-grace preference from this entry point (module docs on
    // `connect_via_relay`) — the caller decides resume policy for itself via
    // whichever `resume::*` functions it calls afterward.
    let (conn, stream, proof, _effective_resume_grace_secs) = connect_and_handshake(
        &endpoint,
        RemoteSpec {
            addr: target.helper_addr,
            server_name: target.server_name.clone(),
            cert_sha256_hex: target.cert_sha256_hex.clone(),
        },
        &target.session_secret,
        random_session_id(),
        ConnectionGeneration::INITIAL,
        0,
        CandidateIdentity { kind: "relay", source: "n/a", provider: "n/a", id: &target.helper_addr.to_string() },
    )
    .await?;
    Ok((conn, stream, proof))
}

/// The generic dial step, plus this crate's own attempt-id generation and
/// `QuicConnect`-stage telemetry/failure-classification around it — split
/// out of what used to be one `connect_and_handshake` function so the actual
/// dial (`endpoint.connect`, now `quicmux::AnyMuxEndpoint::connect`) is
/// visibly just a call into `quicmux`, with everything isekai-specific
/// (ATTACH v2's HELLO/proof/ACK) layered on top by [`attach_handshake`].
/// Kept as a thin wrapper — not two independently-callable public
/// functions — because every current caller wants the combined behavior;
/// splitting it further would just make every call site repeat the same
/// four lines.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn connect_and_handshake(
    endpoint: &AnyMuxEndpoint,
    remote: RemoteSpec,
    session_secret: &[u8],
    session_id: SessionId,
    generation: ConnectionGeneration,
    requested_resume_grace_secs: u32,
    identity: CandidateIdentity<'_>,
) -> Result<(AnyMuxConnection, AnyByteStream, Proof, u32), ConnectAttemptError> {
    // No staggered/parallel candidate racing exists at this layer (`#19`'s
    // `race.rs` handles that one level up, by racing two whole
    // `connect_and_handshake` calls against each other) — every attempt here
    // is dialed immediately, so `start_delay` is always zero. Recorded now
    // anyway so telemetry log consumers don't need a schema migration once
    // that lands.
    let start_delay = Duration::ZERO;
    let attempt_start = Instant::now();
    // Generated up front (independent of the connection) so it's available
    // to the earliest possible failure point (`#13a`: every attempt log line
    // carries this attempt's id).
    let attempt_id = random_attempt_id();

    let conn = match endpoint.connect(remote).await {
        Ok(conn) => conn,
        Err(e) => {
            log_candidate_attempt(&CandidateAttempt {
                identity,
                session_id,
                generation,
                attempt_id,
                start_delay,
                quic_handshake_time: None,
                authenticated_ready_time: None,
                failure_stage: Some("quic-connect"),
                outcome: CandidateOutcome::Cancelled,
            });
            return Err(ConnectAttemptError { stage: ConnectAttemptStage::QuicConnect, source: TransportError::Mux(e) });
        }
    };
    let quic_handshake_time = attempt_start.elapsed();

    attach_handshake(
        conn,
        session_secret,
        session_id,
        generation,
        requested_resume_grace_secs,
        identity,
        attempt_id,
        attempt_start,
        start_delay,
        quic_handshake_time,
    )
    .await
}

/// Everything after the dial: ATTACH v2's HELLO/proof/`AttachReadyV2`/
/// `AttachActivate` handshake (`archive/HELPER_PROTOCOL.md` §4) on an
/// *already-connected* [`AnyMuxConnection`]. Shared by `connect_via_relay`
/// (connection from a fresh `AnyMuxFactory::create_endpoint` call, via
/// [`connect_and_handshake`]), `stun_p2p::connect_stun_p2p` (connection from
/// an endpoint that wrapped a socket that already did a STUN query +
/// hole-punch probes), and `resume::connect_via_relay_resumable` (which
/// additionally needs the live connection and the HELLO proof afterward to
/// open a control stream) — this is the one place the handshake itself
/// lives; callers that don't need the connection/proof simply drop them.
///
/// Takes the dial step's own telemetry context (`attempt_id`/`attempt_start`/
/// `start_delay`/`quic_handshake_time`) rather than measuring its own, so
/// every log line this function emits carries *exactly* the same attempt
/// identity and dial timing [`connect_and_handshake`]'s dial step already
/// established — splitting the dial out must not change what gets logged or
/// when, only where the code that does it lives.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn attach_handshake(
    conn: AnyMuxConnection,
    session_secret: &[u8],
    session_id: SessionId,
    generation: ConnectionGeneration,
    requested_resume_grace_secs: u32,
    identity: CandidateIdentity<'_>,
    attempt_id: AttemptId,
    attempt_start: Instant,
    start_delay: Duration,
    quic_handshake_time: Duration,
) -> Result<(AnyMuxConnection, AnyByteStream, Proof, u32), ConnectAttemptError> {
    let cancelled = |failure_stage: String| {
        log_candidate_attempt(&CandidateAttempt {
            identity,
            session_id,
            generation,
            attempt_id,
            start_delay,
            quic_handshake_time: Some(quic_handshake_time),
            authenticated_ready_time: None,
            failure_stage: Some(&failure_stage),
            outcome: CandidateOutcome::Cancelled,
        });
    };

    // The plain (non-ATTACH-specific) proof, reused as-is by the control
    // stream's `CONTROL_HELLO` (`resume::open_control_stream`) — unrelated to
    // ATTACH v2's own, separately domain-separated proof below.
    let proof = match compute_proof(&conn, session_secret, b"").await {
        Ok(proof) => proof,
        Err(e) => {
            cancelled("compute-proof".to_string());
            return Err(ConnectAttemptError { stage: ConnectAttemptStage::ComputeProof, source: e });
        }
    };

    let transcript = attach_hello_proof_transcript(&session_id, generation, &attempt_id, requested_resume_grace_secs);
    let attach_proof = match compute_proof(&conn, session_secret, &transcript).await {
        Ok(proof) => AttachProof::new(*proof.as_bytes()),
        Err(e) => {
            cancelled("compute-proof".to_string());
            return Err(ConnectAttemptError { stage: ConnectAttemptStage::ComputeProof, source: e });
        }
    };

    let mut stream = match conn.open_bi().await {
        Ok(stream) => stream,
        Err(e) => {
            cancelled("open-stream".to_string());
            return Err(ConnectAttemptError { stage: ConnectAttemptStage::OpenStream, source: TransportError::Mux(e) });
        }
    };
    let hello = AttachHello { session_id, generation, attempt_id, requested_resume_grace_secs, proof: attach_proof };
    if let Err(e) = stream.write_all(&encode_attach_hello(&hello)).await {
        cancelled("hello-write".to_string());
        return Err(ConnectAttemptError { stage: ConnectAttemptStage::HelloWrite, source: TransportError::Mux(e) });
    }

    let response = match read_attach_response(&mut stream).await {
        Ok(response) => response,
        Err(e) => {
            cancelled("ack-read".to_string());
            return Err(ConnectAttemptError { stage: ConnectAttemptStage::AckRead, source: e });
        }
    };
    match response {
        AttachResponse::Ready { attach_token, negotiated_resume_grace_secs, .. } => {
            let activate = AttachActivate { session_id, generation, attempt_id, attach_token };
            if let Err(e) = stream.write_all(&encode_attach_activate(&activate)).await {
                cancelled("activate-write".to_string());
                return Err(ConnectAttemptError { stage: ConnectAttemptStage::ActivateWrite, source: TransportError::Mux(e) });
            }
            let authenticated_ready_time = attempt_start.elapsed();
            info!("isekai-transport: ATTACH_HELLO/AttachReadyV2/AttachActivate ok — stream ready for pass-through");
            log_candidate_attempt(&CandidateAttempt {
                identity,
                session_id,
                generation,
                attempt_id,
                start_delay,
                quic_handshake_time: Some(quic_handshake_time),
                authenticated_ready_time: Some(authenticated_ready_time),
                failure_stage: None,
                outcome: CandidateOutcome::Selected,
            });
            Ok((conn, stream, proof, negotiated_resume_grace_secs))
        }
        AttachResponse::Reject(reason) => {
            cancelled(format!("rejected:{reason:?}"));
            Err(ConnectAttemptError {
                stage: ConnectAttemptStage::Rejected(RejectReason::from_attach_reject(reason)),
                source: TransportError::Rejected(reason),
            })
        }
    }
}

/// Reads a full `AttachResponse`: the type byte first, then — depending on
/// its value — more bytes: `ATTACH_READY_FRAME_LEN - 1` for `AttachReadyV2`,
/// `STALE_GENERATION_REJECT_FRAME_LEN - 1` for `STALE_GENERATION`, or nothing
/// for every other known reject byte (`isekai_protocol::attach::decode_attach_response`'s
/// docs — mirrors the old `read_ack_response`'s two-step read).
async fn read_attach_response(stream: &mut AnyByteStream) -> Result<AttachResponse, TransportError> {
    let mut type_byte = [0u8; 1];
    read_exact(stream, &mut type_byte).await?;
    let mut full = vec![type_byte[0]];
    let extra_len = match type_byte[0] {
        FRAME_ATTACH_READY => ATTACH_READY_FRAME_LEN - 1,
        FRAME_REJECT_STALE_GENERATION => STALE_GENERATION_REJECT_FRAME_LEN - 1,
        _ => 0,
    };
    if extra_len > 0 {
        let mut rest = vec![0u8; extra_len];
        read_exact(stream, &mut rest).await?;
        full.extend_from_slice(&rest);
    }
    Ok(decode_attach_response(&full)?)
}

/// `AnyByteStream::read` only guarantees "at most `buf.len()` bytes, possibly
/// fewer"; the 1-byte ACK response needs the usual `read_exact` loop on top.
/// `pub(crate)` so `resume.rs` (control stream / `RESUME_ACK` handshakes) can
/// reuse it instead of re-implementing the same loop a third time.
pub(crate) async fn read_exact(stream: &mut AnyByteStream, buf: &mut [u8]) -> Result<(), TransportError> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = stream.read(&mut buf[filled..]).await.map_err(TransportError::Mux)?;
        if n == 0 {
            return Err(TransportError::UnexpectedEof);
        }
        filled += n;
    }
    Ok(())
}
