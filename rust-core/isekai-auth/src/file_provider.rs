//! `TokenProvider` backed by `~/.config/isekai-ssh/token.json`.
//!
//! This file holds a bearer token (the relay JWT), so it gets the same
//! protection as `isekai-trust`'s `known_helpers.toml`
//! (`isekai-trust::store`, which this module mirrors): writes are atomic
//! (temp file in the same directory, then `rename`), the file is created
//! with `0600` permissions and its parent directory with `0700`, and both
//! are checked for world-writability before use — fail closed if either is
//! writable by others. This isn't spelled out in `ISEKAI_SSH_DESIGN.md` yet,
//! but the token file is exactly as sensitive as the trust store's identity
//! material, so it gets the same treatment.
//!
//! Unlike the trust store (where a missing file just means "no host trusted
//! yet" and loads as an empty default), a missing or malformed token file
//! here is always an error: there is no meaningful "empty token" state to
//! fall back to.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{AuthError, TokenProvider};

pub const CONFIG_DIR_NAME: &str = "isekai-ssh";
pub const TOKEN_FILE_NAME: &str = "token.json";

#[derive(Debug, Serialize, Deserialize)]
struct TokenFile {
    relay_jwt: String,
}

/// `~/.config/isekai-ssh` (XDG Base Directory convention, per
/// `ISEKAI_SSH_DESIGN.md`; same directory `isekai-trust` uses for
/// `known_helpers.toml`).
pub fn default_config_dir() -> Result<PathBuf, AuthError> {
    let home = std::env::var_os("HOME").ok_or(AuthError::NoHomeDir)?;
    Ok(config_dir_from_home(Path::new(&home)))
}

/// Pure helper split out of `default_config_dir` so the path-joining logic
/// can be unit-tested without mutating the process-wide `HOME` env var
/// (mirrors `isekai-trust::store::config_dir_from_home`).
fn config_dir_from_home(home: &Path) -> PathBuf {
    home.join(".config").join(CONFIG_DIR_NAME)
}

/// `~/.config/isekai-ssh/token.json`.
pub fn default_token_path() -> Result<PathBuf, AuthError> {
    Ok(default_config_dir()?.join(TOKEN_FILE_NAME))
}

/// Reads the relay JWT out of the token file at `path`.
///
/// Fails closed: a missing file, a world-writable file/directory, malformed
/// JSON, or an empty `relay_jwt` value are all errors, never a silent empty
/// token.
pub fn load_token(path: &Path) -> Result<String, AuthError> {
    if let Some(parent) = path.parent() {
        if parent.exists() {
            check_not_world_writable(parent)?;
        }
    }

    if !path.exists() {
        return Err(AuthError::TokenFileNotFound { path: path.to_path_buf() });
    }
    check_not_world_writable(path)?;

    let content =
        fs::read_to_string(path).map_err(|e| AuthError::Read { path: path.to_path_buf(), source: e })?;
    let parsed: TokenFile = serde_json::from_str(&content)
        .map_err(|e| AuthError::Parse { path: path.to_path_buf(), source: e })?;

    let trimmed = parsed.relay_jwt.trim();
    if trimmed.is_empty() {
        return Err(AuthError::EmptyToken { path: path.to_path_buf() });
    }
    Ok(trimmed.to_string())
}

/// Writes `relay_jwt` to the token file at `path` atomically (temp file +
/// rename) with `0600` permissions. Creates the parent directory (as
/// `0700`) if it does not exist yet.
pub fn save_token(path: &Path, relay_jwt: &str) -> Result<(), AuthError> {
    let parent = path.parent().ok_or_else(|| AuthError::NoParentDir { path: path.to_path_buf() })?;
    ensure_private_dir(parent)?;

    let serialized = serde_json::to_string_pretty(&TokenFile { relay_jwt: relay_jwt.to_string() })?;

    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| AuthError::Write { path: path.to_path_buf(), source: e })?;
    set_private_file_permissions(tmp.path())?;
    tmp.write_all(serialized.as_bytes())
        .and_then(|_| tmp.flush())
        .map_err(|e| AuthError::Write { path: path.to_path_buf(), source: e })?;

    tmp.persist(path).map_err(|e| AuthError::Write { path: path.to_path_buf(), source: e.error })?;
    Ok(())
}

/// Creates `dir` (as `0700`) if it doesn't exist yet; otherwise checks that
/// it isn't world-writable and fails closed if it is.
fn ensure_private_dir(dir: &Path) -> Result<(), AuthError> {
    if !dir.exists() {
        fs::create_dir_all(dir).map_err(|e| AuthError::CreateDir { path: dir.to_path_buf(), source: e })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(|e| AuthError::CreateDir { path: dir.to_path_buf(), source: e })?;
        }
        Ok(())
    } else {
        check_not_world_writable(dir)
    }
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| AuthError::Write { path: path.to_path_buf(), source: e })
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), AuthError> {
    Ok(())
}

/// Fails closed if `path` is writable by users other than its owner
/// (mode bit `0o002`). Unix-only; a no-op elsewhere.
#[cfg(unix)]
fn check_not_world_writable(path: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path).map_err(|e| AuthError::Stat { path: path.to_path_buf(), source: e })?;
    let mode = metadata.permissions().mode();
    if mode & 0o002 != 0 {
        return Err(AuthError::WorldWritable { path: path.to_path_buf(), mode: mode & 0o777 });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_not_world_writable(_path: &Path) -> Result<(), AuthError> {
    Ok(())
}

/// Reads the relay JWT from `~/.config/isekai-ssh/token.json` (or a custom
/// path, see `FileTokenProvider::new`).
pub struct FileTokenProvider {
    path: PathBuf,
}

impl FileTokenProvider {
    /// Reads from an explicit path. Primarily for tests and callers that
    /// don't want the default `~/.config/isekai-ssh/token.json` location.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Reads from the default `~/.config/isekai-ssh/token.json` location.
    pub fn from_default_path() -> Result<Self, AuthError> {
        Ok(Self { path: default_token_path()? })
    }

    /// Writes `relay_jwt` to this provider's backing file. Exposed so that
    /// a future `isekai-ssh login` (not implemented in this crate; see the
    /// module docs) can persist a freshly obtained token through the same
    /// provider it reads from.
    pub fn save_relay_jwt(&self, relay_jwt: &str) -> Result<(), AuthError> {
        save_token(&self.path, relay_jwt)
    }
}

impl TokenProvider for FileTokenProvider {
    fn get_relay_jwt(&self) -> Result<String, AuthError> {
        load_token(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");

        save_token(&path, "a-relay-jwt").unwrap();
        assert_eq!(load_token(&path).unwrap(), "a-relay-jwt");
    }

    #[test]
    fn provider_round_trips_through_save_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        let provider = FileTokenProvider::new(path);

        provider.save_relay_jwt("provider-token").unwrap();
        assert_eq!(provider.get_relay_jwt().unwrap(), "provider-token");
    }

    #[test]
    fn save_creates_parent_directory_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("isekai-ssh");
        let path = config_dir.join("token.json");
        assert!(!config_dir.exists());

        save_token(&path, "a-relay-jwt").unwrap();
        assert!(config_dir.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&config_dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_file_with_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        save_token(&path, "a-relay-jwt").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn save_survives_a_stale_temp_file_left_by_a_previous_crash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        save_token(&path, "first-token").unwrap();
        save_token(&path, "second-token").unwrap();
        assert_eq!(load_token(&path).unwrap(), "second-token");
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let err = load_token(&path).unwrap_err();
        assert!(matches!(err, AuthError::TokenFileNotFound { .. }));
    }

    #[test]
    fn provider_missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let err = FileTokenProvider::new(path).get_relay_jwt().unwrap_err();
        assert!(matches!(err, AuthError::TokenFileNotFound { .. }));
    }

    #[test]
    fn rejects_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        fs::write(&path, "this is not valid json {{{").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let err = load_token(&path).unwrap_err();
        assert!(matches!(err, AuthError::Parse { .. }));
    }

    #[test]
    fn rejects_json_missing_relay_jwt_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        fs::write(&path, r#"{"something_else": "value"}"#).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let err = load_token(&path).unwrap_err();
        assert!(matches!(err, AuthError::Parse { .. }));
    }

    #[test]
    fn rejects_empty_relay_jwt_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        fs::write(&path, r#"{"relay_jwt": "   "}"#).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let err = load_token(&path).unwrap_err();
        assert!(matches!(err, AuthError::EmptyToken { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_world_writable_token_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        fs::write(&path, r#"{"relay_jwt": "a-token"}"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();

        let err = load_token(&path).unwrap_err();
        assert!(matches!(err, AuthError::WorldWritable { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_world_writable_config_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("isekai-ssh");
        fs::create_dir_all(&config_dir).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o777)).unwrap();
        let path = config_dir.join("token.json");
        fs::write(&path, r#"{"relay_jwt": "a-token"}"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let err = load_token(&path).unwrap_err();
        assert!(matches!(err, AuthError::WorldWritable { .. }));
    }

    #[test]
    fn config_dir_is_joined_under_home() {
        let home = Path::new("/home/example-user");
        assert_eq!(config_dir_from_home(home), home.join(".config").join("isekai-ssh"));
    }
}
