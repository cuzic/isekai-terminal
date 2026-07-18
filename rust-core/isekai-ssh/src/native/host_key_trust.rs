//! Implements `russh_stream_session::HostKeyVerifier` (M0) backed by
//! `isekai_trust::SshHostKeyTrustStore` (M1) — the native path's TOFU host
//! key check, standing in for `ssh(1)`'s own `known_hosts` prompt.
//!
//! TOFU semantics deliberately mirror `ssh(1)`, not a simpler
//! "always trust" shortcut:
//! - **Known, matching fingerprint**: silently accepted, `last_seen_at`
//!   refreshed.
//! - **Known, mismatched fingerprint**: silently *rejected* — no prompt.
//!   A changed host key is a stronger signal than a new one (could mean
//!   MITM, or a legitimate re-key/redeploy), and `always-connects.md`'s
//!   sole exemption is for "genuinely new host" confirmation, not this
//!   case. A user who intentionally re-deployed still has the exact same
//!   recovery path `ssh(1)`'s own `~/.ssh/known_hosts` mismatch has always
//!   forced: remove the stale entry and reconnect. Automating that removal
//!   would defeat the point of pinning.
//! - **Unknown host**: `confirm_new_host` decides (production: prompt on
//!   the real terminal; tests: inject a fixed answer) — `always-connects.md`
//!   explicitly exempts first-time TOFU confirmation from the "must recover
//!   automatically" rule.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use isekai_trust::{SshHostKeyTrust, SshHostKeyTrustStore};
use russh_stream_session::HostKeyVerifier;

pub(crate) struct FileBackedHostKeyVerifier {
    store_path: PathBuf,
    host_port: String,
    /// Called only for a host never seen before. Kept generic (not a
    /// hardcoded real-stdin prompt) so tests can inject a fixed answer
    /// without touching a real terminal; production wires this to a
    /// blocking stdin prompt (later commit — the interactive terminal I/O
    /// loop this eventually plugs into hasn't taken over the console yet
    /// at the point this runs, so a plain blocking read is safe here).
    confirm_new_host: Arc<dyn Fn(&str) -> bool + Send + Sync>,
}

impl FileBackedHostKeyVerifier {
    pub(crate) fn new(
        store_path: PathBuf,
        host_port: String,
        confirm_new_host: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    ) -> Self {
        Self { store_path, host_port, confirm_new_host }
    }
}

#[async_trait]
impl HostKeyVerifier for FileBackedHostKeyVerifier {
    async fn verify(&self, fingerprint: &str) -> bool {
        let mut store = match isekai_trust::load_ssh_host_key_trust_store(&self.store_path) {
            Ok(store) => store,
            Err(e) => {
                log::warn!("isekai-ssh: failed to load SSH host key trust store, rejecting connection: {e}");
                return false;
            }
        };

        let now = now_rfc3339();
        match store.get(&self.host_port) {
            Some(known) if known.fingerprint == fingerprint => {
                let mut updated = known.clone();
                updated.last_seen_at = now;
                store.insert(self.host_port.clone(), updated);
                save_or_reject(&self.store_path, &store)
            }
            Some(known) => {
                log::error!(
                    "isekai-ssh: host key for {} changed (trusted {}, saw {}) — refusing to connect. \
                     Remove the stale entry from the SSH host key trust store if this change is expected.",
                    self.host_port, known.fingerprint, fingerprint,
                );
                false
            }
            None => {
                if !(self.confirm_new_host)(fingerprint) {
                    return false;
                }
                store.insert(
                    self.host_port.clone(),
                    SshHostKeyTrust {
                        fingerprint: fingerprint.to_string(),
                        trusted_at: now.clone(),
                        last_seen_at: now,
                    },
                );
                save_or_reject(&self.store_path, &store)
            }
        }
    }
}

/// Saving is expected to succeed (the store loaded fine, so its directory
/// exists and is writable) — but a failure here (disk full, permissions
/// changed mid-run) must still fail the connection rather than silently
/// proceed with an unpersisted trust decision that a concurrent/later
/// connection wouldn't see.
fn save_or_reject(store_path: &std::path::Path, store: &SshHostKeyTrustStore) -> bool {
    match isekai_trust::save_ssh_host_key_trust_store(store_path, store) {
        Ok(()) => true,
        Err(e) => {
            log::warn!("isekai-ssh: failed to persist SSH host key trust store, rejecting connection: {e}");
            false
        }
    }
}

fn now_rfc3339() -> String {
    // Matches `wrapper.rs::now_rfc3339`'s own precision/format choice for
    // the sibling `HelperTrust` store (seconds, `Z` suffix) — kept as an
    // independent copy rather than a shared helper since the two crates
    // (`isekai-ssh` vs `isekai-trust`) don't otherwise share a time-
    // formatting dependency, and this is a two-line function.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = secs / 86_400;
    let time_of_day = secs % 86_400;
    let (h, m, s) = (time_of_day / 3600, (time_of_day % 3600) / 60, time_of_day % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Howard Hinnant's `civil_from_days` algorithm (public domain,
/// <http://howardhinnant.github.io/date_algorithms.html>) — converts a
/// day count since the Unix epoch into a proleptic-Gregorian
/// (year, month, day). No `chrono`/`time` dependency needed for a value
/// this codebase only ever displays, never parses back arithmetically.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verifier_with_answer(store_path: PathBuf, host_port: &str, answer: bool) -> FileBackedHostKeyVerifier {
        FileBackedHostKeyVerifier::new(store_path, host_port.to_string(), Arc::new(move |_fp| answer))
    }

    #[tokio::test]
    async fn unknown_host_is_trusted_when_confirmed_and_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("known_ssh_hosts.toml");
        let verifier = verifier_with_answer(store_path.clone(), "example.com:22", true);

        assert!(verifier.verify("SHA256:abc").await);

        let store = isekai_trust::load_ssh_host_key_trust_store(&store_path).unwrap();
        assert_eq!(store.get("example.com:22").unwrap().fingerprint, "SHA256:abc");
    }

    #[tokio::test]
    async fn unknown_host_is_rejected_and_not_persisted_when_declined() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("known_ssh_hosts.toml");
        let verifier = verifier_with_answer(store_path.clone(), "example.com:22", false);

        assert!(!verifier.verify("SHA256:abc").await);

        let store = isekai_trust::load_ssh_host_key_trust_store(&store_path).unwrap();
        assert_eq!(store.get("example.com:22"), None, "declining must not persist a trust entry");
    }

    #[tokio::test]
    async fn known_matching_fingerprint_is_accepted_without_prompting() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("known_ssh_hosts.toml");
        let mut store = SshHostKeyTrustStore::default();
        store.insert(
            "example.com:22".to_string(),
            SshHostKeyTrust {
                fingerprint: "SHA256:abc".to_string(),
                trusted_at: "2026-01-01T00:00:00Z".to_string(),
                last_seen_at: "2026-01-01T00:00:00Z".to_string(),
            },
        );
        isekai_trust::save_ssh_host_key_trust_store(&store_path, &store).unwrap();

        // `confirm_new_host` would panic if called — proves the known-match
        // path never prompts.
        let verifier = FileBackedHostKeyVerifier::new(
            store_path.clone(),
            "example.com:22".to_string(),
            Arc::new(|_| panic!("must not prompt for an already-known, matching host key")),
        );
        assert!(verifier.verify("SHA256:abc").await);

        let updated = isekai_trust::load_ssh_host_key_trust_store(&store_path).unwrap();
        let entry = updated.get("example.com:22").unwrap();
        assert_eq!(entry.trusted_at, "2026-01-01T00:00:00Z", "trusted_at must not change on a re-seen match");
        assert_ne!(entry.last_seen_at, "2026-01-01T00:00:00Z", "last_seen_at must be refreshed");
    }

    #[tokio::test]
    async fn known_mismatched_fingerprint_is_rejected_without_prompting() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("known_ssh_hosts.toml");
        let mut store = SshHostKeyTrustStore::default();
        store.insert(
            "example.com:22".to_string(),
            SshHostKeyTrust {
                fingerprint: "SHA256:original".to_string(),
                trusted_at: "2026-01-01T00:00:00Z".to_string(),
                last_seen_at: "2026-01-01T00:00:00Z".to_string(),
            },
        );
        isekai_trust::save_ssh_host_key_trust_store(&store_path, &store).unwrap();

        let verifier = FileBackedHostKeyVerifier::new(
            store_path.clone(),
            "example.com:22".to_string(),
            Arc::new(|_| panic!("a changed host key must be a hard reject, never a prompt")),
        );
        assert!(!verifier.verify("SHA256:different").await);

        let unchanged = isekai_trust::load_ssh_host_key_trust_store(&store_path).unwrap();
        assert_eq!(
            unchanged.get("example.com:22").unwrap().fingerprint,
            "SHA256:original",
            "a rejected mismatch must not overwrite the previously-trusted fingerprint"
        );
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        // 1970-01-01 is day 0 by definition.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-07-17 (this session's current date), cross-checked against
        // Python's `(date(2026,7,17) - date(1970,1,1)).days` = 20651.
        assert_eq!(civil_from_days(20_651), (2026, 7, 17));
    }
}
