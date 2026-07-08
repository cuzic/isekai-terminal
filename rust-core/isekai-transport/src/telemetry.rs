//! Structured per-connection-attempt logging (`ISEKAI_PIPE_DESIGN.md`'s
//! "known gaps" section — real-network candidate racing (multi-path,
//! relay/STUN fallback, etc., Phase B onward) cannot be tuned without first
//! being able to observe *why* an individual attempt succeeded or failed.
//! This module only emits structured log lines from the single connection
//! attempt that exists today (`relay::connect_and_handshake`) — it does not
//! yet implement candidate racing itself, that's future work tracked
//! separately.
//!
//! Every attempt is logged with the same field set, whether it succeeds or
//! fails, so log consumers (a human grepping `RUST_LOG=info`, or eventually
//! a real metrics pipeline) never have to special-case which fields are
//! present. Four of those fields identify *which candidate* this attempt was
//! against ([`CandidateIdentity`], derived from `isekai_pipe_core::Candidate`
//! by the caller that actually has one — see that type's docs for why these
//! four are distinct concepts, not one):
//!
//! - `candidate_kind`: the route's class (`"relay"`, `"stun-p2p"`, ...) —
//!   `CandidateRoute::class()`. This is what the field originally named
//!   `candidate_source` meant before `#24`; it was renamed once a *real*
//!   provenance concept ([`CandidateOrigin`]) existed and needed the
//!   `candidate_source` name instead, to avoid two different things sharing
//!   one label.
//! - `candidate_source`: which provider discovered this candidate (e.g.
//!   `"legacy-intent"`) — `CandidateOrigin::source`.
//! - `candidate_provider`: that provider's id — `CandidateOrigin::provider_id`.
//! - `candidate_id`: the pool-local `CandidateId` — lets repeated attempts
//!   against the same candidate be correlated across log lines.
//! - `start_delay`: how long this attempt was queued before it actually
//!   started dialing. Always `0` today (attempts are dialed immediately, one
//!   at a time, with no staggered/parallel racing yet) — recorded now so
//!   log consumers don't need a schema migration once staggered racing
//!   lands.
//! - `quic_handshake_time`: elapsed time from dial start to a successful QUIC
//!   handshake, if it got that far.
//! - `authenticated_ready_time`: elapsed time from dial start to a fully
//!   authenticated (`HELLO`/`ACK`'d) stream ready for pass-through, if it got
//!   that far.
//! - `failure_stage`: which phase the attempt died in, if it failed
//!   (`"quic-connect"`, `"compute-proof"`, `"open-stream"`, `"hello-write"`,
//!   `"ack-read"`, or `"rejected:<reason>"` for an authenticated-but-refused
//!   `ACK`). Absent (`None`) on success.
//! - `outcome`: `Selected` (this attempt's stream is the one actually used)
//!   or `Cancelled` (it failed, or — once multi-candidate racing exists —
//!   lost the race to a faster candidate). Every attempt today either
//!   succeeds (`Selected`) or fails (`Cancelled`); there is no "lost the
//!   race while otherwise healthy" case yet.

use std::time::Duration;

/// What ultimately happened to one connection attempt. `Cancelled` covers
/// both "failed outright" (the only case that exists today) and, once
/// multi-candidate racing exists, "succeeded but lost the race to a faster
/// candidate" — both mean this attempt's stream was never handed to the
/// caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateOutcome {
    Selected,
    Cancelled,
}

impl std::fmt::Display for CandidateOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CandidateOutcome::Selected => "selected",
            CandidateOutcome::Cancelled => "cancelled",
        })
    }
}

/// Which candidate a [`CandidateAttempt`] was against, as four plain string
/// tags rather than a borrowed `isekai_pipe_core::Candidate` — this crate's
/// connection-establishment functions (`relay::connect_and_handshake` and
/// friends) work in terms of `RelayTarget`/`StunP2pTarget`, not `Candidate`
/// directly (`isekai-pipe`'s connection entry point is the one place that
/// actually holds a `Candidate` and converts it back into those transport
/// structs, `ISEKAI_PIPE_DESIGN.md` task #23) — computing these four tags
/// once at that call site and passing them down keeps the telemetry
/// derived from `Candidate` without making every transport function take on
/// a structural dependency it doesn't otherwise need.
#[derive(Debug, Clone, Copy)]
pub struct CandidateIdentity<'a> {
    /// `CandidateRoute::class()` (`"relay"` / `"stun-p2p"`).
    pub kind: &'a str,
    /// `CandidateOrigin::source` (e.g. `"legacy-intent"`).
    pub source: &'a str,
    /// `CandidateOrigin::provider_id`.
    pub provider: &'a str,
    /// `CandidateId`, formatted.
    pub id: &'a str,
}

/// One connection attempt's full timing/outcome record. See the module docs
/// for what each field means and why it's always present (even when `None`)
/// rather than shaped differently per outcome.
#[derive(Debug, Clone, Copy)]
pub struct CandidateAttempt<'a> {
    pub identity: CandidateIdentity<'a>,
    pub start_delay: Duration,
    pub quic_handshake_time: Option<Duration>,
    pub authenticated_ready_time: Option<Duration>,
    pub failure_stage: Option<&'a str>,
    pub outcome: CandidateOutcome,
}

/// Emits one structured log line per attempt: `info` on success (`Selected`),
/// `warn` on failure (`Cancelled`) — matching this crate's existing
/// `log::info!`/`log::warn!` conventions elsewhere (`relay.rs`, `resume.rs`).
/// Field values are logged as `key=value` pairs (durations in milliseconds,
/// `None` printed literally as `none`) so a plain `grep`/`RUST_LOG=info`
/// session can already answer "how long did the QUIC handshake take on the
/// last 5 attempts against candidate X" without needing structured-logging
/// tooling.
pub fn log_candidate_attempt(attempt: &CandidateAttempt<'_>) {
    let quic_handshake_ms = format_duration_ms(attempt.quic_handshake_time);
    let authenticated_ready_ms = format_duration_ms(attempt.authenticated_ready_time);
    let failure_stage = attempt.failure_stage.unwrap_or("none");

    let line = format!(
        "candidate attempt: candidate_kind={} candidate_source={} candidate_provider={} candidate_id={} \
         start_delay_ms={} quic_handshake_time_ms={quic_handshake_ms} \
         authenticated_ready_time_ms={authenticated_ready_ms} failure_stage={failure_stage} outcome={}",
        attempt.identity.kind,
        attempt.identity.source,
        attempt.identity.provider,
        attempt.identity.id,
        attempt.start_delay.as_millis(),
        attempt.outcome,
    );
    match attempt.outcome {
        CandidateOutcome::Selected => log::info!("isekai-transport: {line}"),
        CandidateOutcome::Cancelled => log::warn!("isekai-transport: {line}"),
    }
}

fn format_duration_ms(d: Option<Duration>) -> String {
    match d {
        Some(d) => d.as_millis().to_string(),
        None => "none".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_outcome_displays_as_lowercase_words() {
        assert_eq!(CandidateOutcome::Selected.to_string(), "selected");
        assert_eq!(CandidateOutcome::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn format_duration_ms_renders_none_literally() {
        assert_eq!(format_duration_ms(None), "none");
        assert_eq!(format_duration_ms(Some(Duration::from_millis(42))), "42");
    }

    #[test]
    fn log_candidate_attempt_does_not_panic_on_either_outcome() {
        // Smoke test only — this function's job is to log, not to return a
        // value; the point is just that constructing/formatting every field
        // combination (all-`None` durations included) never panics.
        log_candidate_attempt(&CandidateAttempt {
            identity: CandidateIdentity { kind: "relay", source: "legacy-intent", provider: "legacy-intent", id: "0" },
            start_delay: Duration::ZERO,
            quic_handshake_time: None,
            authenticated_ready_time: None,
            failure_stage: Some("quic-connect"),
            outcome: CandidateOutcome::Cancelled,
        });
        log_candidate_attempt(&CandidateAttempt {
            identity: CandidateIdentity { kind: "stun-p2p", source: "legacy-intent", provider: "legacy-intent", id: "0" },
            start_delay: Duration::ZERO,
            quic_handshake_time: Some(Duration::from_millis(30)),
            authenticated_ready_time: Some(Duration::from_millis(45)),
            failure_stage: None,
            outcome: CandidateOutcome::Selected,
        });
    }
}
