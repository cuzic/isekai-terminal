//! Shared backoff/budget policy for a mid-session reconnect loop (Epic R
//! PR2, ADR §2.2.2/Q1). Deliberately the *same shape and values* as
//! `native::mux::mod`'s own `RECONNECT_BUDGET`/`ReconnectBackoff`/
//! `RECONNECT_STABLE_THRESHOLD` (the design this ADR explicitly chose to
//! reuse rather than inventing a new policy) — kept as a separate copy in
//! this crate-root module rather than importing from `native::mux::mod`
//! (which is Windows-mux-specific machinery already wired to its own
//! Ctrl+C-during-wait detection via `wait_or_abort`, itself built on
//! `native::console`'s raw-mode/console-stdin types that don't apply to
//! this module's callers). Reuse here means "same policy", not "same Rust
//! item" — see this module's own callers (`wrapper.rs` on Unix,
//! `native::connect` on Windows' single-process fallback) for why a
//! simpler, console-independent wait is the right fit for both.

use std::time::Duration;

/// How long a mid-session reconnect loop keeps retrying before giving up
/// and returning control to the user — same value and rationale as
/// `native::mux::mod::RECONNECT_BUDGET`: a live interactive session is
/// worth reconnecting for a long time, not just a few seconds, and this
/// loop only ever runs while the user's own `isekai-ssh` invocation is
/// still around to benefit from it.
pub(crate) const RECONNECT_BUDGET: Duration = Duration::from_secs(24 * 60 * 60);

/// Same exponential-backoff-with-jitter shape as
/// `native::mux::mod::ReconnectBackoff` and
/// `isekai-pipe::resume_loop::RESUME_BACKOFF` — jitter specifically to
/// avoid every open tab's reconnect loop retrying (and re-dialing) on the
/// exact same schedule after a shared event like a sleep/resume or roaming
/// network change.
pub(crate) struct ReconnectBackoff {
    pub(crate) initial: Duration,
    pub(crate) max: Duration,
    /// Fraction in `0.0..=1.0` of random jitter applied on top of the
    /// exponential delay. `0.0` disables jitter entirely.
    pub(crate) jitter: f64,
}

pub(crate) const RECONNECT_BACKOFF: ReconnectBackoff = ReconnectBackoff { initial: Duration::from_millis(500), max: Duration::from_secs(10), jitter: 0.25 };

/// A reconnect attempt that stayed connected at least this long before
/// failing again counts as a genuinely separate, later failure — not a
/// continuation of the same reconnect storm — and resets the budget back
/// to a fresh `RECONNECT_BUDGET` window. Comfortably above
/// `RECONNECT_BACKOFF.max` so a run of purely back-to-back failed attempts
/// never spuriously resets the budget that's meant to bound exactly that
/// case. Same value as `native::mux::mod::RECONNECT_STABLE_THRESHOLD`.
pub(crate) const RECONNECT_STABLE_THRESHOLD: Duration = Duration::from_secs(60);

impl ReconnectBackoff {
    fn base_delay(&self, attempt: u32) -> Duration {
        let shift = attempt.min(32);
        let multiplier: u64 = 1u64 << shift;
        let initial_millis = u64::try_from(self.initial.as_millis()).unwrap_or(u64::MAX);
        let max_millis = u64::try_from(self.max.as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(initial_millis.saturating_mul(multiplier).min(max_millis))
    }

    pub(crate) fn delay_for_attempt(&self, attempt: u32) -> Duration {
        use rand::Rng as _;
        let base = self.base_delay(attempt);
        if self.jitter <= 0.0 {
            return base;
        }
        let jitter = self.jitter.min(1.0);
        let factor = 1.0 + rand::thread_rng().gen_range(-jitter..=jitter);
        let jittered_secs = (base.as_secs_f64() * factor).max(0.0);
        Duration::from_secs_f64(jittered_secs).min(self.max)
    }
}

pub(crate) enum ReconnectDecision {
    Retry,
    GiveUp,
}

/// Checks `RECONNECT_BUDGET` against `lost_since` (starting the clock on
/// first use) and waits out the next backoff delay (bumping `attempt`).
/// Unlike `native::mux::mod`'s equivalent, does not itself watch stdin for
/// a Ctrl+C-during-wait abort — a plain `tokio::time::sleep` is
/// interruptible enough on its own: `SIGINT`'s default disposition already
/// terminates the whole process on Unix, and this function's Windows
/// caller (`native::connect`'s single-process fallback loop) is not the
/// full-terminal-raw-mode context `native::mux::mod::wait_or_abort` was
/// built for.
pub(crate) async fn reconnect_backoff_or_give_up(attempt: &mut u32, lost_since: &mut Option<tokio::time::Instant>) -> ReconnectDecision {
    let lost_at = *lost_since.get_or_insert_with(tokio::time::Instant::now);
    if lost_at.elapsed() >= RECONNECT_BUDGET {
        return ReconnectDecision::GiveUp;
    }
    let delay = RECONNECT_BACKOFF.delay_for_attempt(*attempt);
    *attempt += 1;
    tokio::time::sleep(delay).await;
    ReconnectDecision::Retry
}

/// `attempt`/`lost_since`/`lightweight_retries` reset helper — see
/// `RECONNECT_STABLE_THRESHOLD`'s own docs for why a stable-enough interval
/// since the last reconnect resets the budget rather than letting
/// `lost_since` stay pinned to the first-ever failure for the process's
/// whole remaining lifetime.
///
/// `lightweight_retries` resets alongside `attempt`/`lost_since` (Epic R PR2
/// round 2 review finding): without this, it was a *per-process-lifetime*
/// cap rather than a per-storm one — a long-lived session (a `tmux` pane
/// left open for days over a flaky link) that reconnects successfully five
/// separate times, each stable for hours in between, would still hit
/// `MAX_LIGHTWEIGHT_RETRIES` on the sixth *unrelated* blip and fall back to
/// a full re-deploy (or, with auto-bootstrap disabled, simply stop
/// retrying) even though every previous reconnect had nothing wrong with
/// it. Resetting it on the same "was the last attempt stable" signal that
/// already resets the backoff budget keeps both counters describing the
/// same thing: how bad *this* reconnect storm has been, not how many
/// reconnects have ever happened.
pub(crate) fn reset_budget_if_stable(attempt_started: tokio::time::Instant, attempt: &mut u32, lost_since: &mut Option<tokio::time::Instant>, lightweight_retries: &mut u32) {
    if attempt_started.elapsed() >= RECONNECT_STABLE_THRESHOLD {
        *attempt = 0;
        *lost_since = None;
        *lightweight_retries = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_for_attempt_grows_but_is_capped_at_max() {
        let backoff = ReconnectBackoff { initial: Duration::from_millis(100), max: Duration::from_secs(1), jitter: 0.0 };
        assert_eq!(backoff.delay_for_attempt(0), Duration::from_millis(100));
        assert_eq!(backoff.delay_for_attempt(1), Duration::from_millis(200));
        assert_eq!(backoff.delay_for_attempt(10), Duration::from_secs(1), "must be capped at max, not keep doubling forever");
    }

    #[tokio::test]
    async fn reconnect_backoff_or_give_up_retries_within_budget_and_gives_up_after() {
        tokio::time::pause();
        let mut attempt = 0u32;
        let mut lost_since = None;
        // First call starts the clock; RECONNECT_BUDGET hasn't elapsed yet.
        assert!(matches!(reconnect_backoff_or_give_up(&mut attempt, &mut lost_since).await, ReconnectDecision::Retry));
        assert_eq!(attempt, 1);

        tokio::time::advance(RECONNECT_BUDGET + Duration::from_secs(1)).await;
        assert!(matches!(reconnect_backoff_or_give_up(&mut attempt, &mut lost_since).await, ReconnectDecision::GiveUp));
    }

    #[test]
    fn reset_budget_if_stable_resets_only_past_the_threshold() {
        let mut attempt = 5u32;
        let mut lost_since = Some(tokio::time::Instant::now());
        let mut lightweight_retries = 5u32;
        let started_long_ago = tokio::time::Instant::now() - (RECONNECT_STABLE_THRESHOLD + Duration::from_secs(1));
        reset_budget_if_stable(started_long_ago, &mut attempt, &mut lost_since, &mut lightweight_retries);
        assert_eq!(attempt, 0);
        assert!(lost_since.is_none());
        assert_eq!(lightweight_retries, 0, "a stable-enough attempt must also reset the lightweight-retry cap, not just the backoff budget (round 2 review: it used to be a per-process-lifetime cap)");
    }

    #[test]
    fn reset_budget_if_stable_does_not_reset_a_short_lived_attempt() {
        let mut attempt = 5u32;
        let mut lost_since = Some(tokio::time::Instant::now());
        let mut lightweight_retries = 5u32;
        let started_recently = tokio::time::Instant::now();
        reset_budget_if_stable(started_recently, &mut attempt, &mut lost_since, &mut lightweight_retries);
        assert_eq!(attempt, 5, "an attempt shorter than RECONNECT_STABLE_THRESHOLD must not reset the budget");
        assert!(lost_since.is_some());
        assert_eq!(lightweight_retries, 5, "a short-lived attempt must not reset the lightweight-retry cap either");
    }
}
