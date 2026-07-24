//! Lazy garbage collection for the control-plane UNIX domain sockets
//! created by `#@isekai ctl-socket yes` (`ISEKAI_PIPE_DESIGN.md` §8 Epic M).
//!
//! No resident sweeper process exists or is planned: both the local side
//! (`isekai-ssh`'s `ctl_forward.rs`, `runtime_dir/ctl/`) and the remote
//! side (`isekai-pipe serve`'s own `/tmp/isekai-pipe-ctl-*.sock` entries —
//! though the streamlocal forward bind itself is `sshd`'s to clean up on a
//! normal disconnect) instead call [`sweep_stale_sockets`] once, right
//! before creating their own next socket, and rely on that being frequent
//! enough in practice (every new tab/connection) to keep the directory
//! from accumulating garbage left behind by abnormal exits (crash,
//! `kill -9`, a network drop that skipped the normal
//! `ssh -O cancel -R`/unlink path).
//!
//! **Plain-ssh gap (closed, `ISEKAI_PIPE_DESIGN.md` §8 Epic M follow-up
//! #3)**: `isekai-pipe serve` starting up used to be the *only* trigger for
//! the remote-side sweep, but a plain-ssh (`isekai-pipe`非経由) session never
//! starts that process on the remote host at all, so its orphaned
//! `/tmp/isekai-pipe-ctl-*.sock` files were never swept by anything.
//! `isekai-pipe ctl` itself (`isekai-pipe/src/ctl.rs::sweep_stale_ctl_sockets_on_remote`)
//! now also sweeps before every invocation — it's the one binary that always
//! runs remotely regardless of topology, since it's what the interactive
//! shell's `$PROMPT_COMMAND`/manual call actually invokes over the
//! ctl-socket forward.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Removes `.sock` files under `dir` whose name starts with `prefix` and
/// that are either definitely abandoned (a `connect()` attempt gets
/// `ConnectionRefused` — nobody is `listen()`ing there anymore) or older
/// than `staleness_threshold` (a fallback for the rarer case where
/// `connect()` fails for some other reason, e.g. a corrupted non-socket
/// file left at that path). Returns the paths actually removed, for
/// logging; a missing `dir` is treated as "nothing to sweep", not an
/// error (the directory may not exist yet on a machine's very first
/// ctl-socket connection).
pub fn sweep_stale_sockets(dir: &Path, prefix: &str, staleness_threshold: Duration) -> io::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(removed),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with(prefix) || !name.ends_with(".sock") {
            continue;
        }
        if is_abandoned(&path) || is_stale_by_mtime(&path, staleness_threshold) {
            if std::fs::remove_file(&path).is_ok() {
                removed.push(path);
            }
        }
    }
    Ok(removed)
}

/// A UNIX domain socket `connect()` never blocks on network round trips
/// (it's purely local), so a plain blocking attempt that we immediately
/// drop is enough to classify "is anyone listening here" without needing
/// non-blocking sockets or a timeout. `ConnectionRefused` specifically
/// means the path exists as a socket but nothing is `listen()`ing on it —
/// unambiguously abandoned. Any other outcome (including a live listener,
/// or the path not being a socket at all) is left to the mtime fallback.
#[cfg(unix)]
fn is_abandoned(path: &Path) -> bool {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => false,
        Err(e) => e.kind() == io::ErrorKind::ConnectionRefused,
    }
}

#[cfg(not(unix))]
fn is_abandoned(_path: &Path) -> bool {
    false
}

fn is_stale_by_mtime(path: &Path, threshold: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    modified.elapsed().map(|elapsed| elapsed > threshold).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_directory_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let removed = sweep_stale_sockets(&missing, "isekai-pipe-ctl-", Duration::from_secs(3600)).unwrap();
        assert!(removed.is_empty());
    }

    #[test]
    fn ignores_files_not_matching_the_prefix_or_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("unrelated.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("other-prefix-abc.sock"), b"x").unwrap();
        let removed = sweep_stale_sockets(dir.path(), "isekai-pipe-ctl-", Duration::from_secs(3600)).unwrap();
        assert!(removed.is_empty());
        assert!(dir.path().join("unrelated.txt").exists());
        assert!(dir.path().join("other-prefix-abc.sock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn removes_a_socket_with_no_listener() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("isekai-pipe-ctl-abandoned.sock");
        {
            // Bind and immediately drop the listener: the socket file is
            // left behind on disk with nobody listening, exactly like a
            // process that crashed without cleaning up after itself.
            let _listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
        }
        assert!(sock_path.exists());

        let removed = sweep_stale_sockets(dir.path(), "isekai-pipe-ctl-", Duration::from_secs(3600)).unwrap();
        assert_eq!(removed, vec![sock_path.clone()]);
        assert!(!sock_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn leaves_a_socket_with_a_live_listener_alone() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("isekai-pipe-ctl-alive.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();

        let removed = sweep_stale_sockets(dir.path(), "isekai-pipe-ctl-", Duration::from_secs(3600)).unwrap();
        assert!(removed.is_empty());
        assert!(sock_path.exists());
    }

    #[test]
    fn is_stale_by_mtime_reflects_a_configurable_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("isekai-pipe-ctl-fresh.sock");
        std::fs::write(&path, b"anything").unwrap();

        assert!(
            !is_stale_by_mtime(&path, Duration::from_secs(3600)),
            "a file written moments ago should not be stale under a 1-hour threshold"
        );
        assert!(
            is_stale_by_mtime(&path, Duration::from_secs(0)),
            "any file is older than a 0-second threshold"
        );
    }

    #[test]
    fn is_stale_by_mtime_is_false_for_a_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_stale_by_mtime(&dir.path().join("missing"), Duration::from_secs(0)));
    }

    /// Regression guard for a surprise discovered while writing this
    /// module: on Linux, `connect()`ing a UNIX-domain socket path that is
    /// actually a plain regular file (not a socket at all) also returns
    /// `ECONNREFUSED` — the same errno as a genuinely abandoned socket —
    /// rather than the `ENOTSOCK` one might expect. So `is_abandoned`
    /// alone already sweeps corrupted non-socket leftovers too; the mtime
    /// fallback in `sweep_stale_sockets` exists for rarer, genuinely
    /// ambiguous `connect()` outcomes (e.g. a permission error), not for
    /// this case.
    ///
    /// Linux-only: confirmed on a real `test-macos` CI run that macOS's
    /// kernel does *not* share this quirk (`connect()` to a non-socket path
    /// there fails with something other than `ECONNREFUSED`), so
    /// `is_abandoned` correctly falls through to `false` and this specific
    /// case relies on the mtime fallback instead — exactly the "genuinely
    /// ambiguous outcome" case this module's docs already describe, just
    /// with a wider set of platforms hitting it than originally assumed.
    #[cfg(target_os = "linux")]
    #[test]
    fn is_abandoned_is_true_for_a_plain_file_at_the_socket_path_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("isekai-pipe-ctl-not-a-socket.sock");
        std::fs::write(&path, b"not a socket").unwrap();
        assert!(is_abandoned(&path));
    }
}
