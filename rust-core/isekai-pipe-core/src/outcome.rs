//! Side-channel "how did this `isekai-pipe connect` attempt end" signal for
//! `isekai-ssh`'s wrapper to notice after `ssh` exits
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic N; broadened by the "always-connects"
//! principle, §8 Epic N's "connect-failure auto-recovery" addendum).
//!
//! `isekai-pipe connect` runs as `ssh`'s `ProxyCommand` child, not a direct
//! child of the `isekai-ssh` wrapper process — the two share no pipe. The
//! wrapper wires all of `ssh`'s stdio via `Stdio::inherit()` for interactive
//! passthrough and only learns `ssh`'s exit status once the whole process
//! tree (including this `ProxyCommand` grandchild) has exited, at which
//! point it's free to inspect files in the same `runtime_dir` both
//! processes already share via `ISEKAI_PIPE_RUNTIME_DIR`/`ISEKAI_INTENT_ID`
//! (`write_connection_intent`/`claim_connection_intent`, this crate's
//! `lib.rs`). This module adds a sibling side-channel file, written for
//! *every* `run_connect` failure (not just ones that look like stale trust
//! material) — a remote shell command that ran and exited non-zero never
//! touches this path at all (`run_connect` itself returns `Ok(())` in that
//! case; only `ssh`'s own exit code reflects it).
//!
//! **This file's failure can occur at either of two distinct phases**
//! (Epic R PR2 corrected an earlier version of this doc, which claimed a
//! `run_connect` failure "only ever happens before any SSH byte ever
//! flows" — no longer true once the STUN P2P route's data-pump phase
//! started reporting its own failures through this same side channel):
//! a pre-handshake connect failure (`ConnectOutcomeClass::StaleTrust`/
//! `Unreachable`), or a post-handshake mid-session disconnect
//! (`ConnectOutcomeClass::MidSessionDisconnect` — the SSH bridge was
//! genuinely live for a while before failing). "did `run_connect` fail at
//! all" remains a safe, general trigger for the wrapper to attempt
//! recovery either way — without touching `isekai-pipe connect`'s stdout
//! (whose purity — zero bytes until the QUIC bridge is genuinely live — is
//! a hard, separately-tested invariant elsewhere) — but *which* recovery
//! (a full re-deploy vs. a lightweight reconnect) now depends on which
//! phase failed; see `ConnectOutcomeClass`'s own variant docs.
//!
//! Deliberately keyed by `intent_id` (unique per connect attempt), not by
//! profile name — concurrent `isekai-ssh` invocations against the same host
//! never collide, and a retried attempt (a fresh `ConnectionIntent`) always
//! gets its own outcome file rather than risking a read of a stale leftover
//! from an earlier, unrelated invocation.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{claim_json, write_json_atomically, IntentError};

pub const CONNECT_OUTCOME_SCHEMA_VERSION: u32 = 1;

/// Why `isekai-pipe connect` failed, for the wrapper's own recovery
/// decision and logging. `StaleTrust`/`Unreachable` both trigger the exact
/// same recovery action (`isekai-ssh`'s
/// `ConnectFailureRecoveryAction::RebootstrapAndRetry`, a full re-deploy);
/// the distinction between *those two* is purely for a more accurate
/// message to the user, not a different code
/// path. `StaleTrust` is the narrower, high-confidence case (cert-pin
/// mismatch or an explicit `Auth` reject — `TransportError::is_stale_trust_signal`);
/// `Unreachable` is everything else `run_connect` can fail with (a plain
/// QUIC-connect idle timeout because the cached endpoint is dead, a fencing
/// rejection, a resume failure, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "kebab-case")]
pub enum ConnectOutcomeClass {
    StaleTrust,
    Unreachable,
    /// The connect-time handshake already succeeded — this is a failure
    /// from *after* that point (the data-pump phase), not a connect-time
    /// failure (Epic R PR2). Currently only produced by the STUN P2P
    /// route's `relay_stdio` (`MidSessionDisconnectSignal`'s own docs
    /// explain why the Relay route's resume loop does *not* produce this
    /// class — its own internal resume already covers up to 10 days, so an
    /// error escaping it is terminal and `Unreachable`'s
    /// `RebootstrapAndRetry` response is correct there). Drives
    /// `isekai-ssh`'s lightweight reconnect loop
    /// (`ConnectFailureRecoveryAction::RetryConnectLightweight`) rather
    /// than a full re-deploy.
    MidSessionDisconnect,
    /// Any tag this build doesn't recognize (Epic R PR1). The writer
    /// (`isekai-pipe`, possibly overridden via `--isekai-pipe-path`) and
    /// the reader (`isekai-ssh`) are two independently-invoked local
    /// binaries that can legitimately be different builds — this is *not*
    /// about server-side helper staleness (that's the sha256 check in
    /// `always-connects.md`, an unrelated concern). Without this catch-all,
    /// an internally-tagged enum like this one fails the whole
    /// deserialization on an unknown `class` value, which `claim_connect_outcome`
    /// would otherwise have to turn into a hard error for the entire
    /// `isekai-ssh` invocation — worse than just treating it as "some
    /// outcome was recorded" the way the wrapper already treats `Unreachable`.
    /// Must be matched like `Unreachable` (not like "no signal at all") by
    /// `isekai-ssh::wrapper::decide_connect_failure_recovery` (Epic R PR2
    /// changed this function to take `Option<&ConnectOutcomeClass>` instead
    /// of a plain `bool`, specifically so it *can* branch on the class —
    /// see that function's own docs and its `R1-B1` regression note) and
    /// wherever `ConnectOutcomeClass` is otherwise exhaustively matched —
    /// `isekai-ssh::wrapper::outcome_summary` today. `log_auto_bootstrap_disabled`
    /// also special-cases this variant to avoid suggesting `isekai-ssh
    /// init` — see its own doc comment.
    ///
    /// **Must stay the last variant.** `serde_derive` rejects `#[serde(other)]`
    /// anywhere but the last variant at compile time — a future PR (e.g.
    /// Epic R PR2's `MidSessionDisconnect`) must add its variant *above*
    /// this one, not below it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectOutcome {
    pub schema_version: u32,
    pub intent_id: String,
    pub profile: String,
    #[serde(flatten)]
    pub class: ConnectOutcomeClass,
    pub detail: String,
}

/// Same atomic tmp-file + rename write `write_connection_intent` uses
/// (`write_json_atomically`, `lib.rs`).
pub fn write_connect_outcome(runtime_dir: &Path, outcome: &ConnectOutcome) -> Result<PathBuf, IntentError> {
    write_json_atomically(runtime_dir, "connect-outcomes", &outcome.intent_id, outcome)
}

/// Claims (consumes, by rename) the outcome file for `intent_id`, if any.
/// Unlike `claim_connection_intent`, a missing file is the *normal* case
/// (the attempt either succeeded, or failed for a reason that isn't
/// classified as stale trust) — this returns `Ok(None)`, not
/// `Err(IntentError::Missing)`.
pub fn claim_connect_outcome(runtime_dir: &Path, intent_id: &str) -> Result<Option<ConnectOutcome>, IntentError> {
    claim_json(runtime_dir, "connect-outcomes", "connect-outcomes-claimed", intent_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_outcome() -> ConnectOutcome {
        ConnectOutcome {
            schema_version: CONNECT_OUTCOME_SCHEMA_VERSION,
            intent_id: "abc123".to_string(),
            profile: "production".to_string(),
            class: ConnectOutcomeClass::StaleTrust,
            detail: "cert pin mismatch".to_string(),
        }
    }

    #[test]
    fn unknown_class_round_trips_through_the_full_connect_outcome_struct() {
        // Epic R PR1 (R1-S3): `ConnectOutcome` flattens `ConnectOutcomeClass`
        // via `#[serde(flatten)]`, which deserializes through serde's
        // `FlatMapDeserializer` rather than the plain internally-tagged-enum
        // path — proving `#[serde(other)]` works for `ConnectOutcomeClass`
        // in isolation would not prove it still works once flattened into
        // the parent struct. This exercises the actual `ConnectOutcome`
        // struct end to end, the way `claim_connect_outcome` really reads it.
        let json = r#"{
            "schema_version": 1,
            "intent_id": "abc123",
            "profile": "production",
            "class": "some-future-variant-this-build-does-not-know-about",
            "detail": "whatever a newer isekai-pipe build put here"
        }"#;
        let outcome: ConnectOutcome =
            serde_json::from_str(json).expect("an unrecognized class tag must not fail deserialization of the whole struct");
        assert_eq!(outcome.class, ConnectOutcomeClass::Unknown);
        // Prove `#[serde(flatten)]`'s `FlatMapDeserializer` isn't eating
        // sibling fields while it hunts for the unrecognized `class` tag —
        // asserting `class` alone would miss that class of corruption.
        assert_eq!(outcome.schema_version, 1);
        assert_eq!(outcome.intent_id, "abc123");
        assert_eq!(outcome.profile, "production");
        assert_eq!(outcome.detail, "whatever a newer isekai-pipe build put here");
    }

    #[test]
    fn unknown_class_tag_written_by_a_newer_isekai_pipe_build_is_readable_via_claim_connect_outcome() {
        // Unlike `unreachable_class_round_trips_too` etc., this doesn't go
        // through `write_connect_outcome` (today's `isekai-pipe` never
        // writes `Unknown` itself — `write_connect_outcome_for_wrapper`
        // only ever produces `StaleTrust`/`Unreachable`/(after Epic R PR2)
        // `MidSessionDisconnect`). The actual production scenario `Unknown`
        // exists for is a *newer* `isekai-pipe` build writing a `class` tag
        // *this* `isekai-ssh`/`isekai-pipe-core` build predates — simulated
        // here by placing that raw JSON directly at the on-disk path
        // `claim_connect_outcome` reads from, bypassing this build's own
        // (necessarily backwards-only) `Serialize` impl entirely.
        let dir = tempfile::tempdir().unwrap();
        let outcomes_dir = dir.path().join("connect-outcomes");
        fs::create_dir_all(&outcomes_dir).unwrap();
        fs::write(
            outcomes_dir.join("future-build-intent.json"),
            r#"{
                "schema_version": 1,
                "intent_id": "future-build-intent",
                "profile": "production",
                "class": "helper-crashed-mid-session",
                "detail": "a variant introduced by a newer isekai-pipe build"
            }"#,
        )
        .unwrap();

        let claimed = claim_connect_outcome(dir.path(), "future-build-intent").unwrap();
        assert_eq!(
            claimed,
            Some(ConnectOutcome {
                schema_version: 1,
                intent_id: "future-build-intent".to_string(),
                profile: "production".to_string(),
                class: ConnectOutcomeClass::Unknown,
                detail: "a variant introduced by a newer isekai-pipe build".to_string(),
            })
        );
    }

    #[test]
    fn unreachable_class_round_trips_too() {
        let dir = tempfile::tempdir().unwrap();
        let mut outcome = sample_outcome();
        outcome.intent_id = "def456".to_string();
        outcome.class = ConnectOutcomeClass::Unreachable;
        outcome.detail = "transport lost: idle timeout".to_string();
        write_connect_outcome(dir.path(), &outcome).unwrap();

        let claimed = claim_connect_outcome(dir.path(), &outcome.intent_id).unwrap();
        assert_eq!(claimed, Some(outcome));
    }

    #[test]
    fn mid_session_disconnect_class_round_trips_too() {
        let dir = tempfile::tempdir().unwrap();
        let mut outcome = sample_outcome();
        outcome.intent_id = "ghi789".to_string();
        outcome.class = ConnectOutcomeClass::MidSessionDisconnect;
        outcome.detail = "relay_stdio: writing to remote stream failed".to_string();
        write_connect_outcome(dir.path(), &outcome).unwrap();

        let claimed = claim_connect_outcome(dir.path(), &outcome.intent_id).unwrap();
        assert_eq!(claimed, Some(outcome));
    }

    #[test]
    fn write_then_claim_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = sample_outcome();
        write_connect_outcome(dir.path(), &outcome).unwrap();

        let claimed = claim_connect_outcome(dir.path(), &outcome.intent_id).unwrap();
        assert_eq!(claimed, Some(outcome));
    }

    #[test]
    fn claim_of_a_never_written_intent_id_returns_none_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let claimed = claim_connect_outcome(dir.path(), "never-written").unwrap();
        assert_eq!(claimed, None);
    }

    #[test]
    fn claiming_twice_only_succeeds_once() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = sample_outcome();
        write_connect_outcome(dir.path(), &outcome).unwrap();

        assert!(claim_connect_outcome(dir.path(), &outcome.intent_id).unwrap().is_some());
        assert_eq!(claim_connect_outcome(dir.path(), &outcome.intent_id).unwrap(), None);
    }

    #[test]
    fn write_creates_a_0700_permissioned_directory() {
        let dir = tempfile::tempdir().unwrap();
        write_connect_outcome(dir.path(), &sample_outcome()).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let outcomes_dir = dir.path().join("connect-outcomes");
            let mode = fs::metadata(&outcomes_dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn rejects_an_invalid_intent_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut outcome = sample_outcome();
        outcome.intent_id = "../escape".to_string();
        assert!(write_connect_outcome(dir.path(), &outcome).is_err());
        assert!(claim_connect_outcome(dir.path(), "../escape").is_err());
    }
}
