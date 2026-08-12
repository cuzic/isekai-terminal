//! Regression coverage for a real bug found via pre-mortem review
//! (2026-08-12, after the exit-hang/raw-mode bugs fixed in PR #87/#88):
//! `RingBuffer::replay()` used to prepend the soft-reset sequence (`ESC c`,
//! RIS — a full terminal reset) *unconditionally*, even when nothing had
//! ever been written to a fresh session's ring buffer. `daemon.rs`'s
//! `if !replay.is_empty()` guard around sending that replay as the
//! connecting client's first `Frame::Stdout` was written assuming
//! `replay()` *could* return empty — since it never did, that guard was
//! dead code, and **every single `tty attach`, including the very first one
//! to a brand-new session**, sent RIS to the user's real terminal. On most
//! emulators (Windows Terminal, iTerm2, VTE-based ones) that silently wipes
//! the visible screen *and* the scrollback buffer on every connect, with no
//! error and no way to tell it happened short of noticing lost scrollback.
//!
//! Fixed in `ring_buffer.rs::replay` to return an empty `Vec` (no
//! soft-reset, nothing to send) when the buffer itself is empty — the
//! soft-reset only exists to guard against a *replayed* escape sequence
//! that got truncated mid-sequence by eviction, which cannot happen when
//! there's nothing to replay.

#![cfg(unix)]

use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

fn isekai_pipe_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-pipe"))
}

/// See `tty_attach_exit_and_rawmode_e2e.rs`'s identical helper's doc
/// comment: a short, `/tmp`-rooted `$HOME` sidesteps macOS CI's long
/// default temp-dir base, which otherwise overflows `sockaddr_un`'s
/// `sun_path` for this feature's socket path.
fn short_tmp_home() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("e").tempdir_in("/tmp").expect("failed to create a short-path temp dir under /tmp")
}

fn open_pty_pair() -> (OwnedFd, RawFd) {
    let mut master_fd: libc::c_int = -1;
    let mut slave_fd: libc::c_int = -1;
    // SAFETY: `openpty` writes valid, newly-opened fds into both out-params
    // on success; `name`/`termp`/`winp` null is documented as accepted
    // (defaults).
    let rc = unsafe { libc::openpty(&mut master_fd, &mut slave_fd, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(rc, 0, "openpty failed: {}", std::io::Error::last_os_error());
    // SAFETY: `master_fd` was just returned by `openpty` as a valid, open,
    // uniquely-owned descriptor.
    (unsafe { OwnedFd::from_raw_fd(master_fd) }, slave_fd)
}

fn dup_stdio(slave_fd: RawFd) -> Stdio {
    // SAFETY: `slave_fd` is a valid, open fd for the duration of this call
    // (the caller keeps its own copy open until every dup has been made).
    let dup_fd = unsafe { libc::dup(slave_fd) };
    assert!(dup_fd >= 0, "dup failed: {}", std::io::Error::last_os_error());
    // SAFETY: `dup_fd` was just returned by `dup` as a valid, open,
    // uniquely-owned descriptor.
    unsafe { Stdio::from_raw_fd(dup_fd) }
}

/// The very first `isekai-pipe tty attach` to a brand-new session name must
/// not receive the soft-reset (`ESC c` / RIS) sequence at all — there is
/// nothing buffered yet for it to be guarding a replay of. Drives this
/// through a real pty (matching what `sshd` actually gives the process) so
/// this is exercising exactly what a real user's terminal would receive as
/// raw bytes, not just the daemon's own internal `RingBuffer` in isolation.
#[tokio::test]
async fn first_ever_attach_to_a_fresh_session_receives_no_soft_reset() {
    use tokio::io::AsyncReadExt as _;

    let home = short_tmp_home();
    let name = "e2e-no-spurious-reset";
    let (master, slave_fd) = open_pty_pair();

    let mut attach = tokio::process::Command::new(isekai_pipe_bin_path())
        .args(["tty", "attach", name])
        .env("HOME", home.path())
        .env("TERM", "xterm-256color")
        .stdin(dup_stdio(slave_fd))
        .stdout(dup_stdio(slave_fd))
        .stderr(dup_stdio(slave_fd))
        .spawn()
        .expect("failed to spawn `isekai-pipe tty attach` with a real pty");
    unsafe { libc::close(slave_fd) };

    // Read whatever the freshly-spawned shell/daemon sends back over a
    // window generous enough for the shell to start and print its prompt
    // (matching the raw-mode e2e test's own timing), then check the very
    // first bytes.
    let master_std = std::fs::File::from(master);
    let mut master_async = tokio::fs::File::from_std(master_std);
    let mut buf = [0u8; 256];
    let n = tokio::time::timeout(Duration::from_secs(5), master_async.read(&mut buf))
        .await
        .expect("must receive some output from the freshly-spawned shell within 5s")
        .expect("read from the pty master failed");

    assert!(n > 0, "expected at least some bytes from the shell starting up");
    assert!(
        !buf[..n].starts_with(b"\x1bc"),
        "the very first attach to a brand-new session must not receive the soft-reset (ESC c / RIS) sequence — \
         regression: RingBuffer::replay() used to send it unconditionally even with nothing buffered, wiping the \
         user's real terminal scrollback on every connect; got {:?}",
        String::from_utf8_lossy(&buf[..n])
    );

    // Cleanup: exit the shell and reap the process.
    // SAFETY: `master_async`'s underlying fd is valid and open; `b"exit\n"`
    // is a valid buffer for the duration of this one `write` call.
    use tokio::io::AsyncWriteExt as _;
    let _ = master_async.write_all(b"exit\n").await;
    let _ = tokio::time::timeout(Duration::from_secs(5), attach.wait()).await;
}
