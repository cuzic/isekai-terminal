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
    SequentialRelayCandidate, SequentialStunCandidate, StunP2pTarget,
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

/// Marks an `anyhow::Error` as having occurred *after* the STUN P2P
/// handshake already succeeded — i.e. during the pump phase (`relay_stdio`),
/// not while dialing (Epic R PR2, Task 2.4). Mirrors
/// `isekai_transport::StaleTrustSignal`'s attach-at-the-source/
/// downcast-at-the-top shape (`connect.rs::attach_stale_trust_signal`): a
/// bare `err.downcast_ref::<MidSessionDisconnectSignal>()` finds this
/// even through an outer `.context(...)` wrap, since `anyhow`'s own
/// `downcast_ref` already walks the `ContextError` chain — no manual
/// `err.chain()` traversal needed (round 1 review, R1-B2/B2, corrected a
/// wrong assumption in an earlier draft of this fix).
///
/// Deliberately **not** attached to `run_resume_loop`'s own `give_up` path
/// (the Relay — and, before PR3, non-existent-for-STUN — resume loop): that
/// loop already retries internally for up to `DEFAULT_RESUME_GRACE_SECS`
/// (10 days) before giving up, so an error escaping it is essentially
/// terminal and the existing `RebootstrapAndRetry` (full re-deploy) is the
/// correct response, not a lightweight retry (S1(old) from the ADR review).
pub(crate) struct MidSessionDisconnectSignal;

pub(crate) async fn relay_stdio(stream: AnyByteStream) -> Result<()> {
    let (mut quic_read, mut quic_write) = stream.split();
    let mut c2h = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = stdin.read(&mut buf).await.context("reading stdin failed")?;
            if n == 0 {
                let _ = quic_write.shutdown().await;
                return Ok::<_, anyhow::Error>(());
            }
            quic_write
                .write_all(&buf[..n])
                .await
                .context("writing to remote stream failed")?;
        }
    });
    let mut h2c = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = quic_read
                .read(&mut buf)
                .await
                .context("reading remote stream failed")?;
            if n == 0 {
                return Ok::<_, anyhow::Error>(());
            }
            stdout
                .write_all(&buf[..n])
                .await
                .context("writing stdout failed")?;
            stdout.flush().await.context("flushing stdout failed")?;
        }
    });

    let (mut c2h_done, mut h2c_done) = (false, false);
    while !c2h_done || !h2c_done {
        tokio::select! {
            res = &mut c2h, if !c2h_done => {
                c2h_done = true;
                res.context("stdin->remote task panicked")??;
            }
            res = &mut h2c, if !h2c_done => {
                h2c_done = true;
                res.context("remote->stdout task panicked")??;
            }
        }
    }
    Ok(())
}

/// Narrow signal a retried-connect error type must expose for
/// [`retry_while_busy_other_session`] — named distinctly from the underlying
/// `TransportError`/`SequentialConnectError::is_busy_other_session` inherent
/// methods it delegates to, so calling `self.is_busy_other_session()` inside
/// each impl unambiguously reaches the inherent one rather than recursing.
trait BusyOtherSessionSignal {
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
/// PR2, Task 2.11) so `connect.rs`'s single-candidate STUN P2P path can
/// reuse it too — STUN P2P has no resume/control-stream concept of its own
/// (`run_stun_p2p_with_fallback`'s docs), but a fresh reconnect attempt
/// racing this same client's own not-yet-expired parked session is exactly
/// as possible there as it is for relay.
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
    run_resume_loop(&factory, target, profile, established, experimental_network_rebind, tethering_interface).await
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
    run_resume_loop(&factory, &winning_target, profile, established, experimental_network_rebind, tethering_interface).await
}

/// Like the single-candidate `CandidateRoute::StunP2p` path in `run_connect`,
/// but tries `candidates` (each a different STUN server against the same
/// peer) in priority order (`#11`) instead of dialing a single fixed STUN
/// server. STUN P2P has no resume/control-stream concept (`stun_p2p.rs`'s
/// module docs), so — unlike `run_relay_resumable_with_fallback` — there is
/// no `run_resume_loop` step here: the winning candidate's stream goes
/// straight into `relay_stdio`, exactly like the legacy single-candidate path
/// already does.
pub(crate) async fn run_stun_p2p_with_fallback(target: &StunP2pTarget, candidates: &[SequentialStunCandidate]) -> Result<()> {
    let factory = system_quic_factory();
    // Epic R PR2, Task 2.11: STUN P2P used to skip the same
    // `BUSY_OTHER_SESSION` retry the relay paths already get above — a
    // fresh `isekai-pipe connect` process reconnecting right after a
    // mid-session disconnect (this module's whole reason for existing) is
    // exactly the case `retry_while_busy_other_session`'s docs describe:
    // this same client's own prior session still parked on the remote
    // helper, not a real conflicting session.
    let (connection, _winning_stun_server) =
        retry_while_busy_other_session(BUSY_OTHER_SESSION_RETRY_WINDOW, || connect_stun_p2p_with_fallback(&factory, target, candidates))
            .await
            .map_err(attach_stale_trust_signal)?;
    // Epic R PR2, Task 2.4: the handshake above already succeeded, so any
    // error from here on is a mid-session disconnect, not a connect-time
    // failure — mark it so `write_connect_outcome_for_wrapper` classifies
    // it as `ConnectOutcomeClass::MidSessionDisconnect` instead of
    // `Unreachable`.
    relay_stdio(connection.stream).await.map_err(|e| e.context(MidSessionDisconnectSignal))
}

/// Runs the C2H/H2C data pump against `established`, resuming (via
/// `reconnect_and_resume` against `target` — the *specific* candidate that
/// won, in the fallback case) across disconnects until either the local side
/// closes cleanly or the resume window is exceeded. Shared by both
/// `run_relay_resumable` (single fixed target) and
/// `run_relay_resumable_with_fallback` (the winning target out of several
/// candidates) — resuming a session is always scoped to the one connection
/// that established it, never a fresh candidate search.
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
) -> Result<AnyByteStream> {
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
            notify_os(
                "isekai-pipe connect",
                &format!("Giving up reconnecting to '{profile}' (session_id={session_id}).{last_error_suffix}"),
            );
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
                    notify_os(
                        "isekai-pipe connect",
                        &format!("Giving up reconnecting to '{profile}' (session_id={session_id}): server no longer knows this session."),
                    );
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

pub(crate) async fn run_resume_loop(
    factory: &AnyMuxFactory,
    target: &RelayTarget,
    profile: &str,
    established: isekai_transport::ResumableRelaySession,
    experimental_network_rebind: bool,
    tethering_interface: Option<isekai_transport::InterfaceIndex>,
) -> Result<()> {
    let session_id = established.session_id;
    drop(established.connection);

    let resume_window = resume_window_for(established.effective_resume_grace_secs);

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
        let outcome = tokio::select! {
            outcome = run_data_pump(&mut stdin, &mut stdout, &mut quic_read, &mut quic_write, &state.replay, &state.counters) => outcome,
            Some(()) = reconnect_signal_rx.recv() => {
                Err(anyhow::anyhow!("network change detected, reconnecting"))
            }
        };
        reconnect_signal_task.abort();
        state.app_ack_tasks.abort();

        if outcome.is_ok() {
            if let Some(t) = &warm_standby_task {
                t.abort();
            }
            return Ok(());
        }

        // Abandoning this connection (network change, or run_data_pump's own
        // I/O failure) — explicitly reset the send side instead of letting
        // it drop gracefully. `noq`/`qmux`'s `Drop` for a send stream calls
        // `finish()` (a clean FIN) by default, which `isekai-pipe serve`'s
        // `relay_buffered` cannot distinguish from a legitimate half-close
        // (e.g. stdin EOF, where S→C must keep flowing) — so it leaves the
        // session `Established`-but-never-parked on this now-dead
        // connection instead of parking it for resume, and every subsequent
        // RESUME then fails as "not resumable" (`UnknownSession`) forever
        // (found via live debugging, 2026-07-11: the very next reconnect
        // attempt after a network-change-triggered abandon got exactly this
        // rejection). A reset instead makes the server's read return an
        // error, correctly classified as `RelayOutcome::DataStreamDied` and
        // parked.
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
                )
                .await?
            }
        };

        data_stream = new_stream;
        disconnected_since = None;
        state.last_resume_error = None;
    }
}

async fn run_data_pump(
    stdin: &mut (impl AsyncRead + Unpin),
    stdout: &mut (impl AsyncWrite + Unpin),
    quic_read: &mut AnyByteStreamReadHalf,
    quic_write: &mut AnyByteStreamWriteHalf,
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
                res.context("isekai-pipe connect: C2H pump failed")?;
                c2h_done = true;
            }
            res = &mut h2c_fut, if !h2c_done => {
                res.context("isekai-pipe connect: H2C pump failed")?;
                h2c_done = true;
            }
        }
        if c2h_done && h2c_done {
            return Ok(());
        }
    }
}

async fn pump_c2h(
    stdin: &mut (impl AsyncRead + Unpin),
    quic_write: &mut AnyByteStreamWriteHalf,
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
        let n = stdin
            .read(&mut buf[..read_len])
            .await
            .context("reading stdin failed")?;
        if n == 0 {
            let _ = quic_write.shutdown().await;
            return Ok(());
        }
        quic_write
            .write_all(&buf[..n])
            .await
            .context("writing to remote stream failed")?;
        // `read_len`が`remaining_capacity()`で頭打ちにしてあり、`advance_start`は
        // 空きを増やすことしかしないため、ここが`false`になることは無い。それでも
        // 握り潰さないのは、もし起きた場合の被害が「replayバッファに載らないまま
        // QUICへ送出済みのバイトができる」=`end_offset()`由来の
        // `client_sent_offset`がhelper側とずれる、というresume不能状態だから
        // (`.claude/rules/always-connects.md`)。接続ごと畳んでしまえば
        // `run_resume_loop`が再接続からやり直せるので、そちらの方が安全側に倒れる。
        if !replay.lock().unwrap().append(&buf[..n]) {
            anyhow::bail!(
                "C2H replay buffer had no room after a capacity-bounded read ({n} bytes); \
                 dropping this connection rather than desyncing client_sent_offset"
            );
        }
    }
}

async fn pump_h2c(
    quic_read: &mut AnyByteStreamReadHalf,
    stdout: &mut (impl AsyncWrite + Unpin),
    counters: Arc<AppAckCounters>,
) -> Result<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = quic_read
            .read(&mut buf)
            .await
            .context("reading remote stream failed")?;
        if n == 0 {
            return Ok(());
        }
        stdout
            .write_all(&buf[..n])
            .await
            .context("writing stdout failed")?;
        stdout.flush().await.context("flushing stdout failed")?;
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
            )
            .await;

            assert!(result.is_err(), "must return Err once the resume window is exceeded, not a silent Ok/None");
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
