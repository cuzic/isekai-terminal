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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
use crate::traits::{ByteStream, ByteStreamReadHalf, QuicConnection, QuicEndpointFactory};
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
pub const APP_ACK: u8 = 0x12;

const CONTROL_HELLO_FRAME_LEN: usize = 1 + isekai_protocol::hello::PROOF_LEN;
const CONTROL_ACK_FRAME_LEN: usize = 1 + SESSION_ID_LEN;
const APP_ACK_FRAME_LEN: usize = 1 + 8;

/// How often the `APP_ACK` sender loop wakes up to check whether its side's
/// offset has advanced since the last send (`HELPER_PROTOCOL.md` §7.4: "64KiB
/// 受信ごと、または200msごとのどちらか早い方" — this module only implements the
/// time-based half, matching `resume_client.rs`/`isekai-helper/src/main.rs`'s
/// own `spawn_app_ack_tasks`, which also only implements the 200ms timer).
const APP_ACK_INTERVAL: Duration = Duration::from_millis(200);

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

/// Shared, thread-safe bridge between `isekai-ssh`'s C2H/H2C offset
/// bookkeeping and this module's `APP_ACK` send/receive loops
/// (`spawn_app_ack_tasks`). `isekai-ssh` owns the actual replay buffer and
/// stdout-delivery bookkeeping (`ISEKAI_SSH_DESIGN.md`'s task split: replay
/// buffer/backpressure are `isekai-ssh`'s job, control-stream/`APP_ACK`
/// wire-level exchange is `isekai-transport`'s) — this type is the seam
/// between them, plain atomics rather than a callback/closure so both sides
/// stay simple `Send + Sync` values with no lifetime entanglement.
///
/// - `h2c_client_delivered_offset`: written by `isekai-ssh`'s H2C pump loop
///   every time it successfully `write_all`s to its own stdout (the H2C
///   "delivered" source of truth, `ISEKAI_SSH_DESIGN.md`); read by this
///   module's `APP_ACK` sender loop, and also by `isekai-ssh` itself when
///   building a `RESUME` frame's `client_delivered_offset` after a
///   disconnect (this is exactly the "pending ACK, held locally while
///   disconnected" value `ISEKAI_SSH_DESIGN.md`'s H2C-delivered-boundary
///   note describes).
/// - `c2h_helper_committed_offset`: written by this module's `APP_ACK`
///   receiver loop whenever isekai-helper reports progress; read by
///   `isekai-ssh` to know how much of its C2H replay buffer it may discard
///   (backpressure relief).
#[derive(Default)]
pub struct AppAckCounters {
    h2c_client_delivered_offset: AtomicU64,
    c2h_helper_committed_offset: AtomicU64,
}

impl AppAckCounters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `delta` bytes to the H2C-delivered offset. Called by
    /// `isekai-ssh`'s H2C pump loop after each successful stdout write.
    pub fn advance_h2c_client_delivered_offset(&self, delta: u64) {
        self.h2c_client_delivered_offset.fetch_add(delta, Ordering::SeqCst);
    }

    pub fn h2c_client_delivered_offset(&self) -> u64 {
        self.h2c_client_delivered_offset.load(Ordering::SeqCst)
    }

    /// Called by this module's `APP_ACK` receiver loop only; `pub` so a
    /// caller wiring up its own transport for tests can also drive it
    /// directly without going through a real control stream.
    pub fn set_c2h_helper_committed_offset(&self, value: u64) {
        self.c2h_helper_committed_offset.store(value, Ordering::SeqCst);
    }

    pub fn c2h_helper_committed_offset(&self) -> u64 {
        self.c2h_helper_committed_offset.load(Ordering::SeqCst)
    }
}

/// Handles for the two background tasks `spawn_app_ack_tasks` starts.
/// Callers should `abort()` these before/while tearing down a connection —
/// e.g. right before attempting `reconnect_and_resume` — since the control
/// stream they were reading/writing from is about to become invalid anyway
/// (mirrors `resume_client.rs`'s reattach path implicitly doing the same by
/// simply not reusing the old control stream's tasks).
pub struct AppAckTasks {
    send: tokio::task::JoinHandle<()>,
    recv: tokio::task::JoinHandle<()>,
}

impl AppAckTasks {
    pub fn abort(&self) {
        self.send.abort();
        self.recv.abort();
    }
}

/// Spawns the two `APP_ACK` background tasks (`HELPER_PROTOCOL.md` §7.4) on
/// `control_stream`, matching `helper_quic_transport.rs::spawn_app_ack_tasks`
/// byte-for-byte on the wire:
///
/// - send loop: every `APP_ACK_INTERVAL`, if `counters`'s
///   `h2c_client_delivered_offset` advanced since the last send, sends
///   `[APP_ACK] || offset` (client → helper direction, per §7.3's table:
///   "client → helper の場合: client_delivered_offset").
/// - receive loop: reads `[APP_ACK] || offset` frames from isekai-helper and
///   stores the offset into `counters.c2h_helper_committed_offset` (helper →
///   client direction: "helper_committed_offset").
///
/// Both loops are best-effort and simply exit on the first I/O error —
/// `isekai-ssh` doesn't need to react to that directly; it will independently
/// notice the *data* stream has died and drive a reconnect, at which point it
/// should `AppAckTasks::abort()` these (they'd otherwise keep spinning on a
/// now-dead control stream).
pub fn spawn_app_ack_tasks(control_stream: Box<dyn ByteStream>, counters: Arc<AppAckCounters>) -> AppAckTasks {
    let (mut read_half, mut write_half) = control_stream.split();

    let recv_counters = counters.clone();
    let recv = tokio::spawn(async move {
        loop {
            let mut frame = [0u8; APP_ACK_FRAME_LEN];
            if read_exact_half(read_half.as_mut(), &mut frame).await.is_err() {
                break;
            }
            if frame[0] != APP_ACK {
                break;
            }
            let offset = u64::from_be_bytes(frame[1..APP_ACK_FRAME_LEN].try_into().unwrap());
            recv_counters.set_c2h_helper_committed_offset(offset);
        }
    });

    let send_counters = counters;
    let send = tokio::spawn(async move {
        let mut last_sent = 0u64;
        loop {
            tokio::time::sleep(APP_ACK_INTERVAL).await;
            let current = send_counters.h2c_client_delivered_offset();
            if current == last_sent {
                continue;
            }
            let mut frame = Vec::with_capacity(APP_ACK_FRAME_LEN);
            frame.push(APP_ACK);
            frame.extend_from_slice(&current.to_be_bytes());
            if write_half.write_all(&frame).await.is_err() {
                break;
            }
            last_sent = current;
        }
    });

    AppAckTasks { send, recv }
}

/// `ByteStreamReadHalf::read` only guarantees "at most `buf.len()` bytes,
/// possibly fewer" — same `read_exact` loop as `relay::read_exact`, just over
/// a split read half instead of a whole `ByteStream`.
async fn read_exact_half(half: &mut dyn ByteStreamReadHalf, buf: &mut [u8]) -> Result<(), TransportError> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = half.read(&mut buf[filled..]).await?;
        if n == 0 {
            return Err(TransportError::UnexpectedEof);
        }
        filled += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    /// A `ByteStream` backed by an in-memory `tokio::io::duplex` pair —
    /// exercises `spawn_app_ack_tasks`'s wire framing/offset logic
    /// deterministically and quickly, without needing a real QUIC connection
    /// (that end-to-end path is already covered by `tests/resume_e2e.rs`).
    struct DuplexByteStream(DuplexStream);

    #[async_trait]
    impl ByteStream for DuplexByteStream {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            self.0.read(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))
        }
        async fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError> {
            self.0.write_all(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))
        }
        async fn shutdown(&mut self) -> Result<(), TransportError> {
            self.0.shutdown().await.map_err(|e| TransportError::StreamIo(e.to_string()))
        }
        fn split(self: Box<Self>) -> (Box<dyn ByteStreamReadHalf>, Box<dyn crate::traits::ByteStreamWriteHalf>) {
            let (read_half, write_half) = tokio::io::split(self.0);
            (Box::new(DuplexReadHalf(read_half)), Box::new(DuplexWriteHalf(write_half)))
        }
    }

    struct DuplexReadHalf(tokio::io::ReadHalf<DuplexStream>);
    #[async_trait]
    impl ByteStreamReadHalf for DuplexReadHalf {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            self.0.read(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))
        }
    }

    struct DuplexWriteHalf(tokio::io::WriteHalf<DuplexStream>);
    #[async_trait]
    impl crate::traits::ByteStreamWriteHalf for DuplexWriteHalf {
        async fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError> {
            self.0.write_all(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))
        }
        async fn shutdown(&mut self) -> Result<(), TransportError> {
            self.0.shutdown().await.map_err(|e| TransportError::StreamIo(e.to_string()))
        }
    }

    /// Wires up two `spawn_app_ack_tasks` instances against each other's ends
    /// of an in-memory duplex pipe (standing in for one control stream seen
    /// from both the client's and isekai-helper's side) and confirms each
    /// side's `APP_ACK` sends land in the other side's counters within a few
    /// send intervals.
    #[tokio::test(start_paused = true)]
    async fn app_ack_tasks_propagate_offsets_in_both_directions() {
        let (client_half, helper_half) = tokio::io::duplex(4096);

        let client_counters = Arc::new(AppAckCounters::new());
        let client_tasks =
            spawn_app_ack_tasks(Box::new(DuplexByteStream(client_half)), client_counters.clone());

        let helper_counters = Arc::new(AppAckCounters::new());
        let helper_tasks =
            spawn_app_ack_tasks(Box::new(DuplexByteStream(helper_half)), helper_counters.clone());

        // Each side's send loop always advertises its own
        // `h2c_client_delivered_offset` field (`AppAckCounters` doesn't know
        // or care whether it's playing the client or helper role — see the
        // big comment below) — so both sides set that same field here to
        // simulate "I have new progress to report", with different values so
        // the assertions below can't pass by coincidence.
        client_counters.advance_h2c_client_delivered_offset(42);
        helper_counters.advance_h2c_client_delivered_offset(99);

        // Advance past a couple of APP_ACK_INTERVAL ticks so both send loops
        // have had a chance to fire (virtual time via `start_paused = true`,
        // matching `resume_client.rs`'s own reattach-retry test convention).
        for _ in 0..5 {
            tokio::time::advance(APP_ACK_INTERVAL).await;
            tokio::task::yield_now().await;
        }

        // The client's send loop should have told the helper "I've delivered
        // 42 bytes of H2C", landing in the helper's own
        // `c2h_helper_committed_offset` field... wait, no: APP_ACK's meaning
        // is direction-dependent (`HELPER_PROTOCOL.md` §7.3) — from the
        // client, the payload is `client_delivered_offset` (H2C); from the
        // helper, it's `helper_committed_offset` (C2H). Both sides of this
        // test use the *same* `AppAckCounters` shape/receive loop, so what
        // the client *sent* (`h2c_client_delivered_offset`) ends up in the
        // peer's `c2h_helper_committed_offset` field purely because that's
        // the only field the shared receive loop knows how to write to —
        // this test is about proving the frames cross the wire and get
        // stored somewhere durable, not about assigning real
        // client/helper roles to each `AppAckCounters` (real client/helper
        // roles are exercised by `tests/resume_e2e.rs`).
        assert_eq!(
            helper_counters.c2h_helper_committed_offset(),
            42,
            "the client's send loop should have delivered its h2c_client_delivered_offset (42) \
             to the helper side's receive loop"
        );
        assert_eq!(
            client_counters.c2h_helper_committed_offset(),
            99,
            "the helper's send loop should have delivered its h2c_client_delivered_offset field \
             (here holding the stand-in value 99) to the client side's receive loop"
        );

        client_tasks.abort();
        helper_tasks.abort();
    }
}
