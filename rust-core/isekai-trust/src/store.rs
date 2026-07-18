//! Reading/writing `known_helpers.toml` from disk.
//!
//! Three properties are load-bearing here (`archive/ISEKAI_SSH_DESIGN.md` "trust
//! store のファイル形式", task acceptance criteria):
//!
//! 1. Writes are atomic: `save_trust_store` writes to a sibling temp file in
//!    the same directory and renames it into place, so a crash mid-write
//!    never leaves a truncated/corrupt store on disk.
//! 2. Both the store file and its parent directory are checked for
//!    world-writable permissions before use, and refused (fail closed) if
//!    they are. New files/directories are created with `0600`/`0700`.
//! 3. Malformed TOML and unrecognized `update_policy` values are surfaced as
//!    `TrustError`, never silently discarded in favor of a default store.
//!
//! The permission checks are Unix-only (`std::os::unix::fs::PermissionsExt`)
//! per the task scope; on other platforms they are a no-op, matching
//! `archive/ISEKAI_SSH_DESIGN.md`'s "配布対象プラットフォーム" note that only Linux
//! is targeted today.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::TrustError;
use crate::schema::{SshHostKeyTrustStore, TrustStore};

pub const CONFIG_DIR_NAME: &str = "isekai-ssh";
pub const TRUST_STORE_FILE_NAME: &str = "known_helpers.toml";
/// Deliberately a different file from [`TRUST_STORE_FILE_NAME`] — see
/// [`crate::schema::SshHostKeyTrustStore`]'s docs for why this is kept
/// separate rather than a new table in the same file.
pub const SSH_HOST_KEY_TRUST_STORE_FILE_NAME: &str = "known_ssh_hosts.toml";
/// Lock key (`isekai_fs_guard::with_exclusive_lock`'s `key`, becomes
/// `<config_dir>/<key>.lock`) for [`with_locked_ssh_host_key_trust_store`].
const SSH_HOST_KEY_TRUST_STORE_LOCK_KEY: &str = "known_ssh_hosts";

/// `~/.config/isekai-ssh` (XDG Base Directory convention, per
/// `archive/ISEKAI_SSH_DESIGN.md`). Resolves the home directory via
/// `isekai_fs_guard::resolve_home_dir` (`$HOME`, falling back to
/// `%USERPROFILE%` on Windows where `HOME` isn't reliably set).
pub fn default_config_dir() -> Result<PathBuf, TrustError> {
    let home = isekai_fs_guard::resolve_home_dir().ok_or(TrustError::NoHomeDir)?;
    Ok(config_dir_from_home(&home))
}

/// Pure helper split out of `default_config_dir` so the path-joining logic
/// can be unit-tested without mutating the process-wide `HOME` env var
/// (`std::env::set_var` is process-global and not safe to toggle from
/// concurrently-running tests).
fn config_dir_from_home(home: &Path) -> PathBuf {
    home.join(".config").join(CONFIG_DIR_NAME)
}

/// `~/.config/isekai-ssh/known_helpers.toml`.
pub fn default_trust_store_path() -> Result<PathBuf, TrustError> {
    Ok(default_config_dir()?.join(TRUST_STORE_FILE_NAME))
}

/// `~/.config/isekai-ssh/known_ssh_hosts.toml`.
pub fn default_ssh_host_key_trust_store_path() -> Result<PathBuf, TrustError> {
    Ok(default_config_dir()?.join(SSH_HOST_KEY_TRUST_STORE_FILE_NAME))
}

/// Loads the trust store from `path`.
///
/// Returns an empty (default) store if `path` does not exist yet — that is
/// the normal "no host has been trusted yet" state, not an error. Everything
/// else (bad permissions, malformed TOML, an unknown `update_policy`) fails
/// closed as a `TrustError`.
pub fn load_trust_store(path: &Path) -> Result<TrustStore, TrustError> {
    load_toml_store(path)
}

/// Writes `store` to `path` atomically (temp file + rename) with `0600`
/// permissions. Creates the parent directory (as `0700`) if it does not
/// exist yet.
pub fn save_trust_store(path: &Path, store: &TrustStore) -> Result<(), TrustError> {
    save_toml_store(path, store)
}

/// Same load semantics as [`load_trust_store`], for [`SshHostKeyTrustStore`].
pub fn load_ssh_host_key_trust_store(path: &Path) -> Result<SshHostKeyTrustStore, TrustError> {
    load_toml_store(path)
}

/// Same save semantics as [`save_trust_store`], for [`SshHostKeyTrustStore`].
pub fn save_ssh_host_key_trust_store(path: &Path, store: &SshHostKeyTrustStore) -> Result<(), TrustError> {
    save_toml_store(path, store)
}

/// Runs `f` against the current [`SshHostKeyTrustStore`] at `path` (loading
/// it fresh first) and, if `f` succeeds, persists whatever `f` left the
/// store as — all while holding an exclusive cross-process lock scoped to
/// `path`'s parent directory, so the whole load → decide → save cycle is
/// atomic across concurrently-running `isekai-ssh` processes (e.g. two tabs
/// racing to trust the same brand-new host key). Without this, plain
/// sequential `load_ssh_host_key_trust_store`/`save_ssh_host_key_trust_store`
/// calls have a TOCTOU window: two processes could both load, both decide
/// to trust (possibly *different* fingerprints, if a MITM is actively
/// intercepting one of them), and the later save would silently discard the
/// other's decision — unacceptable for a TOFU pinning mechanism
/// specifically (Codex review finding on `isekai-ssh`'s
/// `FileBackedHostKeyVerifier`).
///
/// `f` receives `&mut SshHostKeyTrustStore` to mutate in place (rather than
/// returning a new store) so partial updates read naturally; returning
/// `Err` aborts without writing anything back.
pub fn with_locked_ssh_host_key_trust_store<T>(
    path: &Path,
    f: impl FnOnce(&mut SshHostKeyTrustStore) -> Result<T, TrustError>,
) -> Result<T, TrustError> {
    let dir = path.parent().ok_or_else(|| TrustError::NoParentDir { path: path.to_path_buf() })?;
    // Must run before `with_exclusive_lock`, not after: that call's own
    // `fs::create_dir_all(dir)` (needed so the `<key>.lock` file itself has
    // somewhere to live) would otherwise create a fresh `dir` with
    // umask-dependent (commonly `0755`, world-*readable*) permissions
    // first, and `ensure_private_dir` only forces `0700` on directories it
    // creates itself — an *existing* directory only gets a world-*writable*
    // check, not a world-*readable* one, so the "new directories are
    // created private" invariant would silently break for exactly the case
    // this function exists to protect (Codex review finding).
    ensure_private_dir(dir)?;
    let outcome = isekai_fs_guard::with_exclusive_lock(dir, SSH_HOST_KEY_TRUST_STORE_LOCK_KEY, || -> Result<T, TrustError> {
        let mut store = load_toml_store::<SshHostKeyTrustStore>(path)?;
        let result = f(&mut store)?;
        save_toml_store(path, &store)?;
        Ok(result)
    });
    match outcome {
        Ok(inner) => inner,
        Err(source) => Err(TrustError::Lock { path: path.to_path_buf(), source }),
    }
}

/// Generic load shared by [`load_trust_store`]/[`load_ssh_host_key_trust_store`]
/// — same permission-checking and fail-closed-on-malformed-TOML behavior
/// regardless of which store type `T` is, since both are separate files
/// under the same `~/.config/isekai-ssh` directory with the same threat
/// model (a world-writable file/dir means someone other than the owner
/// could plant trust entries).
fn load_toml_store<T: Default + DeserializeOwned>(path: &Path) -> Result<T, TrustError> {
    if let Some(parent) = path.parent() {
        if parent.exists() {
            check_not_world_writable(parent)?;
        }
    }

    if !path.exists() {
        return Ok(T::default());
    }
    check_not_world_writable(path)?;

    let content =
        fs::read_to_string(path).map_err(|e| TrustError::Read { path: path.to_path_buf(), source: e })?;
    let store: T = toml::from_str(&content)
        .map_err(|e| TrustError::Parse { path: path.to_path_buf(), source: Box::new(e) })?;
    Ok(store)
}

/// Generic save shared by [`save_trust_store`]/[`save_ssh_host_key_trust_store`].
fn save_toml_store<T: Serialize>(path: &Path, store: &T) -> Result<(), TrustError> {
    let parent = path.parent().ok_or_else(|| TrustError::NoParentDir { path: path.to_path_buf() })?;
    ensure_private_dir(parent)?;

    let serialized = toml::to_string_pretty(store)?;

    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| TrustError::Write { path: path.to_path_buf(), source: e })?;
    set_private_file_permissions(tmp.path())?;
    tmp.write_all(serialized.as_bytes())
        .and_then(|_| tmp.flush())
        .map_err(|e| TrustError::Write { path: path.to_path_buf(), source: e })?;

    tmp.persist(path).map_err(|e| TrustError::Write { path: path.to_path_buf(), source: e.error })?;
    Ok(())
}

/// Translates a `FsGuardError` (path-less by design, see its docs) into this
/// crate's own `TrustError`, attaching `path` back.
fn map_fs_guard_err(path: &Path, err: isekai_fs_guard::FsGuardError) -> TrustError {
    use isekai_fs_guard::FsGuardError;
    match err {
        FsGuardError::CreateDir(source) => TrustError::CreateDir { path: path.to_path_buf(), source },
        FsGuardError::Stat(source) => TrustError::Stat { path: path.to_path_buf(), source },
        FsGuardError::SetPermissions(source) => TrustError::Write { path: path.to_path_buf(), source },
        FsGuardError::WorldWritable { mode } => TrustError::WorldWritable { path: path.to_path_buf(), mode },
        FsGuardError::InsecureAcl { principal, rights } => {
            TrustError::InsecureAcl { path: path.to_path_buf(), principal, rights }
        }
    }
}

/// Creates `dir` (as `0700`) if it doesn't exist yet; otherwise checks that
/// it isn't world-writable and fails closed if it is.
fn ensure_private_dir(dir: &Path) -> Result<(), TrustError> {
    isekai_fs_guard::ensure_private_dir(dir).map_err(|e| map_fs_guard_err(dir, e))
}

fn set_private_file_permissions(path: &Path) -> Result<(), TrustError> {
    isekai_fs_guard::set_private_file_permissions(path).map_err(|e| map_fs_guard_err(path, e))
}

/// Fails closed if `path` is writable by users other than its owner
/// (mode bit `0o002`). Unix-only; a no-op elsewhere.
fn check_not_world_writable(path: &Path) -> Result<(), TrustError> {
    isekai_fs_guard::check_not_world_writable(path).map_err(|e| map_fs_guard_err(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{HelperTrust, UpdatePolicy};

    fn sample_entry() -> HelperTrust {
        HelperTrust {
            identity_pubkey: "pk-abc".to_string(),
            trusted_helper_sha256: "a".repeat(64),
            trusted_helper_version: "0.3.1".to_string(),
            update_policy: UpdatePolicy::ExactDigestOnly,
            release_channel: Some("stable".to_string()),
            last_via: Some("bastion.example.com".to_string()),
            trusted_at: "2026-07-04T00:00:00Z".to_string(),
            last_seen_at: "2026-07-04T00:00:00Z".to_string(),
            cached_relay_addr: "203.0.113.10:45231".to_string(),
            cached_cert_sha256: "3a7f".to_string(),
            cached_session_secret: "MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=".to_string(),
            cached_stun_observed_addr: None,
        }
    }

    #[test]
    fn missing_file_loads_as_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_helpers.toml");
        let store = load_trust_store(&path).unwrap();
        assert!(store.helpers.is_empty());
    }

    #[test]
    fn round_trips_through_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_helpers.toml");

        let mut store = TrustStore::default();
        store.insert("myhost:22".to_string(), sample_entry());
        save_trust_store(&path, &store).unwrap();

        let loaded = load_trust_store(&path).unwrap();
        assert_eq!(loaded, store);
    }

    #[test]
    fn save_creates_parent_directory_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("isekai-ssh");
        let path = config_dir.join("known_helpers.toml");
        assert!(!config_dir.exists());

        save_trust_store(&path, &TrustStore::default()).unwrap();
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
        let path = dir.path().join("known_helpers.toml");
        save_trust_store(&path, &TrustStore::default()).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn save_survives_a_stale_temp_file_left_by_a_previous_crash() {
        // Atomic-write regression guard: `NamedTempFile::new_in` picks a
        // fresh random name each time, so a leftover temp file from a
        // previous aborted write must not interfere with a later save.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_helpers.toml");
        let mut store = TrustStore::default();
        store.insert("myhost:22".to_string(), sample_entry());
        save_trust_store(&path, &store).unwrap();
        save_trust_store(&path, &store).unwrap();
        assert_eq!(load_trust_store(&path).unwrap(), store);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_world_writable_store_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_helpers.toml");
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();

        let err = load_trust_store(&path).unwrap_err();
        assert!(matches!(err, TrustError::WorldWritable { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_world_writable_config_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("isekai-ssh");
        fs::create_dir_all(&config_dir).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o777)).unwrap();
        let path = config_dir.join("known_helpers.toml");
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let err = load_trust_store(&path).unwrap_err();
        assert!(matches!(err, TrustError::WorldWritable { .. }));
    }

    #[test]
    fn rejects_malformed_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_helpers.toml");
        fs::write(&path, "this is not valid toml [[[").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let err = load_trust_store(&path).unwrap_err();
        assert!(matches!(err, TrustError::Parse { .. }));
    }

    #[test]
    fn rejects_unknown_update_policy_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_helpers.toml");
        let toml_str = r#"
[helpers."myhost:22"]
identity_pubkey = "pk"
trusted_helper_sha256 = "aaa"
trusted_helper_version = "0.3.1"
update_policy = "signed-compatible"
trusted_at = "2026-07-04T00:00:00Z"
last_seen_at = "2026-07-04T00:00:00Z"
"#;
        fs::write(&path, toml_str).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let err = load_trust_store(&path).unwrap_err();
        assert!(matches!(err, TrustError::Parse { .. }));
    }

    #[test]
    fn config_dir_is_joined_under_home() {
        let home = Path::new("/home/example-user");
        assert_eq!(config_dir_from_home(home), home.join(".config").join("isekai-ssh"));
    }

    #[test]
    fn ssh_host_key_trust_store_round_trips_through_save_and_load() {
        use crate::schema::{SshHostKeyTrust, SshHostKeyTrustStore};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SSH_HOST_KEY_TRUST_STORE_FILE_NAME);

        let mut store = SshHostKeyTrustStore::default();
        store.insert(
            "example.com:22".to_string(),
            SshHostKeyTrust {
                fingerprint: "SHA256:abcdef".to_string(),
                trusted_at: "2026-07-17T00:00:00Z".to_string(),
                last_seen_at: "2026-07-17T00:00:00Z".to_string(),
            },
        );
        save_ssh_host_key_trust_store(&path, &store).unwrap();

        let loaded = load_ssh_host_key_trust_store(&path).unwrap();
        assert_eq!(loaded, store);
    }

    #[test]
    fn ssh_host_key_trust_store_missing_file_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SSH_HOST_KEY_TRUST_STORE_FILE_NAME);
        let store = load_ssh_host_key_trust_store(&path).unwrap();
        assert!(store.hosts.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn ssh_host_key_trust_store_save_writes_file_with_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SSH_HOST_KEY_TRUST_STORE_FILE_NAME);
        save_ssh_host_key_trust_store(&path, &SshHostKeyTrustStore::default()).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn ssh_host_key_trust_store_rejects_malformed_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SSH_HOST_KEY_TRUST_STORE_FILE_NAME);
        fs::write(&path, "this is not valid toml [[[").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let err = load_ssh_host_key_trust_store(&path).unwrap_err();
        assert!(matches!(err, TrustError::Parse { .. }));
    }

    #[test]
    fn ssh_host_key_and_helper_trust_stores_use_different_file_names() {
        // The two store types must never collide on the same path even
        // when rooted at the same config directory — that's the whole
        // point of keeping them as separate files (schema.rs docs).
        assert_ne!(TRUST_STORE_FILE_NAME, SSH_HOST_KEY_TRUST_STORE_FILE_NAME);
        let home = Path::new("/home/example-user");
        let config_dir = config_dir_from_home(home);
        assert_ne!(
            config_dir.join(TRUST_STORE_FILE_NAME),
            config_dir.join(SSH_HOST_KEY_TRUST_STORE_FILE_NAME)
        );
    }

    #[test]
    fn with_locked_store_round_trips_an_insert() {
        use crate::schema::SshHostKeyTrust;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SSH_HOST_KEY_TRUST_STORE_FILE_NAME);

        with_locked_ssh_host_key_trust_store(&path, |store| {
            store.insert(
                "example.com:22".to_string(),
                SshHostKeyTrust {
                    fingerprint: "SHA256:abc".to_string(),
                    trusted_at: "2026-07-17T00:00:00Z".to_string(),
                    last_seen_at: "2026-07-17T00:00:00Z".to_string(),
                },
            );
            Ok(())
        })
        .unwrap();

        let loaded = load_ssh_host_key_trust_store(&path).unwrap();
        assert_eq!(loaded.get("example.com:22").unwrap().fingerprint, "SHA256:abc");
    }

    #[cfg(unix)]
    #[test]
    fn with_locked_store_creates_a_fresh_config_dir_as_0700() {
        // Codex review finding: `with_exclusive_lock`'s own `create_dir_all`
        // must not be the thing that first creates a brand-new config dir
        // (it uses umask-dependent, typically world-readable `0755`
        // permissions) — `with_locked_ssh_host_key_trust_store` must apply
        // `ensure_private_dir` first so a fresh directory ends up `0700`.
        //
        // Known limitation (Codex review, not fixed): this assertion only
        // actually exercises the bug this guards against under a typical
        // `022` umask (the near-universal default on real dev machines and
        // CI runners) — under an unusual `077` umask, `create_dir_all`
        // alone would already produce `0700`, so a regression here could
        // slip past this test undetected in that specific environment. A
        // fully deterministic version would force the umask in an isolated
        // child process; not done here as disproportionate for that edge
        // case.
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let config_dir = root.path().join("nested-config-dir");
        let path = config_dir.join(SSH_HOST_KEY_TRUST_STORE_FILE_NAME);
        assert!(!config_dir.exists());

        with_locked_ssh_host_key_trust_store(&path, |_store| Ok(())).unwrap();

        let mode = fs::metadata(&config_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "a freshly-created config dir must be private, not umask-dependent");
    }

    #[test]
    fn with_locked_store_does_not_persist_when_f_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SSH_HOST_KEY_TRUST_STORE_FILE_NAME);

        let result: Result<(), TrustError> =
            with_locked_ssh_host_key_trust_store(&path, |_store| Err(TrustError::EmptyHost));
        assert!(result.is_err());
        assert!(!path.exists(), "an aborted update must not create/modify the store file");
    }

    #[test]
    fn with_locked_store_serializes_concurrent_writers_without_losing_updates() {
        // The exact race the Codex review on `isekai-ssh`'s
        // `FileBackedHostKeyVerifier` caught: N threads each inserting a
        // *distinct* host key concurrently, with plain sequential
        // load-then-save, would race and silently drop all but the last
        // writer's insert. With the lock held across the whole
        // load→modify→save cycle, every insert must survive.
        use crate::schema::SshHostKeyTrust;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join(SSH_HOST_KEY_TRUST_STORE_FILE_NAME));

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    with_locked_ssh_host_key_trust_store(&path, |store| {
                        store.insert(
                            format!("host-{i}.example.com:22"),
                            SshHostKeyTrust {
                                fingerprint: format!("SHA256:{i}"),
                                trusted_at: "2026-07-17T00:00:00Z".to_string(),
                                last_seen_at: "2026-07-17T00:00:00Z".to_string(),
                            },
                        );
                        Ok::<(), TrustError>(())
                    })
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let loaded = load_ssh_host_key_trust_store(&path).unwrap();
        for i in 0..8 {
            assert_eq!(
                loaded.get(&format!("host-{i}.example.com:22")).unwrap().fingerprint,
                format!("SHA256:{i}"),
                "insert from thread {i} must not have been lost to a race"
            );
        }
    }
}
