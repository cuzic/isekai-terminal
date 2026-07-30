//! The `isekai-pipe claude-hookd __serve` async daemon loop and its
//! `Command`-line parsing. `__serve` is intentionally undocumented in
//! `--help` (leading underscore convention) — it is spawned only by
//! [`super::spawn_detached_daemon`], never meant to be typed by a human.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::UnixListener;

use super::state::{apply_event, apply_timeout, Action, HookEvent, TabState};

/// How long an `Attention` session stays that way without a debounce
/// refresh or a `Resolve` before it reverts to `Idle` on its own
/// (`ISEKAI_PIPE_DESIGN.md` §8 Epic Q). Not yet configurable — the pure
/// state machine (`state.rs`) already takes this as a parameter, so wiring
/// up an override later (`#@isekai tab-attention-timeout-secs`, noted as a
/// future increment in the design doc) needs no change here beyond reading
/// the value from somewhere.
const ATTENTION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How long this daemon keeps running with no events at all before exiting
/// on its own (Epic M's "no resident sweeper" opportunistic policy, applied
/// to this daemon's own lifetime — its socket file is left for the next
/// `claude-hookd event` invocation's `sweep_stale_ctl_sockets_on_remote`
/// call to reclaim, same as every other ctl-socket path).
const IDLE_EXIT: Duration = Duration::from_secs(60 * 60);

/// How long to sleep after a resource-exhaustion class `accept()` failure
/// (EMFILE/ENFILE/ENOMEM/...) before retrying, so a persistent failure
/// becomes a slow retry loop instead of a 100%-CPU busy loop. Fixed, not
/// exponential: this daemon serves a single tab and `MAX_CONSECUTIVE_ACCEPT_ERRORS`
/// already bounds the total time spent retrying, so there is no runaway to
/// dampen further — and a fixed short delay recovers faster once the
/// resource pressure clears.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// How many consecutive non-transient `accept()` failures (see
/// [`classify_accept_error`]) this daemon tolerates before giving up on its
/// listener entirely. At `ACCEPT_ERROR_BACKOFF` each, this is ~3.2s of
/// busy-but-throttled retrying — short enough that a client's bounded
/// `SPAWN_RETRY_DELAYS_MS` retry (see `super::event_command`) will simply
/// spawn a fresh daemon rather than wait out the old one.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: u32 = 32;

/// Hard cap on a single hook event line, applied via [`AsyncReadExt::take`]
/// in [`read_one_event`] so a peer that never sends a newline can't grow
/// that connection's buffer without bound (every other reader in this
/// codebase already caps its input — see `isekai_ssh::native::mux::
/// ctl_forward::read_ctl_line` for the precedent this follows). Deliberately
/// much smaller than `isekai_protocol::MAX_CTL_MESSAGE_LINE_LEN` (8MB, sized
/// for base64-encoded clipboard images on an unrelated wire format): a real
/// `{"session_id":"...","event":"notify"}` line is on the order of 60 bytes,
/// this cap applies per-connection with no limit on concurrent connections,
/// and `read_one_event`'s own docs already commit to this being a distinct,
/// intentionally minimal wire format.
const MAX_EVENT_LINE_LEN: u64 = 64 * 1024;

/// Hard cap on how long [`read_one_event`] waits for a complete line. Without
/// this, a peer that connects and then sends nothing at all (never hitting
/// `MAX_EVENT_LINE_LEN`, so that cap alone doesn't help) holds its `accept`ed
/// fd and spawned task forever — see the
/// `a_hung_connection_does_not_block_other_connections_or_timers` test,
/// which already relies on such connections being tolerated, just not
/// resolved. Enough such connections exhaust fds and trigger the very
/// `accept()` failures `ACCEPT_ERROR_BACKOFF`/`MAX_CONSECUTIVE_ACCEPT_ERRORS`
/// exist to survive. A well-behaved client writes one line and drops the
/// connection immediately, so 5s is generous slack, not a tight budget.
const EVENT_READ_TIMEOUT: Duration = Duration::from_secs(5);

struct DaemonConfig {
    sock_path: PathBuf,
    ctl_sock_path: PathBuf,
    idle_color: (u8, u8, u8),
    attention_color: (u8, u8, u8),
    /// Overridable only via the undocumented `--attention-timeout-ms`/
    /// `--idle-exit-ms` flags, which exist solely so this module's own
    /// tests can drive a real daemon through both timeout paths in
    /// milliseconds instead of the real 10-minute/1-hour durations (Opus
    /// review: a hidden override here, not just in the pure `state.rs`
    /// function, is what makes the *daemon loop itself* — bind, self-heal
    /// paint, accept, self-exit — end-to-end testable at all).
    attention_timeout: Duration,
    idle_exit: Duration,
}

/// Parses `__serve`'s spawn arguments and runs the daemon loop until it
/// self-exits. Always returns `ExitCode::SUCCESS` — there is no interactive
/// caller to report a non-zero exit code to (see module docs), and a
/// malformed spawn (which should never happen, since the only caller is
/// [`super::spawn_detached_daemon`] in the same binary) has nothing sane to
/// do besides exit immediately.
pub(crate) async fn serve_command(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut sock_path = None;
    let mut ctl_sock_path = None;
    let mut idle_color = None;
    let mut attention_color = None;
    let mut attention_timeout = ATTENTION_TIMEOUT;
    let mut idle_exit = IDLE_EXIT;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sock" => sock_path = args.next().map(PathBuf::from),
            "--ctl-sock" => ctl_sock_path = args.next().map(PathBuf::from),
            "--idle-color" => idle_color = args.next().and_then(|v| isekai_pipe_core::parse_hex_color(&v).ok()),
            "--attention-color" => attention_color = args.next().and_then(|v| isekai_pipe_core::parse_hex_color(&v).ok()),
            "--attention-timeout-ms" => {
                if let Some(ms) = args.next().and_then(|v| v.parse().ok()) {
                    attention_timeout = Duration::from_millis(ms);
                }
            }
            "--idle-exit-ms" => {
                if let Some(ms) = args.next().and_then(|v| v.parse().ok()) {
                    idle_exit = Duration::from_millis(ms);
                }
            }
            _ => {}
        }
    }
    let (Some(sock_path), Some(ctl_sock_path)) = (sock_path, ctl_sock_path) else {
        return ExitCode::SUCCESS;
    };
    run(DaemonConfig {
        sock_path,
        ctl_sock_path,
        idle_color: idle_color.unwrap_or(super::DEFAULT_IDLE_COLOR),
        attention_color: attention_color.unwrap_or(super::DEFAULT_ATTENTION_COLOR),
        attention_timeout,
        idle_exit,
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
    // `AddrInUse`. `sweep_stale_ctl_sockets_on_remote` only ever removes
    // sockets nothing is listening on (`isekai_pipe_core::ctl_gc::
    // is_abandoned`'s `ConnectionRefused` check) — it can't race a genuine
    // live sibling out from under it.
    crate::ctl::sweep_stale_ctl_sockets_on_remote();

    let listener = match UnixListener::bind(&config.sock_path) {
        Ok(listener) => listener,
        // Lost the spawn race to a sibling `__serve` that bound first — it
        // is now serving this tab, so this process has nothing left to do.
        // Not an error condition worth logging: this is the expected
        // outcome for the losing side of a race the client-side bounded
        // retry (`super::event_command`'s `SPAWN_RETRY_DELAYS_MS` loop) is
        // specifically designed to tolerate.
        Err(_) => return,
    };

    // Self-healing (design doc item 4): paint idle unconditionally on
    // startup, regardless of why this daemon is starting — including a
    // fresh tab (harmless no-op-looking write to a terminal already showing
    // its default color) and, more importantly, a restart after a crash or
    // the 1h self-exit that left a *previous* daemon's tab stuck in the
    // attention color with nothing left to revert it.
    send_tab_color(&config.ctl_sock_path, config.idle_color).await;

    // Accepting and reading each connection happens on its own spawned task,
    // separate from the main loop below (found by Codex code review,
    // 2026-07-25): the original version called `read_one_event(stream).await`
    // directly inside the `listener.accept()` branch of the `tokio::select!`
    // below, so a connection that opens and then never sends a complete line
    // — a broken client, or even just `nc -U <sock>` left open by a human
    // poking at the socket — blocked that whole `select!` (and therefore the
    // attention timeout, idle-exit, and every other pending connection) for
    // as long as it stayed open. Each spawned task only does I/O and forwards
    // the parsed `(session_id, event)` to `event_rx`; all state mutation and
    // `ctl` sends stay serialized on the single task below, so events are
    // still applied one at a time in arrival order.
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
                // No logging here: `__serve` never installs a logger (unlike
                // `connect`/`serve`'s `env_logger::Builder::init`) and
                // `spawn_detached_daemon` redirects its stdio to `/dev/null`,
                // so a `log::warn!` in this daemon would be silently
                // discarded — observing this would need a file-target
                // logger, which nothing currently needs enough to justify.
                Err(e) => match classify_accept_error(e.kind(), consecutive_accept_errors) {
                    // Peer-caused, self-clearing: retry immediately and
                    // don't count it toward giving up.
                    AcceptRetry::Immediate => {}
                    AcceptRetry::Backoff => {
                        consecutive_accept_errors += 1;
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                    }
                    // Returning here completes `accept_task`'s own future,
                    // which the `select!` below observes directly via
                    // `&mut accept_task` and `break`s on — not via `event_tx`
                    // closing. Dropping `event_tx` (this task's own clone) is
                    // not enough by itself: an in-flight `read_one_event`
                    // task spawned from an earlier `Ok` accept still holds
                    // its own clone for up to `EVENT_READ_TIMEOUT`, so
                    // `event_rx.recv()` returning `None` could lag up to 5s
                    // behind this `return` (Opus review, 2026-07-30). That
                    // lag used to leave this listener's socket file on disk
                    // but unlistened-on for up to 5s, wide enough for a
                    // client's `sweep_stale_ctl_sockets_on_remote` to reclaim
                    // it and a successor daemon to bind and start serving —
                    // only for this `run`'s eventual `remove_file` (once it
                    // finally noticed) to unlink the *successor's* live
                    // socket out from under it. Breaking on `accept_task`'s
                    // own completion instead shrinks that window down to at
                    // most one `select!` iteration's worth of already-queued
                    // `event_rx` messages (`tokio::select!` isn't `biased`,
                    // so a pending event can still be processed before this
                    // branch gets polled) — no longer bounded by
                    // `EVENT_READ_TIMEOUT`, restoring §8 Epic Q's guarantee
                    // that this daemon's self-exit never corrupts a
                    // successor's.
                    AcceptRetry::GiveUp => return,
                },
            }
        }
    });

    let mut state = TabState::new();
    let mut idle_exit_deadline = Instant::now() + config.idle_exit;
    loop {
        let idle_exit_sleep = tokio::time::sleep(idle_exit_deadline.saturating_duration_since(Instant::now()));
        tokio::select! {
            // `accept_task` only ever completes by returning (its
            // `AcceptRetry::GiveUp` self-destruct path) or panicking — either
            // way nothing can accept a new connection anymore, so stop
            // immediately rather than wait out any still-in-flight
            // `read_one_event` tasks (see the long comment at the `GiveUp`
            // arm above for why this can't instead wait on `event_rx`
            // closing).
            _ = &mut accept_task => break,
            received = event_rx.recv() => {
                // In practice unreachable before the `accept_task` branch
                // above fires, since that task is the only thing that could
                // ever drop every `event_tx` clone including its own — kept
                // as a defensive fallback, not the primary signal anymore.
                let Some((session_id, event)) = received else { break };
                idle_exit_deadline = Instant::now() + config.idle_exit;
                let now = Instant::now();
                let (next_state, actions) = apply_event(&state, &session_id, event, now, config.attention_timeout);
                state = next_state;
                execute_actions(&config, &actions).await;
            }
            () = attention_sleep(&state) => {
                let (next_state, actions) = apply_timeout(&state, Instant::now());
                state = next_state;
                execute_actions(&config, &actions).await;
            }
            () = idle_exit_sleep => {
                break;
            }
        }
    }
    accept_task.abort();
    // Graceful-exit cleanup. Not load-bearing for correctness (the sweep in
    // the next `claude-hookd event`/`__serve` invocation would reclaim this
    // file regardless, via the same `is_abandoned` check that protects
    // against the crash case where this line never runs at all) — just
    // avoids leaving a guaranteed-dead file lying around for longer than
    // necessary between now and whenever this tab's next event arrives.
    let _ = std::fs::remove_file(&config.sock_path);
}

/// What the `accept()` loop in [`run`] should do after a failed accept,
/// decided purely from the error kind and how many *other* non-transient
/// failures already happened in a row (i.e. before this one) — kept separate
/// from the loop itself so it's unit-testable without a real socket
/// (`rust-ssot.md`'s "decision logic in one place, not re-derived downstream"
/// principle, applied to this daemon's own accept loop rather than session
/// state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptRetry {
    /// A transient, peer-caused failure (the connecting client's own
    /// syscall was interrupted, or it aborted the connection before the
    /// kernel finished the handshake) — mirrors hyper's `is_connection_error`
    /// classification. Retrying instantly is correct and doesn't indicate
    /// anything is actually wrong with this listener. This assumes such
    /// failures are inherently per-connection and don't persist across
    /// accepts (true for `ECONNABORTED`/`EINTR` on a Unix listener) — if
    /// that assumption ever breaks, `Immediate` failures still never sleep
    /// or count toward `GiveUp`, so this classification alone would degrade
    /// back into a busy loop for those specific kinds.
    Immediate,
    /// A resource-exhaustion class failure (EMFILE/ENFILE/ENOMEM/...) that
    /// won't clear itself instantly — sleep briefly before retrying so this
    /// doesn't become a 100%-CPU busy loop.
    Backoff,
    /// `Backoff`-class failures just reached `MAX_CONSECUTIVE_ACCEPT_ERRORS`
    /// in a row with no successful accept in between — give up on this
    /// listener rather than keep retrying indefinitely.
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

/// Resolves at `state`'s earliest pending `Attention` deadline, or never
/// resolves at all if the tab is fully idle — the `tokio::select!` branch
/// this drives is naturally inert (never the one that fires) in that case,
/// without needing a separate `if` guard.
async fn attention_sleep(state: &TabState) {
    match state.next_deadline() {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending::<()>().await,
    }
}

/// Reads exactly one `{"session_id": "...", "event": "notify"|"resolve"}`
/// line — the daemon's own minimal wire format, deliberately decoupled from
/// Claude Code's own hook JSON schema (only `super::parse_hook_event`, the
/// client side, needs to know that shape; a future Claude Code hook schema
/// change touches one function, not this one). Any I/O error or malformed/
/// unrecognized line is treated as "nothing happened" — a hostile or
/// confused peer must never crash or wedge the daemon: `MAX_EVENT_LINE_LEN`
/// bounds a peer that never sends a newline, and `EVENT_READ_TIMEOUT` bounds
/// one that sends nothing at all.
async fn read_one_event(stream: tokio::net::UnixStream) -> Option<(String, HookEvent)> {
    tokio::time::timeout(EVENT_READ_TIMEOUT, read_one_event_line(stream)).await.ok()?
}

async fn read_one_event_line(stream: tokio::net::UnixStream) -> Option<(String, HookEvent)> {
    // `take` sits *inside* `BufReader` so the cap governs what `BufReader`
    // can ever read from the socket in the first place, rather than
    // capping the already-buffered result: once the limit is hit,
    // `Take::poll_read` reports EOF, `read_line` returns whatever was
    // accumulated (possibly with no trailing newline), and the JSON parse
    // below fails it as malformed — no separate cap-exceeded branch needed.
    let mut reader = BufReader::new(stream.take(MAX_EVENT_LINE_LEN));
    let mut line = String::new();
    reader.read_line(&mut line).await.ok()?;
    let value: serde_json::Value = serde_json::from_str(line.trim_end()).ok()?;
    let session_id = value.get("session_id")?.as_str()?.to_string();
    let event = match value.get("event")?.as_str()? {
        "notify" => HookEvent::Notify,
        "resolve" => HookEvent::Resolve,
        _ => return None,
    };
    Some((session_id, event))
}

async fn execute_actions(config: &DaemonConfig, actions: &[Action]) {
    for action in actions {
        match action {
            Action::SetAttentionColorAndPopup => {
                send_tab_color(&config.ctl_sock_path, config.attention_color).await;
                send_notify_popup(&config.ctl_sock_path).await;
            }
            Action::SetIdleColor => {
                send_tab_color(&config.ctl_sock_path, config.idle_color).await;
            }
        }
    }
}

/// Both send functions are best-effort: `$ISEKAI_CTL_SOCK` can point at a
/// stale forward across a tmux reattach spanning a different SSH connection
/// (`ISEKAI_PIPE_DESIGN.md` Epic M's documented known limitation, which
/// applies identically to `$ISEKAI_TAB_IDLE_COLOR`/`$ISEKAI_TAB_ATTENTION_COLOR`
/// per Epic Q) — a failed send here must never crash or wedge this daemon's
/// event loop, just drop that one color/popup update.
async fn send_tab_color(ctl_sock_path: &std::path::Path, (r, g, b): (u8, u8, u8)) {
    let _ = crate::ctl::send_ctl_message(ctl_sock_path, isekai_protocol::CtlMessage::SetTabColor { r, g, b }).await;
}

async fn send_notify_popup(ctl_sock_path: &std::path::Path) {
    let _ = crate::ctl::send_ctl_message(
        ctl_sock_path,
        isekai_protocol::CtlMessage::Notify {
            kind: isekai_protocol::NotifyKind::Waiting,
            // AI/汎用kind(`Waiting`)なのでtmux_tag/seqは意味を持たない
            // (`isekai_protocol::ctl::CtlMessage::Notify`のdocコメント参照、
            // tmux hook由来kind専用のフィールド)。
            tmux_tag: String::new(),
            seq: 0,
            title: "Claude Code".to_string(),
            body: "needs your input".to_string(),
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::UnixStream;
    use tokio::sync::mpsc;

    /// A fake receiving end of `$ISEKAI_CTL_SOCK` that decodes every
    /// `CtlMessage` `send_tab_color`/`send_notify_popup` actually send over
    /// a real UNIX domain socket connection (preamble line + JSON line, the
    /// real `isekai-pipe ctl` wire format — see `crate::ctl::send_ctl_message`) and
    /// forwards each to `rx`, so the daemon-loop test below observes the
    /// *real* end-to-end effect of a hook event rather than calling
    /// `execute_actions` directly.
    async fn spawn_fake_ctl_socket(sock_path: std::path::PathBuf) -> mpsc::UnboundedReceiver<isekai_protocol::CtlMessage> {
        let listener = UnixListener::bind(&sock_path).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut preamble = String::new();
                    if reader.read_line(&mut preamble).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    if let Ok(msg) = isekai_protocol::decode_ctl_message(line.trim_end().as_bytes()) {
                        let _ = tx.send(msg);
                    }
                });
            }
        });
        rx
    }

    async fn next_message(rx: &mut mpsc::UnboundedReceiver<isekai_protocol::CtlMessage>) -> isekai_protocol::CtlMessage {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for a ctl message")
            .expect("fake ctl socket channel closed unexpectedly")
    }

    /// Drives a real daemon (`run`, not the pure `state.rs` function) through
    /// startup self-heal, a Notify→Attention transition (color + popup),
    /// an attention timeout back to Idle, and self-exit — using millisecond
    /// `--attention-timeout-ms`/`--idle-exit-ms`-equivalent config values
    /// (set directly on `DaemonConfig`, bypassing `serve_command`'s CLI
    /// parsing) so the whole test runs in well under a second instead of
    /// the real 10-minute/1-hour durations (Opus review: this is what makes
    /// the daemon *loop itself*, not just the decision function, testable).
    #[tokio::test]
    async fn daemon_loop_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let ctl_sock_path = dir.path().join("ctl.sock");
        let hookd_sock_path = dir.path().join("hookd.sock");
        let mut ctl_messages = spawn_fake_ctl_socket(ctl_sock_path.clone()).await;

        let idle_color = (0x20, 0x20, 0x20);
        let attention_color = (0xff, 0x88, 0x00);
        let daemon = tokio::spawn(run(DaemonConfig {
            sock_path: hookd_sock_path.clone(),
            ctl_sock_path,
            idle_color,
            attention_color,
            attention_timeout: Duration::from_millis(100),
            idle_exit: Duration::from_millis(300),
        }));

        // Self-heal: idle is painted once on startup, before any client
        // ever connects.
        assert_eq!(next_message(&mut ctl_messages).await, isekai_protocol::CtlMessage::SetTabColor { r: 0x20, g: 0x20, b: 0x20 });

        // A real client connection, exactly like `send_event` in `mod.rs`
        // would make.
        let mut client = UnixStream::connect(&hookd_sock_path).await.unwrap();
        client.write_all(b"{\"session_id\":\"s1\",\"event\":\"notify\"}\n").await.unwrap();
        drop(client);

        assert_eq!(
            next_message(&mut ctl_messages).await,
            isekai_protocol::CtlMessage::SetTabColor { r: 0xff, g: 0x88, b: 0x00 }
        );
        assert!(matches!(next_message(&mut ctl_messages).await, isekai_protocol::CtlMessage::Notify { .. }));

        // No `resolve` ever arrives — the 100ms attention timeout fires and
        // reverts the color on its own.
        assert_eq!(next_message(&mut ctl_messages).await, isekai_protocol::CtlMessage::SetTabColor { r: 0x20, g: 0x20, b: 0x20 });

        // No further events at all — the 300ms idle-exit fires and the
        // daemon task completes on its own, having unlinked its socket.
        tokio::time::timeout(Duration::from_secs(2), daemon).await.expect("daemon did not self-exit in time").unwrap();
        assert!(!hookd_sock_path.exists(), "daemon must unlink its own socket on graceful self-exit");
    }

    #[tokio::test]
    async fn daemon_loses_bind_race_and_exits_immediately_without_disturbing_the_winner() {
        let dir = tempfile::tempdir().unwrap();
        let hookd_sock_path = dir.path().join("hookd.sock");
        // A plain bind (not a full daemon) already occupying the path is
        // enough to simulate "a sibling `__serve` already won" — `run`
        // must not panic, must not remove the winner's socket, and must
        // return promptly.
        let _winner = UnixListener::bind(&hookd_sock_path).unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            run(DaemonConfig {
                sock_path: hookd_sock_path.clone(),
                ctl_sock_path: dir.path().join("ctl.sock"),
                idle_color: (0, 0, 0),
                attention_color: (0, 0, 0),
                attention_timeout: Duration::from_millis(50),
                idle_exit: Duration::from_millis(50),
            }),
        )
        .await;
        assert!(result.is_ok(), "the losing daemon must return promptly, not hang");
        assert!(hookd_sock_path.exists(), "the losing daemon must not remove the winner's live socket");
    }

    /// Pins the fix for the bug Codex code review found (2026-07-25): a
    /// connection that opens and never sends a complete line used to block
    /// the whole `select!` loop (attention timeout, idle-exit, and every
    /// *other* connection) for as long as it stayed open, because
    /// `read_one_event(stream).await` used to run directly inside the
    /// `listener.accept()` branch's body instead of a separately spawned
    /// task. A well-behaved second client's event must still get through —
    /// and get through quickly — while the first, silent connection is still
    /// open.
    #[tokio::test]
    async fn a_hung_connection_does_not_block_other_connections_or_timers() {
        let dir = tempfile::tempdir().unwrap();
        let ctl_sock_path = dir.path().join("ctl.sock");
        let hookd_sock_path = dir.path().join("hookd.sock");
        let mut ctl_messages = spawn_fake_ctl_socket(ctl_sock_path.clone()).await;

        let daemon = tokio::spawn(run(DaemonConfig {
            sock_path: hookd_sock_path.clone(),
            ctl_sock_path,
            idle_color: (0x20, 0x20, 0x20),
            attention_color: (0xff, 0x88, 0x00),
            attention_timeout: Duration::from_millis(100),
            idle_exit: Duration::from_secs(30),
        }));
        assert_eq!(next_message(&mut ctl_messages).await, isekai_protocol::CtlMessage::SetTabColor { r: 0x20, g: 0x20, b: 0x20 });

        // Opens a connection and deliberately never writes anything, never
        // closes it — kept alive for the rest of the test by holding `_hung`.
        let _hung = UnixStream::connect(&hookd_sock_path).await.unwrap();

        // A second, well-behaved client must still be served promptly.
        let mut client = UnixStream::connect(&hookd_sock_path).await.unwrap();
        client.write_all(b"{\"session_id\":\"s1\",\"event\":\"notify\"}\n").await.unwrap();
        drop(client);
        assert_eq!(
            next_message(&mut ctl_messages).await,
            isekai_protocol::CtlMessage::SetTabColor { r: 0xff, g: 0x88, b: 0x00 }
        );
        assert!(matches!(next_message(&mut ctl_messages).await, isekai_protocol::CtlMessage::Notify { .. }));

        // The attention timeout must still fire on schedule too, not just
        // new connections.
        assert_eq!(next_message(&mut ctl_messages).await, isekai_protocol::CtlMessage::SetTabColor { r: 0x20, g: 0x20, b: 0x20 });

        drop(daemon); // stop the still-running daemon task; nothing left to assert.
    }

    /// Pins the fix for the accept-error busy-loop bug (crash-focused review,
    /// 2026-07-30): connection-level failures (a client that aborts mid-
    /// handshake) must not count toward giving up, resource-exhaustion class
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
            // Even one failure short of the give-up threshold, a transient
            // error still doesn't count toward it.
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

    /// Pins the fix for the unbounded `read_line` memory-exhaustion bug
    /// (crash-focused review, 2026-07-30): a peer that floods data without
    /// ever sending a newline must not grow `read_one_event`'s buffer past
    /// `MAX_EVENT_LINE_LEN`, and must get back `None` promptly (well within
    /// `EVENT_READ_TIMEOUT`) once the cap is hit, rather than hang until the
    /// timeout fires.
    #[tokio::test]
    async fn read_one_event_caps_a_newline_free_flood_instead_of_growing_unbounded() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let writer_task = tokio::spawn(async move {
            // More than MAX_EVENT_LINE_LEN, no newline anywhere in it. The
            // socket's own kernel buffer will make this block well before
            // all of it is written, which is fine — `read_one_event` only
            // needs to observe the cap being hit, not consume this whole
            // stream.
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

    /// End-to-end companion to the cap test above, in the same style as
    /// `a_hung_connection_does_not_block_other_connections_or_timers`: drives
    /// this through the *whole* daemon (accept loop's per-connection spawn +
    /// `read_one_event`'s cap together), not `read_one_event` in isolation,
    /// concurrently with a legitimate client. In practice the flooding
    /// connection resolves to `None` within milliseconds of hitting the cap
    /// (`MAX_EVENT_LINE_LEN` is far smaller than a Unix socket's kernel
    /// buffer), so this isn't exercising sustained blocking so much as
    /// guarding against a regression where the cap is missing or wired up
    /// wrong in the full daemon despite `read_one_event`'s own unit test
    /// passing.
    #[tokio::test]
    async fn an_oversized_flood_does_not_block_other_connections() {
        let dir = tempfile::tempdir().unwrap();
        let ctl_sock_path = dir.path().join("ctl.sock");
        let hookd_sock_path = dir.path().join("hookd.sock");
        let mut ctl_messages = spawn_fake_ctl_socket(ctl_sock_path.clone()).await;

        let daemon = tokio::spawn(run(DaemonConfig {
            sock_path: hookd_sock_path.clone(),
            ctl_sock_path,
            idle_color: (0x20, 0x20, 0x20),
            attention_color: (0xff, 0x88, 0x00),
            attention_timeout: Duration::from_millis(100),
            idle_exit: Duration::from_secs(30),
        }));
        assert_eq!(next_message(&mut ctl_messages).await, isekai_protocol::CtlMessage::SetTabColor { r: 0x20, g: 0x20, b: 0x20 });

        let mut flooder = UnixStream::connect(&hookd_sock_path).await.unwrap();
        let flood_task = tokio::spawn(async move {
            let chunk = vec![b'a'; MAX_EVENT_LINE_LEN as usize];
            loop {
                if flooder.write_all(&chunk).await.is_err() {
                    return;
                }
            }
        });

        let mut client = UnixStream::connect(&hookd_sock_path).await.unwrap();
        client.write_all(b"{\"session_id\":\"s1\",\"event\":\"notify\"}\n").await.unwrap();
        drop(client);
        assert_eq!(
            next_message(&mut ctl_messages).await,
            isekai_protocol::CtlMessage::SetTabColor { r: 0xff, g: 0x88, b: 0x00 }
        );
        assert!(matches!(next_message(&mut ctl_messages).await, isekai_protocol::CtlMessage::Notify { .. }));

        flood_task.abort();
        drop(daemon);
    }

    /// Pins the fix for the silent-peer hang (crash-focused review,
    /// 2026-07-30, Finding 2): a peer that connects and then sends nothing
    /// at all never hits `MAX_EVENT_LINE_LEN`, so the cap test above doesn't
    /// cover it — only `EVENT_READ_TIMEOUT` does. Uses a paused, auto-
    /// advancing clock so this doesn't burn 5 real seconds in the test
    /// suite: `read_one_event`'s only path to resolving here is its
    /// internal `tokio::time::timeout` firing, and with nothing else
    /// runnable, the paused clock jumps straight to that deadline.
    #[tokio::test(start_paused = true)]
    async fn read_one_event_gives_up_on_a_peer_that_never_sends_anything() {
        // Holding `_silent_peer` keeps the socket open (no EOF) — without
        // `EVENT_READ_TIMEOUT`, `read_one_event` would await forever.
        let (_silent_peer, reader) = UnixStream::pair().unwrap();
        assert_eq!(read_one_event(reader).await, None);
    }
}
