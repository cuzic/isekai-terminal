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
use isekai_trust::SshHostKeyTrust;
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

/// What one locked look-at-the-store pass resolved to.
enum Resolved {
    /// Final answer, no user interaction needed (already trusted+matching,
    /// or a mismatch — both are decided without ever consulting
    /// `confirm_new_host`).
    Decided(bool),
    /// Never seen before; caller must consult `confirm_new_host` and, if
    /// confirmed, call [`FileBackedHostKeyVerifier::resolve_locked`] again
    /// with `insert_if_unknown: true`.
    NeedsConfirmation,
    /// Lock/IO error, or the blocking task itself panicked — callers treat
    /// this as a hard reject (fail closed).
    Failed,
}

#[async_trait]
impl HostKeyVerifier for FileBackedHostKeyVerifier {
    async fn verify(&self, fingerprint: &str) -> bool {
        // Phase 1 (locked, no user interaction): resolves "already trusted"
        // and "mismatch" outright, without ever calling `confirm_new_host`
        // while holding the store lock.
        match self.resolve_locked(fingerprint, false).await {
            Resolved::Decided(accepted) => return accepted,
            Resolved::NeedsConfirmation => {}
            Resolved::Failed => return false,
        }

        // Outside any lock: ask the (possibly slow/interactive)
        // confirmation callback. Must never happen while holding the store
        // lock (Codex review finding) — the lock key is the whole
        // `known_ssh_hosts` store, not scoped per host, so a human staring
        // at one new-host prompt would otherwise block every *other* tab's
        // host-key checks too, even for completely unrelated, already-known
        // hosts.
        let confirm_new_host = self.confirm_new_host.clone();
        let fingerprint_owned = fingerprint.to_string();
        let confirmed = match tokio::task::spawn_blocking(move || confirm_new_host(&fingerprint_owned)).await {
            Ok(confirmed) => confirmed,
            Err(join_error) => {
                log::error!("isekai-ssh: SSH host key confirmation task panicked, rejecting connection: {join_error}");
                return false;
            }
        };
        if !confirmed {
            return false;
        }

        // Phase 2 (locked again): re-resolve from scratch rather than
        // blindly inserting — another tab connecting to the same
        // brand-new host may have raced us while we were waiting on the
        // user, in which case this reuses the exact same decision logic
        // (accept-and-refresh if it now matches, reject if it now doesn't)
        // instead of overwriting whatever that other tab just decided.
        // `insert_if_unknown: true` means if it's *still* genuinely
        // unknown, this call is the one that persists our now-confirmed
        // trust.
        match self.resolve_locked(fingerprint, true).await {
            Resolved::Decided(accepted) => accepted,
            Resolved::NeedsConfirmation => unreachable!("insert_if_unknown: true never returns NeedsConfirmation"),
            Resolved::Failed => false,
        }
    }
}

impl FileBackedHostKeyVerifier {
    /// Runs one locked look-at-the-store pass. With `insert_if_unknown:
    /// false`, an unknown host resolves to [`Resolved::NeedsConfirmation`]
    /// without modifying the store; with `true`, an unknown host is trusted
    /// and persisted on the spot (used for the post-confirmation re-check,
    /// where "still unknown" means this call is the one that should decide).
    async fn resolve_locked(&self, fingerprint: &str, insert_if_unknown: bool) -> Resolved {
        let store_path = self.store_path.clone();
        let host_port = self.host_port.clone();
        let fingerprint = fingerprint.to_string();

        // `with_locked_ssh_host_key_trust_store` does blocking file I/O and
        // can block for an arbitrary time on the cross-process lock — run
        // it on a blocking-pool thread so it never stalls this tokio
        // runtime's async workers.
        let outcome = tokio::task::spawn_blocking(move || {
            isekai_trust::with_locked_ssh_host_key_trust_store(&store_path, |store| {
                match store.get(&host_port) {
                    Some(known) if known.fingerprint == fingerprint => {
                        let mut updated = known.clone();
                        updated.last_seen_at = now_rfc3339();
                        store.insert(host_port.clone(), updated);
                        Ok(Resolved::Decided(true))
                    }
                    Some(known) => {
                        log::error!(
                            "isekai-ssh: host key for {host_port} changed (trusted {}, saw {fingerprint}) \
                             — refusing to connect. If this change is expected (e.g. you redeployed), \
                             remove the \"{host_port}\" entry from {} and reconnect.",
                            known.fingerprint,
                            store_path.display(),
                        );
                        Ok(Resolved::Decided(false))
                    }
                    None if insert_if_unknown => {
                        let now = now_rfc3339();
                        store.insert(
                            host_port.clone(),
                            SshHostKeyTrust { fingerprint: fingerprint.clone(), trusted_at: now.clone(), last_seen_at: now },
                        );
                        Ok(Resolved::Decided(true))
                    }
                    None => Ok(Resolved::NeedsConfirmation),
                }
            })
        })
        .await;

        match outcome {
            Ok(Ok(resolved)) => resolved,
            Ok(Err(e)) => {
                log::warn!("isekai-ssh: SSH host key trust store operation failed, rejecting connection: {e}");
                Resolved::Failed
            }
            Err(join_error) => {
                log::error!("isekai-ssh: SSH host key trust check task panicked, rejecting connection: {join_error}");
                Resolved::Failed
            }
        }
    }
}

fn now_rfc3339() -> String {
    // A third copy of the same tiny RFC3339 formatter `wrapper.rs`/`init.rs`
    // already each carry their own copy of — checked against a Codex review
    // "reuse" finding suggesting this should be factored into a shared
    // helper, but `wrapper.rs:895-897`'s own doc comment already states the
    // project's deliberate call on this exact question: "duplicated rather
    // than shared across two ~60-line modules for a single timestamp
    // helper." A third copy for a third ~15-line function is consistent
    // with that established precedent, not a new problem.
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
    use isekai_trust::SshHostKeyTrustStore;

    fn verifier_with_answer(store_path: PathBuf, host_port: &str, answer: bool) -> FileBackedHostKeyVerifier {
        FileBackedHostKeyVerifier::new(store_path, host_port.to_string(), Arc::new(move |_fp| answer))
    }

    /// The exact regression the Codex review caught: a slow/interactive
    /// confirmation prompt for one brand-new host must not block `verify`
    /// for a *different*, already-known host sharing the same store —
    /// under the old single-locked-critical-section design, the lock (held
    /// across the whole store, not scoped per host) stayed held for the
    /// full duration of `confirm_new_host`, so host B's check would have
    /// waited on host A's still-pending human confirmation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_pending_confirmation_for_one_host_does_not_block_a_different_known_host() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("known_ssh_hosts.toml");

        // Host B is already trusted — verifying it should never need to
        // touch `confirm_new_host` at all.
        let mut seed = SshHostKeyTrustStore::default();
        seed.insert(
            "known.example.com:22".to_string(),
            isekai_trust::SshHostKeyTrust {
                fingerprint: "SHA256:known".to_string(),
                trusted_at: "2026-01-01T00:00:00Z".to_string(),
                last_seen_at: "2026-01-01T00:00:00Z".to_string(),
            },
        );
        isekai_trust::save_ssh_host_key_trust_store(&store_path, &seed).unwrap();

        // Host A's confirmation blocks until the test explicitly releases
        // it, simulating a human who hasn't answered the prompt yet.
        let (release_tx, release_rx) = std::sync::mpsc::channel::<bool>();
        let release_rx = std::sync::Mutex::new(Some(release_rx));
        let verifier_a = FileBackedHostKeyVerifier::new(
            store_path.clone(),
            "new.example.com:22".to_string(),
            Arc::new(move |_fp| release_rx.lock().unwrap().take().unwrap().recv().unwrap()),
        );
        let verify_a = tokio::spawn(async move { verifier_a.verify("SHA256:new").await });

        // Give host A's task a moment to actually reach (and block inside)
        // its confirmation call.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let verifier_b = verifier_with_answer(store_path.clone(), "known.example.com:22", true);
        let verify_b = tokio::time::timeout(std::time::Duration::from_secs(2), verifier_b.verify("SHA256:known")).await;
        assert_eq!(
            verify_b.expect("verifying the already-known host must not wait on host A's pending confirmation"),
            true
        );

        release_tx.send(true).unwrap();
        assert!(verify_a.await.unwrap(), "host A should end up trusted once its confirmation is released");
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
