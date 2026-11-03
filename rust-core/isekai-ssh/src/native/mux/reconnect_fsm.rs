//! The pure reducer behind [`super::run_with_reconnect`] — the Windows-native
//! mux holder's own independent "attempt → classify outcome → maybe backoff →
//! retry" loop, flagged in `connect_recovery_fsm.rs`'s module doc as *not*
//! part of that reducer and still unpurified
//! (docs/adr/0019-functional-core-effects.md, Step S1 of the follow-up
//! program). Before this, the decision (is this failure retryable yet? has
//! this attempt been "stable" long enough to reset the storm clock? is the
//! budget exhausted? what's the next backoff delay?) lived inline in
//! `run_with_reconnect`/`reconnect_backoff_or_give_up`, mixed with the actual
//! `connect::prepare`/`dispatch`/`wait_or_abort` I/O calls. Now the decisions
//! live here, once, as a pure reducer (§2.1:
//! `apply(&mut self, Event) -> Vec<Effect>`), and the shell
//! (`super::run_with_reconnect`) only performs the one I/O operation the
//! current state asks for and feeds the result back.
//!
//! - Time arrives only as `now`/`attempt_started: Millis` stamped by the
//!   shell from its own `tokio::time::Instant` epoch; all differences use
//!   `saturating_sub` (§2.2).
//! - Jitter arrives as an explicit `seed: u64` on the events that draw a
//!   fresh backoff delay.
//! - Every effect the shell must answer (`Prepare`/`Dispatch`/`Backoff`)
//!   carries a [`Token`]; an answering event whose token isn't the
//!   outstanding one is ignored without changing state or emitting effects
//!   (§2.2 stale-guard).
//! - A successful `Prepare`/`Dispatch` never reaches [`ReconnectFsm::apply`]
//!   at all (same convention as `connect_recovery_fsm::ConnectRecoveryFsm`'s
//!   `Attempt`): the shell just moves straight on to the next phase by
//!   calling [`ReconnectFsm::begin_dispatch`] directly, mirroring
//!   [`ReconnectFsm::start`] itself.
//! - Values are unchanged from `native::mux::mod`'s own pre-existing
//!   constants: [`RECONNECT_BUDGET`] (24h), [`RECONNECT_BACKOFF`] (500ms/10s/
//!   25% jitter), and — **deliberately, do not reconcile with
//!   `connect_recovery_fsm`'s/`reconnect_backoff.rs`'s 200s value** —
//!   [`RECONNECT_STABLE_THRESHOLD`] (60s). See that constant's own doc
//!   comment and `reconnect_backoff.rs`'s `RECONNECT_STABLE_THRESHOLD` doc
//!   for why the two diverge and must stay that way.
//!
//! ## Deliberately *not* folded into this reducer
//!
//! `dispatch`'s own decision tree (which local-pipe connect error branch to
//! take, the `SpawnLock` leader/follower split, `connect_with_retry`'s
//! poll-until-deadline loop) was read closely while scoping this extraction
//! and *not* turned into a second stateful reducer here. Unlike this loop,
//! it has no accumulating storm state, no stale-token risk (each `dispatch`
//! call is a single linear sequence with nothing left "awaiting" once it
//! returns), and — most importantly — it was never duplicated anywhere else
//! in this codebase the way the backoff/retry loop shape was (the bug class
//! `connect_recovery_fsm.rs`'s own doc comment cites, `a266f1f3`/`857f6ae6`
//! D-4, is specifically about *two drifting copies of the same loop*).
//! Forcing `dispatch`'s single ordinary `match` over `ConnectError`/`Result`
//! variants (which already gets Rust's own exhaustiveness checking for free)
//! into `Event`/`Effect`/`apply()` ceremony would add indirection without
//! reducing real risk. Left as plain async code in `mod.rs`, as a deliberate
//! scope decision for this PR (see its description).
// 純粋モジュール(`pure_modules.toml`登録、docs/adr/0019-functional-core-effects.md §2.3)。
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::time::Duration;

use isekai_protocol::Millis;

/// Same exponential-backoff-with-jitter shape as
/// `connect_recovery_fsm`'s/`reconnect_backoff.rs`'s own
/// `ReconnectBackoff`/`isekai-pipe::resume_loop::RESUME_BACKOFF` — all are
/// the one `isekai_transport::backoff::BackoffPolicy`
/// (docs/adr/0019-functional-core-effects.md Step 6, already unified before
/// this PR — `native::mux::mod` already used this type directly, not a local
/// copy of the math). Draw it with the pure `next_delay(attempt, seed)`.
pub(crate) use isekai_transport::backoff::BackoffPolicy as ReconnectBackoff;

/// How long this loop keeps retrying an `OwnerLost`/retryable `prepare`-or-
/// `dispatch` failure before giving up and surfacing
/// [`crate::EXIT_MUX_OWNER_LOST`] to the user after all. Generous rather than
/// a small fixed attempt count: this loop only ever runs while the user's own
/// terminal is still open, so a live interactive session is worth
/// reconnecting for a long time. Moved here unchanged from
/// `native::mux::mod::RECONNECT_BUDGET`.
pub(crate) const RECONNECT_BUDGET: Duration = Duration::from_secs(24 * 60 * 60);

/// Moved here unchanged from `native::mux::mod::RECONNECT_BACKOFF`.
pub(crate) const RECONNECT_BACKOFF: ReconnectBackoff = ReconnectBackoff { initial: Duration::from_millis(500), max: Duration::from_secs(10), jitter: 0.25 };

/// An `OwnerLost` attempt that stayed connected at least this long before
/// losing its (new) owner again counts as a genuinely separate, later
/// failure — not a continuation of the same reconnect storm — and resets
/// `attempt`/`lost_since` back to a fresh [`RECONNECT_BUDGET`] window.
/// Moved here unchanged from `native::mux::mod::RECONNECT_STABLE_THRESHOLD`
/// — **do not change this value or reconcile it with
/// `reconnect_backoff::RECONNECT_STABLE_THRESHOLD`'s 200s** (see that
/// constant's own doc comment: the two were once equal by coincidence and
/// diverged for an unrelated, documented reason; this PR is a structural
/// move only).
pub(crate) const RECONNECT_STABLE_THRESHOLD: Duration = Duration::from_secs(60);

/// `true` if an attempt that started at `attempt_started` and failed/lost
/// its owner at `now` ran long enough to count as a separate, later event
/// (see [`RECONNECT_STABLE_THRESHOLD`]'s own docs). `saturating_sub`, so a
/// non-monotonic `now` reads as "not stable" rather than wrapping (ADR §2.2).
fn was_stable(attempt_started: Millis, now: Millis) -> bool {
    now.saturating_sub(attempt_started) >= RECONNECT_STABLE_THRESHOLD
}

/// Generation token of the one effect the shell currently owes an answer
/// for (§2.2 stale-guard). Issued by the reducer, strictly increasing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Token(pub(crate) u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReconnectEvent {
    /// `connect::prepare` failed. Carries nothing about the error itself
    /// (not pure data) — the shell keeps that for the `PrepareFailedRetrying`
    /// log line and for [`ReconnectEffect::PropagateFatalError`].
    PrepareFailed { token: Token, now: Millis, seed: u64 },
    /// `dispatch` returned an `Err` (not an `OwnerLost`/`Done` outcome).
    DispatchFailed { token: Token, now: Millis, seed: u64 },
    /// `dispatch` returned `DispatchOutcome::OwnerLost` — the mux holder this
    /// attempt was relaying through died mid-session.
    DispatchOwnerLost {
        token: Token,
        /// Whether this invocation runs a one-shot remote command
        /// (`isekai-ssh host -- cmd`), captured by the shell right after
        /// `prepare` succeeded, before `prepared` moved into `dispatch`.
        has_remote_command: bool,
        /// Stamped right before this `prepare`/`dispatch` attempt started.
        attempt_started: Millis,
        now: Millis,
        seed: u64,
    },
    BackoffElapsed { token: Token },
    /// The backoff wait was aborted via Ctrl+C (`wait_or_abort`).
    BackoffAborted { token: Token },
}

/// Log lines whose dynamic content (an `anyhow::Error`'s `{:#}` rendering)
/// the shell fills in from state it keeps locally — the reducer never holds
/// an `anyhow::Error` itself (not comparable/clonable data, and ADR §3-3
/// keeps non-decision payloads out of pure Events/Effects).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReconnectLog {
    /// "isekai-ssh: preparing to reconnect failed ({e:#}); retrying".
    PrepareFailedRetrying,
    /// "isekai-ssh: reconnect attempt failed ({e:#}); retrying".
    DispatchFailedRetrying,
    /// "isekai-ssh: connection to the isekai-ssh owner process was lost while
    /// running a remote command; not auto-retrying ...".
    RemoteCommandInterrupted,
    /// "isekai-ssh: connection to the isekai-ssh owner process was lost;
    /// reconnecting...".
    OwnerLostReconnecting,
    /// "isekai-ssh: reconnecting in {delay:?}...".
    Reconnecting { delay: Duration },
    /// "isekai-ssh: automatic reconnection gave up after {RECONNECT_BUDGET:?}
    /// — reconnect with `isekai-ssh <host>`.".
    BudgetExhausted,
    /// "isekai-ssh: reconnect canceled (Ctrl+C).".
    Canceled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReconnectEffect {
    /// Call `connect::prepare(args)`; answer with `PrepareFailed { token, .. }`
    /// (a success is never reported back — the shell moves straight on to
    /// [`ReconnectFsm::begin_dispatch`]).
    Prepare { token: Token },
    /// Call `dispatch(prepared, ..)`; answer with `DispatchFailed` or
    /// `DispatchOwnerLost` (a `DispatchOutcome::Done` is returned directly by
    /// the shell, same convention as `Prepare`'s success).
    Dispatch { token: Token },
    /// Wait out `delay` via `wait_or_abort`; answer with `BackoffElapsed` or
    /// `BackoffAborted`.
    Backoff { token: Token, delay: Duration },
    Log(ReconnectLog),
    /// Stop and return this exit code (`EXIT_MUX_OWNER_LOST` or
    /// `EXIT_USER_CANCELED`).
    GiveUp { exit_code: u8 },
    /// Stop and return the last `prepare`/`dispatch` call's own error — this
    /// is the *first-ever* failure on this exact loop (never yet recovered
    /// from an `OwnerLost`), which has always been treated as a fresh
    /// misconfiguration rather than something to retry.
    PropagateFatalError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Awaiting {
    NotStarted,
    Preparing(Token),
    Dispatching(Token),
    Backoff(Token),
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReconnectFsm {
    attempt: u32,
    lost_since: Option<Millis>,
    next_token: u64,
    awaiting: Awaiting,
}

impl ReconnectFsm {
    pub(crate) fn new() -> Self {
        Self { attempt: 0, lost_since: None, next_token: 0, awaiting: Awaiting::NotStarted }
    }

    /// The first effect: one `Prepare` attempt. A second call returns
    /// nothing.
    pub(crate) fn start(&mut self) -> Vec<ReconnectEffect> {
        if self.awaiting != Awaiting::NotStarted {
            return Vec::new();
        }
        self.begin_prepare()
    }

    /// Called directly by the shell right after a successful `prepare` (never
    /// through [`Self::apply`] — a success is never reported as an `Event`,
    /// same convention as `connect_recovery_fsm`'s `Attempt`). A no-op unless
    /// currently awaiting a `Prepare` answer.
    pub(crate) fn begin_dispatch(&mut self) -> Vec<ReconnectEffect> {
        if !matches!(self.awaiting, Awaiting::Preparing(_)) {
            return Vec::new();
        }
        let token = self.issue();
        self.awaiting = Awaiting::Dispatching(token);
        vec![ReconnectEffect::Dispatch { token }]
    }

    pub(crate) fn apply(&mut self, event: ReconnectEvent) -> Vec<ReconnectEffect> {
        match (self.awaiting, event) {
            (Awaiting::Preparing(cur), ReconnectEvent::PrepareFailed { token, now, seed }) if token == cur => {
                self.on_retryable_failure(now, seed, ReconnectLog::PrepareFailedRetrying)
            }
            (Awaiting::Dispatching(cur), ReconnectEvent::DispatchFailed { token, now, seed }) if token == cur => {
                self.on_retryable_failure(now, seed, ReconnectLog::DispatchFailedRetrying)
            }
            (Awaiting::Dispatching(cur), ReconnectEvent::DispatchOwnerLost { token, has_remote_command, attempt_started, now, seed }) if token == cur => {
                self.on_owner_lost(has_remote_command, attempt_started, now, seed)
            }
            (Awaiting::Backoff(cur), ReconnectEvent::BackoffElapsed { token }) if token == cur => self.begin_prepare(),
            (Awaiting::Backoff(cur), ReconnectEvent::BackoffAborted { token }) if token == cur => {
                self.finish(vec![ReconnectEffect::Log(ReconnectLog::Canceled)], ReconnectEffect::GiveUp { exit_code: crate::EXIT_USER_CANCELED })
            }
            // Stale/unexpected answer (wrong token, wrong kind, or after
            // `Done`): no state change, no effects (§2.2).
            _ => Vec::new(),
        }
    }

    fn issue(&mut self) -> Token {
        let token = Token(self.next_token);
        self.next_token += 1;
        token
    }

    fn begin_prepare(&mut self) -> Vec<ReconnectEffect> {
        let token = self.issue();
        self.awaiting = Awaiting::Preparing(token);
        vec![ReconnectEffect::Prepare { token }]
    }

    fn finish(&mut self, mut effects: Vec<ReconnectEffect>, last: ReconnectEffect) -> Vec<ReconnectEffect> {
        self.awaiting = Awaiting::Done;
        effects.push(last);
        effects
    }

    /// `prepare`/`dispatch` failed outright (not an `OwnerLost` outcome).
    /// Mirrors `run_with_reconnect`'s original
    /// `Err(e) if lost_since.is_some() => retry; Err(e) => return Err(e)`: a
    /// retry is only ever attempted once this loop has already recovered
    /// from at least one `OwnerLost` before (`lost_since.is_some()`) — the
    /// very first failure on a brand-new loop still fails fast, exactly as
    /// it always has.
    fn on_retryable_failure(&mut self, now: Millis, seed: u64, log: ReconnectLog) -> Vec<ReconnectEffect> {
        if self.lost_since.is_none() {
            return self.finish(Vec::new(), ReconnectEffect::PropagateFatalError);
        }
        let mut effects = vec![ReconnectEffect::Log(log)];
        effects.extend(self.backoff_or_give_up(now, seed));
        effects
    }

    /// `dispatch` returned `OwnerLost`. Unlike [`Self::on_retryable_failure`],
    /// this *always* attempts recovery (never a fatal first failure) —
    /// matches `run_with_reconnect`'s original `OwnerLost` arm having no
    /// `lost_since.is_some()` gate of its own.
    fn on_owner_lost(&mut self, has_remote_command: bool, attempt_started: Millis, now: Millis, seed: u64) -> Vec<ReconnectEffect> {
        if has_remote_command {
            // Re-running a remote-command invocation from scratch after
            // losing the owner mid-execution isn't safe to automate.
            return self.finish(
                vec![ReconnectEffect::Log(ReconnectLog::RemoteCommandInterrupted)],
                ReconnectEffect::GiveUp { exit_code: crate::EXIT_MUX_OWNER_LOST },
            );
        }
        if was_stable(attempt_started, now) {
            self.attempt = 0;
            self.lost_since = None;
        }
        let mut effects = vec![ReconnectEffect::Log(ReconnectLog::OwnerLostReconnecting)];
        effects.extend(self.backoff_or_give_up(now, seed));
        effects
    }

    /// Shared bookkeeping for every place this reducer decides to retry:
    /// checks [`RECONNECT_BUDGET`] against `lost_since` (starting the clock
    /// on first use, same as `run_with_reconnect`'s original
    /// `lost_since.get_or_insert_with`), and either gives up or draws the
    /// next backoff delay (bumping `attempt`).
    fn backoff_or_give_up(&mut self, now: Millis, seed: u64) -> Vec<ReconnectEffect> {
        let lost_at = *self.lost_since.get_or_insert(now);
        if now.saturating_sub(lost_at) >= RECONNECT_BUDGET {
            return self.finish(vec![ReconnectEffect::Log(ReconnectLog::BudgetExhausted)], ReconnectEffect::GiveUp { exit_code: crate::EXIT_MUX_OWNER_LOST });
        }
        let delay = RECONNECT_BACKOFF.next_delay(self.attempt, seed);
        self.attempt += 1;
        let token = self.issue();
        self.awaiting = Awaiting::Backoff(token);
        vec![ReconnectEffect::Log(ReconnectLog::Reconnecting { delay }), ReconnectEffect::Backoff { token, delay }]
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn ms(d: Duration) -> u64 {
        u64::try_from(d.as_millis()).unwrap()
    }

    fn outstanding(effects: &[ReconnectEffect]) -> Option<&ReconnectEffect> {
        effects.last()
    }

    fn token_of(effect: &ReconnectEffect) -> Token {
        match effect {
            ReconnectEffect::Prepare { token } | ReconnectEffect::Dispatch { token } | ReconnectEffect::Backoff { token, .. } => *token,
            other => panic!("no token on {other:?}"),
        }
    }

    // -- the pre-existing backoff/threshold constant tests, moved here
    // unchanged from `native::mux::mod`'s test module -----------------------

    #[test]
    fn reconnect_backoff_doubles_each_attempt_until_capped() {
        let backoff = ReconnectBackoff { initial: Duration::from_millis(100), max: Duration::from_secs(5), jitter: 0.0 };
        assert_eq!(backoff.base_delay(0), Duration::from_millis(100));
        assert_eq!(backoff.base_delay(1), Duration::from_millis(200));
        assert_eq!(backoff.base_delay(2), Duration::from_millis(400));
        assert_eq!(backoff.base_delay(3), Duration::from_millis(800));
    }

    #[test]
    fn reconnect_backoff_converges_to_and_never_exceeds_max() {
        let backoff = ReconnectBackoff { initial: Duration::from_millis(100), max: Duration::from_secs(5), jitter: 0.0 };
        for attempt in 0..64 {
            assert!(backoff.base_delay(attempt) <= backoff.max, "attempt {attempt} exceeded max");
        }
        assert_eq!(backoff.base_delay(63), backoff.max, "backoff must saturate at max for a large attempt count, not overflow/panic");
    }

    #[test]
    fn reconnect_backoff_jitter_stays_within_the_configured_fraction_and_never_exceeds_max() {
        let backoff = ReconnectBackoff { initial: Duration::from_millis(200), max: Duration::from_secs(2), jitter: 0.5 };
        let base = backoff.base_delay(3);
        for seed in 0..200u64 {
            let jittered = backoff.next_delay(3, seed);
            assert!(jittered <= backoff.max, "jittered delay must never exceed max");
            let lower = base.mul_f64(0.5);
            let upper = base.min(backoff.max).mul_f64(1.5).min(backoff.max);
            assert!(
                jittered + Duration::from_millis(1) >= lower && jittered <= upper + Duration::from_millis(1),
                "jittered delay {jittered:?} outside [{lower:?}, {upper:?}]"
            );
        }
    }

    #[test]
    fn reconnect_backoff_zero_jitter_returns_exactly_the_base_delay() {
        let backoff = ReconnectBackoff { initial: Duration::from_millis(50), max: Duration::from_secs(1), jitter: 0.0 };
        for attempt in 0..5 {
            assert_eq!(backoff.next_delay(attempt, 99), backoff.base_delay(attempt));
        }
    }

    #[test]
    fn reconnect_stable_threshold_is_comfortably_above_the_max_backoff() {
        assert!(
            RECONNECT_STABLE_THRESHOLD > RECONNECT_BACKOFF.max * 2,
            "RECONNECT_STABLE_THRESHOLD must stay well above RECONNECT_BACKOFF.max"
        );
    }

    #[test]
    fn reconnect_backoff_never_panics_on_a_huge_attempt_count() {
        let backoff = ReconnectBackoff { initial: Duration::from_millis(1), max: Duration::from_secs(1), jitter: 0.0 };
        assert_eq!(backoff.next_delay(u32::MAX, 1), backoff.max);
    }

    // -- ReconnectFsm behavior ----------------------------------------------

    #[test]
    fn the_very_first_prepare_failure_is_fatal_not_retried() {
        let mut fsm = ReconnectFsm::new();
        let t = token_of(&fsm.start()[0]);
        let effects = fsm.apply(ReconnectEvent::PrepareFailed { token: t, now: Millis(0), seed: 0 });
        assert_eq!(effects, vec![ReconnectEffect::PropagateFatalError], "a brand-new loop must fail fast on its first prepare failure");
    }

    #[test]
    fn the_very_first_dispatch_failure_is_fatal_not_retried() {
        let mut fsm = ReconnectFsm::new();
        let t = token_of(&fsm.start()[0]);
        fsm.begin_dispatch();
        let effects = fsm.apply(ReconnectEvent::DispatchFailed { token: Token(1), now: Millis(0), seed: 0 });
        assert_eq!(effects, vec![ReconnectEffect::PropagateFatalError]);
        let _ = t;
    }

    #[test]
    fn owner_lost_always_attempts_recovery_even_on_the_very_first_attempt() {
        let mut fsm = ReconnectFsm::new();
        fsm.start();
        let effects = fsm.begin_dispatch();
        let token = token_of(&effects[0]);
        let effects = fsm.apply(ReconnectEvent::DispatchOwnerLost { token, has_remote_command: false, attempt_started: Millis(0), now: Millis(0), seed: 0 });
        assert_eq!(effects[0], ReconnectEffect::Log(ReconnectLog::OwnerLostReconnecting));
        assert!(matches!(effects.last(), Some(ReconnectEffect::Backoff { .. })), "{effects:?}");
    }

    #[test]
    fn owner_lost_with_a_remote_command_gives_up_immediately_without_backoff() {
        let mut fsm = ReconnectFsm::new();
        fsm.start();
        let effects = fsm.begin_dispatch();
        let token = token_of(&effects[0]);
        let effects = fsm.apply(ReconnectEvent::DispatchOwnerLost { token, has_remote_command: true, attempt_started: Millis(0), now: Millis(0), seed: 0 });
        assert_eq!(
            effects,
            vec![ReconnectEffect::Log(ReconnectLog::RemoteCommandInterrupted), ReconnectEffect::GiveUp { exit_code: crate::EXIT_MUX_OWNER_LOST }]
        );
    }

    #[test]
    fn a_retryable_failure_after_a_prior_owner_loss_backs_off_instead_of_propagating() {
        let mut fsm = ReconnectFsm::new();
        fsm.start();
        let effects = fsm.begin_dispatch();
        let token = token_of(&effects[0]);
        // First OwnerLost establishes `lost_since`.
        let effects = fsm.apply(ReconnectEvent::DispatchOwnerLost { token, has_remote_command: false, attempt_started: Millis(0), now: Millis(0), seed: 0 });
        let token = token_of(effects.last().unwrap());
        let effects = fsm.apply(ReconnectEvent::BackoffElapsed { token });
        let prepare_token = token_of(&effects[0]);
        // A subsequent prepare failure must now retry, not propagate.
        let effects = fsm.apply(ReconnectEvent::PrepareFailed { token: prepare_token, now: Millis(100), seed: 0 });
        assert_eq!(effects[0], ReconnectEffect::Log(ReconnectLog::PrepareFailedRetrying));
        assert!(matches!(effects.last(), Some(ReconnectEffect::Backoff { .. })), "{effects:?}");
    }

    #[test]
    fn a_stable_owner_lost_resets_attempt_and_budget() {
        let mut fsm = ReconnectFsm::new();
        fsm.start();
        let effects = fsm.begin_dispatch();
        let token = token_of(&effects[0]);
        let stable_end = ms(RECONNECT_STABLE_THRESHOLD) + 1;
        let effects = fsm.apply(ReconnectEvent::DispatchOwnerLost {
            token,
            has_remote_command: false,
            attempt_started: Millis(0),
            now: Millis(stable_end),
            seed: 0,
        });
        assert_eq!(fsm.attempt, 1, "backoff_or_give_up still bumps attempt for the delay it just drew");
        assert!(matches!(effects.last(), Some(ReconnectEffect::Backoff { .. })));
        // Budget is fresh (started at `stable_end`, not 0) — far from exhausted.
        assert_eq!(fsm.lost_since, Some(Millis(stable_end)));
    }

    #[test]
    fn budget_exhaustion_gives_up_after_logging() {
        let mut fsm = ReconnectFsm::new();
        fsm.start();
        let effects = fsm.begin_dispatch();
        let token = token_of(&effects[0]);
        let effects = fsm.apply(ReconnectEvent::DispatchOwnerLost { token, has_remote_command: false, attempt_started: Millis(0), now: Millis(0), seed: 0 });
        let late = ms(RECONNECT_BUDGET) + 1;
        let token = token_of(effects.last().unwrap());
        // Same storm (never stable in between), far past the budget.
        let effects = fsm.apply(ReconnectEvent::BackoffElapsed { token });
        let prepare_token = token_of(&effects[0]);
        let effects = fsm.apply(ReconnectEvent::PrepareFailed { token: prepare_token, now: Millis(late), seed: 0 });
        assert_eq!(effects, vec![ReconnectEffect::Log(ReconnectLog::BudgetExhausted), ReconnectEffect::GiveUp { exit_code: crate::EXIT_MUX_OWNER_LOST }]);
    }

    #[test]
    fn ctrl_c_during_backoff_gives_up_with_the_cancel_exit_code() {
        let mut fsm = ReconnectFsm::new();
        fsm.start();
        let effects = fsm.begin_dispatch();
        let token = token_of(&effects[0]);
        let effects = fsm.apply(ReconnectEvent::DispatchOwnerLost { token, has_remote_command: false, attempt_started: Millis(0), now: Millis(0), seed: 0 });
        let token = token_of(effects.last().unwrap());
        let effects = fsm.apply(ReconnectEvent::BackoffAborted { token });
        assert_eq!(effects, vec![ReconnectEffect::Log(ReconnectLog::Canceled), ReconnectEffect::GiveUp { exit_code: crate::EXIT_USER_CANCELED }]);
    }

    // -- proptests (§3-2, §3-7) ----------------------------------------------

    #[derive(Debug, Clone)]
    struct Step {
        stale: bool,
        stale_offset: u64,
        now: u64,
        has_remote_command: bool,
        stable: bool,
        seed: u64,
        kind: u8,
    }

    fn step(max_now: u64) -> impl Strategy<Value = Step> {
        (any::<bool>(), 1u64..1_000, 0..max_now, any::<bool>(), any::<bool>(), any::<u64>(), 0u8..3).prop_map(
            |(stale, stale_offset, now, has_remote_command, stable, seed, kind)| Step { stale, stale_offset, now, has_remote_command, stable, seed, kind },
        )
    }

    /// Answers the outstanding effect per `s`.
    fn answer(outstanding: &ReconnectEffect, s: &Step) -> Option<ReconnectEvent> {
        match outstanding {
            ReconnectEffect::Prepare { token } => Some(ReconnectEvent::PrepareFailed { token: *token, now: Millis(s.now), seed: s.seed }),
            ReconnectEffect::Dispatch { token } => {
                let attempt_started = if s.stable { Millis(s.now.saturating_sub(ms(RECONNECT_STABLE_THRESHOLD) + 1)) } else { Millis(s.now) };
                if s.kind == 0 {
                    Some(ReconnectEvent::DispatchFailed { token: *token, now: Millis(s.now), seed: s.seed })
                } else {
                    Some(ReconnectEvent::DispatchOwnerLost {
                        token: *token,
                        has_remote_command: s.has_remote_command,
                        attempt_started,
                        now: Millis(s.now),
                        seed: s.seed,
                    })
                }
            }
            ReconnectEffect::Backoff { token, .. } => Some(ReconnectEvent::BackoffElapsed { token: *token }),
            ReconnectEffect::GiveUp { .. } | ReconnectEffect::PropagateFatalError => None,
            other => panic!("non-terminal, non-awaiting effect last: {other:?}"),
        }
    }

    fn stale_event(outstanding: &ReconnectEffect, s: &Step) -> ReconnectEvent {
        let base = match outstanding {
            ReconnectEffect::Prepare { token } | ReconnectEffect::Dispatch { token } | ReconnectEffect::Backoff { token, .. } => token.0,
            _ => 0,
        };
        let token = Token(if s.stale_offset % 2 == 0 { base.wrapping_add(s.stale_offset) } else { base.wrapping_sub(s.stale_offset) });
        let probe = match s.kind {
            0 => ReconnectEffect::Prepare { token },
            1 => ReconnectEffect::Dispatch { token },
            _ => ReconnectEffect::Backoff { token, delay: Duration::ZERO },
        };
        answer(&probe, s).expect("probe is an awaiting effect")
    }

    fn assert_well_formed(effects: &[ReconnectEffect]) {
        assert!(!effects.is_empty(), "a current-token event must always produce a next step");
        let is_next = |e: &ReconnectEffect| {
            matches!(
                e,
                ReconnectEffect::Prepare { .. }
                    | ReconnectEffect::Dispatch { .. }
                    | ReconnectEffect::Backoff { .. }
                    | ReconnectEffect::GiveUp { .. }
                    | ReconnectEffect::PropagateFatalError
            )
        };
        assert_eq!(effects.iter().filter(|e| is_next(e)).count(), 1, "{effects:?}");
        assert!(is_next(effects.last().unwrap()), "{effects:?}");
    }

    proptest! {
        /// §2.2 stale-guard: an answer with a non-outstanding token (any
        /// kind) changes nothing and emits nothing — interleaved anywhere in
        /// an arbitrary run, with arbitrary (non-monotonic) `now`.
        #[test]
        fn stale_tokens_never_change_state_or_emit(steps in proptest::collection::vec(step(u64::MAX / 2), 1..60)) {
            let mut fsm = ReconnectFsm::new();
            let mut effects = fsm.start();
            for s in &steps {
                let Some(last) = outstanding(&effects).cloned() else { break };
                if s.stale {
                    let before = fsm.clone();
                    let out = fsm.apply(stale_event(&last, s));
                    prop_assert!(out.is_empty(), "stale event emitted {out:?}");
                    prop_assert_eq!(&fsm, &before);
                    continue;
                }
                let Some(ev) = answer(&last, s) else {
                    let before = fsm.clone();
                    prop_assert!(fsm.apply(stale_event(&last, s)).is_empty());
                    prop_assert_eq!(&fsm, &before);
                    break;
                };
                effects = fsm.apply(ev);
                assert_well_formed(&effects);
            }
        }

        /// Never-give-up-within-budget, mirroring
        /// `connect_recovery_fsm`'s equivalent proptest: a run of pure
        /// `OwnerLost` failures (no remote command, never "stable") never
        /// ends the recovery while inside [`RECONNECT_BUDGET`] — even with
        /// `now` jumping backwards and forwards.
        #[test]
        fn recoverable_failures_never_give_up_within_the_budget(steps in proptest::collection::vec(step(ms(RECONNECT_BUDGET) - 1), 1..150)) {
            let mut fsm = ReconnectFsm::new();
            let mut effects = fsm.start();
            for s in &steps {
                let s = Step { stale: false, has_remote_command: false, stable: false, kind: 1, ..s.clone() };
                let Some(ev) = answer(outstanding(&effects).unwrap(), &s) else { break };
                effects = fsm.apply(ev);
                assert_well_formed(&effects);
                prop_assert!(
                    !matches!(effects.last(), Some(ReconnectEffect::GiveUp { .. } | ReconnectEffect::PropagateFatalError)),
                    "gave up on a recoverable failure within budget: {effects:?}"
                );
            }
        }
    }
}
