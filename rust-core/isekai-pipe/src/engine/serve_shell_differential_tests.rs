//! Step 10-1 differential test (`ADR_FUNCTIONAL_CORE_EFFECTS.md` §6 Step 10):
//! the **real** `isekai-pipe serve` shell (`AttachRuntime` + `engine/mod.rs`'s
//! `finish_or_park_session` / `SessionTableEntryGuard`, with real target TCP
//! connections to a local listener) against the **pure** reference model
//! ([`ServeAggregate`] plus the shell's documented effect contract, [`Model`]).
//!
//! # Why
//!
//! `serve_fsm.rs`'s proptests prove the reducer keeps §4.1's I-a..I-k. They do
//! not prove the shell feeds the reducer the right events, interprets its
//! effects faithfully (in-lock `Discard`/`StoreParked`/`ResumeGranted`, the
//! out-of-lock `AttachEffect`s), or actually *closes* the target TCP of every
//! discarded incarnation. The defect history (Step 9 report, rank 1: the
//! fencing-slot ↔ session-table desync class, 6+ fixes) is exactly that kind
//! of bug: two state holders, and every discard path must remember to release
//! the slot *and* the socket.
//!
//! # What one case does
//!
//! A random sequence of operations (`Op`: HELLO / bypass HELLO / ACTIVATE /
//! data-stream death → park / target-TCP death / task panic (both RAII
//! backstops fire) / RESUME (grant, grant+OffsetGone re-park, preempt with the
//! relay yielding or ignoring it) / CANCEL / sweep / stale facts carrying a
//! dead incarnation's lease) runs against the shell and, step by step, against
//! the model. After **every** step it asserts:
//!
//! - **differential**: the operation's observable outcome (HELLO ready/reject
//!   + token, activation lease, RESUME decision + lease, the set of swept
//!   sessions) and the full per-session state (arbiter `AttachState` including
//!   lease/token/target handle, index entry, slot count) are identical;
//! - **shell invariants** on the shell's own aggregate: I-c (Established ⇔
//!   index entry with the same lease), I-g, the table-side capacity, I-k (slot
//!   count ≤ `--max-sessions`, whenever admission was not bypassed);
//! - **socket map ⇔ index**: every `SessionIo` belongs to the index entry of
//!   the same lease, and holds a parked socket iff the index says parked;
//! - **the target TCP itself**: the target-side end of every lease's
//!   connection is open iff the model still holds that incarnation (connected
//!   and awaiting activation, or indexed), and is observed **closed** (EOF)
//!   after every discard path — Expired, Evicted, TcpDied, GuardDropped,
//!   Unresumable, superseded/cancelled pending leases, and a stale park's
//!   socket. This is what `serve_fsm.rs` cannot check (ADR §4.1 "限界").
//!   Together with the teardown below it is "every discard path releases the
//!   slot exactly once": the lease leaves the arbiter, the index and the
//!   socket map in the same step, its TCP is closed, and later facts carrying
//!   it (the second RAII backstop, `StaleFacts`) change nothing.
//! - RESUME also checks that the granted output buffer / preempt signal are
//!   the very `Arc`s registered for that incarnation (I-i at the shell level),
//!   and that an accepted re-park signals `reparked`.
//!
//! Each case ends with a teardown (every relay's target dies, a sweep with a
//! zero window, every pending attempt cancelled) that must leave **no** slot,
//! index entry, socket-map entry or open target TCP — a server-side leak that
//! client retries can never recover from (`.claude/rules/always-connects.md`).
//!
//! # Determinism (why this is not flaky)
//!
//! Real time is not paused: the shell's target connects are real loopback
//! TCP, and tokio's paused clock auto-advances whenever the runtime would
//! block on I/O, which would fire `TARGET_CONNECT_TIMEOUT` /
//! `PENDING_ACTIVATION_TIMEOUT` spuriously. Instead every clock-dependent
//! outcome is made independent of the actual clock value:
//!
//! - Operations run **sequentially** on a `current_thread` runtime; each one
//!   awaits the shell to completion (HELLO returns only after the spawned
//!   connect task applied `TargetConnected`). The only fire-and-forget tasks
//!   are the two RAII backstops of `Op::Panic`, which the harness waits for.
//!   Lease ids, target handle ids and the order of applies are therefore the
//!   same in the shell and the model.
//! - Sweep deadlines are only ever `0` or ≥ 3600 s (`max_parked` ∈ {0, 3600 s},
//!   negotiated grace ∈ {none, 0, 3600 s}), so "expired" never depends on how
//!   many milliseconds actually passed.
//! - Eviction picks the oldest park by `(parked_since, id)`; the harness sleeps
//!   [`PARK_SPACING`] (3 ms) before every park, so the shell's millisecond
//!   stamps are strictly increasing, exactly like the model's logical clock.
//! - The 5 s pending-activation timer is never reached: a case stops issuing
//!   operations after [`CASE_BUDGET`] (3.5 s; a case normally takes a few ms).
//! - "Closed" is polled (up to 2 s) because a FIN crosses the loopback
//!   asynchronously; "open" is a single non-blocking read, which can only
//!   err towards passing. Every shell call is bounded by [`OP_TIMEOUT`] so a
//!   hang fails with a message instead of stalling CI.
//!
//! # Limits
//!
//! Operations are sequential, so interleavings of concurrent shell calls at
//! await points are not explored here: those are closed structurally by the
//! single lock + single apply (Step 2a/2b) and pinned by
//! `sweep_resume_race_tests.rs` / `admission_race_tests.rs`. RESUME's preempt
//! loop is mirrored from `handle_resume_stream` (which needs real QUIC
//! streams), not called; the 2 s `PREEMPT_WAIT_TIMEOUT` wait is skipped when
//! the relay "ignores" the preempt.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Read as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use isekai_protocol::attach::{
    AttachKey, AttachRejectReason, AttachToken, AttemptId, ConnectionGeneration, ATTACH_TOKEN_LEN, ATTEMPT_ID_LEN,
};
use isekai_protocol::{Millis, SessionId as WireSessionId};
use proptest::prelude::*;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify};

use super::attach_arbiter::{AttachEffect, AttachState, LeaseId, TargetHandleId};
use super::attach_runtime::{AttachRuntime, EstablishedLease, HelloOutcome, ResumeDecision};
use super::resume::Session;
use super::serve_fsm::{DiscardCause, IndexEntry, ServeAggregate, ServeEffect, ServeEvent, SessionKey, TerminateReason};
use super::{finish_or_park_session, RelayOutcome, SessionTableEntryGuard};

const SESSIONS: u8 = 3;
/// Spacing before every park so the shell's `Millis` stamps strictly increase.
const PARK_SPACING: Duration = Duration::from_millis(3);
/// Upper bound for one shell call (a hang is a failure, not a CI stall).
const OP_TIMEOUT: Duration = Duration::from_secs(10);
/// Stop issuing operations well before the shell's 5 s pending-activation timer.
const CASE_BUDGET: Duration = Duration::from_millis(3500);
/// "Never expires within a case" window / grace.
const LONG_WINDOW: Duration = Duration::from_secs(3600);

fn id(s: u8) -> SessionKey {
    [s; 16]
}

fn key(s: u8, g: u8, at: u8) -> AttachKey {
    AttachKey {
        session_id: WireSessionId::from_bytes(id(s)),
        generation: ConnectionGeneration::new(u64::from(g)),
        attempt_id: AttemptId::from_bytes([at; ATTEMPT_ID_LEN]),
    }
}

/// Token the model uses when the shell produced none (only on a divergence,
/// which the outcome comparison then reports).
fn dummy_token() -> AttachToken {
    AttachToken::new([0; ATTACH_TOKEN_LEN])
}

/// A token the shell's `OsRng` never produces in practice (2^-128).
fn wrong_token() -> AttachToken {
    AttachToken::new([0xEE; ATTACH_TOKEN_LEN])
}

#[derive(Debug, Clone, Copy)]
enum Grace {
    Unset,
    Zero,
    Long,
}

impl Grace {
    fn secs(self) -> Option<u32> {
        match self {
            Grace::Unset => None,
            Grace::Zero => Some(0),
            Grace::Long => Some(3600),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Op {
    /// `AttachRuntime::hello` (admission + `HelloReceived`, Step 2b).
    Hello { s: u8, g: u8, at: u8 },
    /// HELLO bypassing admission (the over-capacity slot only the pre-2b race
    /// could create → unresumable registration). Disables the I-k check.
    HelloBypass { s: u8, g: u8 },
    /// `AttachRuntime::activate` with the last `Ready` key/token of `s`.
    Activate { s: u8, grace: Grace, wrong_token: bool },
    /// `finish_or_park_session(DataStreamDied)` for the relay holding `s`.
    DataStreamDied { s: u8 },
    /// `finish_or_park_session(TcpDied)` for the relay holding `s`.
    TcpDied { s: u8 },
    /// The relay task holding `s` dies: `EstablishedLease` and
    /// `SessionTableEntryGuard` both drop armed (two fire-and-forget backstops,
    /// in either order: whichever lands first discards, the other is a no-op).
    Panic { s: u8, guard_first: bool },
    /// `handle_resume_stream`'s decision loop for `s`.
    Resume { s: u8, relay_yields: bool, offset_gone: bool },
    /// `AttachRuntime::cancel` (the current pending key, or an arbitrary one).
    Cancel { s: u8, g: u8, at: u8, current: bool },
    /// `sweep_expired_parked` with a zero or a 3600 s window.
    Sweep { expire: bool },
    /// `relay_terminated` / `relay_ended` / `park` carrying a dead incarnation's lease.
    StaleFacts { s: u8 },
}

fn op_strategy(allow_bypass: bool) -> impl Strategy<Value = Op> {
    let s = || 0u8..SESSIONS;
    let grace = || prop_oneof![Just(Grace::Unset), Just(Grace::Zero), Just(Grace::Long)];
    prop_oneof![
        5 => (s(), 0u8..3, 0u8..2).prop_map(|(s, g, at)| Op::Hello { s, g, at }),
        // Without bypass, the same weight goes to the real admission entry point.
        3 => (s(), 0u8..3).prop_map(move |(s, g)| if allow_bypass {
            Op::HelloBypass { s, g }
        } else {
            Op::Hello { s, g, at: 0 }
        }),
        6 => (s(), grace(), proptest::bool::weighted(0.1))
            .prop_map(|(s, grace, wrong_token)| Op::Activate { s, grace, wrong_token }),
        4 => s().prop_map(|s| Op::DataStreamDied { s }),
        1 => s().prop_map(|s| Op::TcpDied { s }),
        1 => (s(), any::<bool>()).prop_map(|(s, guard_first)| Op::Panic { s, guard_first }),
        4 => (s(), any::<bool>(), proptest::bool::weighted(0.2))
            .prop_map(|(s, relay_yields, offset_gone)| Op::Resume { s, relay_yields, offset_gone }),
        1 => (s(), 0u8..3, 0u8..2, any::<bool>()).prop_map(|(s, g, at, current)| Op::Cancel { s, g, at, current }),
        2 => any::<bool>().prop_map(|expire| Op::Sweep { expire }),
        2 => s().prop_map(|s| Op::StaleFacts { s }),
    ]
}

/// `(max_sessions, ops)`. `max_sessions` is skewed towards 1 so capacity
/// (admission eviction / busy rejection / unresumable) is actually reached.
fn case_strategy() -> impl Strategy<Value = (usize, Vec<Op>)> {
    (prop_oneof![2 => Just(1usize), 1 => Just(2usize), 1 => Just(3usize)], proptest::bool::weighted(0.4))
        .prop_flat_map(|(max_sessions, bypass)| {
            (Just(max_sessions), proptest::collection::vec(op_strategy(bypass), 0..40))
        })
}

// ---------------------------------------------------------------------------
// Reference model: the pure aggregate + the shell's effect contract.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeKind {
    Granted(LeaseId),
    Preempt(LeaseId),
    Rejected,
}

/// What the model says one shell call should have produced.
#[derive(Default)]
struct Step {
    /// The key whose `hello()` waiter the shell resolves (first resolution wins,
    /// like `AttachRuntime::resolve_waiter`).
    hello_key: Option<AttachKey>,
    hello: Option<Result<AttachToken, AttachRejectReason>>,
    connects: Vec<LeaseId>,
    started: Option<LeaseId>,
    discarded: Vec<(SessionKey, LeaseId, DiscardCause)>,
    resume: Option<ResumeKind>,
}

impl Step {
    fn for_hello(key: AttachKey) -> Self {
        Self { hello_key: Some(key), ..Self::default() }
    }

    fn resolve(&mut self, key: AttachKey, outcome: Result<AttachToken, AttachRejectReason>) {
        if self.hello_key == Some(key) && self.hello.is_none() {
            self.hello = Some(outcome);
        }
    }
}

struct Model {
    agg: ServeAggregate,
    /// Mirror of `AttachRuntime::leases`: `(lease, connected)` — `false` =
    /// `Connecting`, `true` = `PendingTarget` (a live target TCP the shell holds).
    leases: Vec<(LeaseId, bool)>,
    /// Mirror of `AttachRuntime::next_target_id`.
    next_target: u64,
    /// Logical clock: strictly increasing, like the shell's spaced stamps.
    now: u64,
    coverage: BTreeMap<String, usize>,
}

impl Model {
    fn new(max_sessions: usize) -> Self {
        Self { agg: ServeAggregate::new(max_sessions), leases: Vec::new(), next_target: 0, now: 0, coverage: BTreeMap::new() }
    }

    fn tick(&mut self) -> Millis {
        self.now += 1;
        Millis(self.now)
    }

    fn bump(&mut self, what: &str) {
        *self.coverage.entry(what.to_owned()).or_default() += 1;
    }

    fn apply(&mut self, event: ServeEvent, token: AttachToken, step: &mut Step) {
        let effects = self.agg.apply(event);
        self.process(effects, token, step);
    }

    /// Interprets `effects` like `AttachRuntime::execute_effects`: nested
    /// applies run depth-first inline; a target connect completes in a
    /// spawned task, i.e. after the current effect list (FIFO).
    fn process(&mut self, effects: Vec<ServeEffect>, token: AttachToken, step: &mut Step) {
        let mut connecting = VecDeque::new();
        self.interpret(effects, token, step, &mut connecting);
        while let Some(lease) = connecting.pop_front() {
            // `start_connect`'s task: aborted if the lease was cancelled meanwhile.
            let Some(slot) = self.leases.iter_mut().find(|(l, _)| *l == lease) else { continue };
            slot.1 = true;
            let target = TargetHandleId(self.next_target);
            self.next_target += 1;
            let effects = self.agg.apply(ServeEvent::TargetConnected { lease, target, attach_token: token });
            self.interpret(effects, token, step, &mut connecting);
        }
    }

    fn release_lease(&mut self, lease: LeaseId, token: AttachToken, step: &mut Step, connecting: &mut VecDeque<LeaseId>) {
        let effects = self.agg.apply(ServeEvent::RelayEnded { lease });
        self.interpret(effects, token, step, connecting);
    }

    fn interpret(
        &mut self,
        effects: Vec<ServeEffect>,
        token: AttachToken,
        step: &mut Step,
        connecting: &mut VecDeque<LeaseId>,
    ) {
        for effect in effects {
            match effect {
                ServeEffect::Attach(attach) => match attach {
                    AttachEffect::ConnectTarget { lease } => {
                        self.leases.push((lease, false));
                        step.connects.push(lease);
                        connecting.push_back(lease);
                    }
                    AttachEffect::CancelLease { lease } => {
                        // `cancel_lease`: only a lease with a resource stops.
                        if let Some(pos) = self.leases.iter().position(|(l, _)| *l == lease) {
                            self.leases.remove(pos);
                            self.bump("lease cancelled");
                            let effects = self.agg.apply(ServeEvent::LeaseStopped { lease });
                            self.interpret(effects, token, step, connecting);
                        }
                    }
                    AttachEffect::SendReady { key, attach_token } => step.resolve(key, Ok(attach_token)),
                    AttachEffect::SendReject { key, reason } => step.resolve(key, Err(reason)),
                    // Never fires within a case (CASE_BUDGET < PENDING_ACTIVATION_TIMEOUT).
                    AttachEffect::SchedulePendingTimeout { .. } => {}
                    // `activate()`'s interpreter.
                    AttachEffect::StartRelay { lease, .. } => {
                        if step.started.is_none() {
                            match self.leases.iter().position(|(l, _)| *l == lease) {
                                Some(pos) if self.leases[pos].1 => {
                                    self.leases.remove(pos);
                                    step.started = Some(lease);
                                }
                                Some(pos) => {
                                    self.leases.remove(pos);
                                    self.release_lease(lease, token, step, connecting);
                                }
                                None => self.release_lease(lease, token, step, connecting),
                            }
                        }
                    }
                },
                ServeEffect::RegisterIo { .. } | ServeEffect::StoreParked { .. } => {}
                ServeEffect::Discard { id, lease, cause } => {
                    self.bump(&format!("discard {cause:?}"));
                    step.discarded.push((id, lease, cause));
                }
                ServeEffect::ResumeGranted { lease, .. } => step.resume = Some(ResumeKind::Granted(lease)),
                ServeEffect::RequestPreempt { lease, .. } => step.resume = Some(ResumeKind::Preempt(lease)),
                ServeEffect::ResumeRejected { .. } => step.resume = Some(ResumeKind::Rejected),
            }
        }
    }

    fn index_lease(&self, s: u8) -> Option<LeaseId> {
        self.agg.index_entry(&id(s)).map(|e| e.lease)
    }

    /// Whether some live holder still owns `lease`'s target TCP: the shell
    /// (awaiting activation, or parked) or the relay (active).
    fn holds(&self, lease: LeaseId) -> bool {
        self.leases.iter().any(|(l, connected)| *l == lease && *connected)
            || (0..SESSIONS).any(|s| self.index_lease(s) == Some(lease))
    }
}

// ---------------------------------------------------------------------------
// Harness: the real shell, driven op by op.
// ---------------------------------------------------------------------------

/// What `handle_attach_stream` / `handle_resume_stream` hold while relaying.
struct Relay {
    lease: EstablishedLease,
    guard: SessionTableEntryGuard,
    tcp: (OwnedReadHalf, OwnedWriteHalf),
}

/// The `Arc`s registered for one incarnation at activation.
struct Incarnation {
    handle: Arc<Mutex<Session>>,
    preempt: Arc<Notify>,
}

/// Target-side end of one shell connection (`lease: None` = a stale park's
/// probe socket, which the shell must close).
struct Target {
    lease: Option<LeaseId>,
    stream: std::net::TcpStream,
}

enum Peer {
    Open,
    Closed,
}

fn peer_state(stream: &std::net::TcpStream) -> Peer {
    let mut buf = [0u8; 16];
    let mut reader = stream;
    match reader.read(&mut buf) {
        Ok(0) => Peer::Closed,
        Ok(_) => Peer::Open,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Peer::Open,
        Err(_) => Peer::Closed,
    }
}

async fn wait_peer_closed(stream: &std::net::TcpStream) -> bool {
    for _ in 0..2000 {
        if let Peer::Closed = peer_state(stream) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    false
}

async fn within<T>(what: &str, fut: impl std::future::Future<Output = T>) -> Result<T, TestCaseError> {
    tokio::time::timeout(OP_TIMEOUT, fut)
        .await
        .map_err(|_| TestCaseError::fail(format!("shell call `{what}` did not complete within {OP_TIMEOUT:?}")))
}

fn io_fail(what: &str, e: std::io::Error) -> TestCaseError {
    TestCaseError::fail(format!("{what}: {e}"))
}

/// Index entry with `parked_since` reduced to "parked or not" (the shell's
/// stamps are real milliseconds, the model's are logical; only their order
/// matters and that is forced equal, see the module docs).
fn parked_flag(e: IndexEntry) -> IndexEntry {
    IndexEntry { parked_since: e.parked_since.map(|_| Millis(0)), ..e }
}

struct Harness {
    rt: Arc<AttachRuntime>,
    listener: TcpListener,
    target_addr: SocketAddr,
    model: Model,
    max_sessions: usize,
    bypass_used: bool,
    pending: Vec<Option<(AttachKey, AttachToken)>>,
    relays: Vec<Option<Relay>>,
    incarnations: HashMap<LeaseId, Incarnation>,
    /// `(session, lease)` of incarnations that left the index (for `StaleFacts`).
    dead: Vec<(u8, LeaseId)>,
    prev_index: Vec<Option<LeaseId>>,
    targets: Vec<Target>,
}

impl Harness {
    async fn new(max_sessions: usize) -> Result<Self, TestCaseError> {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| io_fail("bind target", e))?;
        let target_addr = listener.local_addr().map_err(|e| io_fail("target addr", e))?;
        let n = usize::from(SESSIONS);
        Ok(Self {
            rt: AttachRuntime::new(target_addr, max_sessions),
            listener,
            target_addr,
            model: Model::new(max_sessions),
            max_sessions,
            bypass_used: false,
            pending: vec![None; n],
            relays: (0..n).map(|_| None).collect(),
            incarnations: HashMap::new(),
            dead: Vec::new(),
            prev_index: vec![None; n],
            targets: Vec::new(),
        })
    }

    async fn op(&mut self, op: Op) -> Result<(), TestCaseError> {
        match op {
            Op::Hello { s, g, at } => self.hello(key(s, g, at), false).await,
            Op::HelloBypass { s, g } => self.hello(key(s, g, 0), true).await,
            Op::Activate { s, grace, wrong_token } => self.activate(s, grace, wrong_token).await,
            Op::DataStreamDied { s } => self.park_relay(s, false).await.map(|_| ()),
            Op::TcpDied { s } => self.tcp_died(s).await,
            Op::Panic { s, guard_first } => self.panic(s, guard_first).await,
            Op::Resume { s, relay_yields, offset_gone } => self.resume(s, relay_yields, offset_gone).await,
            Op::Cancel { s, g, at, current } => {
                let k = match (current, self.pending[usize::from(s)]) {
                    (true, Some((k, _))) => k,
                    _ => key(s, g, at),
                };
                self.cancel(k).await
            }
            Op::Sweep { expire } => self.sweep(expire).await,
            Op::StaleFacts { s } => self.stale_facts(s).await,
        }
    }

    /// Accepts the target connections the model says this step made, in
    /// lease order (sequential ops ⇒ accept order = connect order).
    async fn accept_targets(&mut self, leases: &[LeaseId]) -> Result<(), TestCaseError> {
        for lease in leases {
            let (stream, _) = within("target accept", self.listener.accept()).await?.map_err(|e| io_fail("accept", e))?;
            let stream = stream.into_std().map_err(|e| io_fail("into_std", e))?;
            stream.set_nonblocking(true).map_err(|e| io_fail("set_nonblocking", e))?;
            self.targets.push(Target { lease: Some(*lease), stream });
        }
        // No extra connect beyond what the model predicts (a single poll: can
        // only miss an extra connect, never invent one).
        let extra = tokio::time::timeout(Duration::ZERO, self.listener.accept()).await;
        prop_assert!(extra.is_err(), "the shell opened a target connection the model did not predict");
        Ok(())
    }

    async fn hello(&mut self, k: AttachKey, bypass: bool) -> Result<(), TestCaseError> {
        let s = k.session_id.as_bytes()[0];
        let shell = if bypass {
            within("hello_bypassing_admission", self.rt.hello_bypassing_admission(k)).await?
        } else {
            within("hello", self.rt.hello(k)).await?
        };
        let shell = match shell {
            HelloOutcome::Ready { attach_token } => Ok(attach_token),
            HelloOutcome::Reject(reason) => Err(reason),
        };
        let token = shell.unwrap_or_else(|_| dummy_token());
        let mut step = Step::for_hello(k);
        let effects = if bypass {
            self.model.agg.hello_bypassing_admission(k)
        } else {
            self.model.agg.apply(ServeEvent::AdmitRequested { key: k })
        };
        self.model.process(effects, token, &mut step);
        let Some(model) = step.hello else {
            return Err(TestCaseError::fail(format!("model leaves hello({k:?}) unresolved, yet the shell returned")));
        };
        match (&shell, &model) {
            (Ok(a), Ok(b)) => prop_assert!(a == b, "hello token mismatch for {:?}", k),
            (Err(a), Err(b)) => prop_assert_eq!(a, b, "hello reject reason mismatch for {:?}", k),
            (Ok(_), Err(r)) => prop_assert!(false, "hello {:?}: shell ready, model rejected {:?}", k, r),
            (Err(r), Ok(_)) => prop_assert!(false, "hello {:?}: shell rejected {:?}, model ready", k, r),
        }
        match shell {
            Ok(t) => {
                self.pending[usize::from(s)] = Some((k, t));
                self.model.bump("hello ready");
            }
            Err(reason) => self.model.bump(&format!("hello rejected {reason:?}")),
        }
        if bypass {
            self.bypass_used = true;
        }
        let connects = std::mem::take(&mut step.connects);
        self.accept_targets(&connects).await
    }

    async fn activate(&mut self, s: u8, grace: Grace, wrong: bool) -> Result<(), TestCaseError> {
        let i = usize::from(s);
        let (k, mut tok) = self.pending[i].unwrap_or_else(|| (key(s, 0, 0), dummy_token()));
        if wrong {
            tok = wrong_token();
        }
        let handle = Arc::new(Mutex::new(Session::new(64)));
        let shell = within("activate", self.rt.activate(k, tok, grace.secs(), handle.clone())).await?;
        let mut step = Step::default();
        self.model.apply(
            ServeEvent::Activated { key: k, attach_token: tok, negotiated_grace_secs: grace.secs() },
            dummy_token(),
            &mut step,
        );
        prop_assert_eq!(shell.as_ref().map(|a| a.lease.id()), step.started, "activate({:?}) outcome diverged", k);
        if let Some(act) = shell {
            prop_assert!(self.relays[i].is_none(), "session {} activated while a relay already holds it", s);
            let lease_id = act.lease.id();
            let guard = SessionTableEntryGuard::new(self.rt.clone(), id(s), lease_id);
            self.incarnations.insert(lease_id, Incarnation { handle, preempt: act.preempt.clone() });
            self.relays[i] = Some(Relay { lease: act.lease, guard, tcp: act.tcp.into_split() });
            self.pending[i] = None;
            self.model.bump("activated");
        }
        Ok(())
    }

    /// The relay holding `s` ends with its target TCP still alive:
    /// `finish_or_park_session(DataStreamDied | Preempted)`. Returns whether
    /// there was a relay.
    async fn park_relay(&mut self, s: u8, preempted: bool) -> Result<bool, TestCaseError> {
        let Some(relay) = self.relays[usize::from(s)].take() else { return Ok(false) };
        let lease_id = relay.lease.id();
        tokio::time::sleep(PARK_SPACING).await;
        let (tcp_read, tcp_write) = relay.tcp;
        let outcome = if preempted {
            RelayOutcome::Preempted { tcp_read, tcp_write }
        } else {
            RelayOutcome::DataStreamDied { tcp_read, tcp_write }
        };
        within("finish_or_park_session", finish_or_park_session(&self.rt, relay.lease, relay.guard, id(s), outcome))
            .await?;
        let mut step = Step::default();
        let now = self.model.tick();
        self.model.apply(ServeEvent::Parked { id: id(s), lease: lease_id, now }, dummy_token(), &mut step);
        Ok(true)
    }

    async fn tcp_died(&mut self, s: u8) -> Result<(), TestCaseError> {
        let Some(relay) = self.relays[usize::from(s)].take() else { return Ok(()) };
        let lease_id = relay.lease.id();
        drop(relay.tcp);
        within(
            "finish_or_park_session(TcpDied)",
            finish_or_park_session(&self.rt, relay.lease, relay.guard, id(s), RelayOutcome::TcpDied),
        )
        .await?;
        let mut step = Step::default();
        self.model.apply(ServeEvent::RelayEnded { lease: lease_id }, dummy_token(), &mut step);
        self.model.apply(
            ServeEvent::RelayTerminated { id: id(s), lease: lease_id, reason: TerminateReason::TcpDied },
            dummy_token(),
            &mut step,
        );
        Ok(())
    }

    async fn panic(&mut self, s: u8, guard_first: bool) -> Result<(), TestCaseError> {
        let Some(relay) = self.relays[usize::from(s)].take() else { return Ok(()) };
        let lease_id = relay.lease.id();
        // Both RAII backstops fire. The current-thread scheduler polls spawned
        // tasks FIFO (and each backstop's apply is one uncontended critical
        // section), so the drop order is the apply order.
        if guard_first {
            drop(relay.guard);
            drop(relay.lease);
        } else {
            drop(relay.lease);
            drop(relay.guard);
        }
        drop(relay.tcp);
        for _ in 0..1000 {
            if !self.rt.index_contains(&id(s)).await {
                break;
            }
            tokio::task::yield_now().await;
        }
        // Let the second (no-op) backstop finish too.
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        let mut step = Step::default();
        let ended = ServeEvent::RelayEnded { lease: lease_id };
        let terminated =
            ServeEvent::RelayTerminated { id: id(s), lease: lease_id, reason: TerminateReason::GuardDropped };
        let (first, second) = if guard_first { (terminated, ended) } else { (ended, terminated) };
        self.model.apply(first, dummy_token(), &mut step);
        self.model.apply(second, dummy_token(), &mut step);
        self.model.bump("panic");
        Ok(())
    }

    /// Mirrors `handle_resume_stream`'s loop: on `Preempt`, notify, wait for
    /// the relay to re-park (here: the relay yields or ignores it), then
    /// re-send exactly once; a second `Preempt` is a rejection.
    async fn resume(&mut self, s: u8, relay_yields: bool, offset_gone: bool) -> Result<(), TestCaseError> {
        let i = usize::from(s);
        let mut preempted_once = false;
        loop {
            let decision = within("resume_request", self.rt.resume_request(id(s))).await?;
            let mut step = Step::default();
            self.model.apply(ServeEvent::ResumeRequested { id: id(s) }, dummy_token(), &mut step);
            let model = step.resume;
            match decision {
                ResumeDecision::Granted(grant) => {
                    prop_assert_eq!(model, Some(ResumeKind::Granted(grant.lease)), "RESUME of session {} diverged", s);
                    let Some(inc) = self.incarnations.get(&grant.lease) else {
                        return Err(TestCaseError::fail(format!("granted unknown lease {:?}", grant.lease)));
                    };
                    prop_assert!(
                        Arc::ptr_eq(&inc.handle, &grant.handle),
                        "granted output buffer is not the one registered for lease {:?} (I-i)",
                        grant.lease
                    );
                    prop_assert!(
                        Arc::ptr_eq(&inc.preempt, &grant.preempt),
                        "granted preempt signal is not the one registered for lease {:?}",
                        grant.lease
                    );
                    prop_assert!(self.relays[i].is_none(), "session {} granted while a relay still holds it", s);
                    let guard = SessionTableEntryGuard::new(self.rt.clone(), id(s), grant.lease);
                    if offset_gone {
                        // OffsetGone / RESUME_ACK write failure: re-park, then disarm.
                        tokio::time::sleep(PARK_SPACING).await;
                        within("park(repark)", self.rt.park(id(s), grant.lease, grant.tcp)).await?;
                        guard.disarm();
                        let now = self.model.tick();
                        let mut step = Step::default();
                        self.model.apply(ServeEvent::Parked { id: id(s), lease: grant.lease, now }, dummy_token(), &mut step);
                        self.model.bump("resume granted then re-parked");
                    } else {
                        let lease = self.rt.resumed_lease(grant.lease);
                        self.relays[i] = Some(Relay { lease, guard, tcp: grant.tcp });
                        self.model.bump("resume granted");
                    }
                    return Ok(());
                }
                ResumeDecision::Rejected => {
                    prop_assert_eq!(model, Some(ResumeKind::Rejected), "RESUME of session {} diverged", s);
                    self.model.bump("resume rejected");
                    return Ok(());
                }
                ResumeDecision::Preempt { preempt, reparked } => {
                    let Some(ResumeKind::Preempt(lease)) = model else {
                        return Err(TestCaseError::fail(format!(
                            "RESUME of session {s}: shell asked to preempt, model said {model:?}"
                        )));
                    };
                    let Some(inc) = self.incarnations.get(&lease) else {
                        return Err(TestCaseError::fail(format!("preempt for unknown lease {lease:?}")));
                    };
                    prop_assert!(
                        Arc::ptr_eq(&inc.preempt, &preempt),
                        "RESUME would preempt a different relay than the one holding lease {:?}",
                        lease
                    );
                    if preempted_once {
                        self.model.bump("resume preempt twice -> rejected");
                        return Ok(());
                    }
                    preempted_once = true;
                    let notified = reparked.notified();
                    preempt.notify_waiters();
                    if relay_yields {
                        let held = self.relays[i].as_ref().map(|r| r.lease.id());
                        prop_assert_eq!(held, Some(lease), "preempted session {} is not held by its relay", s);
                        self.park_relay(s, true).await?;
                        prop_assert!(
                            tokio::time::timeout(Duration::ZERO, notified).await.is_ok(),
                            "an accepted re-park of session {} did not signal `reparked`",
                            s
                        );
                        self.model.bump("resume preempt yielded");
                    } else {
                        // The relay ignores it; the real loop waits PREEMPT_WAIT_TIMEOUT and re-sends.
                        drop(notified);
                        self.model.bump("resume preempt ignored");
                    }
                }
            }
        }
    }

    async fn cancel(&mut self, k: AttachKey) -> Result<(), TestCaseError> {
        within("cancel", self.rt.cancel(k)).await?;
        let mut step = Step::default();
        self.model.apply(ServeEvent::CancelReceived { key: k }, dummy_token(), &mut step);
        Ok(())
    }

    async fn sweep(&mut self, expire: bool) -> Result<(), TestCaseError> {
        let max_parked = if expire { Duration::ZERO } else { LONG_WINDOW };
        let mut shell = within("sweep_expired_parked", self.rt.sweep_expired_parked(max_parked)).await?;
        let mut step = Step::default();
        let now = self.model.tick();
        self.model.apply(ServeEvent::Sweep { now, max_parked }, dummy_token(), &mut step);
        let mut model: Vec<SessionKey> = step.discarded.iter().map(|(id, _, _)| *id).collect();
        shell.sort_unstable();
        model.sort_unstable();
        prop_assert_eq!(shell, model, "sweep(max_parked={:?}) discarded different sessions", max_parked);
        Ok(())
    }

    async fn stale_facts(&mut self, s: u8) -> Result<(), TestCaseError> {
        let Some(&(_, lease)) = self.dead.iter().rev().find(|(ds, _)| *ds == s) else { return Ok(()) };
        // A fresh socket for the stale park: the shell must close it, not store it.
        let probe = within("probe connect", TcpStream::connect(self.target_addr)).await?.map_err(|e| io_fail("probe", e))?;
        let (accepted, _) = within("probe accept", self.listener.accept()).await?.map_err(|e| io_fail("accept", e))?;
        let accepted = accepted.into_std().map_err(|e| io_fail("into_std", e))?;
        accepted.set_nonblocking(true).map_err(|e| io_fail("set_nonblocking", e))?;
        self.targets.push(Target { lease: None, stream: accepted });

        within("relay_terminated(stale)", self.rt.relay_terminated(id(s), lease, TerminateReason::GuardDropped)).await?;
        within("relay_ended(stale)", self.rt.relay_ended(lease)).await?;
        within("park(stale)", self.rt.park(id(s), lease, probe.into_split())).await?;

        let mut step = Step::default();
        self.model.apply(
            ServeEvent::RelayTerminated { id: id(s), lease, reason: TerminateReason::GuardDropped },
            dummy_token(),
            &mut step,
        );
        self.model.apply(ServeEvent::RelayEnded { lease }, dummy_token(), &mut step);
        let now = self.model.tick();
        self.model.apply(ServeEvent::Parked { id: id(s), lease, now }, dummy_token(), &mut step);
        prop_assert!(step.discarded.is_empty(), "a stale lease discarded something: {:?}", step.discarded);
        self.model.bump("stale facts");
        Ok(())
    }

    /// The differential comparison plus the shell-side invariants; run after
    /// every operation.
    async fn check(&mut self) -> Result<(), TestCaseError> {
        let ids: Vec<SessionKey> = (0..SESSIONS).map(id).collect();
        let snap = self.rt.snapshot_for_test(&ids).await;
        for s in 0..SESSIONS {
            let i = usize::from(s);
            let model_state = self.model.agg.arbiter().state_for(WireSessionId::from_bytes(id(s))).cloned();
            prop_assert_eq!(&snap.states[i], &model_state, "arbiter state of session {} diverged", s);
            let model_entry = self.model.agg.index_entry(&id(s)).copied();
            prop_assert_eq!(
                snap.index[i].map(parked_flag),
                model_entry.map(parked_flag),
                "index entry of session {} diverged",
                s
            );
            // I-c on the shell's own aggregate: Established ⇔ index entry, same lease.
            let established = match &snap.states[i] {
                Some(AttachState::Established { lease, .. }) => Some(*lease),
                Some(AttachState::Connecting { .. })
                | Some(AttachState::PendingActivation { .. })
                | Some(AttachState::ClosingForSupersede { .. })
                | None => None,
            };
            prop_assert_eq!(established, snap.index[i].map(|e| e.lease), "shell: Established/index mismatch for session {}", s);
            // I-g: an unresumable entry is never parked.
            let parked_unresumable = snap.index[i].is_some_and(|e| e.unresumable && e.parked_since.is_some());
            prop_assert!(!parked_unresumable, "shell: unresumable entry of session {} is parked (I-g)", s);
        }
        prop_assert_eq!(snap.session_count, self.model.agg.arbiter().session_count(), "slot count diverged");
        let live = snap.index.iter().flatten().filter(|e| !e.unresumable).count();
        prop_assert!(live <= self.max_sessions, "shell: {} live entries exceed max_sessions {}", live, self.max_sessions);
        if !self.bypass_used {
            prop_assert!(
                snap.session_count <= self.max_sessions,
                "shell: {} slots exceed max_sessions {} (I-k)",
                snap.session_count,
                self.max_sessions
            );
        }
        // The socket map and the index describe the same incarnations.
        let indexed = snap.index.iter().flatten().count();
        prop_assert_eq!(snap.io.len(), indexed, "shell: socket map and index differ in size");
        for (k, lease, has_socket) in &snap.io {
            let entry = ids.iter().position(|i| i == k).and_then(|i| snap.index[i]);
            let same_lease = entry.is_some_and(|e| e.lease == *lease);
            prop_assert!(same_lease, "shell: SessionIo of session {} (lease {:?}) has no index entry with that lease", k[0], lease);
            let parked = entry.is_some_and(|e| e.parked_since.is_some());
            prop_assert_eq!(*has_socket, parked, "shell: parked socket presence vs index parked flag for session {}", k[0]);
        }
        self.check_targets().await?;
        self.record_dead();
        Ok(())
    }

    /// Every target TCP is open iff the model still holds its incarnation;
    /// released ones must be observed closed. Verified-closed ones are dropped
    /// from the list (a lease never comes back).
    async fn check_targets(&mut self) -> Result<(), TestCaseError> {
        for t in &self.targets {
            let should_be_open = t.lease.is_some_and(|l| self.model.holds(l));
            if should_be_open {
                let open = matches!(peer_state(&t.stream), Peer::Open);
                prop_assert!(open, "target TCP of live lease {:?} was closed by the shell", t.lease);
            } else {
                let closed = wait_peer_closed(&t.stream).await;
                prop_assert!(closed, "target TCP of released lease {:?} is still open (socket leak)", t.lease);
            }
        }
        let model = &self.model;
        self.targets.retain(|t| t.lease.is_some_and(|l| model.holds(l)));
        Ok(())
    }

    fn record_dead(&mut self) {
        for s in 0..SESSIONS {
            let i = usize::from(s);
            let now = self.model.index_lease(s);
            if let Some(old) = self.prev_index[i] {
                if now != Some(old) {
                    self.dead.push((s, old));
                }
            }
            self.prev_index[i] = now;
        }
    }

    /// Ends the case: every relay's target dies, a zero-window sweep, every
    /// pending attempt cancelled — after which nothing may be left.
    async fn teardown(&mut self) -> Result<(), TestCaseError> {
        for s in 0..SESSIONS {
            self.tcp_died(s).await?;
        }
        self.sweep(true).await?;
        for s in 0..SESSIONS {
            let state = self.model.agg.arbiter().state_for(WireSessionId::from_bytes(id(s))).cloned();
            if let Some(AttachState::PendingActivation { key, .. }) = state {
                self.cancel(key).await?;
            }
        }
        self.check().await?;
        let ids: Vec<SessionKey> = (0..SESSIONS).map(id).collect();
        let snap = self.rt.snapshot_for_test(&ids).await;
        prop_assert_eq!(snap.session_count, 0, "a fencing slot outlived the teardown (always-connects.md leak)");
        let leftover_entries = snap.index.iter().flatten().count();
        prop_assert_eq!(leftover_entries, 0, "an index entry outlived the teardown");
        prop_assert!(snap.io.is_empty(), "a SessionIo (socket) outlived the teardown");
        prop_assert!(self.targets.is_empty(), "{} target TCP connection(s) outlived the teardown", self.targets.len());
        Ok(())
    }
}

/// Runs one case; returns the model's coverage counters.
fn run_case(max_sessions: usize, ops: Vec<Op>) -> Result<BTreeMap<String, usize>, TestCaseError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| io_fail("build runtime", e))?;
    runtime.block_on(async move {
        let mut h = Harness::new(max_sessions).await?;
        let started = std::time::Instant::now();
        for op in ops {
            if started.elapsed() > CASE_BUDGET {
                h.model.bump("case budget exhausted");
                break;
            }
            h.op(op).await?;
            h.check().await?;
        }
        h.teardown().await?;
        Ok::<_, TestCaseError>(std::mem::take(&mut h.model.coverage))
    })
}

proptest! {
    // Real sockets: fewer cases than the reducer proptests' default 256 (each
    // case is a few ms; the deterministic coverage test below adds 64 more).
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Step 10-1: the real serve shell matches the `ServeAggregate` model
    /// under arbitrary operation sequences (see the module docs for what is
    /// compared and asserted after every step).
    #[test]
    fn serve_shell_matches_the_serve_aggregate_model((max_sessions, ops) in case_strategy()) {
        run_case(max_sessions, ops)?;
    }
}

/// Non-vacuity of the differential test (same generator, fixed seed, so the
/// result is deterministic): the generated sequences actually drive the shell
/// through every discard cause, every RESUME outcome, admission rejection,
/// supersede/cancel, the panic backstops and stale facts. Also runs those 64
/// deterministic cases through the full differential harness.
#[test]
fn differential_cases_reach_the_interesting_shell_paths() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    let mut runner = TestRunner::deterministic();
    let strategy = case_strategy();
    let mut hits: BTreeMap<String, usize> = BTreeMap::new();
    for case in 0..64 {
        let (max_sessions, ops) = strategy.new_tree(&mut runner).expect("generate a case").current();
        let coverage = run_case(max_sessions, ops.clone())
            .unwrap_or_else(|e| panic!("deterministic case {case} failed: {e}\nmax_sessions={max_sessions} ops={ops:?}"));
        for (k, v) in coverage {
            *hits.entry(k).or_default() += v;
        }
    }
    // Hand-written seed for the one path too deep for the random generator to
    // hit reliably (the first CI run of the 64 deterministic cases reached
    // every other required path but not this one): an over-capacity
    // (unresumable) session whose data stream dies → `Discard{Unresumable}`,
    // which must free its slot and close its target TCP (ADR I-g).
    let seed = vec![
        Op::Hello { s: 0, g: 0, at: 0 },
        Op::Activate { s: 0, grace: Grace::Long, wrong_token: false },
        Op::HelloBypass { s: 1, g: 0 },
        Op::Activate { s: 1, grace: Grace::Long, wrong_token: false },
        Op::DataStreamDied { s: 1 },
        Op::Hello { s: 1, g: 1, at: 0 },
    ];
    let coverage = run_case(1, seed).unwrap_or_else(|e| panic!("unresumable seed case failed: {e}"));
    for (k, v) in coverage {
        *hits.entry(k).or_default() += v;
    }
    let required = [
        "activated",
        "hello rejected BusyOtherSession",
        "hello rejected AttachAlreadyEstablished",
        "lease cancelled",
        "panic",
        "resume granted",
        "resume granted then re-parked",
        "resume preempt yielded",
        "resume preempt ignored",
        "resume preempt twice -> rejected",
        "resume rejected",
        "stale facts",
        "discard Expired",
        "discard Evicted",
        "discard TcpDied",
        "discard GuardDropped",
        "discard Unresumable",
    ];
    let missing: Vec<&str> = required.iter().copied().filter(|k| !hits.contains_key(*k)).collect();
    assert!(missing.is_empty(), "the differential cases never reached {missing:?} (hits: {hits:?})");
}
