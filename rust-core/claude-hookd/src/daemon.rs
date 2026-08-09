//! `claude-hookd __serve`'s async daemon loop and its `Command`-line
//! parsing. `__serve` is intentionally undocumented in `--help` — it is
//! spawned only by [`super::spawn_detached_daemon`], never meant to be
//! typed by a human.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::UnixListener;

use super::delivery::{self, Delivery};
use super::hooks;
use super::state::{apply_event, apply_timeout, Action, HookEvent, TabState};

/// How long an `Attention` session stays that way without a debounce
/// refresh or a `Resolve` before it reverts to `Idle` on its own. Not yet
/// configurable — the pure state machine (`state.rs`) already takes this as
/// a parameter, so wiring up an override later needs no change here beyond
/// reading the value from somewhere.
const ATTENTION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How long a `Deferred` session (an ambiguous `Stop` — see `state.rs`'s
/// module docs) is given the benefit of the doubt before [`apply_timeout`]
/// promotes it to `Attention`. Deliberately longer than `ATTENTION_TIMEOUT`:
/// this bound is meant to almost never actually fire (a normal background
/// wait — a CI run, a long build — should resolve on its own well within
/// it); if it routinely does, all that's been gained is a delayed false
/// positive. Must stay comfortably under `IDLE_EXIT`, since a `Deferred`-
/// only tab receives no further *events* to reset the idle-exit clock — with
/// the current defaults (30 + 10 = 40 min < 60 min) a promotion is always
/// reachable before self-exit.
const MAX_DEFERRAL: Duration = Duration::from_secs(30 * 60);

/// How long a `WireEvent::TeammateDispatched` (a successful, top-level
/// `Agent`/`SendMessage` `PostToolUse`) stays "recent enough" for a later
/// `WireEvent::StopAmbiguousTeammate` on the same session to treat a
/// `type: "teammate"` `background_tasks` entry as plausibly still working
/// (`HookEvent::StopDeferred`) rather than a long-idle teammate
/// (`HookEvent::Notify`, the safe default). Deliberately independent of
/// ever receiving a `TeammateIdle` hook event — this crate has no such hook
/// wired up yet (2026-08-05 design note: `TeammateIdle` fires reliably but
/// only tells us a teammate *finished*, not disambiguated by *which*
/// teammate on a session with several, so treating a bare TTL as the
/// correctness mechanism and any future `TeammateIdle` wiring as purely an
/// early-clear optimization keeps this simple and still fail-safe: a
/// missing/dropped signal just falls back to the TTL, never to a
/// permanently-stuck `StopDeferred`). Same magnitude as `MAX_DEFERRAL` for
/// the same reason: a normal delegated task should resolve well within it,
/// and if it routinely doesn't, all that's gained by a longer TTL is a
/// delayed false positive.
const TEAMMATE_DISPATCH_TTL: Duration = Duration::from_secs(30 * 60);

/// How long this daemon keeps running with no events at all before exiting
/// on its own — its socket file is left for the next `claude-hookd event`
/// invocation's sweep to reclaim.
const IDLE_EXIT: Duration = Duration::from_secs(60 * 60);

/// How long to sleep after a resource-exhaustion class `accept()` failure
/// (EMFILE/ENFILE/ENOMEM/...) before retrying, so a persistent failure
/// becomes a slow retry loop instead of a 100%-CPU busy loop.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// How many consecutive non-transient `accept()` failures this daemon
/// tolerates before giving up on its listener entirely.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: u32 = 32;

/// Hard cap on a single hook event line, applied via [`AsyncReadExt::take`]
/// in [`read_one_event`] so a peer that never sends a newline can't grow
/// that connection's buffer without bound.
const MAX_EVENT_LINE_LEN: u64 = 64 * 1024;

/// Hard cap on how long [`read_one_event`] waits for a complete line.
const EVENT_READ_TIMEOUT: Duration = Duration::from_secs(5);

struct DaemonConfig {
    sock_path: PathBuf,
    delivery: Delivery,
    idle_color: (u8, u8, u8),
    attention_color: (u8, u8, u8),
    waiting_color: (u8, u8, u8),
    /// Overridable only via the undocumented `--attention-timeout-ms`/
    /// `--max-deferral-ms`/`--idle-exit-ms`/`--teammate-dispatch-ttl-ms`
    /// flags, which exist solely so this module's own tests can drive a
    /// real daemon through every timeout/promotion path in milliseconds
    /// instead of the real 10-minute/30-minute/1-hour durations.
    attention_timeout: Duration,
    max_deferral: Duration,
    idle_exit: Duration,
    teammate_dispatch_ttl: Duration,
    /// `~/.config/claude-hookd/hooks` (`main.rs::hooks_dir`), resolved by the
    /// short-lived `claude-hookd event` process and passed down as
    /// `--hooks-dir` — never read from `$HOME` inside this long-running
    /// daemon, matching every other piece of this config. `None` (unset
    /// `$HOME`, or the flag simply not passed by an older/test caller) makes
    /// `hooks::run_hook` a silent no-op, same as a hooks dir that exists but
    /// has no matching `on-*` file in it.
    hooks_dir: Option<PathBuf>,
}

/// Parses `__serve`'s spawn arguments and runs the daemon loop until it
/// self-exits. Always returns `ExitCode::SUCCESS` — there is no interactive
/// caller to report a non-zero exit code to, and a malformed spawn (which
/// should never happen, since the only caller is
/// [`super::spawn_detached_daemon`] in the same binary) has nothing sane to
/// do besides exit immediately.
pub(crate) async fn serve_command(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut sock_path = None;
    let mut delivery_spec = None;
    let mut idle_color = None;
    let mut attention_color = None;
    let mut waiting_color = None;
    let mut attention_timeout = ATTENTION_TIMEOUT;
    let mut max_deferral = MAX_DEFERRAL;
    let mut idle_exit = IDLE_EXIT;
    let mut teammate_dispatch_ttl = TEAMMATE_DISPATCH_TTL;
    let mut hooks_dir = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sock" => sock_path = args.next().map(PathBuf::from),
            "--delivery-spec" => delivery_spec = args.next(),
            "--hooks-dir" => hooks_dir = args.next().map(PathBuf::from),
            "--idle-color" => idle_color = args.next().and_then(|v| super::hexcolor::parse_hex_color(&v).ok()),
            "--attention-color" => attention_color = args.next().and_then(|v| super::hexcolor::parse_hex_color(&v).ok()),
            "--waiting-color" => waiting_color = args.next().and_then(|v| super::hexcolor::parse_hex_color(&v).ok()),
            "--attention-timeout-ms" => {
                if let Some(ms) = args.next().and_then(|v| v.parse().ok()) {
                    attention_timeout = Duration::from_millis(ms);
                }
            }
            "--max-deferral-ms" => {
                if let Some(ms) = args.next().and_then(|v| v.parse().ok()) {
                    max_deferral = Duration::from_millis(ms);
                }
            }
            "--idle-exit-ms" => {
                if let Some(ms) = args.next().and_then(|v| v.parse().ok()) {
                    idle_exit = Duration::from_millis(ms);
                }
            }
            "--teammate-dispatch-ttl-ms" => {
                if let Some(ms) = args.next().and_then(|v| v.parse().ok()) {
                    teammate_dispatch_ttl = Duration::from_millis(ms);
                }
            }
            _ => {}
        }
    }
    let (Some(sock_path), Some(delivery)) = (sock_path, delivery_spec.as_deref().and_then(Delivery::from_spec)) else {
        return ExitCode::SUCCESS;
    };
    run(DaemonConfig {
        sock_path,
        delivery,
        idle_color: idle_color.unwrap_or(super::DEFAULT_IDLE_COLOR),
        attention_color: attention_color.unwrap_or(super::DEFAULT_ATTENTION_COLOR),
        waiting_color: waiting_color.unwrap_or(super::DEFAULT_WAITING_COLOR),
        attention_timeout,
        max_deferral,
        idle_exit,
        teammate_dispatch_ttl,
        hooks_dir,
    })
    .await;
    ExitCode::SUCCESS
}

async fn run(config: DaemonConfig) {
    // Re-sweep immediately before bind (not just the client's earlier
    // sweep before spawning): two clients can both decide "no daemon" and
    // both spawn near-simultaneously, so *this* process's own bind attempt
    // needs its own last-moment check that a same-named stale file left by
    // a crashed/self-exited predecessor doesn't cause a spurious
    // `AddrInUse`.
    super::sweep_stale_daemon_sockets();

    let listener = match UnixListener::bind(&config.sock_path) {
        Ok(listener) => listener,
        // Lost the spawn race to a sibling `__serve` that bound first — it
        // is now serving this tab, so this process has nothing left to do.
        Err(_) => return,
    };

    // Self-healing: paint idle unconditionally on startup, regardless of
    // why this daemon is starting — including a fresh tab (harmless no-op-
    // looking write to a terminal already showing its default color) and,
    // more importantly, a restart after a crash or the 1h self-exit that
    // left a *previous* daemon's tab stuck in the attention color with
    // nothing left to revert it. Deliberately calls `delivery` directly
    // rather than going through `execute_actions`/`hooks::run_hook`: this
    // write isn't a response to an `Action` from a real state transition
    // (`state` is freshly `TabState::new()` at this point), so an
    // `on-idle` hook never fires purely from daemon startup/restart — only
    // from a real `Attention`/`Waiting` → `Idle` transition later in `run`'s
    // loop (adversarial review, 2026-08, flagged this asymmetry as worth
    // documenting explicitly rather than fixing: firing `on-idle` here too
    // would mean every daemon restart — including the routine 1h
    // self-exit-then-respawn — looks identical to a real transition to a
    // hook script, which is more likely to surprise a hook author than help
    // one).
    delivery::send_tab_color(&config.delivery, config.idle_color, config.hooks_dir.as_deref()).await;

    // Accepting and reading each connection happens on its own spawned
    // task, separate from the main loop below: a connection that opens and
    // then never sends a complete line — a broken client, or even just
    // `nc -U <sock>` left open by a human poking at the socket — must not
    // block the whole `select!` (and therefore the attention timeout,
    // idle-exit, and every other pending connection) for as long as it
    // stays open. Each spawned task only does I/O and forwards the parsed
    // `(session_id, event)` to `event_rx`; all state mutation and delivery
    // sends stay serialized on the single task below, so events are still
    // applied one at a time in arrival order.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut accept_task = tokio::spawn(async move {
        let mut consecutive_accept_errors = 0u32;
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    consecutive_accept_errors = 0;
                    let event_tx = event_tx.clone();
                    tokio::spawn(async move {
                        if let Some(event) = read_one_event(stream).await {
                            let _ = event_tx.send(event);
                        }
                    });
                }
                Err(e) => match classify_accept_error(e.kind(), consecutive_accept_errors) {
                    AcceptRetry::Immediate => {}
                    AcceptRetry::Backoff => {
                        consecutive_accept_errors += 1;
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                    }
                    AcceptRetry::GiveUp => return,
                },
            }
        }
    });

    let mut state = TabState::new();
    // A separate piece of per-session state from `state: TabState` on
    // purpose (see `state.rs`'s module docs) — this map answers "did this
    // session recently dispatch work to a teammate?", not "is this session
    // pending attention?", and only ever exists to resolve
    // `WireEvent::StopAmbiguousTeammate` below. Values are the *expiry*
    // instant (`now + config.teammate_dispatch_ttl` at insertion), not the
    // dispatch instant — checking `now < expiry` on lookup then also does
    // this map's only pruning, so no separate sweep task is needed for it
    // (unlike `TabState`, which has real, popup-worthy actions to fire on
    // expiry — this map has none: a lookup miss just means "treat as no
    // recent dispatch," nothing more happens).
    let mut teammate_dispatch: HashMap<String, Instant> = HashMap::new();
    let mut idle_exit_deadline = Instant::now() + config.idle_exit;
    loop {
        let idle_exit_sleep = tokio::time::sleep(idle_exit_deadline.saturating_duration_since(Instant::now()));
        tokio::select! {
            _ = &mut accept_task => break,
            received = event_rx.recv() => {
                let Some((session_id, wire_event)) = received else { break };
                idle_exit_deadline = Instant::now() + config.idle_exit;
                let now = Instant::now();
                let event = match wire_event {
                    WireEvent::Notify => HookEvent::Notify,
                    WireEvent::StopDeferred => HookEvent::StopDeferred,
                    WireEvent::Resolve => HookEvent::Resolve,
                    WireEvent::TeammateDispatched => {
                        teammate_dispatch.insert(session_id.clone(), now + config.teammate_dispatch_ttl);
                        HookEvent::Resolve
                    }
                    WireEvent::StopAmbiguousTeammate => {
                        match teammate_dispatch.get(&session_id) {
                            Some(&expiry) if now < expiry => HookEvent::StopDeferred,
                            _ => HookEvent::Notify,
                        }
                    }
                };
                let (next_state, actions) = apply_event(&state, &session_id, event, now, config.attention_timeout, config.max_deferral);
                state = next_state;
                execute_actions(&config, &actions, &state, now).await;
            }
            () = attention_sleep(&state) => {
                let now = Instant::now();
                let (next_state, actions) = apply_timeout(&state, now, config.attention_timeout);
                state = next_state;
                execute_actions(&config, &actions, &state, now).await;
            }
            () = idle_exit_sleep => {
                break;
            }
        }
    }
    accept_task.abort();
    // Deliberately does *not* `remove_file(&config.sock_path)` here: this
    // daemon's own bind race handling above already relies on stale
    // sockets being reclaimed by the next invocation's sweep — deleting it
    // here would race a successor daemon that binds the same freshly-swept
    // path between this loop's `break` and the `remove_file` call, unlinking
    // the *successor's* live socket. Leaving the file for the sweep to find
    // is strictly safer than trying to delete it eagerly.
}

/// What the `accept()` loop in [`run`] should do after a failed accept,
/// decided purely from the error kind and how many *other* non-transient
/// failures already happened in a row — kept separate from the loop itself
/// so it's unit-testable without a real socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptRetry {
    Immediate,
    Backoff,
    GiveUp,
}

fn classify_accept_error(kind: io::ErrorKind, consecutive_backoff_failures: u32) -> AcceptRetry {
    match kind {
        io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::Interrupted => AcceptRetry::Immediate,
        _ if consecutive_backoff_failures + 1 >= MAX_CONSECUTIVE_ACCEPT_ERRORS => AcceptRetry::GiveUp,
        _ => AcceptRetry::Backoff,
    }
}

/// Resolves at `state`'s earliest pending deadline — `Attention` eviction or
/// `Deferred` promotion, whichever is sooner — or never resolves at all if
/// the tab is fully idle.
async fn attention_sleep(state: &TabState) {
    match state.next_deadline() {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending::<()>().await,
    }
}

/// The daemon's own wire-level event vocabulary — a strict superset of
/// `state::HookEvent`, mirroring `main.rs`'s `ClientEvent` (the two are kept
/// as distinct types rather than shared, same as the rest of this wire
/// format's deliberate decoupling from Claude Code's own hook JSON schema).
/// `TeammateDispatched`/`StopAmbiguousTeammate` are not themselves valid
/// inputs to `state::apply_event` — [`run`]'s main loop translates them into
/// a real `HookEvent` first, using its own `teammate_dispatch` TTL map,
/// before ever touching `TabState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireEvent {
    Notify,
    StopDeferred,
    Resolve,
    TeammateDispatched,
    StopAmbiguousTeammate,
}

/// Reads exactly one
/// `{"session_id": "...", "event": "notify"|"stop_deferred"|"resolve"|
/// "teammate_dispatched"|"stop_ambiguous_teammate"}` line — the daemon's own
/// minimal wire format, deliberately decoupled from Claude Code's own hook
/// JSON schema (only `super::parse_hook_event`, the client side, needs to
/// know that shape).
async fn read_one_event(stream: tokio::net::UnixStream) -> Option<(String, WireEvent)> {
    tokio::time::timeout(EVENT_READ_TIMEOUT, read_one_event_line(stream)).await.ok()?
}

async fn read_one_event_line(stream: tokio::net::UnixStream) -> Option<(String, WireEvent)> {
    // Symmetric with `main.rs::send_event`'s own peer-credential check on
    // the client side (adversarial review, 2026-08: this accept-side check
    // was missing entirely) — under the `/tmp` fallback (no
    // `$XDG_RUNTIME_DIR`, see `daemon_sock_dir`'s docs), the socket path is
    // a deterministic hash any local user can also connect *to* once it
    // exists, not just pre-bind. Without this, any such connection would be
    // read as a legitimate hook event (forged `session_id`/`event`,
    // attacker-controlled tab color/attention-popup state) purely because
    // it reached the socket at all. Checked before any data is read, so a
    // mismatched peer can't influence parsing even indirectly. Uses
    // `super::peer_is_same_uid` (not a bare `stream.peer_cred()`) — see its
    // docs for why a bare call flaked this exact check out on macOS CI.
    if !super::peer_is_same_uid(&stream).await {
        return None;
    }
    let mut reader = BufReader::new(stream.take(MAX_EVENT_LINE_LEN));
    let mut line = String::new();
    reader.read_line(&mut line).await.ok()?;
    let value: serde_json::Value = serde_json::from_str(line.trim_end()).ok()?;
    let session_id = value.get("session_id")?.as_str()?.to_string();
    let event = match value.get("event")?.as_str()? {
        "notify" => WireEvent::Notify,
        "stop_deferred" => WireEvent::StopDeferred,
        "resolve" => WireEvent::Resolve,
        "teammate_dispatched" => WireEvent::TeammateDispatched,
        "stop_ambiguous_teammate" => WireEvent::StopAmbiguousTeammate,
        _ => return None,
    };
    Some((session_id, event))
}

async fn execute_actions(config: &DaemonConfig, actions: &[Action], state: &TabState, now: Instant) {
    for action in actions {
        match action {
            Action::SetAttentionColorAndPopup => {
                delivery::send_tab_color(&config.delivery, config.attention_color, config.hooks_dir.as_deref()).await;
                delivery::send_notify_popup(&config.delivery).await;
                hooks::run_hook(config.hooks_dir.as_deref(), "attention", state, now).await;
            }
            Action::SetWaitingColor => {
                delivery::send_tab_color(&config.delivery, config.waiting_color, config.hooks_dir.as_deref()).await;
                hooks::run_hook(config.hooks_dir.as_deref(), "waiting", state, now).await;
            }
            Action::SetIdleColor => {
                delivery::send_tab_color(&config.delivery, config.idle_color, config.hooks_dir.as_deref()).await;
                hooks::run_hook(config.hooks_dir.as_deref(), "idle", state, now).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::UnixStream;

    /// Drives a real daemon (`run`, not the pure `state.rs` function) through
    /// startup self-heal, a Notify→Attention transition (color + popup),
    /// an attention timeout back to Idle, and self-exit — using millisecond
    /// timeout config values so the whole test runs in well under a second
    /// instead of the real 10-minute/1-hour durations. Uses `Delivery::DirectTty`
    /// against a plain tempfile standing in for a pty device (a real pty
    /// isn't needed to verify the daemon writes the right bytes at the
    /// right times).
    #[tokio::test]
    async fn daemon_loop_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let tty_path = dir.path().join("fake-tty");
        std::fs::write(&tty_path, b"").unwrap();
        let hookd_sock_path = dir.path().join("hookd.sock");

        let idle_color = (0x20, 0x20, 0x20);
        let attention_color = (0xff, 0x88, 0x00);
        let waiting_color = (0x00, 0x60, 0xa0);
        let daemon = tokio::spawn(run(DaemonConfig {
            sock_path: hookd_sock_path.clone(),
            delivery: Delivery::DirectTty { path: tty_path.clone() },
            idle_color,
            attention_color,
            waiting_color,
            attention_timeout: Duration::from_millis(100),
            max_deferral: Duration::from_millis(150),
            idle_exit: Duration::from_millis(300),
            teammate_dispatch_ttl: Duration::from_millis(150),
            // The realistic default for most users, not `None`: `main.rs`'s
            // `hooks_dir()` resolves to `Some(...)` unconditionally whenever
            // `$HOME` is set, whether or not the directory (or any `on-*`
            // file in it) actually exists (adversarial review, 2026-08: this
            // flagship regression test — "behavior is unchanged with no
            // hooks configured" — previously only exercised the `None`
            // case, which in practice almost never happens).
            hooks_dir: Some(dir.path().join("hooks")),
        }));

        // Poll (not a fixed sleep — this project has repeatedly hit CI
        // flakiness from timing-based async tests under load) until startup
        // self-heal has painted idle. This also transitively waits for the
        // listener to be bound, since `run` binds before self-healing.
        poll_tty_contains(&tty_path, "4;264;rgb:20/20/20", "startup self-heal must paint idle").await;

        let mut client = UnixStream::connect(&hookd_sock_path).await.unwrap();
        client.write_all(b"{\"session_id\":\"s1\",\"event\":\"notify\"}\n").await.unwrap();
        drop(client);

        poll_tty_contains(&tty_path, "4;264;rgb:ff/88/00", "notify must paint attention color").await;
        poll_tty_contains(&tty_path, "]9;", "notify must also emit the OSC 9 popup").await;

        // No `resolve` ever arrives — the 100ms attention timeout fires and
        // reverts the color on its own. Poll for the *last* write ending in
        // the idle sequence (not just "contains", which the startup
        // self-heal above already satisfies) so this doesn't pass
        // spuriously before the timeout actually fires.
        poll_until(Duration::from_secs(2), || {
            std::fs::read_to_string(&tty_path).unwrap().trim_end().ends_with("\x1b]4;264;rgb:20/20/20\x1b\\")
        })
        .await
        .unwrap_or_else(|_| panic!("attention timeout must revert to idle: {:?}", std::fs::read_to_string(&tty_path).unwrap()));

        // No further events at all — the 300ms idle-exit fires and the
        // daemon task completes on its own.
        tokio::time::timeout(Duration::from_secs(2), daemon).await.expect("daemon did not self-exit in time").unwrap();
        assert!(hookd_sock_path.exists(), "graceful self-exit must not unlink its own socket file");
    }

    /// End to end through the real daemon loop (not `hooks.rs`'s own unit
    /// tests, which call `hooks::run_hook` directly): a `notify` wire event
    /// must both paint the attention color (already covered above) *and*
    /// fire the configured `on-attention` command hook with the right
    /// `argv[1]`/stdin, without the hook's own runtime delaying the OSC
    /// write it races against.
    #[tokio::test]
    async fn notify_fires_the_configured_command_hook_alongside_the_osc_write() {
        let dir = tempfile::tempdir().unwrap();
        let tty_path = dir.path().join("fake-tty");
        std::fs::write(&tty_path, b"").unwrap();
        let hookd_sock_path = dir.path().join("hookd.sock");
        let hooks_dir = dir.path().join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let out_path = dir.path().join("hook-out");
        {
            use std::os::unix::fs::PermissionsExt as _;
            let script_path = hooks_dir.join("on-attention");
            std::fs::write(&script_path, format!("#!/bin/sh\necho \"$1\" > {out_path:?}.arg\ncat > {out_path:?}.stdin\n")).unwrap();
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }

        let idle_color = (0x20, 0x20, 0x20);
        let attention_color = (0xff, 0x88, 0x00);
        let waiting_color = (0x00, 0x60, 0xa0);
        let daemon = tokio::spawn(run(DaemonConfig {
            sock_path: hookd_sock_path.clone(),
            delivery: Delivery::DirectTty { path: tty_path.clone() },
            idle_color,
            attention_color,
            waiting_color,
            attention_timeout: Duration::from_secs(5),
            max_deferral: Duration::from_secs(5),
            idle_exit: Duration::from_secs(5),
            teammate_dispatch_ttl: Duration::from_secs(5),
            hooks_dir: Some(hooks_dir),
        }));

        poll_tty_contains(&tty_path, "4;264;rgb:20/20/20", "startup self-heal must paint idle").await;

        let mut client = UnixStream::connect(&hookd_sock_path).await.unwrap();
        client.write_all(b"{\"session_id\":\"s1\",\"event\":\"notify\"}\n").await.unwrap();
        drop(client);

        // Both effects of the same `Action::SetAttentionColorAndPopup` must
        // land — the OSC write is not blocked on the hook, nor vice versa.
        poll_tty_contains(&tty_path, "4;264;rgb:ff/88/00", "notify must still paint attention color").await;
        // Polls on parseable JSON, not mere existence — see `hooks.rs`'s
        // matching test for why an existence-only check flaked under load.
        poll_until(Duration::from_secs(2), || {
            out_path.with_extension("arg").exists()
                && std::fs::read_to_string(out_path.with_extension("stdin"))
                    .ok()
                    .is_some_and(|s| serde_json::from_str::<serde_json::Value>(&s).is_ok())
        })
        .await
        .expect("the on-attention hook must run");

        assert_eq!(std::fs::read_to_string(out_path.with_extension("arg")).unwrap().trim(), "attention");
        let stdin_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out_path.with_extension("stdin")).unwrap()).unwrap();
        assert_eq!(stdin_json["aggregate"], "attention");
        assert_eq!(stdin_json["sessions"]["s1"]["kind"], "attention");

        daemon.abort();
    }

    /// Drives the new `stop_deferred` wire event through a real daemon: a
    /// `Deferred` session paints the waiting color with **no** popup, and
    /// once `max_deferral` elapses with no further event, the daemon
    /// self-corrects by promoting it to `Attention` — the actual color +
    /// popup a wrongly-suppressed real notification eventually still gets.
    #[tokio::test]
    async fn stop_deferred_paints_waiting_then_self_corrects_to_attention() {
        let dir = tempfile::tempdir().unwrap();
        let tty_path = dir.path().join("fake-tty");
        std::fs::write(&tty_path, b"").unwrap();
        let hookd_sock_path = dir.path().join("hookd.sock");

        let idle_color = (0x20, 0x20, 0x20);
        let attention_color = (0xff, 0x88, 0x00);
        let waiting_color = (0x00, 0x60, 0xa0);
        let daemon = tokio::spawn(run(DaemonConfig {
            sock_path: hookd_sock_path.clone(),
            delivery: Delivery::DirectTty { path: tty_path.clone() },
            idle_color,
            attention_color,
            waiting_color,
            attention_timeout: Duration::from_millis(100),
            max_deferral: Duration::from_millis(80),
            idle_exit: Duration::from_secs(5),
            teammate_dispatch_ttl: Duration::from_millis(80),
            hooks_dir: None,
        }));

        poll_tty_contains(&tty_path, "4;264;rgb:20/20/20", "startup self-heal must paint idle").await;

        let mut client = UnixStream::connect(&hookd_sock_path).await.unwrap();
        client.write_all(b"{\"session_id\":\"s1\",\"event\":\"stop_deferred\"}\n").await.unwrap();
        drop(client);

        poll_tty_contains(&tty_path, "4;264;rgb:00/60/a0", "stop_deferred must paint the waiting color").await;
        assert!(
            !std::fs::read_to_string(&tty_path).unwrap().contains("]9;"),
            "stop_deferred must never emit the attention popup"
        );

        // No further events — max_deferral (80ms) elapses and the daemon
        // promotes the still-Deferred session to Attention on its own.
        poll_tty_contains(&tty_path, "4;264;rgb:ff/88/00", "an unresolved deferral must self-correct to the attention color").await;
        poll_tty_contains(&tty_path, "]9;", "the self-correction must fire the popup, same as a real Notify would").await;

        daemon.abort();
    }

    /// The 2026-08-05 fix, end to end: a `teammate_dispatched` wire event
    /// (this session's own `PostToolUse(Agent|SendMessage)`) followed by a
    /// `stop_ambiguous_teammate` wire event (a `Stop` whose only
    /// `background_tasks` entry is `type: "teammate"`) must defer — paint
    /// the waiting color, no popup — exactly like a hard `stop_deferred`
    /// would, as long as the dispatch is still within `teammate_dispatch_ttl`.
    #[tokio::test]
    async fn teammate_dispatch_then_ambiguous_stop_defers_within_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let tty_path = dir.path().join("fake-tty");
        std::fs::write(&tty_path, b"").unwrap();
        let hookd_sock_path = dir.path().join("hookd.sock");

        let idle_color = (0x20, 0x20, 0x20);
        let attention_color = (0xff, 0x88, 0x00);
        let waiting_color = (0x00, 0x60, 0xa0);
        let daemon = tokio::spawn(run(DaemonConfig {
            sock_path: hookd_sock_path.clone(),
            delivery: Delivery::DirectTty { path: tty_path.clone() },
            idle_color,
            attention_color,
            waiting_color,
            attention_timeout: Duration::from_millis(200),
            max_deferral: Duration::from_millis(200),
            idle_exit: Duration::from_secs(5),
            teammate_dispatch_ttl: Duration::from_secs(5),
            hooks_dir: None,
        }));

        poll_tty_contains(&tty_path, "4;264;rgb:20/20/20", "startup self-heal must paint idle").await;

        let mut client = UnixStream::connect(&hookd_sock_path).await.unwrap();
        client.write_all(b"{\"session_id\":\"s1\",\"event\":\"teammate_dispatched\"}\n").await.unwrap();
        drop(client);

        let mut client = UnixStream::connect(&hookd_sock_path).await.unwrap();
        client.write_all(b"{\"session_id\":\"s1\",\"event\":\"stop_ambiguous_teammate\"}\n").await.unwrap();
        drop(client);

        poll_tty_contains(&tty_path, "4;264;rgb:00/60/a0", "a recent teammate dispatch must make an ambiguous Stop defer").await;
        assert!(
            !std::fs::read_to_string(&tty_path).unwrap().contains("]9;"),
            "a deferred ambiguous Stop must never emit the attention popup"
        );

        daemon.abort();
    }

    /// The other half: with **no** preceding `teammate_dispatched`,
    /// `stop_ambiguous_teammate` must fall back to the safe default —
    /// exactly like the old, reverted behavior of treating a `"teammate"`
    /// `background_tasks` entry as `Notify` — since a long-idle teammate's
    /// `status` looks identical to a working one.
    #[tokio::test]
    async fn ambiguous_stop_without_a_recent_dispatch_notifies() {
        let dir = tempfile::tempdir().unwrap();
        let tty_path = dir.path().join("fake-tty");
        std::fs::write(&tty_path, b"").unwrap();
        let hookd_sock_path = dir.path().join("hookd.sock");

        let idle_color = (0x20, 0x20, 0x20);
        let attention_color = (0xff, 0x88, 0x00);
        let waiting_color = (0x00, 0x60, 0xa0);
        let daemon = tokio::spawn(run(DaemonConfig {
            sock_path: hookd_sock_path.clone(),
            delivery: Delivery::DirectTty { path: tty_path.clone() },
            idle_color,
            attention_color,
            waiting_color,
            attention_timeout: Duration::from_millis(200),
            max_deferral: Duration::from_millis(200),
            idle_exit: Duration::from_secs(5),
            teammate_dispatch_ttl: Duration::from_secs(5),
            hooks_dir: None,
        }));

        poll_tty_contains(&tty_path, "4;264;rgb:20/20/20", "startup self-heal must paint idle").await;

        let mut client = UnixStream::connect(&hookd_sock_path).await.unwrap();
        client.write_all(b"{\"session_id\":\"s1\",\"event\":\"stop_ambiguous_teammate\"}\n").await.unwrap();
        drop(client);

        poll_tty_contains(&tty_path, "4;264;rgb:ff/88/00", "no recent dispatch must fall back to the attention color").await;
        poll_tty_contains(&tty_path, "]9;", "the safe fallback must fire the popup, same as a real Notify would").await;

        daemon.abort();
    }

    /// The TTL half: a `teammate_dispatched` older than
    /// `teammate_dispatch_ttl` must no longer count — pins the fix's
    /// fail-safe property (a stale dispatch record must not defer forever)
    /// independent of ever wiring up a `TeammateIdle` hook.
    #[tokio::test]
    async fn a_stale_teammate_dispatch_past_its_ttl_no_longer_defers() {
        let dir = tempfile::tempdir().unwrap();
        let tty_path = dir.path().join("fake-tty");
        std::fs::write(&tty_path, b"").unwrap();
        let hookd_sock_path = dir.path().join("hookd.sock");

        let idle_color = (0x20, 0x20, 0x20);
        let attention_color = (0xff, 0x88, 0x00);
        let waiting_color = (0x00, 0x60, 0xa0);
        let daemon = tokio::spawn(run(DaemonConfig {
            sock_path: hookd_sock_path.clone(),
            delivery: Delivery::DirectTty { path: tty_path.clone() },
            idle_color,
            attention_color,
            waiting_color,
            attention_timeout: Duration::from_millis(200),
            max_deferral: Duration::from_millis(200),
            idle_exit: Duration::from_secs(5),
            teammate_dispatch_ttl: Duration::from_millis(50),
            hooks_dir: None,
        }));

        poll_tty_contains(&tty_path, "4;264;rgb:20/20/20", "startup self-heal must paint idle").await;

        let mut client = UnixStream::connect(&hookd_sock_path).await.unwrap();
        client.write_all(b"{\"session_id\":\"s1\",\"event\":\"teammate_dispatched\"}\n").await.unwrap();
        drop(client);

        tokio::time::sleep(Duration::from_millis(150)).await;

        let mut client = UnixStream::connect(&hookd_sock_path).await.unwrap();
        client.write_all(b"{\"session_id\":\"s1\",\"event\":\"stop_ambiguous_teammate\"}\n").await.unwrap();
        drop(client);

        poll_tty_contains(&tty_path, "4;264;rgb:ff/88/00", "a dispatch past its TTL must not defer an ambiguous Stop").await;
        poll_tty_contains(&tty_path, "]9;", "an expired dispatch must fall back to the real Notify popup").await;

        daemon.abort();
    }

    /// Polls (generous timeout, short interval) until `condition` returns
    /// `true`, or returns `Err(())` once `timeout` elapses — the same
    /// "generous polling instead of a fixed sleep" shape this project
    /// already uses elsewhere for async tests under CI load.
    async fn poll_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> Result<(), ()> {
        let deadline = Instant::now() + timeout;
        loop {
            if condition() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn poll_tty_contains(path: &std::path::Path, needle: &str, message: &str) {
        let path = path.to_path_buf();
        let needle = needle.to_string();
        poll_until(Duration::from_secs(2), || std::fs::read_to_string(&path).map(|s| s.contains(&needle)).unwrap_or(false))
            .await
            .unwrap_or_else(|_| panic!("{message}: {:?}", std::fs::read_to_string(&path).unwrap()));
    }

    #[tokio::test]
    async fn daemon_loses_bind_race_and_exits_immediately_without_disturbing_the_winner() {
        let dir = tempfile::tempdir().unwrap();
        let hookd_sock_path = dir.path().join("hookd.sock");
        let _winner = UnixListener::bind(&hookd_sock_path).unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            run(DaemonConfig {
                sock_path: hookd_sock_path.clone(),
                delivery: Delivery::DirectTty { path: dir.path().join("fake-tty") },
                idle_color: (0, 0, 0),
                attention_color: (0, 0, 0),
                waiting_color: (0, 0, 0),
                attention_timeout: Duration::from_millis(50),
                max_deferral: Duration::from_millis(50),
                idle_exit: Duration::from_millis(50),
                teammate_dispatch_ttl: Duration::from_millis(50),
                hooks_dir: None,
            }),
        )
        .await;
        assert!(result.is_ok(), "the losing daemon must return promptly, not hang");
        assert!(hookd_sock_path.exists(), "the losing daemon must not remove the winner's live socket");
    }

    /// Pins the fix for the accept-error busy-loop bug: connection-level
    /// failures must not count toward giving up, resource-exhaustion class
    /// failures must back off instead of spinning, and enough of the latter
    /// in a row must eventually give up rather than retry forever.
    #[test]
    fn classify_accept_error_covers_all_branches() {
        for kind in [
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::Interrupted,
        ] {
            assert_eq!(classify_accept_error(kind, 0), AcceptRetry::Immediate);
            assert_eq!(
                classify_accept_error(kind, MAX_CONSECUTIVE_ACCEPT_ERRORS - 1),
                AcceptRetry::Immediate
            );
        }

        assert_eq!(classify_accept_error(io::ErrorKind::Other, 0), AcceptRetry::Backoff);
        assert_eq!(
            classify_accept_error(io::ErrorKind::Other, MAX_CONSECUTIVE_ACCEPT_ERRORS - 2),
            AcceptRetry::Backoff
        );
        assert_eq!(
            classify_accept_error(io::ErrorKind::Other, MAX_CONSECUTIVE_ACCEPT_ERRORS - 1),
            AcceptRetry::GiveUp
        );
        assert_eq!(
            classify_accept_error(io::ErrorKind::Other, MAX_CONSECUTIVE_ACCEPT_ERRORS),
            AcceptRetry::GiveUp
        );
    }

    /// Pins the fix for the unbounded `read_line` memory-exhaustion bug: a
    /// peer that floods data without ever sending a newline must not grow
    /// `read_one_event`'s buffer past `MAX_EVENT_LINE_LEN`, and must get
    /// back `None` promptly (well within `EVENT_READ_TIMEOUT`) once the cap
    /// is hit, rather than hang until the timeout fires.
    #[tokio::test]
    async fn read_one_event_caps_a_newline_free_flood_instead_of_growing_unbounded() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let writer_task = tokio::spawn(async move {
            let chunk = vec![b'a'; MAX_EVENT_LINE_LEN as usize];
            loop {
                if writer.write_all(&chunk).await.is_err() {
                    return;
                }
            }
        });

        let result = tokio::time::timeout(Duration::from_secs(2), read_one_event(reader))
            .await
            .expect("read_one_event must return promptly once MAX_EVENT_LINE_LEN is hit, not hang until EVENT_READ_TIMEOUT");
        assert_eq!(result, None, "a newline-free flood is malformed input, not a valid event");

        writer_task.abort();
    }

    /// Pins the fix for the silent-peer hang: a peer that connects and then
    /// sends nothing at all never hits `MAX_EVENT_LINE_LEN`, so only
    /// `EVENT_READ_TIMEOUT` covers it. Uses a paused, auto-advancing clock
    /// so this doesn't burn 5 real seconds in the test suite.
    #[tokio::test(start_paused = true)]
    async fn read_one_event_gives_up_on_a_peer_that_never_sends_anything() {
        let (_silent_peer, reader) = UnixStream::pair().unwrap();
        assert_eq!(read_one_event(reader).await, None);
    }

    /// Pins the fix for the idle-exit socket-unlink race: a gracefully
    /// self-exiting daemon must leave its socket file in place for the
    /// sweep to reclaim, not `remove_file` it itself.
    #[tokio::test]
    async fn a_self_exiting_daemon_leaves_its_socket_for_the_sweep_instead_of_unlinking_it() {
        let dir = tempfile::tempdir().unwrap();
        let hookd_sock_path = dir.path().join("claude-hookd-t.sock");

        tokio::time::timeout(
            Duration::from_secs(2),
            run(DaemonConfig {
                sock_path: hookd_sock_path.clone(),
                delivery: Delivery::DirectTty { path: dir.path().join("fake-tty") },
                idle_color: (0, 0, 0),
                attention_color: (0, 0, 0),
                waiting_color: (0, 0, 0),
                attention_timeout: Duration::from_millis(50),
                max_deferral: Duration::from_millis(50),
                idle_exit: Duration::from_millis(50),
                teammate_dispatch_ttl: Duration::from_millis(50),
                hooks_dir: None,
            }),
        )
        .await
        .expect("daemon did not self-exit in time");

        assert!(hookd_sock_path.exists(), "self-exit must not unlink its own socket file");

        for _ in 0..200 {
            if std::os::unix::net::UnixStream::connect(&hookd_sock_path).is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }

        let removed = super::super::ctl_gc::sweep_stale_sockets(dir.path(), "claude-hookd-", Duration::from_secs(24 * 60 * 60))
            .expect("sweep must not fail on a plain tempdir");
        assert_eq!(removed, vec![hookd_sock_path], "the sweep must be the one to reclaim it");
    }
}
