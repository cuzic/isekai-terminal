//! Owns the `(session_id, generation)` pair for one connection attempt
//! across an entire round of candidates (`#25`, ChatGPT second-opinion
//! consultation 2026-07-08, 3rd round). Deliberately knows nothing about
//! candidates, ordering, or concurrency — those are the round runner's job
//! (`resume::connect_via_relay_resumable_with_fallback` for the sequential
//! case, a future concurrent race runner for `#19`). Keeping this type this
//! narrow is what lets both runners share it: a `GenerationCoordinator`
//! carrying candidate/scheduling concepts couldn't serve a race runner
//! without those concepts becoming a lie for the sequential case (and vice
//! versa).
//!
//! Safety property this exists to encode, on top of the server-side
//! `AttachArbiter`'s own fencing (`#18`): [`GenerationCoordinator`] is the
//! *only* thing allowed to advance `generation`, and only ever in reaction
//! to [`crate::AttemptFailure::AmbiguousAfterAttach`] — never for a
//! `RetryablePreAttach` failure, which doesn't need a new generation to
//! retry safely. [`GenerationCoordinator::is_current`] is a second,
//! client-local line of defense against a stale generation's attempt
//! completing late and being mistaken for the current round's winner — the
//! server-side `AttachArbiter` already refuses to let a stale generation's
//! *attach* succeed, but a client that raced a real target connection
//! against a newer round could still receive that stale attempt's result
//! after the fact and must not treat it as authoritative.

use std::fmt;

use isekai_protocol::attach::ConnectionGeneration;
use isekai_protocol::session_id::SessionId;

/// How many times `advance_generation` may be called before a round gives
/// up. Ambiguity is a signal the ATTACH control path itself is unstable —
/// not an ordinary per-candidate failure — so this is deliberately small and
/// independent of the candidate count (`ChatGPT` 2026-07-08 3rd round:
/// "candidate数に比例して増やすべきではない"). Subject to tuning once `#13a`'s
/// telemetry gives real data.
pub const DEFAULT_MAX_GENERATION_ADVANCES: u8 = 2;

/// Identifies one round: the fixed `session_id` for this whole connection
/// attempt, plus the generation every candidate in *this* round must use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundContext {
    pub session_id: SessionId,
    pub generation: ConnectionGeneration,
}

/// Why [`GenerationCoordinator::advance_generation`] refused to advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvanceGenerationError {
    /// `advance_generation` has already been called
    /// `max_generation_advances` times for this coordinator — the round
    /// runner must give up and surface a terminal error rather than keep
    /// advancing forever.
    RetryBudgetExceeded { advances: u8, max: u8 },
    /// The next generation would overflow `u64` — cannot happen in
    /// practice (would require `u64::MAX` prior advances), guarded anyway
    /// per this codebase's convention of never silently wrapping a value a
    /// security decision depends on (mirrors `offset::checked_advance`).
    GenerationOverflow,
}

impl fmt::Display for AdvanceGenerationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RetryBudgetExceeded { advances, max } => {
                write!(f, "generation advance budget exceeded ({advances}/{max} advances already used)")
            }
            Self::GenerationOverflow => write!(f, "connection generation overflowed while advancing"),
        }
    }
}

impl std::error::Error for AdvanceGenerationError {}

/// Owns `generation` for one `session_id` across an entire round of
/// candidates. See module docs for why it deliberately knows nothing about
/// candidates or scheduling.
#[derive(Debug)]
pub struct GenerationCoordinator {
    session_id: SessionId,
    current_generation: ConnectionGeneration,
    generation_advances: u8,
    max_generation_advances: u8,
}

impl GenerationCoordinator {
    /// Starts a fresh coordinator at [`ConnectionGeneration::INITIAL`] with
    /// [`DEFAULT_MAX_GENERATION_ADVANCES`].
    pub fn new(session_id: SessionId) -> Self {
        Self::with_max_generation_advances(session_id, DEFAULT_MAX_GENERATION_ADVANCES)
    }

    pub fn with_max_generation_advances(session_id: SessionId, max_generation_advances: u8) -> Self {
        Self {
            session_id,
            current_generation: ConnectionGeneration::INITIAL,
            generation_advances: 0,
            max_generation_advances,
        }
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// How many times [`Self::advance_generation`] has already succeeded —
    /// exposed for telemetry (`#13a`) so a log consumer can see how close a
    /// round is to exhausting its retry budget without reconstructing it
    /// from individual attempt failures.
    pub fn generation_advances(&self) -> u8 {
        self.generation_advances
    }

    pub fn max_generation_advances(&self) -> u8 {
        self.max_generation_advances
    }

    /// The round every candidate should use right now.
    pub fn current_round(&self) -> RoundContext {
        RoundContext { session_id: self.session_id, generation: self.current_generation }
    }

    /// Whether `round` is still the coordinator's current round — the
    /// client-local acceptance gate a round runner must check before
    /// treating any attempt's success as the round's actual winner (module
    /// docs: guards against a stale generation's attempt completing late).
    pub fn is_current(&self, round: &RoundContext) -> bool {
        round.session_id == self.session_id && round.generation == self.current_generation
    }

    /// Advances to a new generation after an `AmbiguousAfterAttach` failure
    /// (never call this for `RetryablePreAttach` — nothing about the
    /// server's state requires a new generation in that case). `server_floor`
    /// is the `current_generation` a `STALE_GENERATION` reject reported, if
    /// any; the new generation is always strictly greater than *both* the
    /// coordinator's own current generation and `server_floor`, so a
    /// `STALE_GENERATION` bump can never itself immediately go stale again.
    pub fn advance_generation(
        &mut self,
        server_floor: Option<ConnectionGeneration>,
    ) -> Result<RoundContext, AdvanceGenerationError> {
        if self.generation_advances >= self.max_generation_advances {
            return Err(AdvanceGenerationError::RetryBudgetExceeded {
                advances: self.generation_advances,
                max: self.max_generation_advances,
            });
        }

        let from_self = self.current_generation.checked_next().map_err(|_| AdvanceGenerationError::GenerationOverflow)?;
        let next = match server_floor {
            Some(floor) => {
                let from_floor =
                    floor.checked_next().map_err(|_| AdvanceGenerationError::GenerationOverflow)?;
                from_self.max(from_floor)
            }
            None => from_self,
        };

        self.current_generation = next;
        self.generation_advances += 1;
        Ok(self.current_round())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(b: u8) -> SessionId {
        SessionId::from_bytes([b; 16])
    }

    #[test]
    fn initial_round_starts_at_the_initial_generation() {
        let coordinator = GenerationCoordinator::new(sid(1));
        let round = coordinator.current_round();
        assert_eq!(round.session_id, sid(1));
        assert_eq!(round.generation, ConnectionGeneration::INITIAL);
    }

    #[test]
    fn advance_generation_increments_by_one_with_no_server_floor() {
        let mut coordinator = GenerationCoordinator::new(sid(1));
        let round = coordinator.advance_generation(None).unwrap();
        assert_eq!(round.generation, ConnectionGeneration::new(1));
        assert_eq!(coordinator.current_round().generation, ConnectionGeneration::new(1));
    }

    #[test]
    fn advance_generation_jumps_past_a_higher_server_floor() {
        let mut coordinator = GenerationCoordinator::new(sid(1));
        let round = coordinator.advance_generation(Some(ConnectionGeneration::new(10))).unwrap();
        assert_eq!(round.generation, ConnectionGeneration::new(11));
    }

    #[test]
    fn advance_generation_ignores_a_server_floor_lower_than_its_own_next_value() {
        let mut coordinator = GenerationCoordinator::with_max_generation_advances(sid(1), 5);
        coordinator.advance_generation(None).unwrap(); // now at generation 1
        let round = coordinator.advance_generation(Some(ConnectionGeneration::new(0))).unwrap();
        // self.current_generation (1) + 1 = 2, which already exceeds floor+1 (1).
        assert_eq!(round.generation, ConnectionGeneration::new(2));
    }

    #[test]
    fn advance_generation_respects_the_retry_budget() {
        let mut coordinator = GenerationCoordinator::with_max_generation_advances(sid(1), 2);
        coordinator.advance_generation(None).unwrap();
        coordinator.advance_generation(None).unwrap();
        let err = coordinator.advance_generation(None).unwrap_err();
        assert_eq!(err, AdvanceGenerationError::RetryBudgetExceeded { advances: 2, max: 2 });
    }

    #[test]
    fn advance_generation_rejects_overflow() {
        // Cannot reach u64::MAX through normal advances (the retry budget
        // stops that long before), so construct the boundary condition
        // directly via repeated with_max_generation_advances-free advances
        // is impractical; instead verify ConnectionGeneration's own
        // checked_next (exercised via a coordinator already at u64::MAX)
        // surfaces as GenerationOverflow.
        let mut coordinator = GenerationCoordinator::with_max_generation_advances(sid(1), u8::MAX);
        // Force the internal generation to u64::MAX by advancing off of a
        // server_floor at u64::MAX - 1, which yields exactly u64::MAX in one
        // step without needing u64::MAX calls.
        let round = coordinator.advance_generation(Some(ConnectionGeneration::new(u64::MAX - 1))).unwrap();
        assert_eq!(round.generation, ConnectionGeneration::new(u64::MAX));

        let err = coordinator.advance_generation(None).unwrap_err();
        assert_eq!(err, AdvanceGenerationError::GenerationOverflow);
    }

    #[test]
    fn is_current_is_true_only_for_the_exact_current_round() {
        let mut coordinator = GenerationCoordinator::new(sid(1));
        let stale_round = coordinator.current_round();
        coordinator.advance_generation(None).unwrap();

        assert!(!coordinator.is_current(&stale_round), "a superseded round must not read as current");
        assert!(coordinator.is_current(&coordinator.current_round()));

        let wrong_session = RoundContext { session_id: sid(2), generation: coordinator.current_round().generation };
        assert!(!coordinator.is_current(&wrong_session), "a different session_id must never read as current");
    }
}
