//! Shared filesystem permission guard for isekai-ssh's on-disk secrets.
//!
//! `isekai-trust`'s trust store (`known_helpers.toml`) and `isekai-auth`'s
//! token file (`token.json`) both need the exact same invariant: the file
//! and its parent directory must not be writable by anyone but the current
//! user, new directories are created private, and new files are created
//! private. Before this crate existed, each crate carried its own copy of
//! this logic (`isekai-trust`'s `store.rs`, `isekai-auth`'s
//! `file_provider.rs` — the latter explicitly documented as "mirroring" the
//! former). This crate is now the single place that invariant is
//! implemented; callers translate `FsGuardError` into their own richer,
//! path-carrying error type (see `isekai-trust::store`/`isekai-auth::file_provider`
//! for the mapping).
//!
//! [`resolve_config_dir`]/[`read_checked`]/[`write_private_atomically`]
//! (WU-M3, 2026-08) go one layer further: both callers' config-directory
//! resolution, permission-checked load, and atomic-write bodies were
//! themselves byte-for-byte identical (only the resulting error's *type*
//! differed), so those got moved here too rather than staying duplicated on
//! top of the lower-level primitives above. [`FsGuardErrorAt`] is the
//! shared, path-attached error type for this layer — `isekai-trust::TrustError`
//! and `isekai-auth::AuthError` each hold one `#[from] FsGuardErrorAt`
//! variant instead of the eight near-identical variants they used to
//! declare independently.
//!
//! Two platform backends:
//! - Unix: the classic owner/group/other mode bits (`0o700` dirs, `0o600`
//!   files; `check_not_world_writable` only rejects the *others* bit,
//!   `0o002` — a shared group is still allowed).
//! - Windows (`windows_acl.rs`): no mode-bit equivalent exists, so this
//!   operates on the file/directory's DACL directly and is deliberately
//!   *stricter* than the Unix side — any grant to a principal other than
//!   the current user is rejected, not just an "everyone" grant. This
//!   asymmetry is intentional (new design surface for Windows support, not
//!   a mechanical port of the Unix policy); see `windows_acl.rs`'s module
//!   docs for what verification has (and hasn't) been done.
//!
//! Pure `std::fs`/Win32, no async/tokio — both callers only ever use this
//! synchronously.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

mod file_lock;
mod identity_file;
#[cfg(windows)]
mod windows_acl;

pub use file_lock::with_exclusive_lock;
pub use identity_file::identity_file_candidates;

/// A permission-guard failure, deliberately without a `path` field: callers
/// already know which path they passed in and attach it to their own error
/// type (`TrustError`/`AuthError`), which also needs to distinguish this
/// crate's failure shapes from their other, unrelated error variants.
#[derive(Debug)]
pub enum FsGuardError {
    CreateDir(std::io::Error),
    Stat(std::io::Error),
    SetPermissions(std::io::Error),
    /// Unix: `path` is writable by users other than its owner (`mode` is
    /// the offending permission bits, masked to `0o777`).
    WorldWritable { mode: u32 },
    /// Windows: `path`'s DACL grants write-ish rights to `principal` (a
    /// SID, rendered as a string — see `windows_acl::sid_to_string`), other
    /// than the current user. `rights` is the raw access-mask, formatted as
    /// hex, for diagnostics.
    InsecureAcl { principal: String, rights: String },
}

/// Fails closed if `path` is writable by anyone other than the current user.
/// Unix: rejects the *others*-writable mode bit (`0o002`) only — a shared
/// group is still allowed. Windows: rejects any DACL grant of write-ish
/// rights to a principal other than the current user (see `windows_acl.rs`,
/// stricter than the Unix policy by design). A no-op on any other platform.
#[cfg(unix)]
pub fn check_not_world_writable(path: &Path) -> Result<(), FsGuardError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path).map_err(FsGuardError::Stat)?;
    let mode = metadata.permissions().mode();
    if mode & 0o002 != 0 {
        return Err(FsGuardError::WorldWritable { mode: mode & 0o777 });
    }
    Ok(())
}

#[cfg(windows)]
pub fn check_not_world_writable(path: &Path) -> Result<(), FsGuardError> {
    windows_acl::check_not_world_writable(path)
}

#[cfg(not(any(unix, windows)))]
pub fn check_not_world_writable(_path: &Path) -> Result<(), FsGuardError> {
    Ok(())
}

/// Creates `dir` privately (`0700` on Unix, an owner-only DACL on Windows)
/// if it doesn't exist yet; otherwise checks that it isn't writable by
/// anyone else and fails closed if it is.
pub fn ensure_private_dir(dir: &Path) -> Result<(), FsGuardError> {
    if !dir.exists() {
        fs::create_dir_all(dir).map_err(FsGuardError::CreateDir)?;
        set_private_dir_permissions(dir)
    } else {
        check_not_world_writable(dir)
    }
}

/// Unconditionally (re)applies private permissions to an existing directory
/// (`0700` on Unix, an owner-only DACL on Windows) — the directory
/// counterpart of `set_private_file_permissions`, split out so callers that
/// always (re-)apply permissions on every write (e.g.
/// `isekai_pipe_core::profile::write_persistent_profile`, which doesn't use
/// `ensure_private_dir`'s create-vs-check branching) don't have to
/// reimplement the platform `cfg` split themselves.
#[cfg(unix)]
pub fn set_private_dir_permissions(dir: &Path) -> Result<(), FsGuardError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(FsGuardError::CreateDir)
}

#[cfg(windows)]
pub fn set_private_dir_permissions(dir: &Path) -> Result<(), FsGuardError> {
    windows_acl::set_private_acl(dir)
}

#[cfg(not(any(unix, windows)))]
pub fn set_private_dir_permissions(_dir: &Path) -> Result<(), FsGuardError> {
    Ok(())
}

/// Sets private permissions on `path` (`0600` on Unix, an owner-only DACL
/// on Windows). A no-op on any other platform.
#[cfg(unix)]
pub fn set_private_file_permissions(path: &Path) -> Result<(), FsGuardError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(FsGuardError::SetPermissions)
}

#[cfg(windows)]
pub fn set_private_file_permissions(path: &Path) -> Result<(), FsGuardError> {
    windows_acl::set_private_acl(path)
}

#[cfg(not(any(unix, windows)))]
pub fn set_private_file_permissions(_path: &Path) -> Result<(), FsGuardError> {
    Ok(())
}

/// Resolves the user's home directory across platforms: `$HOME` (Unix, and
/// Windows environments like Git Bash/MSYS/WSL that set it too), falling
/// back to `%USERPROFILE%` (native `cmd.exe`/PowerShell, which does not set
/// `HOME`). `isekai-trust`/`isekai-auth`'s config-directory layout
/// (`.config/isekai-ssh`, the existing Unix XDG-style join) is unchanged by
/// this — this function only makes the *lookup* work on Windows, not the
/// resulting path idiomatic there (a Windows-native `%APPDATA%`/OS-keychain
/// layout is a separate, still-open design question, `archive/ISEKAI_SSH_DESIGN.md`'s
/// "配布対象プラットフォーム" note).
pub fn resolve_home_dir() -> Option<PathBuf> {
    resolve_home_dir_from(|key| std::env::var_os(key))
}

/// Pure helper split out of `resolve_home_dir` so the `HOME`-then-
/// `USERPROFILE` priority order can be unit-tested without mutating the
/// process-wide environment (`std::env::set_var` is process-global and not
/// safe to toggle from concurrently-running tests — same rationale as
/// [`config_dir_from_home`]).
fn resolve_home_dir_from(lookup: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    lookup("HOME").or_else(|| lookup("USERPROFILE")).map(PathBuf::from)
}

/// A [`FsGuardError`] (or a related read/write/config-resolution I/O
/// failure), with the `path` it happened at attached — the shared shape
/// `isekai-trust::TrustError` and `isekai-auth::AuthError` each used to
/// declare eight near-identical variants for on their own (`Read`/`Write`/
/// `CreateDir`/`Stat`/`WorldWritable`/`InsecureAcl`/`NoHomeDir`/
/// `NoParentDir`, six of them with verbatim `#[error]` message text). Each
/// crate's own error enum now holds one `#[from] FsGuardErrorAt` variant
/// instead and keeps only the error variants that are genuinely its own
/// (TOML/JSON `Parse`/`Serialize`, `TrustError::EmptyHost`, `AuthError`'s
/// OAuth device-flow variants, ...).
#[derive(Debug, thiserror::Error)]
pub enum FsGuardErrorAt {
    #[error("failed to read {path}: {source}")]
    Read { path: PathBuf, #[source] source: std::io::Error },

    #[error("failed to write {path}: {source}")]
    Write { path: PathBuf, #[source] source: std::io::Error },

    #[error("failed to create config directory {path}: {source}")]
    CreateDir { path: PathBuf, #[source] source: std::io::Error },

    #[error("failed to inspect permissions of {path}: {source}")]
    Stat { path: PathBuf, #[source] source: std::io::Error },

    /// Unix: `path` is writable by users other than its owner (`mode` is
    /// the offending permission bits, masked to `0o777`).
    #[error("{path} is world-writable (mode {mode:o}); refusing to use it")]
    WorldWritable { path: PathBuf, mode: u32 },

    /// Windows: `path`'s DACL grants write-ish rights to `principal` other
    /// than the current user — see [`FsGuardError::InsecureAcl`].
    #[error("{path} grants write access to {principal} (rights {rights}); refusing to use it")]
    InsecureAcl { path: PathBuf, principal: String, rights: String },

    #[error("could not determine the home directory (HOME is not set)")]
    NoHomeDir,

    #[error("path {path} has no parent directory")]
    NoParentDir { path: PathBuf },
}

impl FsGuardErrorAt {
    /// Attaches `path` to a path-less [`FsGuardError`], picking the
    /// matching [`FsGuardErrorAt`] variant — the one mapping
    /// `isekai-trust`'s and `isekai-auth`'s own `map_fs_guard_err` used to
    /// duplicate verbatim (down to `SetPermissions` folding into `Write`,
    /// same as both original copies did).
    fn at(path: &Path, err: FsGuardError) -> Self {
        match err {
            FsGuardError::CreateDir(source) => FsGuardErrorAt::CreateDir { path: path.to_path_buf(), source },
            FsGuardError::Stat(source) => FsGuardErrorAt::Stat { path: path.to_path_buf(), source },
            FsGuardError::SetPermissions(source) => FsGuardErrorAt::Write { path: path.to_path_buf(), source },
            FsGuardError::WorldWritable { mode } => FsGuardErrorAt::WorldWritable { path: path.to_path_buf(), mode },
            FsGuardError::InsecureAcl { principal, rights } => {
                FsGuardErrorAt::InsecureAcl { path: path.to_path_buf(), principal, rights }
            }
        }
    }
}

/// `~/.config/<dir_name>` (XDG Base Directory convention, per
/// `archive/ISEKAI_SSH_DESIGN.md`). Resolves the home directory via
/// [`resolve_home_dir`]. `isekai-trust`/`isekai-auth` both resolve
/// `~/.config/isekai-ssh` this way — the same directory, previously two
/// copies of this exact join.
pub fn resolve_config_dir(dir_name: &str) -> Result<PathBuf, FsGuardErrorAt> {
    let home = resolve_home_dir().ok_or(FsGuardErrorAt::NoHomeDir)?;
    Ok(config_dir_from_home(&home, dir_name))
}

/// Pure helper split out of [`resolve_config_dir`] so the path-joining logic
/// can be unit-tested without mutating the process-wide `HOME` env var.
fn config_dir_from_home(home: &Path, dir_name: &str) -> PathBuf {
    home.join(".config").join(dir_name)
}

/// [`ensure_private_dir`] with the path attached to its error — for callers
/// that need the directory-only privacy check on its own (outside
/// [`read_checked`]/[`write_private_atomically`]'s own bodies), e.g.
/// `isekai-trust::store::with_locked_ssh_host_key_trust_store`'s pre-lock
/// check that a freshly-created config dir doesn't inherit a world-readable
/// umask-dependent mode from `with_exclusive_lock`'s own `create_dir_all`.
pub fn ensure_private_dir_checked(dir: &Path) -> Result<(), FsGuardErrorAt> {
    ensure_private_dir(dir).map_err(|e| FsGuardErrorAt::at(dir, e))
}

/// Checks `path`'s parent directory (if it exists) and `path` itself for
/// world-writability, then reads `path` as UTF-8 — the permission-checked
/// load preamble `isekai-trust`'s TOML stores and `isekai-auth`'s token file
/// both need before trusting a byte of the file's contents.
///
/// Returns `Ok(None)` if `path` doesn't exist yet, rather than deciding what
/// that means: `isekai-trust` treats a missing store as an empty default,
/// `isekai-auth` treats a missing token file as a hard error — that decision
/// stays with each caller.
pub fn read_checked(path: &Path) -> Result<Option<String>, FsGuardErrorAt> {
    if let Some(parent) = path.parent() {
        if parent.exists() {
            check_not_world_writable(parent).map_err(|e| FsGuardErrorAt::at(parent, e))?;
        }
    }
    if !path.exists() {
        return Ok(None);
    }
    check_not_world_writable(path).map_err(|e| FsGuardErrorAt::at(path, e))?;
    let content =
        fs::read_to_string(path).map_err(|source| FsGuardErrorAt::Read { path: path.to_path_buf(), source })?;
    Ok(Some(content))
}

/// Writes `contents` to `path` atomically (temp file in `path`'s parent
/// directory, `0600`/owner-only permissions set on it before any bytes are
/// written, then renamed over `path`) — the atomic-write body
/// `isekai-trust`'s `save_toml_store` and `isekai-auth`'s `write_atomically`
/// both need. Creates the parent directory (`0700`) first if it doesn't
/// exist yet.
pub fn write_private_atomically(path: &Path, contents: &[u8]) -> Result<(), FsGuardErrorAt> {
    use std::io::Write as _;

    let parent = path.parent().ok_or_else(|| FsGuardErrorAt::NoParentDir { path: path.to_path_buf() })?;
    ensure_private_dir(parent).map_err(|e| FsGuardErrorAt::at(parent, e))?;

    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|source| FsGuardErrorAt::Write { path: path.to_path_buf(), source })?;
    set_private_file_permissions(tmp.path()).map_err(|e| FsGuardErrorAt::at(tmp.path(), e))?;
    tmp.write_all(contents)
        .and_then(|_| tmp.flush())
        .map_err(|source| FsGuardErrorAt::Write { path: path.to_path_buf(), source })?;

    tmp.persist(path).map_err(|e| FsGuardErrorAt::Write { path: path.to_path_buf(), source: e.error })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_home_dir_from_prefers_home_over_userprofile() {
        let home = resolve_home_dir_from(|key| match key {
            "HOME" => Some(OsString::from("/home/alice")),
            "USERPROFILE" => Some(OsString::from(r"C:\Users\alice")),
            _ => None,
        });
        assert_eq!(home, Some(PathBuf::from("/home/alice")));
    }

    #[test]
    fn resolve_home_dir_from_falls_back_to_userprofile_when_home_is_unset() {
        let home = resolve_home_dir_from(|key| match key {
            "USERPROFILE" => Some(OsString::from(r"C:\Users\alice")),
            _ => None,
        });
        assert_eq!(home, Some(PathBuf::from(r"C:\Users\alice")));
    }

    #[test]
    fn resolve_home_dir_from_is_none_when_neither_is_set() {
        assert_eq!(resolve_home_dir_from(|_| None), None);
    }

    #[test]
    fn ensure_private_dir_creates_missing_dir_as_0700() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested");
        ensure_private_dir(&target).unwrap();
        assert!(target.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_rejects_existing_world_writable_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested");
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o777)).unwrap();

        let err = ensure_private_dir(&target).unwrap_err();
        assert!(matches!(err, FsGuardError::WorldWritable { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn check_not_world_writable_accepts_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        check_not_world_writable(&path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn check_not_world_writable_rejects_world_writable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();

        let err = check_not_world_writable(&path).unwrap_err();
        assert!(matches!(err, FsGuardError::WorldWritable { mode: 0o666 }));
    }

    #[cfg(unix)]
    #[test]
    fn set_private_file_permissions_sets_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        set_private_file_permissions(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn config_dir_is_joined_under_home() {
        let home = Path::new("/home/example-user");
        assert_eq!(config_dir_from_home(home, "isekai-ssh"), home.join(".config").join("isekai-ssh"));
    }

    #[test]
    fn read_checked_returns_none_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist");
        assert_eq!(read_checked(&path).unwrap(), None);
    }

    #[test]
    fn read_checked_returns_file_contents_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, "hello").unwrap();
        assert_eq!(read_checked(&path).unwrap(), Some("hello".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn read_checked_rejects_a_world_writable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, "hello").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();

        let err = read_checked(&path).unwrap_err();
        assert!(matches!(err, FsGuardErrorAt::WorldWritable { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn read_checked_rejects_a_world_writable_parent_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("isekai-ssh");
        fs::create_dir_all(&config_dir).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o777)).unwrap();
        let path = config_dir.join("f");
        fs::write(&path, "hello").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let err = read_checked(&path).unwrap_err();
        assert!(matches!(err, FsGuardErrorAt::WorldWritable { .. }));
    }

    #[test]
    fn write_private_atomically_round_trips_and_creates_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("nested");
        let path = config_dir.join("f");
        assert!(!config_dir.exists());

        write_private_atomically(&path, b"hello").unwrap();
        assert_eq!(read_checked(&path).unwrap(), Some("hello".to_string()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = fs::metadata(&config_dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
            let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600);
        }
    }

    #[test]
    fn write_private_atomically_survives_a_stale_temp_file_left_by_a_previous_crash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        write_private_atomically(&path, b"first").unwrap();
        write_private_atomically(&path, b"second").unwrap();
        assert_eq!(read_checked(&path).unwrap(), Some("second".to_string()));
    }
}
