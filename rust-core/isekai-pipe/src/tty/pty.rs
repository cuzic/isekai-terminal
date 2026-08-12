//! Spawns the pty-attached shell (or given command) `isekai-pipe tty daemon`
//! owns.
//!
//! **Never `fork()`/`forkpty()` directly in this process** — this binary
//! runs a multi-threaded Tokio runtime (`main.rs`'s
//! `tokio::runtime::Builder::new_multi_thread()`), and `forkpty()`/raw
//! `fork()` in a multi-threaded process is a well-known hazard: the child
//! only inherits the forking thread, but any lock another thread held at
//! fork time (the allocator's internal lock, a logger's, etc.) stays held
//! forever in the child, so the very next allocation/log call the child
//! makes can deadlock. `openpty(3)` is called here, in the parent, before
//! any fork happens at all; `std::process::Command::spawn` then does the
//! actual `fork`+`exec` via the standard library's own already-correct
//! implementation, with a `pre_exec` hook whose body is a *single*
//! async-signal-safe call (`libc::login_tty`) — no allocation, no `Rust`
//! `format!`, nothing that could touch a lock the parent held at fork time.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use tokio::io::unix::AsyncFd;

/// The pty master end — the only handle this process keeps open past spawn
/// time (the slave fd is closed in the parent immediately after `spawn`,
/// see that function's doc comment on why). Deliberately returned as a
/// separate value from the spawned `Child` (a tuple, not one bundling
/// struct) rather than a single struct owning both: `daemon.rs` needs to
/// `.wait()` the child (a blocking call, run on `spawn_blocking`, which
/// needs to *own* the `Child` it moves into that closure) concurrently with
/// this process continuing to read/write the master end — bundling both
/// into one struct would force an awkward partial-move to separate them
/// back out at the one call site that actually needs them apart.
pub(crate) struct PtyMaster {
    fd: AsyncFd<OwnedFd>,
}

/// Spawns `command` (its own argv\[0\] plus args) with a fresh pty as its
/// controlling terminal, sized `cols`x`rows`, with `$TERM` set to `term`.
///
/// The child's controlling-terminal setup happens entirely inside
/// `libc::login_tty` (`setsid` + `TIOCSCTTY` + `dup2` fd 0/1/2 + close if
/// the slave fd is >2, all in one glibc call) run from `pre_exec` — this
/// requires the *daemon process itself* (the caller of this function) to
/// have no controlling terminal of its own already (a process can only
/// acquire a new one via `TIOCSCTTY` as a session leader with none set) —
/// see `daemon.rs`'s own `setsid`/detach step, which must run before this
/// function is ever called.
///
/// `ctl_sock_file`, when given, is exported to the child as
/// `$ISEKAI_TTY_CTL_SOCK_FILE` — see `daemon.rs::run`'s doc comment on why
/// this indirection (a *path*, set once and never changing for this
/// daemon's whole lifetime) exists instead of exporting the ctl-socket
/// value itself the way an ordinary login shell does.
pub(crate) fn spawn(command: &[String], term: &str, cols: u16, rows: u16, ctl_sock_file: Option<&Path>) -> io::Result<(PtyMaster, Child)> {
    let (program, args) = command.split_first().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "spawn: empty command"))?;

    let mut master_fd: libc::c_int = -1;
    let mut slave_fd: libc::c_int = -1;
    let mut winsize = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: `openpty` writes valid, newly-opened fds into `master_fd`/
    // `slave_fd` on success (return 0); `name`/`termp` null is documented
    // as accepted (uses OS defaults for the device name / termios). All
    // three out-params are passed as `*mut` (not `*const`) even where glibc
    // itself only declares `termp`/`winp` as `const` — Apple's `libc` crate
    // declares every `openpty` parameter `*mut`, and `*mut T` coerces to
    // `*const T` implicitly at the call site, so one `*mut`-everywhere call
    // is portable to both without a `cfg` split (found via real macOS CI:
    // the `*const`/`&winsize` version this replaced compiled on Linux but
    // failed E0308 on macOS).
    let rc = unsafe { libc::openpty(&mut master_fd, &mut slave_fd, std::ptr::null_mut(), std::ptr::null_mut(), &mut winsize) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `master_fd` was just returned by `openpty` as a valid, open,
    // uniquely-owned descriptor.
    let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
    set_nonblocking(master.as_raw_fd())?;

    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.env("TERM", term);
    if let Some(ctl_sock_file) = ctl_sock_file {
        cmd.env("ISEKAI_TTY_CTL_SOCK_FILE", ctl_sock_file);
    }
    // Whatever these are set to is overwritten by `login_tty`'s own
    // dup2(slave_fd, 0/1/2) inside `pre_exec` below — `null()` here only
    // covers the brief window between fork and that call, so a `pre_exec`
    // failure (which aborts the exec and reports back to this process as a
    // spawn error) never leaves the child briefly holding this process's
    // real stdio.
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    // SAFETY: this closure runs in the forked child before exec, so it must
    // be async-signal-safe. Its body is exactly one libc call
    // (`login_tty`) — no allocation, no locking, no Rust runtime machinery —
    // satisfying `pre_exec`'s safety contract. `slave_fd` is `Copy` (a raw
    // `c_int`), so the closure captures it by value; nothing borrowed from
    // the parent process's heap crosses into the child through it.
    unsafe {
        cmd.pre_exec(move || {
            if libc::login_tty(slave_fd) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn()?;

    // Close the slave fd in the parent: the child has its own copy (via
    // `login_tty`'s internal `dup2`), and this process has no further use
    // for it. Holding it open here would mean the slave end never has its
    // *last* reference closed when the child exits, so `master`'s read loop
    // would never see the EOF/EIO that signals "the child is gone."
    unsafe { libc::close(slave_fd) };

    Ok((PtyMaster { fd: AsyncFd::new(master)? }, child))
}

#[cfg(unix)]
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is a valid, open descriptor for the duration of this
    // call (the caller retains ownership past this call).
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: same fd, setting a flag `fcntl` itself defines as safe to OR
    // into an already-valid flag set.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl PtyMaster {
    /// Reads once from the pty master into `buf`, `await`ing readiness via
    /// `AsyncFd` rather than blocking a Tokio worker thread on a `File`
    /// read. Returns `Ok(0)` on EOF — **including `EIO`**, which is what a
    /// non-blocking pty master read returns once the slave side has no more
    /// open references (the child exited and this process already closed
    /// its own slave fd copy at spawn time) rather than the `0`-byte read a
    /// regular file/pipe would give; `Ok(0)` lets every caller treat both
    /// the same way without needing to know this pty-specific quirk.
    pub(crate) async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.fd.readable().await?;
            match guard.try_io(|inner| {
                // SAFETY: `inner.as_raw_fd()` is the pty master fd, open for
                // the lifetime of `self`; `buf` is a valid, appropriately
                // sized, exclusively-borrowed buffer for the duration of
                // this single `read(2)` call.
                let n = unsafe { libc::read(inner.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n >= 0 {
                    Ok(n as usize)
                } else {
                    Err(io::Error::last_os_error())
                }
            }) {
                Ok(Ok(n)) => return Ok(n),
                Ok(Err(e)) if e.raw_os_error() == Some(libc::EIO) => return Ok(0),
                Ok(Err(e)) => return Err(e),
                // `try_io` itself returns `Err(WouldBlock-as-tried-again)`
                // when the readiness event was stale (another task raced us
                // to the data) — `AsyncFd`'s documented retry contract, not
                // an actual I/O error.
                Err(_would_block) => continue,
            }
        }
    }

    /// Writes `buf` to the pty master in full, `await`ing writability the
    /// same way [`Self::read`] awaits readability.
    pub(crate) async fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let mut guard = self.fd.writable().await?;
            match guard.try_io(|inner| {
                // SAFETY: same contract as `read`'s, for `write(2)`.
                let n = unsafe { libc::write(inner.as_raw_fd(), buf.as_ptr() as *const libc::c_void, buf.len()) };
                if n >= 0 {
                    Ok(n as usize)
                } else {
                    Err(io::Error::last_os_error())
                }
            }) {
                Ok(Ok(n)) => buf = &buf[n..],
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    /// Applies a new terminal window size via `TIOCSWINSZ` — the pty
    /// equivalent of a local terminal emulator resizing its own window, so
    /// full-screen programs in the shell (`$PAGER`, `vim`, …) redraw
    /// correctly instead of wrapping at the pty's original `openpty` size
    /// forever.
    pub(crate) fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let winsize = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: `self.fd`'s fd is open for the lifetime of `self`;
        // `TIOCSWINSZ` only reads `winsize`, which is a valid, fully
        // initialized, stack-local value for the duration of this call.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ, &winsize) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
