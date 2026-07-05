//! Control stream (`CONTROL_HELLO`/`CONTROL_ACK`/`APP_ACK`) and `RESUME`
//! reconnection support (`ISEKAI_SSH_DESIGN.md` Phase S-4c), ported from
//! `rust-core/src/helper_quic_transport.rs`'s `open_control_stream` /
//! `spawn_app_ack_tasks` and `rust-core/src/isekai_link_relay_transport.rs`'s
//! `reattach_fn` closure — minus anything that touches `noq` directly,
//! `FaultyUdpSocket`, or `isekai-terminal-core`'s UniFFI types. The wire format matches
//! `HELPER_PROTOCOL.md` §7.3/§7.4 byte-for-byte (confirmed against the real
//! `isekai-helper` implementation, `isekai-helper/src/main.rs` +
//! `isekai-helper/src/resume.rs`, which is the actual interop target — not
//! just the design doc's prose).
//!
//! Deliberately **not** ported from `isekai-terminal-core`: the `ReattachableStream`
//! `AsyncRead`/`AsyncWrite` wrapper. That type exists on the Android side
//! purely to present a single object russh can keep driving across a
//! reconnect. `isekai-ssh` has no russh in the loop — it drives its own
//! stdin/stdout pump loops directly — so its reconnect orchestration
//! (replay buffer, backpressure, give-up-after-window) lives in
//! `isekai-ssh` itself and calls the functions here directly rather than
//! going through an `AsyncRead`/`AsyncWrite` facade
//! (`ISEKAI_SSH_DESIGN.md` "進め方": "過度に複雑にしないこと").
//!
//! One deliberate behavioral simplification versus `helper_quic_transport.rs`
//! (documented there as an Android-only latency optimization): this module
//! opens the control stream **sequentially**, after the data stream's
//! HELLO/ACK completes, rather than racing it in a background task. Android
//! does that to avoid delaying the SSH handshake hand-off by up to
//! `CONTROL_STREAM_TIMEOUT`; `isekai-ssh` has no such downstream consumer
//! waiting on the data stream alone, so the extra round trip is an
//! acceptable, much simpler trade.
//!
//! Also deliberately **not** ported: reopening a control stream after a
//! successful `RESUME`. `isekai_link_relay_transport.rs`'s `reattach_fn`
//! doesn't do this either (isekai-helper's `handle_resume_stream` merely
//! *offers* a `HELLO_TIMEOUT`-bounded window to reopen one, and silently
//! continues without resume-refresh support if the client doesn't take it) —
//! this module mirrors that reference behavior rather than adding new
//! untested surface. `APP_ACK`-based buffer trimming simply stops after a
//! resume; the C2H replay buffer's own bound still caps memory use (see
//! `isekai-ssh`'s `resume` module), matching the existing implementation's
//! trade-off exactly.

mod app_ack;

pub use app_ack::{AppAckCounters, AppAckTasks, spawn_app_ack_tasks};

use isekai_protocol::hello::Proof;
use isekai_protocol::offset::{C2hHelperCommittedOffset, C2hSentOffset, H2cClientDeliveredOffset, H2cSentOffset};
use isekai_protocol::resume::{
    decode_resume_ack, decode_resume_reject, encode_resume, ResumeFrame, ResumeProof, FRAME_RESUME_ACK,
    RESUME_ACK_FRAME_LEN,
};
use isekai_protocol::session_id::{decode_session_id, SessionId, SESSION_ID_LEN};
use log::info;

use crate::error::TransportError;
use crate::proof::compute_proof;
use crate::relay::{connect_and_handshake, read_exact, RelayTarget};
use crate::traits::{ByteStream, QuicConnection, QuicEndpointFactory};
use crate::types::{BindSpec, RemoteSpec};

/// `HELPER_PROTOCOL.md` §7.3 control-stream frame markers. `RESUME`/
/// `RESUME_ACK` already live in `isekai_protocol::resume` (Phase S-4a); these
/// three are only used on the control stream itself and never overlap with
/// the data stream's HELLO/ACK vocabulary, so — unlike `RESUME`/`RESUME_ACK`
/// — they didn't need a pure-crate home ahead of time and are defined here,
/// matching `rust-core/src/resume_client.rs`'s `pub(crate)` constants of the
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

/// A successfully-established control stream (`ISEKAI_SSH_DESIGN.md`
/// "接続確立順序" step 2), plus the `session_id` isekai-helper assigned it.
pub struct ControlStream {
    pub stream: Box<dyn ByteStream>,
    pub session_id: SessionId,
}

/// Opens a new bidirectional stream on `conn` and performs the
/// `CONTROL_HELLO`/`CONTROL_ACK` exchange (`HELPER_PROTOCOL.md` §7.3),
/// reusing the same `proof` the data stream's `HELLO` already sent — both are
/// computed from the same connection's exporter with an empty `extra`, so
/// they are always equal; recomputing would just waste an HMAC call
/// (`helper_quic_transport.rs::open_control_stream`'s same shortcut).
pub async fn open_control_stream(
    conn: &dyn QuicConnection,
    proof: &Proof,
) -> Result<ControlStream, TransportError> {
    let mut stream = conn.open_bi().await?;

    let mut hello = Vec::with_capacity(CONTROL_HELLO_FRAME_LEN);
    hello.push(CONTROL_HELLO);
    hello.extend_from_slice(proof.as_bytes());
    stream.write_all(&hello).await?;

    let mut ack = [0u8; CONTROL_ACK_FRAME_LEN];
    read_exact(stream.as_mut(), &mut ack).await?;
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
    pub connection: Box<dyn QuicConnection>,
    pub data_stream: Box<dyn ByteStream>,
    pub control_stream: Box<dyn ByteStream>,
    pub session_id: SessionId,
}

/// Like `relay::connect_via_relay`, but additionally opens the control stream
/// and returns the `session_id` needed to resume later
/// (`ISEKAI_SSH_DESIGN.md` Phase S-4c). Used for the *first* connection to a
/// given isekai-helper instance; `reconnect_and_resume` is used for every
/// subsequent reconnection after a disconnect.
pub async fn connect_via_relay_resumable(
    factory: &dyn QuicEndpointFactory,
    target: &RelayTarget,
) -> Result<ResumableRelaySession, TransportError> {
    let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await?;
    let (conn, data_stream, proof) = connect_and_handshake(
        endpoint.as_ref(),
        RemoteSpec {
            addr: target.helper_addr,
            server_name: target.server_name.clone(),
            cert_sha256_hex: target.cert_sha256_hex.clone(),
        },
        &target.session_secret,
    )
    .await?;

    let control = open_control_stream(conn.as_ref(), &proof).await?;
    info!("isekai-transport: control stream established, session_id={}", control.session_id);

    Ok(ResumableRelaySession {
        connection: conn,
        data_stream,
        control_stream: control.stream,
        session_id: control.session_id,
    })
}

/// The result of a successful `RESUME` (`HELPER_PROTOCOL.md` §7.3): a fresh
/// QUIC connection and its first (and, per this module's simplification, only
/// — see module docs) bidirectional stream, now a raw data-stream
/// pass-through exactly like a fresh `HELLO`/`ACK`'d connection, plus the
/// offsets isekai-helper reports so the caller knows what it may safely
/// discard from its own C2H replay buffer (`helper_committed_offset`) and,
/// for diagnostics/consistency checking, how much it already sent
/// (`helper_sent_offset`).
pub struct ResumeAckOutcome {
    pub connection: Box<dyn QuicConnection>,
    pub data_stream: Box<dyn ByteStream>,
    pub helper_committed_offset: C2hHelperCommittedOffset,
    pub helper_sent_offset: H2cSentOffset,
}

/// Dials a brand-new QUIC connection to `target.helper_addr` and sends
/// `RESUME` on its first bidirectional stream (`HELPER_PROTOCOL.md` §7.3:
/// "新しい QUIC connection の control stream 先頭" — despite the name, this is
/// the *first* stream opened on the new connection, not a stream opened
/// alongside/after a fresh HELLO; the real `isekai-helper` implementation
/// treats whichever frame type arrives first on the first stream as either a
/// new-session `HELLO` or a `RESUME`, see `isekai-helper/src/main.rs::handle_connection`).
///
/// `client_sent_offset`/`client_delivered_offset` must be the caller's
/// current C2H-sent / H2C-delivered offsets (`ISEKAI_SSH_DESIGN.md`'s
/// naming) — the caller (`isekai-ssh`) owns that bookkeeping; this function
/// only knows how to put them on the wire and parse the response.
pub async fn reconnect_and_resume(
    factory: &dyn QuicEndpointFactory,
    target: &RelayTarget,
    session_id: SessionId,
    client_sent_offset: C2hSentOffset,
    client_delivered_offset: H2cClientDeliveredOffset,
) -> Result<ResumeAckOutcome, TransportError> {
    let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await?;
    let conn = endpoint
        .connect(RemoteSpec {
            addr: target.helper_addr,
            server_name: target.server_name.clone(),
            cert_sha256_hex: target.cert_sha256_hex.clone(),
        })
        .await?;

    // `resume_proof = HMAC-SHA256(session_secret, exporter || session_id)`
    // (`HELPER_PROTOCOL.md` §7.3). `compute_proof`'s `extra` parameter is
    // exactly this: the real `isekai-helper` server computes its own expected
    // value the same way — same exporter label, `session_id` bytes appended
    // — for both the initial `HELLO` and `RESUME`/`CONTROL_HELLO`
    // (`isekai-helper/src/main.rs` uses one `EXPORTER_LABEL` throughout,
    // confirmed by reading that file directly rather than trusting
    // `HELPER_PROTOCOL.md`'s prose, which names a different, unused label).
    let resume_proof_bytes = compute_proof(conn.as_ref(), &target.session_secret, session_id.as_bytes()).await?;
    let resume_proof = ResumeProof::new(*resume_proof_bytes.as_bytes());

    let mut stream = conn.open_bi().await?;
    let frame =
        ResumeFrame { session_id, resume_proof, client_sent_offset, client_delivered_offset };
    stream.write_all(&encode_resume(&frame)).await?;

    let mut type_byte = [0u8; 1];
    read_exact(stream.as_mut(), &mut type_byte).await?;
    if type_byte[0] != FRAME_RESUME_ACK {
        let reason = decode_resume_reject(type_byte[0])?;
        return Err(TransportError::ResumeRejected(reason));
    }

    let mut rest = [0u8; RESUME_ACK_FRAME_LEN - 1];
    read_exact(stream.as_mut(), &mut rest).await?;
    let mut full = [0u8; RESUME_ACK_FRAME_LEN];
    full[0] = FRAME_RESUME_ACK;
    full[1..].copy_from_slice(&rest);
    let ack = decode_resume_ack(&full)?;

    info!(
        "isekai-transport: resume succeeded, session_id={session_id}, helper_committed_offset={}",
        ack.helper_committed_offset
    );
    Ok(ResumeAckOutcome {
        connection: conn,
        data_stream: stream,
        helper_committed_offset: ack.helper_committed_offset,
        helper_sent_offset: ack.helper_sent_offset,
    })
}
