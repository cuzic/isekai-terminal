//! Real (I/O-performing) effect executor around [`ServeAggregate`]
//! (`#18-3`, docs/adr/0019-functional-core-effects.md §6 Step 2a), replacing
//! `engine/mod.rs`'s single `active: Arc<AtomicBool>` compare-exchange and —
//! since Step 2a — the separate `SessionTable` lock. One [`AttachRuntime`] is
//! created per `isekai-pipe serve` process and shared across every accepted
//! QUIC connection.
//!
//! Ownership split, so the pure reducer never touches a socket:
//! - [`ServeAggregate`] (`serve_fsm` module: `AttachArbiter` fencing + the
//!   resume session index): decides *what* should happen.
//! - [`AttachRuntime`] (this module): does it — spawns the target `TcpStream`
//!   connect, mints `AttachToken`s, arms/cancels the pending-activation
//!   timer, routes `AttachReadyV2`/reject outcomes back to whichever
//!   connection's `hello()` call is waiting for them, and owns the parked
//!   target sockets / per-session output buffers ([`SessionIo`]).
//!
//! **Locking (ADR §2.4)**: the aggregate *and* the socket map live under one
//! lock (`core`). [`AttachRuntime::apply_with`] stamps `now` *after*
//! acquiring it, applies the event, and interprets the in-lock effects
//! (`RegisterIo`/`Discard`/`StoreParked`/`ResumeGranted`/`RequestPreempt`/
//! `ResumeRejected`, see [`interpret_in_lock`]) in the same critical section;
//! out-of-lock effects (`AttachEffect`s, `Notify` wake-ups) run after the
//! lock is released. `leases`/`waiters` stay separate locks and are never
//! taken while `core` is held (and vice versa); the per-session
//! `Arc<Mutex<Session>>` (output buffer hot path) is likewise never locked
//! while `core` is held.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use isekai_protocol::attach::{AttachKey, AttachRejectReason, AttachToken, ATTACH_TOKEN_LEN};
use isekai_protocol::Millis;
use rand::RngCore;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex, Notify};
use tokio::task::JoinHandle;

use super::attach_arbiter::{AttachEffect, AttachState, LeaseId, TargetHandleId};
use super::resume::Session;
use super::serve_fsm::{DiscardCause, ServeAggregate, ServeEffect, ServeEvent, SessionKey, TerminateReason};

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
/// explicitly (`SendReject`), so this is only a backstop against a future
/// path that forgets to: without it such a caller (its connection task, its
/// `AnyMuxConnection` clone, and its `waiters` entry) leaked forever, and
/// `serve --once` hung. Longer than `TARGET_CONNECT_TIMEOUT` — the longest
/// legitimate wait (a slow target connect) — plus margin.
const HELLO_OUTCOME_TIMEOUT: Duration = Duration::from_secs(30);

/// The target TCP connection of a session whose data stream is gone, kept
/// for a possible `RESUME`.
pub type ParkedTcp = (OwnedReadHalf, OwnedWriteHalf);

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
    Connecting { task: JoinHandle<()> },
    PendingTarget { tcp: TcpStream, timer: Option<JoinHandle<()>> },
}

/// Shell-side resources of one session incarnation (ADR §6 Step 2a). Lives
/// in the same map, under the same lock, as the aggregate's index entry —
/// registered by `Activated`'s `RegisterIo`, removed only by the single
/// `Discard` interpreter ([`discard_io`]).
pub struct SessionIo {
    /// The incarnation this entry belongs to; `Discard` matches on
    /// `(id, lease)` so a late discard never touches a reused session_id.
    lease: LeaseId,
    /// Output buffer / committed offset (relay hot path; its own lock, never
    /// taken while `core` is held).
    handle: Arc<Mutex<Session>>,
    parked_tcp: Option<ParkedTcp>,
    /// Ask the currently-relaying connection to yield (D-2 preemption).
    preempt: Arc<Notify>,
    /// Signalled (out-of-lock) whenever `parked_tcp` becomes `Some`.
    reparked: Arc<Notify>,
}

/// The single lock's contents: the pure aggregate plus the sockets it
/// refers to.
pub(crate) struct ServeCore {
    agg: ServeAggregate,
    io: BTreeMap<SessionKey, SessionIo>,
}

/// Resources a shell call hands to the in-lock interpreter (the reducer's
/// events are plain data and cannot carry them).
#[derive(Default)]
struct Staged {
    /// For `Activated` → `RegisterIo`.
    handle: Option<Arc<Mutex<Session>>>,
    /// For `Parked` → `StoreParked`. Dropped (= TCP closed) after the lock is
    /// released if the reducer did not accept the park.
    tcp: Option<ParkedTcp>,
}

/// A granted `RESUME`: the parked socket *and* that same incarnation's
/// output buffer handle, handed over together in one apply (ADR §2.2,
/// round 4 m-R4-1).
pub struct ResumeGrant {
    pub lease: LeaseId,
    pub tcp: ParkedTcp,
    pub handle: Arc<Mutex<Session>>,
    pub preempt: Arc<Notify>,
}

pub enum ResumeDecision {
    Granted(ResumeGrant),
    /// The session is actively relaying; the caller should notify
    /// `preempt`, wait (bounded) on `reparked`, then re-send the request
    /// exactly once (ADR Q9 default).
    Preempt { preempt: Arc<Notify>, reparked: Arc<Notify> },
    Rejected,
}

/// What the in-lock interpreter produced, for the caller to act on after
/// releasing the lock.
#[derive(Default)]
struct InLockOutcome {
    attach: Vec<AttachEffect>,
    discarded: Vec<SessionKey>,
    wake: Vec<Arc<Notify>>,
    /// `preempt` of the `SessionIo` registered by this apply (`Activated`).
    registered: Option<Arc<Notify>>,
    resume: Option<ResumeDecision>,
    /// `ResumeGranted` whose `SessionIo` unexpectedly had no parked socket,
    /// or `StoreParked` that found no matching `SessionIo` to store the
    /// socket in: the caller discards that incarnation with a new apply
    /// (ADR §2.4-3: never nest an apply inside interpretation).
    orphaned: Option<(SessionKey, LeaseId)>,
}

/// What `activate()` hands back on success.
pub struct Activation {
    pub tcp: TcpStream,
    pub lease: EstablishedLease,
    pub preempt: Arc<Notify>,
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
/// little late by the Drop fallback" instead. (Since Step 2a, releasing the
/// slot also discards the session's index entry and parked socket in the
/// same transition.)
pub struct EstablishedLease {
    runtime: Arc<AttachRuntime>,
    lease: LeaseId,
    armed: bool,
}

impl EstablishedLease {
    fn new(runtime: Arc<AttachRuntime>, lease: LeaseId) -> Self {
        Self { runtime, lease, armed: true }
    }

    /// The incarnation token every later fact about this session carries.
    pub fn id(&self) -> LeaseId {
        self.lease
    }

    /// The target TCP connection died for good — release the slot now.
    pub async fn release(mut self) {
        if std::mem::take(&mut self.armed) {
            self.runtime.relay_ended(self.lease).await;
        }
    }

    /// The data stream died but the target TCP is still alive and is about
    /// to be parked for a possible `RESUME` — the slot must stay
    /// `Established`, so consume this guard without releasing.
    pub fn keep(mut self) {
        self.armed = false;
    }
}

impl Drop for EstablishedLease {
    /// Best-effort fallback only: never panics (a panic here, while already
    /// unwinding from the panic that skipped `release()`/`keep()`, would
    /// abort the process — exactly the failure mode this guard exists to
    /// avoid) and never awaits directly (`relay_ended` is async; `Drop`
    /// isn't). If no tokio runtime is reachable (e.g. this guard outlives
    /// the runtime during process shutdown), the slot simply can't be
    /// released here — logged so it's visible.
    fn drop(&mut self) {
        if !std::mem::take(&mut self.armed) {
            return;
        }
        let lease = self.lease;
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
    core: Mutex<ServeCore>,
    leases: Mutex<HashMap<LeaseId, LeaseResource>>,
    /// Every `hello()` caller still waiting on a key. A list, not a single
    /// slot: a retransmitted `ATTACH_HELLO` for the same key (a second
    /// connection racing the first) must not silently drop the first
    /// caller's sender (PIPE-06) — every one of them gets the outcome.
    waiters: Mutex<HashMap<AttachKey, Vec<oneshot::Sender<HelloOutcome>>>>,
    next_target_id: AtomicU64,
    target: SocketAddr,
    /// Epoch of this shell's `Millis` (ADR §2.2). `tokio::time::Instant`, so
    /// it follows the paused clock in `start_paused` tests.
    epoch: tokio::time::Instant,
    /// ADR §6 Step 11: in test builds, every apply's event/effects, checked
    /// against the serve trace invariants on each record and at drop. A
    /// zero-sized `()` in non-test builds (see [`TraceHook`]).
    #[cfg_attr(not(test), allow(dead_code))]
    trace: TraceHook,
    /// Test builds: opt-in pause points inside `start_connect` for forcing
    /// specific interleavings deterministically (review of #206, F1/F2).
    /// Non-test builds: `()`.
    #[cfg_attr(not(test), allow(dead_code))]
    hooks: TestHooks,
}

/// Test builds: the Step 11 trace recorder. Non-test builds: `()` (nothing is
/// recorded; production behavior is unchanged). A type alias rather than a
/// `#[cfg(test)]` field so the struct literal needs no cfg'd field either.
#[cfg(test)]
type TraceHook = super::trace_invariants::ServeTraceRecorder;
#[cfg(not(test))]
type TraceHook = ();

#[cfg(test)]
type TestHooks = start_connect_tests::Hooks;
#[cfg(not(test))]
type TestHooks = ();

impl AttachRuntime {
    /// `max_sessions`: `--max-sessions` (Phase S-4b) — the admission cap
    /// [`Self::hello`] enforces atomically (ADR Step 2b), and the table-side
    /// cap `Activated` checks (evict the oldest parked session, else register
    /// unresumable — no longer reachable once admission is atomic; Step 2c).
    pub fn new(target: SocketAddr, max_sessions: usize) -> Arc<Self> {
        Arc::new(Self {
            core: Mutex::new(ServeCore { agg: ServeAggregate::new(max_sessions), io: BTreeMap::new() }),
            leases: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
            next_target_id: AtomicU64::new(0),
            target,
            epoch: tokio::time::Instant::now(),
            trace: Default::default(),
            hooks: Default::default(),
        })
    }

    /// The only place this shell reads the clock (ADR §2.2); called only
    /// from inside [`Self::apply_with`], after the lock is acquired, so the
    /// `now`s entering the aggregate are monotone.
    fn stamp(&self) -> Millis {
        Millis(u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    /// Acquire the aggregate lock, stamp `now`, apply, interpret the in-lock
    /// effects, release the lock, then run the out-of-lock wake-ups. The
    /// caller runs the returned `attach` effects (`execute_effects`, or
    /// `activate`'s own interpreter).
    async fn apply_with(&self, make: impl FnOnce(Millis) -> ServeEvent, mut staged: Staged) -> InLockOutcome {
        let mut out = {
            let mut core = self.core.lock().await;
            let now = self.stamp();
            let event = make(now);
            let ServeCore { agg, io } = &mut *core;
            #[cfg(test)]
            let observed = super::trace_invariants::observe_event(agg, &event);
            let effects = agg.apply(event);
            #[cfg(test)]
            self.trace.record(observed, &effects);
            interpret_in_lock(io, effects, &mut staged)
        };
        // Lock released. Anything the reducer did not take (e.g. a socket
        // whose park was stale) is dropped here — closing it.
        drop(staged);
        for notify in std::mem::take(&mut out.wake) {
            notify.notify_waiters();
        }
        out
    }

    /// Applies an event that only ever yields `AttachEffect`s and executes them.
    async fn apply_and_execute(self: &Arc<Self>, event: ServeEvent) -> InLockOutcome {
        let mut out = self.apply_with(move |_| event, Staged::default()).await;
        self.execute_effects(std::mem::take(&mut out.attach)).await;
        out
    }

    /// Whether the arbiter currently holds no session at all — used for the
    /// `--max-idle-lifetime` monitor, mirroring `active.load(..)`'s old role
    /// (self-terminate only once nothing is attached/attaching/established).
    pub async fn is_vacant(&self) -> bool {
        self.core.lock().await.agg.arbiter().session_count() == 0
    }

    /// How many sessions currently hold a slot (connecting, pending, or
    /// established/parked/unresumable). A read-only query: admission no
    /// longer decides from it (that would be the pre-Step-2b check-then-act);
    /// [`Self::hello`] decides inside the same apply that claims the slot.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn session_count(&self) -> usize {
        self.core.lock().await.agg.arbiter().session_count()
    }

    /// Whether `session_id` already holds a slot (of any kind). A read-only
    /// query (tests); admission makes the same distinction inside its apply.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn has_session(&self, session_id: isekai_protocol::SessionId) -> bool {
        self.core.lock().await.agg.arbiter().has_session(session_id)
    }

    /// The lease currently backing `session_id`'s `Established` slot, if any.
    /// A read-only query; `RESUME` no longer uses it to decide anything (the
    /// decision is `ResumeRequested`, resolved atomically in one apply).
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn established_lease_for(&self, session_id: isekai_protocol::SessionId) -> Option<LeaseId> {
        match self.core.lock().await.agg.arbiter().state_for(session_id) {
            Some(AttachState::Established { lease, .. }) => Some(*lease),
            Some(AttachState::Connecting { .. })
            | Some(AttachState::PendingActivation { .. })
            | Some(AttachState::ClosingForSupersede { .. })
            | None => None,
        }
    }

    /// Entry point for a data-stream `ATTACH_HELLO` (after its proof has been
    /// verified): registers a waiter for `key`, applies `AdmitRequested`,
    /// executes whatever effects come back immediately, then waits (possibly
    /// across further effects executed by *other* tasks later) for the
    /// eventual `AttachReadyV2`/reject outcome.
    ///
    /// Admission (`--max-sessions`, Epic N-5) happens in that **same apply**
    /// (docs/adr/0019-functional-core-effects.md Step 2b): a session_id that already
    /// holds a slot (retransmit/reattach/supersede) passes straight through;
    /// a brand-new one claims a slot if fewer than `max_sessions` are held,
    /// else evicts the oldest parked session first, else is rejected with
    /// `BusyOtherSession` without claiming anything. This bounds concurrent
    /// target connects/handshakes *before* any target connect starts, and —
    /// unlike the former `engine/mod.rs::admit_new_session`, which checked
    /// `session_count()` under the lock, dropped it, then called this — two
    /// concurrent new sessions can no longer both pass the check (max+1).
    ///
    /// The wait is bounded by `HELLO_OUTCOME_TIMEOUT` (PIPE-06); on expiry
    /// the now-dead sender is pruned from `waiters`.
    pub async fn hello(self: &Arc<Self>, key: AttachKey) -> HelloOutcome {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.entry(key).or_default().push(tx);
        self.apply_and_execute(ServeEvent::AdmitRequested { key }).await;
        self.wait_hello_outcome(key, rx).await
    }

    async fn wait_hello_outcome(self: &Arc<Self>, key: AttachKey, rx: oneshot::Receiver<HelloOutcome>) -> HelloOutcome {
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

    /// Applies `AttachActivate`; on success (the activation matched the
    /// current `PendingActivation` lease) the session is registered in the
    /// resume index **in the same apply** (with `handle` as its output
    /// buffer; evicting the oldest parked session first if the table is
    /// full, or registering it unresumable if nothing can be evicted), and
    /// this returns the target `TcpStream` the connection task should now
    /// relay through, paired with an [`EstablishedLease`] minted at exactly
    /// this transition and the session's `preempt` signal.
    #[deny(clippy::wildcard_enum_match_arm)]
    pub async fn activate(
        self: &Arc<Self>,
        key: AttachKey,
        attach_token: AttachToken,
        negotiated_grace_secs: Option<u32>,
        handle: Arc<Mutex<Session>>,
    ) -> Option<Activation> {
        let out = self
            .apply_with(
                move |_| ServeEvent::Activated { key, attach_token, negotiated_grace_secs },
                Staged { handle: Some(handle), tcp: None },
            )
            .await;
        let mut started = None;
        // Effect interpreter: every `AttachEffect` variant is listed explicitly
        // (docs/adr/0019-functional-core-effects.md §3-8, Step 7a) so adding a new effect
        // forces a decision here instead of being silently dropped. (The
        // in-lock `ServeEffect`s — `RegisterIo`, an eviction's `Discard` — were
        // already interpreted by `interpret_in_lock`.)
        for effect in out.attach {
            match effect {
                AttachEffect::StartRelay { lease, .. } => {
                    if started.is_none() {
                        let resource = self.leases.lock().await.remove(&lease);
                        match resource {
                            Some(LeaseResource::PendingTarget { tcp, timer }) => {
                                if let Some(timer) = timer {
                                    timer.abort();
                                }
                                let preempt = out.registered.clone().unwrap_or_else(|| Arc::new(Notify::new()));
                                started =
                                    Some(Activation { tcp, lease: EstablishedLease::new(self.clone(), lease), preempt });
                            }
                            Some(LeaseResource::Connecting { task }) => {
                                // Not reachable (Activated requires PendingActivation,
                                // i.e. the connect already finished); never leave an
                                // Established slot without a guard.
                                task.abort();
                                log::warn!("attach_runtime: StartRelay for a lease still connecting; releasing it");
                                self.relay_ended(lease).await;
                            }
                            None => {
                                log::warn!("attach_runtime: StartRelay without a pending target; releasing it");
                                self.relay_ended(lease).await;
                            }
                        }
                    }
                }
                AttachEffect::ConnectTarget { .. }
                | AttachEffect::CancelLease { .. }
                | AttachEffect::SendReady { .. }
                | AttachEffect::SendReject { .. }
                | AttachEffect::SchedulePendingTimeout { .. } => {
                    // `on_activated` currently only emits `StartRelay`; reaching
                    // this arm means the reducer grew a new effect for
                    // `Activated` that this interpreter must now handle.
                    log::warn!("attach_runtime: unexpected non-StartRelay effect from Activated in activate()");
                }
            }
        }
        started
    }

    /// Mints an [`EstablishedLease`] for the lease a granted `RESUME` returned
    /// (`ResumeGrant::lease`). Callers must only call this once the slot is
    /// genuinely about to be relayed through again (right before
    /// `relay_buffered`, after every earlier repark-and-return path) — see
    /// `engine/mod.rs`'s `handle_resume_stream` for why minting this too
    /// early would let a guard dropped on a rejected/reparked `RESUME`
    /// wrongly release a slot that must stay `Established`.
    pub fn resumed_lease(self: &Arc<Self>, lease: LeaseId) -> EstablishedLease {
        EstablishedLease::new(self.clone(), lease)
    }

    pub async fn cancel(self: &Arc<Self>, key: AttachKey) {
        self.apply_and_execute(ServeEvent::CancelReceived { key }).await;
    }

    /// The connection task that reached `Established` calls this once its
    /// relay loop actually ends *for good* (target TCP died — not merely
    /// parked for a possible resume). Since Step 2a this releases the
    /// fencing slot **and** discards the session's index entry in one
    /// transition (ADR I-j).
    pub async fn relay_ended(self: &Arc<Self>, lease: LeaseId) {
        self.apply_and_execute(ServeEvent::RelayEnded { lease }).await;
    }

    /// Fact: the relay of incarnation `lease` of `id` ended without parking.
    /// Idempotent (a no-op if that incarnation is already gone).
    pub async fn relay_terminated(self: &Arc<Self>, id: SessionKey, lease: LeaseId, reason: TerminateReason) {
        self.apply_and_execute(ServeEvent::RelayTerminated { id, lease, reason }).await;
    }

    /// Fact: incarnation `lease` of `id` lost its data stream (or yielded to
    /// a preemption) and `tcp` is still alive. The reducer either accepts the
    /// park (the socket is stored and `reparked` is signalled), discards the
    /// session (`Unresumable` — fencing slot released, socket closed), or
    /// ignores a stale lease (socket closed).
    ///
    /// If the reducer accepted the park but the shell had no matching
    /// `SessionIo` to store the socket in (unreachable unless `RegisterIo`
    /// ran without a staged handle — a programming error), the index would
    /// say "parked" while no socket exists and the slot stays held until
    /// the next RESUME or the sweep window (Step 2a review H-2). Such an
    /// orphan is discarded right away with a follow-up apply, exactly like
    /// `resume_request`'s orphaned grant.
    pub async fn park(self: &Arc<Self>, id: SessionKey, lease: LeaseId, tcp: ParkedTcp) {
        let mut out =
            self.apply_with(move |now| ServeEvent::Parked { id, lease, now }, Staged { handle: None, tcp: Some(tcp) }).await;
        self.execute_effects(std::mem::take(&mut out.attach)).await;
        if let Some((orphan_id, orphan_lease)) = out.orphaned {
            self.relay_terminated(orphan_id, orphan_lease, TerminateReason::GuardDropped).await;
        }
    }

    /// Discards every parked session whose park is at least `max_parked`
    /// old (or its negotiated grace, if shorter) — judging and removing in
    /// **one** apply, so a concurrent `RESUME` can never unpark a session in
    /// between (the sweep×RESUME TOCTOU, ADR §4.1). The fencing slot of each
    /// discarded session is released in the same transition. Returns the
    /// discarded ids (for logging/tests only — nothing else to do with them).
    pub async fn sweep_expired_parked(self: &Arc<Self>, max_parked: Duration) -> Vec<SessionKey> {
        let mut out = self.apply_with(move |now| ServeEvent::Sweep { now, max_parked }, Staged::default()).await;
        self.execute_effects(std::mem::take(&mut out.attach)).await;
        out.discarded
    }

    /// `RESUME` for `id`, resolved in one apply (ADR §2.2 R3-2 / I-i).
    pub async fn resume_request(self: &Arc<Self>, id: SessionKey) -> ResumeDecision {
        let out = self.apply_and_execute(ServeEvent::ResumeRequested { id }).await;
        if let Some((orphan_id, lease)) = out.orphaned {
            self.relay_terminated(orphan_id, lease, TerminateReason::GuardDropped).await;
        }
        out.resume.unwrap_or(ResumeDecision::Rejected)
    }

    #[deny(clippy::wildcard_enum_match_arm)]
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
        let senders = self.waiters.lock().await.remove(&key).unwrap_or_default();
        for tx in senders {
            let _ = tx.send(outcome);
        }
    }

    /// Spawns the target `TcpStream::connect` and records `Connecting { task }`
    /// in `leases` **while still holding the `leases` lock across the spawn**,
    /// so a `CancelLease` effect processed immediately afterward always finds
    /// an entry to abort, and the spawned task — which itself needs that lock
    /// to store `PendingTarget` — can never run first (review 2026-09-29,
    /// PIPE-04). Previously the entry was inserted *after* `tokio::spawn`
    /// returned, so a fast connect could store `PendingTarget { tcp }` first
    /// and then have it overwritten by `Connecting`: `activate()` then found no
    /// target and the attach failed. A failed/timed-out connect also used to
    /// leave its stale `Connecting` entry behind; see [`Self::connect_failed`]
    /// for how it is now removed without opening a new leak.
    async fn start_connect(self: &Arc<Self>, lease: LeaseId) {
        let this = self.clone();
        let target_addr = self.target;
        let mut leases = self.leases.lock().await;
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
                    this.apply_and_execute(ServeEvent::TargetConnected { lease, target: target_id, attach_token })
                        .await;
                }
                Ok(Err(e)) => {
                    log::info!("attach_runtime: target connect failed for lease {lease:?}: {e}");
                    this.connect_failed(lease).await;
                }
                Err(_elapsed) => {
                    log::info!(
                        "attach_runtime: target connect timed out after {TARGET_CONNECT_TIMEOUT:?} for lease {lease:?}"
                    );
                    this.connect_failed(lease).await;
                }
            }
        });
        #[cfg(test)]
        self.hooks.after_connect_spawned().await;
        leases.insert(lease, LeaseResource::Connecting { task });
    }

    /// A target connect failed: report it to the reducer **first**, and drop
    /// the `Connecting` entry only if that report actually consumed the
    /// lease (review of #206, F1).
    ///
    /// Dropping the entry before the apply (the first version of PIPE-04)
    /// opened a leak: a higher-generation `ATTACH_HELLO` applied in between
    /// moved the session to `ClosingForSupersede` and issued `CancelLease`,
    /// whose `cancel_lease` found no entry and so never sent `LeaseStopped`;
    /// the late `TargetConnectFailed` was then stale, and the session stayed
    /// in `ClosingForSupersede` forever (no later HELLO answered, `is_vacant`
    /// never true again — unrecoverable by any client retry).
    ///
    /// `on_target_connect_failed` returns an effect exactly when it removed
    /// `Connecting { lease }` (the `SendReject{Target}` for its waiter); after
    /// that no `CancelLease` for this lease can ever be issued, so the entry
    /// is ours to drop. If it returned nothing, the session already moved on
    /// via a transition that issued `CancelLease { lease }` in the same apply
    /// (supersede or CANCEL): that `cancel_lease` owns the entry — it removes
    /// it, aborts/awaits this task and applies `LeaseStopped` — so the entry
    /// must be left in place for it to find, whichever of the two runs first.
    async fn connect_failed(self: &Arc<Self>, lease: LeaseId) {
        #[cfg(test)]
        self.hooks.before_connect_failure_applied().await;
        let mut out = self.apply_with(move |_| ServeEvent::TargetConnectFailed { lease }, Staged::default()).await;
        let consumed = !out.attach.is_empty();
        if consumed {
            self.forget_connecting(lease).await;
        }
        self.execute_effects(std::mem::take(&mut out.attach)).await;
    }

    /// Drops a finished connect's `Connecting` entry (PIPE-04) — only if it is
    /// still `Connecting`, so an entry some other path already replaced or
    /// removed is left alone.
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
                task.abort();
                let _ = task.await;
                self.apply_and_execute(ServeEvent::LeaseStopped { lease }).await;
            }
            Some(LeaseResource::PendingTarget { tcp, timer }) => {
                if let Some(timer) = timer {
                    timer.abort();
                }
                drop(tcp);
                self.apply_and_execute(ServeEvent::LeaseStopped { lease }).await;
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
            this.apply_and_execute(ServeEvent::PendingExpired { lease }).await;
        });
        match self.leases.lock().await.get_mut(&lease) {
            Some(LeaseResource::PendingTarget { timer: slot, .. }) => *slot = Some(timer),
            _ => timer.abort(),
        }
    }
}

/// **The** `Discard` interpreter (ADR §6 Step 2a): one place, idempotent,
/// matched on `(id, lease)` — a different lease means a different
/// incarnation of a reused session_id, which must not be touched (same
/// policy as `AttachArbiter::on_relay_ended` ignoring an absent lease).
/// Dropping the `SessionIo` closes its parked target TCP, if any. The
/// fencing slot was already released by the same apply's state transition.
fn discard_io(io: &mut BTreeMap<SessionKey, SessionIo>, id: SessionKey, lease: LeaseId) -> bool {
    if io.get(&id).is_some_and(|slot| slot.lease == lease) {
        drop(io.remove(&id));
        true
    } else {
        false
    }
}

/// In-lock interpreter for [`ServeEffect`] (ADR §2.4-2, §3-8): runs inside
/// `apply_with`'s critical section, touches only the socket map guarded by
/// the same lock, never awaits, never takes another lock.
#[deny(clippy::wildcard_enum_match_arm)]
fn interpret_in_lock(
    io: &mut BTreeMap<SessionKey, SessionIo>,
    effects: Vec<ServeEffect>,
    staged: &mut Staged,
) -> InLockOutcome {
    let mut out = InLockOutcome::default();
    for effect in effects {
        match effect {
            ServeEffect::Attach(attach) => out.attach.push(attach),
            ServeEffect::RegisterIo { id, lease, unresumable } => {
                if unresumable {
                    log::warn!(
                        "session table full and no parked session to evict (all sessions active): session_id={} \
                         will not be resumable if its data stream drops",
                        super::hex_lower(&id)
                    );
                }
                if let Some(handle) = staged.handle.take() {
                    let preempt = Arc::new(Notify::new());
                    io.insert(
                        id,
                        SessionIo {
                            lease,
                            handle,
                            parked_tcp: None,
                            preempt: preempt.clone(),
                            reparked: Arc::new(Notify::new()),
                        },
                    );
                    out.registered = Some(preempt);
                } else {
                    log::error!("attach_runtime: RegisterIo without a staged session handle");
                }
            }
            ServeEffect::Discard { id, lease, cause } => {
                if discard_io(io, id, lease) {
                    match cause {
                        DiscardCause::Expired => {
                            log::info!("session {} expired while parked, discarded", super::hex_lower(&id));
                        }
                        DiscardCause::Evicted => {
                            log::warn!(
                                "session table full, evicted oldest parked session {}",
                                super::hex_lower(&id)
                            );
                        }
                        DiscardCause::TcpDied => {}
                        DiscardCause::GuardDropped => {
                            log::warn!("session {} discarded by its guard's Drop fallback", super::hex_lower(&id));
                        }
                        DiscardCause::Unresumable => {
                            log::info!(
                                "session {} lost its data stream but is unresumable (registered over capacity); \
                                 discarding and releasing its slot",
                                super::hex_lower(&id)
                            );
                        }
                    }
                }
                out.discarded.push(id);
            }
            ServeEffect::StoreParked { id, lease } => {
                let accepted = match io.get_mut(&id) {
                    Some(slot) if slot.lease == lease => match staged.tcp.take() {
                        Some(tcp) => {
                            slot.parked_tcp = Some(tcp);
                            out.wake.push(slot.reparked.clone());
                            true
                        }
                        None => false,
                    },
                    Some(_) | None => false,
                };
                if !accepted {
                    log::error!(
                        "attach_runtime: StoreParked for {} found no matching SessionIo/socket; discarding the orphan",
                        super::hex_lower(&id)
                    );
                    out.orphaned = Some((id, lease));
                }
            }
            ServeEffect::ResumeGranted { id, lease } => {
                let grant = match io.get_mut(&id) {
                    Some(slot) if slot.lease == lease => slot.parked_tcp.take().map(|tcp| ResumeGrant {
                        lease,
                        tcp,
                        handle: slot.handle.clone(),
                        preempt: slot.preempt.clone(),
                    }),
                    Some(_) | None => None,
                };
                match grant {
                    Some(grant) => out.resume = Some(ResumeDecision::Granted(grant)),
                    None => {
                        log::error!("attach_runtime: ResumeGranted for {} without a parked socket", super::hex_lower(&id));
                        out.orphaned = Some((id, lease));
                        out.resume = Some(ResumeDecision::Rejected);
                    }
                }
            }
            ServeEffect::RequestPreempt { id, lease } => {
                out.resume = Some(match io.get(&id) {
                    Some(slot) if slot.lease == lease => {
                        ResumeDecision::Preempt { preempt: slot.preempt.clone(), reparked: slot.reparked.clone() }
                    }
                    Some(_) | None => ResumeDecision::Rejected,
                });
            }
            ServeEffect::ResumeRejected { .. } => out.resume = Some(ResumeDecision::Rejected),
        }
    }
    out
}

// テスト専用のフック(本番の挙動は変えない)。
#[cfg(test)]
impl AttachRuntime {
    /// Holds the aggregate lock so a test can queue other tasks behind it
    /// (tokio's `Mutex` is FIFO-fair) and release them in a known order.
    pub(crate) async fn lock_core_for_test(&self) -> tokio::sync::MutexGuard<'_, ServeCore> {
        self.core.lock().await
    }

    /// [`Self::hello`] without admission: claims a fencing slot even over
    /// `--max-sessions`. Only for reproducing the over-capacity slot that the
    /// pre-Step-2b admission race could create (→ an unresumable entry).
    pub(crate) async fn hello_bypassing_admission(self: &Arc<Self>, key: AttachKey) -> HelloOutcome {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.entry(key).or_default().push(tx);
        let attach = {
            let mut core = self.core.lock().await;
            let ServeCore { agg, io } = &mut *core;
            let effects = agg.hello_bypassing_admission(key);
            self.trace.record(super::trace_invariants::ServeTraceEvent::Other("HelloBypassingAdmission"), &effects);
            interpret_in_lock(io, effects, &mut Staged::default()).attach
        };
        self.execute_effects(attach).await;
        rx.await.unwrap_or(HelloOutcome::Reject(AttachRejectReason::Unsupported))
    }

    /// How many per-lease resources (`Connecting`/`PendingTarget`) the shell
    /// still holds (PIPE-04 regression).
    pub(crate) async fn lease_resource_count_for_test(&self) -> usize {
        self.leases.lock().await.len()
    }

    pub(crate) async fn index_contains(&self, id: &SessionKey) -> bool {
        self.core.lock().await.agg.index_entry(id).is_some()
    }

    pub(crate) async fn is_parked(&self, id: &SessionKey) -> bool {
        let core = self.core.lock().await;
        core.agg.index_entry(id).is_some_and(|e| e.parked_since.is_some())
            && core.io.get(id).is_some_and(|s| s.parked_tcp.is_some())
    }

    /// Removes `id`'s `SessionIo` behind the reducer's back, to reach the
    /// (otherwise unreachable) "StoreParked finds no SessionIo" state.
    pub(crate) async fn remove_io_for_test(&self, id: &SessionKey) {
        self.core.lock().await.io.remove(id);
    }

    /// One consistent read (under the aggregate lock) of everything the
    /// Step 10-1 differential test compares against the pure model: the
    /// arbiter state and index entry of each of `ids`, the slot count, and
    /// the shell's own socket map (`(id, lease, has parked socket)` for every
    /// `SessionIo`, including any stray one not in `ids`).
    pub(crate) async fn snapshot_for_test(&self, ids: &[SessionKey]) -> ShellSnapshot {
        let core = self.core.lock().await;
        ShellSnapshot {
            states: ids
                .iter()
                .map(|id| core.agg.arbiter().state_for(isekai_protocol::SessionId::from_bytes(*id)).cloned())
                .collect(),
            index: ids.iter().map(|id| core.agg.index_entry(id).copied()).collect(),
            session_count: core.agg.arbiter().session_count(),
            io: core.io.iter().map(|(id, slot)| (*id, slot.lease, slot.parked_tcp.is_some())).collect(),
        }
    }
}

/// See [`AttachRuntime::snapshot_for_test`].
#[cfg(test)]
pub(crate) struct ShellSnapshot {
    pub(crate) states: Vec<Option<AttachState>>,
    pub(crate) index: Vec<Option<super::serve_fsm::IndexEntry>>,
    pub(crate) session_count: usize,
    pub(crate) io: Vec<(SessionKey, LeaseId, bool)>,
}

// serverとclientの時間定数の関係テスト(ADR_DETERMINISTIC_NETWORK_SIMULATION_L1.md §4.5)。
// このファイルとengine/mod.rsのprivate定数が`pub(crate)`化なしで見えるよう、子モジュールにしてある。
#[cfg(test)]
mod timing_relations;

#[cfg(test)]
mod start_connect_tests {
    use super::*;
    use isekai_protocol::attach::{AttemptId, ConnectionGeneration, ATTEMPT_ID_LEN};
    use std::sync::atomic::AtomicBool;
    use tokio::sync::Semaphore;

    /// Opt-in pause points in `start_connect` (all off by default, so every
    /// other test sees production ordering).
    pub(crate) struct Hooks {
        /// Let the freshly spawned connect task run (to completion of its
        /// connect and its own `leases` access, if it can) before
        /// `start_connect` records `Connecting`.
        yield_after_spawn: AtomicBool,
        /// Park a failed connect right before it reports
        /// `TargetConnectFailed`: `reached` gets a permit, then it waits for
        /// one on `proceed`.
        gate_connect_failure: AtomicBool,
        reached: Semaphore,
        proceed: Semaphore,
    }

    impl Default for Hooks {
        fn default() -> Self {
            Self {
                yield_after_spawn: AtomicBool::new(false),
                gate_connect_failure: AtomicBool::new(false),
                reached: Semaphore::new(0),
                proceed: Semaphore::new(0),
            }
        }
    }

    impl Hooks {
        pub(super) async fn after_connect_spawned(&self) {
            if self.yield_after_spawn.load(Ordering::SeqCst) {
                for _ in 0..64 {
                    tokio::task::yield_now().await;
                }
            }
        }

        pub(super) async fn before_connect_failure_applied(&self) {
            if self.gate_connect_failure.load(Ordering::SeqCst) {
                self.reached.add_permits(1);
                self.proceed.acquire().await.expect("never closed").forget();
            }
        }
    }

    fn key(session: u8) -> AttachKey {
        key_at(session, 1)
    }

    fn key_at(session: u8, generation: u64) -> AttachKey {
        AttachKey {
            session_id: isekai_protocol::SessionId::from_bytes([session; 16]),
            generation: ConnectionGeneration::new(generation),
            attempt_id: AttemptId::from_bytes([1u8; ATTEMPT_ID_LEN]),
        }
    }

    async fn settle() {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
    }

    /// Review of #206 F1: a higher-generation `ATTACH_HELLO` applied while a
    /// failed connect has not yet reported `TargetConnectFailed` must still
    /// supersede it. With the entry dropped before the report, `CancelLease`
    /// found nothing, no `LeaseStopped` was sent, and the session stayed in
    /// `ClosingForSupersede` forever: the new HELLO was never answered.
    #[tokio::test]
    async fn supersede_racing_a_failed_connect_still_resolves_the_newer_hello() {
        let runtime = AttachRuntime::new(closed_target().await, 16);
        runtime.hooks.gate_connect_failure.store(true, Ordering::SeqCst);

        let first = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.hello(key_at(3, 1)).await }
        });
        // The generation-1 connect has failed and is parked just before it
        // reports that to the reducer.
        runtime.hooks.reached.acquire().await.unwrap().forget();
        // The generation-2 connect (if the supersede goes through) must not
        // be parked too.
        runtime.hooks.gate_connect_failure.store(false, Ordering::SeqCst);

        let newer = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.hello(key_at(3, 2)).await }
        });
        // Let the newer HELLO apply and execute its `CancelLease` while the
        // old failure is still unreported, then let the old task go on.
        settle().await;
        runtime.hooks.proceed.add_permits(1);

        let outcome = tokio::time::timeout(Duration::from_secs(10), newer)
            .await
            .expect("the newer HELLO must be answered, not left behind a stuck ClosingForSupersede")
            .unwrap();
        assert!(matches!(outcome, HelloOutcome::Reject(AttachRejectReason::Target)), "its own connect fails too");
        assert!(runtime.is_vacant().await, "no slot may stay held");
        assert_eq!(runtime.lease_resource_count_for_test().await, 0);
        first.abort();
    }

    /// A target address nothing listens on (bound, then closed).
    async fn closed_target() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        addr
    }

    /// PIPE-04 regression: a failed target connect used to leave its stale
    /// `Connecting { task }` entry in `leases` forever.
    #[tokio::test]
    async fn failed_target_connect_rejects_and_leaves_no_lease_bookkeeping() {
        let runtime = AttachRuntime::new(closed_target().await, 16);
        let outcome = runtime.hello(key(1)).await;
        assert!(matches!(outcome, HelloOutcome::Reject(_)), "connect to a closed port must reject the attach");
        assert_eq!(runtime.lease_resource_count_for_test().await, 0, "no Connecting entry may be left behind");
        assert!(runtime.is_vacant().await, "the fencing slot must be released");
    }

    /// PIPE-06 regression: a retransmitted `ATTACH_HELLO` for the same key
    /// (two connections racing) used to replace the first caller's sender,
    /// so that caller got `Unsupported`. Both now get the same outcome.
    #[tokio::test]
    async fn retransmitted_hello_for_the_same_key_resolves_every_waiter() {
        // On the current-thread test runtime the spawned connect task cannot
        // run before `join!` has polled both hellos, so both are registered
        // (the second as a same-attempt retransmit while `Connecting`)
        // before `TargetConnected` resolves the key.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = listener.accept().await {
                held.push(s);
            }
        });
        let runtime = AttachRuntime::new(target, 16);
        let k = key(9);
        let (a, b) = tokio::join!(runtime.hello(k), runtime.hello(k));
        match (a, b) {
            (HelloOutcome::Ready { attach_token: ta }, HelloOutcome::Ready { attach_token: tb }) => {
                assert!(ta == tb, "both waiters of one key must get the same attach token");
            }
            (HelloOutcome::Reject(ra), HelloOutcome::Reject(rb)) => {
                panic!("both hellos rejected: {ra:?} / {rb:?}");
            }
            (HelloOutcome::Ready { .. }, HelloOutcome::Reject(r)) | (HelloOutcome::Reject(r), HelloOutcome::Ready { .. }) => {
                panic!("one waiter of a retransmitted key was dropped with {r:?}");
            }
        }
    }

    /// PIPE-04 regression: a fast connect must always reach `PendingTarget`
    /// (the old insert-after-spawn order could overwrite it with
    /// `Connecting`, so `activate()` found no target). The hook lets the
    /// spawned connect task run to its own `leases` insert before
    /// `start_connect` records `Connecting` — the losing interleaving, made
    /// deterministic (review of #206, F2). With `Connecting` recorded under
    /// the same lock hold as the spawn, the task just waits for it.
    #[tokio::test]
    async fn fast_target_connect_is_always_activatable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = listener.accept().await {
                held.push(s);
            }
        });
        let runtime = AttachRuntime::new(target, 64);
        runtime.hooks.yield_after_spawn.store(true, Ordering::SeqCst);
        for i in 0..8u8 {
            let k = key(i);
            let attach_token = match runtime.hello(k).await {
                HelloOutcome::Ready { attach_token } => attach_token,
                HelloOutcome::Reject(reason) => panic!("hello {i} rejected: {reason:?}"),
            };
            let handle = Arc::new(Mutex::new(Session::new(1024)));
            let activation = runtime.activate(k, attach_token, Some(3600), handle).await;
            assert!(activation.is_some(), "attempt {i}: activate must find the connected target");
        }
    }
}
