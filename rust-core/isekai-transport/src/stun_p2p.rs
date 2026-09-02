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
//! - Reuse that *same* socket as a QUIC endpoint (via the caller-supplied
//!   `QuicEndpointFactory::wrap_bound_socket` — isekai-terminal-core/
//!   isekai-transport crate共有化 Phase 1c: which concrete `noq::AsyncUdpSocket`
//!   wraps the already-STUN-queried/hole-punched socket is pluggable, so
//!   this module never has to know whether it's running against a plain
//!   `tokio::net::UdpSocket` (CLI, `system::SystemQuicEndpointFactory`) or a
//!   fault-injectable one (Android's own factory)) and perform the
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
//! - Full re-rendezvous after both peers' observed addresses change. This
//!   module can resume by redialing the already-known peer address, but it
//!   still has no bootstrap/signaling channel that would teach the server a
//!   new client address and make it punch back.

use std::net::SocketAddr;
use std::time::Duration;

use log::info;

use isekai_protocol::attach::ConnectionGeneration;
use isekai_protocol::session_id::SessionId;

use isekai_protocol::hello::Proof;

use quicmux::{AnyByteStream, AnyMuxConnection, AnyMuxFactory, AnyMuxRebinder, RemoteSpec};

use crate::attempt::AttemptFailure;
use crate::error::TransportError;
use crate::relay::{connect_and_handshake, random_session_id};

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
/// stream and resume material, plus this side's own STUN-observed address —
/// in case the caller still needs to hand it to a signaling/bootstrap
/// channel. Producing that value is this crate's job; wiring it anywhere is
/// not (`archive/ISEKAI_SSH_DESIGN.md` S-6).
pub struct StunP2pConnection {
    pub our_observed_addr: SocketAddr,
    pub connection: AnyMuxConnection,
    pub stream: AnyByteStream,
    pub proof: Proof,
    pub effective_resume_grace_secs: u32,
    pub network_rebinder: Option<AnyMuxRebinder>,
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
    factory: &AnyMuxFactory,
    stun_server: SocketAddr,
    target: &StunP2pTarget,
    requested_resume_grace_secs: u32,
    identity: crate::telemetry::CandidateIdentity<'_>,
) -> Result<StunP2pConnection, TransportError> {
    connect_stun_p2p_with_round(factory, stun_server, target, random_session_id(), ConnectionGeneration::INITIAL, requested_resume_grace_secs, identity)
        .await
        .map_err(AttemptFailure::into_source)
}

/// Wraps an already-bound `socket` (which the caller must already have
/// STUN-queried and used to send hole-punch probes to `target.peer_addr`
/// on — the *punching* half of [`connect_stun_p2p_with_round`]'s work) as a
/// QUIC endpoint and performs the HELLO/proof/ACK handshake. The other half
/// this function does **not** do — binding a socket and querying STUN on it
/// — for callers that must keep that *exact same* socket alive across an
/// out-of-band step between "learn our own observed address" and "punch +
/// connect": `isekai-terminal-core`'s Android transport must report its own
/// observed address to the SSH bootstrap channel (so the peer starts
/// punching toward it) *before* it can punch back or dial, so it cannot let
/// this crate bind-query-punch-dial as one atomic unit the way
/// `connect_stun_p2p`'s self-contained flow does (isekai-terminal-core/
/// isekai-transport crate共有化 Phase 1c). Resume support for a connection
/// established this way still goes through the plain [`crate::resume::reconnect_and_resume`]
/// against a synthesized `RelayTarget{helper_addr: target.peer_addr, ..}` —
/// see that Android transport's own module docs for why a bare redial (no
/// re-STUN/re-punch) is this mode's accepted resume-capability ceiling.
pub async fn connect_stun_p2p_on_socket(
    factory: &AnyMuxFactory,
    socket: tokio::net::UdpSocket,
    target: &StunP2pTarget,
    identity: crate::telemetry::CandidateIdentity<'_>,
) -> Result<(AnyMuxConnection, AnyByteStream, Proof), TransportError> {
    let endpoint = factory.wrap_bound_socket(socket).await.map_err(TransportError::Mux)?;
    let remote = RemoteSpec {
        addr: target.peer_addr,
        server_name: target.server_name.clone(),
        cert_sha256_hex: target.cert_sha256_hex.clone(),
    };
    // Android's on-socket path owns its resume wiring outside this CLI
    // helper flow; keep the request at `0` ("no preference") here.
    let (conn, stream, proof, _effective_resume_grace_secs) = connect_and_handshake(
        &endpoint, remote, &target.session_secret, random_session_id(), ConnectionGeneration::INITIAL, 0, identity,
    )
    .await?;
    Ok((conn, stream, proof))
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
/// `crate::candidate::CandidateRoute::StunP2p`'s dedup-identity
/// docs ("same peer, different STUN server" is a different candidate, not a
/// duplicate).
#[derive(Debug, Clone)]
pub struct SequentialStunCandidate {
    pub stun_server: SocketAddr,
    pub candidate_id: String,
}

/// Kept as an alias (rather than removed outright) purely so existing
/// callers/tests that name this STUN-specific alias keep compiling —
/// [`connect_stun_p2p_with_fallback`]'s actual error type is
/// [`crate::resume::SequentialConnectError`] itself, reused as-is rather
/// than duplicated under a STUN-specific enum: `NoCandidates`/
/// `AllCandidatesFailed`/`StoppedEarly` are the only variants this STUN path
/// ever constructs (it only performs initial STUN establishment; the caller
/// wires the returned connection/proof into the resume loop afterward), so a
/// verbatim-duplicated 3-variant subset (formerly its own
/// `SequentialStunConnectError` enum here, complete with copy-pasted
/// `Display`/`is_stale_trust_signal` impls) bought nothing over reusing the
/// relay path's type directly. New code should just name
/// [`crate::resume::SequentialConnectError`] directly instead of this alias.
pub type SequentialStunConnectError = crate::resume::SequentialConnectError;

/// Like [`connect_stun_p2p`], but tries each of `candidates` (each a
/// different STUN server, same `target`) in order and falls back to the next
/// one when a candidate fails in a way that's provably safe to retry
/// (`AttemptFailure::may_retry_pre_fencing`) — mirrors
/// `resume::connect_via_relay_resumable_with_fallback`'s original (`#12`)
/// simplicity rather than its later (`#25`) generation-retry/`MustResume`
/// convergence machinery, since that machinery exists specifically to
/// recover after an ambiguous first attach. STUN P2P callers resume only
/// after this function returns a successfully established connection.
///
/// Every candidate in one call shares the same `session_id`/
/// `ConnectionGeneration::INITIAL` (`#18-5`'s fencing identity) so the peer
/// can tell a fallback attempt to a different STUN server is still logically
/// the same attach round, not a second concurrent session.
pub async fn connect_stun_p2p_with_fallback(
    factory: &AnyMuxFactory,
    target: &StunP2pTarget,
    candidates: &[SequentialStunCandidate],
    requested_resume_grace_secs: u32,
) -> Result<(StunP2pConnection, SocketAddr), crate::resume::SequentialConnectError> {
    if candidates.is_empty() {
        return Err(crate::resume::SequentialConnectError::NoCandidates);
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
        match connect_stun_p2p_with_round(
            factory,
            candidate.stun_server,
            target,
            session_id,
            ConnectionGeneration::INITIAL,
            requested_resume_grace_secs,
            identity,
        )
        .await
        {
            Ok(conn) => return Ok((conn, candidate.stun_server)),
            Err(failure) => {
                if failure.may_retry_pre_fencing() {
                    failures.push(crate::resume::SequentialFailure { candidate_id: candidate.candidate_id.clone(), failure });
                    continue;
                }
                return Err(crate::resume::SequentialConnectError::StoppedEarly { candidate_id: candidate.candidate_id.clone(), failure });
            }
        }
    }

    Err(crate::resume::SequentialConnectError::AllCandidatesFailed { failures })
}

pub(crate) async fn connect_stun_p2p_with_round(
    factory: &AnyMuxFactory,
    stun_server: SocketAddr,
    target: &StunP2pTarget,
    session_id: SessionId,
    generation: ConnectionGeneration,
    requested_resume_grace_secs: u32,
    identity: crate::telemetry::CandidateIdentity<'_>,
) -> Result<StunP2pConnection, AttemptFailure> {
    let bind_addr = quicmux::BindSpec::any_ipv4().local_addr;
    let socket = tokio::net::UdpSocket::bind(bind_addr).await.map_err(|source| AttemptFailure::RetryablePreAttach {
        source: TransportError::Mux(quicmux::MuxError::Bind { addr: bind_addr, source }),
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

    // どの具体的なnoq::AsyncUdpSocketでラップするか(素通しかフォルト注入可能か)は
    // factory実装ごとに異なる(isekai-terminal-core/isekai-transport crate共有化
    // Phase 1c、`quicmux::AnyMuxFactory::wrap_bound_socket`のdocコメント参照)。
    // `qmux`バックエンドではこの呼び出しは常に`MuxError::Unsupported`で失敗する
    // (QMuxはTCP上で動くためbindされたUDPソケットをラップする概念自体が無い、
    // `quicmux::qmux_backend`のdoc参照) — STUN P2Pが実質noqバックエンド限定に
    // なるのはこの層のジェネリックさとは無関係な、backendそのものの制約。
    let endpoint = factory
        .wrap_bound_socket(socket)
        .await
        .map_err(|source| AttemptFailure::RetryablePreAttach { source: TransportError::Mux(source) })?;

    let remote = RemoteSpec {
        addr: target.peer_addr,
        server_name: target.server_name.clone(),
        cert_sha256_hex: target.cert_sha256_hex.clone(),
    };
    let (connection, stream, proof, effective_resume_grace_secs) =
        connect_and_handshake(&endpoint, remote, &target.session_secret, session_id, generation, requested_resume_grace_secs, identity)
            .await
            .map_err(AttemptFailure::from)?;

    // Taken before `endpoint` goes out of scope, matching
    // `resume::connect_via_relay_resumable`: the rebinder clones the
    // endpoint handle it needs to remain usable after this function returns.
    let network_rebinder = endpoint.rebinder();

    Ok(StunP2pConnection {
        our_observed_addr,
        connection,
        stream,
        proof,
        effective_resume_grace_secs,
        network_rebinder,
    })
}

// `is_stale_trust_signal`/`Display` coverage for the `NoCandidates`/
// `AllCandidatesFailed`/`StoppedEarly` variants this module's own
// `connect_stun_p2p_with_fallback` constructs now lives entirely in
// `resume.rs`'s own test module (`SequentialConnectError` is the same type,
// no longer a separately-tested `SequentialStunConnectError` duplicate).
