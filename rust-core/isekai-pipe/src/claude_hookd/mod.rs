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

/// Fallback colors used when `#@isekai tab-idle-color`/`tab-attention-color`
/// was never set. Shared between the daemon (`daemon::run`'s `DaemonConfig`
/// defaults) and this module's own [`event_command`] fallback below — both
/// need the same idle color to paint the same thing regardless of which one
/// ends up doing the painting.
#[cfg(unix)]
pub(crate) const DEFAULT_IDLE_COLOR: (u8, u8, u8) = (0x20, 0x20, 0x20);
#[cfg(unix)]
pub(crate) const DEFAULT_ATTENTION_COLOR: (u8, u8, u8) = (0xff, 0x88, 0x00);

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
    let Some((session_id, event, is_session_end)) = parse_hook_event(&payload) else {
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

    // Only `Notify` lazily spawns a daemon: if none is running there is by
    // definition no attention state for a *daemon-tracked* `Resolve` to
    // clear (Opus design consult, 2026-07-25). This matters more than it
    // looks — dropping `PostToolUse`'s matcher means it now fires on *every*
    // tool call, so paying `SPAWN_RETRY_DELAYS_MS`'s ~1.55s worst case on
    // each one whenever no daemon happens to be running would add real,
    // visible latency to ordinary tool use. It also protects `SessionEnd`,
    // whose hooks have a documented 1.5s default timeout that ladder alone
    // could exceed.
    if event != HookEvent::Notify {
        // `SessionEnd` alone still gets one direct, best-effort, idempotent
        // `SetTabColor(idle)` — bypassing the daemon/state machine entirely
        // — when no daemon answers. Without this, "the daemon happened to be
        // dead (crashed; the routine 1h idle-exit can't cause this, since
        // any regular Resolve traffic keeps resetting its own idle-exit
        // deadline) right as the very last event for this tab arrives"
        // strands the tab in the attention color forever: a dead `Resolve`
        // no longer spawns a daemon that could self-heal on startup, and
        // once the session is gone there's no more traffic to ever trigger
        // a repaint. Deliberately scoped to just `SessionEnd`, not every
        // `Resolve` — most `Resolve`s are unconditional `PostToolUse`/
        // `PostToolUseFailure` firing on a tab that was never Attention in
        // the first place (no daemon ever needed to exist), and sending a
        // redundant idle-color write to the real ctl socket on literally
        // every tool call would be constant, mostly-pointless overhead for
        // a benefit that only matters in this one specific, rarer case.
        if is_session_end {
            let (r, g, b) = idle_color.unwrap_or(DEFAULT_IDLE_COLOR);
            let _ = crate::ctl::send_ctl_message(&ctl_sock_path, isekai_protocol::CtlMessage::SetTabColor { r, g, b }).await;
        }
        return ExitCode::SUCCESS;
    }

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

/// Extracts just the fields `claude-hookd` needs from Claude Code's hook JSON
/// payload and maps them to this module's own [`HookEvent`] — the one place
/// in this feature that knows Claude Code's hook schema, deliberately kept
/// separate from the daemon's own minimal wire format
/// (`daemon::read_one_event`) so a future Claude Code schema change touches
/// only this function. `None` covers both "malformed JSON" and "an event
/// type `claude-hookd` doesn't act on" identically — both are silent no-ops.
/// The third element is `true` only for `SessionEnd` — `event_command`'s only
/// use for it is deciding whether a `Resolve` that can't reach any daemon
/// still deserves a direct, one-off self-heal `SetTabColor` (see its call
/// site); every other `Resolve` reaching a dead/absent daemon is left alone.
///
/// This mapping was worked out with an Opus design consult (2026-07-25) that
/// fetched the raw `code.claude.com/docs/en/hooks.md` directly (the rendered/
/// summarized page silently drops the `Stop` fields below — verify against
/// the `.md` URL, not the HTML one, if this ever needs re-checking) rather
/// than trusting either of two third-party design docs that had disagreed
/// with each other on some of these fields:
/// - `PermissionRequest` is wired *in addition to* `Notification
///   (notification_type=permission_prompt)`, not instead of it — reversing
///   the original design-consult recommendation to skip it (2026-07-25,
///   found while checking two real, independently-maintained tmux plugins
///   that solve the same problem: `accessd/tmux-agent-indicator`, 81 stars,
///   uses `PermissionRequest` as its *primary* signal; `sandudorogan/
///   tmux-pane-tree` registers both, matcher-less). The original theoretical
///   concern (it fires *before* the dialog, so another `PermissionRequest`
///   hook could auto-allow it and this one would still fire) turns out to
///   cost little in this design specifically: `PostToolUse`'s unconditional
///   `Resolve` (below) clears any such early-and-wrong `Notify` as soon as
///   the auto-allowed tool actually runs, so the worst case is a
///   self-correcting, momentary flash rather than a stuck false positive.
///   Firing both is harmless — a second `Notify` for an already-`Attention`
///   session is just a debounce refresh, not a duplicate popup.
/// - `StopFailure` fires *instead of* `Stop` on an API error — without it, an
///   errored turn silently leaves the tab its idle color.
/// - `Stop`'s hook input includes `background_tasks`/`session_crons`/
///   `stop_hook_active` (Claude Code v2.1.145+; absent on older builds, which
///   this function treats as "no background work" — falling back to
///   unconditional `Notify`, today's pre-existing behavior). Only
///   `background_tasks` is checked: a non-empty array means the session is
///   *paused waiting for its own background work*, not actually done, so
///   `Stop` is skipped (the eventual wake-up produces a fresh `Stop` with an
///   empty array, or a `Notification(idle_prompt)` backstop, so the signal
///   isn't lost). `session_crons` is deliberately ignored — a `/loop`/
///   `ScheduleWakeup` entry can sit there for hours while the user genuinely
///   is needed for something else. `stop_hook_active` is also deliberately
///   ignored and no `Stop` variant maps to `Resolve`: at `Stop`-hook time
///   there's no way to know whether another hook is about to block, and
///   `stop_hook_active: true` is also what a stop-hook loop's final,
///   genuinely-terminal `Stop` looks like — resolving on it risks ending a
///   just-finished turn with the idle color, which costs the user more than
///   a transient extra orange during a `/goal`-style loop does.
/// - `PostToolUse`'s `AskUserQuestion` matcher is dropped: it now fires (and
///   resolves) for every tool, since a `Resolve` on an unrelated/already-Idle
///   session is a tested no-op, and this is also what closes out a
///   `permission_prompt` Notify once the approved tool actually runs (a bare
///   `AskUserQuestion` matcher never covered that case at all).
///   `PostToolUseFailure` gets the same treatment (a permission-approved tool
///   that then fails must still close out the attention it caused).
/// - The `agent_id` guard on `PostToolUse`/`PostToolUseFailure` is required,
///   not optional, once the matcher is dropped: subagents share the parent's
///   `session_id` and (Claude Code v2.1.198+) run in the background by
///   default, so their tool-completion events keep arriving *after* the main
///   turn's `Stop` — without this guard they would immediately clear the
///   attention color `Stop` just set.
/// - `SessionEnd` maps to `Resolve`: nothing can still be waiting on the user
///   once the session itself is gone (covers "user quits Claude Code while
///   the tab is still orange", the one gap the 10-minute attention timeout
///   and the daemon's own startup self-heal don't already close).
#[cfg(unix)]
fn parse_hook_event(payload: &[u8]) -> Option<(String, HookEvent, bool)> {
    let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let session_id = value.get("session_id")?.as_str()?.to_string();
    let hook_event_name = value.get("hook_event_name")?.as_str()?;
    let tool_name = value.get("tool_name").and_then(|v| v.as_str());
    // Present only inside a subagent's own hook calls (see the doc comment's
    // note on the `PostToolUse`/`PostToolUseFailure` guard above).
    let in_subagent = value.get("agent_id").is_some();
    let background_work_pending = value
        .get("background_tasks")
        .and_then(|v| v.as_array())
        .is_some_and(|tasks| !tasks.is_empty());

    let event = match hook_event_name {
        "Notification" => match value.get("notification_type").and_then(|v| v.as_str()) {
            Some("permission_prompt" | "idle_prompt" | "elicitation_dialog" | "agent_needs_input" | "agent_completed") => {
                HookEvent::Notify
            }
            // The MCP elicitation dialog was answered or dismissed.
            Some("elicitation_complete" | "elicitation_response") => HookEvent::Resolve,
            // `auth_success` never means "you're needed"; unrecognized/future
            // sub-types are ignored rather than guessed into a false orange.
            Some(_) => return None,
            // No `notification_type` at all (older Claude Code build): keep
            // the pre-filtering catch-all behavior rather than silently
            // dropping every Notification.
            None => HookEvent::Notify,
        },
        "Stop" if !background_work_pending => HookEvent::Notify,
        "StopFailure" => HookEvent::Notify,
        "PermissionRequest" => HookEvent::Notify,
        "PreToolUse" if tool_name == Some("AskUserQuestion") => HookEvent::Notify,
        "UserPromptSubmit" => HookEvent::Resolve,
        "PostToolUse" | "PostToolUseFailure" if !in_subagent => HookEvent::Resolve,
        "SessionEnd" => HookEvent::Resolve,
        _ => return None,
    };
    Some((session_id, event, hook_event_name == "SessionEnd"))
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
    fn bare_notification_with_no_notification_type_falls_back_to_notify() {
        // Older Claude Code builds that predate the `notification_type`
        // field entirely — keep the pre-filtering catch-all behavior rather
        // than silently dropping every Notification.
        let payload = br#"{"session_id":"s1","hook_event_name":"Notification"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), HookEvent::Notify, false)));
    }

    #[test]
    fn notification_types_that_mean_needs_input_are_notify() {
        for kind in ["permission_prompt", "idle_prompt", "elicitation_dialog", "agent_needs_input", "agent_completed"] {
            let payload = format!(r#"{{"session_id":"s1","hook_event_name":"Notification","notification_type":"{kind}"}}"#);
            assert_eq!(
                parse_hook_event(payload.as_bytes()),
                Some(("s1".to_string(), HookEvent::Notify, false)),
                "notification_type {kind:?} should be Notify"
            );
        }
    }

    #[test]
    fn notification_types_that_close_an_elicitation_are_resolve() {
        for kind in ["elicitation_complete", "elicitation_response"] {
            let payload = format!(r#"{{"session_id":"s1","hook_event_name":"Notification","notification_type":"{kind}"}}"#);
            assert_eq!(parse_hook_event(payload.as_bytes()), Some(("s1".to_string(), HookEvent::Resolve, false)));
        }
    }

    #[test]
    fn auth_success_notification_is_ignored() {
        let payload = br#"{"session_id":"s1","hook_event_name":"Notification","notification_type":"auth_success"}"#;
        assert_eq!(parse_hook_event(payload), None);
    }

    #[test]
    fn stop_without_pending_background_work_is_notify() {
        let payload = br#"{"session_id":"s1","hook_event_name":"Stop","background_tasks":[]}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), HookEvent::Notify, false)));
        // Also true for older Claude Code builds where the field is absent
        // entirely (Claude Code v2.1.145+ only).
        let payload = br#"{"session_id":"s1","hook_event_name":"Stop"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), HookEvent::Notify, false)));
    }

    #[test]
    fn stop_with_pending_background_work_is_ignored() {
        // The session is paused waiting for its own background work to wake
        // it back up, not actually done — the eventual wake-up produces a
        // fresh Stop (empty array) or a Notification(idle_prompt) backstop.
        let payload = br#"{"session_id":"s1","hook_event_name":"Stop","background_tasks":[{"id":"t1","status":"running"}]}"#;
        assert_eq!(parse_hook_event(payload), None);
    }

    #[test]
    fn stop_failure_is_notify() {
        let payload = br#"{"session_id":"s1","hook_event_name":"StopFailure"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), HookEvent::Notify, false)));
    }

    #[test]
    fn permission_request_is_notify() {
        // Wired alongside (not instead of) Notification's permission_prompt —
        // see the doc comment above parse_hook_event for why both are used.
        let payload = br#"{"session_id":"s1","hook_event_name":"PermissionRequest","tool_name":"Bash"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), HookEvent::Notify, false)));
    }

    #[test]
    fn parses_user_prompt_submit_as_resolve() {
        let payload = br#"{"session_id":"s1","hook_event_name":"UserPromptSubmit"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), HookEvent::Resolve, false)));
    }

    #[test]
    fn session_end_is_resolve_and_flagged_for_the_self_heal_fallback() {
        let payload = br#"{"session_id":"s1","hook_event_name":"SessionEnd"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), HookEvent::Resolve, true)));
    }

    #[test]
    fn pre_tool_use_ask_user_question_is_notify() {
        let payload = br#"{"session_id":"s1","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), HookEvent::Notify, false)));
    }

    #[test]
    fn pre_tool_use_for_other_tools_is_ignored() {
        let payload = br#"{"session_id":"s1","hook_event_name":"PreToolUse","tool_name":"Bash"}"#;
        assert_eq!(parse_hook_event(payload), None);
    }

    #[test]
    fn post_tool_use_resolves_regardless_of_tool_name() {
        // The `AskUserQuestion` matcher was deliberately dropped: `Resolve`
        // on an unrelated/already-Idle session is a tested no-op, and this
        // is what closes out a `permission_prompt` Notify once the approved
        // tool actually runs.
        for name in ["PostToolUse", "PostToolUseFailure"] {
            let payload = format!(r#"{{"session_id":"s1","hook_event_name":"{name}","tool_name":"Bash"}}"#);
            assert_eq!(parse_hook_event(payload.as_bytes()), Some(("s1".to_string(), HookEvent::Resolve, false)));
        }
    }

    #[test]
    fn subagent_post_tool_use_is_ignored() {
        // A subagent's own tool completions (shares the parent session_id,
        // runs in the background by default since v2.1.198) must not clear
        // attention the main turn's Stop just raised.
        let payload = br#"{"session_id":"s1","hook_event_name":"PostToolUse","tool_name":"Bash","agent_id":"sub1"}"#;
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
