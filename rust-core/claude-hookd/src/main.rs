//! `claude-hookd`: a small per-tab daemon that turns Claude Code hook events
//! into a persistent, debounced tab-color/notification indicator.
//!
//! Independent of the isekai-terminal ecosystem (extracted 2026-08 from
//! `isekai-pipe claude-hookd`, which remains removed — see git history if
//! you need the old in-tree version). Works over three delivery mechanisms,
//! auto-detected (`delivery::Delivery::resolve`) or forced via
//! `$CLAUDE_HOOKD_DELIVERY`:
//! - a bare SSH+tmux session with `allow-passthrough on` (writes OSC
//!   directly to this pane's tty, wrapped in tmux's passthrough DCS),
//! - a bare SSH session with no tmux at all (writes raw OSC to `$SSH_TTY`),
//! - isekai-terminal's ctl-socket forward (`$ISEKAI_CTL_SOCK`, if present —
//!   the pre-existing mechanism this crate was split out of `isekai-pipe`
//!   from; kept working unchanged for isekai-terminal users).
//!
//! Split into [`state`] (the actual decision logic: an I/O-free, unit-tested
//! pure function — "state and decision logic belong in one place" applied
//! at the scale of this one small feature), [`delivery`] (how an OSC/
//! `CtlMessage` actually reaches the real terminal), [`daemon`] (the async
//! loop and `__serve` CLI), and this module (the CLI entry point: Claude
//! Code hook JSON parsing, per-tab daemon identification/lazy-spawn, and the
//! bounded retry that tolerates the spawn race).
//!
//! Unix-only (`UnixListener`/`UnixStream`) — the daemon runs on the *remote*
//! host (or wherever the interactive shell + tmux/ssh session lives), which
//! for this feature's entire premise (Claude Code hooks in a terminal) is
//! realistically always Unix. On other platforms every subcommand is a
//! silent, immediate no-op.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

#[cfg(unix)]
mod daemon;
#[cfg(unix)]
mod delivery;
mod state;

#[cfg(unix)]
use delivery::Delivery;

/// Bounded retry/backoff for the "daemon wasn't running yet, spawned one,
/// now waiting for it to bind" window. A single retry loses the race when
/// two hooks fire near-simultaneously and both spawn (the loser's *client*
/// would give up before the winner's daemon finishes binding) — this is
/// generous enough that even a slow process start under load has several
/// chances, while the whole sequence still finishes in ~1.5s worst case so
/// a Claude Code hook (which this must never visibly block) doesn't stall.
const SPAWN_RETRY_DELAYS_MS: [u64; 5] = [50, 100, 200, 400, 800];

/// Fallback colors used when `$ISEKAI_TAB_IDLE_COLOR`/`$ISEKAI_TAB_ATTENTION_COLOR`/
/// `$ISEKAI_TAB_WAITING_COLOR` were never set. The first two env var names are
/// unchanged from before this crate's split from `isekai-pipe` — kept for
/// backward compat with `isekai-ssh`'s `#@isekai tab-idle-color`/
/// `tab-attention-color` directives (which inject exactly these names into
/// the remote session), but reading them creates no code dependency on
/// isekai-ssh: anyone can set these env vars directly regardless of how the
/// session was started. `ISEKAI_TAB_WAITING_COLOR` is new (2026-08, alongside
/// `state::Aggregate::Waiting`) and has no such legacy directive yet.
pub(crate) const DEFAULT_IDLE_COLOR: (u8, u8, u8) = (0x20, 0x20, 0x20);
pub(crate) const DEFAULT_ATTENTION_COLOR: (u8, u8, u8) = (0xff, 0x88, 0x00);
/// A dim blue — visually distinct from both the idle gray and the attention
/// orange, and calm on purpose: this color means "something's happening,
/// probably nothing you need to do," not "come look now" (compare
/// `DEFAULT_ATTENTION_COLOR`, which also gets a popup; this one never does).
pub(crate) const DEFAULT_WAITING_COLOR: (u8, u8, u8) = (0x00, 0x60, 0xa0);

/// Prefix for this crate's own daemon sockets under `/tmp` (unrelated to
/// `isekai-pipe-ctl-*` now that this crate no longer depends on isekai-pipe
/// at all — see [`derive_daemon_sock_path`]).
const DAEMON_SOCK_PREFIX: &str = "claude-hookd-";

#[cfg(unix)]
#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("event") => event_command().await,
        // Undocumented on purpose: spawned only by `spawn_detached_daemon`
        // below, never meant to be typed by a human.
        Some("__serve") => daemon::serve_command(args).await,
        Some(other) => {
            eprintln!("claude-hookd: unknown subcommand {other:?} (expected \"event\" or nothing)");
            // Still exit 0: a `PreToolUse`/`PostToolUse` hook must never be
            // visibly disrupted by a misconfigured invocation of this
            // cosmetic feature — see this crate's module docs.
            ExitCode::SUCCESS
        }
    }
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    ExitCode::SUCCESS
}

/// Reads Claude Code's hook JSON payload from stdin, decides whether it
/// means anything to `claude-hookd` at all, and if so relays it to this
/// tab's daemon (lazily spawning one if none is running yet). Always
/// returns `ExitCode::SUCCESS` — see `main`'s doc comment on why a non-zero
/// exit or any stdout output here is unacceptable for a `PreToolUse`/
/// `PostToolUse` hook.
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
    // No usable delivery mechanism in this environment (no tmux, no
    // isekai-terminal ctl-socket, no bare SSH tty either — e.g. a local,
    // non-SSH terminal) is not an error, just nothing for `claude-hookd` to
    // do.
    let Some(delivery) = Delivery::resolve() else {
        return ExitCode::SUCCESS;
    };
    let daemon_sock_path = derive_daemon_sock_path(&delivery);

    // Same lazy sweep every invocation performs — reused here rather than
    // duplicated so a stale socket left by a crashed daemon (or this tab's
    // daemon having already hit its 1h self-exit) doesn't cause a spurious
    // connect failure/spawn every time.
    sweep_stale_daemon_sockets();

    if send_event(&daemon_sock_path, &session_id, event).await {
        return ExitCode::SUCCESS;
    }

    let idle_color = std::env::var("ISEKAI_TAB_IDLE_COLOR").ok().and_then(|v| osc_color::parse_hex_color(&v).ok());

    // Only `Notify`/`StopDeferred` lazily spawn a daemon: if none is running
    // there is by definition no pending state for a *daemon-tracked*
    // `Resolve` to clear. This matters more than it looks — dropping
    // `PostToolUse`'s matcher means it now fires on *every* tool call, so
    // paying `SPAWN_RETRY_DELAYS_MS`'s ~1.55s worst case on each one
    // whenever no daemon happens to be running would add real, visible
    // latency to ordinary tool use. It also protects `SessionEnd`, whose
    // hooks have a documented 1.5s default timeout that ladder alone could
    // exceed. `StopDeferred` must spawn just like `Notify` — otherwise a
    // `StopDeferred` arriving with no daemon running records nothing at all,
    // and the eventual self-correcting promotion to `Attention` (see
    // `state.rs::apply_timeout`) would silently never happen for that
    // session. `StopAmbiguousTeammate` spawns for the same reason *and*
    // stays safe when it does: a freshly spawned daemon has no
    // `teammate_dispatch` history yet (see `daemon.rs::run`), so it
    // resolves to the safe `Notify` default exactly as if no daemon existed
    // at all — spawning one here can never manufacture a false
    // `StopDeferred` out of nothing. `TeammateDispatched` deliberately does
    // *not* spawn, same as plain `Resolve`: with no daemon running there is
    // no pending state to update and no `Stop` yet to inform, so recording
    // the dispatch timestamp would have nothing to do.
    if !matches!(event, ClientEvent::Notify | ClientEvent::StopDeferred | ClientEvent::StopAmbiguousTeammate) {
        // `SessionEnd` alone still gets one direct, best-effort, idempotent
        // idle-color write — bypassing the daemon/state machine entirely —
        // when no daemon answers. Without this, "the daemon happened to be
        // dead right as the very last event for this tab arrives" strands
        // the tab in the attention color forever: a dead `Resolve` no
        // longer spawns a daemon that could self-heal on startup, and once
        // the session is gone there's no more traffic to ever trigger a
        // repaint. Deliberately scoped to just `SessionEnd`, not every
        // `Resolve`.
        if is_session_end {
            let (r, g, b) = idle_color.unwrap_or(DEFAULT_IDLE_COLOR);
            delivery::send_tab_color(&delivery, (r, g, b)).await;
        }
        return ExitCode::SUCCESS;
    }

    let attention_color = std::env::var("ISEKAI_TAB_ATTENTION_COLOR").ok().and_then(|v| osc_color::parse_hex_color(&v).ok());
    let waiting_color = std::env::var("ISEKAI_TAB_WAITING_COLOR").ok().and_then(|v| osc_color::parse_hex_color(&v).ok());
    spawn_detached_daemon(&daemon_sock_path, &delivery, idle_color, attention_color, waiting_color);

    for delay_ms in SPAWN_RETRY_DELAYS_MS {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        if send_event(&daemon_sock_path, &session_id, event).await {
            break;
        }
    }
    ExitCode::SUCCESS
}

/// A hook event as classified purely from Claude Code's own hook JSON — the
/// output of [`parse_hook_event`], before any daemon-side state is
/// consulted. A strict superset of `state::HookEvent` (the pure decision
/// core's own input): `TeammateDispatched`/`StopAmbiguousTeammate` cannot be
/// resolved into a final `state::HookEvent` here, because doing so needs
/// this tab's *history* (has this session recently dispatched work to a
/// teammate?), and history is daemon-held state that a one-shot,
/// short-lived `claude-hookd event` process never has — see `daemon.rs`'s
/// `WireEvent`/`teammate_dispatch` for where that translation actually
/// happens. Keeping these two variants out of `state::HookEvent` keeps
/// `state.rs` exactly what its module docs promise: a pure, I/O-free
/// function that needs nothing beyond `(state, event, now)`.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientEvent {
    Notify,
    StopDeferred,
    Resolve,
    /// `PostToolUse` (success only, not `PostToolUseFailure`) for a
    /// top-level (`!in_subagent`) `Agent` or `SendMessage` call — this
    /// session just handed work to some teammate. Treated identically to
    /// `Resolve` for clearing this tab's pending state (it's real activity,
    /// same as any other successful tool call), but carries distinct wire
    /// identity so `daemon.rs` can *also* record it in `teammate_dispatch`.
    TeammateDispatched,
    /// A `Stop` whose only relevant-looking `background_tasks` entries are
    /// `type: "teammate"` — ambiguous in a way plain `HookEvent::StopDeferred`
    /// isn't: an idle teammate's `status` never stops reading `"running"`
    /// (confirmed live 2026-08-05 — see this crate's handoff notes), so
    /// "a teammate entry exists" alone proves nothing. Resolved daemon-side
    /// by checking `teammate_dispatch` for a *recent* dispatch to this
    /// session (see `daemon.rs::run`'s translation) — recent dispatch +
    /// teammate entry ⇒ plausibly still working (`StopDeferred`); no recent
    /// dispatch ⇒ probably just a long-idle teammate (`Notify`, the safe
    /// default).
    StopAmbiguousTeammate,
}

/// Extracts just the fields `claude-hookd` needs from Claude Code's hook JSON
/// payload and maps them to this module's own [`ClientEvent`] — the one
/// place in this crate that knows Claude Code's hook schema, deliberately
/// kept separate from the daemon's own minimal wire format
/// (`daemon::read_one_event`) so a future Claude Code schema change touches
/// only this function. `None` covers both "malformed JSON" and "an event
/// type `claude-hookd` doesn't act on" identically — both are silent no-ops.
/// The third element is `true` only for `SessionEnd` — `event_command`'s only
/// use for it is deciding whether a `Resolve` that can't reach any daemon
/// still deserves a direct, one-off self-heal tab-color write (see its call
/// site); every other `Resolve` reaching a dead/absent daemon is left alone.
///
/// A `Stop` means "waiting on a background task/agent that will auto-resume
/// me" (`ClientEvent::StopDeferred`, not "blocked on a human"
/// (`ClientEvent::Notify`)) when its `background_tasks` array (present on the
/// real `Stop` hook payload — confirmed 2026-08-03 by reading the zod
/// schemas embedded in the installed Claude Code CLI binary, then verified
/// live by dumping a real `Stop` payload from a `Bash(run_in_background)`
/// session; **not** documented in the public hooks reference, which is
/// incomplete for this payload — see `claude-hookd-background-tasks-design`
/// in the user's memory system for the full trail) contains an entry whose
/// `type` is `"shell"` or `"subagent"` — the two mechanisms this crate
/// actually means to detect unconditionally (`Bash` with `run_in_background`,
/// and a backgrounded `Agent`/subagent). Deliberately narrower than "array
/// non-empty": the array can also contain `"teammate"`/`"dream"`/
/// `"auto-mode scan"`/`"monitor"`/etc. entries that can sit `running`
/// indefinitely (e.g. an idle teammate under `CLAUDE_CODE_TEAMMATE_MODE`),
/// which an earlier, reverted attempt at this same fix (2026-08-03, believed
/// at the time to be checking a nonexistent field — it wasn't) learned the
/// hard way not to trust wholesale. A `"teammate"`-only array no longer
/// falls straight through to `Notify` either, as of 2026-08 — see
/// `ClientEvent::StopAmbiguousTeammate`'s docs; every *other* unrecognized/
/// future `type` still fails to the safe (`Notify`) side, by construction
/// (`matches!` against an explicit allowlist, not a denylist).
///
/// The same `StopDeferred` treatment also applies when `session_crons`
/// (present on the same `Stop` payload, alongside `background_tasks` —
/// confirmed live 2026-08-04 by a temporary `Stop` hook dumping stdin to a
/// file, same method as `background_tasks`'s own discovery; also
/// undocumented in the public hooks reference) is non-empty: each entry is
/// a scheduled self-resume this exact session registered for itself (e.g.
/// the `ScheduleWakeup` tool, or a `/loop`-style recurring one —
/// `recurring: true`/`false` doesn't change the classification, since
/// either kind still means the session comes back on its own). Unlike
/// `background_tasks`, no type-filtering is needed here: every
/// `session_crons` entry unambiguously means Claude Code's own scheduling
/// infrastructure — not a heuristic on this crate's part — already
/// committed to resuming this session, which is if anything a *stronger*
/// self-resolving signal than a `background_tasks` entry. This was the
/// actual root cause of a real false-Attention report (2026-08-04, user
/// noticed a tab going orange while a `ScheduleWakeup`-driven wait was
/// still in flight): `parse_hook_event` checked `background_tasks` only,
/// so a `Stop` with a pending `session_crons` entry but an empty
/// `background_tasks` array fell through to unconditional `Notify`.
///
/// `StopDeferred` is not a permanent suppression — see `state.rs`'s
/// `Pending::Deferred`/`apply_timeout` for the bounded self-correction that
/// makes this safe even when the guess is wrong (an even earlier attempt at
/// this fix, having Claude self-report a marker string when *it* judged a
/// `Stop` ambiguous, was reverted the same day after producing false
/// negatives with no such backstop — self-report alone isn't trustworthy for
/// this, see `claude-hookd-self-report-marker-failed` in memory). Note this
/// bound (`MAX_DEFERRAL`, 30 minutes) is shorter than `ScheduleWakeup`'s own
/// maximum delay (1 hour) — a `session_crons` wait can in principle still
/// get promoted to `Attention` before its own scheduled time arrives. Left
/// as-is rather than special-cased: `state.rs`'s self-correction is
/// deliberately the same safety net regardless of *why* a `Stop` was
/// deferred, and a late, spurious color flip is a smaller cost than adding
/// a second deadline-tracking scheme.
///
/// See the pre-split `isekai-pipe` version's doc comment (git history) for
/// the detailed rationale behind the other mappings below — carried over
/// unchanged, this is purely a file move.
#[cfg(unix)]
fn parse_hook_event(payload: &[u8]) -> Option<(String, ClientEvent, bool)> {
    let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let session_id = value.get("session_id")?.as_str()?.to_string();
    let hook_event_name = value.get("hook_event_name")?.as_str()?;
    let tool_name = value.get("tool_name").and_then(|v| v.as_str());
    let in_subagent = value.get("agent_id").is_some();
    let background_task_types: Vec<&str> = value
        .get("background_tasks")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("type").and_then(|v| v.as_str()))
        .collect();
    let has_relevant_background_work = background_task_types.iter().any(|t| matches!(*t, "shell" | "subagent"));
    let has_teammate_background_work = background_task_types.contains(&"teammate");
    let has_pending_session_cron =
        value.get("session_crons").and_then(|v| v.as_array()).is_some_and(|crons| !crons.is_empty());

    let event = match hook_event_name {
        "Notification" => match value.get("notification_type").and_then(|v| v.as_str()) {
            Some("permission_prompt" | "idle_prompt" | "elicitation_dialog" | "agent_needs_input" | "agent_completed") => {
                ClientEvent::Notify
            }
            Some("elicitation_complete" | "elicitation_response") => ClientEvent::Resolve,
            Some(_) => return None,
            None => ClientEvent::Notify,
        },
        "Stop" if has_relevant_background_work || has_pending_session_cron => ClientEvent::StopDeferred,
        "Stop" if has_teammate_background_work => ClientEvent::StopAmbiguousTeammate,
        "Stop" => ClientEvent::Notify,
        "StopFailure" => ClientEvent::Notify,
        "PermissionRequest" => ClientEvent::Notify,
        "PreToolUse" if tool_name == Some("AskUserQuestion") => ClientEvent::Notify,
        "UserPromptSubmit" => ClientEvent::Resolve,
        "PostToolUse" if !in_subagent && matches!(tool_name, Some("Agent" | "SendMessage")) => ClientEvent::TeammateDispatched,
        "PostToolUse" | "PostToolUseFailure" if !in_subagent => ClientEvent::Resolve,
        "SessionEnd" => ClientEvent::Resolve,
        _ => return None,
    };
    Some((session_id, event, hook_event_name == "SessionEnd"))
}

/// Derives this delivery target's daemon socket path deterministically from
/// [`Delivery::identity`] (a stable string: the ctl-socket path, the tmux
/// session id, or the direct tty device path) via a non-cryptographic hash —
/// short and filename-safe regardless of how long/unusual the underlying
/// path is, while still mapping the same target to the same daemon every
/// time so repeated hook events for the same tab (every pane of one tmux
/// session, for `TmuxSession`) reuse one daemon rather than spawning a new
/// one per event.
#[cfg(unix)]
fn derive_daemon_sock_path(delivery: &Delivery) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    delivery.identity().hash(&mut hasher);
    PathBuf::from(format!("/tmp/{DAEMON_SOCK_PREFIX}{:016x}.sock", hasher.finish()))
}

#[cfg(unix)]
fn sweep_stale_daemon_sockets() {
    let _ = ctl_gc::sweep_stale_sockets(Path::new("/tmp"), DAEMON_SOCK_PREFIX, Duration::from_secs(60 * 60));
}

/// Connects to the daemon socket and sends one line, this module's own
/// minimal wire format (see `daemon::read_one_event`). Returns whether the
/// connect+write succeeded — a proxy for "a daemon is reachable and got
/// this", not a guarantee it was applied (fire-and-forget).
#[cfg(unix)]
async fn send_event(sock_path: &Path, session_id: &str, event: ClientEvent) -> bool {
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::UnixStream;

    let Ok(mut stream) = UnixStream::connect(sock_path).await else {
        return false;
    };
    let event_name = match event {
        ClientEvent::Notify => "notify",
        ClientEvent::StopDeferred => "stop_deferred",
        ClientEvent::Resolve => "resolve",
        ClientEvent::TeammateDispatched => "teammate_dispatched",
        ClientEvent::StopAmbiguousTeammate => "stop_ambiguous_teammate",
    };
    let Ok(session_id_json) = serde_json::to_string(session_id) else {
        return false;
    };
    let line = format!("{{\"session_id\":{session_id_json},\"event\":\"{event_name}\"}}\n");
    stream.write_all(line.as_bytes()).await.is_ok()
}

/// Spawns `claude-hookd __serve` fully detached: `setsid` so it survives
/// this short-lived hook process exiting and never receives its controlling
/// terminal's signals, and all three standard streams redirected to
/// `/dev/null` so it can never hold this hook's stdout pipe open — Claude
/// Code reads that pipe to EOF, so an inherited stdout would block Claude
/// Code itself for as long as the daemon runs, not merely the hook.
/// Delivery/colors are passed explicitly as spawn arguments rather than
/// left to env var inheritance, so the daemon's configuration doesn't
/// depend on which of several near-simultaneous hook processes happened to
/// win the spawn race.
#[cfg(unix)]
fn spawn_detached_daemon(
    daemon_sock_path: &Path,
    delivery: &Delivery,
    idle_color: Option<(u8, u8, u8)>,
    attention_color: Option<(u8, u8, u8)>,
    waiting_color: Option<(u8, u8, u8)>,
) {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("claude-hookd"));
    let mut cmd = Command::new(exe);
    cmd.arg("__serve")
        .arg("--sock")
        .arg(daemon_sock_path)
        .arg("--delivery-spec")
        .arg(delivery.to_spec());
    if let Some(color) = idle_color {
        cmd.arg("--idle-color").arg(osc_color::format_hex_color(color));
    }
    if let Some(color) = attention_color {
        cmd.arg("--attention-color").arg(osc_color::format_hex_color(color));
    }
    if let Some(color) = waiting_color {
        cmd.arg("--waiting-color").arg(osc_color::format_hex_color(color));
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // Safety: the closure only calls `setsid(2)`, an async-signal-safe libc
    // call that touches no Rust-managed state — the standard justification
    // for a `pre_exec` closure.
    unsafe {
        cmd.pre_exec(|| if raw_setsid() < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
    }
    // Best-effort: if spawning fails, this one hook event is silently
    // dropped rather than surfaced — the next hook invocation gets another
    // chance to lazily spawn.
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

/// Lazy garbage collection for this crate's own daemon sockets under `/tmp`.
/// Duplicated (not depending on `isekai-pipe-core::ctl_gc`, which pulls in
/// `isekai-transport` — see crate-level docs on staying independent) —
/// small and self-contained enough that this is the simpler trade-off.
mod ctl_gc {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    pub(crate) fn sweep_stale_sockets(dir: &Path, prefix: &str, staleness_threshold: Duration) -> io::Result<Vec<PathBuf>> {
        let mut removed = Vec::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(removed),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with(prefix) || !name.ends_with(".sock") {
                continue;
            }
            if is_abandoned(&path) || is_stale_by_mtime(&path, staleness_threshold) {
                if std::fs::remove_file(&path).is_ok() {
                    removed.push(path);
                }
            }
        }
        Ok(removed)
    }

    #[cfg(unix)]
    fn is_abandoned(path: &Path) -> bool {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => false,
            Err(e) => e.kind() == io::ErrorKind::ConnectionRefused,
        }
    }

    #[cfg(not(unix))]
    fn is_abandoned(_path: &Path) -> bool {
        false
    }

    fn is_stale_by_mtime(path: &Path, threshold: Duration) -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        let Ok(modified) = meta.modified() else {
            return false;
        };
        modified.elapsed().map(|elapsed| elapsed > threshold).unwrap_or(false)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn missing_directory_is_not_an_error() {
            let dir = tempfile::tempdir().unwrap();
            let missing = dir.path().join("does-not-exist");
            let removed = sweep_stale_sockets(&missing, "claude-hookd-", Duration::from_secs(3600)).unwrap();
            assert!(removed.is_empty());
        }

        #[test]
        fn ignores_files_not_matching_the_prefix_or_suffix() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("unrelated.txt"), b"x").unwrap();
            std::fs::write(dir.path().join("other-prefix-abc.sock"), b"x").unwrap();
            let removed = sweep_stale_sockets(dir.path(), "claude-hookd-", Duration::from_secs(3600)).unwrap();
            assert!(removed.is_empty());
        }

        #[cfg(unix)]
        #[test]
        fn removes_a_socket_with_no_listener() {
            let dir = tempfile::tempdir().unwrap();
            let sock_path = dir.path().join("claude-hookd-abandoned.sock");
            {
                let _listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
            }
            let removed = sweep_stale_sockets(dir.path(), "claude-hookd-", Duration::from_secs(3600)).unwrap();
            assert_eq!(removed, vec![sock_path.clone()]);
            assert!(!sock_path.exists());
        }

        #[cfg(unix)]
        #[test]
        fn leaves_a_socket_with_a_live_listener_alone() {
            let dir = tempfile::tempdir().unwrap();
            let sock_path = dir.path().join("claude-hookd-alive.sock");
            let _listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
            let removed = sweep_stale_sockets(dir.path(), "claude-hookd-", Duration::from_secs(3600)).unwrap();
            assert!(removed.is_empty());
            assert!(sock_path.exists());
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn bare_notification_with_no_notification_type_falls_back_to_notify() {
        let payload = br#"{"session_id":"s1","hook_event_name":"Notification"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::Notify, false)));
    }

    #[test]
    fn notification_types_that_mean_needs_input_are_notify() {
        for kind in ["permission_prompt", "idle_prompt", "elicitation_dialog", "agent_needs_input", "agent_completed"] {
            let payload = format!(r#"{{"session_id":"s1","hook_event_name":"Notification","notification_type":"{kind}"}}"#);
            assert_eq!(
                parse_hook_event(payload.as_bytes()),
                Some(("s1".to_string(), ClientEvent::Notify, false)),
                "notification_type {kind:?} should be Notify"
            );
        }
    }

    #[test]
    fn notification_types_that_close_an_elicitation_are_resolve() {
        for kind in ["elicitation_complete", "elicitation_response"] {
            let payload = format!(r#"{{"session_id":"s1","hook_event_name":"Notification","notification_type":"{kind}"}}"#);
            assert_eq!(parse_hook_event(payload.as_bytes()), Some(("s1".to_string(), ClientEvent::Resolve, false)));
        }
    }

    #[test]
    fn auth_success_notification_is_ignored() {
        let payload = br#"{"session_id":"s1","hook_event_name":"Notification","notification_type":"auth_success"}"#;
        assert_eq!(parse_hook_event(payload), None);
    }

    #[test]
    fn stop_without_background_tasks_is_notify() {
        let payload = br#"{"session_id":"s1","hook_event_name":"Stop"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::Notify, false)));
        let payload = br#"{"session_id":"s1","hook_event_name":"Stop","background_tasks":[]}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::Notify, false)));
    }

    #[test]
    fn stop_with_a_shell_or_subagent_background_task_is_deferred() {
        for kind in ["shell", "subagent"] {
            let payload = format!(
                r#"{{"session_id":"s1","hook_event_name":"Stop","background_tasks":[{{"id":"t1","type":"{kind}","status":"running"}}]}}"#
            );
            assert_eq!(
                parse_hook_event(payload.as_bytes()),
                Some(("s1".to_string(), ClientEvent::StopDeferred, false)),
                "type {kind:?} should defer"
            );
        }
    }

    /// The type filter is deliberately an allowlist, not "array non-empty"
    /// — a long-lived `dream`/`monitor`/etc. entry must not suppress
    /// attention (see `parse_hook_event`'s doc comment on why the original,
    /// reverted `!is_empty()` version was too permissive). `"teammate"` is
    /// excluded from this list on purpose — unlike these other types, it
    /// gets its own dedicated classification
    /// (`ClientEvent::StopAmbiguousTeammate`), see
    /// `stop_with_only_a_teammate_background_task_is_stop_ambiguous_teammate`
    /// below.
    #[test]
    fn stop_with_only_non_relevant_background_task_types_is_notify() {
        for kind in ["dream", "auto-mode scan", "monitor", "MCP task", "cloud session", "workflow", "some-future-type"] {
            let payload = format!(
                r#"{{"session_id":"s1","hook_event_name":"Stop","background_tasks":[{{"id":"t1","type":"{kind}","status":"running"}}]}}"#
            );
            assert_eq!(
                parse_hook_event(payload.as_bytes()),
                Some(("s1".to_string(), ClientEvent::Notify, false)),
                "type {kind:?} should not defer"
            );
        }
    }

    /// The 2026-08-05 fix: `"teammate"` alone (no `"shell"`/`"subagent"`, no
    /// `session_crons`) is ambiguous, not automatically `Notify` — its
    /// `status` never reflects idle (see this crate's handoff notes), so
    /// `claude-hookd` needs its *own* recent-dispatch history
    /// (`daemon.rs`'s `teammate_dispatch`) to disambiguate, which
    /// `parse_hook_event` alone cannot do.
    #[test]
    fn stop_with_only_a_teammate_background_task_is_stop_ambiguous_teammate() {
        let payload = br#"{"session_id":"s1","hook_event_name":"Stop","background_tasks":[{"id":"t1","type":"teammate","status":"running"}]}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::StopAmbiguousTeammate, false)));
    }

    #[test]
    fn stop_defers_if_any_entry_is_relevant_even_alongside_irrelevant_ones() {
        let payload =
            br#"{"session_id":"s1","hook_event_name":"Stop","background_tasks":[{"id":"t1","type":"teammate","status":"running"},{"id":"t2","type":"shell","status":"running"}]}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::StopDeferred, false)));
    }

    /// Pins the fix for the real false-Attention report this file's docs
    /// describe: a `ScheduleWakeup`-driven wait shows up as a non-empty
    /// `session_crons` array with an empty `background_tasks`, and must
    /// still defer rather than fall through to `Notify`.
    #[test]
    fn stop_with_a_pending_session_cron_is_deferred_even_with_no_background_tasks() {
        let payload = br#"{"session_id":"s1","hook_event_name":"Stop","background_tasks":[],"session_crons":[{"id":"c1","schedule":"19 21 * * *","recurring":false}]}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::StopDeferred, false)));
    }

    /// Unlike `background_tasks`, `session_crons` needs no type-filtering —
    /// a recurring entry (e.g. a `/loop`-style schedule) defers exactly like
    /// a one-shot `ScheduleWakeup` entry.
    #[test]
    fn stop_with_a_recurring_session_cron_is_also_deferred() {
        let payload = br#"{"session_id":"s1","hook_event_name":"Stop","session_crons":[{"id":"c1","schedule":"*/5 * * * *","recurring":true}]}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::StopDeferred, false)));
    }

    #[test]
    fn stop_with_an_empty_session_crons_array_is_notify() {
        let payload = br#"{"session_id":"s1","hook_event_name":"Stop","session_crons":[]}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::Notify, false)));
    }

    #[test]
    fn stop_defers_from_session_crons_alongside_an_irrelevant_background_task() {
        let payload = br#"{"session_id":"s1","hook_event_name":"Stop","background_tasks":[{"id":"t1","type":"teammate","status":"running"}],"session_crons":[{"id":"c1","schedule":"19 21 * * *","recurring":false}]}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::StopDeferred, false)));
    }

    #[test]
    fn stop_failure_is_notify() {
        let payload = br#"{"session_id":"s1","hook_event_name":"StopFailure"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::Notify, false)));
    }

    #[test]
    fn permission_request_is_notify() {
        let payload = br#"{"session_id":"s1","hook_event_name":"PermissionRequest","tool_name":"Bash"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::Notify, false)));
    }

    #[test]
    fn parses_user_prompt_submit_as_resolve() {
        let payload = br#"{"session_id":"s1","hook_event_name":"UserPromptSubmit"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::Resolve, false)));
    }

    #[test]
    fn session_end_is_resolve_and_flagged_for_the_self_heal_fallback() {
        let payload = br#"{"session_id":"s1","hook_event_name":"SessionEnd"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::Resolve, true)));
    }

    #[test]
    fn pre_tool_use_ask_user_question_is_notify() {
        let payload = br#"{"session_id":"s1","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::Notify, false)));
    }

    #[test]
    fn pre_tool_use_for_other_tools_is_ignored() {
        let payload = br#"{"session_id":"s1","hook_event_name":"PreToolUse","tool_name":"Bash"}"#;
        assert_eq!(parse_hook_event(payload), None);
    }

    #[test]
    fn post_tool_use_resolves_regardless_of_tool_name() {
        for name in ["PostToolUse", "PostToolUseFailure"] {
            let payload = format!(r#"{{"session_id":"s1","hook_event_name":"{name}","tool_name":"Bash"}}"#);
            assert_eq!(parse_hook_event(payload.as_bytes()), Some(("s1".to_string(), ClientEvent::Resolve, false)));
        }
    }

    #[test]
    fn subagent_post_tool_use_is_ignored() {
        let payload = br#"{"session_id":"s1","hook_event_name":"PostToolUse","tool_name":"Bash","agent_id":"sub1"}"#;
        assert_eq!(parse_hook_event(payload), None);
    }

    /// The 2026-08-05 fix's other half: a successful, top-level dispatch to
    /// a teammate is the only signal `claude-hookd` has for "this session
    /// probably still has one running" — recorded (`daemon.rs`'s
    /// `teammate_dispatch`) so a later ambiguous `Stop` can tell a genuinely
    /// still-working teammate from a long-idle one whose `status` never
    /// stops saying `"running"`.
    #[test]
    fn post_tool_use_agent_or_send_message_is_teammate_dispatched() {
        for tool in ["Agent", "SendMessage"] {
            let payload = format!(r#"{{"session_id":"s1","hook_event_name":"PostToolUse","tool_name":"{tool}"}}"#);
            assert_eq!(
                parse_hook_event(payload.as_bytes()),
                Some(("s1".to_string(), ClientEvent::TeammateDispatched, false)),
                "tool_name {tool:?} should be TeammateDispatched"
            );
        }
    }

    /// A *failed* dispatch never actually handed work to a teammate, so it
    /// must not be recorded as one — falls through to plain `Resolve` like
    /// any other `PostToolUseFailure`, not `TeammateDispatched`.
    #[test]
    fn post_tool_use_failure_for_agent_is_plain_resolve_not_teammate_dispatched() {
        let payload = br#"{"session_id":"s1","hook_event_name":"PostToolUseFailure","tool_name":"Agent"}"#;
        assert_eq!(parse_hook_event(payload), Some(("s1".to_string(), ClientEvent::Resolve, false)));
    }

    /// A nested subagent dispatching its own teammate must not register a
    /// dispatch for the *top-level* session — same `!in_subagent` gate as
    /// every other `PostToolUse` classification.
    #[test]
    fn subagent_post_tool_use_for_agent_is_still_ignored() {
        let payload = br#"{"session_id":"s1","hook_event_name":"PostToolUse","tool_name":"Agent","agent_id":"sub1"}"#;
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
        assert_eq!(parse_hook_event(br#"{"hook_event_name":"Stop"}"#), None);
    }

    #[test]
    fn derive_daemon_sock_path_is_deterministic_and_prefixed() {
        let d = Delivery::IsekaiPipeCtl { ctl_sock: "/tmp/isekai-pipe-ctl-aaaa1111bbbb2222.sock".into() };
        let a = derive_daemon_sock_path(&d);
        let b = derive_daemon_sock_path(&d);
        assert_eq!(a, b, "the same delivery target must always derive the same daemon socket");
        assert!(a.to_string_lossy().contains(DAEMON_SOCK_PREFIX));
    }

    #[test]
    fn derive_daemon_sock_path_differs_for_different_targets() {
        let a = derive_daemon_sock_path(&Delivery::TmuxSession { session_id: "$1".to_string() });
        let b = derive_daemon_sock_path(&Delivery::TmuxSession { session_id: "$2".to_string() });
        assert_ne!(a, b);
    }
}
