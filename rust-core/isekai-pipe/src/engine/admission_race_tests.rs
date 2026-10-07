//! Shell-level test for the `--max-sessions` admission race
//! (`docs/adr/0019-functional-core-effects.md` §6 Step 2b).
//!
//! # The race (before Step 2b)
//!
//! `admit_new_session` checked `has_session()` / `session_count() <
//! max_sessions()` under the aggregate lock, **dropped the lock**, and only
//! then did `hello()` claim a fencing slot (`HelloReceived` → `Connecting`).
//! Two brand-new session_ids admitted concurrently could both pass the check
//! before either claimed a slot, so `max_sessions + 1` sessions held slots.
//!
//! # How the interleaving is forced deterministically
//!
//! Both admissions are released together by a [`Barrier`] and then queue on
//! the aggregate lock, which the test is holding (`lock_core_for_test`).
//! `tokio::sync::Mutex` is FIFO-fair and hands the permit directly to the
//! next waiter on release, so on the default `current_thread` runtime the
//! two tasks' lock acquisitions strictly alternate once the test lets go.
//! With the old split admission that means both checks run before either
//! `HelloReceived` — the max+1 outcome every time, not just sometimes.
//!
//! # After Step 2b
//!
//! `hello()` applies `AdmitRequested`, which judges capacity, evicts the
//! oldest parked session if needed and claims the slot (`HelloReceived`) in
//! **one** apply under that lock. Whichever admission is applied first takes
//! the last slot; the second sees it taken and is rejected with
//! `BusyOtherSession` without claiming anything.

use std::net::SocketAddr;
use std::sync::Arc;

use isekai_protocol::attach::{AttachKey, AttachRejectReason, AttemptId, ConnectionGeneration, ATTEMPT_ID_LEN};
use tokio::net::TcpListener;
use tokio::sync::{Barrier, Mutex};

use super::attach_runtime::{AttachRuntime, HelloOutcome};
use super::resume::SessionId;

async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

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

/// What `handle_attach_stream` does for an `ATTACH_HELLO` once the proof
/// checks out. Before Step 2b this was `admit_new_session(..)` followed by
/// `hello(..)` (this test was first committed against exactly that and
/// failed); now admission and the fencing transition are one apply.
async fn admit_and_hello(rt: &Arc<AttachRuntime>, key: AttachKey) -> HelloOutcome {
    rt.hello(key).await
}

/// Two brand-new sessions admitted concurrently into the last free slot:
/// exactly one gets it, the other is rejected with `BusyOtherSession`, and
/// the number of slots never exceeds `--max-sessions`.
#[tokio::test]
async fn concurrent_admissions_never_exceed_max_sessions() {
    let rt = AttachRuntime::new(spawn_target().await, 1);
    let barrier = Arc::new(Barrier::new(3));

    let held = rt.lock_core_for_test().await;
    let spawn_admission = |id: SessionId| {
        let rt = rt.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            admit_and_hello(&rt, key_for(id)).await
        })
    };
    let a = spawn_admission([0x41; 16]);
    let b = spawn_admission([0x42; 16]);
    barrier.wait().await;
    settle().await;
    assert!(!a.is_finished() && !b.is_finished(), "both admissions must be queued on the aggregate lock");
    drop(held);

    let outcomes = [a.await.unwrap(), b.await.unwrap()];
    let ready = outcomes.iter().filter(|o| matches!(o, HelloOutcome::Ready { .. })).count();
    let busy = outcomes
        .iter()
        .filter(|o| matches!(o, HelloOutcome::Reject(AttachRejectReason::BusyOtherSession)))
        .count();

    assert!(rt.session_count().await <= 1, "slots exceeded --max-sessions (max+1 admitted)");
    assert_eq!((ready, busy), (1, 1), "exactly one admission wins the last slot; the other is BusyOtherSession");
}
