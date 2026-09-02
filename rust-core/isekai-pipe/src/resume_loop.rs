//! The relay/STUN-P2P data pump and relay-resumable reconnect loop —
//! everything downstream of a successful [`crate::connect`] dial. Owns the
//! C2H replay buffer, the RESUME backoff/retry loop, warm-standby promotion,
//! and the OS network-change → reconnect signal plumbing. See
//! `run_resume_loop`'s own doc comment for why the network-change handling
//! needed a background task (`spawn_reconnect_signal`) rather than racing
//! `run_data_pump` directly in one `select!`.

use std::io::IsTerminal;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use isekai_transport::{
    compute_proof, connect_stun_p2p_with_fallback, connect_via_relay_resumable, connect_via_relay_resumable_with_fallback,
    open_control_stream, reconnect_and_resume, spawn_app_ack_tasks, system_quic_factory, AnyByteStream,
    AnyByteStreamReadHalf, AnyByteStreamWriteHalf, AnyMuxConnection, AnyMuxFactory, AnyMuxRebinder, AppAckCounters,
    AppAckTasks, BackoffPolicy, BindSpec, C2hSentOffset, H2cClientDeliveredOffset, RelayTarget,
    ResumableRelaySession, SequentialRelayCandidate, SequentialStunCandidate, StunP2pConnection,
    StunP2pTarget,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::connect::{attach_stale_trust_signal, relay_endpoint_factory};
use crate::RelayTransportKind;
use crate::DEFAULT_RESUME_WINDOW;

const C2H_REPLAY_BUFFER_CAPACITY: usize = 4 * 1024 * 1024;
/// `jitter: 0.25`(±25%、ADR_SLEEP_RESUME_MUX_OWNER_DEATH.md D-4): スリープ
/// 復帰やネットワーク瞬断は「複数タブが同時に同じイベントを見る」典型例
/// であり、ジッター無しの純粋な指数バックオフでは全タブが同一グリッド上で
/// 同期再試行し、サーバー側のresume競合(D-2)を確実に踏みにいく。
const RESUME_BACKOFF: BackoffPolicy = BackoffPolicy {
    initial: Duration::from_millis(500),
    max: Duration::from_secs(10),
    jitter: 0.25,
};
const BACKPRESSURE_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Bounds `replay_and_advance`'s post-resume replay write — same rationale
/// and magnitude as `isekai_transport::resume`'s `TRANSPORT_STEP_TIMEOUT`
/// (not reused directly since that constant is private to that crate):
/// don't trust `noq`'s own idle-timeout/keepalive to catch a connection this
/// call itself just received from a successful `RESUME`, e.g. right after a
/// host suspend/resume where a monotonic clock can undercount elapsed real
/// time.
const REPLAY_WRITE_TIMEOUT: Duration = Duration::from_secs(15);
/// How often `run_resume_loop`'s background task calls
/// `WarmStandby::ensure_warm` while `--tethering-interface` is set. Matches
/// the "~15-30s while the primary looks healthy" half of the
/// `pc-tethering-warm-standby-design` memory's agreed tiering — the more
/// aggressive "~1-3s once the primary looks like it's degrading" half is not
/// implemented (this loop has no independent signal that the primary is
/// degrading, only that it's already dead, at which point promotion is
/// already being attempted).
const WARM_STANDBY_PROBE_INTERVAL: Duration = Duration::from_secs(20);
/// How many multiples of [`WARM_STANDBY_PROBE_INTERVAL`] the *wall-clock* gap
/// between two consecutive warm-standby ticks must exceed before it's
/// treated as a host suspend/resume rather than ordinary scheduling jitter
/// (ADR_SLEEP_RESUME_MUX_OWNER_DEATH.md D-3) — comfortably above what
/// `MissedTickBehavior::Delay` coalescing or CPU contention could plausibly
/// cause on their own, since `tokio::time::interval` ticks on a *monotonic*
/// clock that (unlike wall-clock) does not count suspended time at all, so a
/// real suspend shows up here as wall-clock racing far ahead of the
/// monotonic tick count.
const WARM_STANDBY_SUSPEND_JUMP_FACTOR: u32 = 3;
/// STUN P2P uses the server-granted resume grace unchanged, but this client
/// only keeps bare-redial resume attempts alive briefly before returning
/// control to the wrapper's full STUN re-establishment loop.
const STUN_RESUME_GIVE_UP_WINDOW: Duration = Duration::from_secs(120);

/// Marks an `anyhow::Error` as having occurred *after* the STUN P2P
/// handshake already succeeded — i.e. after the route has entered its
/// STUN-scoped `run_resume_loop`, not while dialing. Mirrors
/// `isekai_transport::StaleTrustSignal`'s attach-at-the-source/
/// downcast-at-the-top shape (`connect.rs::attach_stale_trust_signal`): a
/// bare `err.downcast_ref::<MidSessionDisconnectSignal>()` finds this
/// even through an outer `.context(...)` wrap, since `anyhow`'s own
/// `downcast_ref` already walks the `ContextError` chain — no manual
/// `err.chain()` traversal needed (round 1 review, R1-B2/B2, corrected a
/// wrong assumption in an earlier draft of this fix).
///
/// Deliberately attached only by STUN P2P callers of `run_resume_loop`.
/// Relay callers still do not add it: relay resume already retries
/// internally for its full server-granted window, so an error escaping that
/// loop is terminal and the existing full re-bootstrap response is correct.
/// STUN P2P has a shorter client-side give-up boundary because bare redial
/// cannot fix cases where the server-side observed address is no longer
/// reachable; once that boundary is hit, the wrapper should retry a fresh
/// STUN establishment rather than silently wait for the much longer server
/// resume-grace.
///
/// Attached to whatever `run_resume_loop` returns as `Err`, which since
/// `PumpFailure`/[`ParentGoneSignal`] (Task 2.9 / issue #111, 2026-09) can be
/// a *resume-exhaustion* error (`resume_with_backoff_until_deadline`'s
/// give-up, after a `Remote`-classified pump failure or network change), a
/// `PumpFailure::Local` error, or a `ParentGoneSignal`-marked error — the
/// latter two returned immediately, without ever entering the resume loop.
/// All are correctly `MidSessionDisconnect` from the wrapper's point of
/// view: `connect_command`'s `write_connect_outcome_for_wrapper` special-cases
/// `ParentGoneSignal` to skip writing an outcome file at all (see its own
/// docs) rather than reach this class, since that path means there is no
/// meaningful action left for the wrapper to take (see `ParentGoneSignal`'s
/// docs for why); a bare `PumpFailure::Local` reaching here instead (the
/// rarer path where the local pipe breaks while `ssh(1)` is still alive —
/// see `PumpFailure`'s docs) is exactly the case a lightweight retry
/// (spawning a fresh `isekai-pipe connect`) is the right response to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MidSessionDisconnectSignal;

impl std::fmt::Display for MidSessionDisconnectSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the connection was lost mid-session, after it was already established")
    }
}

impl std::error::Error for MidSessionDisconnectSignal {}

/// Marks an `anyhow::Error` as meaning "this process's local peer (normally
/// `ssh(1)`) is gone, so no recovery action is meaningful" — Task 2.9 /
/// issue #111. Two independent sources attach it, both handled uniformly by
/// `connect_command::write_connect_outcome_for_wrapper` (which checks for
/// this marker first and skips writing a `ConnectOutcome` file at all when
/// present, rather than classifying it `Unreachable`/`MidSessionDisconnect`):
///
/// 1. [`parent_watchdog`]'s blocking-`poll()` watchdog, raced against the
///    whole `run_connect` future in `connect_command` — fires uniformly
///    across dial, handshake, the resume backoff loop, and the active pump,
///    on Unix targets where it's available (see that module's docs for why
///    a blocking `poll()` rather than `prctl(PR_SET_PDEATHSIG)`/`kqueue`).
/// 2. `run_resume_loop`'s own `PumpFailure::Local` branch and its EOF-latch
///    (see `PumpFailure`'s docs) — the reactive fallback that still matters
///    on non-Unix targets (MSYS2/Cygwin-hosted `ssh.exe`'s native-Windows
///    `isekai-pipe connect` child has no `poll(2)` available) and, on Unix,
///    as defense-in-depth for the moment between a real I/O failure and the
///    watchdog's own detection.
///
/// Skipping the outcome write (rather than writing `Unreachable`, which
/// `RebootstrapAndRetry`s, or `MidSessionDisconnect`, which lightweight-
/// retries) matters concretely on the relay route: `run_relay_resumable`/
/// `run_relay_resumable_with_fallback` attach no `MidSessionDisconnectSignal`
/// (see its own docs for why), so an unmarked `Local`/watchdog-triggered
/// error would otherwise surface as `Unreachable` → `RebootstrapAndRetry`
/// (`wrapper.rs::decide_connect_failure_recovery`) — an arm that, unlike
/// `RetryConnectLightweight`, has no guard against re-running a non-idempotent
/// remote command (`wrapper.rs`'s B5 guard, Epic R PR2). A broken local pipe
/// means the `ssh(1)` session this command was running under is already
/// gone; silently re-deploying and re-running `isekai-ssh host -- ./deploy.sh`
/// in response would be exactly the hazard that guard exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParentGoneSignal;

impl std::fmt::Display for ParentGoneSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the local peer (most likely ssh(1)) is gone; no recovery action is meaningful")
    }
}

impl std::error::Error for ParentGoneSignal {}

/// `run_data_pump`'s failure, classified by which side of the pipe broke.
///
/// Originally the sole fix for the hang `MidSessionDisconnectSignal`'s docs
/// describe (Task 2.9 / issue #111): a `Local` failure means the process's
/// own stdin/stdout — the pipe `ssh(1)`'s `ProxyCommand` gave this process —
/// is gone, most often because `ssh(1)` itself already exited. Confirmed
/// 2026-09-02 by direct experiment (synthetic `ProxyCommand`, real OpenSSH
/// `ssh(1)`): `ssh(1)` reaps this child when *it* decides to exit (e.g.
/// `ConnectTimeout`), but does **not** when something else kills `ssh(1)`
/// itself (`SIGTERM`/`SIGKILL`) — the child is simply reparented to init and
/// keeps running.
///
/// This split alone is **not sufficient**, for two reasons a second round of
/// adversarial review found: (a) the dominant signal when `ssh(1)` dies is a
/// clean stdin **EOF** (`pump_c2h`'s `n == 0` branch), not an `Err` —
/// `PumpFailure::Local` never fires from that side in the common case; and
/// (b) neither pump is even running while this process is blocked in
/// `resume_with_backoff_until_deadline` or an initial dial/handshake, so a
/// `Local`/`Remote` classification of a pump result can't help there at all.
/// [`parent_watchdog`]'s blocking-`poll()` watchdog is the actual primary
/// fix for both — see its module docs. What's left for `PumpFailure`/the
/// EOF-latch to cover: the reactive fallback on non-Unix targets (no
/// watchdog there), and, on Unix, the narrower case (a) doesn't fully close
/// even with the watchdog present — a `Remote` failure in `pump_h2c` arriving
/// *after* `pump_c2h` already saw clean EOF in the same generation, which
/// the EOF-latch (`run_resume_loop`'s `c2h_already_done` check) declines to
/// resume past for the identical reason: no local producer is left to
/// resume for. A `Remote` failure (the QUIC stream itself, the C2H replay
/// buffer invariant, or an OS-reported network change) with no
/// already-observed local EOF is exactly the case the resume loop exists
/// for and is handled unchanged.
#[derive(Debug)]
enum PumpFailure {
    Local(anyhow::Error),
    Remote(anyhow::Error),
}

/// Narrow signal a retried-connect error type must expose for
/// [`retry_while_busy_other_session`] — named distinctly from the underlying
/// `TransportError`/`SequentialConnectError::is_busy_other_session` inherent
/// methods it delegates to, so calling `self.is_busy_other_session()` inside
/// each impl unambiguously reaches the inherent one rather than recursing.
pub(crate) trait BusyOtherSessionSignal {
    fn signals_busy_other_session(&self) -> bool;
}

impl BusyOtherSessionSignal for isekai_transport::TransportError {
    fn signals_busy_other_session(&self) -> bool {
        self.is_busy_other_session()
    }
}

impl BusyOtherSessionSignal for isekai_transport::SequentialConnectError {
    fn signals_busy_other_session(&self) -> bool {
        self.is_busy_other_session()
    }
}

/// Deliberately **not** derived from `resume_window_for`/`resume-grace`
/// (unlike a same-process resume loop's own deadline) — even though a
/// `BUSY_OTHER_SESSION` reject on the very first connect most often means
/// *this same client's* previous session is still parked on the remote
/// helper (see `TransportError::is_busy_other_session`'s docs), waiting for
/// that park to clear on its own is no longer a sound thing to size against
/// `resume-grace`, now that the value is days long by default
/// (`isekai_pipe_core::DEFAULT_RESUME_GRACE_SECS`'s docs). A fixed, short
/// window here is defense-in-depth for `isekai-pipe serve` deployments that
/// predate `ISEKAI_PIPE_DESIGN.md` §8's parked-session-preemption fix
/// (`engine/mod.rs::hello_with_parked_preemption`) — helper reuse
/// deliberately doesn't force those to redeploy (`reuse.rs`'s fingerprint
/// exclusion), so they can keep running the old, un-preempting behavior for
/// up to `--max-idle-lifetime` (30 days) after this fix ships. Against a
/// server that *does* have the fix, a legitimate retry here succeeds almost
/// immediately (the preemption is atomic, no real waiting involved), so
/// this window is never the limiting factor in the common case; against one
/// that doesn't, it turns what would otherwise be a silent multi-day hang
/// into a fast, visible failure instead.
pub(crate) const BUSY_OTHER_SESSION_RETRY_WINDOW: Duration = Duration::from_secs(180);

/// Retries `attempt` while — and only while — it fails with
/// `BUSY_OTHER_SESSION`, for up to `window` (production call sites always
/// pass `BUSY_OTHER_SESSION_RETRY_WINDOW` — see that constant's docs for why
/// it's a short fixed window rather than `resume-grace`-derived; `window`
/// is a parameter rather than the constant used directly only so tests can
/// inject a short one instead of a real 180-second wait). Every other
/// failure is returned immediately on the first attempt, unchanged from
/// before this wrapper existed: this only closes the gap where a fresh
/// `isekai-pipe connect` process (a brand new `session_id` every time,
/// since neither `connect_via_relay_resumable` nor `_with_fallback` persist
/// one across invocations) would otherwise fail outright instead of waiting
/// the same window a same-process resume would have. `pub(crate)` (Epic R
/// PR2, Task 2.11) so STUN P2P's initial establishment paths can reuse it
/// too: a fresh reconnect attempt racing this same client's own
/// not-yet-expired parked session is exactly as possible there as it is for
/// relay.
pub(crate) async fn retry_while_busy_other_session<T, E, F, Fut>(window: Duration, mut attempt: F) -> Result<T, E>
where
    E: BusyOtherSessionSignal,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let deadline = Instant::now() + window;
    let mut attempt_no: u32 = 0;
    loop {
        let err = match attempt().await {
            Ok(ok) => return Ok(ok),
            Err(err) => err,
        };
        let now = Instant::now();
        if !err.signals_busy_other_session() || now >= deadline {
            return Err(err);
        }
        let delay = RESUME_BACKOFF.delay_for_attempt(attempt_no, &mut rand::thread_rng()).min(deadline - now);
        attempt_no = attempt_no.saturating_add(1);
        eprintln!(
            "isekai-pipe connect: remote helper reports BUSY_OTHER_SESSION (likely this client's own prior \
             session still parked from an earlier disconnect); retrying in {delay:?}"
        );
        tokio::time::sleep(delay).await;
    }
}

pub(crate) async fn run_relay_resumable(
    target: &RelayTarget,
    profile: &str,
    requested_resume_grace_secs: u64,
    identity: isekai_transport::CandidateIdentity<'_>,
    experimental_network_rebind: bool,
    relay_transport: RelayTransportKind,
    tethering_interface: Option<isekai_transport::InterfaceIndex>,
) -> Result<()> {
    let factory = relay_endpoint_factory(relay_transport);
    let requested = u32::try_from(requested_resume_grace_secs).unwrap_or(u32::MAX);
    let established = retry_while_busy_other_session(BUSY_OTHER_SESSION_RETRY_WINDOW, || connect_via_relay_resumable(&factory, target, requested, identity))
        .await
        .map_err(attach_stale_trust_signal)?;
    run_resume_loop(&factory, target, profile, established, experimental_network_rebind, tethering_interface, None).await
}

/// Like `run_relay_resumable`, but tries `candidates` in priority order
/// (`ISEKAI_PIPE_DESIGN.md` task #12: relay-endpoint fallback) instead of
/// dialing a single fixed target. Falls back only across pre-attach
/// failures — see `connect_via_relay_resumable_with_fallback`'s and
/// `AttemptFailure`'s docs for why an ambiguous or terminal failure on one
/// candidate stops the whole attempt rather than trying the next one.
pub(crate) async fn run_relay_resumable_with_fallback(
    candidates: &[SequentialRelayCandidate],
    profile: &str,
    requested_resume_grace_secs: u64,
    experimental_network_rebind: bool,
    relay_transport: RelayTransportKind,
    tethering_interface: Option<isekai_transport::InterfaceIndex>,
) -> Result<()> {
    let factory = relay_endpoint_factory(relay_transport);
    let requested = u32::try_from(requested_resume_grace_secs).unwrap_or(u32::MAX);
    let (established, winning_target) =
        retry_while_busy_other_session(BUSY_OTHER_SESSION_RETRY_WINDOW, || connect_via_relay_resumable_with_fallback(&factory, candidates, requested))
            .await
            .map_err(attach_stale_trust_signal)?;
    run_resume_loop(&factory, &winning_target, profile, established, experimental_network_rebind, tethering_interface, None).await
}

pub(crate) async fn run_stun_p2p_resumable(
    factory: &AnyMuxFactory,
    target: &RelayTarget,
    profile: &str,
    connection: StunP2pConnection,
) -> Result<()> {
    // The whole body is wrapped in `MidSessionDisconnectSignal`, not just
    // `run_resume_loop`'s own result (round 3 code review, minor 3): the
    // STUN P2P handshake has already fully succeeded by the time this
    // function is called (that's `connection`'s existence), so a failure to
    // open the control stream right after is just as much a mid-session
    // disconnect as a later resume give-up — it must not escape unmarked
    // and get classified as a connect-time `Unreachable` instead.
    async {
        let control = open_control_stream(&connection.connection, &connection.proof).await?;
        let established = ResumableRelaySession {
            connection: connection.connection,
            data_stream: connection.stream,
            control_stream: control.stream,
            session_id: control.session_id,
            effective_resume_grace_secs: connection.effective_resume_grace_secs,
            network_rebinder: connection.network_rebinder,
        };
        run_resume_loop(
            factory,
            target,
            profile,
            established,
            // STUN P2P's punched NAT mapping is tied to the socket used for
            // initial establishment. Rebinding or warm-standby promotion would
            // switch sockets/interfaces without the rejected re-rendezvous
            // primitive, so both are deliberately disabled on this path.
            /* experimental_network_rebind */ false,
            /* tethering_interface */ None,
            Some(STUN_RESUME_GIVE_UP_WINDOW),
        )
        .await
    }
    .await
    .map_err(|e| e.context(MidSessionDisconnectSignal))
}

/// Like the single-candidate `CandidateRoute::StunP2p` path in `run_connect`,
/// but tries `candidates` (each a different STUN server against the same
/// peer) in priority order (`#11`) instead of dialing a single fixed STUN
/// server. After the winning STUN connection is established, it uses the
/// same byte-level RESUME loop as relay mode, scoped to that winning peer
/// address and session id.
pub(crate) async fn run_stun_p2p_with_fallback(
    target: &StunP2pTarget,
    candidates: &[SequentialStunCandidate],
    profile: &str,
    requested_resume_grace_secs: u64,
) -> Result<()> {
    let factory = system_quic_factory();
    // Epic R PR2, Task 2.11: STUN P2P used to skip the same
    // `BUSY_OTHER_SESSION` retry the relay paths already get above — a
    // fresh `isekai-pipe connect` process reconnecting right after a
    // mid-session disconnect (this module's whole reason for existing) is
    // exactly the case `retry_while_busy_other_session`'s docs describe:
    // this same client's own prior session still parked on the remote
    // helper, not a real conflicting session.
    let (connection, _winning_stun_server) =
        retry_while_busy_other_session(BUSY_OTHER_SESSION_RETRY_WINDOW, || {
            let requested = u32::try_from(requested_resume_grace_secs).unwrap_or(u32::MAX);
            connect_stun_p2p_with_fallback(&factory, target, candidates, requested)
        })
            .await
            .map_err(attach_stale_trust_signal)?;
    let relay_target = RelayTarget {
        helper_addr: target.peer_addr,
        server_name: target.server_name.clone(),
        cert_sha256_hex: target.cert_sha256_hex.clone(),
        session_secret: target.session_secret.clone(),
        // `run_stun_p2p_with_fallback` receives no `ConnectionIntent`, so
        // this path intentionally has no source for `local_bind_port_range`.
        local_bind_port_range: None,
    };
    run_stun_p2p_resumable(&factory, &relay_target, profile, connection).await
}

/// Runs the C2H/H2C data pump against `established`, resuming (via
/// `reconnect_and_resume` against `target` — the *specific* candidate that
/// won, in the fallback case) across disconnects until one of: the local
/// side closes cleanly, the resume window is exceeded, or a `PumpFailure`/
/// EOF-latch-driven give-up decides no resume attempt is worthwhile at all
/// (see `PumpFailure`'s and `ParentGoneSignal`'s docs). Shared by both
/// `run_relay_resumable` (single fixed target) and
/// `run_relay_resumable_with_fallback` (the winning target out of several
/// candidates) — resuming a session is always scoped to the one connection
/// that established it, never a fresh candidate search. `max_resume_window`
/// is `None` for relay callers and `Some` only for STUN P2P's shorter
/// client-side give-up boundary; it does not alter the server-granted
/// `effective_resume_grace_secs`.
/// Picks an OS-assigned-ephemeral-port wildcard bind address matching
/// `remote`'s address family — the same "let the OS pick a fresh source"
/// approach `BindSpec::any_ipv4()` already uses for every *new* connection,
/// reused here for `AnyMuxRebinder::rebind`'s replacement socket. Not
/// an explicit interface choice (see `AnyMuxRebinder::rebind`'s docs):
/// just a fresh socket for the OS to route via its current default path,
/// which is what actually helps after e.g. a Wi-Fi disconnect where the OS
/// has since switched its default route to something else.
fn remote_bind_spec(remote: std::net::SocketAddr, local_bind_port_range: Option<(u16, u16)>) -> BindSpec {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
    let local_addr = if remote.is_ipv4() {
        SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)
    } else {
        SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0)
    };
    BindSpec { local_addr, port_range: local_bind_port_range }
}

/// Spawns this connection generation's "the current connection should be
/// abandoned and reconnected via RESUME" signal source for `run_resume_loop`,
/// and returns a task to `.abort()` once the caller's own `select!` resolves
/// (unconditionally — cheap/harmless to abort either shape below) alongside
/// a receiver that yields exactly once, the moment reconnection should
/// happen.
///
/// Two shapes, chosen by whether `rebinder` is both present and
/// `experimental_network_rebind` is set:
///
/// - **Default** (`experimental_network_rebind` off, or this generation's
///   `AnyMuxFactory` doesn't support rebinding): every OS-reported
///   network change (`isekai-netmon`; real backends on Windows/macOS/Linux,
///   a no-op elsewhere) is forwarded immediately — this is exactly the
///   behavior this function replaced (`network_monitor.next_change()` raced
///   directly against `run_data_pump` in the same `select!`), just moved
///   into its own task so both shapes can feed the same channel.
/// - **Experimental with a rebinder**: tries `AnyMuxRebinder::rebind`
///   first on every `InterfaceChange` event; only a *failed* rebind attempt
///   is forwarded, and this task then stops (that generation's endpoint is
///   about to be abandoned by the RESUME reconnect the failure triggers, so
///   continuing to watch it is pointless). A `Wake` event (host
///   suspend/resume, `isekai_netmon::ClockSkewWatchdog`) skips the rebind
///   attempt entirely and forwards immediately instead — a local rebind
///   cannot fix a connection that went stale while this host was
///   suspended, so trying one first would only delay the reconnect this
///   task exists to speed up. A *successful* rebind is invisible to the
///   caller's `select!` entirely — `run_data_pump`'s QUIC stream keeps
///   running untouched, because `rebind` only swaps the endpoint's local
///   socket, never the connection/stream objects above it (the same
///   property Android's `multipath_transport.rs` relies on for its own
///   `rebind_abstract()`-based failover, verified there on real hardware).
///   `rebind`'s own success only means "the local socket switch itself
///   succeeded" — not that the new path can actually reach the peer, which
///   this task has no way to confirm; a rebind that succeeds but doesn't
///   restore connectivity eventually surfaces as an ordinary QUIC idle
///   timeout, same as before this feature existed.
///
/// `monitor` is a fresh `isekai_netmon::system_monitor()` from the caller
/// (rather than one long-lived instance shared across every generation)
/// because a rebinder is only valid for the specific endpoint it came from —
/// once a RESUME reconnect replaces that endpoint, the old rebinder (and, by
/// construction, the old task holding it) must not keep running, so each
/// connection generation gets its own task and its own OS registration
/// rather than one shared across the whole `run_resume_loop` call. Taken as
/// a parameter rather than constructed inside this function so tests can
/// inject a controllable mock instead of the real OS-backed one (on this
/// development platform, Linux, a real `AF_NETLINK`-based backend — see
/// `isekai-netmon`'s own module docs).
/// Minimal async rebind interface this function needs — generic (not
/// boxed as `dyn`) so both the real `isekai_transport::AnyMuxRebinder` and
/// this module's own test-only mock can satisfy it. `AnyMuxRebinder` is a
/// plain enum (see its own docs on why: exactly one real backend supports
/// rebinding today, so a trait-object hierarchy would be overkill) with no
/// public constructor for a fake value, so a test that wants to exercise
/// "rebind succeeds"/"rebind fails" without a real `noq` endpoint needs its
/// own minimal seam instead of constructing an `AnyMuxRebinder` directly.
trait Rebindable: Send {
    fn rebind(&self, bind: BindSpec) -> impl std::future::Future<Output = Result<(), isekai_transport::MuxError>> + Send;
}

impl Rebindable for AnyMuxRebinder {
    fn rebind(&self, bind: BindSpec) -> impl std::future::Future<Output = Result<(), isekai_transport::MuxError>> + Send {
        AnyMuxRebinder::rebind(self, bind)
    }
}

fn spawn_reconnect_signal<R: Rebindable + 'static>(
    monitor: Box<dyn isekai_netmon::NetworkChangeMonitor>,
    rebinder: Option<R>,
    experimental_network_rebind: bool,
    helper_addr: std::net::SocketAddr,
    local_bind_port_range: Option<(u16, u16)>,
) -> (tokio::task::JoinHandle<()>, tokio::sync::mpsc::Receiver<()>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<()>(1);
    let handle = tokio::spawn(async move {
        let mut network_monitor = monitor;
        match (experimental_network_rebind, rebinder) {
            (true, Some(rebinder)) => {
                let bind = remote_bind_spec(helper_addr, local_bind_port_range);
                while let Some(event) = network_monitor.next_change().await {
                    if event.cause == isekai_netmon::NetworkChangeCause::Wake {
                        // A rebind only swaps the local socket — it cannot
                        // fix a connection that went stale while this host
                        // was suspended: the peer had the whole wall-clock
                        // gap to idle-time it out and discard the parked
                        // session (`isekai_netmon::ClockSkewWatchdog`'s
                        // docs). Skip straight to a full reconnect, same as
                        // a failed rebind below, instead of first trying —
                        // and waiting out — a rebind that cannot help.
                        log::info!(
                            "isekai-pipe connect: host resume detected; skipping rebind and reconnecting now"
                        );
                        let _ = tx.send(()).await;
                        return;
                    }
                    log::info!("isekai-pipe connect: rebind_attempted");
                    match rebinder.rebind(bind).await {
                        Ok(()) => {
                            log::info!(
                                "isekai-pipe connect: rebind ok, continuing existing connection"
                            );
                        }
                        Err(e) => {
                            log::warn!("isekai-pipe connect: rebind_immediate_error: {e}");
                            let _ = tx.send(()).await;
                            return;
                        }
                    }
                }
            }
            _ => {
                if network_monitor.next_change().await.is_some() {
                    log::info!(
                        "isekai-pipe connect: OS reported a network change; treating the current connection \
                         as stale and reconnecting now instead of waiting for it to time out"
                    );
                    let _ = tx.send(()).await;
                }
            }
        }
    });
    (handle, rx)
}

/// Writes `replay`'s buffered-but-unacknowledged bytes past `committed_offset`
/// onto a freshly (re)established `stream`, then discards them from `replay`
/// on success — shared by both `run_resume_loop`'s `WarmStandby::promote`
/// fast path and its ordinary `reconnect_and_resume` retry loop below, since
/// both hand back a resumed connection with the same "helper says it
/// committed up to X" offset semantics. Returns `false` (leaving `replay`
/// untouched past `committed_offset`) if the write itself fails, so the
/// caller knows to discard `stream` and retry instead of treating it as
/// live.
///
/// Also returns `false` — without touching `replay` at all — when
/// `committed_offset` is outside `replay`'s buffered range
/// (`ReplayBuffer::replay_from` returning `None`): either the helper's
/// claimed offset is *behind* what this client already discarded as
/// confirmed (bytes were dropped without ever actually being acknowledged —
/// data loss), or *ahead* of everything this client has ever sent (the
/// helper claims to have committed bytes that don't exist). Both are
/// protocol inconsistencies, not "nothing to replay" — silently proceeding
/// (as an earlier version of this function did) would desync this client's
/// own offset bookkeeping from the helper's, corrupting every future
/// `client_sent_offset` this session reports (codex review,
/// quicmux-server-resume).
async fn replay_and_advance(replay: &Mutex<C2hReplayBuffer>, committed_offset: u64, stream: &mut AnyByteStream) -> bool {
    let Some(bytes) = replay.lock().unwrap().replay_from(committed_offset) else {
        eprintln!(
            "isekai-pipe connect: helper's committed_offset={committed_offset} is outside the local \
             replay buffer's range — treating this resumed connection as unusable and retrying"
        );
        return false;
    };
    if !bytes.is_empty() {
        match tokio::time::timeout(REPLAY_WRITE_TIMEOUT, stream.write_all(&bytes)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => return false,
        }
    }
    replay.lock().unwrap().advance_start(committed_offset);
    true
}

/// Reestablishes the control stream on `conn` — a freshly resumed connection
/// from either `reconnect_and_resume` or `WarmStandby::promote` — and
/// resumes the `APP_ACK` background exchange against the *same* `counters`
/// this whole `run_resume_loop` call already uses (not a fresh
/// `AppAckCounters`: `pump_c2h`'s backpressure trim reads
/// `counters.c2h_helper_committed_offset()` directly every iteration, so a
/// new instance here would silently desync from what the data pump is
/// actually watching). Without this, `counters.c2h_helper_committed_offset`
/// would freeze at whatever it was when the *first* disconnect happened —
/// `pump_c2h` would then never see it advance again, and the C2H replay
/// buffer would fill to `C2H_REPLAY_BUFFER_CAPACITY` and stall stdin reads
/// (codex review, quicmux-server-resume — the same class of gap already
/// fixed for `isekai-terminal-core`'s three Android transports via
/// `spawn_control_stream_reestablishment_after_resume`, just missed here
/// since this is the separate CLI binary).
///
/// Synchronous (unlike the Android fix's fire-and-forget/timeout-bounded
/// spawn) to match this function's own caller: `connect_via_relay_resumable`
/// already treats the *initial* control stream as a required, synchronous
/// step (`?`, not a best-effort background task) — Android's leniency is
/// specifically about not delaying an SSH shell handoff for a possibly-slow
/// legacy helper, which doesn't apply to reattaching an already-open resume
/// loop against isekai's own server.
async fn reestablish_control_stream(
    conn: &AnyMuxConnection,
    session_secret: &[u8],
    counters: &Arc<AppAckCounters>,
) -> Result<AppAckTasks> {
    let proof = compute_proof(conn, session_secret, b"").await?;
    let control = open_control_stream(conn, &proof).await?;
    Ok(spawn_app_ack_tasks(control.stream, counters.clone()))
}

/// The server clamps our request to its own configured max (or applies its
/// own default when we requested `0`) and echoes back what it actually
/// granted — that, not our own request, is the real deadline: the server
/// will have already discarded the parked session past this point
/// regardless of how long we keep retrying (`ISEKAI_PIPE_DESIGN.md`).
///
/// `0` itself is treated as "no real value was ever learned" rather than a
/// literal zero-second window: `isekai-transport::resume::finish_via_resume`
/// (the `MustResume` ambiguous-attach convergence path) has no ATTACH_HELLO
/// exchange to learn the server's actual grant from, and — even after that
/// function's own fix to fall back to the caller's originally *requested*
/// grace period instead of hardcoding `0` — a caller that itself requested
/// `0` (isekai-ssh/isekai-pipe connect's own "let the server pick its
/// default" convention) still produces `0` here. Without this fallback, any
/// session that ever passed through that convergence path would give up on
/// its very first subsequent disconnect instead of resuming at all (codex
/// review, quicmux-server-resume).
fn resume_window_for(effective_resume_grace_secs: u32) -> Duration {
    match effective_resume_grace_secs {
        0 => DEFAULT_RESUME_WINDOW,
        secs => Duration::from_secs(secs.into()),
    }
}

fn clamp_resume_window(resume_window: Duration, max_resume_window: Option<Duration>) -> Duration {
    max_resume_window.map(|max| resume_window.min(max)).unwrap_or(resume_window)
}

/// `run_resume_loop`'s own resume-window computation, factored out so it has
/// exactly one call site in production code and one in its test (round 3
/// code review, significant finding on the first cut of this test): a test
/// that merely *re-derived* `clamp_resume_window(resume_window_for(...), ...)`
/// inline, rather than calling the same function `run_resume_loop` calls,
/// couldn't actually catch a regression that dropped the clamp from
/// `run_resume_loop` itself — it would keep passing because it never
/// exercises that call site at all.
fn effective_resume_window(effective_resume_grace_secs: u32, max_resume_window: Option<Duration>) -> Duration {
    clamp_resume_window(resume_window_for(effective_resume_grace_secs), max_resume_window)
}

// ── tssh風のライブ再接続表示(`run_resume_loop`専用) ──────────────
//
// `isekai-pipe connect` は `ssh(1)` の ProxyCommand として起動され、OpenSSH の
// 仕様上 stderr は通常 ssh 自身の stderr(＝ユーザーの実端末)にそのまま
// 継承される。tssh(trzsz-ssh)本家のUDPモードreconnectと同じく、stderrに
// 直接 `\r` + ANSI エスケープでその場書き換えするだけで、Android アプリ側
// (`rust-core/src/orchestrator.rs`)のように新しいUI基盤を用意しなくても
// ライブな状態表示ができる。
//
// ただし `isekai-ssh --log-file` 相当が有効な場合、`ssh` の stderr は
// 端末ではなくログファイルへpipeされる(`isekai-ssh/src/wrapper.rs`の
// `log_file::is_enabled()`)。この場合に `\r`/ANSI を出すとログファイルが
// 読めない制御文字だらけになるため、`is_terminal()` で分岐し、非TTY時は
// 改行区切りの平文へフォールバックする。

/// 再接続中の状態メッセージを組み立てる。TTY時は`\r`+ANSI色でその場書き換え
/// 用(呼び出し側で`eprint!`し、改行しない)、非TTY時はログファイル向けの
/// 改行区切り平文(呼び出し側で`eprintln!`する)。副作用を持たない純粋関数
/// として切り出してあり、単体テストしやすい。
fn format_reconnect_status(is_tty: bool, elapsed_secs: u64, total_secs: u64) -> String {
    if is_tty {
        format!(
            "\r\x1b[0;33misekai-pipe connect: connection lost, trying to reconnect... ({elapsed_secs}s/{total_secs}s)\x1b[0m\x1b[K"
        )
    } else {
        format!(
            "isekai-pipe connect: connection lost, trying to reconnect... ({elapsed_secs}s/{total_secs}s elapsed)"
        )
    }
}

fn print_reconnect_status(is_tty: bool, disconnected_at: Instant, resume_window: Duration) {
    let elapsed_secs = Instant::now().saturating_duration_since(disconnected_at).as_secs();
    let msg = format_reconnect_status(is_tty, elapsed_secs, resume_window.as_secs());
    if is_tty {
        eprint!("{msg}");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    } else {
        eprintln!("{msg}");
    }
}

fn print_reconnect_success(is_tty: bool, session_id: isekai_transport::SessionId) {
    if is_tty {
        eprintln!("\r\x1b[0;32misekai-pipe connect: reconnected.\x1b[0m\x1b[K");
    } else {
        eprintln!("isekai-pipe connect: reconnected.");
    }
    notify_os("isekai-pipe connect", &format!("Reconnected (session_id={session_id})."));
}

/// Best-effort OS-level notification (Windows toast / Linux desktop
/// notification via D-Bus / macOS notification) for the two reconnect
/// events a user should notice even if they're not looking at the terminal
/// right now: a successful reconnect, and giving up entirely. This is a
/// lightweight stand-in for a full tssh-style in-terminal overlay (a
/// separate, larger follow-up) — deliberately not called for every
/// individual backoff retry or transient failure, which would just spam
/// notifications for what's usually a self-healing blip. Any failure
/// (no notification daemon, headless environment, ...) is silently
/// ignored — exactly like `isekai-ssh/src/log_file.rs`'s own philosophy,
/// this must never be able to affect the connection itself.
fn notify_os(summary: &str, body: &str) {
    let _ = notify_rust::Notification::new().summary(summary).body(body).show();
}

/// TTY時のみ呼ばれる: 1回のバックオフ待機(`delay`)を最大1秒刻みに分割し、
/// 都度その場書き換えでカウントダウンを再描画する。`delay`全体を素通しで
/// 待つのと合計の待ち時間は変わらない(`RESUME_BACKOFF`/`deadline`の意味は
/// 変えない、表示だけの変更)。
/// タイミング(何回・どれだけ待つか)と実際の描画処理(`on_tick`)を分離してある
/// ―― `print_reconnect_status`が直接I/Oを行うため、タイミングだけを
/// `tokio::time::pause()`で決定的にテストできるようにするため。
async fn sleep_with_live_status(delay: Duration, mut on_tick: impl FnMut()) {
    // `tokio::time::Instant`を使う(`std::time::Instant`ではない) —
    // `tokio::time::pause()`/`advance()`が影響するのはtokio自身の時計だけで、
    // OSの実時計(`std::time::Instant::now()`)は素通りする。混在させると
    // テストでpause中に`remaining`がほぼ縮まらずビジーループする
    // (実際にこの取り違えで発生した不具合、テストで検出)。
    let wake_at = tokio::time::Instant::now() + delay;
    loop {
        let remaining = wake_at.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(remaining.min(Duration::from_secs(1))).await;
        on_tick();
    }
}

/// One backoff wait inside [`resume_with_backoff_until_deadline`]'s retry
/// loop: sleeps out `delay` (via `sleep_with_live_status` when `is_tty`,
/// ticking `on_tick`) — but returns early the moment `network_monitor`
/// reports a fresh OS network-change event, since that's a concrete signal
/// worth retrying on immediately rather than sitting out the rest of a
/// blind backoff. `tokio::select!`'s pattern-match branch form leaves the
/// monitor branch disabled (never fires again) for the rest of *this* call
/// if the monitor ever yields `None` (permanently stopped) — that call just
/// falls back to the plain timeout, no extra bookkeeping needed here.
async fn wait_backoff_or_network_change(
    delay: Duration,
    is_tty: bool,
    mut on_tick: impl FnMut(),
    network_monitor: &mut dyn isekai_netmon::NetworkChangeMonitor,
) {
    tokio::select! {
        _ = async {
            if is_tty {
                sleep_with_live_status(delay, &mut on_tick).await;
            } else {
                tokio::time::sleep(delay).await;
            }
        } => {}
        Some(_) = network_monitor.next_change() => {
            log::info!(
                "isekai-pipe connect: OS reported another network change while backing off; \
                 retrying immediately instead of waiting out the remaining backoff"
            );
        }
    }
}

/// The mutable, session-scoped state `run_resume_loop`'s two extracted
/// helpers (`promote_warm_standby_once`/`resume_with_backoff_until_deadline`)
/// both need to read and update across a disconnect — grouped here so the
/// two helpers take one `&mut` parameter instead of five separate ones.
struct ResumeLoopState {
    session_id: isekai_transport::SessionId,
    counters: Arc<AppAckCounters>,
    replay: Arc<Mutex<C2hReplayBuffer>>,
    app_ack_tasks: AppAckTasks,
    network_rebinder: Option<AnyMuxRebinder>,
    /// tssh風のライブ再接続表示(`format_reconnect_status`等)を使うかどうか。
    /// プロセスの生存期間中に変わることは無いのでループ開始前に1回だけ判定する。
    is_tty: bool,
    /// 直近の再接続試行(promote/backoffいずれも)が失敗した理由。ギブアップ
    /// メッセージに"Last error: ..."として付け足す。再接続成功のたびに
    /// `None`へリセットされる。
    last_resume_error: Option<String>,
    /// `UnknownSession`が何回連続で返ってきたか。サーバ側`RESUME`ハンドラ
    /// (`isekai-pipe serve`側`engine/mod.rs::handle_resume`相当)は、
    /// 「session_idがテーブルに無い」(本当に消滅)だけでなく「テーブルには
    /// あるがまだparkされていない」(直前のdata streamのresetをまだ処理し
    /// 終えていない一時的な状態、`parked_tcp == None`)や「fencing slotが
    /// 一致しない」場合も同じ`UnknownToken`/`UnknownSession`を返す
    /// (ワイヤに複数の意味を1値へ潰しているため区別できない)。1回だけで
    /// 即terminal扱いすると、この一時的なraceを本当に消滅したものと誤認して
    /// 本来resumeできたはずのセッションを早まって諦めてしまう
    /// (Codexレビューで指摘、実際に`engine/mod.rs`の該当箇所を確認して
    /// 再現条件を特定した)。`resume_with_backoff_until_deadline`はこの
    /// カウンタが`UNKNOWN_SESSION_CONFIRM_THRESHOLD`に達したときだけ
    /// give upする——「毎回同じ確定的signalが返り続けている」ことでしか
    /// 本物の消滅とは区別できないため。
    consecutive_unknown_session: u32,
}

/// Fast path: promote the already-warm standby connection instead of
/// waiting through `resume_with_backoff_until_deadline`'s backoff loop — the
/// entire point of keeping one warm (`warm_standby.rs`'s module docs).
/// Returns `Some(stream)` only once promotion, replay, and (best-effort)
/// control-stream re-establishment have all been attempted; a missing
/// standby, an in-flight promotion, a transport failure, or a replay
/// mismatch all return `None`, and every `None` here falls straight through
/// to the caller's ordinary `reconnect_and_resume` retry loop unchanged —
/// this is a latency optimization, not a correctness dependency. On
/// success, clears `state.network_rebinder`: the promoted connection was
/// dialed directly by `WarmStandby`, not via the endpoint this generation's
/// rebinder came from, so there is no rebinder to carry over — the next
/// disconnect just falls back to a full resume, same as any other
/// rebinder-less generation.
async fn promote_warm_standby_once(
    warm_standby: &isekai_transport::WarmStandby,
    target: &RelayTarget,
    state: &mut ResumeLoopState,
) -> Option<AnyByteStream> {
    let client_sent_offset = C2hSentOffset::new(state.replay.lock().unwrap().end_offset());
    let client_delivered_offset = H2cClientDeliveredOffset::new(state.counters.h2c_client_delivered_offset());
    let mut promoted = match warm_standby.promote(client_sent_offset, client_delivered_offset).await {
        Ok(promoted) => promoted,
        Err(e) => {
            let msg = format!("{e:#}");
            log::info!("isekai-pipe connect: warm-standby promote unavailable ({msg}); falling back to full resume");
            state.last_resume_error = Some(msg);
            return None;
        }
    };
    if !replay_and_advance(&state.replay, promoted.helper_committed_offset.get(), &mut promoted.data_stream).await {
        let msg = "warm-standby promote succeeded but replay failed; falling back to full resume";
        // TTY時はその場書き換え中のライブ表示行を壊さないよう、まず行を
        // クリアしてから改行付きで出す(このメッセージ自体は1episodeにつき
        // 最大1回で、per-attempt的な連発ではないためdebugログへは落とさない)。
        if state.is_tty {
            eprintln!("\r\x1b[Kisekai-pipe connect: {msg}");
        } else {
            eprintln!("isekai-pipe connect: {msg}");
        }
        state.last_resume_error = Some(msg.to_string());
        return None;
    }
    log::info!("isekai-pipe connect: promoted warm-standby connection for session_id={}", state.session_id);
    print_reconnect_success(state.is_tty, state.session_id);
    match reestablish_control_stream(&promoted.connection, &target.session_secret, &state.counters).await {
        Ok(new_tasks) => state.app_ack_tasks = new_tasks,
        Err(e) => eprintln!(
            "isekai-pipe connect: control stream re-establishment after promote failed ({e:#}), \
             continuing without resume support until the next reattach"
        ),
    }
    drop(promoted.connection);
    state.network_rebinder = None;
    Some(promoted.data_stream)
}

/// The ordinary `reconnect_and_resume` retry loop, run until either a resume
/// attempt succeeds or `deadline` passes. Returns `Ok(stream)` on success
/// (having also re-established the control stream and updated
/// `state.network_rebinder`); returns `Err` once `deadline` has passed,
/// having printed a final give-up message and aborted `warm_standby_task`
/// (Epic R PR1, B6: no longer closes `stdout` itself — see `give_up`'s own
/// docs for why) — the caller propagates this `Err` (e.g. via `?`) so it
/// eventually reaches
/// `connect_command`'s `Err` arm and `write_connect_outcome_for_wrapper`
/// classifies it as `ConnectOutcomeClass::Unreachable`, letting `isekai-ssh`'s
/// wrapper auto-retry (`.claude/rules/always-connects.md`) instead of the
/// give-up silently looking like a clean exit to everything downstream of
/// this function.
///
/// Each backoff wait races against `network_monitor.next_change()`: unlike
/// `spawn_reconnect_signal` (which only watches while a connection is
/// actually up, to detect the *first* disconnect early), this is watched
/// while already disconnected and retrying, so a fresh OS network-change
/// event (e.g. the new interface/route finishing DHCP after the earlier
/// disconnect) cuts the remaining backoff short and retries immediately
/// instead of blindly waiting out `RESUME_BACKOFF`. `network_monitor` is a
/// fresh instance the caller creates per disconnect episode (mirroring
/// `spawn_reconnect_signal`'s own one-per-generation rule) — passed in
/// rather than constructed here so tests can inject a controllable mock.
/// How many *consecutive* `UnknownSession` rejections
/// `resume_with_backoff_until_deadline` requires before treating the
/// session as genuinely, permanently gone — see `is_unknown_session_rejection`'s
/// docs for why a single occurrence isn't proof enough.
const UNKNOWN_SESSION_CONFIRM_THRESHOLD: u32 = 3;

/// Minimum time since disconnect that must have elapsed before
/// `UNKNOWN_SESSION_CONFIRM_THRESHOLD` consecutive rejections are trusted as
/// proof of permanent loss, required *in addition to* the streak count
/// above. At `RESUME_BACKOFF`'s schedule (500ms, 1s, 2s, ...) 3 consecutive
/// attempts land around t≈3.5s — comfortably inside `isekai-pipe serve
/// --idle-timeout`'s default (15s) worst case for the "not-yet-parked"
/// race `is_unknown_session_rejection` describes: a "break-before-make"
/// roam (Wi-Fi drop, AP switch, airplane mode) means the client's
/// `quic_write.reset(0)` on the *old* path never reaches the server, so the
/// server can only notice the old connection died via its own QUIC idle
/// timeout, not the explicit reset — the fast path this streak was tuned
/// around. Without this floor, `UNKNOWN_SESSION_CONFIRM_THRESHOLD` alone
/// would misfire and kill exactly the roaming reconnects this project's
/// resumable transport exists to survive (`CLAUDE.md`'s differentiator:
/// "QUIC接続耐性(ローミング...)"). 30s is double the idle-timeout default,
/// leaving margin without meaningfully eating into the (now days-long)
/// deadline a truly-dead session would otherwise be retried against.
const UNKNOWN_SESSION_MIN_ELAPSED_FLOOR: Duration = Duration::from_secs(30);

/// `UnknownSession` is the wire reason for three different server-side
/// situations that `isekai-pipe serve`'s `RESUME` handler
/// (`engine/mod.rs`, roughly `sessions.get()` returning `None`, *or*
/// `parked_tcp` still being `None`, *or* the `AttachArbiter` established
/// lease not matching) all collapse into the same
/// `quicmux::ResumeRejectReason::UnknownToken` value — only the first of
/// those means "this session_id will never resume again"; the other two are
/// transient races (the old data stream's reset hasn't finished being
/// processed and parked yet) that a subsequent attempt, moments later, can
/// still recover from. Since the wire protocol can't currently tell these
/// apart, a *single* `UnknownSession` is not reliable proof of permanent
/// loss (confirmed by reading `engine/mod.rs`'s `RESUME` handler directly,
/// per a Codex review finding) — only `UNKNOWN_SESSION_CONFIRM_THRESHOLD`
/// consecutive occurrences are treated as such by the caller.
/// `Auth`/`OffsetGone` and any non-rejection `TransportError` (network/mux
/// failures) are left to the existing deadline-bound retry loop unchanged —
/// those are rejections of a *specific attempt*, not proof the session
/// itself is gone, so this function deliberately doesn't guess at their
/// retriability.
fn is_unknown_session_rejection(e: &isekai_transport::TransportError) -> bool {
    matches!(
        e,
        isekai_transport::TransportError::ResumeRejected(isekai_transport::ResumeRejectReason::UnknownSession)
    )
}

/// Pure decision core of the `UnknownSession` streak-tracking described on
/// `ResumeLoopState::consecutive_unknown_session`'s docs, factored out so it
/// can be unit-tested without a real `AnyMuxFactory`/network dial (unlike
/// `resume_with_backoff_until_deadline` itself). Given the streak length
/// going into this attempt, whether this attempt's error was an
/// `UnknownSession` rejection, and how long it's been since the disconnect
/// that started this episode, returns the streak length coming out of it
/// and whether the caller should give up now — both the streak count *and*
/// `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR` must be satisfied (see that
/// constant's docs for why the streak alone isn't enough).
fn update_unknown_session_streak(previous_streak: u32, is_unknown_session: bool, elapsed_since_disconnect: Duration) -> (u32, bool) {
    if !is_unknown_session {
        return (0, false);
    }
    let streak = previous_streak.saturating_add(1);
    let should_give_up = streak >= UNKNOWN_SESSION_CONFIRM_THRESHOLD && elapsed_since_disconnect >= UNKNOWN_SESSION_MIN_ELAPSED_FLOOR;
    (streak, should_give_up)
}

/// Shared give-up cleanup for `resume_with_backoff_until_deadline`'s two
/// terminal paths (deadline exceeded, or a definitive `UnknownSession`
/// rejection): clears the live TTY status line, prints one final message,
/// and stops the warm-standby probe task.
///
/// Deliberately does **not** close `stdout` itself (Epic R PR1, B6): this
/// function returns into an `Err` that propagates all the way up through
/// `run_connect` to `connect_command`, which writes the `ConnectOutcome`
/// side-channel file (`write_connect_outcome_for_wrapper`) *before* the
/// process exits — and only the process exiting closes `stdout`, well
/// after that write. An explicit `stdout.shutdown()` here used to race
/// that write: `ssh(1)` sees EOF and can exit, and the wrapper's
/// `claim_connect_outcome` can run, before the outcome file has actually
/// landed on disk, silently downgrading a recoverable failure into
/// `NoRecoverableSignal`. The invariant this relies on — "the outcome file
/// is written before stdout is ever closed" — must keep holding; don't
/// reintroduce an explicit close here without re-verifying it.
///
/// The Windows-native connect path (`native/connect.rs`, `connect_attempt`)
/// has an analogous but distinct race: it guards against dropping its
/// `isekai-pipe connect` `Child` (whose `kill_on_drop` would then kill the
/// child) before that child finishes writing its own outcome file, via a
/// short (~1s) `tokio::time::timeout` grace period on `child.wait()` rather
/// than by reordering writes-vs-closes the way this function does. Different
/// mechanism, same underlying concern — don't assume fixing one side fixes
/// the other.
fn give_up(is_tty: bool, warm_standby_task: &Option<tokio::task::JoinHandle<()>>, message: &str) {
    if is_tty {
        // その場書き換え中だったライブ表示行をクリアしてから
        // ギブアップメッセージを改行付きで出す。
        eprint!("\r\x1b[K");
    }
    eprintln!("{message}");
    if let Some(t) = warm_standby_task {
        t.abort();
    }
}

/// `max_resume_window` is the STUN-only client-side clamp already applied
/// by `run_resume_loop`; when present, this helper also suppresses desktop
/// give-up notifications so repeated lightweight STUN retries do not spam
/// the user.
async fn resume_with_backoff_until_deadline(
    factory: &AnyMuxFactory,
    target: &RelayTarget,
    profile: &str,
    resume_window: Duration,
    disconnected_at: Instant,
    deadline: Instant,
    state: &mut ResumeLoopState,
    warm_standby_task: &Option<tokio::task::JoinHandle<()>>,
    network_monitor: &mut dyn isekai_netmon::NetworkChangeMonitor,
    max_resume_window: Option<Duration>,
) -> Result<AnyByteStream> {
    let notify_on_give_up = max_resume_window.is_none();
    let mut attempt: u32 = 0;
    loop {
        let now = Instant::now();
        if now >= deadline {
            let exceeded_by = now.saturating_duration_since(deadline);
            let last_error_suffix = state
                .last_resume_error
                .as_deref()
                .map(|e| format!(" Last error: {e}."))
                .unwrap_or_default();
            let session_id = state.session_id;
            give_up(
                state.is_tty,
                warm_standby_task,
                &format!(
                    "isekai-pipe connect: giving up on session_id={session_id} for '{profile}' - \
                     the resume window ({resume_window:?}) was exceeded by {exceeded_by:?}.{last_error_suffix} \
                     Ending this connect attempt; ssh will treat this as a lost connection.",
                ),
            );
            if notify_on_give_up {
                notify_os(
                    "isekai-pipe connect",
                    &format!("Giving up reconnecting to '{profile}' (session_id={session_id}).{last_error_suffix}"),
                );
            }
            return Err(anyhow::anyhow!(
                "resume window ({resume_window:?}) exceeded by {exceeded_by:?} for session_id={session_id}\
                 for '{profile}'.{last_error_suffix}"
            ));
        }

        let delay = RESUME_BACKOFF.delay_for_attempt(attempt, &mut rand::thread_rng()).min(deadline - now);
        attempt = attempt.saturating_add(1);
        wait_backoff_or_network_change(
            delay,
            state.is_tty,
            || print_reconnect_status(true, disconnected_at, resume_window),
            network_monitor,
        )
        .await;

        let client_sent_offset = C2hSentOffset::new(state.replay.lock().unwrap().end_offset());
        let client_delivered_offset = H2cClientDeliveredOffset::new(state.counters.h2c_client_delivered_offset());
        match reconnect_and_resume(
            factory,
            target,
            state.session_id,
            client_sent_offset,
            client_delivered_offset,
        )
        .await
        {
            Ok(mut resumed) => {
                // A successful RESUME_ACK is proof the session was known and
                // parked — whatever streak of `UnknownSession` preceded it
                // (if any) was the transient not-yet-parked race, not
                // genuine loss.
                state.consecutive_unknown_session = 0;
                if !replay_and_advance(&state.replay, resumed.helper_committed_offset.get(), &mut resumed.data_stream).await {
                    // resume自体は成功したがreplayが不整合 —実質「この試行は
                    // 失敗した」ので、既存のErr(e)アームと同じTTY/非TTY分岐・
                    // last_resume_error更新を行う(codexレビューで指摘: この
                    // continue経路だけ元々何も表示せずlast_resume_errorも
                    // 更新していなかった)。
                    let msg = "resume succeeded but replay failed".to_string();
                    if state.is_tty {
                        log::debug!("isekai-pipe connect: resume attempt {attempt} {msg}");
                    } else {
                        eprintln!("isekai-pipe connect: resume attempt {attempt} {msg}");
                    }
                    state.last_resume_error = Some(msg);
                    continue;
                }
                print_reconnect_success(state.is_tty, state.session_id);
                match reestablish_control_stream(&resumed.connection, &target.session_secret, &state.counters).await {
                    Ok(new_tasks) => state.app_ack_tasks = new_tasks,
                    Err(e) => eprintln!(
                        "isekai-pipe connect: control stream re-establishment after resume failed ({e:#}), \
                         continuing without resume support until the next reattach"
                    ),
                }
                drop(resumed.connection);
                state.network_rebinder = resumed.network_rebinder;
                return Ok(resumed.data_stream);
            }
            Err(e) => {
                // See `is_unknown_session_rejection`'s docs: a single
                // occurrence isn't reliable proof the session is gone for
                // good (it's also what a transient not-yet-parked race on
                // the server looks like), so only give up once it's been
                // confirmed `UNKNOWN_SESSION_CONFIRM_THRESHOLD` times in a
                // row *and* `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR` has passed
                // (see that constant's docs — the streak alone misfires
                // during break-before-make roaming). Any other outcome
                // (success, or a *different* error) resets the streak
                // elsewhere in this loop.
                let (streak, should_give_up) = update_unknown_session_streak(
                    state.consecutive_unknown_session,
                    is_unknown_session_rejection(&e),
                    Instant::now().saturating_duration_since(disconnected_at),
                );
                state.consecutive_unknown_session = streak;
                if should_give_up {
                    let session_id = state.session_id;
                    give_up(
                        state.is_tty,
                        warm_standby_task,
                        &format!(
                            "isekai-pipe connect: giving up on session_id={session_id} for '{profile}' - \
                             the server no longer knows this session ({UNKNOWN_SESSION_CONFIRM_THRESHOLD} \
                             consecutive UnknownSession rejections; reclaimed, or the server itself \
                             restarted), retrying would never succeed. Ending this connect attempt; \
                             ssh will treat this as a lost connection.",
                        ),
                    );
                    if notify_on_give_up {
                        notify_os(
                            "isekai-pipe connect",
                            &format!("Giving up reconnecting to '{profile}' (session_id={session_id}): server no longer knows this session."),
                        );
                    }
                    return Err(anyhow::anyhow!(
                        "server no longer recognizes session_id={session_id} for '{profile}' (UnknownSession); \
                         retrying would never succeed."
                    ));
                }
                let msg = format!("{e:#}");
                // TTY時はその場書き換えのライブ表示とスクロール表示が混ざると
                // UXを壊すため、個々の失敗はdebugログへ格下げする(既定の
                // `info`フィルタでは表示されない、`RUST_LOG=debug`で見られる)。
                // 非TTY(ログファイル等)では引き続きeprintln!のまま残す —
                // ログでは個々の失敗を追えることの方が重要なため。
                if state.is_tty {
                    log::debug!("isekai-pipe connect: resume attempt {attempt} failed: {msg}");
                } else {
                    eprintln!("isekai-pipe connect: resume attempt {attempt} failed: {msg}");
                }
                state.last_resume_error = Some(msg);
            }
        }
    }
}

/// The EOF-latch decision (Task 2.9 / issue #111): whether `run_resume_loop`
/// should give up outright rather than attempt a resume, given one
/// `run_data_pump` outcome. `true` for `PumpFailure::Local` unconditionally,
/// and for a `PumpFailure::Remote` that arrived *after* `pump_c2h` already
/// reached a clean stdin EOF in the same generation (`c2h_already_done`) —
/// both cases reach the identical conclusion that no local producer is left
/// to resume for. Extracted as its own pure function specifically so it's
/// directly unit-testable without needing a real pump/QUIC connection (see
/// the `tests` module below).
fn should_give_up_without_resuming(outcome: &Result<(), PumpFailure>, c2h_already_done: bool) -> bool {
    match outcome {
        Ok(()) => false,
        Err(PumpFailure::Local(_)) => true,
        Err(PumpFailure::Remote(_)) => c2h_already_done,
    }
}

pub(crate) async fn run_resume_loop(
    factory: &AnyMuxFactory,
    target: &RelayTarget,
    profile: &str,
    established: isekai_transport::ResumableRelaySession,
    experimental_network_rebind: bool,
    tethering_interface: Option<isekai_transport::InterfaceIndex>,
    max_resume_window: Option<Duration>,
) -> Result<()> {
    let session_id = established.session_id;
    drop(established.connection);

    let resume_window = effective_resume_window(established.effective_resume_grace_secs, max_resume_window);

    let counters = Arc::new(AppAckCounters::new());
    let mut state = ResumeLoopState {
        session_id,
        app_ack_tasks: spawn_app_ack_tasks(established.control_stream, counters.clone()),
        counters,
        replay: Arc::new(Mutex::new(C2hReplayBuffer::new(C2H_REPLAY_BUFFER_CAPACITY))),
        network_rebinder: established.network_rebinder,
        // tssh風のライブ再接続表示(このループ内でのみ使う、詳細は
        // `format_reconnect_status`周辺のモジュールドキュメント参照)。
        is_tty: std::io::stderr().is_terminal(),
        last_resume_error: None,
        consecutive_unknown_session: 0,
    };

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut data_stream = established.data_stream;
    let mut disconnected_since: Option<Instant> = None;

    // `--tethering-interface`: keeps a second connection warm on a specific
    // physical interface and promotes it (no fresh dial, no backoff wait) as
    // the first thing tried on disconnect, below — see `warm_standby.rs`'s
    // module docs. `None` when the flag wasn't given; every use below is a
    // no-op in that case, matching this codebase's "opportunistic,
    // default-off" convention for experimental features.
    let warm_standby = tethering_interface
        .map(|iface| Arc::new(isekai_transport::WarmStandby::new_bound_to_interface(factory.clone(), target.clone(), session_id, iface)));
    let warm_standby_task = warm_standby.clone().map(|ws| {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(WARM_STANDBY_PROBE_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Wall-clock (not monotonic — see `WARM_STANDBY_SUSPEND_JUMP_FACTOR`'s
            // docs) timestamp of the previous tick, to detect a host suspend/
            // resume spanning this interval.
            let mut last_wall = std::time::SystemTime::now();
            loop {
                interval.tick().await;
                let now = std::time::SystemTime::now();
                if now.duration_since(last_wall).map(|elapsed| elapsed > WARM_STANDBY_PROBE_INTERVAL * WARM_STANDBY_SUSPEND_JUMP_FACTOR).unwrap_or(false) {
                    log::info!(
                        "isekai-pipe connect: wall clock jumped further than this warm-standby interval allows for \
                         (likely a host suspend/resume); discarding the standby connection rather than trusting its \
                         own probe (ADR_SLEEP_RESUME_MUX_OWNER_DEATH.md D-3)"
                    );
                    ws.invalidate().await;
                }
                last_wall = now;
                if let Err(e) = ws.ensure_warm().await {
                    log::warn!("isekai-pipe connect: warm-standby ensure_warm failed: {e:#}");
                }
            }
        })
    });

    // `c2h_already_done` lives here — outside the `loop` below, and never
    // reset back to `false` once set — not just outside the inner
    // `select!`. It's a permanent property of this process's own stdin
    // (once the underlying fd hits EOF, every subsequent read sees EOF
    // again too), so re-deriving it fresh each generation would only be
    // correct if `pump_c2h`'s read were guaranteed to be polled at least
    // once before any sibling future could fail first in the *next*
    // generation's `select!` — `tokio::select!` polls its branches in
    // random order for fairness, so that's not guaranteed. Declaring it
    // once, set-only-to-true, sidesteps needing that guarantee (found by
    // adversarial review, 2026-09-02, N3's follow-up).
    let mut c2h_already_done = false;
    loop {
        // See `spawn_reconnect_signal`'s docs for the full design rationale
        // (this replaces what used to be a single `network_monitor` shared
        // across the whole loop, racing `run_data_pump` directly — that
        // shape cancelled the data pump, and the QUIC stream halves split
        // out of `data_stream` below with it, the instant *any* network
        // change fired, leaving no way to try a fast rebind without losing
        // the stream first).
        let (reconnect_signal_task, mut reconnect_signal_rx) = spawn_reconnect_signal(
            isekai_netmon::system_monitor(),
            state.network_rebinder.take(),
            experimental_network_rebind,
            target.helper_addr,
            target.local_bind_port_range,
        );

        let (mut quic_read, mut quic_write) = data_stream.split();
        // Passed to `run_data_pump` as `&mut` (not as part of its own
        // return value) specifically so it survives `run_data_pump`'s
        // future being cancelled when the `reconnect_signal_rx` branch wins
        // instead — see `run_data_pump`'s docs (N3).
        let outcome = tokio::select! {
            result = run_data_pump(&mut stdin, &mut stdout, &mut quic_read, &mut quic_write, &state.replay, &state.counters, &mut c2h_already_done) => result,
            Some(()) = reconnect_signal_rx.recv() => {
                Err(PumpFailure::Remote(anyhow::anyhow!("network change detected, reconnecting")))
            }
        };
        reconnect_signal_task.abort();
        state.app_ack_tasks.abort();

        let give_up = should_give_up_without_resuming(&outcome, c2h_already_done);
        match (outcome, give_up) {
            (Ok(()), _) => {
                if let Some(t) = &warm_standby_task {
                    t.abort();
                }
                return Ok(());
            }
            (Err(e), true) => {
                // Deliberately does *not* reset `quic_write` (contrast the
                // unconditional reset below) — an explicit `shutdown()`
                // instead, matching `pump_c2h`'s own stdin-EOF path, queues
                // a clean FIN via the underlying transport rather than the
                // reset's error signal. A reset here would make the server
                // park this session for the full resume grace
                // (`RelayOutcome::DataStreamDied`) for a connection nothing
                // will ever resume — see `ParentGoneSignal`'s docs.
                //
                // NOTE: `shutdown()` only *queues* the FIN — verified
                // against noq's actual `poll_shutdown` (adversarial review,
                // 2026-09-02, N2), it resolves `Poll::Ready` immediately
                // without waiting for the connection driver to actually put
                // the FIN on the wire, so `.await`ing it doesn't even yield
                // to the scheduler. It is *not* what makes this path
                // reliable — it's kept for parity with `pump_c2h`'s pattern
                // and because queuing the right frame (FIN, not reset) is
                // still necessary, just not sufficient. The actual flush
                // opportunity is the bounded sleep `connect_command`
                // performs once this error (tagged `ParentGoneSignal`)
                // reaches it, regardless of whether it came from here or
                // from the watchdog directly — seeing this comment without
                // that sleep in place means the fix regressed.
                let _ = quic_write.shutdown().await;
                if let Some(t) = &warm_standby_task {
                    t.abort();
                }
                let (PumpFailure::Local(inner) | PumpFailure::Remote(inner)) = e;
                return Err(inner.context(ParentGoneSignal).context(
                    "isekai-pipe connect: giving up rather than attempting resume — either local \
                     stdin/stdout I/O failed directly, or the remote side failed after the local \
                     side (stdin) already reached a clean EOF; either way, the process on the \
                     other end of this pipe (most likely ssh(1)) has nothing left to resume for",
                ));
            }
            (Err(_), _) => {}
        }

        // Abandoning this connection (network change, or run_data_pump's own
        // remote-side I/O failure with no already-observed local EOF — the
        // give-up cases above already returned) — explicitly reset the send
        // side instead of letting it drop gracefully. `noq`/`qmux`'s `Drop`
        // for a send stream calls `finish()` (a clean FIN) by default, which
        // `isekai-pipe serve`'s `relay_buffered` cannot distinguish from a
        // legitimate half-close (e.g. stdin EOF, where S→C must keep
        // flowing) — so it leaves the session `Established`-but-never-parked
        // on this now-dead connection instead of parking it for resume, and
        // every subsequent RESUME then fails as "not resumable"
        // (`UnknownSession`) forever (found via live debugging, 2026-07-11:
        // the very next reconnect attempt after a network-change-triggered
        // abandon got exactly this rejection). A reset instead makes the
        // server's read return an error, correctly classified as
        // `RelayOutcome::DataStreamDied` and parked.
        quic_write.reset(0);

        // The resume window's clock starts here, at disconnect detection —
        // before the fast-path promote attempt below, not after it, so a
        // slow-to-fail promote still counts against the deadline the same as
        // a slow-to-fail `reconnect_and_resume` attempt would.
        let disconnected_at = *disconnected_since.get_or_insert_with(Instant::now);
        let deadline = disconnected_at + resume_window;
        // tssh風のライブ再接続表示: 切断検知の瞬間に即座に1回出す(これが
        // 無いと、最初の再接続試行が失敗するまで何も表示されない)。
        print_reconnect_status(state.is_tty, disconnected_at, resume_window);

        let promoted_stream = match &warm_standby {
            Some(ws) => promote_warm_standby_once(ws, target, &mut state).await,
            None => None,
        };

        let new_stream = match promoted_stream {
            Some(stream) => stream,
            None => {
                // Fresh per disconnect episode, same one-registration-per-
                // generation rule as `spawn_reconnect_signal`'s own monitor
                // — this one just watches for a *later* network change
                // while already backing off, not the first one that got us
                // here.
                let mut backoff_network_monitor = isekai_netmon::system_monitor();
                resume_with_backoff_until_deadline(
                    factory,
                    target,
                    profile,
                    resume_window,
                    disconnected_at,
                    deadline,
                    &mut state,
                    &warm_standby_task,
                    &mut *backoff_network_monitor,
                    max_resume_window,
                )
                .await?
            }
        };

        data_stream = new_stream;
        disconnected_since = None;
        state.last_resume_error = None;
    }
}

/// Runs both pump directions until either both finish cleanly or one fails.
///
/// `c2h_already_done` is the **EOF-latch**'s state: set to `true` the
/// instant `pump_c2h` itself resolves `Ok` (a clean stdin EOF), and never
/// otherwise. Deliberately an out-parameter (`&mut bool`), not a second
/// element of this function's own return value — a round of adversarial
/// review found that shape (found 2026-09-02, N3) silently discards the
/// latch: `run_resume_loop`'s outer `select!` also races
/// `reconnect_signal_rx.recv()` against this whole function, and when
/// *that* branch wins, this function's future — including whatever it
/// would have returned — is simply dropped, never producing a value at
/// all. Writing through `&mut bool` instead means the mutation already
/// happened, synchronously, at the moment `pump_c2h` finished — regardless
/// of whether this function's *own* future is later cancelled by the
/// sibling branch winning the outer race — so the caller's copy of the flag
/// is correct either way. See `PumpFailure`'s docs for why the latch
/// matters: a `Remote` failure (from `pump_h2c`, or the network-change
/// signal) arriving after `pump_c2h` already cleanly finished means no
/// local producer is left to resume for either way, the same conclusion a
/// `Local` failure reaches directly.
async fn run_data_pump(
    stdin: &mut (impl AsyncRead + Unpin),
    stdout: &mut (impl AsyncWrite + Unpin),
    quic_read: &mut AnyByteStreamReadHalf,
    quic_write: &mut AnyByteStreamWriteHalf,
    replay: &Arc<Mutex<C2hReplayBuffer>>,
    counters: &Arc<AppAckCounters>,
    c2h_already_done: &mut bool,
) -> Result<(), PumpFailure> {
    let c2h_fut = pump_c2h(stdin, quic_write, replay.clone(), counters.clone());
    let h2c_fut = pump_h2c(quic_read, stdout, counters.clone());
    tokio::pin!(c2h_fut);
    tokio::pin!(h2c_fut);

    let mut h2c_done = false;
    loop {
        tokio::select! {
            res = &mut c2h_fut, if !*c2h_already_done => {
                res?;
                *c2h_already_done = true;
            }
            res = &mut h2c_fut, if !h2c_done => {
                res?;
                h2c_done = true;
            }
        }
        if *c2h_already_done && h2c_done {
            return Ok(());
        }
    }
}

async fn pump_c2h(
    stdin: &mut (impl AsyncRead + Unpin),
    quic_write: &mut AnyByteStreamWriteHalf,
    replay: Arc<Mutex<C2hReplayBuffer>>,
    counters: Arc<AppAckCounters>,
) -> Result<(), PumpFailure> {
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
        // Reading our own stdin failing is a local-side failure (see
        // `PumpFailure`'s docs) — distinct from `write_all`/`append` below,
        // which fail only on the QUIC/remote side.
        let n = stdin
            .read(&mut buf[..read_len])
            .await
            .context("reading stdin failed")
            .map_err(PumpFailure::Local)?;
        if n == 0 {
            let _ = quic_write.shutdown().await;
            return Ok(());
        }
        quic_write
            .write_all(&buf[..n])
            .await
            .context("writing to remote stream failed")
            .map_err(PumpFailure::Remote)?;
        // `read_len`が`remaining_capacity()`で頭打ちにしてあり、`advance_start`は
        // 空きを増やすことしかしないため、ここが`false`になることは無い。それでも
        // 握り潰さないのは、もし起きた場合の被害が「replayバッファに載らないまま
        // QUICへ送出済みのバイトができる」=`end_offset()`由来の
        // `client_sent_offset`がhelper側とずれる、というresume不能状態だから
        // (`.claude/rules/always-connects.md`)。接続ごと畳んでしまえば
        // `run_resume_loop`が再接続からやり直せるので、そちらの方が安全側に倒れる
        // (resume自体を試す価値がある失敗なのでRemote扱い)。
        if !replay.lock().unwrap().append(&buf[..n]) {
            return Err(PumpFailure::Remote(anyhow::anyhow!(
                "C2H replay buffer had no room after a capacity-bounded read ({n} bytes); \
                 dropping this connection rather than desyncing client_sent_offset"
            )));
        }
    }
}

async fn pump_h2c(
    quic_read: &mut AnyByteStreamReadHalf,
    stdout: &mut (impl AsyncWrite + Unpin),
    counters: Arc<AppAckCounters>,
) -> Result<(), PumpFailure> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = quic_read
            .read(&mut buf)
            .await
            .context("reading remote stream failed")
            .map_err(PumpFailure::Remote)?;
        if n == 0 {
            return Ok(());
        }
        // Writing/flushing our own stdout failing is a local-side failure
        // (see `PumpFailure`'s docs) — most often `ssh(1)` itself already
        // exited and closed the pipe this process writes into.
        stdout
            .write_all(&buf[..n])
            .await
            .context("writing stdout failed")
            .map_err(PumpFailure::Local)?;
        stdout.flush().await.context("flushing stdout failed").map_err(PumpFailure::Local)?;
        counters.advance_h2c_client_delivered_offset(n as u64);
    }
}

/// C→H 方向（client → helper）に送出したバイト列の再送用バッファ。
///
/// 実体は[`quicmux::ReplayBuffer`]。以前はこのファイルに専用の
/// `VecDeque<u8>`+`start_offset`+`capacity`実装を持っていたが、
/// `engine/resume.rs`の`OutputBuffer`・quicmuxの`ReplayBuffer`と
/// ほぼ同一で、しかも`advance_start`の範囲外挙動だけが静かに
/// 食い違っていた(こちらは`start_offset`を`confirmed_offset`まで
/// 飛ばす、quicmux/`OutputBuffer`はend_offsetでclampする)。
/// clamp側へ一本化した — 詳しい理由は
/// [`quicmux::ReplayBuffer::advance_start`]のdocs参照。
///
/// ここでjump-aheadが問題にならなかったのは、このファイル固有の構造の
/// おかげでしかない: `pump_c2h`は単一タスク内で「appendしてから次の周回で
/// advance_start」の順に実行するため、`engine/mod.rs`側にある
/// 「peerへ送出済みだがappend前」の窓がそもそも開かない。加えて
/// `replay_and_advance`が`replay_from`で範囲外offsetを先に弾くため、
/// この分岐は到達不能だった。
type C2hReplayBuffer = quicmux::ReplayBuffer;

#[cfg(test)]
mod tests {
    use super::*;

    // `C2hReplayBuffer`(= `quicmux::ReplayBuffer`)自体の
    // append/replay_from/advance_start/容量まわりのユニットテストは、型の
    // 定義と同じ場所(`quicmux::resume`)へ一本化した。ここにあった
    // `replay_buffer_replays_unconfirmed_suffix`/
    // `replay_buffer_replay_from_beyond_end_offset_is_none`/
    // `replay_buffer_backpressures_at_capacity`はそちらと1対1で重複していた。
    // isekai-pipe側にしか無いロジック(`replay_and_advance`が範囲外の
    // committed_offsetをどう扱うか)のテストは下に残してある。

    #[test]
    fn resume_window_for_zero_falls_back_to_the_default_window_instead_of_zero_seconds() {
        // `0` means "no real value was ever learned" (the `MustResume`
        // convergence path, or a caller that itself requested `0`), not a
        // literal zero-second resume window that would give up on the very
        // next disconnect (codex review, quicmux-server-resume).
        assert_eq!(resume_window_for(0), DEFAULT_RESUME_WINDOW);
    }

    #[test]
    fn resume_window_for_a_real_value_uses_it_verbatim() {
        assert_eq!(resume_window_for(180), Duration::from_secs(180));
    }

    #[test]
    fn clamp_resume_window_leaves_relay_window_unchanged_when_no_max_is_set() {
        assert_eq!(clamp_resume_window(Duration::from_secs(600), None), Duration::from_secs(600));
    }

    #[test]
    fn clamp_resume_window_caps_stun_window_without_changing_the_server_grace_value() {
        assert_eq!(
            clamp_resume_window(Duration::from_secs(600), Some(STUN_RESUME_GIVE_UP_WINDOW)),
            STUN_RESUME_GIVE_UP_WINDOW
        );
    }

    #[test]
    fn clamp_resume_window_does_not_extend_a_shorter_server_window() {
        assert_eq!(
            clamp_resume_window(Duration::from_secs(30), Some(STUN_RESUME_GIVE_UP_WINDOW)),
            Duration::from_secs(30)
        );
    }

    /// Task 2.9 / issue #111: pins `run_resume_loop`'s EOF-latch decision —
    /// see `should_give_up_without_resuming`'s own docs. A clean `Ok(())`
    /// never gives up; a `Local` failure always does, regardless of
    /// `c2h_already_done`; a `Remote` failure gives up only when
    /// `c2h_already_done` is `true` (a `Remote` failure with the local side
    /// still live must keep resuming exactly as before this fix).
    #[test]
    fn should_give_up_without_resuming_matches_the_eof_latch_design() {
        let local = || PumpFailure::Local(anyhow::anyhow!("local"));
        let remote = || PumpFailure::Remote(anyhow::anyhow!("remote"));

        assert!(!should_give_up_without_resuming(&Ok(()), false), "a clean exit must never give up");
        assert!(!should_give_up_without_resuming(&Ok(()), true), "a clean exit must never give up, even with the latch set");

        assert!(should_give_up_without_resuming(&Err(local()), false), "Local must give up unconditionally");
        assert!(should_give_up_without_resuming(&Err(local()), true), "Local must give up unconditionally");

        assert!(
            !should_give_up_without_resuming(&Err(remote()), false),
            "Remote with the local side still live must keep resuming (unchanged pre-fix behavior)"
        );
        assert!(
            should_give_up_without_resuming(&Err(remote()), true),
            "Remote arriving after the local side already reached clean EOF must give up (the EOF-latch)"
        );
    }

    /// A `NetworkChangeMonitor` that fires exactly one event, then never
    /// resolves again — enough to prove `run_resume_loop`'s `tokio::select!`
    /// (`#20b`'s follow-on network-change wiring) actually treats a signal
    /// arriving *before* the data pump finishes as a reason to abandon the
    /// current connection and reconnect, without needing a real OS backend
    /// or a real QUIC connection to exercise that race in isolation.
    struct FireOnceNetworkChangeMonitor {
        fired: bool,
        /// Defaults to `InterfaceChange` at every existing call site (via
        /// `Default`) — only the `Wake`-specific tests below set this
        /// explicitly, so this field's addition (`NetworkChangeCause`
        /// wiring) didn't need to touch every pre-existing construction.
        cause: isekai_netmon::NetworkChangeCause,
    }

    impl Default for FireOnceNetworkChangeMonitor {
        fn default() -> Self {
            Self { fired: false, cause: isekai_netmon::NetworkChangeCause::InterfaceChange }
        }
    }

    #[async_trait::async_trait]
    impl isekai_netmon::NetworkChangeMonitor for FireOnceNetworkChangeMonitor {
        async fn next_change(&mut self) -> Option<isekai_netmon::NetworkChangeEvent> {
            if self.fired {
                std::future::pending().await
            } else {
                self.fired = true;
                Some(isekai_netmon::NetworkChangeEvent { cause: self.cause })
            }
        }
    }

    #[tokio::test]
    async fn network_change_event_wins_the_race_against_a_pump_that_never_finishes() {
        let mut monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> =
            Box::new(FireOnceNetworkChangeMonitor::default());
        // Stands in for `run_data_pump` (which would otherwise only resolve
        // on clean stdin EOF or a real I/O error) — mirrors the general
        // "pump vs. network-change signal" `tokio::select!` shape
        // `run_resume_loop` uses (today via `spawn_reconnect_signal`'s
        // channel rather than polling a monitor directly in this exact
        // `select!`, but the race semantics under test here are the same
        // either way), without needing real stdin/stdout or a QUIC
        // connection.
        let never_finishes = std::future::pending::<Result<()>>();

        let outcome: Result<()> = tokio::select! {
            outcome = never_finishes => outcome,
            Some(_) = monitor.next_change() => Err(anyhow::anyhow!("network change detected, reconnecting early")),
        };

        assert!(outcome.is_err(), "a network-change event must win the race and produce an early-reconnect signal");
    }

    #[tokio::test]
    async fn no_network_change_event_leaves_the_pump_to_finish_on_its_own() {
        let mut monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> = Box::new(isekai_netmon::NoopNetworkChangeMonitor);
        let finishes_soon = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok::<(), anyhow::Error>(())
        };

        let outcome: Result<()> = tokio::select! {
            outcome = finishes_soon => outcome,
            Some(_) = monitor.next_change() => Err(anyhow::anyhow!("network change detected, reconnecting early")),
        };

        assert!(outcome.is_ok(), "with no network-change signal, the pump's own outcome must be used unchanged");
    }

    struct MockRebinder {
        should_succeed: bool,
    }

    impl Rebindable for MockRebinder {
        async fn rebind(&self, _bind: BindSpec) -> Result<(), isekai_transport::MuxError> {
            if self.should_succeed {
                Ok(())
            } else {
                Err(isekai_transport::MuxError::Rebind("mock failure".to_string()))
            }
        }
    }

    const TEST_HELPER_ADDR: &str = "127.0.0.1:9";

    #[tokio::test]
    async fn spawn_reconnect_signal_forwards_plain_network_change_when_not_experimental() {
        let monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> =
            Box::new(FireOnceNetworkChangeMonitor::default());
        let (task, mut rx) =
            spawn_reconnect_signal(monitor, None::<MockRebinder>, /* experimental */ false, TEST_HELPER_ADDR.parse().unwrap(), None);

        assert!(rx.recv().await.is_some(), "a plain network change must be forwarded when experimental rebind is off");
        task.abort();
    }

    #[tokio::test]
    async fn spawn_reconnect_signal_forwards_plain_network_change_when_experimental_but_no_rebinder() {
        // Experimental is on, but this generation's endpoint factory doesn't
        // support rebinding (`rebinder: None`) - must fall back to exactly
        // the non-experimental behavior, not silently drop the event.
        let monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> =
            Box::new(FireOnceNetworkChangeMonitor::default());
        let (task, mut rx) =
            spawn_reconnect_signal(monitor, None::<MockRebinder>, /* experimental */ true, TEST_HELPER_ADDR.parse().unwrap(), None);

        assert!(rx.recv().await.is_some(), "with no rebinder available, a network change must still be forwarded");
        task.abort();
    }

    #[derive(Debug)]
    struct FakeConnectError(bool);

    impl BusyOtherSessionSignal for FakeConnectError {
        fn signals_busy_other_session(&self) -> bool {
            self.0
        }
    }

    #[tokio::test]
    async fn retry_while_busy_other_session_does_not_retry_other_failures() {
        let mut calls = 0u32;
        let result: Result<(), FakeConnectError> = retry_while_busy_other_session(Duration::from_secs(1), || {
            calls += 1;
            async { Err(FakeConnectError(false)) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls, 1, "a non-BUSY_OTHER_SESSION failure must not be retried");
    }

    #[tokio::test]
    async fn retry_while_busy_other_session_retries_until_a_later_attempt_succeeds() {
        let calls = std::cell::Cell::new(0u32);
        let result = retry_while_busy_other_session(Duration::from_secs(1), || {
            let n = calls.get();
            calls.set(n + 1);
            async move { if n == 0 { Err(FakeConnectError(true)) } else { Ok::<(), FakeConnectError>(()) } }
        })
        .await;
        assert!(result.is_ok(), "a BUSY_OTHER_SESSION failure must be retried until it succeeds");
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn is_unknown_session_rejection_is_true_only_for_unknown_session() {
        assert!(is_unknown_session_rejection(&isekai_transport::TransportError::ResumeRejected(
            isekai_transport::ResumeRejectReason::UnknownSession
        )));
        assert!(!is_unknown_session_rejection(&isekai_transport::TransportError::ResumeRejected(
            isekai_transport::ResumeRejectReason::Auth
        )));
        assert!(!is_unknown_session_rejection(&isekai_transport::TransportError::ResumeRejected(
            isekai_transport::ResumeRejectReason::OffsetGone
        )));
    }

    /// Regression test for the Codex-review finding: `isekai-pipe serve`'s
    /// `RESUME` handler returns the exact same `UnknownSession` wire reason
    /// both for "this session_id never existed / was evicted" and for "this
    /// session exists but the server hasn't finished parking it yet" (a
    /// transient race right after a reset). A single `UnknownSession` must
    /// not be enough to give up — only `UNKNOWN_SESSION_CONFIRM_THRESHOLD`
    /// in a row (past `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`) should.
    #[test]
    fn update_unknown_session_streak_does_not_give_up_below_the_threshold() {
        let mut streak = 0;
        let elapsed = UNKNOWN_SESSION_MIN_ELAPSED_FLOOR + Duration::from_secs(1);
        for _ in 0..(UNKNOWN_SESSION_CONFIRM_THRESHOLD - 1) {
            let should_give_up;
            (streak, should_give_up) = update_unknown_session_streak(streak, true, elapsed);
            assert!(!should_give_up, "must not give up before the threshold is reached");
        }
        assert_eq!(streak, UNKNOWN_SESSION_CONFIRM_THRESHOLD - 1);
    }

    #[test]
    fn update_unknown_session_streak_gives_up_once_the_threshold_and_floor_are_both_reached() {
        let mut streak = 0;
        let mut should_give_up = false;
        let elapsed = UNKNOWN_SESSION_MIN_ELAPSED_FLOOR + Duration::from_secs(1);
        for _ in 0..UNKNOWN_SESSION_CONFIRM_THRESHOLD {
            (streak, should_give_up) = update_unknown_session_streak(streak, true, elapsed);
        }
        assert!(should_give_up, "must give up once both the streak and the elapsed floor are reached");
        assert_eq!(streak, UNKNOWN_SESSION_CONFIRM_THRESHOLD);
    }

    /// Regression test for the Fable-review finding: at `RESUME_BACKOFF`'s
    /// schedule, `UNKNOWN_SESSION_CONFIRM_THRESHOLD` consecutive attempts
    /// land around t≈3.5s — well before `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`
    /// (30s). Reaching the streak alone must not be enough during a
    /// break-before-make roam, where the server can take up to its
    /// `--idle-timeout` (15s default) to notice the old connection died.
    #[test]
    fn update_unknown_session_streak_does_not_give_up_before_the_elapsed_floor_even_at_threshold() {
        let mut streak = 0;
        for _ in 0..(UNKNOWN_SESSION_CONFIRM_THRESHOLD + 5) {
            // Realistic early-episode elapsed time (a few seconds), well
            // under the floor, even though the streak count alone would
            // already be well past the threshold.
            let should_give_up;
            (streak, should_give_up) = update_unknown_session_streak(streak, true, Duration::from_secs(4));
            assert!(!should_give_up, "must not give up before the elapsed floor is reached, regardless of streak length");
        }
        assert!(streak >= UNKNOWN_SESSION_CONFIRM_THRESHOLD, "sanity check: the streak itself did clear the threshold");
    }

    /// The scenario this whole streak exists to tolerate: the server's
    /// not-yet-parked race resolves after a couple of attempts (a later
    /// attempt returns something other than `UnknownSession` — e.g. this
    /// models a successful resume, which the real caller represents by
    /// simply never calling this function again for that episode; here we
    /// model it as a non-`UnknownSession` outcome to prove the streak itself
    /// resets rather than staying "primed" to give up early on the next
    /// disconnect episode).
    #[test]
    fn update_unknown_session_streak_resets_on_a_non_unknown_session_outcome() {
        let elapsed = UNKNOWN_SESSION_MIN_ELAPSED_FLOOR + Duration::from_secs(1);
        let (streak, _) = update_unknown_session_streak(0, true, elapsed);
        let (streak, should_give_up) = update_unknown_session_streak(streak, false, elapsed);
        assert_eq!(streak, 0, "a non-UnknownSession outcome must reset the streak");
        assert!(!should_give_up);
    }

    #[tokio::test]
    async fn retry_while_busy_other_session_gives_up_once_the_window_elapses() {
        let result: Result<(), FakeConnectError> =
            retry_while_busy_other_session(Duration::from_secs(1), || async { Err(FakeConnectError(true)) }).await;
        assert!(result.is_err(), "must stop retrying once the window has elapsed");
    }

    #[tokio::test]
    async fn spawn_reconnect_signal_does_not_forward_after_a_successful_rebind() {
        let monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> =
            Box::new(FireOnceNetworkChangeMonitor::default());
        let rebinder = MockRebinder { should_succeed: true };
        let (task, mut rx) = spawn_reconnect_signal(
            monitor,
            Some(rebinder),
            /* experimental */ true,
            TEST_HELPER_ADDR.parse().unwrap(),
            None,
        );

        let result = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(
            result.is_err(),
            "a successful rebind must not forward a reconnect signal - the caller's data pump should keep running untouched"
        );
        task.abort();
    }

    #[tokio::test]
    async fn spawn_reconnect_signal_forwards_after_a_failed_rebind() {
        let monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> =
            Box::new(FireOnceNetworkChangeMonitor::default());
        let rebinder = MockRebinder { should_succeed: false };
        let (task, mut rx) = spawn_reconnect_signal(
            monitor,
            Some(rebinder),
            /* experimental */ true,
            TEST_HELPER_ADDR.parse().unwrap(),
            None,
        );

        assert!(rx.recv().await.is_some(), "a failed rebind attempt must fall back to the reconnect signal");
        task.abort();
    }

    #[tokio::test]
    async fn spawn_reconnect_signal_skips_rebind_and_forwards_immediately_on_wake() {
        // A `Wake` event (host suspend/resume) must forward straight to a
        // full reconnect without ever calling `rebind` — a local rebind
        // cannot fix a connection that went stale during a suspend, so
        // trying one first (and, on success, swallowing the signal
        // entirely — see the test above) would be actively wrong here, not
        // just slower.
        let monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> =
            Box::new(FireOnceNetworkChangeMonitor { fired: false, cause: isekai_netmon::NetworkChangeCause::Wake });
        let rebinder = MockRebinder { should_succeed: true };
        let (task, mut rx) = spawn_reconnect_signal(
            monitor,
            Some(rebinder),
            /* experimental */ true,
            TEST_HELPER_ADDR.parse().unwrap(),
            None,
        );

        assert!(
            rx.recv().await.is_some(),
            "a Wake event must forward immediately even though this rebinder would have succeeded"
        );
        task.abort();
    }

    /// Minimal real-QUIC fixture for `reestablish_control_stream`/
    /// `replay_and_advance`'s new behavior (codex review,
    /// quicmux-server-resume): a listener that accepts one connection and
    /// speaks just enough of the control-stream wire format
    /// (`CONTROL_HELLO`/`CONTROL_ACK`, `archive/HELPER_PROTOCOL.md` §7.3) to
    /// let `open_control_stream` succeed — mirrors
    /// `isekai-transport::warm_standby`'s own test listener, minus the
    /// RESUME dispatch this doesn't need.
    mod resume_control_stream_tests {
        use super::*;
        use isekai_protocol::hello::{ALPN, EXPORTER_LABEL};
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        const CONTROL_HELLO: u8 = 0x10;
        const CONTROL_ACK: u8 = 0x11;
        const CONTROL_HELLO_FRAME_LEN: usize = 1 + 32; // type byte + 32-byte proof
        const CONTROL_ACK_FRAME_LEN: usize = 1 + 16; // type byte + 16-byte session_id

        fn test_server_config() -> (quicmux::MuxServerConfig, String) {
            let (mut config, cert_sha256_hex) = quicmux::test_support::self_signed_server_config("isekai-pipe.local");
            config.alpn = ALPN.to_vec();
            config.exporter_label = EXPORTER_LABEL.to_vec();
            config.max_concurrent_bidi_streams = 4;
            (config, cert_sha256_hex)
        }

        /// Accepts exactly one connection and, on its first bidi stream,
        /// reads a `CONTROL_HELLO` frame (ignoring the proof — this fixture
        /// isn't testing auth) and replies with `CONTROL_ACK` plus a fixed
        /// session_id, then holds the connection open by looping
        /// `accept_bi()` (matching `warm_standby.rs`'s own listener, which
        /// documents why: dropping the connection right after the write can
        /// race the client's read of that same write).
        async fn spawn_control_hello_listener() -> (SocketAddr, String) {
            let (server_config, cert_sha256_hex) = test_server_config();
            let listener = quicmux::AnyMuxListener::bind_noq(server_config, quicmux::BindSpec::any_ipv4()).await.unwrap();
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listener.local_addr().unwrap().port());
            tokio::spawn(async move {
                let Some(incoming) = listener.accept().await else { return };
                let Ok(conn) = incoming.accept().await else { return };
                loop {
                    let Ok(stream) = conn.accept_bi().await else { break };
                    let (mut recv, mut send) = stream.split();
                    let mut hello = [0u8; CONTROL_HELLO_FRAME_LEN];
                    if recv.read(&mut hello).await.unwrap_or(0) == 0 || hello[0] != CONTROL_HELLO {
                        continue;
                    }
                    let mut ack = vec![CONTROL_ACK];
                    ack.extend_from_slice(&[0x7Fu8; CONTROL_ACK_FRAME_LEN - 1]);
                    let _ = send.write_all(&ack).await;
                }
            });
            (addr, cert_sha256_hex)
        }

        async fn connect(addr: SocketAddr, cert_sha256_hex: String) -> quicmux::AnyMuxConnection {
            let factory = system_quic_factory();
            let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await.unwrap();
            endpoint
                .connect(quicmux::RemoteSpec { addr, server_name: "isekai-pipe.local".to_string(), cert_sha256_hex })
                .await
                .unwrap()
        }

        #[tokio::test]
        async fn reestablish_control_stream_succeeds_against_a_real_listener() {
            let (addr, cert_sha256_hex) = spawn_control_hello_listener().await;
            let conn = connect(addr, cert_sha256_hex).await;
            let counters = Arc::new(AppAckCounters::new());

            let tasks = reestablish_control_stream(&conn, b"any-session-secret", &counters).await;
            assert!(tasks.is_ok(), "{:?}", tasks.err());
            tasks.unwrap().abort();
        }

        /// Minimal `AsyncRead`/`AsyncWrite` fakes that always fail — enough to
        /// exercise `pump_c2h`/`pump_h2c`'s local-vs-remote classification
        /// without a real broken pipe. Zero-sized and field-free, so no
        /// pinning hazard from a plain (non-`pin_project`) `impl AsyncRead`/
        /// `AsyncWrite`.
        struct FailingReader;
        impl tokio::io::AsyncRead for FailingReader {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "synthetic local stdin failure")))
            }
        }

        struct FailingWriter;
        impl tokio::io::AsyncWrite for FailingWriter {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "synthetic local stdout failure")))
            }
            fn poll_flush(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_shutdown(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        /// Regression test for Task 2.9 / issue #111 (2026-09-02): a failed
        /// `stdin.read()` (this process's own local pipe, most often closed
        /// because `ssh(1)` itself already exited — see `PumpFailure`'s
        /// docs) must be classified `Local`, not `Remote`. Before this fix,
        /// `pump_c2h` had no such distinction and `run_resume_loop` treated
        /// every pump failure as worth resuming past, which for a `Local`
        /// failure with a healthy network produced an unbounded reconnect
        /// loop instead of giving up.
        #[tokio::test]
        async fn pump_c2h_classifies_a_stdin_read_failure_as_local() {
            let (addr, cert_sha256_hex) = spawn_control_hello_listener().await;
            let conn = connect(addr, cert_sha256_hex).await;
            let stream = conn.open_bi().await.unwrap();
            let (_recv, mut send) = stream.split();

            let mut stdin = FailingReader;
            let replay = Arc::new(Mutex::new(C2hReplayBuffer::new(1024)));
            let counters = Arc::new(AppAckCounters::new());

            let result = pump_c2h(&mut stdin, &mut send, replay, counters).await;
            assert!(
                matches!(result, Err(PumpFailure::Local(_))),
                "a stdin read failure must be classified Local, not Remote: {result:?}"
            );
        }

        /// Same regression as above, for `pump_h2c`'s `stdout` side: a
        /// successful QUIC read followed by a failed `stdout.write_all()`
        /// must be classified `Local`, not `Remote`. Sends a real
        /// `CONTROL_HELLO` frame so the fixture listener's `CONTROL_ACK`
        /// reply gives `pump_h2c` genuine bytes to read before its `stdout`
        /// write fails — otherwise this would only prove the (already
        /// separately covered) EOF path, not the write-failure one.
        #[tokio::test]
        async fn pump_h2c_classifies_a_stdout_write_failure_as_local() {
            let (addr, cert_sha256_hex) = spawn_control_hello_listener().await;
            let conn = connect(addr, cert_sha256_hex).await;
            let stream = conn.open_bi().await.unwrap();
            let (mut recv, mut send) = stream.split();

            let mut hello = vec![CONTROL_HELLO];
            hello.extend_from_slice(&[0u8; CONTROL_HELLO_FRAME_LEN - 1]);
            send.write_all(&hello).await.unwrap();

            let mut stdout = FailingWriter;
            let counters = Arc::new(AppAckCounters::new());
            let result = pump_h2c(&mut recv, &mut stdout, counters).await;
            assert!(
                matches!(result, Err(PumpFailure::Local(_))),
                "a stdout write failure must be classified Local, not Remote: {result:?}"
            );
        }

        #[tokio::test]
        async fn replay_and_advance_rejects_a_committed_offset_beyond_what_was_ever_sent() {
            let (addr, cert_sha256_hex) = spawn_control_hello_listener().await;
            let conn = connect(addr, cert_sha256_hex).await;
            let mut stream = conn.open_bi().await.unwrap();

            let replay = Mutex::new(C2hReplayBuffer::new(1024));
            assert!(replay.lock().unwrap().append(b"hello"));

            // The helper claims committed_offset=999, but this client never
            // sent more than 5 bytes — a protocol inconsistency that must
            // not be silently accepted (codex review).
            let ok = replay_and_advance(&replay, 999, &mut stream).await;
            assert!(!ok, "an out-of-range committed_offset must be rejected, not silently accepted");
            assert_eq!(replay.lock().unwrap().end_offset(), 5, "the replay buffer must be untouched on rejection");
        }

        #[tokio::test]
        async fn replay_and_advance_still_replays_a_valid_in_range_offset() {
            // Regression check: the new out-of-range rejection above must
            // not have broken the ordinary, already-tested in-range path.
            let (addr, cert_sha256_hex) = spawn_control_hello_listener().await;
            let conn = connect(addr, cert_sha256_hex).await;
            let mut stream = conn.open_bi().await.unwrap();

            let replay = Mutex::new(C2hReplayBuffer::new(1024));
            assert!(replay.lock().unwrap().append(b"hello world"));

            let ok = replay_and_advance(&replay, 6, &mut stream).await;
            assert!(ok);
            assert_eq!(replay.lock().unwrap().end_offset(), 11);
        }

        #[tokio::test]
        async fn resume_with_backoff_until_deadline_returns_err_once_the_resume_window_is_exceeded() {
            // Regression test for the bug reported 2026-07-16: this give-up
            // path used to `return None`, and `run_resume_loop` turned that
            // into `Ok(())` — a silent "clean exit" that never reached
            // `connect_command`'s `Err` arm, so `write_connect_outcome_for_wrapper`
            // never fired and `isekai-ssh`'s wrapper had no signal to
            // auto-retry on (e.g. after a Windows sleep/wake outlasts the
            // resume window). Must return `Err` so the "always-connects"
            // auto-recovery in `wrapper.rs` actually engages.
            let (addr, cert_sha256_hex) = spawn_control_hello_listener().await;
            let conn = connect(addr, cert_sha256_hex.clone()).await;
            let counters = Arc::new(AppAckCounters::new());
            let app_ack_tasks = reestablish_control_stream(&conn, b"any-session-secret", &counters).await.unwrap();

            let target = RelayTarget {
                helper_addr: addr,
                server_name: "isekai-pipe.local".to_string(),
                cert_sha256_hex,
                session_secret: b"any-session-secret".to_vec(),
                local_bind_port_range: None,
            };
            let factory = system_quic_factory();
            let mut state = ResumeLoopState {
                session_id: isekai_transport::SessionId::from_bytes([0x7Fu8; 16]),
                counters,
                replay: Arc::new(Mutex::new(C2hReplayBuffer::new(1024))),
                app_ack_tasks,
                network_rebinder: None,
                is_tty: false,
                last_resume_error: Some("connection refused".to_string()),
                consecutive_unknown_session: 0,
            };
            let now = Instant::now();
            let mut monitor = isekai_netmon::NoopNetworkChangeMonitor;

            let result = resume_with_backoff_until_deadline(
                &factory,
                &target,
                "test-profile",
                Duration::from_secs(0),
                now,
                now, // deadline already reached: must give up on the first check
                &mut state,
                &None,
                &mut monitor,
                None,
            )
            .await;

            assert!(result.is_err(), "must return Err once the resume window is exceeded, not a silent Ok/None");
            state.app_ack_tasks.abort();
        }

        /// Round 3 code review, significant finding (revised after a second
        /// round caught that the first cut of this test re-derived the
        /// composition inline instead of calling `effective_resume_window`
        /// — the same function `run_resume_loop` calls — which meant it
        /// couldn't actually catch a regression that dropped the clamp from
        /// `run_resume_loop` itself). The three `clamp_resume_window` unit
        /// tests only cover that helper in isolation; this test additionally
        /// pins `effective_resume_window` (the exact call `run_resume_loop`
        /// makes) by driving the *real* `resume_with_backoff_until_deadline`
        /// (same proven fixture as the test above) with its result, for a
        /// STUN-shaped scenario (a large server-granted grace, clamped down
        /// by a short `max_resume_window`) — and asserts the give-up error
        /// reports the *clamped* window, not the unclamped one.
        #[tokio::test]
        async fn effective_resume_window_clamp_reaches_the_real_give_up_message_for_stun() {
            let (addr, cert_sha256_hex) = spawn_control_hello_listener().await;
            let conn = connect(addr, cert_sha256_hex.clone()).await;
            let counters = Arc::new(AppAckCounters::new());
            let app_ack_tasks = reestablish_control_stream(&conn, b"any-session-secret", &counters).await.unwrap();

            let target = RelayTarget {
                helper_addr: addr,
                server_name: "isekai-pipe.local".to_string(),
                cert_sha256_hex,
                session_secret: b"any-session-secret".to_vec(),
                local_bind_port_range: None,
            };
            let factory = system_quic_factory();
            let mut state = ResumeLoopState {
                session_id: isekai_transport::SessionId::from_bytes([0x7Fu8; 16]),
                counters,
                replay: Arc::new(Mutex::new(C2hReplayBuffer::new(1024))),
                app_ack_tasks,
                network_rebinder: None,
                is_tty: false,
                last_resume_error: Some("connection refused".to_string()),
                consecutive_unknown_session: 0,
            };
            let mut monitor = isekai_netmon::NoopNetworkChangeMonitor;

            // A server that granted a multi-hour resume grace, exactly as a
            // real relay/STUN peer with a generous `--resume-window` would.
            let server_granted_grace_secs: u32 = 6 * 60 * 60;
            let max_resume_window = Some(STUN_RESUME_GIVE_UP_WINDOW);
            // The exact function `run_resume_loop` calls — not a
            // re-derivation of its composition — so this test actually
            // fails if a future change drops the clamp from that call site.
            let resume_window = effective_resume_window(server_granted_grace_secs, max_resume_window);
            assert_eq!(resume_window, STUN_RESUME_GIVE_UP_WINDOW, "sanity check on the composition itself before using it below");

            // `now` for both `disconnected_at` and `deadline`, exactly like
            // the sibling test above — the give-up branch triggers on
            // `now >= deadline`, and the message formats the `resume_window`
            // *parameter* directly, not an elapsed delta, so there's no need
            // to subtract from `Instant::now()` (which would panic on a host
            // with under 121s of monotonic uptime).
            let now = Instant::now();

            let result = resume_with_backoff_until_deadline(
                &factory,
                &target,
                "test-profile",
                resume_window,
                now,
                now,
                &mut state,
                &None,
                &mut monitor,
                max_resume_window,
            )
            .await;

            let Err(err) = result else {
                panic!("must give up immediately once the clamped (short) window is exceeded");
            };
            let message = format!("{err:#}");
            assert!(
                message.contains(&format!("{STUN_RESUME_GIVE_UP_WINDOW:?}")),
                "give-up message must report the clamped STUN window, not the unclamped server grace: {message:?}"
            );
            assert!(
                !message.contains(&format!("{:?}", resume_window_for(server_granted_grace_secs))),
                "give-up message must not report the unclamped multi-hour window: {message:?}"
            );
            state.app_ack_tasks.abort();
        }
    }

    mod reconnect_status_tests {
        use super::*;

        #[test]
        fn format_reconnect_status_tty_uses_in_place_ansi_redraw() {
            let msg = format_reconnect_status(true, 3, 60);
            assert!(msg.starts_with('\r'), "TTY表示はその場書き換え(\\r開始)のはず: {msg:?}");
            assert!(msg.contains("\x1b[0;33m"), "黄色のANSIエスケープを含むはず: {msg:?}");
            assert!(msg.ends_with("\x1b[K"), "行末までクリアするはず: {msg:?}");
            assert!(msg.contains("3s/60s"), "経過/上限秒数を含むはず: {msg:?}");
            assert!(!msg.contains('\n'), "改行を含んではいけない(呼び出し側がeprint!でその場書き換えする前提): {msg:?}");
        }

        #[test]
        fn format_reconnect_status_non_tty_is_plain_text_without_ansi() {
            let msg = format_reconnect_status(false, 3, 60);
            assert!(!msg.contains('\r'), "非TTY時は\\rを含んではいけない: {msg:?}");
            assert!(!msg.contains('\x1b'), "非TTY時はANSIエスケープを含んではいけない: {msg:?}");
            assert!(msg.contains("3s"), "経過秒数を含むはず: {msg:?}");
            assert!(msg.contains("60s"), "上限秒数を含むはず: {msg:?}");
        }

        // `sleep_with_live_status`本体はタイミングだけを担当し(実際の描画は
        // `on_tick`コールバックに委譲)、`tokio::time::pause()`で仮想時間を
        // 進めれば実時間を待たずに決定的に検証できる。
        #[tokio::test(start_paused = true)]
        async fn sleep_with_live_status_ticks_once_per_second_until_delay_elapses() {
            // `#[tokio::test(start_paused = true)]`下では、他にやることが
            // 無い間はtokioの仮想時計が次のタイマーまで自動的に早送りされる
            // ため、手動で`tokio::time::advance`を挟まずそのまま`.await`
            // すればよい(spawn+手動advanceは、spawnされたタスクが実際に
            // 最初のtimer登録を終える前にadvanceが先に走ってしまう競合が
            // あり不安定だった)。
            // 2.5秒の待機は 1s + 1s + 0.5s の3チャンクに分かれ、3回tickするはず。
            let mut tick_count = 0;
            sleep_with_live_status(Duration::from_millis(2500), || tick_count += 1).await;
            assert_eq!(tick_count, 3);
        }

        #[tokio::test(start_paused = true)]
        async fn sleep_with_live_status_ticks_once_for_a_sub_second_delay() {
            let mut tick_count = 0;
            sleep_with_live_status(Duration::from_millis(300), || tick_count += 1).await;
            assert_eq!(
                tick_count, 1,
                "1秒未満の待機でも最低1回はtickして呼び出し元に経過を伝えるはず"
            );
        }
    }

    mod wait_backoff_or_network_change_tests {
        use super::*;

        // `wait_backoff_or_network_change`はバックオフ待機とOSネットワーク
        // 変化通知を`tokio::select!`でレースさせるだけなので、
        // `sleep_with_live_status`と同じ`tokio::time::pause()`パターンで
        // 実時間を待たずに決定的に検証できる。

        #[tokio::test(start_paused = true)]
        async fn returns_early_when_the_network_monitor_fires_before_the_delay_elapses() {
            let mut monitor = FireOnceNetworkChangeMonitor::default();
            let started = tokio::time::Instant::now();
            let mut tick_count = 0;
            wait_backoff_or_network_change(Duration::from_secs(10), true, || tick_count += 1, &mut monitor).await;
            assert_eq!(
                tokio::time::Instant::now(),
                started,
                "監視から即座にイベントが来た場合、10秒のdelayを一切待たずに返るはず"
            );
            assert_eq!(tick_count, 0, "早期リターンした場合はon_tick(ライブ再描画)も一切呼ばれないはず");
        }

        #[tokio::test(start_paused = true)]
        async fn waits_out_the_full_delay_when_the_network_monitor_never_fires() {
            let mut monitor = isekai_netmon::NoopNetworkChangeMonitor;
            let started = tokio::time::Instant::now();
            wait_backoff_or_network_change(Duration::from_millis(2500), false, || (), &mut monitor).await;
            assert_eq!(
                tokio::time::Instant::now() - started,
                Duration::from_millis(2500),
                "監視が一度も発火しない場合は今まで通りdelay全体を待つはず"
            );
        }
    }
}
