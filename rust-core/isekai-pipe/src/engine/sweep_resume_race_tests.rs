//! L0 tests for the sweep × RESUME race
//! (`ADR_FUNCTIONAL_CORE_EFFECTS.md` §4.1 / §6 Step 1.5 → Step 2a, round 1
//! B-2, round 2 m-R2-9).
//!
//! **History**: Step 1.5 landed these as *characterization* tests asserting
//! the then-current (buggy) behavior, each marked `CURRENT BEHAVIOR`. Step 2a
//! (single aggregate under a single lock) flipped every one of those
//! assertions — that flip is the evidence that the refactor *closed* the
//! window rather than moving it. The old interleaving and what each
//! assertion used to say are kept in the comments below.
//!
//! # The race (before Step 2a)
//!
//! `SessionTable::sweep_expired_parked` was two-phase: phase 1 collected
//! expired ids under the table lock, then dropped it; phase 2 called
//! `remove(&id)` for each collected id **without re-checking** that the
//! session was still parked. RESUME (`sessions.get` → `handle.lock()` →
//! `parked_since = None; parked_tcp.take()` → `established_lease_for`) could
//! run entirely inside that gap, after which the sweep discarded a session
//! that had just been resumed and `release_slot_for` freed the fencing slot
//! of a lease that was actively relaying again (and the session_id became
//! re-admittable mid-relay).
//!
//! # After Step 2a
//!
//! Sweep (judge expiry + remove + release slot) and RESUME (check parked +
//! unpark + hand over socket and output buffer) are each **one** apply on
//! `ServeAggregate` under `AttachRuntime`'s single lock. Whichever is applied
//! first wins *entirely*; the other observes the result:
//!
//! - RESUME first → granted; the sweep sees an active session and discards
//!   nothing; the slot stays `Established`.
//! - sweep first → discarded and slot released; the RESUME is rejected
//!   (`UnknownToken`) and nobody is relaying.
//!
//! "Both" (RESUME got the socket *and* the sweep discarded the session) is
//! no longer reachable.
//!
//! # How the order is forced deterministically
//!
//! `tokio::sync::Mutex` is documented as fair (FIFO). The test holds the
//! aggregate lock (`lock_core_for_test`), queues the two tasks behind it in
//! the desired order (with the default `current_thread` runtime, a spawned
//! task enqueues on the lock the first time it is polled), then releases it.
//!
//! The sweep uses `max_parked = 0`, so any parked session counts as expired
//! regardless of the clock (the old tests back-dated `parked_since` instead;
//! `parked_since` is now a `Millis` stamped by the shell, ADR §2.2).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use isekai_protocol::attach::{AttachKey, AttemptId, ConnectionGeneration, ATTEMPT_ID_LEN};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use super::attach_runtime::{AttachRuntime, HelloOutcome, ResumeDecision};
use super::resume::{Session, SessionId};

/// Lets every other ready task on the current-thread runtime run until it
/// next blocks.
async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

/// A local target the `AttachRuntime` connects to; accepted streams are kept
/// alive so the relayed "target TCP" stays open.
async fn spawn_target() -> SocketAddr {
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap();
    let accepted = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(async move {
        while let Ok((s, _)) = target.accept().await {
            accepted.lock().await.push(s);
        }
    });
    target_addr
}

fn key_for(id: SessionId) -> AttachKey {
    AttachKey {
        session_id: isekai_protocol::SessionId::from_bytes(id),
        generation: ConnectionGeneration::new(1),
        attempt_id: AttemptId::from_bytes([7u8; ATTEMPT_ID_LEN]),
    }
}

/// ATTACH_HELLO → target connect → AttachActivate (slot `Established`,
/// session registered in the index), then the data stream "dies" and the
/// target TCP is parked — exactly what `finish_or_park_session`'s
/// `DataStreamDied` arm does. Returns the established lease.
async fn establish_and_park(
    rt: &Arc<AttachRuntime>,
    id: SessionId,
) -> super::attach_arbiter::LeaseId {
    let key = key_for(id);
    let attach_token = match rt.hello(key).await {
        HelloOutcome::Ready { attach_token } => attach_token,
        HelloOutcome::Reject(reason) => panic!("hello rejected: {reason:?}"),
    };
    let handle = Arc::new(Mutex::new(Session::new(1024)));
    let activation = rt.activate(key, attach_token, Some(3600), handle).await.expect("activate must establish the slot");
    let lease_id = activation.lease.id();
    assert_eq!(rt.established_lease_for(key.session_id).await, Some(lease_id));
    let (r, w) = activation.tcp.into_split();
    activation.lease.keep();
    rt.park(id, lease_id, (r, w)).await;
    assert!(rt.is_parked(&id).await, "precondition: session is parked");
    lease_id
}

/// The interleaving Step 1.5 characterized (RESUME's unpark lands before the
/// sweep's removal). Before Step 2a the sweep still discarded the
/// just-resumed session and released its slot; now the sweep, applied after
/// the RESUME, sees an active session and does nothing.
#[tokio::test]
async fn sweep_after_concurrent_unpark_no_longer_discards_live_session() {
    let rt = AttachRuntime::new(spawn_target().await, 16);
    let id: SessionId = [0x11; 16];
    let session_id = isekai_protocol::SessionId::from_bytes(id);
    let established = establish_and_park(&rt, id).await;

    let held = rt.lock_core_for_test().await;
    let resumer = {
        let rt = rt.clone();
        tokio::spawn(async move { rt.resume_request(id).await })
    };
    settle().await;
    assert!(!resumer.is_finished(), "RESUME must be queued on the aggregate lock");
    let sweep = {
        let rt = rt.clone();
        tokio::spawn(async move { rt.sweep_expired_parked(StdDuration::ZERO).await })
    };
    settle().await;
    assert!(!sweep.is_finished(), "sweep must be queued behind RESUME");
    drop(held);

    let decision = resumer.await.unwrap();
    let discarded = sweep.await.unwrap();

    // RESUME won the unpark and got the socket *and* the same incarnation's
    // lease (unchanged precondition from Step 1.5).
    let grant = match decision {
        ResumeDecision::Granted(grant) => grant,
        ResumeDecision::Preempt { .. } | ResumeDecision::Rejected => panic!("RESUME must have been granted"),
    };
    assert_eq!(grant.lease, established);

    // FLIPPED by Step 2a (was: `discarded == vec![id]` /
    // "CURRENT BEHAVIOR: sweep reports the live session as discarded").
    assert!(discarded.is_empty(), "the sweep must not discard a session RESUME has just unparked");
    // FLIPPED (was: `!table.contains(&id)` / "the live session is gone from the table").
    assert!(rt.index_contains(&id).await, "the live session stays in the index");
    // FLIPPED (was: `established_lease_for == None` / "the relaying lease's
    // Established slot was released by the sweep backstop").
    assert_eq!(
        rt.established_lease_for(session_id).await,
        Some(established),
        "the relaying lease keeps its Established slot"
    );
    // FLIPPED (was: `!has_session` / "the same session_id is re-admittable mid-relay").
    assert!(rt.has_session(session_id).await, "the session_id is not re-admittable while it is relaying");

    drop(grant);
}

/// The opposite order: the sweep's apply runs first. The session is
/// discarded and its slot released atomically, and the RESUME that follows
/// is rejected without ever receiving a socket — so nothing relays through a
/// released slot.
#[tokio::test]
async fn sweep_before_resume_discards_atomically_and_rejects_the_resume() {
    let rt = AttachRuntime::new(spawn_target().await, 16);
    let id: SessionId = [0x22; 16];
    let session_id = isekai_protocol::SessionId::from_bytes(id);
    establish_and_park(&rt, id).await;

    let held = rt.lock_core_for_test().await;
    let sweep = {
        let rt = rt.clone();
        tokio::spawn(async move { rt.sweep_expired_parked(StdDuration::ZERO).await })
    };
    settle().await;
    assert!(!sweep.is_finished(), "sweep must be queued on the aggregate lock");
    let resumer = {
        let rt = rt.clone();
        tokio::spawn(async move { rt.resume_request(id).await })
    };
    settle().await;
    assert!(!resumer.is_finished(), "RESUME must be queued behind the sweep");
    drop(held);

    let discarded = sweep.await.unwrap();
    let decision = resumer.await.unwrap();

    assert_eq!(discarded, vec![id], "the sweep discards the expired parked session");
    assert!(
        matches!(decision, ResumeDecision::Rejected),
        "RESUME after the discard must be rejected and receive no socket"
    );
    assert!(!rt.index_contains(&id).await);
    assert_eq!(rt.established_lease_for(session_id).await, None, "slot released in the same apply as the discard");
    assert!(!rt.has_session(session_id).await);
}

/// Step 2a's intended behavior change (2) at the shell level (ADR I-g,
/// round 2 N-3): a session registered over capacity (formerly
/// `InsertOutcome::Rejected` — not in any table) whose data stream dies used
/// to be parked into a handle no table referenced, leaking its fencing slot
/// and target TCP until process exit (the same session_id's re-ATTACH was
/// rejected with `AttachAlreadyEstablished` forever). Now the park is a
/// `Discard{Unresumable}`: slot released, TCP closed.
#[tokio::test]
async fn parking_an_unresumable_session_releases_its_slot() {
    let rt = AttachRuntime::new(spawn_target().await, 1);
    // Fill the table with one actively relaying session.
    let active: SessionId = [0x31; 16];
    let key_active = key_for(active);
    let token = match rt.hello(key_active).await {
        HelloOutcome::Ready { attach_token } => attach_token,
        HelloOutcome::Reject(reason) => panic!("hello rejected: {reason:?}"),
    };
    let active_activation =
        rt.activate(key_active, token, Some(3600), Arc::new(Mutex::new(Session::new(1024)))).await.unwrap();

    // A second session is registered over capacity (unresumable).
    let over: SessionId = [0x32; 16];
    let key_over = key_for(over);
    let token = match rt.hello(key_over).await {
        HelloOutcome::Ready { attach_token } => attach_token,
        HelloOutcome::Reject(reason) => panic!("hello rejected: {reason:?}"),
    };
    let over_activation =
        rt.activate(key_over, token, Some(3600), Arc::new(Mutex::new(Session::new(1024)))).await.unwrap();
    let over_lease = over_activation.lease.id();
    assert!(rt.index_contains(&over).await, "unresumable sessions are indexed too (ADR I-c)");

    // Its data stream dies: `finish_or_park_session` keeps the lease and parks.
    let (r, w) = over_activation.tcp.into_split();
    over_activation.lease.keep();
    rt.park(over, over_lease, (r, w)).await;

    assert!(!rt.index_contains(&over).await);
    assert_eq!(rt.established_lease_for(key_over.session_id).await, None, "fencing slot released (was: leaked)");
    assert!(!rt.has_session(key_over.session_id).await, "the same session_id can ATTACH again");
    // The active session is untouched.
    assert_eq!(rt.established_lease_for(key_active.session_id).await, Some(active_activation.lease.id()));
    active_activation.lease.keep();
}
