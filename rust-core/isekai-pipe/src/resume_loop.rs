//! The relay/STUN-P2P data pump and relay-resumable reconnect loop —
//! everything downstream of a successful [`crate::connect`] dial. Owns the
//! C2H replay buffer, the RESUME backoff/retry loop, warm-standby promotion,
//! and the OS network-change → reconnect signal plumbing. See
//! `run_resume_loop`'s own doc comment for why the network-change handling
//! needed a background task (`spawn_reconnect_signal`) rather than racing
//! `run_data_pump` directly in one `select!`.

use std::collections::VecDeque;
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

use crate::connect::{attach_stale_trust_signal, relay_endpoint_factory, RelayTransportKind};
use crate::DEFAULT_RESUME_WINDOW;

const C2H_REPLAY_BUFFER_CAPACITY: usize = 4 * 1024 * 1024;
const RESUME_BACKOFF: BackoffPolicy = BackoffPolicy {
    initial: Duration::from_millis(500),
    max: Duration::from_secs(10),
    jitter: 0.0,
};
const BACKPRESSURE_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How often `run_resume_loop`'s background task calls
/// `WarmStandby::ensure_warm` while `--tethering-interface` is set. Matches
/// the "~15-30s while the primary looks healthy" half of the
/// `pc-tethering-warm-standby-design` memory's agreed tiering — the more
/// aggressive "~1-3s once the primary looks like it's degrading" half is not
/// implemented (this loop has no independent signal that the primary is
/// degrading, only that it's already dead, at which point promotion is
/// already being attempted).
const WARM_STANDBY_PROBE_INTERVAL: Duration = Duration::from_secs(20);

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

/// Retries `attempt` while — and only while — it fails with
/// `BUSY_OTHER_SESSION`, for up to `resume_window_for(requested_resume_grace_secs)`
/// (the same deadline a resume loop would use, since a `BUSY_OTHER_SESSION`
/// reject on the very first connect most often means *this same client's*
/// previous session is still parked on the remote helper, waiting out that
/// exact window after an earlier ungraceful disconnect — see
/// `TransportError::is_busy_other_session`'s docs). Every other failure is
/// returned immediately on the first attempt, unchanged from before this
/// wrapper existed: this only closes the gap where a fresh `isekai-pipe
/// connect` process (a brand new `session_id` every time, since neither
/// `connect_via_relay_resumable` nor `_with_fallback` persist one across
/// invocations) would otherwise fail outright instead of waiting the same
/// window a same-process resume would have.
async fn retry_while_busy_other_session<T, E, F, Fut>(requested_resume_grace_secs: u32, mut attempt: F) -> Result<T, E>
where
    E: BusyOtherSessionSignal,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let deadline = Instant::now() + resume_window_for(requested_resume_grace_secs);
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
        let delay = RESUME_BACKOFF.base_delay(attempt_no).min(deadline - now);
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
    let established = retry_while_busy_other_session(requested, || connect_via_relay_resumable(&factory, target, requested, identity))
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
        retry_while_busy_other_session(requested, || connect_via_relay_resumable_with_fallback(&factory, candidates, requested))
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
    let (connection, _winning_stun_server) = connect_stun_p2p_with_fallback(&system_quic_factory(), target, candidates)
        .await
        .map_err(attach_stale_trust_signal)?;
    relay_stdio(connection.stream).await
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
///   network change (`isekai-netmon`; a no-op on platforms other than
///   Windows/macOS today) is forwarded immediately — this is exactly the
///   behavior this function replaced (`network_monitor.next_change()` raced
///   directly against `run_data_pump` in the same `select!`), just moved
///   into its own task so both shapes can feed the same channel.
/// - **Experimental with a rebinder**: tries `AnyMuxRebinder::rebind`
///   first on every change; only a *failed* rebind attempt is forwarded,
///   and this task then stops (that generation's endpoint is about to be
///   abandoned by the RESUME reconnect the failure triggers, so continuing
///   to watch it is pointless). A *successful* rebind is invisible to the
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
/// inject a controllable mock instead of the real (on this development
/// platform, Linux, always-`NoopNetworkChangeMonitor`) OS-backed one.
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
                while network_monitor.next_change().await.is_some() {
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
    if !bytes.is_empty() && stream.write_all(&bytes).await.is_err() {
        return false;
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
            log::info!("isekai-pipe connect: warm-standby promote unavailable ({e:#}); falling back to full resume");
            return None;
        }
    };
    if !replay_and_advance(&state.replay, promoted.helper_committed_offset.get(), &mut promoted.data_stream).await {
        eprintln!("isekai-pipe connect: warm-standby promote succeeded but replay failed; falling back to full resume");
        return None;
    }
    log::info!("isekai-pipe connect: promoted warm-standby connection for session_id={}", state.session_id);
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
/// attempt succeeds or `deadline` passes. Returns `Some(stream)` on success
/// (having also re-established the control stream and updated
/// `state.network_rebinder`); returns `None` once `deadline` has passed,
/// having already closed `stdout` and aborted `warm_standby_task` — the
/// caller's only remaining step on `None` is to return `Ok(())`.
async fn resume_with_backoff_until_deadline(
    factory: &AnyMuxFactory,
    target: &RelayTarget,
    profile: &str,
    resume_window: Duration,
    deadline: Instant,
    state: &mut ResumeLoopState,
    stdout: &mut tokio::io::Stdout,
    warm_standby_task: &Option<tokio::task::JoinHandle<()>>,
) -> Option<AnyByteStream> {
    let mut attempt: u32 = 0;
    loop {
        let now = Instant::now();
        if now >= deadline {
            let exceeded_by = now.saturating_duration_since(deadline);
            eprintln!(
                "isekai-pipe connect: giving up on session_id={} for '{profile}' - \
                 the resume window ({resume_window:?}) was exceeded by {exceeded_by:?}. \
                 Closing stdin/stdout; ssh will treat this as a lost connection.",
                state.session_id
            );
            let _ = stdout.shutdown().await;
            if let Some(t) = warm_standby_task {
                t.abort();
            }
            return None;
        }

        let delay = RESUME_BACKOFF.base_delay(attempt).min(deadline - now);
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(delay).await;

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
                if !replay_and_advance(&state.replay, resumed.helper_committed_offset.get(), &mut resumed.data_stream).await {
                    continue;
                }
                match reestablish_control_stream(&resumed.connection, &target.session_secret, &state.counters).await {
                    Ok(new_tasks) => state.app_ack_tasks = new_tasks,
                    Err(e) => eprintln!(
                        "isekai-pipe connect: control stream re-establishment after resume failed ({e:#}), \
                         continuing without resume support until the next reattach"
                    ),
                }
                drop(resumed.connection);
                state.network_rebinder = resumed.network_rebinder;
                return Some(resumed.data_stream);
            }
            Err(e) => {
                eprintln!("isekai-pipe connect: resume attempt {attempt} failed: {e:#}");
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
            loop {
                interval.tick().await;
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
        let deadline = *disconnected_since.get_or_insert_with(Instant::now) + resume_window;

        let promoted_stream = match &warm_standby {
            Some(ws) => promote_warm_standby_once(ws, target, &mut state).await,
            None => None,
        };

        let new_stream = match promoted_stream {
            Some(stream) => stream,
            None => {
                match resume_with_backoff_until_deadline(
                    factory,
                    target,
                    profile,
                    resume_window,
                    deadline,
                    &mut state,
                    &mut stdout,
                    &warm_standby_task,
                )
                .await
                {
                    Some(stream) => stream,
                    None => return Ok(()),
                }
            }
        };

        data_stream = new_stream;
        disconnected_since = None;
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
        replay.lock().unwrap().append(&buf[..n]);
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

struct C2hReplayBuffer {
    data: VecDeque<u8>,
    start_offset: u64,
    capacity: usize,
}

impl C2hReplayBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(capacity.min(1 << 20)),
            start_offset: 0,
            capacity,
        }
    }

    fn is_full(&self) -> bool {
        self.data.len() >= self.capacity
    }

    fn remaining_capacity(&self) -> usize {
        self.capacity.saturating_sub(self.data.len())
    }

    fn append(&mut self, bytes: &[u8]) {
        debug_assert!(
            self.data.len() + bytes.len() <= self.capacity,
            "C2hReplayBuffer::append called past capacity"
        );
        self.data.extend(bytes.iter().copied());
    }

    fn advance_start(&mut self, confirmed_offset: u64) {
        let wanted = confirmed_offset.saturating_sub(self.start_offset) as usize;
        let drop_count = wanted.min(self.data.len());
        self.data.drain(..drop_count);
        self.start_offset += drop_count as u64;
        if confirmed_offset > self.start_offset {
            self.start_offset = confirmed_offset;
        }
    }

    fn end_offset(&self) -> u64 {
        self.start_offset + self.data.len() as u64
    }

    fn replay_from(&self, from: u64) -> Option<Vec<u8>> {
        if from < self.start_offset || from > self.end_offset() {
            return None;
        }
        let skip = (from - self.start_offset) as usize;
        Some(self.data.iter().skip(skip).copied().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_buffer_replays_unconfirmed_suffix() {
        let mut buffer = C2hReplayBuffer::new(16);
        buffer.append(b"hello ");
        buffer.append(b"world");

        assert_eq!(buffer.end_offset(), 11);
        assert_eq!(buffer.replay_from(6).unwrap(), b"world");
        buffer.advance_start(6);
        assert_eq!(buffer.remaining_capacity(), 11);
        assert!(buffer.replay_from(0).is_none());
    }

    #[test]
    fn replay_buffer_replay_from_beyond_end_offset_is_none() {
        // The other boundary of `replay_from`'s range check (the "helper
        // claims to have committed bytes this client never even sent" case
        // `replay_and_advance` must treat as a protocol inconsistency, not
        // "nothing to replay" — codex review, quicmux-server-resume).
        let mut buffer = C2hReplayBuffer::new(16);
        buffer.append(b"hello");
        assert_eq!(buffer.end_offset(), 5);
        assert!(buffer.replay_from(6).is_none());
    }

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
    fn replay_buffer_backpressures_at_capacity() {
        let mut buffer = C2hReplayBuffer::new(4);
        buffer.append(b"abcd");

        assert!(buffer.is_full());
        assert_eq!(buffer.remaining_capacity(), 0);
        buffer.advance_start(2);
        assert!(!buffer.is_full());
        assert_eq!(buffer.replay_from(2).unwrap(), b"cd");
    }

    /// A `NetworkChangeMonitor` that fires exactly one event, then never
    /// resolves again — enough to prove `run_resume_loop`'s `tokio::select!`
    /// (`#20b`'s follow-on network-change wiring) actually treats a signal
    /// arriving *before* the data pump finishes as a reason to abandon the
    /// current connection and reconnect, without needing a real OS backend
    /// or a real QUIC connection to exercise that race in isolation.
    struct FireOnceNetworkChangeMonitor {
        fired: bool,
    }

    #[async_trait::async_trait]
    impl isekai_netmon::NetworkChangeMonitor for FireOnceNetworkChangeMonitor {
        async fn next_change(&mut self) -> Option<isekai_netmon::NetworkChangeEvent> {
            if self.fired {
                std::future::pending().await
            } else {
                self.fired = true;
                Some(isekai_netmon::NetworkChangeEvent)
            }
        }
    }

    #[tokio::test]
    async fn network_change_event_wins_the_race_against_a_pump_that_never_finishes() {
        let mut monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> =
            Box::new(FireOnceNetworkChangeMonitor { fired: false });
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
            Box::new(FireOnceNetworkChangeMonitor { fired: false });
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
            Box::new(FireOnceNetworkChangeMonitor { fired: false });
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
        let result: Result<(), FakeConnectError> = retry_while_busy_other_session(1, || {
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
        let result = retry_while_busy_other_session(1, || {
            let n = calls.get();
            calls.set(n + 1);
            async move { if n == 0 { Err(FakeConnectError(true)) } else { Ok::<(), FakeConnectError>(()) } }
        })
        .await;
        assert!(result.is_ok(), "a BUSY_OTHER_SESSION failure must be retried until it succeeds");
        assert_eq!(calls.get(), 2);
    }

    #[tokio::test]
    async fn retry_while_busy_other_session_gives_up_once_the_resume_window_elapses() {
        let result: Result<(), FakeConnectError> =
            retry_while_busy_other_session(1, || async { Err(FakeConnectError(true)) }).await;
        assert!(result.is_err(), "must stop retrying once the resume window has elapsed");
    }

    #[tokio::test]
    async fn spawn_reconnect_signal_does_not_forward_after_a_successful_rebind() {
        let monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> =
            Box::new(FireOnceNetworkChangeMonitor { fired: false });
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
            Box::new(FireOnceNetworkChangeMonitor { fired: false });
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
            let cert = rcgen::generate_simple_self_signed(vec!["isekai-pipe.local".to_string()]).unwrap();
            let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().clone());
            let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
            let cert_sha256_hex = {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(cert_der.as_ref());
                hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
            };
            let config = quicmux::MuxServerConfig {
                alpn: ALPN.to_vec(),
                exporter_label: EXPORTER_LABEL.to_vec(),
                max_idle_timeout: Duration::from_secs(15),
                keep_alive_interval: Duration::from_secs(5),
                max_concurrent_bidi_streams: 4,
                max_concurrent_uni_streams: 0,
                multipath: false,
                cert_chain: vec![cert_der],
                private_key: key_der,
            };
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
            replay.lock().unwrap().append(b"hello");

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
            replay.lock().unwrap().append(b"hello world");

            let ok = replay_and_advance(&replay, 6, &mut stream).await;
            assert!(ok);
            assert_eq!(replay.lock().unwrap().end_offset(), 11);
        }
    }
}
