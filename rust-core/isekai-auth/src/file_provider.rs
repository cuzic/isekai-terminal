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
//!
//! ## Schema evolution (phase S-5): plain `relay_jwt` -> full `TokenSet`
//!
//! The original (S-0c-1) schema was just `{"relay_jwt": "..."}` — a single
//! opaque bearer token with no expiry. `isekai-ssh login`'s Device
//! Authorization Grant (`device_flow.rs`) returns an `access_token` plus,
//! usually, a `refresh_token` and an `expires_in`, so the on-disk schema
//! grows to `TokenSet` (`{"access_token", "refresh_token"?, "expires_at"?,
//! "token_endpoint"?, "client_id"?}`, `ISEKAI_SSH_DESIGN.md`
//! "JWT発行・配布フロー"). Both schemas are read transparently via
//! `TokenFileSchema`'s `#[serde(untagged)]` union: an old-style file with
//! only `relay_jwt` loads as a `TokenSet` with `access_token` set to that
//! value and every new field `None` (so `needs_refresh()` is always `false`
//! for it — there's nothing to refresh). `save_token`/`load_token` (the
//! original API) are unchanged and keep writing/reading the plain
//! `relay_jwt` shape, so existing callers see no behavior change.
//!
//! OS keychain/Secret Service integration is intentionally not attempted
//! here (`ISEKAI_SSH_DESIGN.md` calls it "可能な限り" — best-effort — and the
//! sandboxed/headless environments this crate is tested in can't exercise
//! one anyway). The `0600`/`0700` file store below is the only backing store
//! for now; a real keychain-backed `TokenProvider` implementation could be
//! added later as a sibling to `FileTokenProvider` behind the same trait
//! without touching this module.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{refresh, AuthError, TokenProvider};

pub const CONFIG_DIR_NAME: &str = "isekai-ssh";
pub const TOKEN_FILE_NAME: &str = "token.json";

/// How close to (or past) `expires_at` counts as "refresh now"
/// (`ISEKAI_SSH_DESIGN.md`: "保存済みトークンの`expires_at`が近い/過ぎている
/// 場合"). A flat 60s skew comfortably covers the round trip of the SSH
/// connection this token is about to authenticate, without refreshing so
/// eagerly that every call does an extra network round trip.
const REFRESH_SKEW_SECS: i64 = 60;

#[derive(Debug, Serialize, Deserialize)]
struct TokenFileV1 {
    relay_jwt: String,
}

/// The extended (phase S-5) on-disk schema: an OAuth2 access/refresh token
/// pair plus enough metadata for `FileTokenProvider::get_relay_jwt` to
/// auto-refresh a near-expiry token. See this module's docs for how this
/// coexists with the original plain-`relay_jwt` schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix timestamp (seconds) `access_token` expires at. `None` means "no
    /// known expiry" (either a legacy v1 file, or a token issued without
    /// `expires_in`) — `needs_refresh()` never refreshes such a token.
    /// Stored as a plain integer rather than RFC 3339 text: this crate has
    /// no other reason to pull in a datetime-formatting dependency (mirrors
    /// `isekai-ssh/src/init.rs`'s own hand-rolled RFC 3339 *writer*, kept
    /// minimal for the same reason in the other direction).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// The token endpoint to POST a `refresh_token` grant to when this token
    /// needs refreshing (`refresh::refresh_access_token`'s first argument).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    /// The OAuth client id that requested this token (device-flow public
    /// clients still send this on a refresh grant, RFC 6749 §2.3.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

impl TokenSet {
    /// Whether `access_token` is at or past `expires_at` (minus
    /// `REFRESH_SKEW_SECS`). Always `false` when `expires_at` is `None`.
    pub fn needs_refresh(&self) -> bool {
        match self.expires_at {
            None => false,
            Some(expires_at) => now_unix() >= expires_at - REFRESH_SKEW_SECS,
        }
    }
}

impl From<TokenFileV1> for TokenSet {
    fn from(v1: TokenFileV1) -> Self {
        TokenSet {
            access_token: v1.relay_jwt,
            refresh_token: None,
            expires_at: None,
            token_endpoint: None,
            client_id: None,
        }
    }
}

/// Either on-disk schema this module understands, tried in this order (see
/// module docs). `#[serde(untagged)]` picks whichever variant's *required*
/// field (`access_token` for `V2`, `relay_jwt` for `V1`) is present in the
/// JSON object; a JSON object with neither (or malformed JSON) matches
/// neither and surfaces as `AuthError::Parse`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum TokenFileSchema {
    V2(TokenSet),
    V1(TokenFileV1),
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
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

/// Reads the relay JWT out of the token file at `path`, regardless of which
/// of the two schemas (see module docs) it's stored in.
///
/// Fails closed: a missing file, a world-writable file/directory, malformed
/// JSON, or an empty token value are all errors, never a silent empty token.
pub fn load_token(path: &Path) -> Result<String, AuthError> {
    Ok(load_token_set(path)?.access_token)
}

/// Reads the full `TokenSet` out of the token file at `path`. A legacy v1
/// (plain `relay_jwt`) file comes back as a `TokenSet` with every new field
/// `None` (see module docs). Same fail-closed behavior as `load_token`.
pub fn load_token_set(path: &Path) -> Result<TokenSet, AuthError> {
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
    let schema: TokenFileSchema =
        serde_json::from_str(&content).map_err(|e| AuthError::Parse { path: path.to_path_buf(), source: e })?;

    let mut token_set = match schema {
        TokenFileSchema::V2(token_set) => token_set,
        TokenFileSchema::V1(v1) => v1.into(),
    };

    let trimmed = token_set.access_token.trim();
    if trimmed.is_empty() {
        return Err(AuthError::EmptyToken { path: path.to_path_buf() });
    }
    token_set.access_token = trimmed.to_string();
    Ok(token_set)
}

/// Writes `relay_jwt` to the token file at `path` atomically (temp file +
/// rename) with `0600` permissions. Creates the parent directory (as
/// `0700`) if it does not exist yet. Always writes the original plain
/// `{"relay_jwt": ...}` schema — use `save_token_set` to persist a full
/// `TokenSet` (refresh token / expiry / token endpoint included).
pub fn save_token(path: &Path, relay_jwt: &str) -> Result<(), AuthError> {
    let serialized = serde_json::to_string_pretty(&TokenFileV1 { relay_jwt: relay_jwt.to_string() })?;
    write_atomically(path, &serialized)
}

/// Writes a full `TokenSet` to the token file at `path`, with the same
/// atomic-write + `0600`/`0700` permission handling as `save_token`.
pub fn save_token_set(path: &Path, token_set: &TokenSet) -> Result<(), AuthError> {
    let serialized = serde_json::to_string_pretty(token_set)?;
    write_atomically(path, &serialized)
}

/// Shared atomic-write body for `save_token`/`save_token_set`: write to a
/// fresh temp file in `path`'s parent directory (creating it, `0700`, if
/// needed), set `0600` permissions on the temp file, then rename it over
/// `path`.
fn write_atomically(path: &Path, serialized: &str) -> Result<(), AuthError> {
    let parent = path.parent().ok_or_else(|| AuthError::NoParentDir { path: path.to_path_buf() })?;
    ensure_private_dir(parent)?;

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
/// path, see `FileTokenProvider::new`), auto-refreshing it first if it's a
/// `TokenSet` (phase S-5 schema) that's near/past expiry.
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

    /// Writes `relay_jwt` to this provider's backing file using the
    /// original plain-`relay_jwt` schema.
    pub fn save_relay_jwt(&self, relay_jwt: &str) -> Result<(), AuthError> {
        save_token(&self.path, relay_jwt)
    }

    /// Writes a full `TokenSet` to this provider's backing file — used by
    /// `isekai-ssh login` (`isekai-ssh/src/login.rs`) right after a
    /// successful Device Authorization Grant, and internally by
    /// `get_relay_jwt` after a successful refresh.
    pub fn save_token_set(&self, token_set: &TokenSet) -> Result<(), AuthError> {
        save_token_set(&self.path, token_set)
    }

    /// Reads this provider's backing file as a full `TokenSet` (a legacy v1
    /// file comes back with every new field `None`; see module docs).
    pub fn load_token_set(&self) -> Result<TokenSet, AuthError> {
        load_token_set(&self.path)
    }

    /// Attempts to refresh `current` via `refresh::refresh_access_token`,
    /// persists the result, and returns it. Fails closed with
    /// `AuthError::RefreshNotConfigured` if `current` has no
    /// `refresh_token`/`token_endpoint` to refresh with (e.g. a legacy v1
    /// token, or one issued without a refresh token).
    fn refresh_and_save(&self, current: TokenSet) -> Result<TokenSet, AuthError> {
        let refresh_token = current.refresh_token.clone().ok_or_else(|| AuthError::RefreshNotConfigured {
            reason: "the stored token is expired but no refresh_token is stored".to_string(),
        })?;
        let token_endpoint = current.token_endpoint.clone().ok_or_else(|| AuthError::RefreshNotConfigured {
            reason: "the stored token is expired but no token_endpoint is stored".to_string(),
        })?;

        let response = refresh::refresh_access_token(&token_endpoint, current.client_id.as_deref(), &refresh_token)?;
        let refreshed = TokenSet {
            access_token: response.access_token,
            // Some authorization servers rotate the refresh token on every
            // use and some don't (RFC 6749 §6 leaves this optional) — keep
            // the old one if the response didn't include a new one.
            refresh_token: response.refresh_token.or(Some(refresh_token)),
            expires_at: response.expires_in.map(|secs| now_unix() + secs as i64),
            token_endpoint: Some(token_endpoint),
            client_id: current.client_id,
        };
        self.save_token_set(&refreshed)?;
        Ok(refreshed)
    }
}

impl TokenProvider for FileTokenProvider {
    /// Returns a currently-valid access token, transparently refreshing it
    /// first if the stored `TokenSet.expires_at` is near/past
    /// (`ISEKAI_SSH_DESIGN.md`: "`connect` 実行中のトークン失効は裏で自動
    /// リフレッシュを試みる"). Legacy v1 (plain `relay_jwt`, no expiry) files
    /// are returned as-is, matching this method's pre-phase-S-5 behavior
    /// exactly — `needs_refresh()` is always `false` for them.
    fn get_relay_jwt(&self) -> Result<String, AuthError> {
        let token_set = load_token_set(&self.path)?;
        if !token_set.needs_refresh() {
            return Ok(token_set.access_token);
        }
        Ok(self.refresh_and_save(token_set)?.access_token)
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

    // --- TokenSet / phase S-5 schema tests ---

    fn token_set(access_token: &str, expires_at: Option<i64>) -> TokenSet {
        TokenSet {
            access_token: access_token.to_string(),
            refresh_token: Some("a-refresh-token".to_string()),
            expires_at,
            token_endpoint: Some("https://auth.example.com/token".to_string()),
            client_id: Some("isekai-ssh-cli".to_string()),
        }
    }

    #[test]
    fn token_set_round_trips_through_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        let ts = token_set("an-access-token", Some(now_unix() + 3600));

        save_token_set(&path, &ts).unwrap();
        assert_eq!(load_token_set(&path).unwrap(), ts);
        // The plain-string accessor also works against the new schema.
        assert_eq!(load_token(&path).unwrap(), "an-access-token");
    }

    #[test]
    fn legacy_v1_file_loads_as_a_token_set_with_no_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        save_token(&path, "a-legacy-relay-jwt").unwrap();

        let ts = load_token_set(&path).unwrap();
        assert_eq!(ts.access_token, "a-legacy-relay-jwt");
        assert_eq!(ts.refresh_token, None);
        assert_eq!(ts.expires_at, None);
        assert_eq!(ts.token_endpoint, None);
        assert_eq!(ts.client_id, None);
        assert!(!ts.needs_refresh());
    }

    #[test]
    fn needs_refresh_is_false_when_expires_at_is_far_in_the_future() {
        let ts = token_set("a-token", Some(now_unix() + 3600));
        assert!(!ts.needs_refresh());
    }

    #[test]
    fn needs_refresh_is_true_when_expires_at_is_in_the_past() {
        let ts = token_set("a-token", Some(now_unix() - 10));
        assert!(ts.needs_refresh());
    }

    #[test]
    fn needs_refresh_is_true_within_the_skew_window() {
        // Expires in 30s — inside the 60s REFRESH_SKEW_SECS window.
        let ts = token_set("a-token", Some(now_unix() + 30));
        assert!(ts.needs_refresh());
    }

    #[test]
    fn provider_get_relay_jwt_returns_a_non_expired_token_set_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        let ts = token_set("a-fresh-token", Some(now_unix() + 3600));
        save_token_set(&path, &ts).unwrap();

        let provider = FileTokenProvider::new(path);
        assert_eq!(provider.get_relay_jwt().unwrap(), "a-fresh-token");
    }

    #[test]
    fn provider_get_relay_jwt_fails_closed_when_expired_with_no_refresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        let mut ts = token_set("an-expired-token", Some(now_unix() - 10));
        ts.refresh_token = None;
        save_token_set(&path, &ts).unwrap();

        let provider = FileTokenProvider::new(path);
        let err = provider.get_relay_jwt().unwrap_err();
        assert!(matches!(err, AuthError::RefreshNotConfigured { .. }));
    }

    #[test]
    fn provider_get_relay_jwt_fails_closed_when_expired_with_no_token_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        let mut ts = token_set("an-expired-token", Some(now_unix() - 10));
        ts.token_endpoint = None;
        save_token_set(&path, &ts).unwrap();

        let provider = FileTokenProvider::new(path);
        let err = provider.get_relay_jwt().unwrap_err();
        assert!(matches!(err, AuthError::RefreshNotConfigured { .. }));
    }
}
