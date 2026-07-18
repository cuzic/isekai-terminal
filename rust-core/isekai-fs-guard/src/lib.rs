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
/// `isekai-trust::store::config_dir_from_home`).
fn resolve_home_dir_from(lookup: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    lookup("HOME").or_else(|| lookup("USERPROFILE")).map(PathBuf::from)
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
}
