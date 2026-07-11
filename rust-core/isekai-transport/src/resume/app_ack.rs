//! `APP_ACK` background exchange (`archive/HELPER_PROTOCOL.md` §7.4): the
//! thread-safe counters bridge (`AppAckCounters`) between `isekai-ssh`'s C2H/
//! H2C offset bookkeeping and the two background tasks
//! (`spawn_app_ack_tasks`) that actually put `APP_ACK` frames on the wire.
//! Split out of `super` (the control-stream/`RESUME` establishment logic)
//! since it is a self-contained unit on its own: it only touches the control
//! stream after `super::open_control_stream` has already handed it over.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::error::TransportError;

use super::APP_ACK;

/// Minimal async byte-stream-half interfaces this module's send/receive
/// loops need — generic (implemented for a concrete type, not boxed as
/// `dyn`) so both `quicmux::AnyByteStreamReadHalf`/`AnyByteStreamWriteHalf`
/// and this module's own test-only in-memory duplex-pipe stand-in can
/// satisfy them. `quicmux::AnyByteStream`'s enum design deliberately has no
/// room for a third "test mock" variant (see that type's own docs on why:
/// it enumerates real backends, not test doubles), so exercising this
/// module's wire-framing/offset logic deterministically without a real
/// QUIC/QMux round trip needs its own minimal seam instead of going through
/// `quicmux` directly.
trait HalfRead: Send {
    fn read(&mut self, buf: &mut [u8]) -> impl std::future::Future<Output = Result<usize, TransportError>> + Send;
}

trait HalfWrite: Send {
    fn write_all(&mut self, buf: &[u8]) -> impl std::future::Future<Output = Result<(), TransportError>> + Send;
}

impl HalfRead for quicmux::AnyByteStreamReadHalf {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        quicmux::AnyByteStreamReadHalf::read(self, buf).await.map_err(TransportError::Mux)
    }
}

impl HalfWrite for quicmux::AnyByteStreamWriteHalf {
    async fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError> {
        quicmux::AnyByteStreamWriteHalf::write_all(self, buf).await.map_err(TransportError::Mux)
    }
}

const APP_ACK_FRAME_LEN: usize = 1 + 8;

/// How often the `APP_ACK` sender loop wakes up to check whether its side's
/// offset has advanced since the last send (`archive/HELPER_PROTOCOL.md` §7.4: "64KiB
/// 受信ごと、または200msごとのどちらか早い方" — this module only implements the
/// time-based half, matching `resume_client.rs`/`isekai-helper/src/main.rs`'s
/// own `spawn_app_ack_tasks`, which also only implements the 200ms timer).
const APP_ACK_INTERVAL: Duration = Duration::from_millis(200);

/// Shared, thread-safe bridge between `isekai-ssh`'s C2H/H2C offset
/// bookkeeping and this module's `APP_ACK` send/receive loops
/// (`spawn_app_ack_tasks`). `isekai-ssh` owns the actual replay buffer and
/// stdout-delivery bookkeeping (`archive/ISEKAI_SSH_DESIGN.md`'s task split: replay
/// buffer/backpressure are `isekai-ssh`'s job, control-stream/`APP_ACK`
/// wire-level exchange is `isekai-transport`'s) — this type is the seam
/// between them, plain atomics rather than a callback/closure so both sides
/// stay simple `Send + Sync` values with no lifetime entanglement.
///
/// - `h2c_client_delivered_offset`: written by `isekai-ssh`'s H2C pump loop
///   every time it successfully `write_all`s to its own stdout (the H2C
///   "delivered" source of truth, `archive/ISEKAI_SSH_DESIGN.md`); read by this
///   module's `APP_ACK` sender loop, and also by `isekai-ssh` itself when
///   building a `RESUME` frame's `client_delivered_offset` after a
///   disconnect (this is exactly the "pending ACK, held locally while
///   disconnected" value `archive/ISEKAI_SSH_DESIGN.md`'s H2C-delivered-boundary
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

/// Spawns the two `APP_ACK` background tasks (`archive/HELPER_PROTOCOL.md` §7.4) on
/// `control_stream`, matching `isekai_pipe_quic_transport.rs::spawn_app_ack_tasks`
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
pub fn spawn_app_ack_tasks(control_stream: quicmux::AnyByteStream, counters: Arc<AppAckCounters>) -> AppAckTasks {
    let (read_half, write_half) = control_stream.split();
    spawn_app_ack_tasks_over(read_half, write_half, counters)
}

/// The actual send/receive loop logic, generic over [`HalfRead`]/
/// [`HalfWrite`] so it can run against either a real
/// `quicmux::AnyByteStream` half (via [`spawn_app_ack_tasks`]) or this
/// module's test-only duplex-pipe stand-in (see this module's tests).
fn spawn_app_ack_tasks_over(
    mut read_half: impl HalfRead + 'static,
    mut write_half: impl HalfWrite + 'static,
    counters: Arc<AppAckCounters>,
) -> AppAckTasks {
    let recv_counters = counters.clone();
    let recv = tokio::spawn(async move {
        loop {
            let mut frame = [0u8; APP_ACK_FRAME_LEN];
            if read_exact_half(&mut read_half, &mut frame).await.is_err() {
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

/// [`HalfRead::read`] only guarantees "at most `buf.len()` bytes, possibly
/// fewer" — same `read_exact` loop as `relay::read_exact`, just over a split
/// read half instead of a whole `quicmux::AnyByteStream`.
async fn read_exact_half(half: &mut impl HalfRead, buf: &mut [u8]) -> Result<(), TransportError> {
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    /// [`HalfRead`]/[`HalfWrite`] backed by an in-memory `tokio::io::duplex`
    /// pair's halves — exercises [`spawn_app_ack_tasks_over`]'s wire
    /// framing/offset logic deterministically and quickly, without needing a
    /// real QUIC connection (that end-to-end path is already covered by
    /// `tests/resume_e2e.rs`).
    struct DuplexReadHalf(tokio::io::ReadHalf<DuplexStream>);
    impl HalfRead for DuplexReadHalf {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            self.0.read(buf).await.map_err(|e| TransportError::Mux(quicmux::MuxError::StreamIo(e.to_string())))
        }
    }

    struct DuplexWriteHalf(tokio::io::WriteHalf<DuplexStream>);
    impl HalfWrite for DuplexWriteHalf {
        async fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError> {
            self.0.write_all(buf).await.map_err(|e| TransportError::Mux(quicmux::MuxError::StreamIo(e.to_string())))
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
        let (client_read, client_write) = tokio::io::split(client_half);
        let (helper_read, helper_write) = tokio::io::split(helper_half);

        let client_counters = Arc::new(AppAckCounters::new());
        let client_tasks = spawn_app_ack_tasks_over(DuplexReadHalf(client_read), DuplexWriteHalf(client_write), client_counters.clone());

        let helper_counters = Arc::new(AppAckCounters::new());
        let helper_tasks = spawn_app_ack_tasks_over(DuplexReadHalf(helper_read), DuplexWriteHalf(helper_write), helper_counters.clone());

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
        // is direction-dependent (`archive/HELPER_PROTOCOL.md` §7.3) — from the
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
