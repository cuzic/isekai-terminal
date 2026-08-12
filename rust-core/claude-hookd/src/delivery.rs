//! How `claude-hookd` gets an OSC/`CtlMessage` from this process to the
//! user's real terminal. Three mechanisms, auto-detected in priority order
//! (or forced via `$CLAUDE_HOOKD_DELIVERY`):
//!
//! 1. **`IsekaiPipeCtl`** (`$ISEKAI_CTL_SOCK` set): the pre-existing
//!    mechanism this crate was split out of `isekai-pipe` from — sends a
//!    `CtlMessage` over the ctl-socket forward `isekai-ssh`/isekai-terminal
//!    set up, which then relays it to the real terminal itself. Keeps
//!    working exactly as before for isekai-terminal users; not required for
//!    anyone else.
//! 2. **`TmuxSession`** (`$TMUX` set, no `$ISEKAI_CTL_SOCK`): identifies the
//!    tmux *session* (`tmux display-message -p -t $TMUX_PANE '#{session_id}'`),
//!    not the pane — a real physical terminal tab is one tmux client
//!    attached to one session, and that session can have many
//!    windows/panes, each potentially running its own Claude Code instance.
//!    Keying by session (not pane) means every pane in that session shares
//!    one daemon and one aggregate [`state::TabState`], so one pane
//!    resolving its question can't paint over another pane's still-pending
//!    attention (2026-08, see the crate's git history for the bug this
//!    fixes — the original per-pane keying let N independent, mutually
//!    unaware daemons race to paint the one physical tab they all shared).
//!    Every actual write re-resolves *at write time* which pane is
//!    currently active in that session (`tmux display-message -p -t
//!    <session_id> '#{pane_tty}'`) rather than caching one pane's tty at
//!    daemon-spawn time — this also sidesteps needing to know whether tmux
//!    forwards `allow-passthrough` from a currently-inactive pane at all:
//!    writing into whichever pane is active *right now* always lands on the
//!    one pane tmux is actually rendering to the attached client, and stays
//!    correct even if the pane that happened to spawn the daemon is later
//!    closed. Wrapped in tmux's passthrough DCS
//!    ([`wrap_for_tmux_passthrough`]) so a tmux server with
//!    `allow-passthrough on` forwards it to the real outer terminal.
//!    Requires no isekai-terminal infrastructure at all — just a bare `ssh`
//!    + `tmux` session and `allow-passthrough` enabled.
//! 3. **`DirectTty`** (neither of the above; `$SSH_TTY` set): writes the
//!    raw, unwrapped OSC straight to `$SSH_TTY`'s device — correct when
//!    there's no tmux in the way at all, so nothing needs to relay/unwrap
//!    anything, and there is only ever one pane to begin with.
//!
//! If none of these resolve, `claude-hookd` has no way to reach a real
//! terminal and [`Delivery::resolve`] returns `None` — callers must treat
//! this as a silent no-op (see this crate's `main.rs`), never an error: a
//! misconfigured or unusual environment must not make Claude Code hooks
//! fail.

use std::path::{Path, PathBuf};

use super::protocol;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Delivery {
    IsekaiPipeCtl { ctl_sock: PathBuf },
    TmuxSession { session_id: String },
    DirectTty { path: PathBuf },
}

impl Delivery {
    /// Auto-detects (or honors `$CLAUDE_HOOKD_DELIVERY`'s explicit choice:
    /// `isekai-pipe`/`tmux-passthrough`/`direct`) which mechanism to use.
    /// `None` if the requested/detected mechanism has no usable target in
    /// this environment (e.g. `tmux-passthrough` forced but not actually
    /// running inside tmux) — see this module's docs on why that must be a
    /// silent no-op, not a fallback to a different mechanism the user didn't
    /// ask for.
    pub(crate) fn resolve() -> Option<Self> {
        Self::resolve_from(
            std::env::var("CLAUDE_HOOKD_DELIVERY").ok().as_deref(),
            std::env::var("ISEKAI_CTL_SOCK").ok(),
            std::env::var("TMUX_PANE").ok(),
            std::env::var("SSH_TTY").ok(),
            query_session_id,
        )
    }

    fn resolve_from(
        explicit: Option<&str>,
        ctl_sock: Option<String>,
        tmux_pane: Option<String>,
        ssh_tty: Option<String>,
        query_session_id: impl Fn(&str) -> Option<String>,
    ) -> Option<Self> {
        match explicit {
            Some("isekai-pipe") => return ctl_sock.map(|s| Delivery::IsekaiPipeCtl { ctl_sock: s.into() }),
            Some("tmux-passthrough") => {
                return tmux_pane.as_deref().and_then(&query_session_id).map(|session_id| Delivery::TmuxSession { session_id });
            }
            Some("direct") => return ssh_tty.map(|s| Delivery::DirectTty { path: s.into() }),
            // unset, or an unrecognized value — fall through to auto-detect
            _ => {}
        }
        if let Some(ctl_sock) = ctl_sock {
            return Some(Delivery::IsekaiPipeCtl { ctl_sock: ctl_sock.into() });
        }
        if let Some(session_id) = tmux_pane.as_deref().and_then(&query_session_id) {
            return Some(Delivery::TmuxSession { session_id });
        }
        ssh_tty.map(|s| Delivery::DirectTty { path: s.into() })
    }

    /// A stable string identifying this delivery target, used to derive this
    /// tab's daemon socket name (`main.rs::derive_daemon_sock_path`) — the
    /// same target (ctl-socket path, tmux session id, or direct tty device)
    /// must always derive the same daemon socket so repeated hook events for
    /// the same tab reuse one daemon rather than spawning a new one every
    /// time. Crucially, `TmuxSession`'s identity is the *session*, not any
    /// one pane — see this module's docs on why every pane in a session must
    /// share one daemon.
    ///
    /// Identical to [`Self::to_spec`] — deliberately implemented in terms of
    /// it rather than duplicating the match, so a future change to the spec
    /// format (which [`Self::from_spec`] parses back) can't silently
    /// desync the daemon-socket identity from it.
    pub(crate) fn identity(&self) -> String {
        self.to_spec()
    }

    /// Round-trips through `--delivery-spec` (see `main.rs`'s
    /// `spawn_detached_daemon`) so the detached `__serve` daemon doesn't
    /// need to re-resolve `$TMUX_PANE`/etc itself (it may not even inherit
    /// the same environment — spawn args are passed explicitly, matching
    /// this crate's general "resolve once, thread the value through"
    /// convention, same as `hooks_dir` — see `main.rs::hooks_dir`).
    pub(crate) fn to_spec(&self) -> String {
        match self {
            Delivery::IsekaiPipeCtl { ctl_sock } => format!("ctl:{}", ctl_sock.display()),
            Delivery::TmuxSession { session_id } => format!("tmux-session:{session_id}"),
            Delivery::DirectTty { path } => format!("tty:{}", path.display()),
        }
    }

    pub(crate) fn from_spec(spec: &str) -> Option<Self> {
        if let Some(rest) = spec.strip_prefix("ctl:") {
            return Some(Delivery::IsekaiPipeCtl { ctl_sock: PathBuf::from(rest) });
        }
        if let Some(rest) = spec.strip_prefix("tmux-session:") {
            return Some(Delivery::TmuxSession { session_id: rest.to_string() });
        }
        if let Some(rest) = spec.strip_prefix("tty:") {
            return Some(Delivery::DirectTty { path: PathBuf::from(rest) });
        }
        None
    }
}

fn query_format(target: &str, format: &str) -> Option<String> {
    let output = std::process::Command::new("tmux").args(["display-message", "-p", "-t", target, format]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn query_session_id(tmux_pane: &str) -> Option<String> {
    query_format(tmux_pane, "#{session_id}")
}

/// Resolves *at write time* which pane is currently active in `session_id`
/// — deliberately not cached from daemon-spawn time, see this module's docs.
fn query_current_pane_tty(session_id: &str) -> Option<PathBuf> {
    query_format(session_id, "#{pane_tty}").map(PathBuf::from)
}

/// Best-effort: a failed send here must never crash or wedge the caller,
/// just drop that one color/popup update (same trust model the pre-split
/// `isekai-pipe ctl`-based version already used). `hooks_dir` is only
/// consulted for `TmuxSession`/`DirectTty` (see [`super::tab_color::resolve`])
/// — `IsekaiPipeCtl` sends the raw RGB value over the ctl-socket and lets the
/// receiving end (`isekai-ssh`) decide how to render it, so it has no use
/// for a local tab-color script at all.
pub(crate) async fn send_tab_color(delivery: &Delivery, rgb: (u8, u8, u8), hooks_dir: Option<&Path>) {
    send_tab_color_with(delivery, rgb, hooks_dir, query_current_pane_tty).await
}

async fn send_tab_color_with(
    delivery: &Delivery,
    (r, g, b): (u8, u8, u8),
    hooks_dir: Option<&Path>,
    resolve_active_pane_tty: impl Fn(&str) -> Option<PathBuf>,
) {
    // Only resolved for TmuxSession/DirectTty — IsekaiPipeCtl sends the raw
    // RGB value over the ctl-socket instead and has no use for a local
    // tab-color script at all, so this must not spawn one just to discard
    // the result.
    let raw_osc = match delivery {
        Delivery::IsekaiPipeCtl { .. } => String::new(),
        Delivery::TmuxSession { .. } | Delivery::DirectTty { .. } => {
            super::tab_color::resolve(hooks_dir, r, g, b).await
        }
    };
    deliver(delivery, protocol::CtlMessage::SetTabColor { r, g, b }, &raw_osc, &resolve_active_pane_tty).await
}

/// Same shape as [`send_tab_color`]/[`send_tab_color_with`], for
/// `CtlMessage::SetProgress` — see [`super::tab_progress::resolve`] for how
/// the raw OSC 9;4 bytes are produced for the `TmuxSession`/`DirectTty`
/// paths. `IsekaiPipeCtl` sends the raw `(state, progress)` value over the
/// ctl-socket and lets the receiving end (`isekai-ssh`) decide how to render
/// it, exactly like `send_tab_color` does for RGB — so it has no use for a
/// local progress script either.
pub(crate) async fn send_progress(delivery: &Delivery, state: protocol::ProgressState, progress: u8, hooks_dir: Option<&Path>) {
    send_progress_with(delivery, state, progress, hooks_dir, query_current_pane_tty).await
}

async fn send_progress_with(
    delivery: &Delivery,
    state: protocol::ProgressState,
    progress: u8,
    hooks_dir: Option<&Path>,
    resolve_active_pane_tty: impl Fn(&str) -> Option<PathBuf>,
) {
    let raw_osc = match delivery {
        Delivery::IsekaiPipeCtl { .. } => String::new(),
        Delivery::TmuxSession { .. } | Delivery::DirectTty { .. } => {
            super::tab_progress::resolve(hooks_dir, state, progress).await
        }
    };
    deliver(delivery, protocol::CtlMessage::SetProgress { state, progress }, &raw_osc, &resolve_active_pane_tty).await
}

pub(crate) async fn send_notify_popup(delivery: &Delivery) {
    send_notify_popup_with(delivery, query_current_pane_tty).await
}

async fn send_notify_popup_with(delivery: &Delivery, resolve_active_pane_tty: impl Fn(&str) -> Option<PathBuf>) {
    // OSC 9: the iTerm2/Growl-style "post a system notification" convention
    // several terminal emulators support (same choice
    // `isekai-ssh::ctl_forward::osc_sequence_for` makes for the same message
    // kind).
    let raw_osc = "\x1b]9;Claude Code: needs your input\x07";
    let ctl_msg = protocol::CtlMessage::Notify {
        kind: protocol::NotifyKind::Waiting,
        tmux_tag: String::new(),
        seq: 0,
        title: "Claude Code".to_string(),
        body: "needs your input".to_string(),
    };
    deliver(delivery, ctl_msg, raw_osc, &resolve_active_pane_tty).await
}

/// The shared 3-way dispatch every `send_*` helper in this module reduces
/// to: `IsekaiPipeCtl` sends `ctl_msg` over the ctl-socket; `TmuxSession`
/// resolves the currently-active pane and writes `raw_osc` wrapped for tmux
/// passthrough; `DirectTty` writes `raw_osc` unwrapped. Keeping this in one
/// place means the two callers can't independently drift on *how* tmux
/// wrapping is done (only on *what* `ctl_msg`/`raw_osc` are) — the OSC
/// sequence content itself is still each caller's own concern
/// ([`super::tab_color::resolve`] for colors, a compiled-in literal for the
/// notify popup).
///
/// `raw_osc` empty is a deliberate downgrade (either a tab-color script
/// failed/declined to run, or — for a fixed literal like the notify
/// popup — simply never happens) — not a reason to write an empty
/// passthrough wrapper to the tty.
async fn deliver(
    delivery: &Delivery,
    ctl_msg: protocol::CtlMessage,
    raw_osc: &str,
    resolve_active_pane_tty: &impl Fn(&str) -> Option<PathBuf>,
) {
    match delivery {
        Delivery::IsekaiPipeCtl { ctl_sock } => {
            let _ = send_ctl_message(ctl_sock, ctl_msg).await;
        }
        Delivery::TmuxSession { session_id } => {
            let Some(path) = resolve_active_pane_tty(session_id) else { return };
            if !raw_osc.is_empty() {
                write_tty(&path, &wrap_for_tmux_passthrough(raw_osc));
            }
        }
        Delivery::DirectTty { path } => {
            if !raw_osc.is_empty() {
                write_tty(path, raw_osc);
            }
        }
    }
}

/// Wraps an arbitrary escape sequence in tmux's passthrough DCS
/// (`\ePtmux;<payload with every ESC doubled>\e\\`) so a tmux server with
/// `allow-passthrough on` forwards it to the real outer terminal instead of
/// swallowing it. Every real `ESC` (`0x1b`) byte inside `inner` must be
/// doubled per tmux's own escaping rule for this wrapper. Inlined from the
/// former `osc-color` crate dependency (removed 2026-08) — this part is
/// generic (no terminal-kind knowledge needed), so unlike
/// [`super::tab_color`]'s job it had no reason to move to a shell script.
fn wrap_for_tmux_passthrough(inner: &str) -> String {
    let escaped = inner.replace('\x1b', "\x1b\x1b");
    format!("\x1bPtmux;{escaped}\x1b\\")
}

fn write_tty(path: &Path, bytes: &str) {
    use std::io::Write as _;
    // Best-effort, sync: these writes are tiny (well under a pty's PIPE_BUF)
    // and infrequent (one per state transition, not per hook event — see
    // `state.rs`'s debouncing), so a blocking open+write from an async
    // context is an acceptable, simpler alternative to `tokio::fs` here.
    //
    // `.append(true)` (not just `.write(true)`) matters: a real tty/pty
    // device has no meaningful seek position, so this makes no behavioral
    // difference there — but this same function's tests stand a plain
    // tempfile in for the tty, and a bare `.write(true)` open always seeks
    // to position 0 on every call, so a shorter write doesn't fully
    // overwrite a longer previous one and leaves stale trailing bytes
    // behind (found via a real, non-deterministic CI failure: a slow CI
    // runner let the attention timeout fire and overwrite before the test's
    // own read, and the leftover tail of the earlier, longer popup message
    // survived past the new, shorter idle-color write and corrupted the
    // assertion). `.append(true)` makes every write observably a clean
    // sequential log instead.
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).append(true).open(path) {
        let _ = f.write_all(bytes.as_bytes());
    }
}

/// Duplicated from (no longer depends on) `isekai-pipe::ctl::send_ctl_message`
/// — this crate is deliberately independent of `isekai-pipe`/`isekai-ssh`
/// (see crate-level docs), and this is the one place that still needs to
/// speak the ctl-socket wire protocol for the `IsekaiPipeCtl` delivery mode.
/// Kept in sync manually; if the real protocol (preamble line + one JSON
/// line, half-close) ever changes, both copies need updating together (same
/// duplication trade-off this project already accepts elsewhere — see
/// `isekai-ssh-e2e-test-self-containment-convention`).
async fn send_ctl_message(sock_path: &Path, msg: protocol::CtlMessage) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(sock_path).await?;
    let mut preamble = sock_path.to_string_lossy().into_owned().into_bytes();
    preamble.push(b'\n');
    stream.write_all(&preamble).await?;
    let mut line = serde_json::to_vec(&msg)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.shutdown().await.ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_session_id(_pane: &str) -> Option<String> {
        None
    }
    fn fake_session_id(_pane: &str) -> Option<String> {
        Some("$3".to_string())
    }

    #[test]
    fn resolves_isekai_pipe_ctl_when_ctl_sock_is_set() {
        let d = Delivery::resolve_from(None, Some("/tmp/ctl.sock".to_string()), None, None, no_session_id);
        assert_eq!(d, Some(Delivery::IsekaiPipeCtl { ctl_sock: "/tmp/ctl.sock".into() }));
    }

    #[test]
    fn ctl_sock_takes_priority_over_tmux_when_both_present() {
        let d = Delivery::resolve_from(None, Some("/tmp/ctl.sock".to_string()), Some("%1".to_string()), None, fake_session_id);
        assert_eq!(d, Some(Delivery::IsekaiPipeCtl { ctl_sock: "/tmp/ctl.sock".into() }));
    }

    #[test]
    fn falls_back_to_tmux_passthrough_when_no_ctl_sock() {
        let d = Delivery::resolve_from(None, None, Some("%1".to_string()), None, fake_session_id);
        assert_eq!(d, Some(Delivery::TmuxSession { session_id: "$3".to_string() }));
    }

    #[test]
    fn falls_back_to_direct_tty_when_no_ctl_sock_or_tmux() {
        let d = Delivery::resolve_from(None, None, None, Some("/dev/pts/7".to_string()), no_session_id);
        assert_eq!(d, Some(Delivery::DirectTty { path: "/dev/pts/7".into() }));
    }

    #[test]
    fn resolves_to_none_when_nothing_is_available() {
        let d = Delivery::resolve_from(None, None, None, None, no_session_id);
        assert_eq!(d, None);
    }

    #[test]
    fn explicit_override_forces_tmux_passthrough_even_with_ctl_sock_present() {
        let d = Delivery::resolve_from(
            Some("tmux-passthrough"),
            Some("/tmp/ctl.sock".to_string()),
            Some("%1".to_string()),
            None,
            fake_session_id,
        );
        assert_eq!(d, Some(Delivery::TmuxSession { session_id: "$3".to_string() }));
    }

    #[test]
    fn explicit_override_with_no_usable_target_is_none_not_a_fallback() {
        // Forced tmux-passthrough but not actually inside tmux (no session id
        // resolvable) — must not silently fall back to a different mechanism
        // the user didn't ask for.
        let d = Delivery::resolve_from(Some("tmux-passthrough"), Some("/tmp/ctl.sock".to_string()), None, None, no_session_id);
        assert_eq!(d, None);
    }

    #[test]
    fn explicit_isekai_pipe_override_requires_ctl_sock() {
        let d = Delivery::resolve_from(Some("isekai-pipe"), None, None, None, no_session_id);
        assert_eq!(d, None);
    }

    #[test]
    fn spec_round_trips_for_all_variants() {
        for d in [
            Delivery::IsekaiPipeCtl { ctl_sock: "/tmp/a.sock".into() },
            Delivery::TmuxSession { session_id: "$3".to_string() },
            Delivery::DirectTty { path: "/dev/pts/7".into() },
        ] {
            assert_eq!(Delivery::from_spec(&d.to_spec()), Some(d));
        }
    }

    #[test]
    fn identity_distinguishes_ctl_from_tty_at_the_same_path_string() {
        let ctl = Delivery::IsekaiPipeCtl { ctl_sock: "/tmp/x".into() };
        let tty = Delivery::DirectTty { path: "/tmp/x".into() };
        assert_ne!(ctl.identity(), tty.identity());
    }

    #[test]
    fn identity_is_shared_across_panes_of_the_same_tmux_session() {
        // The whole point of keying by session rather than pane: two
        // different panes (different `$TMUX_PANE`, different underlying
        // pane tty) in the same session must still resolve to the same
        // daemon so they share one aggregate `TabState` — see this module's
        // docs on the multi-pane-vs-one-physical-tab bug this fixes.
        let pane_a = Delivery::resolve_from(None, None, Some("%1".to_string()), None, |_| Some("$3".to_string()));
        let pane_b = Delivery::resolve_from(None, None, Some("%2".to_string()), None, |_| Some("$3".to_string()));
        assert_eq!(pane_a.unwrap().identity(), pane_b.unwrap().identity());
    }

    /// Writes a deterministic `hooks_dir/tab-color` script and returns its
    /// containing dir — used instead of the embedded default (which depends
    /// on ambient `$TERM_PROGRAM`) so these tests only exercise *this
    /// module's* wrapping/dispatch logic, not `tab_color`'s terminal-kind
    /// detection (already covered by that module's own tests). An
    /// adversarial review (2026-08-09) caught that the original versions of
    /// these tests asserted the Windows-Terminal-specific OSC format via the
    /// real embedded default, which only passed because this sandbox
    /// happens to run under tmux (`$TERM_PROGRAM=tmux`, not `iTerm.app`) —
    /// on a real macOS iTerm2 machine they would have failed every time.
    fn custom_tab_color_hooks_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("tab-color");
        std::fs::write(&script_path, "#!/bin/sh\nprintf 'OSCTEST:%s' \"$1\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
        dir
    }

    #[tokio::test]
    async fn send_tab_color_over_tmux_session_resolves_the_active_pane_at_write_time() {
        let tty_dir = tempfile::tempdir().unwrap();
        let path = tty_dir.path().join("fake-tty");
        std::fs::write(&path, b"").unwrap();
        let hooks_dir = custom_tab_color_hooks_dir();
        let delivery = Delivery::TmuxSession { session_id: "$3".to_string() };
        let resolved_path = path.clone();
        send_tab_color_with(&delivery, (0xff, 0x00, 0x00), Some(hooks_dir.path()), move |session_id| {
            assert_eq!(session_id, "$3");
            Some(resolved_path.clone())
        })
        .await;
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("\x1bPtmux;"), "must be wrapped for tmux passthrough: {written:?}");
        assert!(written.contains("OSCTEST:ff0000"), "must contain the tab-color script's output: {written:?}");
    }

    #[tokio::test]
    async fn send_tab_color_over_tmux_session_is_a_silent_no_op_when_no_pane_resolves() {
        // e.g. every pane in the session has since been closed — must not
        // panic or write anywhere.
        let delivery = Delivery::TmuxSession { session_id: "$3".to_string() };
        send_tab_color_with(&delivery, (0xff, 0x00, 0x00), None, |_| None).await;
    }

    #[tokio::test]
    async fn send_tab_color_over_direct_tty_is_not_wrapped() {
        let tty_dir = tempfile::tempdir().unwrap();
        let path = tty_dir.path().join("fake-tty");
        std::fs::write(&path, b"").unwrap();
        let hooks_dir = custom_tab_color_hooks_dir();
        let delivery = Delivery::DirectTty { path: path.clone() };
        send_tab_color(&delivery, (0x00, 0xff, 0x00), Some(hooks_dir.path())).await;
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.starts_with("\x1bPtmux;"), "direct delivery must not wrap: {written:?}");
        assert!(written.contains("OSCTEST:00ff00"));
    }

    /// Writes a deterministic `hooks_dir/tab-progress` script — same
    /// rationale as [`custom_tab_color_hooks_dir`] (isolates these tests
    /// from `tab_progress`'s own terminal-kind detection, already covered by
    /// that module's own tests).
    fn custom_tab_progress_hooks_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("tab-progress");
        std::fs::write(&script_path, "#!/bin/sh\nprintf 'OSCTEST:%s:%s' \"$1\" \"$2\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
        dir
    }

    #[tokio::test]
    async fn send_progress_over_tmux_session_resolves_the_active_pane_at_write_time() {
        let tty_dir = tempfile::tempdir().unwrap();
        let path = tty_dir.path().join("fake-tty");
        std::fs::write(&path, b"").unwrap();
        let hooks_dir = custom_tab_progress_hooks_dir();
        let delivery = Delivery::TmuxSession { session_id: "$3".to_string() };
        let resolved_path = path.clone();
        send_progress_with(&delivery, protocol::ProgressState::Indeterminate, 0, Some(hooks_dir.path()), move |session_id| {
            assert_eq!(session_id, "$3");
            Some(resolved_path.clone())
        })
        .await;
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("\x1bPtmux;"), "must be wrapped for tmux passthrough: {written:?}");
        assert!(written.contains("OSCTEST:3:0"), "must contain the tab-progress script's output: {written:?}");
    }

    #[tokio::test]
    async fn send_progress_over_tmux_session_is_a_silent_no_op_when_no_pane_resolves() {
        let delivery = Delivery::TmuxSession { session_id: "$3".to_string() };
        send_progress_with(&delivery, protocol::ProgressState::Indeterminate, 0, None, |_| None).await;
    }

    #[tokio::test]
    async fn send_progress_over_direct_tty_is_not_wrapped() {
        let tty_dir = tempfile::tempdir().unwrap();
        let path = tty_dir.path().join("fake-tty");
        std::fs::write(&path, b"").unwrap();
        let hooks_dir = custom_tab_progress_hooks_dir();
        let delivery = Delivery::DirectTty { path: path.clone() };
        send_progress(&delivery, protocol::ProgressState::Normal, 42, Some(hooks_dir.path())).await;
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.starts_with("\x1bPtmux;"), "direct delivery must not wrap: {written:?}");
        assert!(written.contains("OSCTEST:1:42"));
    }

    /// Unlike `send_tab_color`, `IsekaiPipeCtl` delivery has no prior direct
    /// test of the actual bytes written to the ctl-socket — this pins that
    /// `send_progress` forwards the raw `CtlMessage::SetProgress` value
    /// unchanged, letting the receiving `isekai-ssh` decide the OSC (see
    /// this module's docs on why `IsekaiPipeCtl` never resolves a local raw
    /// OSC sequence at all).
    #[tokio::test]
    async fn send_progress_over_isekai_pipe_ctl_sends_the_raw_ctl_message() {
        use tokio::io::AsyncReadExt as _;

        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("ctl.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        let accept = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            buf
        });

        let delivery = Delivery::IsekaiPipeCtl { ctl_sock: sock_path.clone() };
        send_progress(&delivery, protocol::ProgressState::Indeterminate, 0, None).await;

        let received = accept.await.unwrap();
        let text = String::from_utf8(received).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(sock_path.to_string_lossy().as_ref()), "preamble must be the ctl-sock path");
        let msg: protocol::CtlMessage = serde_json::from_str(lines.next().expect("a message line")).unwrap();
        assert_eq!(msg, protocol::CtlMessage::SetProgress { state: protocol::ProgressState::Indeterminate, progress: 0 });
    }
}
