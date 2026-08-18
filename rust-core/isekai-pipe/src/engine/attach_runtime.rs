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

/// Bounds `start_connect`'s `TcpStream::connect` to `target` (normally
/// `127.0.0.1:22`, but configurable). Without this, a target that
/// blackholes the SYN (firewalled but not RST-ing) leaves the lease stuck in
/// `Connecting` for however long the OS's own TCP connect timeout is
/// (minutes on Linux) — a client retry with a new `generation` still
/// self-heals via `ClosingForSupersede`, so this isn't a stuck-forever bug,
/// just an unnecessarily long first-attempt hang. Deliberately longer than
/// `HELLO_TIMEOUT`/`PENDING_ACTIVATION_TIMEOUT` (both 5s, bounding a
/// same-process wire exchange) since this bounds an actual TCP handshake
/// over the network to `target`.
const TARGET_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// What a `hello()` caller needs in order to build the wire-level
/// `AttachResponse` — deliberately *not* the full wire type, since
/// `negotiated_resume_grace_secs` depends on `requested_resume_grace_secs`
/// (an ATTACH-unrelated policy value the connection task already knows),
/// which this runtime has no reason to also track.
pub enum HelloOutcome {
    Ready { attach_token: AttachToken },
    Reject(AttachRejectReason),
}

enum LeaseResource {
    Connecting { task: JoinHandle<()> },
    PendingTarget { tcp: TcpStream, timer: Option<JoinHandle<()>> },
}

/// RAII guard over an `Established` fencing slot. `AttachRuntime::activate`
/// mints one exactly when the arbiter actually transitions a session to
/// `Established` (never earlier — a session in `PendingActivation` isn't
/// occupying the slot this guard is meant to protect, and releasing it
/// early would defeat both the parked-for-resume case and the ordinary
/// `AttachActivate` timeout case; see `engine/mod.rs`'s call sites for why
/// this must be minted at the `StartRelay`/`respond_resume_accepted`
/// boundary and nowhere earlier).
///
/// Whoever owns this must eventually call exactly one of [`Self::release`]
/// (the target TCP died for good) or [`Self::keep`] (park the slot for a
/// possible `RESUME`). If neither runs — most notably because the owning
/// task panicked while relaying — `Drop` falls back to releasing the slot
/// itself, so `.claude/rules/always-connects.md`'s "a missed `relay_ended`
/// permanently orphans the slot" failure mode degrades to "released a
/// little late by the Drop fallback" instead.
pub struct EstablishedLease {
    runtime: Arc<AttachRuntime>,
    lease: Option<LeaseId>,
}

impl EstablishedLease {
    fn new(runtime: Arc<AttachRuntime>, lease: LeaseId) -> Self {
        Self { runtime, lease: Some(lease) }
    }

    /// The target TCP connection died for good — release the slot now.
    pub async fn release(mut self) {
        if let Some(lease) = self.lease.take() {
            self.runtime.relay_ended(lease).await;
        }
    }

    /// The data stream died but the target TCP is still alive and parked
    /// for a possible `RESUME` — the slot must stay `Established`, so
    /// consume this guard without releasing.
    pub fn keep(mut self) {
        self.lease = None;
    }
}

impl Drop for EstablishedLease {
    /// Best-effort fallback only: never panics (a panic here, while already
    /// unwinding from the panic that skipped `release()`/`keep()`, would
    /// abort the process — exactly the failure mode this guard exists to
    /// avoid) and never awaits directly (`relay_ended` is async; `Drop`
    /// isn't). If no tokio runtime is reachable (e.g. this guard outlives
    /// the runtime during process shutdown), the slot simply can't be
    /// released here — logged so it's visible, left to the existing
    /// `sweep_expired_parked` backstop.
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else { return };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            log::warn!(
                "EstablishedLease dropped outside a tokio runtime; lease {lease:?} could not be released"
            );
            return;
        };
        log::warn!(
            "EstablishedLease dropped without release()/keep() (lease={lease:?}); \
             releasing via Drop fallback — the owning task likely panicked or returned early"
        );
        let runtime = self.runtime.clone();
        handle.spawn(async move {
            runtime.relay_ended(lease).await;
        });
    }
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
        self.arbiter.lock().await.session_count() == 0
    }

    /// How many sessions currently hold a slot (connecting, pending, or
    /// established/parked) — used by `engine/mod.rs`'s Epic N-5 admission
    /// control to decide whether a brand-new `session_id` fits under
    /// `--max-sessions` without needing to evict anything first.
    pub async fn session_count(&self) -> usize {
        self.arbiter.lock().await.session_count()
    }

    /// Whether `session_id` already holds a slot (of any kind) — a
    /// retransmit or reattach of a session already known to the arbiter
    /// never counts against the `--max-sessions` admission check, only a
    /// genuinely new `session_id` does.
    pub async fn has_session(&self, session_id: isekai_protocol::SessionId) -> bool {
        self.arbiter.lock().await.has_session(session_id)
    }

    /// The lease currently backing `session_id`'s `Established` slot, if any
    /// — `RESUME` (a wire family entirely separate from ATTACH v2) uses this
    /// to confirm it is reattaching to the session that actually occupies
    /// the slot, without itself going through `HelloReceived`/fencing at all
    /// (module docs: resuming the *same* session is never a fencing
    /// conflict, since the whole point of `RESUME` is that it already won
    /// its round).
    pub async fn established_lease_for(&self, session_id: isekai_protocol::SessionId) -> Option<LeaseId> {
        match self.arbiter.lock().await.state_for(session_id) {
            Some(AttachState::Established { lease, .. }) => Some(*lease),
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
    /// transfers out of this runtime's bookkeeping at this point — paired
    /// with an [`EstablishedLease`] minted at exactly this transition
    /// (the arbiter has just moved this session to `Established`, per
    /// `AttachArbiter::on_activated`).
    pub async fn activate(self: &Arc<Self>, key: AttachKey, attach_token: AttachToken) -> Option<(TcpStream, EstablishedLease)> {
        let effects = self.arbiter.lock().await.apply(AttachEvent::Activated { key, attach_token });
        for effect in effects {
            if let AttachEffect::StartRelay { lease, .. } = effect {
                if let Some(LeaseResource::PendingTarget { tcp, timer }) = self.leases.lock().await.remove(&lease) {
                    if let Some(timer) = timer {
                        timer.abort();
                    }
                    return Some((tcp, EstablishedLease::new(self.clone(), lease)));
                }
            }
        }
        None
    }

    /// Mints an [`EstablishedLease`] for a lease already known to be
    /// `Established` — used by the `RESUME` path, which reattaches to a
    /// slot `hello()`/`activate()` established on a *previous* connection
    /// (`established_lease_for` looked it up) rather than transitioning it
    /// itself. Callers must only call this once the slot is genuinely about
    /// to be relayed through again (right before `relay_buffered`, after
    /// every earlier repark-and-return path) — see `engine/mod.rs`'s
    /// `handle_resume_stream` for why minting this too early would let a
    /// guard dropped on a rejected/reparked `RESUME` wrongly release a slot
    /// that must stay `Established`.
    pub fn resumed_lease(self: &Arc<Self>, lease: LeaseId) -> EstablishedLease {
        EstablishedLease::new(self.clone(), lease)
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
                    AttachEffect::SendReady { key, attach_token } => {
                        self.resolve_waiter(key, HelloOutcome::Ready { attach_token }).await;
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
            match tokio::time::timeout(TARGET_CONNECT_TIMEOUT, TcpStream::connect(target_addr)).await {
                Ok(Ok(tcp)) => {
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
                Ok(Err(e)) => {
                    log::info!("attach_runtime: target connect failed for lease {lease:?}: {e}");
                    let effects = this.arbiter.lock().await.apply(AttachEvent::TargetConnectFailed { lease });
                    this.execute_effects(effects).await;
                }
                Err(_elapsed) => {
                    log::info!(
                        "attach_runtime: target connect timed out after {TARGET_CONNECT_TIMEOUT:?} for lease {lease:?}"
                    );
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
