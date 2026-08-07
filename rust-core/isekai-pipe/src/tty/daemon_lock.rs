//! Exact, race-free "is a `tty-daemon` for this name currently alive"
//! detection, via an exclusive `flock(2)` on a sidecar `<name>.lock` file
//! held by the daemon for its **entire process life** — not the
//! scoped-to-one-critical-section pattern
//! `isekai_fs_guard::with_exclusive_lock` uses elsewhere in this codebase
//! (that one releases as soon as the passed closure returns, which is the
//! wrong shape for "stay locked for as long as I'm running").
//!
//! A design review rejected porting `native/mux/mod.rs`'s `SpawnLock`
//! (mtime-based staleness, a heuristic that exists only because that
//! feature's Windows named-pipe world lacks a better primitive) — `flock`
//! gives an *exact* answer here: the kernel releases it unconditionally the
//! instant the holding file descriptor closes, including on `SIGKILL`, so
//! "can I acquire this lock right now" is never a guess.
//!
//! **How the race between two concurrent `tty attach` invocations spawning
//! a daemon for the same name resolves**: deliberately *not* solved by
//! having `attach` acquire-then-hand-off the lock to the daemon it spawns
//! (that would need passing an open fd across an `exec` boundary, real but
//! avoidable complexity) — instead, `attach` always just fires off a
//! `spawn_detached` unconditionally on a failed connect (see `attach.rs`),
//! and **every** resulting `tty-daemon` process independently calls
//! [`DaemonLock::try_acquire`] as its own first action (`daemon.rs`). The
//! kernel's `flock` is what's actually atomic between independent
//! processes; whichever daemon process wins proceeds normally, and every
//! loser observes `Ok(None)` and exits immediately (an expected, silent,
//! zero-cost outcome — "someone else is already handling this name" is not
//! an error). `attach` simply retries its own connect with a short bounded
//! backoff until whichever daemon actually won is listening.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};

pub(crate) struct DaemonLock {
    _file: File,
}

impl DaemonLock {
    /// Tries to become *the* daemon for `name` without blocking.
    /// `Ok(Some(lock))`: no other daemon currently holds this name — the
    /// caller now exclusively owns it for as long as `lock` (and therefore
    /// the underlying fd) stays alive, which in practice means "until this
    /// process exits." `Ok(None)`: another (live) daemon already holds it —
    /// the caller should exit immediately, not treat this as an error.
    pub(crate) fn try_acquire(dir: &Path, name: &str) -> io::Result<Option<Self>> {
        let path = lock_path(dir, name);
        // This file's content is never read or written — it exists purely
        // to be locked — so `truncate(false)` avoids pointlessly rewriting
        // it on every attempt; it has no correctness bearing either way.
        let file = OpenOptions::new().create(true).write(true).truncate(false).open(&path)?;
        // SAFETY: `file.as_raw_fd()` is a valid, open fd for the duration of
        // this call (`file` outlives it), matching `flock(2)`'s contract.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            Ok(Some(Self { _file: file }))
        } else {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(err)
            }
        }
    }
}

fn lock_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_acquirer_gets_the_lock_second_concurrent_one_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let first = DaemonLock::try_acquire(dir.path(), "work").unwrap();
        assert!(first.is_some());

        let second = DaemonLock::try_acquire(dir.path(), "work").unwrap();
        assert!(second.is_none(), "a second concurrent acquirer for the same name must not also succeed");
    }

    #[test]
    fn releasing_the_lock_lets_a_later_acquirer_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let first = DaemonLock::try_acquire(dir.path(), "work").unwrap();
        drop(first);

        let second = DaemonLock::try_acquire(dir.path(), "work").unwrap();
        assert!(second.is_some(), "the lock must be released (and reacquirable) once the holder drops");
    }

    #[test]
    fn different_names_do_not_contend() {
        let dir = tempfile::tempdir().unwrap();
        let a = DaemonLock::try_acquire(dir.path(), "work").unwrap();
        let b = DaemonLock::try_acquire(dir.path(), "personal").unwrap();
        assert!(a.is_some());
        assert!(b.is_some());
    }
}
