//! Real (I/O-performing) effect executor around [`AttachArbiter`]
//! (`#18-3`), replacing `engine/mod.rs`'s single `active: Arc<AtomicBool>`
//! compare-exchange. One [`AttachRuntime`] is created per `isekai-pipe serve`
//! process (mirrors `active`'s old lifetime) and shared across every
//! accepted QUIC connection.
//!
//! Ownership split, so the pure reducer never touches a socket:
//! - [`AttachArbiter`] (this crate's `attach_arbiter` module): decides *what*
//!   should happen.
//! - [`AttachRuntime`] (this module): does it — spawns the target `TcpStream`
//!   connect, mints `AttachToken`s, arms/cancels the pending-activation
//!   timer, and routes `AttachReadyV2`/reject outcomes back to whichever
//!   connection's `hello()` call is waiting for them (which may be a
//!   *different* task than the one that ultimately caused the resolution —
//!   e.g. a superseded attempt's eventual `ConnectTarget` success is reported
//!   by a background task, not by the connection that is still blocked in
//!   `hello()`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use isekai_protocol::attach::{AttachKey, AttachRejectReason, AttachToken, ATTACH_TOKEN_LEN};
use rand::RngCore;
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

use super::attach_arbiter::{AttachArbiter, AttachEffect, AttachEvent, AttachState, LeaseId, TargetHandleId};

/// How long a `PendingActivation` lease may wait for `AttachActivate` before
/// the runtime gives up and closes the target connection
/// (`AttachEvent::PendingExpired`). Mirrors `HELLO_TIMEOUT`'s role for the
/// original v1 HELLO/ACK exchange.
const PENDING_ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);

/// What a `hello()` caller needs in order to build the wire-level
/// `AttachResponse` — deliberately *not* the full wire type, since
/// `negotiated_resume_grace_secs` depends on `requested_resume_grace_secs`
/// (an ATTACH-unrelated policy value the connection task already knows),
/// which this runtime has no reason to also track.
pub enum HelloOutcome {
    Ready { lease: LeaseId, attach_token: AttachToken },
    Reject(AttachRejectReason),
}

enum LeaseResource {
    Connecting { task: JoinHandle<()> },
    PendingTarget { tcp: TcpStream, timer: Option<JoinHandle<()>> },
}

pub struct AttachRuntime {
    arbiter: Mutex<AttachArbiter>,
    leases: Mutex<HashMap<LeaseId, LeaseResource>>,
    waiters: Mutex<HashMap<AttachKey, oneshot::Sender<HelloOutcome>>>,
    next_target_id: AtomicU64,
    target: SocketAddr,
}

impl AttachRuntime {
    pub fn new(target: SocketAddr) -> Arc<Self> {
        Arc::new(Self {
            arbiter: Mutex::new(AttachArbiter::new()),
            leases: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
            next_target_id: AtomicU64::new(0),
            target,
        })
    }

    /// Whether the arbiter currently holds no session at all — used for the
    /// `--max-idle-lifetime` monitor, mirroring `active.load(..)`'s old role
    /// (self-terminate only once nothing is attached/attaching/established).
    pub async fn is_vacant(&self) -> bool {
        matches!(self.arbiter.lock().await.state(), AttachState::Vacant)
    }

    /// The lease currently backing `session_id`'s `Established` slot, if any
    /// — `RESUME` (a wire family entirely separate from ATTACH v2) uses this
    /// to confirm it is reattaching to the session that actually occupies
    /// the slot, without itself going through `HelloReceived`/fencing at all
    /// (module docs: resuming the *same* session is never a fencing
    /// conflict, since the whole point of `RESUME` is that it already won
    /// its round).
    pub async fn established_lease_for(&self, session_id: isekai_protocol::SessionId) -> Option<LeaseId> {
        match self.arbiter.lock().await.state() {
            AttachState::Established { key, lease } if key.session_id == session_id => Some(*lease),
            _ => None,
        }
    }

    /// The `(session_id, lease)` currently occupying the `Established` slot,
    /// if any — used by a fresh `ATTACH_HELLO` that just lost the fencing
    /// race with `BusyOtherSession` to find out *whose* session is in the
    /// way, so the caller (which alone has access to `SessionTable`, this
    /// runtime deliberately doesn't) can attempt to preempt it if — and only
    /// if — it turns out to be merely parked rather than actively relaying
    /// (`engine/mod.rs`'s `handle_attach_stream`, per `ISEKAI_PIPE_DESIGN.md`
    /// §8's parked-session-preemption design). A plain read: taking no lock
    /// beyond the snapshot itself, so the answer can be stale by the time
    /// the caller acts on it — that's fine, the actual eviction decision is
    /// made atomically by `SessionTable::claim_parked`, not here.
    pub async fn current_established(&self) -> Option<(isekai_protocol::SessionId, LeaseId)> {
        match self.arbiter.lock().await.state() {
            AttachState::Established { key, lease } => Some((key.session_id, *lease)),
            _ => None,
        }
    }

    /// Entry point for a data-stream `ATTACH_HELLO`: registers a waiter for
    /// `key`, applies the event, executes whatever effects come back
    /// immediately, then waits (possibly across further effects executed by
    /// *other* tasks later) for the eventual `AttachReadyV2`/reject outcome.
    pub async fn hello(self: &Arc<Self>, key: AttachKey) -> HelloOutcome {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.insert(key, tx);
        let effects = self.arbiter.lock().await.apply(AttachEvent::HelloReceived { key });
        self.execute_effects(effects).await;
        rx.await.unwrap_or(HelloOutcome::Reject(AttachRejectReason::Unsupported))
    }

    /// Applies `AttachActivate`; on success (the activation matched the
    /// current `PendingActivation` lease), returns the target `TcpStream`
    /// the connection task should now relay through — ownership fully
    /// transfers out of this runtime's bookkeeping at this point.
    pub async fn activate(self: &Arc<Self>, key: AttachKey, attach_token: AttachToken) -> Option<TcpStream> {
        let effects = self.arbiter.lock().await.apply(AttachEvent::Activated { key, attach_token });
        for effect in effects {
            if let AttachEffect::StartRelay { lease, .. } = effect {
                if let Some(LeaseResource::PendingTarget { tcp, timer }) = self.leases.lock().await.remove(&lease) {
                    if let Some(timer) = timer {
                        timer.abort();
                    }
                    return Some(tcp);
                }
            }
        }
        None
    }

    pub async fn cancel(self: &Arc<Self>, key: AttachKey) {
        let effects = self.arbiter.lock().await.apply(AttachEvent::CancelReceived { key });
        self.execute_effects(effects).await;
    }

    /// The connection task that reached `Established` calls this once its
    /// relay loop actually ends *for good* (target TCP died — not merely
    /// parked for a possible resume, which leaves the arbiter `Established`
    /// so a matching `RESUME` can still find its slot).
    pub async fn relay_ended(self: &Arc<Self>, lease: LeaseId) {
        let effects = self.arbiter.lock().await.apply(AttachEvent::RelayEnded { lease });
        self.execute_effects(effects).await;
    }

    fn execute_effects<'a>(
        self: &'a Arc<Self>,
        effects: Vec<AttachEffect>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            for effect in effects {
                match effect {
                    AttachEffect::ConnectTarget { lease } => self.start_connect(lease).await,
                    AttachEffect::CancelLease { lease } => self.cancel_lease(lease).await,
                    AttachEffect::SendReady { key, lease, attach_token } => {
                        self.resolve_waiter(key, HelloOutcome::Ready { lease, attach_token }).await;
                    }
                    AttachEffect::SendReject { key, reason } => {
                        self.resolve_waiter(key, HelloOutcome::Reject(reason)).await;
                    }
                    AttachEffect::SchedulePendingTimeout { lease } => self.arm_pending_timeout(lease).await,
                    AttachEffect::StartRelay { .. } => {
                        // Only ever produced by `Activated`, which `activate()`
                        // handles directly rather than through this generic
                        // path — reaching this arm would mean some other event
                        // triggered it, which the reducer never does.
                        log::warn!("attach_runtime: unexpected StartRelay effect outside activate()");
                    }
                }
            }
        })
    }

    async fn resolve_waiter(self: &Arc<Self>, key: AttachKey, outcome: HelloOutcome) {
        if let Some(tx) = self.waiters.lock().await.remove(&key) {
            let _ = tx.send(outcome);
        }
    }

    /// Spawns the target `TcpStream::connect`, then — synchronously, before
    /// this function returns — records `Connecting { task }` in `leases` so
    /// a `CancelLease` effect processed immediately afterward always finds
    /// an entry to abort.
    async fn start_connect(self: &Arc<Self>, lease: LeaseId) {
        let this = self.clone();
        let target_addr = self.target;
        let task = tokio::spawn(async move {
            match TcpStream::connect(target_addr).await {
                Ok(tcp) => {
                    let target_id = TargetHandleId(this.next_target_id.fetch_add(1, Ordering::SeqCst));
                    let mut token_bytes = [0u8; ATTACH_TOKEN_LEN];
                    rand::rngs::OsRng.fill_bytes(&mut token_bytes);
                    let attach_token = AttachToken::new(token_bytes);
                    this.leases.lock().await.insert(lease, LeaseResource::PendingTarget { tcp, timer: None });
                    let effects = this.arbiter.lock().await.apply(AttachEvent::TargetConnected {
                        lease,
                        target: target_id,
                        attach_token,
                    });
                    this.execute_effects(effects).await;
                }
                Err(e) => {
                    log::info!("attach_runtime: target connect failed for lease {lease:?}: {e}");
                    let effects = this.arbiter.lock().await.apply(AttachEvent::TargetConnectFailed { lease });
                    this.execute_effects(effects).await;
                }
            }
        });
        self.leases.lock().await.insert(lease, LeaseResource::Connecting { task });
    }

    async fn cancel_lease(self: &Arc<Self>, lease: LeaseId) {
        let resource = self.leases.lock().await.remove(&lease);
        match resource {
            Some(LeaseResource::Connecting { task }) => {
                task.abort();
                let _ = task.await;
                let effects = self.arbiter.lock().await.apply(AttachEvent::LeaseStopped { lease });
                self.execute_effects(effects).await;
            }
            Some(LeaseResource::PendingTarget { tcp, timer }) => {
                if let Some(timer) = timer {
                    timer.abort();
                }
                drop(tcp);
                let effects = self.arbiter.lock().await.apply(AttachEvent::LeaseStopped { lease });
                self.execute_effects(effects).await;
            }
            None => {}
        }
    }

    /// Arms the pending-activation timer and, once the lease is still
    /// `PendingTarget` (it may have already moved on — activated, expired
    /// via a different path, or been cancelled — by the time this runs),
    /// records the timer's `JoinHandle` so `activate()`/`cancel_lease` can
    /// abort it once it is no longer needed.
    async fn arm_pending_timeout(self: &Arc<Self>, lease: LeaseId) {
        let this = self.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(PENDING_ACTIVATION_TIMEOUT).await;
            let effects = this.arbiter.lock().await.apply(AttachEvent::PendingExpired { lease });
            this.execute_effects(effects).await;
        });
        match self.leases.lock().await.get_mut(&lease) {
            Some(LeaseResource::PendingTarget { timer: slot, .. }) => *slot = Some(timer),
            _ => timer.abort(),
        }
    }
}
