//! Side-channel "how did this `isekai-pipe connect` attempt end" signal for
//! `isekai-ssh`'s wrapper to notice after `ssh` exits
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic N).
//!
//! `isekai-pipe connect` runs as `ssh`'s `ProxyCommand` child, not a direct
//! child of the `isekai-ssh` wrapper process — the two share no pipe. The
//! wrapper wires all of `ssh`'s stdio via `Stdio::inherit()` for interactive
//! passthrough and only learns `ssh`'s exit status once the whole process
//! tree (including this `ProxyCommand` grandchild) has exited, at which
//! point it's free to inspect files in the same `runtime_dir` both
//! processes already share via `ISEKAI_PIPE_RUNTIME_DIR`/`ISEKAI_INTENT_ID`
//! (`write_connection_intent`/`claim_connection_intent`, this crate's
//! `lib.rs`). This module adds a sibling side-channel file for exactly one
//! purpose: telling the wrapper "the cached trust for this profile looks
//! stale, a re-bootstrap is worth trying" without touching `isekai-pipe
//! connect`'s stdout (whose purity — zero bytes until the QUIC bridge is
//! genuinely live — is a hard, separately-tested invariant elsewhere).
//!
//! Deliberately keyed by `intent_id` (unique per connect attempt), not by
//! profile name — concurrent `isekai-ssh` invocations against the same host
//! never collide, and a retried attempt (a fresh `ConnectionIntent`) always
//! gets its own outcome file rather than risking a read of a stale leftover
//! from an earlier, unrelated invocation.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{create_private_dir, validate_intent_id, IntentError};

pub const CONNECT_OUTCOME_SCHEMA_VERSION: u32 = 1;

/// The only classification that exists today. A separate enum (rather than
/// a bare bool) leaves room to add other connect-outcome signals later
/// without a breaking schema change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "kebab-case")]
pub enum ConnectOutcomeClass {
    StaleTrust,
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

/// Same atomic tmp-file + rename write `write_connection_intent` uses.
pub fn write_connect_outcome(runtime_dir: &Path, outcome: &ConnectOutcome) -> Result<PathBuf, IntentError> {
    validate_intent_id(&outcome.intent_id)?;
    let outcomes = runtime_dir.join("connect-outcomes");
    create_private_dir(runtime_dir)?;
    create_private_dir(&outcomes)?;
    let path = outcomes.join(format!("{}.json", outcome.intent_id));
    let tmp = outcomes.join(format!("{}.{}.tmp", outcome.intent_id, std::process::id()));
    let bytes = serde_json::to_vec_pretty(outcome)?;
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Claims (consumes, by rename) the outcome file for `intent_id`, if any.
/// Unlike `claim_connection_intent`, a missing file is the *normal* case
/// (the attempt either succeeded, or failed for a reason that isn't
/// classified as stale trust) — this returns `Ok(None)`, not
/// `Err(IntentError::Missing)`.
pub fn claim_connect_outcome(runtime_dir: &Path, intent_id: &str) -> Result<Option<ConnectOutcome>, IntentError> {
    validate_intent_id(intent_id)?;
    let src = runtime_dir.join("connect-outcomes").join(format!("{intent_id}.json"));
    let claimed_dir = runtime_dir.join("connect-outcomes-claimed");
    create_private_dir(runtime_dir)?;
    create_private_dir(&claimed_dir)?;
    let dst = claimed_dir.join(format!("{intent_id}.{}.json", std::process::id()));
    match fs::rename(&src, &dst) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(IntentError::Io(e)),
    }
    let bytes = fs::read(&dst)?;
    let outcome: ConnectOutcome = serde_json::from_slice(&bytes)?;
    Ok(Some(outcome))
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
