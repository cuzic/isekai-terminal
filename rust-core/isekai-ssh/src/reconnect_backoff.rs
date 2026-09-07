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
/// to a fresh `RECONNECT_BUDGET` window.
///
/// Comfortably above `RECONNECT_BACKOFF.max` (10s) so a run of purely
/// back-to-back failed attempts never spuriously resets the budget that's
/// meant to bound exactly that case — but must *also* stay above the
/// longest a single attempt can legitimately take to fail at all, not just
/// the backoff *between* attempts. `isekai-pipe::resume_loop::
/// BUSY_OTHER_SESSION_RETRY_WINDOW` is 180s: a single `isekai-pipe connect`
/// invocation (what `run_ssh_once`/`ConnectRecoveryOps::attempt` each
/// measure `attempt_started` around) can spend up to that long retrying
/// internally before ever reporting failure back to this crate. At the
/// previous value of 60s, every attempt that hit that internal retry ceiling
/// looked "stable" purely from having taken a while to fail — during a real,
/// ongoing outage this reset the redeploy gate and lightweight-retry budget
/// back to fresh on every single failure, defeating the storm protection
/// both exist for (`/code-review` on `isekai-ssh` PR #115, round 2: 200s
/// gives 20s of margin over the 180s ceiling for the scheduling/connection
/// overhead surrounding that internal retry loop, without being so large it
/// meaningfully delays recognizing an actually-new, later failure).
///
/// This diverges from `native::mux::mod::RECONNECT_STABLE_THRESHOLD`
/// (still 60s), which historically documented the same value — that copy
/// gates `native::mux::mod::run_with_reconnect`'s own reconnect loop
/// (Windows' default mux/`ControlMaster`-equivalent path), which also wraps
/// an `isekai-pipe connect` child and is exposed to the identical false-
/// stable-reset risk, but fixing it is out of scope for the PR that found
/// this (scoped to the `RedeployGate`/lightweight-retry code this crate's
/// `run_ssh_with_connect_failure_recovery`/`drive_connect_recovery` own) —
/// tracked as a known follow-up, not silently forgotten.
pub(crate) const RECONNECT_STABLE_THRESHOLD: Duration = Duration::from_secs(200);

/// How often a full re-deploy (`bootstrap_and_register`: a *separate* SSH
/// dial to the bootstrap host, re-uploading/relaunching `isekai-pipe serve`)
/// may run, independent of which `ConnectOutcomeClass` keeps triggering it.
/// The first-ever re-deploy for a storm is immediate — the "cached trust
/// went stale, one redeploy fixes it" case, `wrapper.rs`'s most common
/// `RebootstrapAndRetry` scenario, must not regress in latency — but
/// subsequent ones back off 60s → 120s → 240s → capped at 300s. 60s as the
/// floor was originally chosen to numerically match
/// [`RECONNECT_STABLE_THRESHOLD`] — that constant later grew to 200s for an
/// unrelated reason (see its own docs), so the two are no longer equal, but
/// this one's own reasoning (a redeploy costs two real SSH logins; 60s
/// between attempts one and two is a reasonable floor on its own) still
/// holds independently.
///
/// This exists because `decide_connect_failure_recovery` gives
/// `Unreachable`/`StaleTrust`/`Unknown` no lightweight (no-redeploy) tier at
/// all — every single failure classified that way drives
/// `ConnectFailureRecoveryAction::RebootstrapAndRetry` — so without this
/// gate, a *live* network with a genuinely broken remote helper (disk full,
/// crash-looping, port conflict; `isekai-bootstrap::reuse`'s pid/fingerprint/
/// sha256 check makes the redeploy itself a no-op against a still-running
/// but stuck helper) would redeploy on every retry, forever: at
/// [`RECONNECT_BACKOFF`]'s 10s cap that is roughly 17,000 SSH logins/day
/// against the target host (2 per redeploy attempt) for zero effect (opus
/// adversarial review, `isekai-ssh` PR #115 round 2). While the gate is
/// closed, `RebootstrapAndRetry` falls through to a plain lightweight
/// reconnect with the existing intent instead — most reconnects after a
/// real, transient network blip need nothing more than that, matching both
/// `ADR_MIDSESSION_DISCONNECT_RECOVERY.md`'s own observation ("re-deploying
/// the helper is often unnecessary — the server-side helper is usually
/// still alive") and tssh/tsshd's actual design (confirmed by reading
/// `tssh/udp.go` and `tsshd/server.go`: `tsshd` stays resident across
/// reconnects and a reconnecting client simply re-joins the existing
/// session; `tssh` never re-deploys `tsshd`).
pub(crate) const REDEPLOY_BACKOFF: ReconnectBackoff = ReconnectBackoff { initial: Duration::from_secs(60), max: Duration::from_secs(300), jitter: 0.25 };

/// The single authority for "is a full re-deploy allowed right now" —
/// deliberately the *only* place this decision is made (`.claude/rules/
/// rust-ssot.md`'s "don't duplicate a judgment across two call sites"
/// principle): `wrapper.rs::run_ssh_with_connect_failure_recovery` and
/// `native::connect::drive_connect_recovery` (Windows single-process
/// fallback) share this exact type — not just the same policy — so a
/// redeploy can never happen more often than [`REDEPLOY_BACKOFF`] allows on
/// either platform, regardless of which `ConnectOutcomeClass` keeps
/// triggering it.
pub(crate) struct RedeployGate {
    /// The instant at which the *next* redeploy becomes allowed, computed
    /// once by `record_attempt` — deliberately not "the last redeploy time
    /// plus a delay recomputed on every `due()` call": `delay_for_attempt`
    /// draws fresh jitter on every call, so recomputing it inside `due()`
    /// made the gate's threshold wobble by ±25% on every single check (opus
    /// review round 2, BLOCKER R2-1 — found because it made two freshly
    /// added unit tests flaky, each failing roughly half the time). Storing
    /// the resolved instant makes `due()` a pure, idempotent predicate.
    next_due_at: Option<tokio::time::Instant>,
    attempt: u32,
}

impl RedeployGate {
    pub(crate) fn new() -> Self {
        Self { next_due_at: None, attempt: 0 }
    }

    /// `true` on the very first call (no redeploy has happened yet this
    /// storm) or once the delay [`Self::record_attempt`] resolved for the
    /// last recorded attempt has elapsed.
    pub(crate) fn due(&self) -> bool {
        match self.next_due_at {
            None => true,
            Some(at) => tokio::time::Instant::now() >= at,
        }
    }

    pub(crate) fn record_attempt(&mut self) {
        self.next_due_at = Some(tokio::time::Instant::now() + REDEPLOY_BACKOFF.delay_for_attempt(self.attempt));
        self.attempt += 1;
    }

    /// `due()` immediately followed by `record_attempt()` if it was —
    /// atomically, as one call. Prefer this at call sites over pairing
    /// `due()`/`record_attempt()` by hand: nothing enforces that pairing
    /// (`/code-review` on `isekai-ssh` PR #115, round 2), so a future call
    /// site that checks `due()` but forgets `record_attempt()` on some new
    /// branch would silently leave the gate perpetually open, reintroducing
    /// the unbounded-redeploy-storm bug this type exists to prevent.
    /// `due()`/`record_attempt()` stay separate (pub(crate)) only for tests
    /// that need to inspect gate state without mutating it.
    pub(crate) fn try_consume(&mut self) -> bool {
        if !self.due() {
            return false;
        }
        self.record_attempt();
        true
    }

    /// Same "this attempt ran long enough to count as a separate, later
    /// event" heuristic as [`reset_budget_if_stable`] — applied here too so
    /// a long-lived session that reconnects successfully many times doesn't
    /// have an unrelated, much-later blip immediately throttled as if it
    /// were still the same old storm.
    pub(crate) fn reset_if_stable(&mut self, attempt_started: tokio::time::Instant) {
        if attempt_started.elapsed() >= RECONNECT_STABLE_THRESHOLD {
            self.next_due_at = None;
            self.attempt = 0;
        }
    }
}

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

    mod redeploy_gate_tests {
        use super::*;

        #[test]
        fn due_is_true_before_any_redeploy_has_happened() {
            let gate = RedeployGate::new();
            assert!(gate.due(), "the very first redeploy for a storm must not be delayed");
        }

        // `REDEPLOY_BACKOFF.delay_for_attempt(0)` draws from `[45s, 75s]`
        // (60s base, ±25% jitter). Bracketing the assertions outside that
        // whole range — instead of exactly at the 60s mean, which
        // `record_attempt`'s fresh jitter draw made a ~50%-flaky boundary
        // before this fix (opus adversarial review, PR #115 round 2,
        // BLOCKER R2-1) — makes these deterministic regardless of which
        // value was actually drawn.
        #[tokio::test(start_paused = true)]
        async fn due_stays_false_until_the_jitter_range_for_the_first_attempt_has_fully_elapsed() {
            let mut gate = RedeployGate::new();
            gate.record_attempt();
            tokio::time::advance(Duration::from_secs(44)).await;
            assert!(!gate.due(), "44s is below even the minimum possible draw (45s) for the first attempt's delay");
            tokio::time::advance(Duration::from_secs(32)).await; // cumulative 76s
            assert!(gate.due(), "76s is above even the maximum possible draw (75s) for the first attempt's delay");
        }

        // `delay_for_attempt(1)` (the second recorded attempt) draws from
        // `[90s, 150s]` (120s base, ±25%) — same bracketing-outside-the-range
        // approach, chosen to also prove the delay actually grew between the
        // first and second call rather than staying pinned at the first
        // attempt's `[45s, 75s]` range.
        #[tokio::test(start_paused = true)]
        async fn backoff_grows_with_each_recorded_attempt() {
            let mut gate = RedeployGate::new();
            gate.record_attempt(); // attempt 0 recorded -> next delay drawn from [45s, 75s]
            gate.record_attempt(); // attempt 1 recorded -> next delay drawn from [90s, 150s]
            tokio::time::advance(Duration::from_secs(89)).await;
            assert!(!gate.due(), "89s is below even the minimum possible draw (90s) for the second attempt's delay");
            tokio::time::advance(Duration::from_secs(62)).await; // cumulative 151s
            assert!(gate.due(), "151s is above even the maximum possible draw (150s) for the second attempt's delay");
        }

        #[tokio::test(start_paused = true)]
        async fn reset_if_stable_reopens_the_gate_immediately_for_a_long_since_stable_storm() {
            let mut gate = RedeployGate::new();
            gate.record_attempt();
            gate.record_attempt();
            assert!(!gate.due());
            let attempt_started = tokio::time::Instant::now();
            tokio::time::advance(RECONNECT_STABLE_THRESHOLD + Duration::from_secs(1)).await;
            gate.reset_if_stable(attempt_started);
            assert!(gate.due(), "an attempt that stayed connected past the stable threshold must reset the gate to fresh");
        }

        #[tokio::test(start_paused = true)]
        async fn reset_if_stable_does_not_reopen_the_gate_for_a_short_lived_attempt() {
            let mut gate = RedeployGate::new();
            gate.record_attempt();
            let attempt_started = tokio::time::Instant::now();
            tokio::time::advance(Duration::from_secs(1)).await;
            gate.reset_if_stable(attempt_started);
            assert!(!gate.due(), "a same-storm attempt must not reset the gate just because it was checked");
        }
    }
}
