//! Cross-process exclusive locking, generalized from `isekai-pipe-core`'s
//! private `ProfileLock` (`profile.rs`, `update_persistent_profile`) — that
//! implementation predates this module and stays as its own copy rather
//! than being migrated to call this one (out of scope for the change that
//! introduced this generalization; not otherwise treated as a separate
//! source of truth to keep in sync — the logic is small and stable).
//!
//! Use [`with_exclusive_lock`] to make a read-modify-write cycle against a
//! shared file (or set of files) atomic across concurrently-running
//! processes — e.g. multiple `isekai-ssh` tabs each deciding whether to
//! trust a newly-seen SSH host key and persisting that decision.

use std::fs;
use std::io;
use std::path::Path;

/// Runs `f` while holding an exclusive advisory lock scoped to
/// `<dir>/<key>.lock` — concurrent callers using a *different* `key` never
/// block each other; callers using the *same* `key` (including from
/// different processes) are fully serialized around `f`.
///
/// The lock is released automatically when this function returns (even on
/// panic-via-unwind, or if the holding process crashes/is killed) — no
/// separate cleanup step is needed, unlike a lockfile whose mere
/// *existence* signals ownership.
pub fn with_exclusive_lock<T>(dir: &Path, key: &str, f: impl FnOnce() -> T) -> io::Result<T> {
    let _lock = FileLock::acquire(dir, key)?;
    Ok(f())
}

/// Holds an exclusive advisory lock (`flock(2)`, `LOCK_EX`) on
/// `<dir>/<key>.lock` for its lifetime. `flock` is held by the open file
/// description and is released when the underlying fd is closed (this
/// struct's `Drop`), even on a crash.
#[cfg(unix)]
struct FileLock {
    _file: fs::File,
}

#[cfg(unix)]
impl FileLock {
    fn acquire(dir: &Path, key: &str) -> io::Result<Self> {
        use std::os::unix::io::AsRawFd;

        fs::create_dir_all(dir)?;
        let lock_path = dir.join(format!("{key}.lock"));
        // `truncate(false)`: this file's content is never read or written —
        // it exists only to be locked — so truncating it on every
        // acquisition would be pure waste, not a correctness concern either
        // way.
        let file = fs::OpenOptions::new().create(true).write(true).truncate(false).open(&lock_path)?;
        // SAFETY: `file.as_raw_fd()` is a valid, open fd for the duration of
        // this call (the `File` outlives it), matching `flock(2)`'s contract.
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }
}

/// Windows counterpart of the `flock(2)`-based lock above: `LockFileEx`
/// with `LOCKFILE_EXCLUSIVE_LOCK` (and no `LOCKFILE_FAIL_IMMEDIATELY`, so it
/// blocks until acquired, matching `LOCK_EX`'s semantics) on the same
/// handle the file was opened with. The lock is released when the handle
/// closes (`File`'s `Drop`) — even on a crash — the same "no separate
/// cleanup step" property `flock` has.
///
/// **Not verified against a real Windows machine** — see
/// `windows_acl.rs`'s module docs for what verification
/// (`cargo check --target x86_64-pc-windows-gnu`) has and hasn't been done;
/// the same caveat applies here (inherited from the `ProfileLock` this was
/// generalized from, which carries the identical caveat).
#[cfg(windows)]
struct FileLock {
    _file: fs::File,
}

#[cfg(windows)]
impl FileLock {
    fn acquire(dir: &Path, key: &str) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;

        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK};
        use windows::Win32::System::IO::OVERLAPPED;

        fs::create_dir_all(dir)?;
        let lock_path = dir.join(format!("{key}.lock"));
        let file = fs::OpenOptions::new().create(true).read(true).write(true).open(&lock_path)?;

        let handle = HANDLE(file.as_raw_handle());
        let mut overlapped = OVERLAPPED::default();
        // SAFETY: `handle` is a valid, open file handle for the duration of
        // this call (`file` outlives it); locking the whole file (`u32::MAX`
        // bytes both halves) matches `flock`'s whole-file semantics above.
        unsafe {
            LockFileEx(handle, LOCKFILE_EXCLUSIVE_LOCK, 0, u32::MAX, u32::MAX, &mut overlapped)
                .map_err(|e| io::Error::from_raw_os_error(e.code().0 as i32))?;
        }
        Ok(Self { _file: file })
    }
}

/// Neither unix nor windows: locking is a no-op. Kept so this module still
/// compiles rather than gating the whole crate on `cfg(any(unix, windows))`.
#[cfg(not(any(unix, windows)))]
struct FileLock;

#[cfg(not(any(unix, windows)))]
impl FileLock {
    fn acquire(_dir: &Path, _key: &str) -> io::Result<Self> {
        Ok(Self)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// The point of `flock`(2)/`LockFileEx`: a second acquirer of the
    /// *same* key must actually block until the first releases, not just
    /// coexist. Verified by having two threads each hold the lock for a
    /// short sleep while incrementing a counter before and after — if
    /// locking weren't exclusive, both increments could interleave inside
    /// the "critical section" instead of being strictly ordered.
    #[test]
    fn same_key_serializes_concurrent_callers() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        let counter = Arc::new(AtomicU32::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let dir_path = dir_path.clone();
            let counter = counter.clone();
            handles.push(std::thread::spawn(move || {
                with_exclusive_lock(&dir_path, "shared-key", || {
                    let before = counter.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    // If another thread's critical section overlapped with
                    // ours, `counter` would have advanced by more than 1
                    // between our own read and write below.
                    assert_eq!(counter.load(Ordering::SeqCst), before + 1);
                })
                .unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 8);
    }

    #[test]
    fn different_keys_do_not_block_each_other() {
        let dir = tempfile::tempdir().unwrap();
        // Acquire and hold key "a" on a background thread; key "b" must
        // still be immediately acquirable from this thread without waiting
        // for "a" to be released.
        let dir_a = dir.path().to_path_buf();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            with_exclusive_lock(&dir_a, "a", || {
                release_rx.recv().ok();
            })
            .unwrap();
        });
        // Give the holder thread a moment to actually acquire the lock.
        std::thread::sleep(std::time::Duration::from_millis(20));

        let started = std::time::Instant::now();
        with_exclusive_lock(dir.path(), "b", || {}).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(500), "key \"b\" should not wait on key \"a\"");

        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }
}
