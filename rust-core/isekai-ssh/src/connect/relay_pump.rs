//! `--mode relay`'s resumable connect+relay lifecycle (`ISEKAI_SSH_DESIGN.md`
//! Phase S-4c/S-4d): `run_relay_resumable` establishes a resumable session,
//! then drives `run_data_pump` in a loop, transparently reconnecting
//! (`isekai_transport::reconnect_and_resume`) whenever the QUIC connection is
//! lost. See `super::stun_pump` for `--mode stun`'s simpler, non-resumable
//! counterpart.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use isekai_transport::{
    connect_via_relay_resumable, reconnect_and_resume, spawn_app_ack_tasks, AppAckCounters, BackoffPolicy,
    ByteStreamReadHalf, ByteStreamWriteHalf, C2hSentOffset, H2cClientDeliveredOffset, RelayTarget,
    SystemQuicEndpointFactory,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{relay_hello_failure_message, TargetSource};
use crate::resume::C2hReplayBuffer;

/// Upper bound on unconfirmed C2H bytes kept in memory
/// (`ISEKAI_SSH_DESIGN.md`'s C2H replay buffer). Matches isekai-helper's own
/// `DEFAULT_RESUME_BUFFER_SIZE` (`isekai-helper/src/main.rs`) so neither side
/// is the tighter bottleneck.
const C2H_REPLAY_BUFFER_CAPACITY: usize = 4 * 1024 * 1024;

/// Reconnect backoff between resume attempts. Deliberately no jitter here
/// (`BackoffPolicy::base_delay`, not `delay_for_attempt`) — isekai-ssh is a
/// single CLI process reconnecting to one specific isekai-helper instance,
/// not a fleet of clients that could thunder against a shared server, so the
/// jitter's only purpose (avoiding a reconnect stampede) doesn't apply, and
/// skipping it avoids pulling in a `rand::Rng` just for this.
const RESUME_BACKOFF: BackoffPolicy =
    BackoffPolicy { initial: Duration::from_millis(500), max: Duration::from_secs(10), jitter: 0.0 };

/// How often `pump_c2h`'s backpressure wait re-checks whether the replay
/// buffer has room again, while stdin reads are paused
/// (`ISEKAI_SSH_DESIGN.md`: "読み取りを呼ばなければパイプが埋まって...という
/// 単純な仕組みで十分" — a plain poll loop is that "simple enough" mechanism).
const BACKPRESSURE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// `--mode relay`'s connect+relay lifecycle (`ISEKAI_SSH_DESIGN.md` Phase
/// S-4c/S-4d): establishes a resumable session, then drives `run_data_pump`
/// in a loop. As long as `run_data_pump` reports a real disconnect (not a
/// clean EOF), this function keeps trying `reconnect_and_resume` — bounded by
/// `resume_window` (`ConnectArgs::resume_window`, matching isekai-helper's
/// own `--resume-window`) — before finally giving up. Giving up
/// (`resume_window` exceeded) is deliberate, not incidental: it prints an
/// stderr message saying by how much the window was exceeded, explicitly
/// shuts down `stdout` and drops `stdin`, and only then returns `Ok(())` —
/// letting the process exit closes whatever the explicit shutdown/drop
/// couldn't (e.g. the underlying fds themselves, which `tokio::io::stdout()`'s
/// `shutdown()` does not close, only flushes/marks done).
pub async fn run_relay_resumable(
    target: RelayTarget,
    host: &str,
    source: &TargetSource,
    resume_window: Duration,
) -> Result<()> {
    log::info!("isekai-ssh: connecting to isekai-helper at {} (--mode relay)", target.helper_addr);
    let factory = SystemQuicEndpointFactory;
    let established = connect_via_relay_resumable(&factory, &target)
        .await
        .with_context(|| relay_hello_failure_message(host, source))?;
    log::info!(
        "isekai-ssh: HELLO/ACK + control stream established (session_id={}) — relaying stdin/stdout <-> QUIC",
        established.session_id
    );

    let session_id = established.session_id;
    // The `connection` handles returned alongside each data/control stream
    // are not needed past this point: every concrete `ByteStream` keeps its
    // own connection alive internally (proven by `isekai-transport`'s own
    // `relay_e2e.rs`, which drops its connection handle immediately and
    // still successfully uses the resulting stream) — isekai-ssh only ever
    // needs the streams themselves.
    drop(established.connection);

    let counters = Arc::new(AppAckCounters::new());
    let app_ack_tasks = spawn_app_ack_tasks(established.control_stream, counters.clone());
    let replay = Arc::new(Mutex::new(C2hReplayBuffer::new(C2H_REPLAY_BUFFER_CAPACITY)));

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut data_stream = established.data_stream;
    let mut disconnected_since: Option<Instant> = None;
    let mut attempt: u32 = 0;

    loop {
        let (quic_read, quic_write) = data_stream.split();
        let outcome =
            run_data_pump(&mut stdin, &mut stdout, quic_read, quic_write, &replay, &counters).await;
        app_ack_tasks.abort();

        match outcome {
            Ok(()) => return Ok(()),
            Err(e) => {
                log::warn!("isekai-ssh: data stream ended with an error, attempting to resume: {e:#}");
            }
        }

        let deadline = *disconnected_since.get_or_insert_with(Instant::now) + resume_window;
        let new_stream = loop {
            let now = Instant::now();
            if now >= deadline {
                let exceeded_by = now.saturating_duration_since(deadline);
                // Deliberately `eprintln!`, not `log::warn!` (module docs):
                // this must be visible regardless of `RUST_LOG` — it's the
                // one moment `ssh` is about to see its ProxyCommand die, and
                // the user deserves a clearer reason than whatever generic
                // message `ssh` itself prints for a closed pipe.
                eprintln!(
                    "isekai-ssh: giving up on session_id={session_id} for '{host}' — the resume window \
                     ({resume_window:?}) was exceeded by {exceeded_by:?} without reconnecting to \
                     isekai-helper. Closing stdin/stdout; ssh will treat this like any other lost \
                     connection. Run `ssh {host}` again once isekai-helper is reachable."
                );
                // Explicit close/shutdown before exiting (S-4d): don't just
                // rely on the process exit to close these as a side effect.
                // `tokio::io::Stdout::shutdown` flushes and marks the write
                // side done; `tokio::io::Stdin` has no analogous shutdown
                // primitive, so dropping our handle to it is the closest
                // equivalent available from here (the underlying fd itself
                // is still only actually closed when the process exits,
                // same as `stdout`'s fd).
                let _ = stdout.shutdown().await;
                drop(stdin);
                return Ok(());
            }
            let delay = RESUME_BACKOFF.base_delay(attempt).min(deadline - now);
            attempt = attempt.saturating_add(1);
            tokio::time::sleep(delay).await;

            let client_sent_offset = C2hSentOffset::new(replay.lock().unwrap().end_offset());
            let client_delivered_offset = H2cClientDeliveredOffset::new(counters.h2c_client_delivered_offset());
            match reconnect_and_resume(&factory, &target, session_id, client_sent_offset, client_delivered_offset)
                .await
            {
                Ok(mut resumed) => {
                    drop(resumed.connection);
                    let to_replay =
                        { replay.lock().unwrap().replay_from(resumed.helper_committed_offset.get()) };
                    if let Some(bytes) = to_replay {
                        if !bytes.is_empty() {
                            if let Err(e) = resumed.data_stream.write_all(&bytes).await {
                                log::warn!("isekai-ssh: failed to replay unconfirmed C2H bytes after resume: {e}");
                                continue;
                            }
                        }
                    }
                    replay.lock().unwrap().advance_start(resumed.helper_committed_offset.get());
                    log::info!(
                        "isekai-ssh: resume succeeded (session_id={session_id}, \
                         helper_committed_offset={})",
                        resumed.helper_committed_offset
                    );
                    break resumed.data_stream;
                }
                Err(e) => {
                    log::warn!("isekai-ssh: resume attempt {attempt} failed: {e:#}, retrying");
                }
            }
        };

        // A fresh data stream is resumed, but per `isekai_transport::resume`'s
        // module docs, the control stream is deliberately *not* reopened
        // after a resume (mirrors `isekai_link_relay_transport.rs::reattach_fn`'s
        // reference behavior) — `app_ack_tasks` above was already aborted;
        // there is nothing to restart it with. `counters.h2c_client_delivered_offset()`
        // still gets included directly in any subsequent `RESUME` frame, so
        // no progress-reporting information is lost, only the
        // opportunistic mid-connection buffer trimming via `APP_ACK`.
        data_stream = new_stream;
        disconnected_since = None;
        attempt = 0;
    }
}

/// Drives both pump directions concurrently in a single task (not two
/// separate `tokio::spawn`ed tasks, unlike `stun_pump::relay_stdio`) so
/// `stdin`/`stdout` can be borrowed across reconnects instead of needing to
/// be recreated (or made `'static`) every time `run_relay_resumable` loops.
///
/// Matches `stun_pump::relay_stdio`'s "clean EOF on one side does not abort
/// the other" rule for a clean `Ok(())` (both sides must finish before this
/// returns `Ok(())`), but diverges for errors: any error on *either* side
/// immediately ends the whole pump — returning that `Err` (the other side's
/// future is simply dropped, canceling it) — rather than waiting for the
/// survivor to also finish. Once the underlying QUIC connection is gone,
/// there is nothing for the survivor to usefully keep doing.
/// `run_relay_resumable` treats `Ok(())` as "clean shutdown, no resume" and
/// any `Err` as "disconnected, attempt to resume" — it deliberately does not
/// try to distinguish "the QUIC connection died" from "a local stdio error
/// occurred" (`ISEKAI_SSH_DESIGN.md`'s minimal S-4c scope): a local-only
/// error (e.g. `ssh` itself dying, closing our stdout) will simply keep
/// failing every resume attempt's own subsequent pump too, and eventually
/// hit `run_relay_resumable`'s give-up path regardless.
async fn run_data_pump(
    stdin: &mut (impl AsyncRead + Unpin),
    stdout: &mut (impl AsyncWrite + Unpin),
    quic_read: Box<dyn ByteStreamReadHalf>,
    quic_write: Box<dyn ByteStreamWriteHalf>,
    replay: &Arc<Mutex<C2hReplayBuffer>>,
    counters: &Arc<AppAckCounters>,
) -> Result<()> {
    let c2h_fut = pump_c2h(stdin, quic_write, replay.clone(), counters.clone());
    let h2c_fut = pump_h2c(quic_read, stdout, counters.clone());
    tokio::pin!(c2h_fut);
    tokio::pin!(h2c_fut);

    let mut c2h_done = false;
    let mut h2c_done = false;
    loop {
        tokio::select! {
            res = &mut c2h_fut, if !c2h_done => {
                res.context("isekai-ssh: C2H (stdin -> isekai-helper) pump failed")?;
                c2h_done = true;
            }
            res = &mut h2c_fut, if !h2c_done => {
                res.context("isekai-ssh: H2C (isekai-helper -> stdout) pump failed")?;
                h2c_done = true;
            }
        }
        if c2h_done && h2c_done {
            return Ok(());
        }
    }
}

/// C2H direction with backpressure and replay-buffer tee
/// (`ISEKAI_SSH_DESIGN.md` Phase S-4c task 2). Before every stdin read,
/// first syncs `replay`'s confirmed-prefix marker from
/// `counters.c2h_helper_committed_offset()` — the value
/// `isekai_transport::spawn_app_ack_tasks`'s receive loop keeps up to date
/// from isekai-helper's `APP_ACK`s — then waits for `replay` to have room
/// (`C2hReplayBuffer::is_full`/`remaining_capacity`), deliberately *not*
/// reading from stdin while the buffer is full, which is what actually
/// backpressures the parent `ssh` process (`ISEKAI_SSH_DESIGN.md`:
/// "読み取りを呼ばなければパイプが埋まってssh側の書き込みがブロックされる").
/// Without this sync step, `replay` would only ever get trimmed right after
/// an actual resume (`run_relay_resumable`'s `advance_start` call there) and
/// would hit its capacity — stalling stdin forever — on any sufficiently
/// long-lived, uninterrupted session; syncing continuously here is what
/// makes `APP_ACK`'s "trim opportunistically while still connected" purpose
/// actually take effect (`HELPER_PROTOCOL.md` §7.4).
///
/// Every byte written to the data stream is also appended to `replay` so a
/// future resume can replay it if isekai-helper didn't confirm committing it
/// before the disconnect.
async fn pump_c2h(
    stdin: &mut (impl AsyncRead + Unpin),
    mut quic_write: Box<dyn ByteStreamWriteHalf>,
    replay: Arc<Mutex<C2hReplayBuffer>>,
    counters: Arc<AppAckCounters>,
) -> Result<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        loop {
            let mut r = replay.lock().unwrap();
            r.advance_start(counters.c2h_helper_committed_offset());
            if !r.is_full() {
                break;
            }
            drop(r);
            tokio::time::sleep(BACKPRESSURE_POLL_INTERVAL).await;
        }
        let read_len = buf.len().min(replay.lock().unwrap().remaining_capacity());
        let n = stdin.read(&mut buf[..read_len]).await.context("isekai-ssh: reading from stdin failed")?;
        if n == 0 {
            let _ = quic_write.shutdown().await;
            return Ok(());
        }
        quic_write.write_all(&buf[..n]).await.context("isekai-ssh: writing to isekai-helper failed")?;
        replay.lock().unwrap().append(&buf[..n]);
    }
}

/// H2C direction. Every successful stdout write also advances
/// `counters`'s `h2c_client_delivered_offset` — the "pending ACK, held
/// locally while disconnected, included in the next `RESUME`" value
/// `ISEKAI_SSH_DESIGN.md`'s H2C-delivered-boundary note describes.
async fn pump_h2c(
    mut quic_read: Box<dyn ByteStreamReadHalf>,
    stdout: &mut (impl AsyncWrite + Unpin),
    counters: Arc<AppAckCounters>,
) -> Result<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = quic_read.read(&mut buf).await.context("isekai-ssh: reading from isekai-helper failed")?;
        if n == 0 {
            return Ok(());
        }
        stdout.write_all(&buf[..n]).await.context("isekai-ssh: writing to stdout failed")?;
        stdout.flush().await.context("isekai-ssh: flushing stdout failed")?;
        counters.advance_h2c_client_delivered_offset(n as u64);
    }
}
