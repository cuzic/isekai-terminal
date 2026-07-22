//! Local-only build-profile config for Epic P ("リモート発ビルドトリガー",
//! `ISEKAI_PIPE_DESIGN.md` §8 Epic P): what a remote `isekai-pipe ctl build
//! <name>` invocation actually runs on *this* machine.
//!
//! Deliberately not an `#@isekai` ssh_config directive (unlike
//! bootstrap/transport settings, this is local automation config, not
//! connection config — a shell command with args doesn't fit ssh_config's
//! directive-line quoting well, and `wrapper/config.rs`'s
//! `IsekaiConfigBuilder` would grow a very different kind of field). Its own
//! TOML file under the same `~/.config/isekai-ssh/` directory
//! `isekai_trust::store` already uses for `known_helpers.toml`/
//! `known_ssh_hosts.toml`.
//!
//! `profile.host` scopes which SSH destination may invoke a given profile —
//! this is the whole security boundary: the wire only ever carries a
//! `profile` *name* (`isekai_protocol::CtlMessage::BuildRequest`), never a
//! raw command, and a name only resolves to something if this file already
//! maps `(host, name)` to a command a human configured ahead of time.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const BUILD_PROFILES_FILE_NAME: &str = "build_profiles.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildProfileStore {
    #[serde(default, rename = "profile")]
    pub profiles: Vec<BuildProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildProfile {
    /// The ssh_config `Host` alias this profile may be invoked from —
    /// matched against `IsekaiConfig::profile`/the wrapper's resolved
    /// destination, *not* the remote-supplied `CtlMessage::BuildRequest`
    /// (which never carries a host).
    pub host: String,
    /// The name a remote `isekai-pipe ctl build <name>` references.
    pub name: String,
    /// Working directory the command runs in, on this (client) machine.
    pub dir: String,
    /// Shell command line, run via the local platform shell
    /// (`sh -c`/`cmd /C`) so it can contain `&&`/pipes/etc.
    pub command: String,
    /// Glob (relative to `dir`) matching build output to push back to the
    /// remote host once the command exits. Omit for profiles that only
    /// stream logs and never transfer a result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_glob: Option<String>,
    /// Remote destination directory `result_glob` matches are pushed into
    /// (via a recursive `isekai-ssh <host> -- cat > ...` invocation, not a
    /// ctl-socket message — see `ISEKAI_PIPE_DESIGN.md` §8 Epic P). Required
    /// whenever `result_glob` is set; `add()` enforces this pairing so a
    /// profile can never silently build something it has nowhere to send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest_dir: Option<String>,
}

/// `~/.config/isekai-ssh/build_profiles.toml` — same directory
/// `isekai_trust::store::default_config_dir` resolves, so build-profile
/// config lives alongside the trust store rather than inventing a second
/// config-directory convention.
pub fn default_build_profiles_path() -> Result<PathBuf> {
    Ok(isekai_trust::store::default_config_dir()
        .context("isekai-ssh: could not determine the config directory")?
        .join(BUILD_PROFILES_FILE_NAME))
}

/// Returns an empty store if `path` does not exist yet (the normal
/// "no build profile registered yet" state, not an error). Malformed TOML
/// fails closed, same as `isekai_trust::store::load_trust_store`.
pub fn load_build_profiles(path: &Path) -> Result<BuildProfileStore> {
    match fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents)
            .with_context(|| format!("isekai-ssh: failed to parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BuildProfileStore::default()),
        Err(e) => Err(e).with_context(|| format!("isekai-ssh: failed to read {}", path.display())),
    }
}

/// Writes `store` to `path` atomically (temp file + rename), creating the
/// parent directory privately if needed — same invariants as
/// `isekai_trust::store::save_trust_store`, reimplemented here rather than
/// shared because that crate's `load_toml_store`/`save_toml_store` helpers
/// are private to it (scoped to its own `TrustError`, not a generic API).
pub fn save_build_profiles(path: &Path, store: &BuildProfileStore) -> Result<()> {
    let dir = path.parent().context("isekai-ssh: build profiles path has no parent directory")?;
    isekai_fs_guard::ensure_private_dir(dir)
        .map_err(|e| anyhow::anyhow!("isekai-ssh: failed to prepare {}: {e:?}", dir.display()))?;

    let serialized = toml::to_string_pretty(store).context("isekai-ssh: failed to serialize build profiles")?;
    let tmp_path = dir.join(format!(".{BUILD_PROFILES_FILE_NAME}.tmp"));
    {
        let mut tmp_file = fs::File::create(&tmp_path)
            .with_context(|| format!("isekai-ssh: failed to create {}", tmp_path.display()))?;
        tmp_file
            .write_all(serialized.as_bytes())
            .with_context(|| format!("isekai-ssh: failed to write {}", tmp_path.display()))?;
    }
    isekai_fs_guard::set_private_file_permissions(&tmp_path)
        .map_err(|e| anyhow::anyhow!("isekai-ssh: failed to chmod {}: {e:?}", tmp_path.display()))?;
    fs::rename(&tmp_path, path)
        .with_context(|| format!("isekai-ssh: failed to rename {} to {}", tmp_path.display(), path.display()))?;
    Ok(())
}

/// Looks up the profile a `CtlMessage::BuildRequest { profile }` from `host`
/// may invoke. `host` is this connection's own resolved destination (never
/// remote-supplied), so a compromised remote cannot reference a profile
/// registered for a *different* host.
pub fn find_profile<'a>(store: &'a BuildProfileStore, host: &str, name: &str) -> Option<&'a BuildProfile> {
    store.profiles.iter().find(|p| p.host == host && p.name == name)
}

/// Inserts or replaces the `(host, name)` entry. Fails if `result_glob` is
/// set without `dest_dir` (or vice versa) — a profile that builds something
/// but has nowhere to send it, or names a destination for a result it never
/// declared, is almost certainly a typo, not a considered choice.
pub fn upsert_profile(store: &mut BuildProfileStore, profile: BuildProfile) -> Result<()> {
    if profile.result_glob.is_some() != profile.dest_dir.is_some() {
        anyhow::bail!(
            "isekai-ssh build-profile: --result-glob and --dest-dir must be given together (or both omitted)"
        );
    }
    store.profiles.retain(|p| !(p.host == profile.host && p.name == profile.name));
    store.profiles.push(profile);
    Ok(())
}

/// Removes the `(host, name)` entry, if present. Returns whether anything
/// was actually removed, so callers can report "nothing to remove" instead
/// of silently no-op'ing.
pub fn remove_profile(store: &mut BuildProfileStore, host: &str, name: &str) -> bool {
    let before = store.profiles.len();
    store.profiles.retain(|p| !(p.host == host && p.name == name));
    store.profiles.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_profile() -> BuildProfile {
        BuildProfile {
            host: "mybox".to_string(),
            name: "win".to_string(),
            dir: "/home/user/isekai-terminal".to_string(),
            command: "cargo build --release".to_string(),
            result_glob: Some("target/release/*.exe".to_string()),
            dest_dir: Some("~/isekai-build-results/win".to_string()),
        }
    }

    #[test]
    fn load_missing_file_returns_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BUILD_PROFILES_FILE_NAME);
        let store = load_build_profiles(&path).unwrap();
        assert!(store.profiles.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BUILD_PROFILES_FILE_NAME);
        let mut store = BuildProfileStore::default();
        upsert_profile(&mut store, sample_profile()).unwrap();
        save_build_profiles(&path, &store).unwrap();

        let loaded = load_build_profiles(&path).unwrap();
        assert_eq!(loaded, store);
    }

    #[test]
    fn save_creates_a_private_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("isekai-ssh");
        let path = config_dir.join(BUILD_PROFILES_FILE_NAME);
        save_build_profiles(&path, &BuildProfileStore::default()).unwrap();

        let mode = fs::metadata(&config_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert!(path.exists());
    }

    #[test]
    fn load_rejects_malformed_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BUILD_PROFILES_FILE_NAME);
        fs::write(&path, "not valid toml [[[").unwrap();
        assert!(load_build_profiles(&path).is_err());
    }

    #[test]
    fn find_profile_scopes_by_host_and_name() {
        let mut store = BuildProfileStore::default();
        upsert_profile(&mut store, sample_profile()).unwrap();

        assert!(find_profile(&store, "mybox", "win").is_some());
        assert!(find_profile(&store, "otherbox", "win").is_none());
        assert!(find_profile(&store, "mybox", "linux").is_none());
    }

    #[test]
    fn upsert_replaces_an_existing_entry_for_the_same_host_and_name() {
        let mut store = BuildProfileStore::default();
        upsert_profile(&mut store, sample_profile()).unwrap();
        let mut updated = sample_profile();
        updated.command = "cargo build --release --target x86_64-pc-windows-msvc".to_string();
        upsert_profile(&mut store, updated.clone()).unwrap();

        assert_eq!(store.profiles.len(), 1);
        assert_eq!(store.profiles[0], updated);
    }

    #[test]
    fn upsert_rejects_result_glob_without_dest_dir() {
        let mut store = BuildProfileStore::default();
        let mut profile = sample_profile();
        profile.dest_dir = None;
        assert!(upsert_profile(&mut store, profile).is_err());
    }

    #[test]
    fn upsert_rejects_dest_dir_without_result_glob() {
        let mut store = BuildProfileStore::default();
        let mut profile = sample_profile();
        profile.result_glob = None;
        assert!(upsert_profile(&mut store, profile).is_err());
    }

    #[test]
    fn upsert_allows_omitting_both_result_glob_and_dest_dir() {
        let mut store = BuildProfileStore::default();
        let mut profile = sample_profile();
        profile.result_glob = None;
        profile.dest_dir = None;
        assert!(upsert_profile(&mut store, profile).is_ok());
    }

    #[test]
    fn remove_profile_reports_whether_anything_was_removed() {
        let mut store = BuildProfileStore::default();
        upsert_profile(&mut store, sample_profile()).unwrap();

        assert!(remove_profile(&mut store, "mybox", "win"));
        assert!(!remove_profile(&mut store, "mybox", "win"));
    }
}
