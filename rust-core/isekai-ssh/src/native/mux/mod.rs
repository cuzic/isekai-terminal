//! `ControlMaster`-equivalent multiplexer for the Windows-native path: when
//! several tabs each run `isekai-ssh <host>` to the *same* fully-resolved
//! destination, exactly one process (the *owner*) holds the single
//! authenticated `russh` connection and every other process (a *client*)
//! reaches its own private remote shell through the owner over a
//! `local-ipc-mux` named-pipe channel, instead of each independently
//! re-authenticating a fresh SSH connection.
//!
//! Submodules: [`protocol`] (the SSH-specific frame codec), [`naming`] (how a
//! resolved config maps to a pipe name), [`owner`] (the accept loop + per-client
//! relay over the shared handle), [`client`] (the local terminal ↔ owner
//! relay). The generic dispatch ([`dispatch`]) is written against the
//! [`local_ipc_mux::ExclusiveChannel`] trait so it's unit-tested end-to-end
//! with `InMemoryChannel`; [`run`] is the one place that names the concrete
//! `WindowsNamedPipeChannel`.
//!
//! ## Relationship to the declined "standing QUIC broker" ADR
//!
//! `ISEKAI_PIPE_DESIGN.md`'s ADR *「複数isekai-sshプロセスによるisekai-pipe共有
//! (マルチプレクス)」* declined to build a standing QUIC broker for sharing an
//! `isekai-pipe` **transport** across processes, on the grounds that SSH's own
//! `ControlMaster`/`ControlPersist` (CLI) already solves it more simply — and
//! it listed an explicit reconsideration trigger: *「ControlMasterが使えない
//! クライアントが主要用途になった」*. Windows without a real `ssh(1)`/ControlMaster
//! is exactly that situation, so this feature deliberately revisits that
//! trigger.
//!
//! Crucially this is **a different kind of thing** from what the ADR declined:
//! it shares the SSH *protocol-layer* `client::Handle` (which multiplexes
//! independent channels natively), not a QUIC transport broker. The ADR's list
//! of costs it declined to pay still applies, and is addressed (or knowingly
//! accepted) here rather than dismissed:
//!
//! * **常駐broker / process lifecycle**: no separate daemon — the owner *is* a
//!   normal `isekai-ssh` tab that also serves siblings. Its lifetime is tied to
//!   its own foreground shell (see the "known limitation" below), so there is
//!   no daemon to supervise, upgrade, or reap.
//! * **ローカルIPC / multiplex protocol**: [`local_ipc_mux`] (named pipe, same-
//!   user ACL) plus this crate's small versioned frame protocol ([`protocol`]),
//!   with an explicit size cap, version field, and auth token.
//! * **crash recovery / re-election**: deliberately *not* an election. If the
//!   owner dies, each client's multiplexed shell is gone too, so a client just
//!   reports the loss and exits ([`client::ClientOutcome::OwnerLost`] →
//!   [`crate::EXIT_MUX_OWNER_LOST`]); a fresh `isekai-ssh <host>` becomes the
//!   new owner through the ordinary claim path.
//! * **session isolation**: every client gets an independent SSH shell channel;
//!   one client's error is logged and contained ([`owner::serve_clients`]),
//!   never propagated to siblings or the owner.
//! * **per-session flow control**: each relay is a single sequential loop per
//!   direction, so a slow client back-pressures only its own SSH channel (see
//!   [`owner`]'s module docs).
//! * **stale session cleanup**: an owner exit drops the handle (closing all
//!   channels); a client exit drops its pipe connection (the owner's relay task
//!   ends and closes that one channel). The token file is the only on-disk
//!   artifact and is best-effort unlinked by the owner on exit.
//!
//! **Known limitation (deferred)**: true `ControlPersist` — the shared
//! connection outliving the tab that created it — is *not* implemented. The
//! owner tears down when its own foreground shell exits, at which point
//! connected clients hit the owner-lost path and must reconnect (becoming the
//! new owner). Decoupling the master's lifetime from its initiator needs a
//! detached background master process, which is out of scope for this pass and
//! left as follow-up work.

pub(crate) mod build_relay;
pub(crate) mod client;
pub(crate) mod ctl_forward;
pub(crate) mod handoff;
pub(crate) mod holder;
pub(crate) mod naming;
pub(crate) mod owner;
pub(crate) mod protocol;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use local_ipc_mux::{ConnectError, ExclusiveChannel};

use crate::log_file::log_line;
use crate::native::connect::{self, OwnerHook, Prepared};
use holder::HolderSpawner;

/// How long the foreground process waits, after successfully spawning a
/// detached holder, for that holder to actually claim the channel and start
/// accepting before giving up and falling back to a plain direct connect — a
/// slow or failed holder must never block this tab from connecting at all
/// (the always-connects principle).
const HOLDER_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// `isekai-ssh <destination>` entrypoint on Windows: resolves the config,
/// then dispatches through the concrete named-pipe channel, auto-reconnecting
/// on [`DispatchOutcome::OwnerLost`] (see [`run_with_reconnect`]). Swapping in
/// a different [`ExclusiveChannel`] implementation later (e.g. a Unix one) is
/// the single concrete type here.
#[cfg(windows)]
pub(crate) async fn run(args: Vec<String>) -> Result<u8> {
    run_with_reconnect::<local_ipc_mux::WindowsNamedPipeChannel, _>(args, &holder::DetachedProcessSpawner, &crate::native::console::prompt_passphrase).await
}

/// How long a `EXIT_MUX_OWNER_LOST` (the mux holder this process was
/// relaying through died mid-session) is retried before giving up and
/// surfacing [`crate::EXIT_MUX_OWNER_LOST`] to the user after all — the
/// `always-connects.md` principle applied to a case this module's own docs
/// used to treat as unrecoverable (see [`run_with_reconnect`]'s docs).
/// Generous rather than a small fixed attempt count, matching
/// `isekai-pipe::resume_loop`'s own resume-window philosophy: a live
/// interactive session (this loop only ever runs while the user's terminal
/// is still open, per `RECONNECT_BACKOFF`'s own cancel path) is worth
/// reconnecting for a long time, not just a few seconds.
const RECONNECT_BUDGET: Duration = Duration::from_secs(24 * 60 * 60);

/// Backoff between `OwnerLost` reconnect attempts — same exponential shape
/// (no jitter) as `isekai-pipe::resume_loop::RESUME_BACKOFF`, reimplemented
/// locally rather than depending on `isekai-transport` (whose `BackoffPolicy`
/// lives in `isekai_transport::backoff`) purely to reuse ~10 lines of pure
/// math: that crate pulls in `noq`/`quicmux`/`timed-fsm` and the rest of the
/// QUIC transport stack, none of which `isekai-ssh`'s binary otherwise links
/// against — not worth the dependency weight for this.
struct ReconnectBackoff {
    initial: Duration,
    max: Duration,
    /// Fraction in `0.0..=1.0` of random jitter applied on top of the
    /// exponential delay (ADR_SLEEP_RESUME_MUX_OWNER_DEATH.md D-4) — same
    /// rationale and shape as `isekai_transport::backoff::BackoffPolicy`'s
    /// own `jitter` field (not reused directly: this struct exists
    /// specifically to avoid depending on `isekai-transport`, see its own
    /// doc comment above). `0.0` disables jitter entirely.
    jitter: f64,
}
const RECONNECT_BACKOFF: ReconnectBackoff = ReconnectBackoff { initial: Duration::from_millis(500), max: Duration::from_secs(10), jitter: 0.25 };

/// An `OwnerLost` attempt that stayed connected at least this long before
/// losing its (new) owner again counts as a genuinely separate, later
/// failure — not a continuation of the same reconnect storm — and resets
/// `attempt`/`lost_since` back to a fresh [`RECONNECT_BUDGET`] window (see
/// `run_with_reconnect`'s loop). Without this, `lost_since` stays pinned to
/// the *first-ever* owner loss for the rest of the process's life: a session
/// that reconnects successfully and runs happily for 23 hours before its
/// (new, unrelated) owner dies would only have ~1 hour of budget left,
/// instead of a fresh 24 hours, purely because the clock never restarted.
/// Comfortably above `RECONNECT_BACKOFF.max` so a run of purely back-to-back
/// failed attempts (each shorter than this) never spuriously resets the
/// budget that's meant to bound exactly that case.
const RECONNECT_STABLE_THRESHOLD: Duration = Duration::from_secs(60);

impl ReconnectBackoff {
    fn base_delay(&self, attempt: u32) -> Duration {
        let shift = attempt.min(32);
        let multiplier: u64 = 1u64 << shift;
        let initial_millis = u64::try_from(self.initial.as_millis()).unwrap_or(u64::MAX);
        let max_millis = u64::try_from(self.max.as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(initial_millis.saturating_mul(multiplier).min(max_millis))
    }

    /// `base_delay` with random jitter applied (mirrors
    /// `isekai_transport::backoff::BackoffPolicy::delay_for_attempt`) — a
    /// sleep/resume or a roaming network change wakes every open tab's
    /// reconnect loop at once, and a jitter-free exponential backoff would
    /// have them all retry (and silently re-deploy, `native::connect::
    /// drive_connect_recovery`) on the exact same schedule.
    fn delay_for_attempt(&self, attempt: u32) -> Duration {
        use rand::Rng as _;
        let base = self.base_delay(attempt);
        if self.jitter <= 0.0 {
            return base;
        }
        let jitter = self.jitter.min(1.0);
        let factor = 1.0 + rand::thread_rng().gen_range(-jitter..=jitter);
        let jittered_secs = (base.as_secs_f64() * factor).max(0.0);
        Duration::from_secs_f64(jittered_secs).min(self.max)
    }
}

/// [`run`]'s actual body, generic the same way [`dispatch`] is so it's unit
/// testable without the concrete Windows named-pipe channel.
///
/// On [`DispatchOutcome::OwnerLost`] — the mux holder this process was
/// relaying through died mid-session (`client::ClientRunResult::OwnerLost`'s
/// docs) — this used to surface [`crate::EXIT_MUX_OWNER_LOST`] immediately
/// and tell the user to reconnect by hand (this module's own former "no
/// special recovery code path" doc comment, from the original M4 plan).
/// That was fine for the case it was written for (the user closed their last
/// tab, so the whole owner process — tied to that tab's own foreground shell
/// at the time — tore down along with it, and there was nothing left to
/// reconnect *to* yet). It stopped being fine once the holder became a
/// genuinely detached background process (this module's "Known limitation
/// (deferred)" paragraph above is itself now stale — a re-exec'd holder has
/// no foreground shell of its own, see [`run_as_holder`]'s docs): an
/// unexpected holder death while tabs are still attached is now an
/// exceptional event (a crash, or the PR #65 class of stale-clock hang this
/// process's own `isekai-pipe connect` child could still hit while its
/// *own* resume loop gives up), not "the user is done", and the existing
/// `dispatch` `NotFound` arm already knows how to spawn a fresh holder and
/// become its client — exactly what a human manually retyping
/// `isekai-ssh <host>` would trigger. This function just does that
/// automatically, with backoff, instead of making the human do it.
///
/// A fresh [`connect::Prepared`] is resolved per attempt (`Prepared` isn't
/// `Clone` — it's a one-shot value `dispatch` consumes) via
/// `connect::prepare(args.clone())`, same as [`run`] used to do once. This
/// is safe to redo silently on every retry: by construction, this loop only
/// starts after a *first* `dispatch` attempt already reached
/// `DispatchOutcome::OwnerLost`, i.e. after an owner connection was already
/// established at least once, which means this exact destination's host key
/// is already in the trust store — `connect::prepare`'s
/// `TofuConfirmation::AlwaysPrompt` policy only actually prompts for a
/// destination `build_connection_intent` doesn't yet recognize, so a repeat
/// call for an already-trusted destination can't reach that prompt (see
/// `connect::prepare_with_tofu`'s docs). No stale passphrase hand-off is
/// reused either — each attempt resolves its own, same as a fresh manual
/// invocation would.
///
/// Does **not** hold a single [`console::RawModeGuard`] across the whole
/// loop (an earlier version of this function did) — that broke a brand-new
/// destination's TOFU confirmation prompt on the very first iteration
/// (`connect::prepare`'s own inline auto-bootstrap dial, or the direct-
/// connect fallback's SSH host-key confirmation inside `dispatch`, both run
/// in normal/cooked mode and need real line editing/echo), since raw mode
/// was enabled *before* either could run. Each attempt's own interactive
/// phase enables its own narrowly-scoped guard instead: `client::run` around
/// the mux-client relay, `native::connect::run_prepared` around a
/// single-process fallback's shell I/O loop (both already did this before
/// this loop existed), and [`wait_or_abort`] around the backoff wait itself
/// (see its docs for why that one specifically still needs raw mode). None
/// of these ever overlap in time, so there's no repeat of the "a second
/// nested enable/disable pair fights this one" hazard a single shared guard
/// was originally introduced to sidestep.
///
/// A `prepare` failure is only ever retried (rather than
/// propagated immediately) once this loop has already reached that state at
/// least once (`lost_since.is_some()`) — the very first call, before any
/// `OwnerLost` has happened, still fails fast on a genuine misconfiguration
/// (a malformed destination, `--isekai-no-bootstrap` against an unregistered
/// host) exactly as it always has, rather than retrying it for up to
/// [`RECONNECT_BUDGET`].
async fn run_with_reconnect<C, S>(
    args: Vec<String>,
    spawner: &S,
    prompt_passphrase: &(dyn Fn(&Path, u32) -> Option<String> + Send + Sync),
) -> Result<u8>
where
    C: ExclusiveChannel + Send + 'static,
    S: HolderSpawner,
{
    let mut attempt: u32 = 0;
    let mut lost_since: Option<tokio::time::Instant> = None;

    loop {
        let attempt_started = tokio::time::Instant::now();
        let prepared = match connect::prepare(args.clone()).await {
            Ok(prepared) => prepared,
            Err(e) if lost_since.is_some() => {
                // Already reconnected at least once before, so this exact
                // destination is known-trusted — a `prepare` failure here is
                // a transient hiccup (trust-store/log-file I/O, a momentary
                // DNS/network blip), not a fresh misconfiguration. Retry it
                // with the same budget/backoff as an `OwnerLost` cycle rather
                // than aborting the whole reconnect loop over it.
                log_line!("isekai-ssh: preparing to reconnect failed ({e:#}); retrying");
                match reconnect_backoff_or_give_up(&mut attempt, &mut lost_since).await {
                    ReconnectDecision::Retry => continue,
                    ReconnectDecision::GiveUp(code) => return Ok(code),
                }
            }
            Err(e) => return Err(e),
        };
        // Captured before `prepared` moves into `dispatch` below — used by
        // the `OwnerLost` arm to decide whether auto-retry is even safe (see
        // its own comment): re-running a *remote command* invocation
        // (`isekai-ssh host -- cmd`) from scratch after losing the owner
        // mid-execution isn't the same as reconnecting an interactive shell —
        // it could silently repeat a non-idempotent action, so it must not
        // auto-retry the way an interactive session does.
        let has_remote_command = prepared.plan().remote_command().is_some();
        let outcome = match dispatch::<C, S>(prepared, args.clone(), spawner, prompt_passphrase).await {
            Ok(outcome) => outcome,
            Err(e) if lost_since.is_some() => {
                // Same reasoning as the `prepare` failure arm above: once
                // we've already recovered from a lost owner at least once,
                // any further error out of `dispatch` (e.g. `client::run`
                // failing outright, or the direct-connect fallback it can
                // fall through to failing for a transient reason) is a
                // retryable hiccup, not a reason to give up the whole
                // reconnect loop — this was itself found still broken by a
                // second adversarial review pass after the `prepare`-only
                // fix above, the exact same bug class one level further down
                // the same function's error paths.
                log_line!("isekai-ssh: reconnect attempt failed ({e:#}); retrying");
                match reconnect_backoff_or_give_up(&mut attempt, &mut lost_since).await {
                    ReconnectDecision::Retry => continue,
                    ReconnectDecision::GiveUp(code) => return Ok(code),
                }
            }
            Err(e) => return Err(e),
        };
        match outcome {
            DispatchOutcome::Done(code) => return Ok(code),
            DispatchOutcome::OwnerLost => {
                if has_remote_command {
                    // A remote-command invocation was interrupted mid-flight
                    // by the owner dying — auto-retrying would silently
                    // re-run it from scratch, which is only safe for an
                    // idempotent command and this crate has no way to know
                    // whether it was one (found by adversarial review, 2026-08).
                    // Falls back to the pre-auto-retry behavior: surface the
                    // loss immediately and let the user judge whether
                    // reconnecting and rerunning is safe.
                    log_line!(
                        "isekai-ssh: connection to the isekai-ssh owner process was lost while running a remote \
                         command; not auto-retrying (rerunning it could repeat a non-idempotent action) — \
                         reconnect with `isekai-ssh <host> ...` if that's safe to do."
                    );
                    return Ok(crate::EXIT_MUX_OWNER_LOST);
                }
                if attempt_started.elapsed() >= RECONNECT_STABLE_THRESHOLD {
                    // This attempt ran long enough to count as a genuinely
                    // separate, later failure rather than a continuation of
                    // the same storm — see `RECONNECT_STABLE_THRESHOLD`'s docs.
                    attempt = 0;
                    lost_since = None;
                }
                log_line!("isekai-ssh: connection to the isekai-ssh owner process was lost; reconnecting...");
                match reconnect_backoff_or_give_up(&mut attempt, &mut lost_since).await {
                    ReconnectDecision::Retry => {}
                    ReconnectDecision::GiveUp(code) => return Ok(code),
                }
            }
        }
    }
}

enum ReconnectDecision {
    Retry,
    GiveUp(u8),
}

/// Shared bookkeeping for both places `run_with_reconnect`'s loop can decide
/// to retry (an `OwnerLost` dispatch outcome, or a `prepare` failure once
/// already mid-reconnect): checks the [`RECONNECT_BUDGET`] against
/// `lost_since` (starting the clock on first use), waits out the next
/// backoff delay (bumping `attempt`), and reports whether the caller should
/// loop again or give up.
async fn reconnect_backoff_or_give_up(attempt: &mut u32, lost_since: &mut Option<tokio::time::Instant>) -> ReconnectDecision {
    let lost_at = *lost_since.get_or_insert_with(tokio::time::Instant::now);
    if lost_at.elapsed() >= RECONNECT_BUDGET {
        log_line!(
            "isekai-ssh: automatic reconnection gave up after {RECONNECT_BUDGET:?} — reconnect with `isekai-ssh <host>`."
        );
        return ReconnectDecision::GiveUp(crate::EXIT_MUX_OWNER_LOST);
    }
    let delay = RECONNECT_BACKOFF.delay_for_attempt(*attempt);
    *attempt += 1;
    log_line!("isekai-ssh: reconnecting in {delay:?}...");
    if wait_or_abort(delay).await == WaitOutcome::Aborted {
        log_line!("isekai-ssh: reconnect canceled (Ctrl+C).");
        return ReconnectDecision::GiveUp(crate::EXIT_USER_CANCELED);
    }
    ReconnectDecision::Retry
}

#[derive(Debug, PartialEq, Eq)]
enum WaitOutcome {
    Elapsed,
    Aborted,
}

/// Waits out `delay`, but returns early as [`WaitOutcome::Aborted`] if the
/// user presses Ctrl+C (`0x03`) on stdin while waiting — otherwise, between
/// `OwnerLost` reconnect attempts, there is no live remote session to send
/// that byte *to*, so without this a stuck user's only way out is killing
/// the whole terminal window (`native/mux/client.rs`'s own module docs flag
/// this exact gap: no `SIGINT`/`ctrl_c`/`SetConsoleCtrlHandler` handling
/// exists anywhere in this crate). Any other byte read here (a stray
/// keypress while reconnecting, which has nowhere meaningful to go) is
/// discarded, not buffered — reusing
/// `console_stdin::ConsoleStdin`'s process-wide singleton reader (see its
/// module docs) so this doesn't race whatever the next attempt's own
/// `ConsoleStdin::open()` call does.
///
/// Enables its own [`console::RawModeGuard`], scoped to this wait only: this
/// runs strictly between two attempts (the previous one's own guard —
/// `client::run`'s or `native::connect::run_prepared`'s — has already
/// dropped, and the next attempt's `connect::prepare`/auth hasn't started
/// yet), so it never overlaps either. Without raw mode here, Ctrl+C during
/// the wait would be intercepted by the terminal as a real interrupt signal
/// instead of arriving as a plain `0x03` byte this function can catch and
/// turn into a clean [`crate::EXIT_USER_CANCELED`] exit. Best-effort: if
/// raw mode can't be enabled (non-interactive stdio), the wait still runs,
/// it just can't recognize Ctrl+C as an early-abort signal.
async fn wait_or_abort(delay: Duration) -> WaitOutcome {
    let _raw_mode = crate::native::console::RawModeGuard::enable().ok();
    wait_or_abort_over(delay, &mut crate::native::console_stdin::ConsoleStdin::open()).await
}

/// [`wait_or_abort`]'s actual logic with stdin injected, so it's testable
/// against a controlled stream instead of this test process's own real
/// stdin (whose state — a real console, a closed pipe, `/dev/null` — a test
/// can't control or rely on, unlike `client::run_inner`'s equivalent
/// injection for the same reason).
///
/// Loops rather than resolving on the first `select!` winner: a naive
/// single `select!` would let a closed/already-EOF stdin (`Ok(0)` resolves
/// immediately, never blocking) win the race against `sleep(delay)` on
/// *every* call, skipping the backoff delay entirely and hot-looping
/// reconnect attempts with no wait at all (found by real-world review: an
/// `isekai-ssh` invocation with redirected/closed stdin — a script, `< NUL`
/// — would never actually back off). A non-abort keypress is discarded and
/// the wait continues; an EOF or read error means stdin will never usefully
/// produce more input, so the remaining delay is just waited out directly
/// rather than re-issuing reads against a stream that can't ever satisfy
/// them.
async fn wait_or_abort_over<I: tokio::io::AsyncRead + Unpin>(delay: Duration, stdin: &mut I) -> WaitOutcome {
    use tokio::io::AsyncReadExt;

    let mut buf = [0u8; 64];
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return WaitOutcome::Elapsed,
            result = stdin.read(&mut buf) => {
                match result {
                    Ok(n) if n > 0 && buf[..n].contains(&0x03) => return WaitOutcome::Aborted,
                    Ok(0) | Err(_) => {
                        // EOF/error: nothing more this stream can ever tell
                        // us — stop polling it and just wait out the rest of
                        // the delay.
                        sleep.await;
                        return WaitOutcome::Elapsed;
                    }
                    Ok(_) => continue,
                }
            }
        }
    }
}

/// The holder re-exec entrypoint: `main.rs` calls this instead of [`run`] when
/// [`holder::is_holder_reexec`] is true. Claims the channel and serves clients
/// only — no foreground shell (see [`run_as_holder`]'s docs).
#[cfg(windows)]
pub(crate) async fn run_as_holder_entrypoint(args: Vec<String>) -> Result<u8> {
    // Read the passphrase hand-off (Phase 1b), if any, off this process's own
    // stdin *before* `connect::prepare` (a trust-store lookup that can
    // involve a network re-deploy dial) or `try_claim` — draining stdin
    // immediately is what lets the spawning client's own hand-off write
    // (`holder::DetachedProcessSpawner::spawn`, which writes on a dedicated
    // thread specifically so it never blocks on this process being slow to
    // read) actually complete promptly instead of sitting in the pipe buffer
    // until a slow `prepare` gets around to reading it. The spawning client
    // either wrote an encoded payload and closed its write end (EOF right
    // after), or (the common case: no encrypted identity in play) left
    // stdin null, which reads as EOF immediately too — `handoff::decode`
    // treats an empty read as an empty (no-op) set either way.
    let mut handoff_bytes = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut tokio::io::stdin(), &mut handoff_bytes)
        .await
        .context("isekai-ssh mux holder: failed to read the passphrase hand-off from stdin")?;
    let handoff = handoff::decode(&handoff_bytes).unwrap_or_else(|e| {
        log_line!("isekai-ssh mux holder: ignoring a malformed passphrase hand-off payload: {e:#}");
        handoff::HandoffCredentials::default()
    });

    // `Silent`, not `prepare`'s default `AlwaysPrompt`: a detached holder has
    // no console to confirm a brand-new host key on. See
    // `connect::prepare_with_tofu`'s docs for why this is normally a no-op
    // (the destination is already trusted by the time a holder is spawned)
    // and what happens in the rare case it isn't.
    let prepared = connect::prepare_with_tofu(args, crate::wrapper::TofuConfirmation::Silent).await?;
    let channel_name = naming::channel_name(prepared.host_config(), prepared.resolution(), prepared.plan().destination_host());
    let token_path = prepared.runtime_dir().join(naming::token_file_name(&channel_name));
    let holder_channel = local_ipc_mux::WindowsNamedPipeChannel::try_claim(&channel_name)
        .await
        .context("isekai-ssh mux holder: failed to claim the channel it was spawned to serve")?;
    run_as_holder(prepared, holder_channel, &token_path, handoff).await
}

/// What one full [`dispatch`] attempt concluded with — kept distinct from a
/// bare exit code (unlike `dispatch`'s previous `Result<u8>` return type) so
/// [`run_with_reconnect`] can tell "the owner died mid-session, worth
/// automatically retrying" apart from every other outcome, including a
/// remote shell that happens to itself exit with
/// [`crate::EXIT_MUX_OWNER_LOST`]'s numeric value — see
/// [`client::ClientRunResult::OwnerLost`]'s docs for why that distinction
/// has to survive past `client::run` in the first place.
enum DispatchOutcome {
    /// This attempt ran to some conclusion; this is this process's own exit
    /// code (the remote shell's real exit code, whether reached via the mux
    /// or a plain unmultiplexed connect).
    Done(u8),
    /// The mux owner this attempt was relaying through died mid-session —
    /// see [`run_with_reconnect`]'s docs on what the caller does about it.
    OwnerLost,
}

/// The role-selecting core, generic over the IPC channel (and the holder
/// spawner) so it's testable with `InMemoryChannel`/a fake spawner. The
/// foreground process is **always a client**: it never claims the channel
/// itself. If no holder is currently listening, it spawns a detached one
/// (Phase 1 `ControlPersist`-equivalent redesign — see this module's docs)
/// and retries as a client; any failure along the way (spawn failure, the
/// holder never coming up, a genuine pipe-infrastructure problem) falls back
/// to a plain single-process connect so a mux hiccup never blocks connecting
/// at all (the always-connects principle). A single call here only ever
/// tries once — [`run_with_reconnect`], the caller, is what loops on
/// [`DispatchOutcome::OwnerLost`].
async fn dispatch<C, S>(
    prepared: Prepared,
    holder_args: Vec<String>,
    spawner: &S,
    prompt_passphrase: &(dyn Fn(&Path, u32) -> Option<String> + Send + Sync),
) -> Result<DispatchOutcome>
where
    C: ExclusiveChannel + Send + 'static,
    S: HolderSpawner,
{
    let channel_name = naming::channel_name(prepared.host_config(), prepared.resolution(), prepared.plan().destination_host());
    let token_path = prepared.runtime_dir().join(naming::token_file_name(&channel_name));

    match C::connect(&channel_name).await {
        Ok(conn) => run_as_client_over(prepared, conn, &token_path, handoff::HandoffCredentials::default()).await,
        Err(ConnectError::NotFound { .. }) => {
            // Nobody to relay to yet. Several tabs opened at once against the
            // same fresh destination would all reach this branch
            // simultaneously — without coordination, each would redundantly
            // resolve the passphrase hand-off (re-prompting the user for the
            // *same* passphrase, once per tab) and spawn its own holder (the
            // losers harmlessly fail `try_claim` and exit, but only after
            // wastefully re-decrypting first). `SpawnLock` is a best-effort
            // cross-process mutex over exactly this "resolve hand-off + spawn"
            // critical section: the one tab that acquires it is the sole
            // "spawn leader"; every other concurrent tab skips straight to
            // waiting for *that* leader's holder to come up.
            let lock_path = prepared.runtime_dir().join(naming::spawn_lock_file_name(&channel_name));
            let spawn_lock = SpawnLock::try_acquire(lock_path, HOLDER_STARTUP_TIMEOUT);
            if !spawn_lock.acquired {
                log_line!("isekai-ssh: another tab is already spawning a mux holder for this destination; waiting for it");
                return match connect_with_retry::<C>(&channel_name, HOLDER_STARTUP_TIMEOUT).await {
                    Ok(conn) => run_as_client_over(prepared, conn, &token_path, handoff::HandoffCredentials::default()).await,
                    Err(e) => {
                        log_line!("isekai-ssh: no mux holder ever became reachable ({e}); connecting directly");
                        connect::run_prepared(prepared, None, handoff::HandoffCredentials::default()).await.map(DispatchOutcome::Done)
                    }
                };
            }

            // This tab is the spawn leader — it (unlike this still-interactive
            // process's own SSH auth for a plain single-process connect) can
            // never prompt for a passphrase-protected identity's passphrase
            // once detached. Resolve the whole hand-off set *now*, while we
            // still can (Phase 1b — see `handoff`'s module docs). Cheap and
            // prompt-free when there's no encrypted identity in play:
            // `resolve_handoff_credentials` never prompts for a key it hasn't
            // first confirmed is encrypted.
            let home = isekai_fs_guard::resolve_home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
            let resolved_handoff = handoff::resolve_handoff_credentials(prepared.host_config(), &home, prompt_passphrase);
            let encoded_handoff = (!resolved_handoff.is_empty()).then(|| handoff::encode(&resolved_handoff));

            if let Err(e) = spawner.spawn(&holder_args, encoded_handoff.as_deref().map(|z| z.as_slice())) {
                log_line!("isekai-ssh: failed to spawn a detached mux holder ({e}); connecting directly");
                drop(spawn_lock);
                return connect::run_prepared(prepared, None, resolved_handoff).await.map(DispatchOutcome::Done);
            }
            let result = match connect_with_retry::<C>(&channel_name, HOLDER_STARTUP_TIMEOUT).await {
                Ok(conn) => run_as_client_over(prepared, conn, &token_path, resolved_handoff).await,
                Err(e) => {
                    log_line!("isekai-ssh: the detached mux holder never came up ({e}); connecting directly");
                    connect::run_prepared(prepared, None, resolved_handoff).await.map(DispatchOutcome::Done)
                }
            };
            // Held across the whole spawn+wait window (not just the spawn
            // call itself) so a follower's own retry loop above has the
            // leader's holder to find for as long as the leader is willing to
            // wait for it — releasing any earlier would let a follower give up
            // and redundantly spawn its own holder while this one might still
            // be about to come up.
            drop(spawn_lock);
            result
        }
        // A transient local-pipe busy/I/O condition — *not necessarily* "no
        // holder to reach": a holder can already exist but not yet be
        // calling `accept()` at all (still mid SSH handshake/authentication,
        // before its own `OwnerHook` fires `serve_clients` — see
        // `run_as_holder`'s docs), during which its one pre-created pipe
        // instance can be fully occupied by another tab that raced ahead of
        // us. `WindowsNamedPipeChannel::connect` already retries
        // `ERROR_PIPE_BUSY` briefly on its own (see its docs) but only for a
        // few hundred milliseconds — far shorter than a real SSH handshake
        // can take — so this surfaces here well before a holder that's
        // genuinely on its way up would ever get the chance to start
        // accepting. Retry patiently for the same budget a freshly-spawned
        // holder gets (`connect_with_retry`, which retries on this exact
        // error too) before concluding it's a genuine pipe-infrastructure
        // problem and falling back — per the always-connects principle, a
        // mux hiccup must never block connecting, but it also shouldn't
        // needlessly give up on multiplexing within milliseconds of a
        // legitimately-busy (not broken) pipe.
        Err(ConnectError::Io { source, .. }) => {
            log_line!("isekai-ssh: local mux channel busy/unavailable ({source}); retrying before falling back");
            match connect_with_retry::<C>(&channel_name, HOLDER_STARTUP_TIMEOUT).await {
                Ok(conn) => run_as_client_over(prepared, conn, &token_path, handoff::HandoffCredentials::default()).await,
                Err(e) => {
                    log_line!("isekai-ssh: local mux channel still unavailable after retrying ({e}); connecting directly without multiplexing");
                    connect::run_prepared(prepared, None, handoff::HandoffCredentials::default()).await.map(DispatchOutcome::Done)
                }
            }
        }
    }
}

/// Retries [`ExclusiveChannel::connect`] while it keeps reporting
/// [`ConnectError::NotFound`] (nobody has claimed the channel yet) *or*
/// [`ConnectError::Io`] (the channel exists but is momentarily busy — e.g. a
/// holder that claimed it hasn't started calling `accept()` yet, still mid
/// SSH handshake/authentication; see `dispatch`'s `Err(ConnectError::Io {..})`
/// arm), giving up once `deadline` has elapsed. Any other error (or either of
/// these past the deadline) is returned to the caller, which falls back to a
/// plain direct connect.
async fn connect_with_retry<C>(channel_name: &str, deadline: Duration) -> Result<C::Connection, ConnectError>
where
    C: ExclusiveChannel,
{
    let start = tokio::time::Instant::now();
    loop {
        match C::connect(channel_name).await {
            Ok(conn) => return Ok(conn),
            Err(ConnectError::NotFound { .. }) | Err(ConnectError::Io { .. }) if start.elapsed() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Claimed by no one *yet*: this process's foreground path spawned a detached
/// holder (or is a re-exec'd holder itself) which claims the channel, writes
/// the per-session auth token, then runs the ordinary connect+auth+recovery
/// with an [`OwnerHook`] that starts accepting clients the moment the shared
/// session authenticates. Unlike a plain single-process connect, this process
/// opens **no foreground shell of its own** — [`connect::run_prepared`]
/// returns as soon as the accept loop itself ends (idle-exit or a fatal
/// local-IPC error), which is this function's entire body once the hook
/// fires (see [`OwnerHook`]'s docs on why `run_authenticated_session` skips
/// the shell in this mode).
async fn run_as_holder<C>(prepared: Prepared, holder_channel: C, token_path: &Path, handoff: handoff::HandoffCredentials) -> Result<u8>
where
    C: ExclusiveChannel + Send + 'static,
{
    let token = Arc::new(write_owner_token(token_path)?);
    let cleanup_path = token_path.to_path_buf();
    // Captured before `prepared` moves into `connect::run_prepared` below —
    // `Option<(u8, u8, u8)>` is `Copy`, so this is a cheap read, not a clone
    // of anything expensive.
    let tab_idle_color = prepared.resolution().tab_idle_color();
    let tab_attention_color = prepared.resolution().tab_attention_color();
    let hook: OwnerHook = Box::new(move |handle, ctl_routes| {
        tokio::spawn(async move {
            if let Err(e) =
                owner::serve_clients(holder_channel, handle, token, ctl_routes, tab_idle_color, tab_attention_color).await
            {
                log_line!("isekai-ssh mux holder: the client accept loop ended: {e:#}");
            }
        })
    });
    let result = connect::run_prepared(prepared, Some(hook), handoff).await;
    // Best-effort: don't leave the token file behind once this holder exits.
    let _ = std::fs::remove_file(&cleanup_path);
    result
}

/// Relays this terminal to an already-connected holder. If the holder rejects
/// the connection before any shell session existed, falls back to a plain
/// single-process connect — reusing `handoff` (the passphrase hand-off set
/// this process already resolved before spawning that holder, if any) so a
/// user is never prompted for the same passphrase twice in one invocation
/// (empty when this connect went straight to an already-live holder, i.e. no
/// hand-off was ever resolved in the first place). Returns
/// [`DispatchOutcome::OwnerLost`], not an exit code, if the holder died
/// mid-session — see [`run_with_reconnect`]'s docs for what the caller does
/// with that.
async fn run_as_client_over<Conn>(prepared: Prepared, conn: Conn, token_path: &Path, handoff: handoff::HandoffCredentials) -> Result<DispatchOutcome>
where
    Conn: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Extracted before `prepared` potentially moves into `connect::run_prepared`
    // below (the fallback branches take it by value) — this is the same host/
    // profile identity Epic P Phase 2's build-profile lookup uses on the Unix
    // path (`resolution.profile()`).
    let host = prepared.resolution().profile().to_string();
    // `--isekai-tty` is silently ignored when the caller already gave an
    // explicit trailing remote command — same opportunistic convention as
    // every other consumer of `crate::wrapper::resolved_tty_selection` (see
    // its doc comment).
    let tty_exec = if prepared.plan().remote_command().is_none() {
        crate::tty_attach::resolve_exec_command(
            crate::wrapper::resolved_tty_selection(prepared.plan(), prepared.resolution()).as_ref(),
            prepared.resolution().profile(),
        )
    } else {
        None
    };
    // Same `-t`/`-T` intent the non-mux path derives via
    // `decide_session_kind`/`wants_pty` (`connect.rs`), sent to the owner in
    // `Hello` so it can make the identical decision — see `client::run`'s
    // doc comment. `--isekai-tty` always forces a PTY (`isekai-pipe tty
    // attach` needs one to relay through), independent of that computation —
    // same as the Unix `ssh(1)` path forcing `-t` in `apply_ctl_socket_forward`.
    let want_pty = tty_exec.is_some() || connect::wants_pty(prepared.plan().remote_command(), prepared.plan().request_tty);
    let remote_command = prepared.plan().remote_command().map(|cmd| cmd.join(" "));
    let token = match read_owner_token_or_fall_back(token_path) {
        ClientToken::Ready(token) => token,
        // The holder released its claim (or hadn't finished writing the token
        // file) in the race between our successful connect and now. A mux
        // hiccup must never block connecting (the always-connects
        // principle) — dial SSH ourselves, unmultiplexed.
        ClientToken::FallBack => return connect::run_prepared(prepared, None, handoff).await.map(DispatchOutcome::Done),
    };
    match client::run(conn, &token, host, remote_command, want_pty, tty_exec).await? {
        client::ClientRunResult::ExitCode(code) => Ok(DispatchOutcome::Done(code)),
        client::ClientRunResult::OwnerLost => Ok(DispatchOutcome::OwnerLost),
        // The holder rejected us before any shell session existed (protocol
        // version mismatch, or a stale token read in the window before a new
        // holder rewrote it — see `ClientOutcome::Rejected`'s docs, and — the
        // case `handoff` actually matters for — the holder's own SSH auth
        // failing before it ever reaches `serve_clients`). Nothing was lost,
        // so it's always safe to fall back to a fresh unmultiplexed connect
        // rather than fail this invocation outright.
        client::ClientRunResult::Rejected { reason } => {
            log_line!("isekai-ssh: the mux holder rejected this connection ({reason}); connecting directly");
            connect::run_prepared(prepared, None, handoff).await.map(DispatchOutcome::Done)
        }
    }
}

/// Best-effort cross-process mutex over the "resolve passphrase hand-off +
/// spawn a detached holder" critical section for one destination (see
/// `dispatch`'s `NotFound` arm) — closes the window where several tabs opened
/// at once against the same fresh destination would each redundantly
/// re-prompt the user for the same passphrase and each spawn their own
/// (mostly-losing) holder. Backed by an exclusively-created lock file
/// (`std::fs::OpenOptions::create_new`'s atomicity, same primitive
/// [`write_owner_token`]'s sibling token file doesn't need since nothing
/// *races* to create that one).
struct SpawnLock {
    path: PathBuf,
    /// Whether *this* value is the one holding the lock — only it removes
    /// the file on drop, so a value that lost the race never deletes a
    /// still-active leader's lock out from under it.
    acquired: bool,
}

impl SpawnLock {
    /// A lock older than `stale_after` is treated as abandoned (a leader that
    /// crashed — or was killed — before it could clean up after itself) and
    /// is reclaimed rather than left to starve every future invocation for
    /// this destination forever. `stale_after` is the same budget a holder
    /// gets to come up (`HOLDER_STARTUP_TIMEOUT`) — a lock that's outlived
    /// that can no longer represent a leader still usefully waiting.
    fn try_acquire(path: PathBuf, stale_after: Duration) -> Self {
        if Self::create_new(&path) {
            return Self { path, acquired: true };
        }
        if Self::is_stale(&path, stale_after) {
            let _ = std::fs::remove_file(&path);
            if Self::create_new(&path) {
                return Self { path, acquired: true };
            }
        }
        Self { path, acquired: false }
    }

    fn create_new(path: &Path) -> bool {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::OpenOptions::new().write(true).create_new(true).open(path).is_ok()
    }

    fn is_stale(path: &Path, stale_after: Duration) -> bool {
        let Ok(metadata) = std::fs::metadata(path) else { return false };
        let Ok(modified) = metadata.modified() else { return false };
        modified.elapsed().map(|age| age > stale_after).unwrap_or(false)
    }
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        if self.acquired {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Generates a fresh 32-byte token and writes it where only the owning OS user
/// can read it. On Unix the file is chmod 0600 (belt-and-suspenders for the
/// Linux test build); on Windows the runtime dir already lives under the user
/// profile, so the named pipe's same-user ACL is the primary control and this
/// token is defense-in-depth beneath it.
fn write_owner_token(path: &Path) -> Result<Vec<u8>> {
    use rand::RngCore as _;
    let mut token = vec![0u8; 32];
    rand::thread_rng().fill_bytes(&mut token);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating mux token dir {}", parent.display()))?;
    }
    std::fs::write(path, &token).with_context(|| format!("writing mux token file {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on mux token file {}", path.display()))?;
    }
    Ok(token)
}

/// Whether a would-be client obtained the owner's token, or must fall back to a
/// plain single-process connect.
enum ClientToken {
    /// The token was read — connect to the owner and relay to it.
    Ready(Vec<u8>),
    /// The token couldn't be read (the owner released its claim, or hadn't
    /// finished writing the token file, in the claim race). Per the
    /// always-connects principle a mux hiccup must never block connecting, so
    /// the caller connects directly (unmultiplexed) instead of failing.
    FallBack,
}

/// Reads the owner's token, degrading to [`ClientToken::FallBack`] (logging the
/// cause) rather than erroring when it can't be read — so a lost/racing owner
/// never turns a would-be client into a hard connect failure.
fn read_owner_token_or_fall_back(path: &Path) -> ClientToken {
    match read_owner_token(path) {
        Ok(token) => ClientToken::Ready(token),
        Err(e) => {
            log_line!("isekai-ssh: could not read the mux owner's auth token ({e:#}); connecting directly");
            ClientToken::FallBack
        }
    }
}

/// Reads the owner's token, retrying briefly to cover the small window where a
/// client's claim failed but the freshly-elected owner hasn't finished writing
/// the token file yet.
fn read_owner_token(path: &Path) -> Result<Vec<u8>> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match std::fs::read(path) {
            Ok(token) => return Ok(token),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(anyhow::Error::new(e).context(format!("reading mux token file {}", path.display()))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use local_ipc_mux::{ClaimError, InMemoryChannel};
    use russh::client;
    use russh::server::{self, Auth, Msg as ServerMsg, Server as _, Session as ServerSession};
    use russh::{Channel as RusshChannel, CryptoVec};
    use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey as SshPrivateKey};
    use russh_stream_session::{authenticate_session, establish_over_stream, verifying_handler, Credential, HostKeyVerifier, VerifyOutcome};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    struct AcceptAllHostKeys;
    #[async_trait]
    impl HostKeyVerifier for AcceptAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> VerifyOutcome {
            VerifyOutcome::Accepted
        }
    }

    #[derive(Clone)]
    struct EchoShellServer;
    impl server::Server for EchoShellServer {
        type Handler = EchoShellHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> EchoShellHandler {
            EchoShellHandler
        }
    }
    #[derive(Clone)]
    struct EchoShellHandler;
    #[async_trait]
    impl server::Handler for EchoShellHandler {
        type Error = russh::Error;
        async fn auth_password(&mut self, _u: &str, _p: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }
        async fn channel_open_session(&mut self, _c: RusshChannel<ServerMsg>, _s: &mut ServerSession) -> Result<bool, Self::Error> {
            Ok(true)
        }
        async fn shell_request(&mut self, channel: russh::ChannelId, session: &mut ServerSession) -> Result<(), Self::Error> {
            session.data(channel, CryptoVec::from(b"ready\n".to_vec()))?;
            Ok(())
        }
        // Echo stdin back, then cleanly end the session so the client's relay
        // terminates deterministically (no timeout) with a real Exit(0).
        async fn data(&mut self, channel: russh::ChannelId, data: &[u8], session: &mut ServerSession) -> Result<(), Self::Error> {
            session.data(channel, CryptoVec::from(data.to_vec()))?;
            session.exit_status_request(channel, 0)?;
            session.close(channel)?;
            Ok(())
        }
    }

    async fn authed_handle() -> client::Handle<russh_stream_session::VerifyingHandler<AcceptAllHostKeys>> {
        let keypair = Ed25519Keypair::from_seed(&[130; 32]);
        let host_key = SshPrivateKey::from(keypair);
        let config = Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut server = EchoShellServer;
        tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });

        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let handler = verifying_handler(&verifier);
        let mut handle = establish_over_stream(Arc::new(client::Config::default()), stream, handler).await.unwrap();
        assert!(authenticate_session(&mut handle, "tester", &Credential::Password("x".to_string())).await.unwrap());
        handle
    }

    /// The full owner+client path over `InMemoryChannel`: an owner serves an
    /// accept loop on a real (mock) SSH handle; a client connects through the
    /// channel, drives `client::run_inner` with canned stdin, and receives the
    /// remote shell banner plus its echoed stdin relayed all the way back —
    /// proving the two halves interoperate through the actual frame protocol.
    #[tokio::test]
    async fn owner_and_client_relay_end_to_end_over_in_memory_channel() {
        let name = "isekai-ssh-mux-e2e-test";
        let token = Arc::new(b"shared-secret-token".to_vec());
        let handle = authed_handle().await;

        let owner_channel = InMemoryChannel::try_claim(name).await.unwrap();
        let serve_token = token.clone();
        tokio::spawn(async move {
            let _ =
                owner::serve_clients(owner_channel, Arc::new(tokio::sync::Mutex::new(handle)), serve_token, None, None, None)
                    .await;
        });

        // A second try_claim must fail (owner exists) — the real dispatch's
        // signal to become a client.
        assert!(matches!(InMemoryChannel::try_claim(name).await, Err(ClaimError::AlreadyClaimed { .. })));

        let conn = InMemoryChannel::connect(name).await.unwrap();
        let (cr, mut cw) = tokio::io::split(conn);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        // Drive the real client relay: it sends Hello, streams "hello\n" then
        // EOF, and receives the banner + echoed stdin back before the mock
        // shell cleanly exits (Exit(0)). No timeout: the server ends the
        // session deterministically after echoing.
        // `super::client` (the mux client module), not `russh::client` which
        // is imported as `client` above for `client::Handle`.
        let outcome = super::client::run_inner(
            cr, &mut cw, &token, "xterm".to_string(), 80, 24, &b"hello\n"[..], &mut stdout, &mut stderr, None, "mybox".to_string(), None, true, None,
        )
        .await
        .unwrap();

        assert_eq!(outcome, super::client::ClientOutcome::Exited(0), "a clean remote exit must reach the client as Exited(0)");
        assert!(
            stdout.windows(6).any(|w| w == b"ready\n"),
            "the remote banner must be relayed to the client's stdout, saw {:?}",
            String::from_utf8_lossy(&stdout)
        );
        assert!(
            stdout.windows(6).any(|w| w == b"hello\n"),
            "the client's stdin must be echoed back through the remote shell, saw {:?}",
            String::from_utf8_lossy(&stdout)
        );
    }

    /// A missing owner token (the owner released its claim / hadn't written the
    /// file in the claim race) must degrade to a fall-back single-process
    /// connect, not a hard error — the always-connects principle for a mux
    /// hiccup. Guards `run_as_client`'s token-read step.
    #[test]
    fn a_missing_owner_token_falls_back_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such.token");
        assert!(
            matches!(read_owner_token_or_fall_back(&missing), ClientToken::FallBack),
            "a token that can't be read must fall back to a direct connect, never fail"
        );
    }

    /// The happy path still yields the real token so a client relays to the
    /// owner rather than needlessly falling back.
    #[test]
    fn a_present_owner_token_is_used_rather_than_falling_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mux.token");
        let written = write_owner_token(&path).unwrap();
        match read_owner_token_or_fall_back(&path) {
            ClientToken::Ready(token) => assert_eq!(token, written, "the token used must be the one on disk"),
            ClientToken::FallBack => panic!("a readable token must be used, not fall back to a direct connect"),
        }
    }

    #[test]
    fn token_write_then_read_round_trips_and_is_restricted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("mux.token");
        let written = write_owner_token(&path).unwrap();
        assert_eq!(written.len(), 32, "token must be 32 bytes");
        let read = read_owner_token(&path).unwrap();
        assert_eq!(written, read, "the token read back must match what was written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the token file must be owner-only (0600)");
        }
    }

    // -- connect_with_retry ---------------------------------------------

    #[tokio::test]
    async fn connect_with_retry_succeeds_immediately_when_a_holder_is_already_there() {
        let name = "isekai-ssh-mux-retry-immediate-test";
        // `connect` requires no `accept` on the other side to succeed at this
        // layer (see `InMemoryChannel::connect`'s implementation) — the claim
        // just needs to stay alive for the channel to be reachable, so
        // leaking it (never dropped) keeps it alive without an explicit
        // `accept` loop this test doesn't need.
        let owner_channel = InMemoryChannel::try_claim(name).await.unwrap();
        std::mem::forget(owner_channel);
        let result = connect_with_retry::<InMemoryChannel>(name, Duration::from_secs(1)).await;
        assert!(result.is_ok(), "an already-claimed channel must connect on the first try, not retry");
    }

    #[tokio::test]
    async fn connect_with_retry_succeeds_once_a_holder_claims_the_channel_mid_wait() {
        tokio::time::pause();
        let name = "isekai-ssh-mux-retry-eventual-test";
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _channel = InMemoryChannel::try_claim(name).await.expect("the simulated holder must win the claim");
            // Held alive for the rest of the test via this task's own scope.
            std::future::pending::<()>().await;
        });
        let result = connect_with_retry::<InMemoryChannel>(name, Duration::from_secs(5)).await;
        assert!(result.is_ok(), "a holder claiming the channel mid-wait must eventually be reachable, not time out");
    }

    #[tokio::test]
    async fn connect_with_retry_gives_up_once_the_deadline_elapses() {
        tokio::time::pause();
        let name = "isekai-ssh-mux-retry-never-test";
        let result = connect_with_retry::<InMemoryChannel>(name, Duration::from_millis(200)).await;
        assert!(matches!(result, Err(ConnectError::NotFound { .. })), "a channel nobody ever claims must give up past the deadline, not hang forever");
    }

    /// A test-only `ExclusiveChannel` that reports [`ConnectError::Io`] for a
    /// configurable number of `connect(name)` calls before delegating to a
    /// real [`InMemoryChannel`] — standing in for a real Windows named pipe
    /// returning `ERROR_PIPE_BUSY` while a holder has claimed the channel but
    /// hasn't started calling `accept()` yet (still mid SSH handshake/auth).
    /// State is keyed by `name` in a process-global registry (mirroring
    /// `InMemoryChannel`'s own registry) so concurrent tests using distinct
    /// names don't interfere.
    struct FlakyIoChannel(InMemoryChannel);

    fn flaky_io_countdowns() -> &'static std::sync::Mutex<std::collections::HashMap<String, usize>> {
        static COUNTDOWNS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, usize>>> = std::sync::OnceLock::new();
        COUNTDOWNS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    /// The next `busy_calls` calls to `FlakyIoChannel::connect(name)` return
    /// `Io`; every call after that delegates to `InMemoryChannel::connect`.
    fn set_flaky_io_countdown(name: &str, busy_calls: usize) {
        flaky_io_countdowns().lock().unwrap().insert(name.to_string(), busy_calls);
    }

    #[async_trait]
    impl ExclusiveChannel for FlakyIoChannel {
        type Connection = <InMemoryChannel as ExclusiveChannel>::Connection;

        async fn try_claim(name: &str) -> Result<Self, ClaimError> {
            InMemoryChannel::try_claim(name).await.map(FlakyIoChannel)
        }

        async fn accept(&mut self) -> std::io::Result<Self::Connection> {
            self.0.accept().await
        }

        async fn connect(name: &str) -> Result<Self::Connection, ConnectError> {
            // Scoped so the (non-`Send`) `MutexGuard` is fully dropped before
            // the `.await` below — `async_trait` boxes this function's body
            // as a single `Send` future, and a guard that's merely
            // `drop()`-called (rather than falling out of its own block
            // scope) can still be considered live across the await by the
            // generator transform.
            let busy = {
                let mut countdowns = flaky_io_countdowns().lock().unwrap();
                match countdowns.get_mut(name) {
                    Some(remaining) if *remaining > 0 => {
                        *remaining -= 1;
                        true
                    }
                    _ => false,
                }
            };
            if busy {
                return Err(ConnectError::Io { name: name.to_string(), source: std::io::Error::other("simulated ERROR_PIPE_BUSY") });
            }
            InMemoryChannel::connect(name).await
        }
    }

    /// The gap `connect_with_retry`'s `Io`-retry closes: a holder that has
    /// claimed the channel but hasn't started `accept()`ing yet (still mid
    /// SSH handshake/auth) can make a concurrent tab's connect attempt see a
    /// transient busy/`Io` error — that must be retried, not treated as an
    /// immediate "give up and connect directly" signal.
    #[tokio::test]
    async fn connect_with_retry_recovers_from_a_transient_busy_pipe_error() {
        let name = "isekai-ssh-mux-retry-flaky-io-test";
        // A channel must actually be claimed for the eventual real
        // `InMemoryChannel::connect` (once the simulated busy countdown runs
        // out) to succeed — mirrors `connect_with_retry_succeeds_immediately_
        // when_a_holder_is_already_there`'s leak-to-keep-alive pattern.
        let owner_channel = FlakyIoChannel::try_claim(name).await.unwrap();
        std::mem::forget(owner_channel);
        set_flaky_io_countdown(name, 3);
        let result = connect_with_retry::<FlakyIoChannel>(name, Duration::from_secs(5)).await;
        assert!(result.is_ok(), "a transient busy/Io error must be retried and eventually succeed, not immediately surface");
    }

    #[tokio::test]
    async fn connect_with_retry_gives_up_on_a_persistently_busy_pipe_past_the_deadline() {
        tokio::time::pause();
        let name = "isekai-ssh-mux-retry-persistent-io-test";
        set_flaky_io_countdown(name, usize::MAX);
        let result = connect_with_retry::<FlakyIoChannel>(name, Duration::from_millis(200)).await;
        assert!(matches!(result, Err(ConnectError::Io { .. })), "a channel that's never anything but busy must still give up past the deadline, not hang forever");
    }

    // -- SpawnLock ---------------------------------------------------------

    #[test]
    fn spawn_lock_try_acquire_succeeds_when_unclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("some.spawning.lock");
        let lock = SpawnLock::try_acquire(path.clone(), Duration::from_secs(10));
        assert!(lock.acquired, "an unclaimed lock path must be acquirable");
        assert!(path.exists(), "acquiring the lock must create the lock file");
    }

    #[test]
    fn spawn_lock_try_acquire_fails_while_another_holder_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("some.spawning.lock");
        let _first = SpawnLock::try_acquire(path.clone(), Duration::from_secs(10));
        let second = SpawnLock::try_acquire(path, Duration::from_secs(10));
        assert!(!second.acquired, "a second attempt while the first still (recently) holds the lock must fail");
    }

    #[test]
    fn spawn_lock_is_released_on_drop_and_then_reacquirable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("some.spawning.lock");
        {
            let _lock = SpawnLock::try_acquire(path.clone(), Duration::from_secs(10));
            assert!(path.exists());
        }
        assert!(!path.exists(), "dropping the lock must remove the lock file");
        let reacquired = SpawnLock::try_acquire(path, Duration::from_secs(10));
        assert!(reacquired.acquired, "after the previous holder drops, a fresh attempt must succeed");
    }

    #[test]
    fn spawn_lock_reclaims_a_stale_lock_left_by_a_crashed_leader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("some.spawning.lock");
        std::fs::write(&path, b"").unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(std::time::SystemTime::now() - Duration::from_secs(3600)).unwrap();

        let lock = SpawnLock::try_acquire(path, Duration::from_secs(10));
        assert!(lock.acquired, "a lock file far older than the staleness threshold must be reclaimed, not block every future invocation forever");
    }

    #[test]
    fn spawn_lock_does_not_reclaim_a_fresh_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("some.spawning.lock");
        let _first = SpawnLock::try_acquire(path.clone(), Duration::from_secs(600));
        let second = SpawnLock::try_acquire(path, Duration::from_secs(600));
        assert!(!second.acquired, "a fresh, still-plausibly-live lock must not be reclaimed just because a second attempt raced it");
    }

    // -- dispatch fallback sequencing ------------------------------------

    /// Builds a throwaway `Prepared` for `dispatch` tests: a bogus
    /// `--isekai-pipe-path` makes the ultimate fallback (`connect::run_prepared`)
    /// fail fast and deterministically instead of hanging on a real network
    /// dial, mirroring `native/connect.rs`'s own `bogus_pipe`-based recovery
    /// tests.
    fn test_prepared(destination: &str, runtime_dir: &std::path::Path) -> connect::Prepared {
        use isekai_pipe_core::{BootstrapProvenance, ConnectionIntent, IntentTransport, ServerIdentity};

        let bogus_pipe = std::env::temp_dir().join(format!("isekai-mux-dispatch-test-nonexistent-pipe-binary-{destination}"));
        let plan = crate::wrapper::parse_wrapper(vec!["--isekai-pipe-path".to_string(), bogus_pipe.display().to_string(), destination.to_string()])
            .expect("parse_wrapper");
        let (resolution, host_config) = crate::wrapper::resolve_for_native(&plan).expect("resolve_for_native");
        let intent = ConnectionIntent::new(
            destination,
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::Relay {
                helper_addr: "203.0.113.5:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "example.com:22".to_string() },
        );
        connect::Prepared::for_test(plan, resolution, host_config, intent, runtime_dir.to_path_buf())
    }

    /// A holder spawn failure must degrade `dispatch` to a plain direct
    /// connect, never a hard error of its own or a hang — the always-connects
    /// principle for a mux hiccup. The direct connect itself then fails too
    /// here (the bogus pipe path), but that's `test_prepared`'s own
    /// deliberately-unreachable target, not the mux layer misbehaving.
    #[tokio::test]
    async fn dispatch_falls_back_to_a_direct_connect_when_spawning_the_holder_fails() {
        let runtime_dir = tempfile::tempdir().unwrap();
        let prepared = test_prepared("dispatch-spawn-failure-host", runtime_dir.path());
        let spawner = holder::tests_support::RecordingSpawner::failing();

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            dispatch::<InMemoryChannel, _>(prepared, vec!["dispatch-spawn-failure-host".to_string()], &spawner, &|_path, _attempt| None),
        )
        .await
        .expect("dispatch must not hang when the holder spawn itself fails");

        assert!(result.is_err(), "the fallback direct connect against the bogus pipe path must still fail here, but via the fallback, not a hang");
        let calls = spawner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "dispatch must attempt to spawn a holder exactly once before falling back");
        assert!(calls[0].1.is_none(), "no encrypted identity was configured, so no passphrase hand-off payload should ever be produced");
    }

    /// `dispatch` must resolve the passphrase hand-off set (Phase 1b) and
    /// pass its encoded bytes to the spawner *before* spawning a holder for a
    /// destination with a passphrase-protected identity — this is the wiring
    /// between `dispatch`, `handoff::resolve_handoff_credentials`, and
    /// `holder::HolderSpawner::spawn` end to end (the crypto/decrypt
    /// correctness itself is `handoff`'s own module tests' job).
    #[tokio::test]
    async fn dispatch_hands_off_the_decrypted_key_to_the_spawner_when_an_identity_is_encrypted() {
        use rand::rngs::OsRng;
        use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey as SshPrivateKey};

        let runtime_dir = tempfile::tempdir().unwrap();
        let key_dir = tempfile::tempdir().unwrap();
        let key_path = key_dir.path().join("id_ed25519");
        let key = SshPrivateKey::from(Ed25519Keypair::random(&mut OsRng));
        let encrypted = key.encrypt(&mut OsRng, "hunter2").unwrap();
        std::fs::write(&key_path, encrypted.to_openssh(Default::default()).unwrap().as_bytes()).unwrap();

        let mut prepared = test_prepared("dispatch-handoff-host", runtime_dir.path());
        prepared.host_config_mut().identity_file = vec![key_path.clone()];

        let spawner = holder::tests_support::RecordingSpawner::failing();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            dispatch::<InMemoryChannel, _>(prepared, vec!["dispatch-handoff-host".to_string()], &spawner, &|_path, _attempt| Some("hunter2".to_string())),
        )
        .await
        .expect("dispatch must not hang resolving the hand-off set");
        assert!(result.is_err(), "the fallback direct connect against the bogus pipe path must still fail here");

        let calls = spawner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let payload = calls[0].1.as_deref().expect("an encrypted identity must produce a non-empty hand-off payload");
        let decoded = handoff::decode(payload).expect("the encoded hand-off payload must decode cleanly");
        let credential = decoded.get(&key_path).expect("the encrypted candidate's path must be a key in the decoded hand-off set");
        let cleartext = russh_keys::PrivateKey::from_openssh(&credential.private_key_pem).expect("the hand-off PEM must be valid OpenSSH text");
        assert!(!cleartext.is_encrypted(), "the hand-off PEM must be the decrypted cleartext key, not the original ciphertext");
    }

    /// A holder that was spawned successfully but never actually claims the
    /// channel (crashed before `try_claim`, or was simply slow) must not wedge
    /// `dispatch` forever — it gives up once its own startup-wait deadline
    /// elapses and falls back to a direct connect, same as a spawn failure.
    #[tokio::test]
    async fn dispatch_falls_back_to_a_direct_connect_when_the_spawned_holder_never_claims_the_channel() {
        tokio::time::pause();
        let runtime_dir = tempfile::tempdir().unwrap();
        let prepared = test_prepared("dispatch-holder-never-arrives-host", runtime_dir.path());
        let spawner = holder::tests_support::RecordingSpawner::succeeding();

        let result = tokio::time::timeout(
            HOLDER_STARTUP_TIMEOUT + Duration::from_secs(5),
            dispatch::<InMemoryChannel, _>(prepared, vec!["dispatch-holder-never-arrives-host".to_string()], &spawner, &|_path, _attempt| None),
        )
        .await
        .expect("dispatch must give up waiting for the holder within its own startup timeout, not hang");

        assert!(result.is_err(), "a holder that spawned but never claims the channel must fall back to a direct connect (which itself fails against the bogus pipe path here, but via the fallback path, not a hang)");
    }

    /// Simulates a second tab arriving while a *different* tab is already in
    /// the middle of resolving the hand-off set and spawning a holder for the
    /// same destination (the `SpawnLock` this tab would otherwise contend
    /// for is pre-acquired here to stand in for that other tab). This tab
    /// must not redundantly resolve its own hand-off (which would re-prompt
    /// the user for the same passphrase) or spawn its own holder — it must
    /// simply wait for the leader's holder instead.
    #[tokio::test]
    async fn dispatch_does_not_spawn_a_redundant_holder_while_another_tab_holds_the_spawn_lock() {
        tokio::time::pause();
        let runtime_dir = tempfile::tempdir().unwrap();
        let prepared = test_prepared("dispatch-spawn-lock-contention-host", runtime_dir.path());
        let channel_name = naming::channel_name(prepared.host_config(), prepared.resolution(), prepared.plan().destination_host());
        let lock_path = runtime_dir.path().join(naming::spawn_lock_file_name(&channel_name));
        let held_by_another_tab = SpawnLock::try_acquire(lock_path, HOLDER_STARTUP_TIMEOUT);
        assert!(held_by_another_tab.acquired, "test setup: the simulated other tab must win the lock");

        let spawner = holder::tests_support::RecordingSpawner::failing();
        let result = tokio::time::timeout(
            HOLDER_STARTUP_TIMEOUT + Duration::from_secs(5),
            dispatch::<InMemoryChannel, _>(prepared, vec!["dispatch-spawn-lock-contention-host".to_string()], &spawner, &|_path, _attempt| {
                panic!("a follower must never resolve its own passphrase hand-off while another tab's spawn lock is held")
            }),
        )
        .await
        .expect("dispatch must not hang waiting on another tab's spawn lock");

        assert!(result.is_err(), "the eventual fallback direct connect must still fail here (bogus pipe path), but via the follower path");
        assert!(spawner.calls.lock().unwrap().is_empty(), "a follower must never spawn its own holder while another tab's spawn lock is held");
    }

    // `ReconnectBackoff`/`RECONNECT_BUDGET`/`wait_or_abort`'s decision logic
    // is exercised directly here — same rationale `isekai-transport::backoff`
    // and `isekai-pipe::resume_loop` use for their own backoff math: pure,
    // no real console or real dispatch needed. `run_with_reconnect` itself
    // stays untested directly, matching the pre-existing boundary
    // `client::run` had (it needs a real console handle to enable raw mode
    // and read `ConsoleStdin`, same as `client::run` always did — only
    // `client::run_inner` was ever unit-tested against injected streams).

    #[test]
    fn reconnect_backoff_doubles_each_attempt_until_capped() {
        let backoff = ReconnectBackoff { initial: Duration::from_millis(100), max: Duration::from_secs(5), jitter: 0.0 };
        assert_eq!(backoff.base_delay(0), Duration::from_millis(100));
        assert_eq!(backoff.base_delay(1), Duration::from_millis(200));
        assert_eq!(backoff.base_delay(2), Duration::from_millis(400));
        assert_eq!(backoff.base_delay(3), Duration::from_millis(800));
    }

    #[test]
    fn reconnect_backoff_converges_to_and_never_exceeds_max() {
        let backoff = ReconnectBackoff { initial: Duration::from_millis(100), max: Duration::from_secs(5), jitter: 0.0 };
        for attempt in 0..64 {
            assert!(backoff.base_delay(attempt) <= backoff.max, "attempt {attempt} exceeded max");
        }
        assert_eq!(backoff.base_delay(63), backoff.max, "backoff must saturate at max for a large attempt count, not overflow/panic");
    }

    #[test]
    fn reconnect_backoff_jitter_stays_within_the_configured_fraction_and_never_exceeds_max() {
        let backoff = ReconnectBackoff { initial: Duration::from_millis(200), max: Duration::from_secs(2), jitter: 0.5 };
        let base = backoff.base_delay(3);
        for _ in 0..200 {
            let jittered = backoff.delay_for_attempt(3);
            assert!(jittered <= backoff.max, "jittered delay must never exceed max");
            let lower = base.mul_f64(0.5);
            let upper = base.min(backoff.max).mul_f64(1.5).min(backoff.max);
            assert!(
                jittered + Duration::from_millis(1) >= lower && jittered <= upper + Duration::from_millis(1),
                "jittered delay {jittered:?} outside [{lower:?}, {upper:?}]"
            );
        }
    }

    #[test]
    fn reconnect_backoff_zero_jitter_returns_exactly_the_base_delay() {
        let backoff = ReconnectBackoff { initial: Duration::from_millis(50), max: Duration::from_secs(1), jitter: 0.0 };
        for attempt in 0..5 {
            assert_eq!(backoff.delay_for_attempt(attempt), backoff.base_delay(attempt));
        }
    }

    #[test]
    fn reconnect_stable_threshold_is_comfortably_above_the_max_backoff() {
        // A run of purely back-to-back failed attempts (each shorter than
        // `RECONNECT_STABLE_THRESHOLD`) must never spuriously look "stable"
        // and reset the `RECONNECT_BUDGET` clock — see
        // `RECONNECT_STABLE_THRESHOLD`'s own docs. If someone raises
        // `RECONNECT_BACKOFF.max` above (or close to) the threshold without
        // updating this too, that guarantee quietly breaks.
        assert!(
            RECONNECT_STABLE_THRESHOLD > RECONNECT_BACKOFF.max * 2,
            "RECONNECT_STABLE_THRESHOLD must stay well above RECONNECT_BACKOFF.max"
        );
    }

    #[test]
    fn reconnect_backoff_never_panics_on_a_huge_attempt_count() {
        // Mirrors `isekai_transport::backoff::BackoffPolicy`'s own overflow
        // guard test — `attempt.min(32)` before the `1u64 << shift` is what
        // makes this safe; a regression here would panic in debug builds.
        let backoff = ReconnectBackoff { initial: Duration::from_millis(1), max: Duration::from_secs(1), jitter: 0.0 };
        assert_eq!(backoff.delay_for_attempt(u32::MAX), backoff.max);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_or_abort_elapses_when_the_delay_runs_out_with_no_input() {
        let mut never_ready = tokio::io::empty();
        // `tokio::io::empty()` yields `Ok(0)` (EOF) immediately on every
        // read, same as a genuinely closed stdin — exercises the "read
        // resolved but produced nothing abort-worthy" arm, not the timer
        // arm specifically (see the next test for a stdin that never
        // resolves at all, which does exercise the timer arm).
        let before = tokio::time::Instant::now();
        let outcome = wait_or_abort_over(Duration::from_secs(1), &mut never_ready).await;
        assert_eq!(outcome, WaitOutcome::Elapsed);
        // Regression guard: an EOF/closed stdin (e.g. `isekai-ssh` run with
        // redirected/closed stdin, or `< NUL`) must still wait out the full
        // backoff delay, not resolve instantly just because the read never
        // blocks — a prior version of this function's `select!` let the
        // always-immediately-ready EOF read win every race, skipping the
        // delay entirely and hot-looping reconnect attempts with zero wait.
        assert!(before.elapsed() >= Duration::from_secs(1), "an EOF stdin must not skip the backoff delay");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_or_abort_elapses_via_the_timer_when_stdin_never_resolves() {
        // `tokio::io::duplex`'s read half never yields anything as long as
        // nothing is ever written to its write half (kept alive here by
        // binding it to `_write_half`, not dropping it — a dropped write
        // half would make the read half see EOF instead, defeating the
        // point) — this is what actually exercises `wait_or_abort_over`'s
        // `tokio::time::sleep` arm winning the `select!`, distinct from the
        // test above.
        let (mut read_half, _write_half) = tokio::io::duplex(64);
        let outcome = wait_or_abort_over(Duration::from_secs(1), &mut read_half).await;
        assert_eq!(outcome, WaitOutcome::Elapsed);
    }

    #[tokio::test]
    async fn wait_or_abort_aborts_on_ctrl_c() {
        let mut stdin = std::io::Cursor::new(vec![b'x', 0x03, b'y']);
        let outcome = wait_or_abort_over(Duration::from_secs(30), &mut stdin).await;
        assert_eq!(outcome, WaitOutcome::Aborted);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_or_abort_ignores_ordinary_keystrokes_typed_while_waiting() {
        // A stray keypress during the reconnect wait has nowhere meaningful
        // to go (no live remote session yet) and must not be mistaken for
        // Ctrl+C — the read resolving with ordinary bytes is discarded and
        // the wait keeps going (not treated as `Elapsed`/`Aborted` on the
        // spot) until the cursor hits EOF and the remaining delay is waited
        // out for real — `start_paused` is what keeps that 30s delay from
        // making this test actually take 30 real seconds.
        let mut stdin = std::io::Cursor::new(vec![b'h', b'i']);
        let outcome = wait_or_abort_over(Duration::from_secs(30), &mut stdin).await;
        assert_eq!(outcome, WaitOutcome::Elapsed);
    }
}
