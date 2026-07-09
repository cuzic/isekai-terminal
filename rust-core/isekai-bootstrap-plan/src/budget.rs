//! [`BootstrapBudget`]: per-phase timeout allocation for a
//! [`crate::BootstrapPlan`] attempt. `ISEKAI_PIPE_DESIGN.md` §8 Epic A notes
//! that trying SSH bootstrap, candidate gathering, a direct attempt, and a
//! relay fallback one after another with independent timeouts makes
//! connection attempts unpredictably slow — this type makes the phase split
//! explicit so an executor allocates a fixed slice of one overall deadline
//! to each phase instead of stacking full timeouts on top of each other.
//!
//! Duration-only by design, the same way `isekai_pipe_core::CandidateValidity`
//! deliberately holds no `Instant`: this crate has no runtime clock to
//! consult, so it defines the policy, not the deadline. An executor resolves
//! these durations against its own clock when a plan actually starts
//! running.

use std::time::Duration;

/// One stage of a bootstrap attempt that gets its own slice of the overall
/// budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BootstrapPhase {
    /// Establishing the SSH connection(s) needed to deploy/reach
    /// `isekai-pipe serve` (the jump chain plus the final hop).
    SshBootstrap,
    /// Collecting and exchanging connection candidates (STUN-observed
    /// addresses, relay endpoints) once the SSH bootstrap has completed.
    CandidateGathering,
    /// Dialing a direct or STUN-P2P candidate.
    DirectAttempt,
    /// Falling back to a relay candidate after direct/STUN attempts were
    /// exhausted or ruled out.
    RelayFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapBudget {
    overall: Duration,
    ssh_bootstrap: Duration,
    candidate_gathering: Duration,
    direct_attempt: Duration,
    relay_fallback: Duration,
}

impl BootstrapBudget {
    /// Builds a budget, rejecting any phase allocation that alone exceeds
    /// the overall deadline (a phase's slice can never legitimately be
    /// larger than the whole attempt is allowed to take).
    pub fn new(
        overall: Duration,
        ssh_bootstrap: Duration,
        candidate_gathering: Duration,
        direct_attempt: Duration,
        relay_fallback: Duration,
    ) -> Result<Self, BudgetError> {
        for (phase, dur) in [
            (BootstrapPhase::SshBootstrap, ssh_bootstrap),
            (BootstrapPhase::CandidateGathering, candidate_gathering),
            (BootstrapPhase::DirectAttempt, direct_attempt),
            (BootstrapPhase::RelayFallback, relay_fallback),
        ] {
            if dur > overall {
                return Err(BudgetError::PhaseExceedsOverall { phase, phase_budget: dur, overall });
            }
        }
        Ok(Self { overall, ssh_bootstrap, candidate_gathering, direct_attempt, relay_fallback })
    }

    /// Splits `overall` evenly across all four phases — a reasonable
    /// starting point for a caller that has no more specific policy yet;
    /// always valid (each quarter never exceeds the whole).
    pub fn split_evenly(overall: Duration) -> Self {
        let quarter = overall / 4;
        Self { overall, ssh_bootstrap: quarter, candidate_gathering: quarter, direct_attempt: quarter, relay_fallback: quarter }
    }

    pub fn overall(&self) -> Duration {
        self.overall
    }

    pub fn phase(&self, phase: BootstrapPhase) -> Duration {
        match phase {
            BootstrapPhase::SshBootstrap => self.ssh_bootstrap,
            BootstrapPhase::CandidateGathering => self.candidate_gathering,
            BootstrapPhase::DirectAttempt => self.direct_attempt,
            BootstrapPhase::RelayFallback => self.relay_fallback,
        }
    }

    /// Sum of every phase's allocation — deliberately *not* validated to
    /// stay `<= overall` at construction time, since not every phase runs
    /// on every attempt (a plan whose `RoutePolicy` never allows `Relay`
    /// never spends `relay_fallback`). Callers that want to detect a budget
    /// that would blow the overall deadline if every phase actually ran
    /// sequentially should check [`Self::exceeds_overall_if_all_phases_run`].
    pub fn total_if_all_phases_run(&self) -> Duration {
        self.ssh_bootstrap + self.candidate_gathering + self.direct_attempt + self.relay_fallback
    }

    pub fn exceeds_overall_if_all_phases_run(&self) -> bool {
        self.total_if_all_phases_run() > self.overall
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BudgetError {
    #[error("{phase:?} budget {phase_budget:?} exceeds the overall deadline {overall:?}")]
    PhaseExceedsOverall { phase: BootstrapPhase, phase_budget: Duration, overall: Duration },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_evenly_divides_overall_into_four_equal_phases() {
        let budget = BootstrapBudget::split_evenly(Duration::from_secs(40));
        assert_eq!(budget.overall(), Duration::from_secs(40));
        assert_eq!(budget.phase(BootstrapPhase::SshBootstrap), Duration::from_secs(10));
        assert_eq!(budget.phase(BootstrapPhase::CandidateGathering), Duration::from_secs(10));
        assert_eq!(budget.phase(BootstrapPhase::DirectAttempt), Duration::from_secs(10));
        assert_eq!(budget.phase(BootstrapPhase::RelayFallback), Duration::from_secs(10));
        assert!(!budget.exceeds_overall_if_all_phases_run());
    }

    #[test]
    fn rejects_a_phase_budget_larger_than_overall() {
        let err = BootstrapBudget::new(
            Duration::from_secs(10),
            Duration::from_secs(20),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(
            err,
            BudgetError::PhaseExceedsOverall {
                phase: BootstrapPhase::SshBootstrap,
                phase_budget: Duration::from_secs(20),
                overall: Duration::from_secs(10),
            }
        );
    }

    #[test]
    fn accepts_phases_that_individually_fit_but_would_overrun_if_summed() {
        // Each phase alone is within the overall deadline, but running all
        // four sequentially would blow it — construction succeeds (not
        // every phase necessarily runs), while the sum-exceeds check
        // flags it for a caller that wants to know.
        let budget = BootstrapBudget::new(
            Duration::from_secs(10),
            Duration::from_secs(8),
            Duration::from_secs(8),
            Duration::from_secs(8),
            Duration::from_secs(8),
        )
        .unwrap();
        assert!(budget.exceeds_overall_if_all_phases_run());
        assert_eq!(budget.total_if_all_phases_run(), Duration::from_secs(32));
    }
}
