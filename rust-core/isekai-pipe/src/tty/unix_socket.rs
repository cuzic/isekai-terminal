//! Private-directory + private-socket setup, and same-user peer
//! verification, for the daemon↔attach Unix domain socket.
//!
//! A design review weighted this module's correctness heavily: unlike this
//! crate's other local IPC (`ctl_file`'s ctl-socket forward, which only
//! relays cosmetic title/clipboard messages), a mistake here is a path to
//! **arbitrary code execution as the user** — whatever connects to a
//! `tty-daemon` socket can send `Frame::Stdin` bytes straight into a live
//! shell. Two layers, deliberately not just one:
//!
//! 1. **Filesystem permissions**, tightened past what `isekai_fs_guard`'s
//!    shared `ensure_private_dir` does elsewhere in this codebase (that
//!    helper's `create_dir_all`-then-`chmod` has a real, if brief,
//!    TOCTOU window — not changed here, since it's shared by other
//!    established callers with a different risk profile; this module just
//!    doesn't use it). [`private_runtime_dir`] creates the socket directory
//!    with `DirBuilder::mode(0o700)` — private from the moment it exists,
//!    no separate `chmod` step — then verifies ownership/mode/not-a-symlink
//!    before trusting it. [`bind_private_socket`] narrows the process
//!    `umask` around the `bind` call so the socket file itself is created
//!    `0600`, not the platform default.
//! 2. **`SO_PEERCRED`** ([`verify_peer_is_self`]) — the actually decisive
//!    control, used on *both* ends: the daemon checks every accepted
//!    connection's uid before trusting anything it sends, and `tty attach`
//!    checks the daemon's uid before sending it anything (the socket path
//!    can be squatted by whoever creates it first, so connecting
//!    successfully proves nothing about who's on the other end without
//!    this). Even if layer 1 had a gap, this still refuses to relay a
//!    single byte to or from a different user.
//!
//! **Ordering requirement**: [`bind_private_socket`]'s `umask` narrowing is
//! a *process-global*, not per-thread, setting — on this crate's
//! multi-threaded Tokio runtime, a concurrently-running task creating a
//! file on another thread during that window would unexpectedly get the
//! narrowed mode too (harmless — narrower is never wrong for a
//! *permissions* bug — but still not the intended scope). Callers must
//! call it as the daemon's first action, before spawning any other task
//! that touches the filesystem, to keep the window from overlapping
//! anything.

use std::io;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

/// `~/.cache/isekai-pipe/tty` — the directory `<name>.sock`/`<name>.lock`
/// live under. Created privately (`0700`, no separate `chmod`) if it
/// doesn't exist; if it already exists, verified rather than trusted
/// blindly (owner must be the current effective user, mode must not grant
/// group/other any access, and it must not be a symlink — a symlink swap
/// is exactly the kind of TOCTOU a "just check the mode bits" verification
/// alone would miss).
///
/// `.recursive(true)`: on a fresh `$HOME` (no `~/.cache` yet at all — the
/// common case in a container/CI environment, and what an earlier version
/// of this function missed, found by this feature's own e2e test failing
/// with "the daemon never bound its socket" — a non-recursive `create` on a
/// missing *grandparent* fails with `NotFound`, not `AlreadyExists`, so it
/// fell through to a real `Err` instead of proceeding). `DirBuilderExt::mode`
/// applies to every directory this creates, including any newly-created
/// intermediates — so a from-scratch `~/.cache` ends up `0700` too, not
/// just the leaf; harmless (more private than the usual `0755` default, and
/// only affects directories this call itself is the one bringing into
/// existence — an already-existing `~/.cache` is left untouched, only
/// verified like the leaf directory itself).
pub(crate) fn private_runtime_dir() -> io::Result<PathBuf> {
    let home = isekai_fs_guard::resolve_home_dir().ok_or_else(|| io::Error::other("could not determine the home directory"))?;
    let dir = home.join(".cache").join("isekai-pipe").join("tty");

    match std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    verify_private_dir(&dir)?;
    Ok(dir)
}

/// Conservative bound on the *whole* socket path, safely under both
/// platforms this module ships for: `sockaddr_un.sun_path` is 104 bytes on
/// macOS and 108 on Linux, each including the NUL terminator — 90 leaves
/// comfortable margin on the tighter (macOS) limit without needing a
/// `cfg`-gated exact constant per platform.
const MAX_SOCKET_PATH_LEN: usize = 90;

/// `dir.join(format!("{name}.sock"))`, unless that would overflow
/// [`MAX_SOCKET_PATH_LEN`] — real bug found live (2026-08-12, this
/// feature's own e2e tests running on macOS CI): `dir` is
/// [`private_runtime_dir`]'s `$HOME`-rooted path, so a long-enough `$HOME`
/// (a plausible macOS default — CI's own `/var/folders/<hash>/T/...` is
/// already close) plus a long `name` (this feature's own auto-derived
/// `--isekai-tty` names, or simply a name near `validate_name`'s own
/// 200-byte ceiling — chosen for ordinary filesystem filename limits, which
/// have nothing to do with `sockaddr_un`'s much tighter one) can exceed the
/// kernel's `sun_path` limit *regardless of `$HOME`'s length*, failing every
/// `connect(2)`/`bind(2)` with "path must be shorter than SUN_LEN" — not a
/// theoretical concern, an actual CI failure this exact function exists to
/// prevent.
///
/// Falls back to a short, fixed-length hash of the natural path — not just
/// of `name` alone, so a collision would need two different `dir` roots
/// *and* names to hash alike, never just two long names sharing a prefix.
/// Both `daemon.rs` (bind) and `attach.rs` (connect) call this same
/// function with the same `(dir, name)`, so they always agree on which path
/// — natural or hashed — a given session actually uses.
pub(crate) fn socket_path(dir: &Path, name: &str) -> PathBuf {
    use std::os::unix::ffi::OsStrExt as _;

    let natural = dir.join(format!("{name}.sock"));
    if natural.as_os_str().len() <= MAX_SOCKET_PATH_LEN {
        return natural;
    }
    let short = &isekai_trust::hex_sha256(natural.as_os_str().as_bytes())[..16];
    dir.join(format!("{short}.sock"))
}

fn verify_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    // `symlink_metadata`, not `metadata`: the latter follows a symlink,
    // which would happily "verify" the *target's* ownership/mode while the
    // path a caller actually opens is the symlink itself.
    let meta = std::fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::other(format!("{} must not be a symlink", dir.display())));
    }
    if !meta.is_dir() {
        return Err(io::Error::other(format!("{} exists but is not a directory", dir.display())));
    }
    // SAFETY: no unsafe here — `geteuid()` takes no arguments and cannot
    // fail (POSIX guarantees it always succeeds).
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return Err(io::Error::other(format!("{} is not owned by the current user (uid {} != {euid})", dir.display(), meta.uid())));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(io::Error::other(format!("{} grants group/other access (mode {:o})", dir.display(), meta.mode() & 0o777)));
    }
    Ok(())
}

/// Binds a Unix domain socket at `path`, narrowing the process `umask`
/// around the call so the resulting socket file is created `0600` (private
/// from the instant it exists) rather than whatever the ambient umask would
/// otherwise have produced — see this module's doc comment on the
/// call-ordering requirement this relies on. `path` must not already exist
/// (the caller is responsible for the stale-socket detection/cleanup dance,
/// `flock.rs`) — `bind` fails with `AddrInUse` if it does, which is treated
/// as a hard error here rather than silently unlinking, so a caller can't
/// accidentally steal a socket a *live* daemon is still using.
pub(crate) fn bind_private_socket(path: &Path) -> io::Result<UnixListener> {
    // SAFETY: `umask` takes a plain mode value and returns the previous
    // one; no preconditions beyond a live process.
    let previous = unsafe { libc::umask(0o077) };
    let result = std::os::unix::net::UnixListener::bind(path);
    // SAFETY: restores exactly what was there before, regardless of `bind`'s
    // outcome — this must run even on the error path, hence not using `?`
    // before it.
    unsafe { libc::umask(previous) };
    let std_listener = result?;
    std_listener.set_nonblocking(true)?;
    UnixListener::from_std(std_listener)
}

/// Verifies that the peer on `stream` (the connecting client, from the
/// daemon's side; or the daemon, from `tty attach`'s side — this check is
/// symmetric and used both ways) is running as the *same* effective user as
/// this process. The one substantive access-control decision in this whole
/// feature — see this module's doc comment.
pub(crate) fn verify_peer_is_self(stream: &UnixStream) -> io::Result<()> {
    let cred = stream.peer_cred()?;
    // SAFETY: see `verify_private_dir`'s identical call.
    let euid = unsafe { libc::geteuid() };
    if cred.uid() != euid {
        return Err(io::Error::other(format!("peer uid {} does not match the current user (uid {euid})", cred.uid())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_uses_the_natural_name_when_it_fits() {
        let dir = PathBuf::from("/tmp/short");
        let path = socket_path(&dir, "myhost");
        assert_eq!(path, dir.join("myhost.sock"));
    }

    #[test]
    fn socket_path_falls_back_to_a_short_hash_when_the_natural_path_would_overflow_sun_path() {
        // A `dir` long enough on its own that even a short `name` overflows
        // `MAX_SOCKET_PATH_LEN` — standing in for a long `$HOME` (this
        // module's real bug: CI's own long macOS temp-dir default).
        let dir = PathBuf::from("/tmp/a-suspiciously-long-directory-name-standing-in-for-a-long-home-path/.cache/isekai-pipe/tty");
        let name = "wt-7cf62198-6b3f-4b33-810f-6b93cf102770";
        let path = socket_path(&dir, name);

        assert_ne!(path, dir.join(format!("{name}.sock")), "must not use the natural (too-long) path");
        assert!(
            path.as_os_str().len() <= MAX_SOCKET_PATH_LEN,
            "the fallback path itself must respect the bound it exists to enforce: {path:?} ({} bytes)",
            path.as_os_str().len()
        );
        assert_eq!(path.parent(), Some(dir.as_path()), "the fallback must still live in the same directory, just under a shorter filename");
    }

    #[test]
    fn socket_path_is_deterministic_so_daemon_and_attach_always_agree() {
        let dir = PathBuf::from("/tmp/a-suspiciously-long-directory-name-standing-in-for-a-long-home-path/.cache/isekai-pipe/tty");
        let name = "wt-7cf62198-6b3f-4b33-810f-6b93cf102770";
        assert_eq!(socket_path(&dir, name), socket_path(&dir, name), "the same (dir, name) must always hash to the same fallback path");
    }

    #[test]
    fn socket_path_fallback_differs_for_different_names_in_the_same_dir() {
        let dir = PathBuf::from("/tmp/a-suspiciously-long-directory-name-standing-in-for-a-long-home-path/.cache/isekai-pipe/tty");
        let a = socket_path(&dir, "wt-11111111-1111-1111-1111-111111111111");
        let b = socket_path(&dir, "wt-22222222-2222-2222-2222-222222222222");
        assert_ne!(a, b, "two different long names in the same dir must not collide onto the same fallback socket path");
    }
}
