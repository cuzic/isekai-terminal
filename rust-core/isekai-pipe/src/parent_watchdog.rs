//! Detects that this process's local peer — normally `ssh(1)`'s
//! `ProxyCommand`, which owns the other end of our stdin/stdout — is gone,
//! even when nothing is currently being read or written. Task 2.9 / issue
//! #111 (`ADR_MIDSESSION_DISCONNECT_RECOVERY.md` Round 6/7): `ssh(1)` does
//! not reliably reap this process when it is killed externally
//! (`SIGTERM`/`SIGKILL` — confirmed by direct experiment, 2026-09-02), so
//! without an active signal, an orphaned `isekai-pipe connect` can spend up
//! to the full server-granted resume window (10 days by default on the
//! relay route) redialing and replaying bytes into a stdout nobody reads.
//!
//! `resume_loop::PumpFailure`'s `Local`/`Remote` split (and the EOF-latch
//! next to it) catch this reactively, as a side effect of an actual read or
//! write attempt — but a quiet session (nothing flowing either direction)
//! never attempts either, so those mechanisms can't fire while this process
//! sits blocked in `resume_with_backoff_until_deadline` or an initial
//! dial/handshake. This module closes that gap directly, instead of
//! extending the reactive approach further.
//!
//! ## Why a blocking `poll()`, not `prctl(PR_SET_PDEATHSIG)`/`kqueue`
//!
//! An earlier design considered forcing `isekai-pipe connect` to become
//! `ssh(1)`'s literal direct child (prepending `exec` to the `ProxyCommand`
//! string) so a kernel parent-death primitive (`PR_SET_PDEATHSIG` on Linux,
//! `kqueue`/`EVFILT_PROC`/`NOTE_EXIT` on macOS) could be armed against it.
//! Adversarial review (2 rounds, 2 independent reviewers, with direct
//! experiments run by both) found:
//!
//! - `ssh(1)` **already** execs `ProxyCommand` via `$SHELL -c "exec <cmd>"`
//!   — `ssh_config(5)`: "the command string ... is executed using the
//!   user's shell 'exec' directive to avoid a lingering shell process".
//!   `isekai-pipe connect` is already `ssh(1)`'s direct, PID-stable child
//!   today, with no wrapper change needed. Prepending a *second* `exec` (as
//!   the rejected design proposed) produces `exec exec <cmd>`, which every
//!   POSIX shell tested (`dash`, `bash`) fails immediately with
//!   `exec: exec: not found` (`exec` is a special builtin, so the outer
//!   `exec` does a `PATH` lookup for a program literally named `exec`) —
//!   this would have broken every Unix connection. See
//!   `isekai-ssh/src/wrapper.rs::proxy_command`'s doc comment for the
//!   corresponding one-line invariant this leaves behind (the command
//!   string must stay a single simple command, or the implicit `exec`
//!   guarantee is lost).
//! - Even with that premise corrected, `PR_SET_PDEATHSIG`/`kqueue` would
//!   still need: two divergent platform-specific implementations (Linux vs.
//!   macOS, the latter unverified in this repo's CI at review time); an
//!   arm-time race (the parent can already be dead before the primitive is
//!   armed) that needs an explicit guard — and a real, independently-found
//!   bug in the obvious form of that guard: an orphan reparents to the
//!   nearest **child subreaper** (e.g. `systemd --user`), not PID 1, so
//!   `getppid() == 1` silently never fires; and `SIGTERM`'s default action
//!   terminates abruptly, skipping every graceful-shutdown `Drop` — which
//!   reintroduces exactly the problem `run_resume_loop`'s explicit
//!   `quic_write.reset(0)` vs. clean-FIN distinction exists to avoid (an
//!   abrupt kill here would make the *server* park the session for the
//!   full resume grace instead of tearing it down immediately).
//!
//! A blocking `poll()` sidesteps all of this: it is level-triggered, so
//! there is no arm-time race to guard in the first place (if the peer is
//! already gone when `poll()` is called, it returns immediately); it tests
//! the actual predicate this process cares about — "is anyone still on the
//! other end of my stdio" — rather than a proxy for it ("did one specific
//! PID exit"), so it also catches the rarer case where `ssh(1)` itself is
//! still alive but this specific pipe broke; it needs exactly one identical
//! implementation for Linux, macOS, and every BSD (all expose the same
//! POSIX `poll(2)`); and firing it wakes ordinary async code via a channel,
//! which can run a normal graceful shutdown instead of fighting
//! async-signal-safety inside a signal handler.
//!
//! ## Why fd 0 *and* fd 1, and why `events: 0`
//!
//! `ssh(1)` gives its `ProxyCommand` two independent `pipe(2)`s (confirmed
//! by direct experiment, 2026-09-02: different inodes, not a shared
//! `socketpair(2)` — an earlier draft of this analysis wrongly assumed the
//! latter, generalizing from `ssh_proxy_fdpass_connect()`'s `ProxyUseFdpass`
//! path instead of the ordinary `ProxyCommand` path actually used here).
//! Watching both removes any dependency on which specific flag a given
//! platform's `poll()` happens to report for which half:
//!
//! - fd 0 (our stdin, `ssh(1)`'s write end): reports `POLLHUP` once `ssh(1)`
//!   closes its end.
//! - fd 1 (our stdout, `ssh(1)`'s read end): reports `POLLERR` (not
//!   `POLLHUP`) once `ssh(1)` closes its end — measured directly; a
//!   watchdog that only checked `POLLHUP` would silently never fire on this
//!   fd.
//!
//! `events: 0` (not `POLLIN`/`POLLOUT`) is requested on both: `POLLERR`/
//! `POLLHUP`/`POLLNVAL` are always reported in `revents` regardless of the
//! requested mask, and fd 1 (a pipe write end) is `POLLOUT`-ready almost
//! continuously while healthy, so requesting `POLLOUT` would make this
//! thread spin instead of block.
//!
//! ## Why a dedicated thread, not `tokio::io::unix::AsyncFd`
//!
//! `AsyncFd` requires the underlying fd to be `O_NONBLOCK`. Since
//! `O_NONBLOCK` is a property of the *open file description*, not the fd
//! number, setting it on fd 1 for this watchdog would make
//! `resume_loop::pump_h2c`'s real `stdout` writes non-blocking too —
//! surfacing a transient `WouldBlock` as a spurious `PumpFailure::Local`
//! and exiting a healthy session on a partial write. A plain blocking
//! `std::thread` avoids the question entirely.

use tokio::sync::watch;

/// Fires exactly once, when this process's local peer is gone. Clone
/// freely — every clone observes the same one-shot transition via
/// [`watch::Receiver::changed`]. On non-Unix targets (see [`spawn`]) this
/// simply never fires; callers must not treat that as a guarantee the peer
/// is alive, only as "no additional signal is available on this platform"
/// (the `resume_loop` EOF-latch is the fallback there).
pub(crate) type ParentGoneWatch = watch::Receiver<bool>;

/// Waits for the watchdog to fire — resolves once `*rx.borrow() == true`,
/// never before.
///
/// Deliberately **not** `let _ = rx.changed().await;` (an earlier, wrong
/// draft — found by adversarial review, 2026-09-02): `changed()` also
/// returns (as `Err`) the instant every sender is dropped, which happens on
/// two reachable paths that mean the *opposite* of "the parent is gone" —
/// `spawn`'s watchdog thread failing to start at all (fd/memory exhaustion),
/// and `watch_loop`'s own `poll()` call failing unexpectedly and giving up.
/// Both drop `tx` without ever sending `true`. Treating that the same as a
/// real fire made `connect_command`'s `select!` abort the connection
/// immediately in the first case, and abort a healthy mid-session pump in
/// the second — exactly backwards from those paths' own "fail open, fall
/// back to reactive detection" comments. Looping on `changed()` and checking
/// the actual value, then pending forever once the channel closes without
/// ever having fired, fixes both: a closed-without-firing channel now
/// correctly means "no additional signal available," not "parent gone."
pub(crate) async fn wait(rx: &mut ParentGoneWatch) {
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await
}

#[cfg(unix)]
pub(crate) fn spawn() -> ParentGoneWatch {
    let (tx, rx) = watch::channel(false);
    // Deliberately not joined/cancelled anywhere: the process either exits
    // normally (the OS tears down every thread, this one included) or this
    // thread itself drives that exit by sending on `tx` — there is no
    // third case where anything still needs to wait on it.
    let spawned = std::thread::Builder::new().name("parent-watchdog".to_string()).spawn(move || watch_loop(tx));
    if spawned.is_err() {
        // Spawning a plain OS thread failing at all is itself a sign of a
        // process in serious trouble (fd/memory exhaustion) — fail open:
        // the reactive PumpFailure/EOF-latch mechanisms remain as a
        // fallback, same as the non-Unix stub below.
        log::warn!("isekai-pipe connect: failed to spawn the parent-liveness watchdog thread; falling back to reactive detection only");
    }
    rx
}

#[cfg(unix)]
fn watch_loop(tx: watch::Sender<bool>) {
    const TERMINAL: libc::c_short = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;
    loop {
        let mut fds = [libc::pollfd { fd: 0, events: 0, revents: 0 }, libc::pollfd { fd: 1, events: 0, revents: 0 }];
        // SAFETY: `fds` is a valid, correctly-sized array for the duration
        // of this call; `poll(2)` only reads/writes through the pointer we
        // give it.
        let ret = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // An unexpected `poll()` failure leaves this watchdog unable to
            // usefully continue; give up quietly rather than busy-loop on a
            // condition that keeps recurring. The reactive mechanisms
            // remain as a fallback.
            log::warn!("isekai-pipe connect: parent-liveness watchdog poll() failed unexpectedly, giving up: {err}");
            return;
        }
        if fds.iter().any(|pfd| pfd.revents & TERMINAL != 0) {
            let _ = tx.send(true);
            return;
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn spawn() -> ParentGoneWatch {
    // No `poll()`-equivalent this module implements on non-Unix targets
    // today (see module docs) — dropping `tx` immediately closes the
    // channel, which `wait` now correctly treats as "no signal available,
    // never fires" rather than a real fire (see `wait`'s docs, N1). This is
    // the MSYS2/Cygwin-hosted `ssh.exe` case in practice: the spawned
    // `isekai-pipe connect` there is a native Windows binary with no
    // `poll(2)`, so `resume_loop`'s EOF-latch is the only signal available.
    let (_tx, rx) = watch::channel(false);
    rx
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Regression test for the module's core claim: closing the *far* end
    /// of a pipe this process holds open must make the watchdog fire, even
    /// though nothing is reading or writing at the time. Doesn't touch the
    /// real fd 0/1 (would require subprocess plumbing) — exercises the same
    /// `poll()` logic directly against a throwaway pipe pair instead.
    #[test]
    fn watch_loop_fires_when_the_peer_closes_its_end() {
        let (read_fd, write_fd) = {
            let mut fds = [0i32; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            (fds[0], fds[1])
        };
        let (tx, mut rx) = watch::channel(false);
        let handle = std::thread::spawn(move || {
            const TERMINAL: libc::c_short = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;
            loop {
                let mut fds = [libc::pollfd { fd: read_fd, events: 0, revents: 0 }];
                let ret = unsafe { libc::poll(fds.as_mut_ptr(), 1, -1) };
                if ret < 0 {
                    continue;
                }
                if fds[0].revents & TERMINAL != 0 {
                    let _ = tx.send(true);
                    return;
                }
            }
        });

        // Closing the write end is what an externally-killed `ssh(1)`
        // effectively does to this process's stdin.
        unsafe {
            libc::close(write_fd);
        }

        let fired = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(5), rx.changed()).await })
        })
        .join()
        .unwrap();
        assert!(fired.is_ok(), "the watchdog must fire promptly once the peer closes its end");
        handle.join().unwrap();
        unsafe {
            libc::close(read_fd);
        }
    }
}
