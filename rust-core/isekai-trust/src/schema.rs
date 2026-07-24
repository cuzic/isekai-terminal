//! Value types for `~/.config/isekai-ssh/known_helpers.toml`
//! (`archive/ISEKAI_SSH_DESIGN.md` "trust store のファイル形式").

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How `isekai-ssh connect` is allowed to accept a re-deployed
/// `isekai-helper` binary without re-running `init`.
///
/// Only `ExactDigestOnly` exists today because release signing is not
/// implemented yet (`archive/ISEKAI_SSH_DESIGN.md` "引き続き未決の項目"). This is a
/// closed enum on purpose: `serde`'s derived `Deserialize` rejects any
/// string that isn't a known variant, which is what makes loading a store
/// with a future/unknown `update_policy` (e.g. a `"signed-compatible"`
/// value from a newer isekai-ssh) fail closed instead of silently
/// defaulting to a permissive policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdatePolicy {
    ExactDigestOnly,
}

/// One `[helpers."host:port"]` entry.
///
/// `last_via` is purely informational (the jumphost last used to reach this
/// host) and, unlike the trust store key itself, is not part of the
/// helper's identity — see `normalize::normalize_host_port`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperTrust {
    pub identity_pubkey: String,
    pub trusted_helper_sha256: String,
    pub trusted_helper_version: String,
    pub update_policy: UpdatePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_via: Option<String>,
    pub trusted_at: String,
    pub last_seen_at: String,

    /// The relay-assigned public address of the remote isekai-helper
    /// instance (`"ip:port"`), captured the last time `init` (or a
    /// re-deployment) completed a successful HELLO/proof/ACK handshake.
    /// `isekai-ssh connect` (S-2) uses this together with
    /// `cached_cert_sha256`/`cached_session_secret` below to build an
    /// `isekai_transport::RelayTarget` directly, without going through
    /// `--via` on the common path (`archive/ISEKAI_SSH_DESIGN.md` "trust store の
    /// ファイル形式").
    pub cached_relay_addr: String,
    /// `HandshakeJson::cert_sha256` from that same handshake.
    pub cached_cert_sha256: String,
    /// `HandshakeJson::session_secret` from that same handshake, still
    /// base64-encoded (as isekai-helper emits it) — callers decode it
    /// themselves, mirroring how `isekai_transport::RelayTarget::session_secret`
    /// is populated from `--dev-insecure-session-secret` today. If
    /// isekai-helper has since restarted, this cached secret no longer
    /// matches its current session and the HELLO/proof exchange will be
    /// rejected — see `isekai-ssh::connect`'s handling of that case.
    pub cached_session_secret: String,
    /// `HandshakeJson::stun_observed_addr()` (`"ip:port"`) from that same
    /// handshake, if the remote `isekai-helper` was launched with
    /// `--stun-server` (`#20b`) and it answered. `None` when no STUN
    /// exchange happened (the common case before `#20b`, or when every
    /// configured STUN server failed to respond,
    /// `isekai-bootstrap::openssh::collect_client_stun_candidates`) — this is
    /// purely a cache of what the *server* observed about itself; deciding
    /// whether/how to actually attempt a direct STUN-P2P connection using it
    /// is deferred to a future candidate-selection pass (`#13b`), not this
    /// field's own concern. `#[serde(default)]` keeps existing
    /// `known_helpers.toml` files (written before this field existed) loading
    /// unchanged, with this simply absent/`None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_stun_observed_addr: Option<String>,
}

/// The whole `known_helpers.toml` document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustStore {
    #[serde(default)]
    pub helpers: BTreeMap<String, HelperTrust>,
    /// Reserved for release signing keys once `update_policy` gains a
    /// `signed-compatible`-style variant; unused (and always empty on
    /// disk) while `UpdatePolicy` only has `ExactDigestOnly`.
    #[serde(default)]
    pub release_keys: BTreeMap<String, String>,
}

impl TrustStore {
    pub fn get(&self, host_port: &str) -> Option<&HelperTrust> {
        self.helpers.get(host_port)
    }

    pub fn insert(&mut self, host_port: String, trust: HelperTrust) {
        self.helpers.insert(host_port, trust);
    }

    pub fn remove(&mut self, host_port: &str) -> Option<HelperTrust> {
        self.helpers.remove(host_port)
    }
}

/// One trusted SSH host key (TOFU), keyed by `host:port`
/// (`normalize::normalize_host_port`). Deliberately a separate store/file
/// from [`TrustStore`]/`HelperTrust`: this is the *SSH protocol* host key
/// (what a native, non-`ssh(1)` client verifies during the SSH handshake,
/// `ssh_config(5)`'s `known_hosts` equivalent), not the isekai-helper
/// QUIC/relay endpoint identity `HelperTrust` tracks — a corrupted or
/// tampered-with file for one must not threaten the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshHostKeyTrust {
    /// SHA-256 fingerprint, matching the format
    /// `russh_stream_session::HostKeyVerifier::verify`'s `fingerprint`
    /// argument (`PublicKey::fingerprint(HashAlg::Sha256).to_string()`).
    pub fingerprint: String,
    pub trusted_at: String,
    pub last_seen_at: String,
}

/// The whole `known_ssh_hosts.toml` document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshHostKeyTrustStore {
    #[serde(default)]
    pub hosts: BTreeMap<String, SshHostKeyTrust>,
}

impl SshHostKeyTrustStore {
    pub fn get(&self, host_port: &str) -> Option<&SshHostKeyTrust> {
        self.hosts.get(host_port)
    }

    pub fn insert(&mut self, host_port: String, trust: SshHostKeyTrust) {
        self.hosts.insert(host_port, trust);
    }

    pub fn remove(&mut self, host_port: &str) -> Option<SshHostKeyTrust> {
        self.hosts.remove(host_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            cached_stun_observed_addr: Some("198.51.100.7:45231".to_string()),
        }
    }

    #[test]
    fn serializes_and_parses_back_via_toml() {
        let mut store = TrustStore::default();
        store.insert("myhost:22".to_string(), sample_entry());
        store.release_keys.insert("stable".to_string(), "release-key-material".to_string());

        let serialized = toml::to_string_pretty(&store).unwrap();
        let parsed: TrustStore = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, store);
    }

    #[test]
    fn parses_the_documented_schema_example() {
        let toml_str = r#"
[helpers."myhost:22"]
identity_pubkey = "pk"
trusted_helper_sha256 = "aaa"
trusted_helper_version = "0.3.1"
update_policy = "exact-digest-only"
release_channel = "stable"
last_via = "bastion.example.com"
trusted_at = "2026-07-04T00:00:00Z"
last_seen_at = "2026-07-04T00:00:00Z"
cached_relay_addr = "203.0.113.10:45231"
cached_cert_sha256 = "3a7f..."
cached_session_secret = "MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE="

[release_keys]
stable = "release-key-material"
"#;
        let store: TrustStore = toml::from_str(toml_str).unwrap();
        let entry = store.get("myhost:22").unwrap();
        assert_eq!(entry.update_policy, UpdatePolicy::ExactDigestOnly);
        assert_eq!(entry.cached_relay_addr, "203.0.113.10:45231");
        assert_eq!(entry.cached_cert_sha256, "3a7f...");
        assert_eq!(entry.cached_session_secret, "MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=");
        // `#20b`: a `known_helpers.toml` written before this field existed
        // (no `cached_stun_observed_addr` key at all) must still load, with
        // this simply defaulting to `None`.
        assert_eq!(entry.cached_stun_observed_addr, None);
        assert_eq!(store.release_keys.get("stable").unwrap(), "release-key-material");
    }

    #[test]
    fn rejects_unknown_update_policy_value() {
        let toml_str = r#"
[helpers."myhost:22"]
identity_pubkey = "pk"
trusted_helper_sha256 = "aaa"
trusted_helper_version = "0.3.1"
update_policy = "signed-compatible"
trusted_at = "2026-07-04T00:00:00Z"
last_seen_at = "2026-07-04T00:00:00Z"
"#;
        let err = toml::from_str::<TrustStore>(toml_str).unwrap_err();
        // Fail closed: an unrecognized update_policy is a parse error, not a
        // value that silently deserializes to some default variant.
        assert!(err.to_string().contains("signed-compatible") || err.to_string().contains("unknown variant"));
    }

    #[test]
    fn ssh_host_key_trust_store_serializes_and_parses_back_via_toml() {
        let mut store = SshHostKeyTrustStore::default();
        store.insert(
            "example.com:22".to_string(),
            SshHostKeyTrust {
                fingerprint: "SHA256:abcdef1234567890".to_string(),
                trusted_at: "2026-07-17T00:00:00Z".to_string(),
                last_seen_at: "2026-07-17T00:00:00Z".to_string(),
            },
        );

        let serialized = toml::to_string_pretty(&store).unwrap();
        let parsed: SshHostKeyTrustStore = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, store);
    }

    #[test]
    fn ssh_host_key_trust_store_get_insert_remove() {
        let mut store = SshHostKeyTrustStore::default();
        assert_eq!(store.get("example.com:22"), None);

        store.insert(
            "example.com:22".to_string(),
            SshHostKeyTrust {
                fingerprint: "SHA256:abc".to_string(),
                trusted_at: "2026-07-17T00:00:00Z".to_string(),
                last_seen_at: "2026-07-17T00:00:00Z".to_string(),
            },
        );
        assert_eq!(store.get("example.com:22").unwrap().fingerprint, "SHA256:abc");

        let removed = store.remove("example.com:22").unwrap();
        assert_eq!(removed.fingerprint, "SHA256:abc");
        assert_eq!(store.get("example.com:22"), None);
    }
}
