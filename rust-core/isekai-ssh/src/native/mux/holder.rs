//! Spawns and identifies the detached `ControlPersist`-equivalent holder
//! process: a background `isekai-ssh` invocation with **no foreground shell
//! of its own** (see [`super::run_as_holder`]), started by the foreground
//! `dispatch` path the moment it finds no existing holder to connect to for a
//! destination.
//!
//! Real detachment (`DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP`)
//! is Windows-only and untestable on Linux CI, so the actual spawn mechanics
//! sit behind the injectable [`HolderSpawner`] trait — `dispatch`'s
//! retry/fallback *sequencing* around it is unit-tested here against a fake
//! spawner, on every platform.
//!
//! **Passphrase hand-off (Phase 1b)**: a detached holder has no console, so it
//! can never prompt for a passphrase-protected identity's passphrase itself.
//! When the *spawning* client already decrypted one (because it prompted
//! interactively before discovering it needed to spawn a holder), it hands the
//! cleartext PEM to the holder over the holder's own stdin pipe — never via
//! argv or an environment variable — by passing `Some(handoff_bytes)` to
//! [`HolderSpawner::spawn`]. `None` means "no hand-off needed" (no
//! passphrase-protected identity is in play) and the child's stdin is left
//! null.

use std::io;

/// The env var marking a re-exec'd process as the detached holder rather than
/// an ordinary foreground `isekai-ssh` invocation — checked once, at the very
/// top of `main.rs`, before any normal argv parsing/dispatch happens. An env
/// var (not a hidden CLI flag) so the holder's argv is *exactly* the original
/// destination args the foreground process was invoked with — the same
/// `Prepared` must resolve to the same channel name on both sides.
const HOLDER_MARKER_ENV: &str = "ISEKAI_SSH_MUX_HOLDER";

/// True iff this process was itself re-exec'd to become a detached holder
/// (checked by `main.rs` before its normal dispatch).
pub(crate) fn is_holder_reexec() -> bool {
    std::env::var_os(HOLDER_MARKER_ENV).is_some()
}

/// Spawns a background copy of the current executable with the given argv,
/// marked via [`HOLDER_MARKER_ENV`] so it takes the holder path instead of
/// ordinary dispatch. Injected so `dispatch`'s retry/fallback sequencing
/// around it is testable without really spawning a detached process.
pub(crate) trait HolderSpawner {
    /// `handoff`, when `Some`, is written to the spawned process's stdin and
    /// the pipe is then closed (EOF) before this call returns — the holder
    /// reads it before claiming the channel. `None` leaves stdin null.
    fn spawn(&self, args: &[String], handoff: Option<&[u8]>) -> io::Result<()>;
}

/// The real spawner: self-re-execs [`std::env::current_exe`] fully detached
/// from this process's console/job — so it outlives this tab even after this
/// tab's own shell exits (the entire point of the `ControlPersist`-equivalent
/// redesign; see `super`'s module docs).
pub(crate) struct DetachedProcessSpawner;

#[cfg(windows)]
impl HolderSpawner for DetachedProcessSpawner {
    fn spawn(&self, args: &[String], handoff: Option<&[u8]>) -> io::Result<()> {
        use std::io::Write as _;
        use std::os::windows::process::CommandExt as _;

        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let exe = std::env::current_exe()?;
        let mut command = std::process::Command::new(exe);
        command
            .args(args)
            .env(HOLDER_MARKER_ENV, "1")
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command.stdin(if handoff.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() });

        let mut child = command.spawn()?;
        if let Some(bytes) = handoff {
            // Write on a dedicated OS thread rather than blocking `spawn`'s
            // caller (a `dispatch`/`tokio` task) on `write_all`: a named
            // pipe's kernel buffer is a few KB, and a large hand-off (an
            // RSA-4096 key, a key+certificate, or several identities) can
            // exceed that — `write_all` would then block until the holder
            // actually reads enough to make room, which only happens *after*
            // its own `connect::prepare` (trust-store lookup, possibly a
            // network re-deploy) finishes reading it (see
            // `run_as_holder_entrypoint`'s docs). Detaching the write means a
            // slow holder start-up delays only *this* thread, never the
            // caller — the caller has already moved on to
            // `connect_with_retry` by the time this thread even starts.
            let bytes = bytes.to_vec();
            let mut stdin = child.stdin.take().expect("stdin was requested as piped above");
            std::thread::spawn(move || {
                // Write-then-drop: the holder's read side sees EOF right
                // after its handoff payload. Nothing to do with a failed
                // write here — the holder simply reads an incomplete/no
                // payload and decodes it as (or falls back to) an empty
                // hand-off set, same as if none had been sent.
                let _ = stdin.write_all(&bytes);
            });
        }
        // Deliberately never `wait()`/`kill()`: dropping `Child` does not
        // terminate the process (only an explicit `kill()` would) — this is
        // exactly the detachment the `ControlPersist`-equivalent design needs.
        drop(child);
        Ok(())
    }
}

#[cfg(not(windows))]
impl HolderSpawner for DetachedProcessSpawner {
    fn spawn(&self, _args: &[String], _handoff: Option<&[u8]>) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "detached mux holder spawning is Windows-only"))
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    //! A fake [`HolderSpawner`] for exercising `dispatch`'s sequencing (spawn
    //! failure, spawn success but the holder never comes up, spawn success and
    //! the holder comes up) without any real process ever starting — used
    //! from `super::tests`.
    use super::HolderSpawner;
    use std::io;
    use std::sync::Mutex;

    pub(crate) struct RecordingSpawner {
        pub(crate) result: Mutex<Option<io::Result<()>>>,
        pub(crate) calls: Mutex<Vec<(Vec<String>, Option<Vec<u8>>)>>,
    }

    impl RecordingSpawner {
        pub(crate) fn succeeding() -> Self {
            Self { result: Mutex::new(Some(Ok(()))), calls: Mutex::new(Vec::new()) }
        }

        pub(crate) fn failing() -> Self {
            Self {
                result: Mutex::new(Some(Err(io::Error::new(io::ErrorKind::Other, "spawn failed for test")))),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl HolderSpawner for RecordingSpawner {
        fn spawn(&self, args: &[String], handoff: Option<&[u8]>) -> io::Result<()> {
            self.calls.lock().unwrap().push((args.to_vec(), handoff.map(|b| b.to_vec())));
            self.result.lock().unwrap().take().expect("spawn called more than once in this test").map(|()| ())
        }
    }
}
