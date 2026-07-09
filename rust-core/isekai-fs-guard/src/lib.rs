//! Shared filesystem permission guard for isekai-ssh's on-disk secrets.
//!
//! `isekai-trust`'s trust store (`known_helpers.toml`) and `isekai-auth`'s
//! token file (`token.json`) both need the exact same invariant: the file
//! and its parent directory must not be world-writable, new directories are
//! created `0700`, and new files are created `0600`. Before this crate
//! existed, each crate carried its own copy of this logic (`isekai-trust`'s
//! `store.rs`, `isekai-auth`'s `file_provider.rs` — the latter explicitly
//! documented as "mirroring" the former). This crate is now the single
//! place that invariant is implemented; callers translate `FsGuardError`
//! into their own richer, path-carrying error type (see
//! `isekai-trust::store`/`isekai-auth::file_provider` for the mapping).
//!
//! Pure `std::fs`, no async/tokio — both callers only ever use this
//! synchronously.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// A permission-guard failure, deliberately without a `path` field: callers
/// already know which path they passed in and attach it to their own error
/// type (`TrustError`/`AuthError`), which also needs to distinguish this
/// crate's three failure shapes from their other, unrelated error variants.
#[derive(Debug)]
pub enum FsGuardError {
    CreateDir(std::io::Error),
    Stat(std::io::Error),
    SetPermissions(std::io::Error),
    WorldWritable { mode: u32 },
}

/// Fails closed if `path` is writable by users other than its owner (mode
/// bit `0o002`). Unix-only; a no-op elsewhere (matching this project's
/// Linux-only "配布対象プラットフォーム" scope, `archive/ISEKAI_SSH_DESIGN.md`).
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

#[cfg(not(unix))]
pub fn check_not_world_writable(_path: &Path) -> Result<(), FsGuardError> {
    Ok(())
}

/// Creates `dir` (as `0700`) if it doesn't exist yet; otherwise checks that
/// it isn't world-writable and fails closed if it is.
pub fn ensure_private_dir(dir: &Path) -> Result<(), FsGuardError> {
    if !dir.exists() {
        fs::create_dir_all(dir).map_err(FsGuardError::CreateDir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(FsGuardError::CreateDir)?;
        }
        Ok(())
    } else {
        check_not_world_writable(dir)
    }
}

/// Sets `0600` permissions on `path`. Unix-only; a no-op elsewhere.
#[cfg(unix)]
pub fn set_private_file_permissions(path: &Path) -> Result<(), FsGuardError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(FsGuardError::SetPermissions)
}

#[cfg(not(unix))]
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
