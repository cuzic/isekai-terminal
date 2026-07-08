//! Persistent profile schema (`chatgpt.md` §13 "永続profile") and its
//! migration path from the legacy `known_helpers.toml` trust store
//! (`isekai_trust::schema::{TrustStore, HelperTrust}`,
//! `archive/ISEKAI_PIPE_MIGRATION.md` P5 "旧名整理").
//!
//! `known_helpers.toml` is keyed by `host:port` and stores a single cached
//! relay address/session secret per entry -- it has no concept of
//! `peer_id`, multiple candidate sources, or a `last_path_hint`. This module
//! does not invent values for fields the legacy schema cannot supply, and
//! it does not remove or rewrite `known_helpers.toml` itself: per
//! `archive/ISEKAI_PIPE_MIGRATION.md`'s "互換名" note, behavior changes and name
//! changes ship in separate PRs, so callers keep reading/writing the legacy
//! store exactly as before until something is wired up to actually prefer
//! `PersistentProfile`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use isekai_trust::schema::{HelperTrust, TrustStore};

use crate::{IntentTransport, RelayPolicy, ServerIdentity};

pub const PERSISTENT_PROFILE_SCHEMA_VERSION: u32 = 1;

/// `chatgpt.md` §13's persistent profile document, one per logical host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistentProfile {
    pub schema_version: u32,
    pub profile: String,
    /// Not tracked by the legacy `known_helpers.toml` schema -- always
    /// `None` for a migrated profile until a real ISEKAI-link/rendezvous
    /// peer identity is assigned (`chatgpt.md` §4 `PeerId`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    pub server_identity: ServerIdentity,
    pub service: String,
    #[serde(default)]
    pub link_endpoints: Vec<String>,
    #[serde(default)]
    pub rendezvous: Vec<String>,
    #[serde(default)]
    pub stun_servers: Vec<String>,
    #[serde(default)]
    pub relay_endpoints: Vec<String>,
    pub relay_policy: RelayPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_bootstrap_at: Option<String>,
    /// Not tracked by the legacy schema -- always `None` for a migrated
    /// profile (`chatgpt.md` §13's `last_path_hint`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_path_hint: Option<PathHint>,
    /// Bridges back to the single-cached-relay-address transport that
    /// `known_helpers.toml`-backed connects still use today
    /// (`isekai-ssh::wrapper::build_connection_intent`,
    /// `isekai-pipe::intent_from_profile`). Present only for profiles
    /// migrated from (or still mastered by) a `known_helpers.toml` entry;
    /// a profile created after real candidate-source exchange
    /// (`chatgpt.md` §17-20) replaces this with populated
    /// `link_endpoints`/`stun_servers`/`relay_endpoints` and leaves this
    /// `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_relay_transport: Option<LegacyRelayTransport>,
}

/// `chatgpt.md` §13's `last_path_hint`: a short-lived observation used only
/// to bias the next candidate search, never a permanent connection target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathHint {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// The one piece of `known_helpers.toml` (`HelperTrust`) that current
/// `connect` code paths actually dial: a single relay-reachable helper
/// address plus the session secret needed for the HELLO/proof handshake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacyRelayTransport {
    pub helper_addr: String,
    pub session_secret_b64: String,
}

impl PersistentProfile {
    /// Converts a single `known_helpers.toml` entry (keyed by `host:port`,
    /// e.g. `isekai_trust::normalize_host_port`'s output) into the new
    /// schema. Fields the legacy schema cannot supply (`peer_id`,
    /// `link_endpoints`, `rendezvous`, `stun_servers`, `relay_endpoints`,
    /// `last_path_hint`) are left empty/`None` rather than guessed.
    pub fn migrate_legacy_helper_trust(profile_name: &str, trust: &HelperTrust) -> Self {
        Self {
            schema_version: PERSISTENT_PROFILE_SCHEMA_VERSION,
            profile: profile_name.to_string(),
            peer_id: None,
            server_identity: ServerIdentity {
                cert_sha256_hex: trust.cached_cert_sha256.clone(),
            },
            service: "ssh".to_string(),
            link_endpoints: Vec::new(),
            rendezvous: Vec::new(),
            stun_servers: Vec::new(),
            relay_endpoints: Vec::new(),
            relay_policy: RelayPolicy::RelayAllowed,
            remote_version: Some(trust.trusted_helper_version.clone()),
            last_bootstrap_at: Some(trust.trusted_at.clone()),
            last_path_hint: None,
            legacy_relay_transport: Some(LegacyRelayTransport {
                helper_addr: trust.cached_relay_addr.clone(),
                session_secret_b64: trust.cached_session_secret.clone(),
            }),
        }
    }

    /// Reconstructs the `IntentTransport::Relay` that current `connect`
    /// code paths need, when this profile still carries a
    /// `legacy_relay_transport` bridge. Returns `None` once a profile has
    /// moved past the single-cached-relay-address model (no bridge left to
    /// reconstruct from).
    pub fn to_legacy_relay_transport(&self) -> Option<IntentTransport> {
        self.legacy_relay_transport
            .as_ref()
            .map(|legacy| IntentTransport::Relay {
                helper_addr: legacy.helper_addr.clone(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: legacy.session_secret_b64.clone(),
            })
    }
}

/// Migrates every entry in a loaded `known_helpers.toml` document. Pure
/// (no I/O): callers load the `TrustStore` with `isekai_trust::load_trust_store`
/// as they already do, and separately decide whether/where to persist the
/// result (`write_persistent_profile` below, or nothing at all for a
/// dry-run/inspection use).
pub fn migrate_trust_store(store: &TrustStore) -> Vec<PersistentProfile> {
    store
        .helpers
        .iter()
        .map(|(key, trust)| PersistentProfile::migrate_legacy_helper_trust(key, trust))
        .collect()
}

/// `chatgpt.md` §33's `state/profiles/` layout, rooted the same way
/// `default_runtime_dir` roots `runtime/` (an env override, else a
/// platform-conventional per-user directory).
pub fn default_profiles_dir() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("ISEKAI_PIPE_PROFILES_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("isekai").join("profiles"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("isekai")
            .join("profiles"));
    }
    Ok(std::env::temp_dir().join("isekai-profiles"))
}

/// Writes `profile` to `<dir>/<profile.profile>.json`, atomically (write to
/// a sibling temp file, then rename) and with owner-only permissions,
/// mirroring `write_connection_intent`'s approach in `lib.rs`.
pub fn write_persistent_profile(dir: &Path, profile: &PersistentProfile) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let path = dir.join(format!("{}.json", profile.profile));
    let tmp = dir.join(format!("{}.{}.tmp", profile.profile, std::process::id()));
    let bytes = serde_json::to_vec_pretty(profile)?;
    fs::write(&tmp, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Loads a previously written persistent profile, if present.
pub fn load_persistent_profile(dir: &Path, profile_name: &str) -> io::Result<Option<PersistentProfile>> {
    let path = dir.join(format!("{profile_name}.json"));
    match fs::read(&path) {
        Ok(bytes) => {
            let profile = serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(Some(profile))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isekai_trust::schema::UpdatePolicy;

    fn sample_trust() -> HelperTrust {
        HelperTrust {
            identity_pubkey: "pk-abc".to_string(),
            trusted_helper_sha256: "a".repeat(64),
            trusted_helper_version: "0.3.1".to_string(),
            update_policy: UpdatePolicy::ExactDigestOnly,
            release_channel: Some("stable".to_string()),
            last_via: Some("bastion.example.com".to_string()),
            trusted_at: "2026-07-04T00:00:00Z".to_string(),
            last_seen_at: "2026-07-05T00:00:00Z".to_string(),
            cached_relay_addr: "203.0.113.10:45231".to_string(),
            cached_cert_sha256: "3a7f".repeat(16),
            cached_session_secret: "c2VjcmV0MTIzNDU2Nzg5MDEyMzQ1Njc4OTAxMjM=".to_string(),
        }
    }

    #[test]
    fn migrates_legacy_entry_into_new_schema() {
        let trust = sample_trust();
        let profile = PersistentProfile::migrate_legacy_helper_trust("myhost:22", &trust);

        assert_eq!(profile.schema_version, PERSISTENT_PROFILE_SCHEMA_VERSION);
        assert_eq!(profile.profile, "myhost:22");
        assert_eq!(profile.peer_id, None);
        assert_eq!(profile.server_identity.cert_sha256_hex, trust.cached_cert_sha256);
        assert_eq!(profile.service, "ssh");
        assert!(profile.link_endpoints.is_empty());
        assert!(profile.stun_servers.is_empty());
        assert_eq!(profile.relay_policy, RelayPolicy::RelayAllowed);
        assert_eq!(profile.remote_version.as_deref(), Some("0.3.1"));
        assert_eq!(profile.last_bootstrap_at.as_deref(), Some("2026-07-04T00:00:00Z"));
        assert_eq!(profile.last_path_hint, None);
        assert_eq!(
            profile.legacy_relay_transport,
            Some(LegacyRelayTransport {
                helper_addr: trust.cached_relay_addr.clone(),
                session_secret_b64: trust.cached_session_secret.clone(),
            })
        );
    }

    #[test]
    fn rebuilds_relay_transport_from_migrated_profile() {
        let trust = sample_trust();
        let profile = PersistentProfile::migrate_legacy_helper_trust("myhost:22", &trust);

        assert_eq!(
            profile.to_legacy_relay_transport(),
            Some(IntentTransport::Relay {
                helper_addr: trust.cached_relay_addr,
                server_name: "isekai-helper".to_string(),
                session_secret_b64: trust.cached_session_secret,
            })
        );
    }

    #[test]
    fn profile_without_legacy_bridge_has_no_relay_transport() {
        let profile = PersistentProfile {
            schema_version: PERSISTENT_PROFILE_SCHEMA_VERSION,
            profile: "future-host".to_string(),
            peer_id: Some("peer_01".to_string()),
            server_identity: ServerIdentity {
                cert_sha256_hex: "ab".repeat(32),
            },
            service: "ssh".to_string(),
            link_endpoints: vec!["https://link.example.com".to_string()],
            rendezvous: Vec::new(),
            stun_servers: Vec::new(),
            relay_endpoints: Vec::new(),
            relay_policy: RelayPolicy::RelayAllowed,
            remote_version: None,
            last_bootstrap_at: None,
            last_path_hint: None,
            legacy_relay_transport: None,
        };
        assert_eq!(profile.to_legacy_relay_transport(), None);
    }

    #[test]
    fn migrates_every_entry_in_a_trust_store() {
        let mut store = TrustStore::default();
        store.insert("host-a:22".to_string(), sample_trust());
        store.insert("host-b:22".to_string(), sample_trust());

        let mut profiles = migrate_trust_store(&store);
        profiles.sort_by(|a, b| a.profile.cmp(&b.profile));

        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].profile, "host-a:22");
        assert_eq!(profiles[1].profile, "host-b:22");
    }

    #[test]
    fn round_trips_through_json_serialization() {
        let trust = sample_trust();
        let profile = PersistentProfile::migrate_legacy_helper_trust("myhost:22", &trust);

        let json = serde_json::to_string_pretty(&profile).unwrap();
        let parsed: PersistentProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, profile);
    }

    #[test]
    fn writes_and_loads_a_persistent_profile() {
        let dir = std::env::temp_dir().join(format!(
            "isekai-pipe-profile-test-{}-{}",
            std::process::id(),
            profile_test_nonce()
        ));
        let trust = sample_trust();
        let profile = PersistentProfile::migrate_legacy_helper_trust("myhost:22", &trust);

        write_persistent_profile(&dir, &profile).unwrap();
        let loaded = load_persistent_profile(&dir, "myhost:22").unwrap();
        assert_eq!(loaded, Some(profile));

        assert_eq!(load_persistent_profile(&dir, "no-such-host").unwrap(), None);
        let _ = fs::remove_dir_all(dir);
    }

    fn profile_test_nonce() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
    }
}
