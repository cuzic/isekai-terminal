//! Pure reconnect backoff calculation (`archive/ISEKAI_SSH_DESIGN.md` phase S-0d-2,
//! "`BackoffPolicy`（純粋関数、過剰な抽象化をしない）"). This module
//! deliberately does *not* introduce a `Clock` abstraction or drive an actual
//! reconnect loop — that lands in S-4 once `isekai-ssh`/resume needs it. All
//! this type does is answer "how long should attempt N wait", as a pure
//! function of its inputs (plus an explicitly-supplied RNG for the jitter
//! variant, so even that stays free of hidden global state).

use std::time::Duration;

/// Exponential backoff with optional jitter.
///
/// - `initial`: the base delay for attempt `0`.
/// - `max`: the ceiling every computed delay is clamped to.
/// - `jitter`: a fraction in `0.0..=1.0` describing how much the actual delay
///   may randomly deviate (plus or minus) from the exponential base delay.
///   `0.0` disables jitter entirely.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackoffPolicy {
    pub initial: Duration,
    pub max: Duration,
    pub jitter: f64,
}

impl BackoffPolicy {
    /// Exponential base delay for `attempt` (0-based): `initial * 2^attempt`,
    /// clamped to `max`. A pure function of its inputs only — no randomness,
    /// no I/O, no wall-clock reads, and no panics/overflow regardless of how
    /// large `attempt` is (large attempts just saturate at `max`).
    pub fn base_delay(&self, attempt: u32) -> Duration {
        // Shifting by more than 63 is UB territory for `1u64 << shift`, but
        // by shift=32 the multiplier already vastly exceeds any realistic
        // `max`, so capping the shift here is purely a safety margin, not a
        // behavior change.
        let shift = attempt.min(32);
        let multiplier: u64 = 1u64 << shift;
        let initial_millis = u64::try_from(self.initial.as_millis()).unwrap_or(u64::MAX);
        let max_millis = u64::try_from(self.max.as_millis()).unwrap_or(u64::MAX);
        let millis = initial_millis.saturating_mul(multiplier).min(max_millis);
        Duration::from_millis(millis)
    }

    /// `base_delay` with random jitter applied. `rng` is supplied by the
    /// caller rather than reached for globally, so this stays a function of
    /// its explicit inputs and is trivially reproducible with a seeded RNG in
    /// tests.
    pub fn delay_for_attempt<R: rand::Rng + ?Sized>(&self, attempt: u32, rng: &mut R) -> Duration {
        let base = self.base_delay(attempt);
        if self.jitter <= 0.0 {
            return base;
        }
        let jitter = self.jitter.min(1.0);
        let factor = 1.0 + rng.gen_range(-jitter..=jitter);
        let jittered_secs = (base.as_secs_f64() * factor).max(0.0);
        Duration::from_secs_f64(jittered_secs).min(self.max)
    }

    /// [`Self::delay_for_attempt`] with the RNG derived from a plain `u64`
    /// `seed` — a pure function of `(self, attempt, seed)`. Production
    /// callers draw `seed` fresh (`rand::random()`) once per delay; tests and
    /// reducers pass a fixed one, so the jitter is reproducible without
    /// threading an RNG type through.
    pub fn next_delay(&self, attempt: u32, seed: u64) -> Duration {
        use rand::SeedableRng as _;
        self.delay_for_attempt(attempt, &mut rand::rngs::StdRng::seed_from_u64(seed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    proptest! {
        #[test]
        fn next_delay_is_deterministic_per_seed(
            initial_ms in 1u64..5_000, max_ms in 1u64..120_000, jitter in 0.0f64..=1.0,
            attempt in any::<u32>(), seed in any::<u64>(),
        ) {
            let p = BackoffPolicy { initial: Duration::from_millis(initial_ms), max: Duration::from_millis(max_ms), jitter };
            prop_assert_eq!(p.next_delay(attempt, seed), p.next_delay(attempt, seed));
        }

        #[test]
        fn next_delay_never_exceeds_max_and_stays_within_jitter_band(
            initial_ms in 1u64..5_000, max_ms in 1u64..120_000, jitter in 0.0f64..=1.0,
            attempt in any::<u32>(), seed in any::<u64>(),
        ) {
            let p = BackoffPolicy { initial: Duration::from_millis(initial_ms), max: Duration::from_millis(max_ms), jitter };
            let got = p.next_delay(attempt, seed).as_secs_f64();
            let base = p.base_delay(attempt).as_secs_f64();
            prop_assert!(got <= p.max.as_secs_f64() + 1e-9);
            prop_assert!(got >= base * (1.0 - jitter) - 1e-9);
            prop_assert!(got <= (base * (1.0 + jitter)).min(p.max.as_secs_f64()) + 1e-9);
        }

        #[test]
        fn base_delay_is_monotonic_in_attempt(
            initial_ms in 1u64..5_000, max_ms in 1u64..120_000, a in any::<u32>(), b in any::<u32>(),
        ) {
            let p = BackoffPolicy { initial: Duration::from_millis(initial_ms), max: Duration::from_millis(max_ms), jitter: 0.0 };
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            prop_assert!(p.base_delay(lo) <= p.base_delay(hi));
        }

        #[test]
        fn zero_jitter_next_delay_ignores_the_seed(attempt in any::<u32>(), s1 in any::<u64>(), s2 in any::<u64>()) {
            let p = BackoffPolicy { initial: Duration::from_millis(500), max: Duration::from_secs(10), jitter: 0.0 };
            prop_assert_eq!(p.next_delay(attempt, s1), p.next_delay(attempt, s2));
        }
    }

    #[test]
    fn base_delay_doubles_each_attempt_until_capped() {
        let policy =
            BackoffPolicy { initial: Duration::from_millis(100), max: Duration::from_secs(5), jitter: 0.0 };
        assert_eq!(policy.base_delay(0), Duration::from_millis(100));
        assert_eq!(policy.base_delay(1), Duration::from_millis(200));
        assert_eq!(policy.base_delay(2), Duration::from_millis(400));
        assert_eq!(policy.base_delay(3), Duration::from_millis(800));
    }

    #[test]
    fn base_delay_converges_to_and_never_exceeds_max() {
        let policy =
            BackoffPolicy { initial: Duration::from_millis(100), max: Duration::from_secs(5), jitter: 0.0 };
        for attempt in 0..64 {
            assert!(policy.base_delay(attempt) <= policy.max, "attempt {attempt} exceeded max");
        }
        assert_eq!(policy.base_delay(10), policy.max);
        // Must not panic/overflow for absurdly large attempt counts either.
        assert_eq!(policy.base_delay(1_000_000), policy.max);
    }

    #[test]
    fn zero_jitter_returns_exactly_the_base_delay() {
        let policy =
            BackoffPolicy { initial: Duration::from_millis(50), max: Duration::from_secs(1), jitter: 0.0 };
        let mut rng = StdRng::seed_from_u64(42);
        for attempt in 0..5 {
            assert_eq!(policy.delay_for_attempt(attempt, &mut rng), policy.base_delay(attempt));
        }
    }

    #[test]
    fn jitter_stays_within_the_configured_fraction_and_never_exceeds_max() {
        let policy =
            BackoffPolicy { initial: Duration::from_millis(200), max: Duration::from_secs(2), jitter: 0.5 };
        let mut rng = StdRng::seed_from_u64(7);
        for attempt in 0..20 {
            let base = policy.base_delay(attempt).as_secs_f64();
            let lower = base * 0.5;
            let upper = (base * 1.5).min(policy.max.as_secs_f64());
            let got = policy.delay_for_attempt(attempt, &mut rng).as_secs_f64();
            assert!(
                got >= lower - 1e-9 && got <= upper + 1e-9,
                "attempt {attempt}: got {got}, expected within [{lower}, {upper}]"
            );
        }
    }

    #[test]
    fn jitter_actually_varies_the_delay_across_calls() {
        let policy =
            BackoffPolicy { initial: Duration::from_millis(500), max: Duration::from_secs(10), jitter: 0.3 };
        let mut rng = StdRng::seed_from_u64(123);
        let samples: Vec<_> = (0..10).map(|_| policy.delay_for_attempt(2, &mut rng)).collect();
        assert!(
            samples.windows(2).any(|w| w[0] != w[1]),
            "expected jitter to produce varying delays across calls: {samples:?}"
        );
    }

    #[test]
    fn negative_jitter_is_treated_as_disabled() {
        let policy =
            BackoffPolicy { initial: Duration::from_millis(100), max: Duration::from_secs(5), jitter: -1.0 };
        let mut rng = StdRng::seed_from_u64(1);
        assert_eq!(policy.delay_for_attempt(0, &mut rng), policy.base_delay(0));
    }
}
