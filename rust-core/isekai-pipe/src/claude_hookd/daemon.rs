//! The `isekai-pipe claude-hookd __serve` async daemon loop and its
//! `Command`-line parsing. `__serve` is intentionally undocumented in
//! `--help` (leading underscore convention) — it is spawned only by
//! [`super::spawn_detached_daemon`], never meant to be typed by a human.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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

/// Fallback colors used when `#@isekai tab-idle-color`/`tab-attention-color`
/// was never set (no `--idle-color`/`--attention-color` spawn argument).
const DEFAULT_IDLE_COLOR: (u8, u8, u8) = (0x20, 0x20, 0x20);
const DEFAULT_ATTENTION_COLOR: (u8, u8, u8) = (0xff, 0x88, 0x00);

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
        idle_color: idle_color.unwrap_or(DEFAULT_IDLE_COLOR),
        attention_color: attention_color.unwrap_or(DEFAULT_ATTENTION_COLOR),
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

    let mut state = TabState::new();
    let mut idle_exit_deadline = Instant::now() + config.idle_exit;
    loop {
        let idle_exit_sleep = tokio::time::sleep(idle_exit_deadline.saturating_duration_since(Instant::now()));
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _addr)) = accepted else { continue };
                idle_exit_deadline = Instant::now() + config.idle_exit;
                if let Some((session_id, event)) = read_one_event(stream).await {
                    let now = Instant::now();
                    let (next_state, actions) = apply_event(&state, &session_id, event, now, config.attention_timeout);
                    state = next_state;
                    execute_actions(&config, &actions).await;
                }
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
    // Graceful-exit cleanup. Not load-bearing for correctness (the sweep in
    // the next `claude-hookd event`/`__serve` invocation would reclaim this
    // file regardless, via the same `is_abandoned` check that protects
    // against the crash case where this line never runs at all) — just
    // avoids leaving a guaranteed-dead file lying around for longer than
    // necessary between now and whenever this tab's next event arrives.
    let _ = std::fs::remove_file(&config.sock_path);
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
/// confused peer must never crash or wedge the daemon.
async fn read_one_event(stream: tokio::net::UnixStream) -> Option<(String, HookEvent)> {
    let mut reader = BufReader::new(stream);
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
            title: "Claude Code".to_string(),
            body: "needs your input".to_string(),
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
