//! Relay-only connection establishment: HELLO/proof/ACK against the
//! relay-assigned public address of a remote isekai-helper
//! (`archive/ISEKAI_SSH_DESIGN.md` phase S-0d-1). Mirrors
//! `isekai_link_relay_transport.rs::connect_relay_stream`, minus what is out
//! of scope for this phase: no `resume_client::ReattachableStream` hand-off
//! and no control stream (`RESUME`/session table land in S-4a onward).

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use isekai_protocol::hello::{decode_ack_response, encode_hello, AckResponse};
use log::info;

use isekai_protocol::hello::Proof;

use crate::attempt::{ConnectAttemptError, ConnectAttemptStage, RejectReason};
use crate::error::TransportError;
use crate::proof::compute_proof;
use crate::telemetry::{log_candidate_attempt, CandidateAttempt, CandidateIdentity, CandidateOutcome};
use crate::traits::{ByteStream, QuicConnection, QuicEndpoint, QuicEndpointFactory};
use crate::types::{BindSpec, RemoteSpec};

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
    /// (see `RemoteSpec::server_name`'s docs); kept configurable rather than
    /// hardcoded so a future non-isekai-helper QUIC endpoint could reuse this
    /// function.
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
pub async fn connect_via_relay(
    factory: &dyn QuicEndpointFactory,
    target: &RelayTarget,
) -> Result<Box<dyn ByteStream>, TransportError> {
    let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await?;
    // No resume support on this path (module docs), so there is no grace
    // period to request — `0` ("no preference").
    let (_conn, stream, _proof, _effective_resume_grace_secs) = connect_and_handshake(
        endpoint.as_ref(),
        RemoteSpec {
            addr: target.helper_addr,
            server_name: target.server_name.clone(),
            cert_sha256_hex: target.cert_sha256_hex.clone(),
        },
        &target.session_secret,
        0,
        CandidateIdentity { kind: "relay", source: "n/a", provider: "n/a", id: &target.helper_addr.to_string() },
    )
    .await?;
    Ok(stream)
}

/// The HELLO/proof/ACK handshake itself (`archive/HELPER_PROTOCOL.md` §4), layered on
/// top of an *already-created* `QuicEndpoint`. Shared by `connect_via_relay`
/// (endpoint from a fresh `QuicEndpointFactory::create_endpoint` call),
/// `stun_p2p::connect_stun_p2p` (endpoint wrapping a socket that already did
/// a STUN query + hole-punch probes), and `resume::connect_via_relay_resumable`
/// (Phase S-4c, which additionally needs the live connection and the HELLO
/// proof afterward to open a control stream) — `archive/ISEKAI_SSH_DESIGN.md` calls
/// out that "既存の`connect_via_relay`と同じロジックを再利用できるはず" for both,
/// so this is the one place the handshake itself lives. Callers that don't
/// need the connection/proof (`connect_via_relay`, `connect_stun_p2p`) simply
/// drop them.
pub(crate) async fn connect_and_handshake(
    endpoint: &dyn QuicEndpoint,
    remote: RemoteSpec,
    session_secret: &[u8],
    requested_resume_grace_secs: u32,
    identity: CandidateIdentity<'_>,
) -> Result<(Box<dyn QuicConnection>, Box<dyn ByteStream>, Proof, u32), ConnectAttemptError> {
    // No staggered/parallel candidate racing exists yet (`ISEKAI_PIPE_DESIGN.md`
    // — every attempt today is dialed immediately, one at a time), so
    // `start_delay` is always zero. Recorded now anyway so telemetry log
    // consumers don't need a schema migration once that lands.
    let start_delay = Duration::ZERO;
    let attempt_start = Instant::now();
    let cancelled = |failure_stage: &str,
                     quic_handshake_time: Option<Duration>,
                     authenticated_ready_time: Option<Duration>| {
        log_candidate_attempt(&CandidateAttempt {
            identity,
            start_delay,
            quic_handshake_time,
            authenticated_ready_time,
            failure_stage: Some(failure_stage),
            outcome: CandidateOutcome::Cancelled,
        });
    };

    let conn = match endpoint.connect(remote).await {
        Ok(conn) => conn,
        Err(e) => {
            cancelled("quic-connect", None, None);
            return Err(ConnectAttemptError { stage: ConnectAttemptStage::QuicConnect, source: e });
        }
    };
    let quic_handshake_time = attempt_start.elapsed();

    let proof = match compute_proof(conn.as_ref(), session_secret, b"").await {
        Ok(proof) => proof,
        Err(e) => {
            cancelled("compute-proof", Some(quic_handshake_time), None);
            return Err(ConnectAttemptError { stage: ConnectAttemptStage::ComputeProof, source: e });
        }
    };

    let mut stream = match conn.open_bi().await {
        Ok(stream) => stream,
        Err(e) => {
            cancelled("open-stream", Some(quic_handshake_time), None);
            return Err(ConnectAttemptError { stage: ConnectAttemptStage::OpenStream, source: e });
        }
    };
    if let Err(e) = stream.write_all(&encode_hello(&proof, requested_resume_grace_secs)).await {
        cancelled("hello-write", Some(quic_handshake_time), None);
        return Err(ConnectAttemptError { stage: ConnectAttemptStage::HelloWrite, source: e });
    }

    let ack = match read_ack_response(stream.as_mut()).await {
        Ok(ack) => ack,
        Err(e) => {
            cancelled("ack-read", Some(quic_handshake_time), None);
            return Err(ConnectAttemptError { stage: ConnectAttemptStage::AckRead, source: e });
        }
    };
    match ack {
        AckResponse::Ack { effective_resume_grace_secs } => {
            let authenticated_ready_time = attempt_start.elapsed();
            info!("isekai-transport: HELLO/ACK ok — stream ready for pass-through");
            log_candidate_attempt(&CandidateAttempt {
                identity,
                start_delay,
                quic_handshake_time: Some(quic_handshake_time),
                authenticated_ready_time: Some(authenticated_ready_time),
                failure_stage: None,
                outcome: CandidateOutcome::Selected,
            });
            Ok((conn, stream, proof, effective_resume_grace_secs))
        }
        other => {
            cancelled(&format!("rejected:{other:?}"), Some(quic_handshake_time), None);
            let reason = RejectReason::from_ack_response(other);
            Err(ConnectAttemptError {
                stage: ConnectAttemptStage::Rejected(reason),
                source: TransportError::Rejected(other),
            })
        }
    }
}

/// Reads a full `ACK` frame: the type byte first, then — only when it's
/// `FRAME_ACK` — `RESUME_GRACE_LEN` more bytes, since reject variants stay a
/// bare single byte (`isekai_protocol::hello::decode_ack_response`'s docs).
async fn read_ack_response(stream: &mut dyn ByteStream) -> Result<AckResponse, TransportError> {
    use isekai_protocol::hello::{FRAME_ACK, RESUME_GRACE_LEN};

    let mut type_byte = [0u8; 1];
    read_exact(stream, &mut type_byte).await?;
    let mut full = vec![type_byte[0]];
    if type_byte[0] == FRAME_ACK {
        let mut rest = [0u8; RESUME_GRACE_LEN];
        read_exact(stream, &mut rest).await?;
        full.extend_from_slice(&rest);
    }
    Ok(decode_ack_response(&full)?)
}

/// `ByteStream::read` only guarantees "at most `buf.len()` bytes, possibly
/// fewer"; the 1-byte ACK response needs the usual `read_exact` loop on top.
/// `pub(crate)` so `resume.rs` (control stream / `RESUME_ACK` handshakes) can
/// reuse it instead of re-implementing the same loop a third time.
pub(crate) async fn read_exact(stream: &mut dyn ByteStream, buf: &mut [u8]) -> Result<(), TransportError> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = stream.read(&mut buf[filled..]).await?;
        if n == 0 {
            return Err(TransportError::UnexpectedEof);
        }
        filled += n;
    }
    Ok(())
}
