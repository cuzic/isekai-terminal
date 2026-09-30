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

/// Upper bound on how long [`AttachRuntime::hello`] waits for its
/// `AttachReadyV2`/reject outcome (review 2026-09-29, PIPE-06). Every
/// arbiter transition that displaces a waiting key now resolves it
/// explicitly, so this is only a backstop against a future path that
/// forgets to: without it such a caller (its connection task, its
/// `AnyMuxConnection` clone, and its `waiters` entry) leaked forever, and
/// `serve --once` hung. Longer than `TARGET_CONNECT_TIMEOUT` — the longest
/// legitimate wait (a slow target connect) — plus margin.
const HELLO_OUTCOME_TIMEOUT: Duration = Duration::from_secs(30);

/// What a `hello()` caller needs in order to build the wire-level
/// `AttachResponse` — deliberately *not* the full wire type, since
/// `negotiated_resume_grace_secs` depends on `requested_resume_grace_secs`
/// (an ATTACH-unrelated policy value the connection task already knows),
/// which this runtime has no reason to also track.
#[derive(Clone, Copy)]
pub enum HelloOutcome {
    Ready { attach_token: AttachToken },
    Reject(AttachRejectReason),
}

enum LeaseResource {
    /// `task` is `None` between the arbiter emitting `ConnectTarget` (the
    /// entry is registered right then, inside [`AttachRuntime::apply_event`])
    /// and `start_connect` actually spawning the connect — so a
    /// `CancelLease` racing into that gap still finds the lease (PIPE-04).
    Connecting { task: Option<JoinHandle<()>> },
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

    /// The lease this guard protects (`None` only after `release`/`keep`,
    /// which consume `self`, so always `Some` for a caller holding one).
    pub fn lease_id(&self) -> Option<LeaseId> {
        self.lease
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
    /// One entry per waiting `hello()` call. A `Vec` rather than a single
    /// sender: a retransmitted ATTACH_HELLO for the same key (e.g. arriving
    /// on a fresh QUIC connection) used to *replace* the earlier caller's
    /// sender, dropping it — that caller then saw a spurious
    /// `Reject(Unsupported)` (PIPE-06). Every waiter now gets the outcome.
    waiters: Mutex<HashMap<AttachKey, Vec<oneshot::Sender<HelloOutcome>>>>,
    /// Serializes `engine/mod.rs`'s `--max-sessions` admission check with the
    /// arbiter slot reservation it guards (`hello_register`), so concurrent
    /// brand-new sessions can no longer all pass the same "below the cap"
    /// check before any of them is counted (PIPE-11).
    admission: Mutex<()>,
    next_target_id: AtomicU64,
    target: SocketAddr,
}

impl AttachRuntime {
    pub fn new(target: SocketAddr) -> Arc<Self> {
        Arc::new(Self {
            arbiter: Mutex::new(AttachArbiter::new()),
            leases: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
            admission: Mutex::new(()),
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
    #[cfg(test)]
    pub async fn hello(self: &Arc<Self>, key: AttachKey) -> HelloOutcome {
        let rx = self.hello_register(key).await;
        self.hello_wait(key, rx).await
    }

    /// First half of [`Self::hello`]: registers a waiter for `key` and
    /// applies `HelloReceived` — after this returns, a brand-new
    /// `session_id` already occupies an arbiter slot (so it is counted by
    /// [`Self::session_count`]). Split out so admission control can run
    /// "count, evict, reserve" under [`Self::admission_guard`] without also
    /// holding that lock across the (possibly many-second) wait for the
    /// outcome (PIPE-11).
    pub async fn hello_register(self: &Arc<Self>, key: AttachKey) -> oneshot::Receiver<HelloOutcome> {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.entry(key).or_default().push(tx);
        let effects = self.apply_event(AttachEvent::HelloReceived { key }).await;
        self.execute_effects(effects).await;
        rx
    }

    /// Second half of [`Self::hello`]: waits (bounded by
    /// `HELLO_OUTCOME_TIMEOUT`) for the outcome `hello_register` subscribed
    /// to. On timeout the now-dead sender is pruned from `waiters`.
    pub async fn hello_wait(self: &Arc<Self>, key: AttachKey, rx: oneshot::Receiver<HelloOutcome>) -> HelloOutcome {
        match tokio::time::timeout(HELLO_OUTCOME_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => HelloOutcome::Reject(AttachRejectReason::Unsupported),
            Err(_elapsed) => {
                log::warn!("attach_runtime: no ATTACH outcome for {key:?} within {HELLO_OUTCOME_TIMEOUT:?}; giving up");
                let mut waiters = self.waiters.lock().await;
                if let Some(senders) = waiters.get_mut(&key) {
                    senders.retain(|tx| !tx.is_closed());
                    if senders.is_empty() {
                        waiters.remove(&key);
                    }
                }
                HelloOutcome::Reject(AttachRejectReason::Target)
            }
        }
    }

    /// See the `admission` field's docs.
    pub async fn admission_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.admission.lock().await
    }

    /// Applies `AttachActivate`; on success (the activation matched the
    /// current `PendingActivation` lease), returns the target `TcpStream`
    /// the connection task should now relay through — ownership fully
    /// transfers out of this runtime's bookkeeping at this point — paired
    /// with an [`EstablishedLease`] minted at exactly this transition
    /// (the arbiter has just moved this session to `Established`, per
    /// `AttachArbiter::on_activated`).
    pub async fn activate(self: &Arc<Self>, key: AttachKey, attach_token: AttachToken) -> Option<(TcpStream, EstablishedLease)> {
        let effects = self.apply_event(AttachEvent::Activated { key, attach_token }).await;
        for effect in effects {
            if let AttachEffect::StartRelay { lease, .. } = effect {
                let resource = self.leases.lock().await.remove(&lease);
                match resource {
                    Some(LeaseResource::PendingTarget { tcp, timer }) => {
                        if let Some(timer) = timer {
                            timer.abort();
                        }
                        return Some((tcp, EstablishedLease::new(self.clone(), lease)));
                    }
                    other => {
                        // The arbiter already moved this session to
                        // `Established`, but there is no target connection to
                        // hand out (should be unreachable since PIPE-04's
                        // `start_connect` fix, but defended here regardless):
                        // no `EstablishedLease` will ever exist for it, so
                        // nobody would ever call `relay_ended` — give the slot
                        // back right now instead of orphaning it forever.
                        log::warn!(
                            "attach_runtime: StartRelay for lease {lease:?} found no pending target; releasing the slot"
                        );
                        if let Some(LeaseResource::Connecting { task: Some(task) }) = other {
                            task.abort();
                        }
                        self.relay_ended(lease).await;
                        return None;
                    }
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
        let effects = self.apply_event(AttachEvent::CancelReceived { key }).await;
        self.execute_effects(effects).await;
    }

    /// The connection task that reached `Established` calls this once its
    /// relay loop actually ends *for good* (target TCP died — not merely
    /// parked for a possible resume, which leaves the arbiter `Established`
    /// so a matching `RESUME` can still find its slot).
    pub async fn relay_ended(self: &Arc<Self>, lease: LeaseId) {
        let effects = self.apply_event(AttachEvent::RelayEnded { lease }).await;
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
        if let Some(senders) = self.waiters.lock().await.remove(&key) {
            for tx in senders {
                let _ = tx.send(outcome);
            }
        }
    }

    /// Spawns the target `TcpStream::connect` and records `Connecting { task }`
    /// in `leases` **while still holding the `leases` lock across the spawn**,
    /// so a `CancelLease` effect processed immediately afterward always finds
    /// an entry to abort, and — the actual correctness point (review
    /// 2026-09-29, PIPE-04) — the spawned task's own `leases` access (its
    /// `Connecting` → `PendingTarget` transition) is guaranteed to happen
    /// *after* this registration. The previous shape spawned first and
    /// inserted `Connecting` afterwards; on the multi-threaded runtime a fast
    /// loopback connect could insert `PendingTarget { tcp }` first and then
    /// have it overwritten by the late `Connecting { task }`, so `activate()`
    /// found no target, never minted an `EstablishedLease`, and the slot sat
    /// `Established` forever (`.claude/rules/always-connects.md`).
    async fn start_connect(self: &Arc<Self>, lease: LeaseId) {
        let this = self.clone();
        let target_addr = self.target;
        let mut leases = self.leases.lock().await;
        // `apply_event` registered `Connecting { task: None }` together with
        // the `ConnectTarget` effect. If it is gone, a `CancelLease` already
        // won the race (and reported `LeaseStopped`) — don't connect at all.
        if !matches!(leases.get(&lease), Some(LeaseResource::Connecting { task: None })) {
            return;
        }
        let task = tokio::spawn(async move {
            match tokio::time::timeout(TARGET_CONNECT_TIMEOUT, TcpStream::connect(target_addr)).await {
                Ok(Ok(tcp)) => {
                    let target_id = TargetHandleId(this.next_target_id.fetch_add(1, Ordering::SeqCst));
                    let mut token_bytes = [0u8; ATTACH_TOKEN_LEN];
                    rand::rngs::OsRng.fill_bytes(&mut token_bytes);
                    let attach_token = AttachToken::new(token_bytes);
                    {
                        let mut leases = this.leases.lock().await;
                        // Only a lease still `Connecting` may advance: if the
                        // entry is gone, `cancel_lease` already took it (and
                        // is aborting/awaiting this very task), so the fresh
                        // TCP connection must simply be dropped here.
                        if !matches!(leases.get(&lease), Some(LeaseResource::Connecting { .. })) {
                            return;
                        }
                        leases.insert(lease, LeaseResource::PendingTarget { tcp, timer: None });
                    }
                    let effects = this.apply_event(AttachEvent::TargetConnected {
                        lease,
                        target: target_id,
                        attach_token,
                    })
                    .await;
                    this.execute_effects(effects).await;
                }
                Ok(Err(e)) => {
                    log::info!("attach_runtime: target connect failed for lease {lease:?}: {e}");
                    this.forget_connecting(lease).await;
                    let effects = this.apply_event(AttachEvent::TargetConnectFailed { lease }).await;
                    this.execute_effects(effects).await;
                }
                Err(_elapsed) => {
                    log::info!(
                        "attach_runtime: target connect timed out after {TARGET_CONNECT_TIMEOUT:?} for lease {lease:?}"
                    );
                    this.forget_connecting(lease).await;
                    let effects = this.apply_event(AttachEvent::TargetConnectFailed { lease }).await;
                    this.execute_effects(effects).await;
                }
            }
        });
        leases.insert(lease, LeaseResource::Connecting { task: Some(task) });
    }

    /// Applies `event` to the arbiter **while holding the `leases` lock**, and
    /// registers a `Connecting { task: None }` placeholder for every
    /// `ConnectTarget` effect before either lock is released. Without this,
    /// between `apply` returning `ConnectTarget` and `start_connect` getting
    /// to register the lease, a concurrent `CancelLease` (e.g. a superseding
    /// generation arriving on another connection) found no entry and did
    /// nothing, leaving `ClosingForSupersede` waiting forever for a
    /// `LeaseStopped` nobody would send (PIPE-04). Lock order is always
    /// `leases` → `arbiter`; no code path takes them the other way round.
    async fn apply_event(self: &Arc<Self>, event: AttachEvent) -> Vec<AttachEffect> {
        let mut leases = self.leases.lock().await;
        let effects = self.arbiter.lock().await.apply(event);
        for effect in &effects {
            if let AttachEffect::ConnectTarget { lease } = effect {
                leases.insert(*lease, LeaseResource::Connecting { task: None });
            }
        }
        effects
    }

    /// Drops `lease`'s `Connecting` bookkeeping once its connect attempt has
    /// failed for good — without this the entry (and its finished
    /// `JoinHandle`) stayed in `leases` forever (PIPE-04's side note).
    async fn forget_connecting(self: &Arc<Self>, lease: LeaseId) {
        let mut leases = self.leases.lock().await;
        if matches!(leases.get(&lease), Some(LeaseResource::Connecting { .. })) {
            leases.remove(&lease);
        }
    }

    async fn cancel_lease(self: &Arc<Self>, lease: LeaseId) {
        let resource = self.leases.lock().await.remove(&lease);
        match resource {
            Some(LeaseResource::Connecting { task }) => {
                if let Some(task) = task {
                    task.abort();
                    let _ = task.await;
                }
                let effects = self.apply_event(AttachEvent::LeaseStopped { lease }).await;
                self.execute_effects(effects).await;
            }
            Some(LeaseResource::PendingTarget { tcp, timer }) => {
                if let Some(timer) = timer {
                    timer.abort();
                }
                drop(tcp);
                let effects = self.apply_event(AttachEvent::LeaseStopped { lease }).await;
                self.execute_effects(effects).await;
            }
            None => {
                // Nothing left to tear down: the connect attempt already
                // finished and dropped its own bookkeeping (`forget_connecting`
                // after a failed connect). The lease is therefore genuinely
                // stopped — say so, or a `ClosingForSupersede` waiting on this
                // lease would never advance to its `next` attempt. Harmless
                // otherwise: the arbiter ignores `LeaseStopped` for a lease it
                // isn't superseding.
                let effects = self.apply_event(AttachEvent::LeaseStopped { lease }).await;
                self.execute_effects(effects).await;
            }
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
            let effects = this.apply_event(AttachEvent::PendingExpired { lease }).await;
            this.execute_effects(effects).await;
        });
        match self.leases.lock().await.get_mut(&lease) {
            Some(LeaseResource::PendingTarget { timer: slot, .. }) => *slot = Some(timer),
            _ => timer.abort(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isekai_protocol::attach::{AttemptId, ConnectionGeneration};
    use isekai_protocol::SessionId;

    fn key(session: u8, generation: u64, attempt: u8) -> AttachKey {
        AttachKey {
            session_id: SessionId::from_bytes([session; 16]),
            generation: ConnectionGeneration::new(generation),
            attempt_id: AttemptId::from_bytes([attempt; 16]),
        }
    }

    /// A target that accepts and holds every connection open.
    async fn listening_target() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        addr
    }

    /// A loopback address nothing listens on (bound once, then released).
    fn closed_target() -> SocketAddr {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        addr
    }

    /// PIPE-04 regression: the fast loopback connect used to be able to
    /// finish (and insert `PendingTarget`) before `start_connect` registered
    /// `Connecting`, which then overwrote it — `activate()` found nothing and
    /// the slot was orphaned `Established`. Repeated many times on the
    /// multi-threaded runtime, where that interleaving was reachable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn hello_then_activate_always_hands_out_the_target_connection() {
        let target = listening_target().await;
        for i in 0..200u32 {
            let runtime = AttachRuntime::new(target);
            let k = key((i % 250) as u8, 0, 1);
            let HelloOutcome::Ready { attach_token } = runtime.hello(k).await else {
                panic!("iteration {i}: expected Ready");
            };
            let activated = runtime.activate(k, attach_token).await;
            assert!(activated.is_some(), "iteration {i}: activate() lost the target connection");
            let (_tcp, lease) = activated.unwrap();
            lease.release().await;
            assert!(runtime.is_vacant().await, "iteration {i}: slot not released");
        }
    }

    /// PIPE-04 side note: a failed connect must not leave its `Connecting`
    /// bookkeeping behind forever.
    #[tokio::test]
    async fn failed_target_connect_rejects_and_leaves_no_lease_bookkeeping() {
        let runtime = AttachRuntime::new(closed_target());
        let outcome = runtime.hello(key(1, 0, 1)).await;
        assert!(matches!(outcome, HelloOutcome::Reject(AttachRejectReason::Target)));
        assert!(runtime.leases.lock().await.is_empty());
        assert!(runtime.is_vacant().await);
    }

    /// PIPE-04: if the arbiter reaches `Established` but no target resource
    /// exists, `activate()` must give the slot back rather than orphan it.
    #[tokio::test]
    async fn activate_without_a_pending_target_releases_the_slot() {
        let runtime = AttachRuntime::new(listening_target().await);
        let k = key(1, 0, 1);
        let HelloOutcome::Ready { attach_token } = runtime.hello(k).await else { panic!("expected Ready") };
        // Simulate the lost resource.
        runtime.leases.lock().await.clear();
        assert!(runtime.activate(k, attach_token).await.is_none());
        assert!(runtime.is_vacant().await, "slot must not stay Established without an EstablishedLease");
    }

    /// PIPE-06: a waiter superseded by a strictly larger generation while
    /// still `Connecting` used to wait forever; it must now be told
    /// `StaleGeneration`. Runs on the (default) current-thread runtime, so
    /// the first attempt's spawned connect task cannot run before the
    /// second `hello_register` supersedes it — the old attempt is
    /// deterministically still `Connecting` at that point.
    #[tokio::test]
    async fn superseded_connecting_waiter_is_rejected_instead_of_hanging() {
        let runtime = AttachRuntime::new(listening_target().await);
        let old = key(1, 5, 1);
        let rx_old = runtime.hello_register(old).await;
        let _rx_new = runtime.hello_register(key(1, 6, 1)).await;
        let outcome = tokio::time::timeout(Duration::from_secs(5), rx_old)
            .await
            .expect("superseded waiter must be resolved promptly")
            .expect("sender must not just be dropped");
        assert!(matches!(
            outcome,
            HelloOutcome::Reject(AttachRejectReason::StaleGeneration { current_generation }) if current_generation == ConnectionGeneration::new(6)
        ));
    }

    /// PIPE-06: a retransmitted HELLO for the same key used to replace (and
    /// thereby drop) the earlier caller's sender; both must get the outcome.
    #[tokio::test]
    async fn retransmitted_hello_for_the_same_key_resolves_every_waiter() {
        let runtime = AttachRuntime::new(listening_target().await);
        let k = key(1, 0, 1);
        let rx1 = runtime.hello_register(k).await;
        let rx2 = runtime.hello_register(k).await;
        let o1 = tokio::time::timeout(Duration::from_secs(5), rx1).await.unwrap().unwrap();
        let o2 = tokio::time::timeout(Duration::from_secs(5), rx2).await.unwrap().unwrap();
        assert!(matches!(o1, HelloOutcome::Ready { .. }));
        assert!(matches!(o2, HelloOutcome::Ready { .. }));
    }

    /// PIPE-06: CANCEL for a still-connecting attempt resolves its waiter
    /// (current-thread runtime: the connect task has not run yet, see above).
    #[tokio::test]
    async fn cancel_resolves_the_cancelled_attempts_waiter() {
        let runtime = AttachRuntime::new(listening_target().await);
        let k = key(1, 0, 1);
        let rx = runtime.hello_register(k).await;
        runtime.cancel(k).await;
        let outcome = tokio::time::timeout(Duration::from_secs(5), rx).await.unwrap().unwrap();
        assert!(matches!(outcome, HelloOutcome::Reject(AttachRejectReason::AlreadyAttached)));
        assert!(runtime.is_vacant().await);
    }
}
