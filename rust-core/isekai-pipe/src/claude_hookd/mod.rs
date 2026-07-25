//! `isekai-pipe claude-hookd` (`ISEKAI_PIPE_DESIGN.md` §8 Epic Q): a small
//! per-tab daemon that turns Claude Code hook events into a persistent,
//! debounced tab-color indicator via the existing `isekai-pipe ctl`
//! send path (`CtlMessage::SetTabColor`/`Notify`) — no new SSH/QUIC channel,
//! no new wire protocol, this is purely local plumbing on the remote host.
//!
//! Split into [`state`] (the actual decision logic: an I/O-free, unit-tested
//! pure function, `.claude/rules/rust-ssot.md`'s "state and decision logic
//! belong in one place" principle applied at the scale of this one small
//! feature), [`daemon`] (the async loop and daemon-side `__serve` CLI), and
//! this module (the `event` CLI hook scripts actually call: Claude Code hook
//! JSON parsing, per-tab daemon identification/lazy-spawn, and the bounded
//! retry that tolerates the spawn race).
//!
//! Unix-only (`UnixListener`/`UnixStream`, matching `ctl_forward.rs`'s own
//! scoping) — the daemon runs on the *remote* host, which for this feature's
//! entire premise (Claude Code hooks) is realistically always Unix. On other
//! platforms `claude_hookd_command` is a silent, immediate no-op — see the
//! design doc's "明示的にスコープ外(v1)" note that this whole Epic only
//! affects the `isekai-ssh` + Windows Terminal combination on the *local*
//! side; there's never a Windows *remote* target for it either.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

#[cfg(unix)]
mod daemon;
mod state;

#[cfg(unix)]
use state::HookEvent;

/// Bounded retry/backoff for the "daemon wasn't running yet, spawned one,
/// now waiting for it to bind" window (`ISEKAI_PIPE_DESIGN.md` §8 Epic Q).
/// A single retry loses the race when two hooks fire near-simultaneously
/// and both spawn (the loser's *client* would give up before the winner's
/// daemon finishes binding) — this is generous enough that even a slow
/// process start under load has several chances, while the whole sequence
/// still finishes in ~1.5s worst case so a Claude Code hook (which this
/// must never visibly block) doesn't stall.
#[cfg(unix)]
const SPAWN_RETRY_DELAYS_MS: [u64; 5] = [50, 100, 200, 400, 800];

#[cfg(unix)]
pub(crate) async fn claude_hookd_command(mut args: impl Iterator<Item = String>) -> ExitCode {
    match args.next().as_deref() {
        Some("event") => event_command().await,
        // Undocumented on purpose (leading underscore): spawned only by
        // `spawn_detached_daemon` below, never typed by a human.
        Some("__serve") => daemon::serve_command(args).await,
        _ => {
            eprintln!("isekai-pipe claude-hookd: expected \"event\" (see ISEKAI_PIPE_DESIGN.md §8 Epic Q)");
            // Still exit 0: this subcommand's entire contract (design doc
            // §5) is "never visibly disrupt whatever invoked it", which for
            // a `PreToolUse` hook specifically means a non-zero exit here
            // would block the *user's* tool call over what is at most a
            // misconfigured `.claude/settings.json` — a cosmetic feature
            // must not have that blast radius.
            ExitCode::SUCCESS
        }
    }
}

#[cfg(not(unix))]
pub(crate) async fn claude_hookd_command(_args: impl Iterator<Item = String>) -> ExitCode {
    ExitCode::SUCCESS
}

/// Reads Claude Code's hook JSON payload from stdin, decides whether it
/// means anything to `claude-hookd` at all, and if so relays it to this
/// tab's daemon (lazily spawning one if none is running yet). Always
/// returns `ExitCode::SUCCESS` — see [`claude_hookd_command`]'s doc comment
/// on why a non-zero exit or any stdout output here is unacceptable for a
/// `PreToolUse`/`PostToolUse` hook.
#[cfg(unix)]
async fn event_command() -> ExitCode {
    use tokio::io::AsyncReadExt as _;

    let mut payload = Vec::new();
    if tokio::io::stdin().read_to_end(&mut payload).await.is_err() {
        return ExitCode::SUCCESS;
    }
    let Some((session_id, event)) = parse_hook_event(&payload) else {
        return ExitCode::SUCCESS;
    };
    // `#@isekai ctl-socket` is opt-in and defaults off (`ISEKAI_PIPE_DESIGN.md`
    // Epic M) — most sessions simply won't have this set, which is not an
    // error, just nothing for `claude-hookd` to do.
    let Ok(ctl_sock) = std::env::var("ISEKAI_CTL_SOCK") else {
        return ExitCode::SUCCESS;
    };
    let ctl_sock_path = PathBuf::from(ctl_sock);
    let Some(daemon_sock_path) = derive_daemon_sock_path(&ctl_sock_path) else {
        return ExitCode::SUCCESS;
    };

    // Same lazy sweep every `isekai-pipe ctl` invocation already does
    // (`crate::ctl::sweep_stale_ctl_sockets_on_remote`) — reused here rather
    // than duplicated so a stale `isekai-pipe-ctl-hookd-*.sock` left by a
    // crashed daemon (or this tab's daemon having already hit its 1h
    // self-exit) doesn't cause a spurious connect failure/spawn every time.
    crate::ctl::sweep_stale_ctl_sockets_on_remote();

    if send_event(&daemon_sock_path, &session_id, event).await {
        return ExitCode::SUCCESS;
    }

    let idle_color = std::env::var("ISEKAI_TAB_IDLE_COLOR").ok().and_then(|v| isekai_pipe_core::parse_hex_color(&v).ok());
    let attention_color = std::env::var("ISEKAI_TAB_ATTENTION_COLOR")
        .ok()
        .and_then(|v| isekai_pipe_core::parse_hex_color(&v).ok());
    spawn_detached_daemon(&daemon_sock_path, &ctl_sock_path, idle_color, attention_color);

    for delay_ms in SPAWN_RETRY_DELAYS_MS {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        if send_event(&daemon_sock_path, &session_id, event).await {
            break;
        }
    }
    ExitCode::SUCCESS
}

/// Extracts just the two fields `claude-hookd` needs from Claude Code's hook
/// JSON payload and maps `hook_event_name` (`tool_name`, for
/// `PreToolUse`/`PostToolUse`) to this module's own [`HookEvent`] — the one
/// place in this feature that knows Claude Code's hook schema, deliberately
/// kept separate from the daemon's own minimal wire format
/// (`daemon::read_one_event`) so a future Claude Code schema change touches
/// only this function. `None` covers both "malformed JSON" and "an event
/// type `claude-hookd` doesn't act on" identically — both are silent no-ops.
#[cfg(unix)]
fn parse_hook_event(payload: &[u8]) -> Option<(String, HookEvent)> {
    let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let session_id = value.get("session_id")?.as_str()?.to_string();
    let hook_event_name = value.get("hook_event_name")?.as_str()?;
    let tool_name = value.get("tool_name").and_then(|v| v.as_str());
    let event = match hook_event_name {
        "Notification" | "Stop" => HookEvent::Notify,
        "UserPromptSubmit" => HookEvent::Resolve,
        "PreToolUse" if tool_name == Some("AskUserQuestion") => HookEvent::Notify,
        "PostToolUse" if tool_name == Some("AskUserQuestion") => HookEvent::Resolve,
        _ => return None,
    };
    Some((session_id, event))
}

/// `$ISEKAI_CTL_SOCK` is `/tmp/isekai-pipe-ctl-<token>.sock`
/// (`isekai-ssh::ctl_forward::REMOTE_SOCK_PREFIX` — this crate can't import
/// that `pub(crate)` constant from a sibling crate, so the prefix is
/// duplicated as a literal here and pinned by
/// `derives_from_the_real_ctl_socket_naming_convention` below rather than
/// silently drifting). The derived path keeps the same directory and the
/// same `isekai-pipe-ctl-` prefix `sweep_stale_ctl_sockets_on_remote`
/// already scans for (`crate::ctl::CTL_SOCKET_REMOTE_PREFIX`), just with an
/// extra `hookd-` component, so it is covered by that existing sweep without
/// any new cleanup logic.
#[cfg(unix)]
fn derive_daemon_sock_path(ctl_sock_path: &Path) -> Option<PathBuf> {
    let name = ctl_sock_path.file_name()?.to_str()?;
    let token = name.strip_prefix("isekai-pipe-ctl-")?.strip_suffix(".sock")?;
    let parent = ctl_sock_path.parent()?;
    Some(parent.join(format!("isekai-pipe-ctl-hookd-{token}.sock")))
}

/// Connects to the daemon socket and sends one line, this module's own
/// minimal wire format (see `daemon::read_one_event`). Returns whether the
/// connect+write succeeded — a proxy for "a daemon is reachable and got
/// this", not a guarantee it was applied (fire-and-forget, same trust model
/// `isekai-pipe ctl title` etc. already use for the real ctl socket).
#[cfg(unix)]
async fn send_event(sock_path: &Path, session_id: &str, event: HookEvent) -> bool {
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::UnixStream;

    let Ok(mut stream) = UnixStream::connect(sock_path).await else {
        return false;
    };
    let event_name = match event {
        HookEvent::Notify => "notify",
        HookEvent::Resolve => "resolve",
    };
    let Ok(session_id_json) = serde_json::to_string(session_id) else {
        return false;
    };
    let line = format!("{{\"session_id\":{session_id_json},\"event\":\"{event_name}\"}}\n");
    stream.write_all(line.as_bytes()).await.is_ok()
}

/// Spawns `isekai-pipe claude-hookd __serve` fully detached: `setsid` so it
/// survives this short-lived hook process exiting and never receives its
/// controlling terminal's signals, and all three standard streams
/// redirected to `/dev/null` so it can never hold this hook's stdout pipe
/// open — Claude Code reads that pipe to EOF, so an inherited stdout would
/// block Claude Code itself for as long as the daemon runs (up to
/// `IDLE_EXIT`), not merely the hook (found in the Opus design review before
/// any of this shipped). Colors are passed explicitly as spawn arguments
/// (`isekai_pipe_core::format_hex_color`, so no `#`/shell concerns even
/// though this is `Command` argv, not a shell string) rather than left to
/// env var inheritance, so the daemon's configuration doesn't depend on
/// which of several near-simultaneous hook processes happened to win the
/// spawn race.
#[cfg(unix)]
fn spawn_detached_daemon(
    daemon_sock_path: &Path,
    ctl_sock_path: &Path,
    idle_color: Option<(u8, u8, u8)>,
    attention_color: Option<(u8, u8, u8)>,
) {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("isekai-pipe"));
    let mut cmd = Command::new(exe);
    cmd.arg("claude-hookd")
        .arg("__serve")
        .arg("--sock")
        .arg(daemon_sock_path)
        .arg("--ctl-sock")
        .arg(ctl_sock_path);
    if let Some(color) = idle_color {
        cmd.arg("--idle-color").arg(isekai_pipe_core::format_hex_color(color));
    }
    if let Some(color) = attention_color {
        cmd.arg("--attention-color").arg(isekai_pipe_core::format_hex_color(color));
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // Safety: the closure only calls `setsid(2)`, an async-signal-safe libc
    // call that touches no Rust-managed state — the standard justification
    // for a `pre_exec` closure (see `std::os::unix::process::CommandExt::
    // pre_exec`'s own docs on what's sound to do here).
    unsafe {
        cmd.pre_exec(|| if raw_setsid() < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
    }
    // Best-effort: if spawning fails, this one hook event is silently
    // dropped rather than surfaced — the next hook invocation gets another
    // chance to lazily spawn, and nothing here may ever block or fail the
    // caller (see `claude_hookd_command`'s doc comment).
    let _ = cmd.spawn();
}

#[cfg(unix)]
unsafe extern "C" {
    fn setsid() -> i32;
}

#[cfg(unix)]
fn raw_setsid() -> i32 {
    unsafe { setsid() }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn parses_notification_and_stop_as_notify() {
        for name in ["Notification", "Stop"] {
            let payload = format!(r#"{{"session_id":"s1","hook_event_name":"{name}"}}"#);
            assert_eq!(parse_hook_event(payload.as_bytes()), Some(("s1".to_string(), HookEvent::Notify)));
        }
    }

    #[test]
    fn parses_user_prompt_submit_as_resolve() {
        let payload = br#"{"session_id":"s1","hook_event_name":"UserPromptSubmit"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), HookEvent::Resolve)));
    }

    #[test]
    fn pre_tool_use_ask_user_question_is_notify() {
        let payload = br#"{"session_id":"s1","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), HookEvent::Notify)));
    }

    #[test]
    fn post_tool_use_ask_user_question_is_resolve() {
        let payload = br#"{"session_id":"s1","hook_event_name":"PostToolUse","tool_name":"AskUserQuestion"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), HookEvent::Resolve)));
    }

    #[test]
    fn pre_tool_use_for_other_tools_is_ignored() {
        let payload = br#"{"session_id":"s1","hook_event_name":"PreToolUse","tool_name":"Bash"}"#;
        assert_eq!(parse_hook_event(payload), None);
    }

    #[test]
    fn post_tool_use_for_other_tools_is_ignored() {
        let payload = br#"{"session_id":"s1","hook_event_name":"PostToolUse","tool_name":"Bash"}"#;
        assert_eq!(parse_hook_event(payload), None);
    }

    #[test]
    fn unrecognized_hook_event_name_is_ignored() {
        let payload = br#"{"session_id":"s1","hook_event_name":"SessionStart"}"#;
        assert_eq!(parse_hook_event(payload), None);
    }

    #[test]
    fn malformed_json_is_ignored_not_a_panic() {
        assert_eq!(parse_hook_event(b"not json"), None);
        assert_eq!(parse_hook_event(b""), None);
        assert_eq!(parse_hook_event(br#"{"hook_event_name":"Stop"}"#), None); // missing session_id
    }

    #[test]
    fn derives_from_the_real_ctl_socket_naming_convention() {
        // Pins the literal prefix duplicated from
        // `isekai-ssh::ctl_forward::REMOTE_SOCK_PREFIX` (this crate can't
        // import a sibling crate's `pub(crate)` item) — if that convention
        // ever changes, this test and `isekai-ssh`'s own
        // `forward_option_args` test (which asserts
        // `forward.remote_path.starts_with(REMOTE_SOCK_PREFIX)`) must be
        // updated together.
        let ctl_sock = Path::new("/tmp/isekai-pipe-ctl-aaaa1111bbbb2222.sock");
        let derived = derive_daemon_sock_path(ctl_sock).unwrap();
        assert_eq!(derived, Path::new("/tmp/isekai-pipe-ctl-hookd-aaaa1111bbbb2222.sock"));
    }

    #[test]
    fn derive_daemon_sock_path_rejects_paths_outside_the_convention() {
        assert!(derive_daemon_sock_path(Path::new("/tmp/something-else.sock")).is_none());
        assert!(derive_daemon_sock_path(Path::new("/tmp/isekai-pipe-ctl-aaaa.txt")).is_none());
    }
}
