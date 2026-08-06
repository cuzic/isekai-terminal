//! `isekai-pipe tty daemon <name>` / `isekai-pipe tty attach <name>`: a
//! self-owned remote pty session, dtach/mosh-style — reconnecting after the
//! *local* `isekai-ssh`/`ssh(1)` process was fully killed (not just a
//! network drop, which the existing QUIC/transport resume in
//! `engine/resume.rs` already handles) still lands back in the exact same
//! remote shell process.
//!
//! This exists because transport-level resume cannot do this on its own: it
//! only parks the raw TCP socket to the real `sshd`, but the *SSH protocol's
//! own* encryption/sequence-number state lives in the local SSH client
//! process and dies with it — a fresh process can't splice a new,
//! unauthenticated-at-the-SSH-layer connection into the middle of an
//! already-encrypted stream. The shell must instead be owned by something
//! that outlives both the local process *and* the SSH protocol session
//! itself, the same role tmux/screen/dtach/mosh-server play for their
//! respective tools.
//!
//! ## Module layout
//!
//! - [`protocol`]: the daemon↔attach-client frame wire format.
//! - [`ring_buffer`]: the drop-oldest replay-on-attach buffer.
//! - `pty` (task #12): spawns the pty-attached shell process.
//! - `daemon` (task #13/#14/#17): the long-lived process — accept loop,
//!   attach-slot preemption, pty relay, socket/lock lifecycle.
//! - `attach` (task #15/#16/#18): the thin client `isekai-ssh` execs as the
//!   remote command in place of a login shell.
//!
//! ## Non-goals (scope decisions, confirmed with the maintainer)
//!
//! - No concurrent multi-client attach (tmux's broadcast-to-everyone model).
//!   A new `attach` always preempts whatever was previously attached —
//!   dtach/mosh-style. The actual requirement (survive full process death)
//!   doesn't need simultaneous viewers, and this keeps the protocol/slot
//!   logic far simpler (see [`Frame::Preempted`](protocol::Frame::Preempted)).
//! - Daemon lifetime is exactly the shell's lifetime: when the spawned shell
//!   exits, the daemon exits too and cleans up its socket/lock — "the
//!   session ends when the shell inside it ends," matching tmux killing a
//!   session when its last pane dies. No separate manual-kill-required
//!   persistence, no idle timeout to tune.
//! - Every remote host this project targets is Linux (the same assumption
//!   `isekai-ssh`'s `ctl_forward.rs` already makes) — `pty`/`daemon`/`attach`
//!   are Unix-only and use Linux-specific primitives (`SO_PEERCRED`)
//!   without a portability shim.

mod protocol;
mod ring_buffer;
mod attach_slot;
#[cfg(unix)]
mod pty;
#[cfg(unix)]
mod unix_socket;
#[cfg(unix)]
mod daemon_lock;
#[cfg(unix)]
mod daemon;
#[cfg(unix)]
mod attach;

use std::process::ExitCode;

/// `isekai-pipe tty <daemon|attach> <name> [...]` dispatch.
pub(crate) async fn tty_command(mut args: impl Iterator<Item = String>) -> ExitCode {
    match args.next().as_deref() {
        Some("daemon") => tty_daemon_command(args).await,
        Some("attach") => tty_attach_command(args).await,
        Some(other) => {
            eprintln!("isekai-pipe tty: unknown subcommand {other:?} (expected \"daemon\" or \"attach\")");
            ExitCode::from(crate::EX_USAGE)
        }
        None => {
            eprintln!("isekai-pipe tty: a subcommand is required (\"daemon\" or \"attach\")");
            ExitCode::from(crate::EX_USAGE)
        }
    }
}

#[cfg(unix)]
async fn tty_daemon_command(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(name) = args.next() else {
        eprintln!("isekai-pipe tty daemon: a <name> is required");
        return ExitCode::from(crate::EX_USAGE);
    };
    // Everything after an optional `--` is the command to run in place of
    // the default login shell; `next()` already consumed `name`, so a bare
    // `--` (with nothing before it in `args`) is exactly "no command given
    // beyond the name," handled the same as omitting `--` entirely.
    let rest: Vec<String> = args.collect();
    let command = match rest.split_first() {
        Some((sep, tail)) if sep == "--" && !tail.is_empty() => tail.to_vec(),
        _ => default_shell_command(),
    };

    match daemon::run(&name, command).await {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("isekai-pipe tty daemon: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `$SHELL -l` (a login shell, matching `tmux new-session`'s own default),
/// falling back to `/bin/sh -l` when `$SHELL` isn't set — every remote host
/// this feature targets is Linux, where `/bin/sh` always exists.
#[cfg(unix)]
fn default_shell_command() -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    vec![shell, "-l".to_string()]
}

#[cfg(unix)]
async fn tty_attach_command(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(name) = args.next() else {
        eprintln!("isekai-pipe tty attach: a <name> is required");
        return ExitCode::from(crate::EX_USAGE);
    };
    match attach::run(&name).await {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("isekai-pipe tty attach: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
async fn tty_daemon_command(_args: impl Iterator<Item = String>) -> ExitCode {
    eprintln!("isekai-pipe tty daemon: only supported on Unix (the remote host this feature targets is always Linux)");
    ExitCode::from(crate::EX_USAGE)
}

#[cfg(not(unix))]
async fn tty_attach_command(_args: impl Iterator<Item = String>) -> ExitCode {
    eprintln!("isekai-pipe tty attach: only supported on Unix (the remote host this feature targets is always Linux)");
    ExitCode::from(crate::EX_USAGE)
}
