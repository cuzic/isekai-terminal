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
//! 2. **`Tty` with tmux passthrough** (`$TMUX` set, no `$ISEKAI_CTL_SOCK`):
//!    writes the raw OSC directly to this pane's tty device
//!    (`tmux display-message -p -t $TMUX_PANE '#{pane_tty}'`), wrapped in
//!    tmux's passthrough DCS (`osc_color::wrap_for_tmux_passthrough`) so a
//!    tmux server with `allow-passthrough on` forwards it to the real outer
//!    terminal. Requires no isekai-terminal infrastructure at all — just a
//!    bare `ssh` + `tmux` session and `allow-passthrough` enabled.
//! 3. **`Tty` direct** (neither of the above; `$SSH_TTY` set): writes the
//!    raw, unwrapped OSC straight to `$SSH_TTY`'s device — correct when
//!    there's no tmux in the way at all, so nothing needs to relay/unwrap
//!    anything.
//!
//! If none of these resolve, `claude-hookd` has no way to reach a real
//! terminal and [`Delivery::resolve`] returns `None` — callers must treat
//! this as a silent no-op (see this crate's `main.rs`), never an error: a
//! misconfigured or unusual environment must not make Claude Code hooks
//! fail.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Delivery {
    IsekaiPipeCtl { ctl_sock: PathBuf },
    Tty { path: PathBuf, wrap_tmux_passthrough: bool },
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
            query_pane_tty,
        )
    }

    fn resolve_from(
        explicit: Option<&str>,
        ctl_sock: Option<String>,
        tmux_pane: Option<String>,
        ssh_tty: Option<String>,
        query_pane_tty: impl Fn(&str) -> Option<PathBuf>,
    ) -> Option<Self> {
        match explicit {
            Some("isekai-pipe") => return ctl_sock.map(|s| Delivery::IsekaiPipeCtl { ctl_sock: s.into() }),
            Some("tmux-passthrough") => {
                return tmux_pane
                    .as_deref()
                    .and_then(&query_pane_tty)
                    .map(|path| Delivery::Tty { path, wrap_tmux_passthrough: true });
            }
            Some("direct") => return ssh_tty.map(|s| Delivery::Tty { path: s.into(), wrap_tmux_passthrough: false }),
            // unset, or an unrecognized value — fall through to auto-detect
            _ => {}
        }
        if let Some(ctl_sock) = ctl_sock {
            return Some(Delivery::IsekaiPipeCtl { ctl_sock: ctl_sock.into() });
        }
        if let Some(path) = tmux_pane.as_deref().and_then(&query_pane_tty) {
            return Some(Delivery::Tty { path, wrap_tmux_passthrough: true });
        }
        ssh_tty.map(|s| Delivery::Tty { path: s.into(), wrap_tmux_passthrough: false })
    }

    /// A stable string identifying this delivery target, used to derive this
    /// tab's daemon socket name (`main.rs::derive_daemon_sock_path`) — the
    /// same target (ctl-socket path, or pane tty device) must always derive
    /// the same daemon socket so repeated hook events for the same pane
    /// reuse one daemon rather than spawning a new one every time.
    pub(crate) fn identity(&self) -> String {
        match self {
            Delivery::IsekaiPipeCtl { ctl_sock } => format!("ctl:{}", ctl_sock.display()),
            Delivery::Tty { path, .. } => format!("tty:{}", path.display()),
        }
    }

    /// Round-trips through `--delivery-spec` (see `main.rs`'s
    /// `spawn_detached_daemon`) so the detached `__serve` daemon doesn't
    /// need to re-resolve `$TMUX_PANE`/etc itself (it may not even inherit
    /// the same environment — spawn args are passed explicitly, matching
    /// this crate's general "resolve once, thread the value through"
    /// convention, same as `osc_color::TerminalKind`).
    pub(crate) fn to_spec(&self) -> String {
        match self {
            Delivery::IsekaiPipeCtl { ctl_sock } => format!("ctl:{}", ctl_sock.display()),
            Delivery::Tty { path, wrap_tmux_passthrough: true } => format!("tmux-tty:{}", path.display()),
            Delivery::Tty { path, wrap_tmux_passthrough: false } => format!("tty:{}", path.display()),
        }
    }

    pub(crate) fn from_spec(spec: &str) -> Option<Self> {
        if let Some(rest) = spec.strip_prefix("ctl:") {
            return Some(Delivery::IsekaiPipeCtl { ctl_sock: PathBuf::from(rest) });
        }
        if let Some(rest) = spec.strip_prefix("tmux-tty:") {
            return Some(Delivery::Tty { path: PathBuf::from(rest), wrap_tmux_passthrough: true });
        }
        if let Some(rest) = spec.strip_prefix("tty:") {
            return Some(Delivery::Tty { path: PathBuf::from(rest), wrap_tmux_passthrough: false });
        }
        None
    }
}

fn query_pane_tty(tmux_pane: &str) -> Option<PathBuf> {
    let output = std::process::Command::new("tmux").args(["display-message", "-p", "-t", tmux_pane, "#{pane_tty}"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Best-effort: a failed send here must never crash or wedge the caller,
/// just drop that one color/popup update (same trust model the pre-split
/// `isekai-pipe ctl`-based version already used).
pub(crate) async fn send_tab_color(delivery: &Delivery, (r, g, b): (u8, u8, u8)) {
    match delivery {
        Delivery::IsekaiPipeCtl { ctl_sock } => {
            let _ = send_ctl_message(ctl_sock, isekai_protocol::CtlMessage::SetTabColor { r, g, b }).await;
        }
        Delivery::Tty { path, wrap_tmux_passthrough } => {
            let seq = osc_color::tab_color_sequence(osc_color::TerminalKind::resolve(), r, g, b);
            write_tty(path, &maybe_wrap(&seq, *wrap_tmux_passthrough));
        }
    }
}

pub(crate) async fn send_notify_popup(delivery: &Delivery) {
    match delivery {
        Delivery::IsekaiPipeCtl { ctl_sock } => {
            let _ = send_ctl_message(
                ctl_sock,
                isekai_protocol::CtlMessage::Notify {
                    kind: isekai_protocol::NotifyKind::Waiting,
                    tmux_tag: String::new(),
                    seq: 0,
                    title: "Claude Code".to_string(),
                    body: "needs your input".to_string(),
                },
            )
            .await;
        }
        Delivery::Tty { path, wrap_tmux_passthrough } => {
            // OSC 9: the iTerm2/Growl-style "post a system notification"
            // convention several terminal emulators support (same choice
            // `isekai-ssh::ctl_forward::osc_sequence_for` makes for the same
            // message kind).
            let seq = "\x1b]9;Claude Code: needs your input\x07".to_string();
            write_tty(path, &maybe_wrap(&seq, *wrap_tmux_passthrough));
        }
    }
}

fn maybe_wrap(seq: &str, wrap_tmux_passthrough: bool) -> String {
    if wrap_tmux_passthrough {
        osc_color::wrap_for_tmux_passthrough(seq)
    } else {
        seq.to_string()
    }
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
async fn send_ctl_message(sock_path: &Path, msg: isekai_protocol::CtlMessage) -> std::io::Result<()> {
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

    fn no_pane_tty(_pane: &str) -> Option<PathBuf> {
        None
    }
    fn fake_pane_tty(_pane: &str) -> Option<PathBuf> {
        Some(PathBuf::from("/dev/pts/3"))
    }

    #[test]
    fn resolves_isekai_pipe_ctl_when_ctl_sock_is_set() {
        let d = Delivery::resolve_from(None, Some("/tmp/ctl.sock".to_string()), None, None, no_pane_tty);
        assert_eq!(d, Some(Delivery::IsekaiPipeCtl { ctl_sock: "/tmp/ctl.sock".into() }));
    }

    #[test]
    fn ctl_sock_takes_priority_over_tmux_when_both_present() {
        let d = Delivery::resolve_from(None, Some("/tmp/ctl.sock".to_string()), Some("%1".to_string()), None, fake_pane_tty);
        assert_eq!(d, Some(Delivery::IsekaiPipeCtl { ctl_sock: "/tmp/ctl.sock".into() }));
    }

    #[test]
    fn falls_back_to_tmux_passthrough_when_no_ctl_sock() {
        let d = Delivery::resolve_from(None, None, Some("%1".to_string()), None, fake_pane_tty);
        assert_eq!(d, Some(Delivery::Tty { path: "/dev/pts/3".into(), wrap_tmux_passthrough: true }));
    }

    #[test]
    fn falls_back_to_direct_tty_when_no_ctl_sock_or_tmux() {
        let d = Delivery::resolve_from(None, None, None, Some("/dev/pts/7".to_string()), no_pane_tty);
        assert_eq!(d, Some(Delivery::Tty { path: "/dev/pts/7".into(), wrap_tmux_passthrough: false }));
    }

    #[test]
    fn resolves_to_none_when_nothing_is_available() {
        let d = Delivery::resolve_from(None, None, None, None, no_pane_tty);
        assert_eq!(d, None);
    }

    #[test]
    fn explicit_override_forces_tmux_passthrough_even_with_ctl_sock_present() {
        let d = Delivery::resolve_from(
            Some("tmux-passthrough"),
            Some("/tmp/ctl.sock".to_string()),
            Some("%1".to_string()),
            None,
            fake_pane_tty,
        );
        assert_eq!(d, Some(Delivery::Tty { path: "/dev/pts/3".into(), wrap_tmux_passthrough: true }));
    }

    #[test]
    fn explicit_override_with_no_usable_target_is_none_not_a_fallback() {
        // Forced tmux-passthrough but not actually inside tmux (no pane tty
        // resolvable) — must not silently fall back to a different mechanism
        // the user didn't ask for.
        let d = Delivery::resolve_from(Some("tmux-passthrough"), Some("/tmp/ctl.sock".to_string()), None, None, no_pane_tty);
        assert_eq!(d, None);
    }

    #[test]
    fn explicit_isekai_pipe_override_requires_ctl_sock() {
        let d = Delivery::resolve_from(Some("isekai-pipe"), None, None, None, no_pane_tty);
        assert_eq!(d, None);
    }

    #[test]
    fn spec_round_trips_for_all_variants() {
        for d in [
            Delivery::IsekaiPipeCtl { ctl_sock: "/tmp/a.sock".into() },
            Delivery::Tty { path: "/dev/pts/3".into(), wrap_tmux_passthrough: true },
            Delivery::Tty { path: "/dev/pts/7".into(), wrap_tmux_passthrough: false },
        ] {
            assert_eq!(Delivery::from_spec(&d.to_spec()), Some(d));
        }
    }

    #[test]
    fn identity_distinguishes_ctl_from_tty_at_the_same_path_string() {
        let ctl = Delivery::IsekaiPipeCtl { ctl_sock: "/tmp/x".into() };
        let tty = Delivery::Tty { path: "/tmp/x".into(), wrap_tmux_passthrough: false };
        assert_ne!(ctl.identity(), tty.identity());
    }

    #[tokio::test]
    async fn send_tab_color_over_tty_writes_the_wrapped_osc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake-tty");
        std::fs::write(&path, b"").unwrap();
        let delivery = Delivery::Tty { path: path.clone(), wrap_tmux_passthrough: true };
        send_tab_color(&delivery, (0xff, 0x00, 0x00)).await;
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("\x1bPtmux;"), "must be wrapped for tmux passthrough: {written:?}");
        assert!(written.contains("4;264;rgb:ff/00/00"), "must contain the WT-compatible tab-color OSC: {written:?}");
    }

    #[tokio::test]
    async fn send_tab_color_over_direct_tty_is_not_wrapped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake-tty");
        std::fs::write(&path, b"").unwrap();
        let delivery = Delivery::Tty { path: path.clone(), wrap_tmux_passthrough: false };
        send_tab_color(&delivery, (0x00, 0xff, 0x00)).await;
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.starts_with("\x1bPtmux;"), "direct delivery must not wrap: {written:?}");
        assert!(written.contains("4;264;rgb:00/ff/00"));
    }
}
