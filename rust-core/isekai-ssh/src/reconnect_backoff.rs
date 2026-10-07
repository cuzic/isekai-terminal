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
//!
//! Pure since ADR_FUNCTIONAL_CORE_EFFECTS.md Step 6+7: the accounting here
//! ([`RedeployGate`], [`RecoveryBudget`]) never reads a clock or an RNG
//! itself. Time arrives as `now: Millis` stamped by the shell
//! (`connect_recovery_driver.rs`), and jitter as an explicit `seed: u64`.
//! The one reducer that drives both is `connect_recovery_fsm.rs`, shared by
//! the Unix (`wrapper.rs`) and Windows-native (`native::connect`) paths.
// 純粋モジュール(`pure_modules.toml`登録、ADR_FUNCTIONAL_CORE_EFFECTS.md §2.3)。
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::time::Duration;

use isekai_protocol::Millis;

/// How long a mid-session reconnect loop keeps retrying before giving up
/// and returning control to the user — same value and rationale as
/// `native::mux::mod::RECONNECT_BUDGET`: a live interactive session is
/// worth reconnecting for a long time, not just a few seconds, and this
/// loop only ever runs while the user's own `isekai-ssh` invocation is
/// still around to benefit from it.
pub(crate) const RECONNECT_BUDGET: Duration = Duration::from_secs(24 * 60 * 60);

/// Same exponential-backoff-with-jitter shape as
/// `native::mux::mod::ReconnectBackoff` and
/// `isekai-pipe::resume_loop::RESUME_BACKOFF` — all three are now the one
/// `isekai_transport::backoff::BackoffPolicy` (ADR_FUNCTIONAL_CORE_EFFECTS.md
/// Step 6); jitter specifically avoids every open tab's reconnect loop
/// retrying (and re-dialing) on the exact same schedule after a shared event
/// like a sleep/resume or roaming network change. Draw it with the pure
/// `next_delay(attempt, seed)`.
pub(crate) use isekai_transport::backoff::BackoffPolicy as ReconnectBackoff;

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

/// `Millis + Duration`, rounding the duration *up* to whole milliseconds and
/// saturating — so a deadline computed here is never earlier than the
/// `tokio::time::Instant + Duration` it replaced (Step 6+7: the shell's
/// `now` stamps are floored to whole milliseconds, so ceiling the delay keeps
/// every gate opening at-or-after the pre-Step-6+7 instant).
fn add_ceil(at: Millis, delay: Duration) -> Millis {
    let ms = u64::try_from(delay.as_nanos().div_ceil(1_000_000)).unwrap_or(u64::MAX);
    Millis(at.0.saturating_add(ms))
}

/// `true` if an attempt that started at `attempt_started` and failed at
/// `now` ran long enough to count as a separate, later event (see
/// [`RECONNECT_STABLE_THRESHOLD`]). `saturating_sub`, so a non-monotonic
/// `now` reads as "not stable" rather than wrapping (ADR §2.2).
fn was_stable(attempt_started: Millis, now: Millis) -> bool {
    now.saturating_sub(attempt_started) >= RECONNECT_STABLE_THRESHOLD
}

/// The single authority for "is a full re-deploy allowed right now" —
/// deliberately the *only* place this decision is made (`.claude/rules/
/// rust-ssot.md`'s "don't duplicate a judgment across two call sites"
/// principle). Since Step 6+7 it is owned by the one shared reducer
/// (`connect_recovery_fsm::ConnectRecoveryFsm`) that both
/// `wrapper.rs::run_ssh_with_connect_failure_recovery` and
/// `native::connect::run_native_connect_with_recovery` drive, so a redeploy
/// can never happen more often than [`REDEPLOY_BACKOFF`] allows on either
/// platform, regardless of which `ConnectOutcomeClass` keeps triggering it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RedeployGate {
    /// The deadline at which the *next* redeploy becomes allowed, computed
    /// once by `record_attempt` — deliberately not "the last redeploy time
    /// plus a delay recomputed on every `due()` call": the delay draws fresh
    /// jitter, so recomputing it inside `due()` made the gate's threshold
    /// wobble by ±25% on every single check (opus review round 2, BLOCKER
    /// R2-1 — found because it made two freshly added unit tests flaky, each
    /// failing roughly half the time). Storing the resolved deadline makes
    /// `due()` a pure, idempotent predicate. Step 6+7 keeps this: the
    /// jittered deadline is resolved exactly once, from the one `seed` passed
    /// to `record_attempt`.
    next_due_at: Option<Millis>,
    attempt: u32,
}

impl RedeployGate {
    pub(crate) fn new() -> Self {
        Self { next_due_at: None, attempt: 0 }
    }

    /// `true` on the very first call (no redeploy has happened yet this
    /// storm) or once the deadline [`Self::record_attempt`] resolved for the
    /// last recorded attempt has been reached.
    pub(crate) fn due(&self, now: Millis) -> bool {
        match self.next_due_at {
            None => true,
            Some(at) => now >= at,
        }
    }

    pub(crate) fn record_attempt(&mut self, now: Millis, seed: u64) {
        self.next_due_at = Some(add_ceil(now, REDEPLOY_BACKOFF.next_delay(self.attempt, seed)));
        self.attempt += 1;
    }

    /// `due()` immediately followed by `record_attempt()` if it was —
    /// atomically, as one call. Prefer this over pairing them by hand:
    /// nothing enforces that pairing (`/code-review` on `isekai-ssh` PR #115,
    /// round 2), so a call site that checks `due()` but forgets
    /// `record_attempt()` would silently leave the gate perpetually open,
    /// reintroducing the unbounded-redeploy-storm bug this type exists to
    /// prevent.
    pub(crate) fn try_consume(&mut self, now: Millis, seed: u64) -> bool {
        if !self.due(now) {
            return false;
        }
        self.record_attempt(now, seed);
        true
    }

    /// Same "this attempt ran long enough to count as a separate, later
    /// event" heuristic as [`RecoveryBudget::reset_if_stable`] — applied here
    /// too so a long-lived session that reconnects successfully many times
    /// doesn't have an unrelated, much-later blip immediately throttled as if
    /// it were still the same old storm.
    pub(crate) fn reset_if_stable(&mut self, attempt_started: Millis, now: Millis) {
        if was_stable(attempt_started, now) {
            self.next_due_at = None;
            self.attempt = 0;
        }
    }
}

/// The per-storm reconnect accounting both recovery paths used to keep as
/// three loose locals (`attempt`/`lost_since`/`lightweight_retries`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RecoveryBudget {
    /// Backoff attempt counter, bumped by every [`Self::next_backoff`] that
    /// returns a delay.
    pub(crate) attempt: u32,
    /// When the current storm's `RECONNECT_BUDGET` clock started — set on
    /// the first backoff of a storm, not on the first failure.
    pub(crate) lost_since: Option<Millis>,
    /// Lightweight (no-redeploy) retries taken this storm; compared against
    /// `connect_recovery_fsm::MAX_LIGHTWEIGHT_RETRIES`.
    pub(crate) lightweight_retries: u32,
}

impl RecoveryBudget {
    /// `attempt`/`lost_since`/`lightweight_retries` reset — see
    /// `RECONNECT_STABLE_THRESHOLD`'s own docs for why a stable-enough
    /// interval since the last reconnect resets the budget rather than
    /// letting `lost_since` stay pinned to the first-ever failure for the
    /// process's whole remaining lifetime.
    ///
    /// `lightweight_retries` resets alongside `attempt`/`lost_since` (Epic R
    /// PR2 round 2 review finding): without this, it was a
    /// *per-process-lifetime* cap rather than a per-storm one — a long-lived
    /// session that reconnects successfully five separate times, each stable
    /// for hours in between, would still hit `MAX_LIGHTWEIGHT_RETRIES` on the
    /// sixth *unrelated* blip and fall back to a full re-deploy (or, with
    /// auto-bootstrap disabled, simply stop retrying).
    pub(crate) fn reset_if_stable(&mut self, attempt_started: Millis, now: Millis) {
        if was_stable(attempt_started, now) {
            *self = Self::default();
        }
    }

    /// Checks `RECONNECT_BUDGET` against `lost_since` (starting the clock at
    /// `now` on first use) and, if there is budget left, draws the next
    /// backoff delay from `seed` and bumps `attempt`. `None` means give up.
    /// The pure half of the former async `reconnect_backoff_or_give_up`; the
    /// shell does the actual sleep (`RecoveryEffect::Backoff`).
    pub(crate) fn next_backoff(&mut self, now: Millis, seed: u64) -> Option<Duration> {
        let lost_at = *self.lost_since.get_or_insert(now);
        if now.saturating_sub(lost_at) >= RECONNECT_BUDGET {
            return None;
        }
        let delay = RECONNECT_BACKOFF.next_delay(self.attempt, seed);
        self.attempt += 1;
        Some(delay)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    use super::*;

    fn ms(d: Duration) -> u64 {
        u64::try_from(d.as_millis()).unwrap()
    }

    #[test]
    fn delay_for_attempt_grows_but_is_capped_at_max() {
        let backoff = ReconnectBackoff { initial: Duration::from_millis(100), max: Duration::from_secs(1), jitter: 0.0 };
        assert_eq!(backoff.next_delay(0, 0), Duration::from_millis(100));
        assert_eq!(backoff.next_delay(1, 0), Duration::from_millis(200));
        assert_eq!(backoff.next_delay(10, 0), Duration::from_secs(1), "must be capped at max, not keep doubling forever");
    }

    #[test]
    fn next_backoff_retries_within_budget_and_gives_up_after() {
        let mut budget = RecoveryBudget::default();
        // First call starts the clock; RECONNECT_BUDGET hasn't elapsed yet.
        assert!(budget.next_backoff(Millis(1_000), 7).is_some());
        assert_eq!(budget.attempt, 1);
        assert_eq!(budget.lost_since, Some(Millis(1_000)));
        assert!(budget.next_backoff(Millis(1_000 + ms(RECONNECT_BUDGET) + 1_000), 7).is_none());
        assert_eq!(budget.attempt, 1, "giving up must not bump the attempt counter");
    }

    #[test]
    fn next_backoff_does_not_give_up_when_now_goes_backwards() {
        let mut budget = RecoveryBudget::default();
        assert!(budget.next_backoff(Millis(ms(RECONNECT_BUDGET)), 1).is_some());
        // A `now` smaller than `lost_since` must read as "not elapsed".
        assert!(budget.next_backoff(Millis(0), 1).is_some());
    }

    #[test]
    fn reset_if_stable_resets_only_past_the_threshold() {
        let mut budget = RecoveryBudget { attempt: 5, lost_since: Some(Millis(1)), lightweight_retries: 5 };
        budget.reset_if_stable(Millis(0), Millis(ms(RECONNECT_STABLE_THRESHOLD) + 1_000));
        assert_eq!(budget.attempt, 0);
        assert!(budget.lost_since.is_none());
        assert_eq!(budget.lightweight_retries, 0, "a stable-enough attempt must also reset the lightweight-retry cap, not just the backoff budget (round 2 review: it used to be a per-process-lifetime cap)");
    }

    #[test]
    fn reset_if_stable_does_not_reset_a_short_lived_attempt() {
        let mut budget = RecoveryBudget { attempt: 5, lost_since: Some(Millis(1)), lightweight_retries: 5 };
        budget.reset_if_stable(Millis(10_000), Millis(10_000));
        assert_eq!(budget.attempt, 5, "an attempt shorter than RECONNECT_STABLE_THRESHOLD must not reset the budget");
        assert!(budget.lost_since.is_some());
        assert_eq!(budget.lightweight_retries, 5, "a short-lived attempt must not reset the lightweight-retry cap either");
        // Time going backwards must not look "stable" either.
        budget.reset_if_stable(Millis(u64::MAX), Millis(0));
        assert_eq!(budget.attempt, 5);
    }

    mod redeploy_gate_tests {
        use super::*;

        #[test]
        fn due_is_true_before_any_redeploy_has_happened() {
            let gate = RedeployGate::new();
            assert!(gate.due(Millis(0)), "the very first redeploy for a storm must not be delayed");
        }

        // `REDEPLOY_BACKOFF.next_delay(0, seed)` draws from `[45s, 75s]`
        // (60s base, ±25% jitter). Bracketing the assertions outside that
        // whole range — instead of exactly at the 60s mean (opus adversarial
        // review, PR #115 round 2, BLOCKER R2-1) — makes these hold for every
        // seed; looping over seeds checks exactly that.
        #[test]
        fn due_stays_false_until_the_jitter_range_for_the_first_attempt_has_fully_elapsed() {
            for seed in 0..64 {
                let mut gate = RedeployGate::new();
                gate.record_attempt(Millis(0), seed);
                assert!(!gate.due(Millis(44_000)), "44s is below even the minimum possible draw (45s) for the first attempt's delay");
                assert!(gate.due(Millis(76_000)), "76s is above even the maximum possible draw (75s) for the first attempt's delay");
            }
        }

        /// The Step 6+7 hard constraint: the jittered deadline is resolved
        /// once, at `record_attempt`, so repeated `due()` checks at the same
        /// `now` can never flip, and `due()` never mutates the gate.
        #[test]
        fn due_is_idempotent_because_the_jittered_deadline_is_resolved_once() {
            let mut gate = RedeployGate::new();
            gate.record_attempt(Millis(0), 12345);
            let snapshot = gate.clone();
            for now in (40_000..80_000).step_by(250) {
                let first = gate.due(Millis(now));
                for _ in 0..4 {
                    assert_eq!(gate.due(Millis(now)), first);
                }
            }
            assert_eq!(gate, snapshot, "due() must not mutate the gate");
        }

        // `next_delay(1, ..)` (the second recorded attempt) draws from
        // `[90s, 150s]` (120s base, ±25%).
        #[test]
        fn backoff_grows_with_each_recorded_attempt() {
            for seed in 0..64 {
                let mut gate = RedeployGate::new();
                gate.record_attempt(Millis(0), seed);
                gate.record_attempt(Millis(0), seed);
                assert!(!gate.due(Millis(89_000)), "89s is below even the minimum possible draw (90s) for the second attempt's delay");
                assert!(gate.due(Millis(151_000)), "151s is above even the maximum possible draw (150s) for the second attempt's delay");
            }
        }

        #[test]
        fn try_consume_records_only_when_due() {
            let mut gate = RedeployGate::new();
            assert!(gate.try_consume(Millis(0), 1));
            assert!(!gate.try_consume(Millis(1_000), 1));
            assert!(gate.try_consume(Millis(76_000), 1));
        }

        #[test]
        fn reset_if_stable_reopens_the_gate_immediately_for_a_long_since_stable_storm() {
            let mut gate = RedeployGate::new();
            gate.record_attempt(Millis(0), 1);
            gate.record_attempt(Millis(0), 1);
            assert!(!gate.due(Millis(0)));
            let now = Millis(ms(RECONNECT_STABLE_THRESHOLD) + 1_000);
            gate.reset_if_stable(Millis(0), now);
            assert!(gate.due(now), "an attempt that stayed connected past the stable threshold must reset the gate to fresh");
        }

        #[test]
        fn reset_if_stable_does_not_reopen_the_gate_for_a_short_lived_attempt() {
            let mut gate = RedeployGate::new();
            gate.record_attempt(Millis(0), 1);
            gate.reset_if_stable(Millis(0), Millis(1_000));
            assert!(!gate.due(Millis(1_000)), "a same-storm attempt must not reset the gate just because it was checked");
        }
    }
}

// 時間定数の関係テスト(ADR_DETERMINISTIC_NETWORK_SIMULATION_L1.md §4.5(a))。
#[cfg(test)]
mod timing_relations;
