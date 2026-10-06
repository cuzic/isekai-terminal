//! L0 characterization tests for the sweep × RESUME race
//! (`ADR_FUNCTIONAL_CORE_EFFECTS.md` §4.1 / §6 Step 1.5, round 1 B-2,
//! round 2 m-R2-9).
//!
//! **These tests assert today's (buggy) behavior on purpose.** Step 2a
//! (single aggregate under a single lock) is expected to flip every
//! `CURRENT BEHAVIOR` assertion below; that flip is the evidence that the
//! refactor *closed* the window rather than moving it. Do not "fix" these
//! assertions without also landing that change.
//!
//! # The race
//!
//! `SessionTable::sweep_expired_parked` is two-phase: phase 1 collects
//! expired ids while holding the table lock (and each session's lock in
//! turn), then drops both; phase 2 calls `remove(&id)` for each collected id
//! **without re-checking** that the session is still parked. The RESUME path
//! (`engine/mod.rs::handle_resume_stream`: `sessions.get` → `handle.lock()` →
//! `parked_since = None; parked_tcp.take()` → `established_lease_for`) can
//! run entirely inside that gap. Phase 2 then discards a session that has
//! just been resumed, and the sweep backstop's caller
//! (`release_slot_for`) releases the fencing slot of a lease that is
//! actively relaying again.
//!
//! # How the window is hit deterministically
//!
//! No production hook is needed. `tokio::sync::Mutex` is documented as fair
//! (FIFO): a released permit is handed directly to the oldest waiter, so a
//! later `lock()` cannot barge ahead of it. With the default
//! `current_thread` test runtime:
//!
//! 1. The test holds the session's own lock.
//! 2. The sweep task starts phase 1: takes the table lock, then blocks on
//!    the session lock (still holding the table lock).
//! 3. The "resumer" task (modelling RESUME) blocks on the table lock behind
//!    the sweep.
//! 4. The test releases the session lock. Phase 1 sees the session expired,
//!    then releases the table lock — which is handed to the resumer. Phase 2's
//!    `remove` therefore queues *behind* the resumer.
//! 5. The resumer's `get` releases the table lock (handed to phase 2's
//!    `remove`) and, in the same poll, unparks the session uncontended.
//! 6. Phase 2 removes the now-live session.
//!
//! The resumer asserts that it really did unpark a TCP connection, so if the
//! interleaving ever stopped being the one described above the test would
//! fail loudly instead of passing vacuously.
//!
//! # Why not `start_paused`
//!
//! The ADR suggests `#[tokio::test(start_paused = true)]`. The sweep's
//! deadline is computed from `Session::parked_since: std::time::Instant`
//! (`since.elapsed()`), which tokio's paused clock does not control, so a
//! paused clock would add nothing here — expiry is set up by back-dating
//! `parked_since`, exactly as the existing `resume.rs` sweep tests do. A
//! paused clock would also be actively harmful for the `AttachRuntime` half:
//! auto-advance can fire `TARGET_CONNECT_TIMEOUT`/`PENDING_ACTIVATION_TIMEOUT`
//! while the real loopback `TcpStream::connect` is still in flight.
//! Determinism comes from mutex fairness instead (see above).
//!
//! # Not covered here
//!
//! - Concurrent `admit_new_session` (max+1 check-then-act, Step 2b) and the
//!   `InsertOutcome::Rejected` → `DataStreamDied` orphan park (N-3, Step 2a/2c)
//!   are not characterized in this file; they need the full QUIC
//!   `handle_attach_stream` path. Step 2a's proptest (I-g) covers the latter.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use isekai_protocol::attach::{AttachKey, AttemptId, ConnectionGeneration, ATTEMPT_ID_LEN};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use super::attach_runtime::{AttachRuntime, HelloOutcome};
use super::release_slot_for;
use super::resume::{Session, SessionId, SessionTable};

const MAX_PARKED: StdDuration = StdDuration::from_secs(30);

/// Lets every other ready task on the current-thread runtime run until it
/// next blocks. A handful of yields is far more than needed for the short,
/// I/O-free lock sequences used below.
async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

async fn loopback_tcp_pair() -> (OwnedReadHalf, OwnedWriteHalf, tokio::net::TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (server, _) = listener.accept().await.unwrap();
    let (r, w) = server.into_split();
    (r, w, client)
}

/// Spawns the sweep task (phase 1 + phase 2) and, if given an
/// `AttachRuntime`, the same per-id `release_slot_for` loop as the
/// production backstop in `engine/mod.rs::run_serve`.
fn spawn_sweep(
    table: SessionTable,
    attach_runtime: Option<Arc<AttachRuntime>>,
) -> tokio::task::JoinHandle<Vec<SessionId>> {
    tokio::spawn(async move {
        let expired = table.sweep_expired_parked(MAX_PARKED).await;
        if let Some(rt) = attach_runtime {
            for id in &expired {
                release_slot_for(&rt, isekai_protocol::SessionId::from_bytes(*id)).await;
            }
        }
        expired
    })
}

/// What the modelled RESUME observed.
struct Resumed {
    tcp: Option<(OwnedReadHalf, OwnedWriteHalf)>,
    lease_seen: bool,
}

/// Models the first half of `handle_resume_stream`: look the session up,
/// unpark it, then confirm its `Established` slot.
fn spawn_resumer(
    table: SessionTable,
    id: SessionId,
    attach_runtime: Option<Arc<AttachRuntime>>,
) -> tokio::task::JoinHandle<Resumed> {
    tokio::spawn(async move {
        let handle = table.get(&id).await.expect("RESUME must find the session (it is still in the table)");
        let tcp = {
            let mut session = handle.lock().await;
            session.parked_since = None;
            session.parked_tcp.take()
        };
        let lease_seen = match attach_runtime {
            Some(rt) => rt.established_lease_for(isekai_protocol::SessionId::from_bytes(id)).await.is_some(),
            None => false,
        };
        Resumed { tcp, lease_seen }
    })
}

/// Drives the interleaving described in the module docs and returns
/// `(sweep result, resumer result)`.
async fn race_sweep_against_resume(
    table: &SessionTable,
    id: SessionId,
    handle: &Arc<Mutex<Session>>,
    attach_runtime: Option<Arc<AttachRuntime>>,
) -> (Vec<SessionId>, Resumed) {
    // 1. Hold the session lock so phase 1 parks inside the table lock.
    let held = handle.lock().await;
    let sweep = spawn_sweep(table.clone(), attach_runtime.clone());
    settle().await;
    assert!(!sweep.is_finished(), "sweep phase 1 must be blocked on the session lock");

    // 2. Queue the resumer on the table lock, behind the sweep.
    let resumer = spawn_resumer(table.clone(), id, attach_runtime);
    settle().await;
    assert!(!resumer.is_finished(), "RESUME must be queued on the table lock held by sweep phase 1");

    // 3. Let phase 1 finish; FIFO hand-off puts the resumer ahead of phase 2.
    drop(held);
    let resumed = resumer.await.unwrap();
    let discarded = sweep.await.unwrap();
    (discarded, resumed)
}

/// `SessionTable`-only half: a session that RESUME has just unparked (i.e.
/// is live again) is still discarded by the in-flight sweep.
#[tokio::test]
async fn sweep_after_concurrent_unpark_currently_discards_live_session() {
    let table = SessionTable::new();
    let id = SessionTable::generate_session_id();
    let (r, w, _peer) = loopback_tcp_pair().await;
    let mut session = Session::new(1024);
    session.parked_tcp = Some((r, w));
    session.parked_since = Some(std::time::Instant::now() - StdDuration::from_secs(60));
    let handle = table.insert(id, session).await;

    let (discarded, resumed) = race_sweep_against_resume(&table, id, &handle, None).await;

    // Precondition of the race (not expected to change in Step 2a's *model*,
    // but Step 2a may reject the RESUME instead — see below): RESUME won the
    // unpark, so from its point of view the session is live and relaying.
    assert!(resumed.tcp.is_some(), "RESUME must have unparked the target TCP inside the sweep's window");
    assert_eq!(handle.lock().await.parked_since, None, "session is live (unparked) after RESUME");

    // CURRENT BEHAVIOR (Step 2a flips these): phase 2 removes the id without
    // re-checking, so a live session disappears from the table — every later
    // RESUME for it gets UnknownToken, and the same session_id becomes
    // re-admittable while it is still being relayed.
    assert_eq!(discarded, vec![id], "CURRENT BEHAVIOR: sweep reports the live session as discarded");
    assert!(!table.contains(&id).await, "CURRENT BEHAVIOR: the live session is gone from the table");
}

/// `AttachRuntime` half (round 2 m-R2-9): the same race, plus the
/// production backstop's `release_slot_for`, frees the `Established` fencing
/// slot of a lease that RESUME has just confirmed and is relaying through.
#[tokio::test]
async fn sweep_after_concurrent_unpark_currently_releases_slot_of_relaying_lease() {
    // Local target the AttachRuntime connects to; keep accepted streams alive.
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr: SocketAddr = target.local_addr().unwrap();
    let accepted = Arc::new(Mutex::new(Vec::new()));
    {
        let accepted = accepted.clone();
        tokio::spawn(async move {
            while let Ok((s, _)) = target.accept().await {
                accepted.lock().await.push(s);
            }
        });
    }
    let attach_runtime = AttachRuntime::new(target_addr);

    let table = SessionTable::new();
    let id = SessionTable::generate_session_id();
    let session_id = isekai_protocol::SessionId::from_bytes(id);
    let key = AttachKey {
        session_id,
        generation: ConnectionGeneration::new(1),
        attempt_id: AttemptId::from_bytes([7u8; ATTEMPT_ID_LEN]),
    };

    // ATTACH_HELLO → target connect → AttachActivate: slot is Established.
    let attach_token = match attach_runtime.hello(key).await {
        HelloOutcome::Ready { attach_token } => attach_token,
        HelloOutcome::Reject(reason) => panic!("hello rejected: {reason:?}"),
    };
    let (tcp, lease) = attach_runtime.activate(key, attach_token).await.expect("activate must establish the slot");
    let established = attach_runtime.established_lease_for(session_id).await.expect("slot is Established");

    // Data stream died, target TCP still alive: park it and keep the slot
    // (what `finish_or_park_session`'s `DataStreamDied` arm does).
    let (r, w) = tcp.into_split();
    let mut session = Session::new(1024);
    session.parked_tcp = Some((r, w));
    session.parked_since = Some(std::time::Instant::now() - StdDuration::from_secs(60));
    let handle = table.insert(id, session).await;
    lease.keep();
    assert_eq!(attach_runtime.established_lease_for(session_id).await, Some(established));

    let (discarded, resumed) =
        race_sweep_against_resume(&table, id, &handle, Some(attach_runtime.clone())).await;

    // RESUME unparked the TCP *and* saw the Established slot, so in
    // production it proceeds past `established_lease_for` into
    // `resumed_lease` + `relay_buffered`: the session is actively relaying.
    assert!(resumed.tcp.is_some(), "RESUME must have unparked the target TCP inside the sweep's window");
    assert!(resumed.lease_seen, "RESUME must have found the Established slot and be relaying through it");

    // CURRENT BEHAVIOR (Step 2a flips these): the sweep discarded the live
    // session and `release_slot_for` → `relay_ended` freed its fencing slot
    // while it is still relaying.
    assert_eq!(discarded, vec![id], "CURRENT BEHAVIOR: sweep reports the live session as discarded");
    assert!(!table.contains(&id).await, "CURRENT BEHAVIOR: the live session is gone from the table");
    assert_eq!(
        attach_runtime.established_lease_for(session_id).await,
        None,
        "CURRENT BEHAVIOR: the relaying lease's Established slot was released by the sweep backstop"
    );
    assert!(
        !attach_runtime.has_session(session_id).await,
        "CURRENT BEHAVIOR: the arbiter forgot the session, so the same session_id is re-admittable mid-relay"
    );

    drop(resumed);
}
