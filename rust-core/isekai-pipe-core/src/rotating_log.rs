//! Small, dependency-free rotating append log used by long-lived helper
//! processes.
//!
//! `isekai-pipe connect` originally owned this type because PR #116 only
//! needed runtime rotation for that process's `env_logger` output. The
//! Windows-native `isekai-ssh` mux holder has the same lifetime problem:
//! it can live for days or weeks while its stderr is intentionally detached,
//! so diagnostics need bounded growth without relying on a process restart.
//! Keeping the implementation here lets both binaries use the exact same
//! rotation contract without introducing a logging framework dependency into
//! either side.
//!
//! Deliberately hand-rolled instead of pulling in `tracing-appender`: this
//! crate (and `isekai-pipe`, `isekai-ssh`) intentionally stays thin
//! (`ADR_ISEKAI_SSH_LOCAL_SCROLLBACK.md`'s "isekai-pipe should stay thin"
//! decision) and `tracing-appender` would drag in the whole
//! `tracing-subscriber` dependency graph just to be used as a `Write` impl.
//!
//! `isekai-ssh/src/log_file.rs`'s own `Sink` (`OnceLock<Mutex<File>>` with
//! an open-time `truncate_over` check) is a different, deliberately simpler
//! mechanism, not reused here: it only ever judges size once, at `open()`
//! (i.e. once per process start), and would never fire again for a process
//! that itself never restarts — exactly wrong for a holder that can run for
//! days without restarting. Reusing `Sink` directly would need a
//! `truncate_over`-at-write-time mode `isekai-ssh`'s own callers have no use
//! for, so the two stay separate types even though both now live behind
//! `isekai-ssh/src/log_file.rs`'s three process-wide sinks.

use std::io::Write as _;

/// Rotates once a log file this process writes reaches this size.
///
/// The value intentionally matches `isekai-ssh/src/log_file.rs`'s historical
/// default verbose-log cap and the PR #116 `isekai-pipe connect` behavior:
/// large enough to preserve useful reconnect history, small enough that a
/// forgotten holder log does not grow without bound.
pub const LOG_ROTATE_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// A `std::io::Write` target that rotates by renaming the current file to
/// `<name>.1` (overwriting any older `.1` -- a single backup generation) once
/// it has accumulated at least [`LOG_ROTATE_MAX_BYTES`], then opens a fresh
/// file at the original path and keeps writing.
///
/// This is deliberately tiny and fail-open. Rotation errors are swallowed by
/// [`Write::write`](std::io::Write::write): if the rename fails because the
/// filesystem is read-only, a handle is locked, or an antivirus scanner is
/// briefly in the way on Windows, the caller keeps appending to the current
/// over-threshold file rather than losing diagnostics or failing the actual
/// SSH/QUIC connection. Ordinary write/flush errors still surface through
/// the `Write` trait because `env_logger` and the process-wide `isekai-ssh`
/// holder sink both already treat logging as best-effort.
pub struct RotatingLogFile {
    path: std::path::PathBuf,
    file: std::fs::File,
    written: u64,
}

impl RotatingLogFile {
    /// Opens (creating parent dirs as needed) `path`, appending to any
    /// existing content, and seeds the internal byte counter from the file's
    /// current size.
    ///
    /// Seeding from metadata matters for holder-style processes: a new
    /// process may inherit a file that is already near or over the threshold
    /// from an earlier run. Counting only bytes written after this `open`
    /// would defer rotation by another full 5 MiB and make the cap much less
    /// predictable in the exact "diagnostics after many reconnects" scenario
    /// these files exist for.
    pub fn open(path: std::path::PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path,
            file,
            written,
        })
    }

    /// Renames the current file to `<name>.1` and reopens a fresh file at
    /// `self.path`. The caller intentionally decides whether a failed
    /// rotation is fatal; [`Write::write`](std::io::Write::write) treats it
    /// as non-fatal so logging cannot break the connection path.
    fn rotate(&mut self) -> std::io::Result<()> {
        let file_name = self.path.file_name().unwrap_or_default();
        let mut rotated_name = file_name.to_os_string();
        rotated_name.push(".1");
        let rotated_path = self.path.with_file_name(rotated_name);
        // `std::fs::rename` atomically replaces an existing `rotated_path`
        // on both Unix (`rename(2)`) and Windows (`MoveFileExW` with
        // `MOVEFILE_REPLACE_EXISTING`), so a separate remove would only turn
        // one atomic operation into a delete-then-rename window.
        std::fs::rename(&self.path, &rotated_path)?;
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl std::io::Write for RotatingLogFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written >= LOG_ROTATE_MAX_BYTES {
            let _ = self.rotate();
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_without_rotating_below_the_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("holder.log");
        let mut log = RotatingLogFile::open(path.clone()).unwrap();
        log.write_all(b"first line\n").unwrap();
        log.write_all(b"second line\n").unwrap();
        drop(log);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "first line\nsecond line\n"
        );
        assert!(
            !path.with_file_name("holder.log.1").exists(),
            "no rotation should have happened yet"
        );
    }

    #[test]
    fn rotates_once_the_threshold_is_crossed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("holder.log");
        // Pre-seed the file past the threshold directly (bypassing the
        // struct) so this test doesn't need to actually write 5MB through
        // it to exercise the rotation branch.
        std::fs::write(&path, vec![b'x'; LOG_ROTATE_MAX_BYTES as usize + 1]).unwrap();

        let mut log = RotatingLogFile::open(path.clone()).unwrap();
        assert_eq!(
            log.written,
            LOG_ROTATE_MAX_BYTES + 1,
            "written must be seeded from the pre-existing file's size"
        );
        log.write_all(b"after rotation\n").unwrap();
        drop(log);

        let rotated_path = path.with_file_name("holder.log.1");
        assert!(
            rotated_path.exists(),
            "the oversized file should have been rotated out to .1"
        );
        assert_eq!(
            std::fs::read(&rotated_path).unwrap().len(),
            LOG_ROTATE_MAX_BYTES as usize + 1
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "after rotation\n",
            "the new file should start fresh"
        );
    }

    #[test]
    fn overwrites_an_older_generation_1_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("holder.log");
        std::fs::write(
            path.with_file_name("holder.log.1"),
            b"stale generation from a previous rotation\n",
        )
        .unwrap();
        std::fs::write(&path, vec![b'x'; LOG_ROTATE_MAX_BYTES as usize + 1]).unwrap();

        let mut log = RotatingLogFile::open(path.clone()).unwrap();
        log.write_all(b"fresh\n").unwrap();
        drop(log);

        assert_eq!(
            std::fs::read(path.with_file_name("holder.log.1"))
                .unwrap()
                .len(),
            LOG_ROTATE_MAX_BYTES as usize + 1
        );
    }
}
