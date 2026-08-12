//! Regression coverage for three real bugs found via live manual
//! reproduction (2026-08-12, real Windows Terminal client + this sandbox as
//! the SSH server) that no existing automated test caught:
//!
//! 1. `isekai-pipe tty attach` never put its own pty (the one `sshd`
//!    allocates for the SSH session) into raw mode, so control characters
//!    were echoed by the kernel's `ECHOCTL` as literal `^X` text and Ctrl-D
//!    behaved like canonical-mode `VEOF` (only flushed the current line)
//!    instead of the raw EOF byte a shell reading in raw mode expects.
//!    Fixed by `attach.rs`'s `RawModeGuard`.
//! 2. Even after (1), typing `exit` hung forever: `tokio::io::stdin()`'s
//!    read on Unix runs on an internal blocking-thread, which
//!    `JoinHandle::abort()` cannot interrupt, so letting `relay()` return
//!    normally let `main`'s `Runtime` drop and wait forever on that stuck
//!    thread during its default graceful shutdown. Fixed by an explicit
//!    `std::process::exit()`.
//! 3. Fixing (2) surfaced a race: `daemon.rs::run()` returning immediately
//!    after `notify_exit()` let the daemon process's own teardown win
//!    against its still-scheduled writer task actually flushing
//!    `Frame::Exit` to the socket, so `tty attach` usually saw the
//!    connection drop uncleanly ("connection to the tty daemon closed
//!    unexpectedly") instead of a clean exit. Fixed by a short grace sleep.
//!
//! All three are pure Unix/Linux server-side bugs (`src/tty/` is
//! `#[cfg(unix)]` throughout — the *client* that execs `isekai-ssh` happens
//! to run on Windows in the reports that found these, but nothing here is
//! Windows-specific), so — unlike the raw-mode-on-Windows-console bug fixed
//! earlier in the same investigation (`isekai-ssh`'s own `RawModeGuard`) —
//! these are fully exercisable in ordinary Linux CI.

#![cfg(unix)]

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

fn isekai_pipe_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-pipe"))
}

/// Opens a fresh pty pair the same way `src/tty/pty.rs::spawn` does for the
/// shell it owns — here standing in for the pty `sshd` allocates for the
/// SSH session `isekai-pipe tty attach` itself runs inside of.
fn open_pty_pair() -> (OwnedFd, RawFd) {
    let mut master_fd: libc::c_int = -1;
    let mut slave_fd: libc::c_int = -1;
    // SAFETY: `openpty` writes valid, newly-opened fds into both out-params
    // on success; `name`/`termp`/`winp` null is documented as accepted
    // (defaults). Same call shape as `pty.rs::spawn`.
    let rc = unsafe { libc::openpty(&mut master_fd, &mut slave_fd, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(rc, 0, "openpty failed: {}", std::io::Error::last_os_error());
    // SAFETY: `master_fd` was just returned by `openpty` as a valid, open,
    // uniquely-owned descriptor.
    (unsafe { OwnedFd::from_raw_fd(master_fd) }, slave_fd)
}

/// `dup`s `slave_fd` for exclusive ownership by one of the child's three
/// standard streams — `Stdio::from_raw_fd` takes ownership of whatever fd
/// it's given, so stdin/stdout/stderr each need their own independent copy
/// of the same underlying pty slave (matching what a real `login_tty`-style
/// dup2 of one fd onto 0/1/2 achieves).
fn dup_stdio(slave_fd: RawFd) -> Stdio {
    // SAFETY: `slave_fd` is a valid, open fd for the duration of this call
    // (the caller keeps its own copy open until every dup has been made).
    let dup_fd = unsafe { libc::dup(slave_fd) };
    assert!(dup_fd >= 0, "dup failed: {}", std::io::Error::last_os_error());
    // SAFETY: `dup_fd` was just returned by `dup` as a valid, open,
    // uniquely-owned descriptor.
    unsafe { Stdio::from_raw_fd(dup_fd) }
}

/// Reads the pty's current termios via the master end (Linux's `TCGETS`
/// reports the same line-discipline state regardless of which end of the
/// pair you query it from) — `true` once both `ICANON` and `ECHO` are
/// clear, i.e. raw mode is actually in effect.
fn is_raw_mode(master: &OwnedFd) -> bool {
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `master.as_raw_fd()` is a valid, open fd for the duration of
    // this call; `termios` is a valid, appropriately-sized out-param.
    let rc = unsafe { libc::tcgetattr(master.as_raw_fd(), &mut termios) };
    assert_eq!(rc, 0, "tcgetattr on the pty master failed: {}", std::io::Error::last_os_error());
    (termios.c_lflag & (libc::ICANON | libc::ECHO)) == 0
}

async fn wait_until<F: Fn() -> bool>(timeout: Duration, poll_interval: Duration, condition: F) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(poll_interval).await;
    }
    condition()
}

/// Regression test for bug (1): once `isekai-pipe tty attach` has had a
/// moment to start up, its own pty (standing in for the one `sshd` would
/// allocate) must be in raw mode — `ICANON`/`ECHO` both clear — not left at
/// the kernel's cooked-mode default `openpty` starts every fresh pty at.
#[tokio::test]
async fn tty_attach_puts_its_own_pty_into_raw_mode() {
    let home = tempfile::tempdir().unwrap();
    let name = "e2e-rawmode";
    let (master, slave_fd) = open_pty_pair();

    let mut attach = std::process::Command::new(isekai_pipe_bin_path())
        .args(["tty", "attach", name])
        .env("HOME", home.path())
        .env("TERM", "xterm-256color")
        .stdin(dup_stdio(slave_fd))
        .stdout(dup_stdio(slave_fd))
        .stderr(dup_stdio(slave_fd))
        .spawn()
        .expect("failed to spawn `isekai-pipe tty attach` with a real pty");
    // This test's own copy of the slave fd must close too, same reason
    // `pty.rs::spawn` closes it in the parent: only the child's dup'd
    // copies should keep the slave end alive.
    unsafe { libc::close(slave_fd) };

    let became_raw = wait_until(Duration::from_secs(5), Duration::from_millis(50), || is_raw_mode(&master)).await;
    assert!(became_raw, "isekai-pipe tty attach must put its own pty into raw mode (ICANON/ECHO both clear) shortly after starting");

    // Cleanup: send `exit` through the pty exactly like a real user would,
    // then reap the process so this test doesn't leak it.
    // SAFETY: `master`'s fd is valid and open; `b"exit\n"` is a valid
    // buffer for the duration of this one `write` call.
    let cmd = b"exit\n";
    let n = unsafe { libc::write(master.as_raw_fd(), cmd.as_ptr() as *const libc::c_void, cmd.len()) };
    assert_eq!(n, cmd.len() as isize, "write to the pty master failed: {}", std::io::Error::last_os_error());
    let exited = tokio::time::timeout(Duration::from_secs(5), tokio::task::spawn_blocking(move || attach.wait())).await;
    assert!(exited.is_ok(), "isekai-pipe tty attach must exit after `exit` even in the raw-mode-only cleanup path");
}

/// Regression test for bugs (2) and (3): after the remote shell exits
/// cleanly, `isekai-pipe tty attach` itself must actually terminate — not
/// hang forever (bug 2) — and must do so via a clean `Frame::Exit`, not an
/// uncleanly-dropped connection (bug 3, surfaced as a
/// "closed unexpectedly" stderr message and — since `daemon.rs`'s
/// `exit_status.code().unwrap_or(1)` fallback only applies when the real
/// exit status truly is unavailable, which an uncleanly dropped connection
/// on the `attach` side reports as generic failure — the wrong exit code).
///
/// Uses piped (non-tty) stdio deliberately, unlike the raw-mode test above:
/// raw mode itself is irrelevant to this bug pair (`RawModeGuard::enable()`
/// is designed to no-op gracefully on non-tty stdin — see its own doc
/// comment), and a plain pipe makes asserting on stdout content trivial.
#[tokio::test]
async fn tty_attach_exits_promptly_and_cleanly_after_shell_exit() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let home = tempfile::tempdir().unwrap();
    let name = "e2e-exit-promptly";

    let mut attach = tokio::process::Command::new(isekai_pipe_bin_path())
        .args(["tty", "attach", name])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn `isekai-pipe tty attach`");

    let mut stdin = attach.stdin.take().expect("stdin must be piped");
    stdin.write_all(b"exit 0\n").await.expect("failed to write `exit 0` to tty attach's stdin");
    drop(stdin);

    // The decisive assertion for bug (2): this must not time out. Before
    // the `std::process::exit` fix, this hung indefinitely (the process
    // never actually terminated even though the shell — and the daemon —
    // had already exited).
    let wait_result = tokio::time::timeout(Duration::from_secs(5), attach.wait())
        .await
        .expect("isekai-pipe tty attach must exit within 5s of the remote shell exiting cleanly (regression: it used to hang forever — see attach.rs's std::process::exit fix)")
        .expect("wait() on the child itself failed");

    let mut stderr = String::new();
    attach.stderr.take().expect("stderr must be piped").read_to_string(&mut stderr).await.expect("failed to read stderr");

    // The decisive assertion for bug (3): a clean exit must not look like a
    // dropped connection. Before the `daemon.rs` grace-sleep fix, this
    // message appeared on the vast majority of runs even though the shell
    // itself had exited successfully.
    assert!(
        !stderr.contains("closed unexpectedly"),
        "a clean shell exit must not be reported as the daemon connection dropping unexpectedly (regression: notify_exit/process-teardown race — see daemon.rs's EXIT_NOTIFY_GRACE fix); stderr was: {stderr:?}"
    );
    assert!(wait_result.success(), "`exit 0` in the remote shell must produce a successful exit status end-to-end, got {wait_result:?} (stderr: {stderr:?})");
}
