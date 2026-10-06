//! The one "always-connects" connect-failure recovery reducer shared by
//! `isekai-ssh`'s Unix (`wrapper.rs::run_ssh_with_connect_failure_recovery`)
//! and Windows-native (`native::connect::run_native_connect_with_recovery`)
//! paths (ADR_FUNCTIONAL_CORE_EFFECTS.md §6 Step 6+7).
//!
//! Before Step 6+7 each path had its own copy of the loop body (attempt →
//! claim outcome → decide → maybe redeploy → backoff → retry), and real bugs
//! had to be fixed twice: a266f1f3 (redeploy+retry ran only once, the
//! user-observed "it looks like it crashed" exit) and 857f6ae6 D-4 (missing
//! jitter). Now the *decisions* live here, once, as a pure reducer
//! (§2.1: `apply(&mut self, Event) -> Vec<Effect>`), and the single shell
//! (`connect_recovery_driver::drive_connect_recovery`) interprets the
//! effects against a per-platform `ConnectRecoveryOps` port.
//!
//! - Time arrives only as `now`/`started: Millis` stamped by the shell; all
//!   differences use `saturating_sub` (§2.2).
//! - Jitter arrives as an explicit `seed: u64` on the events that can draw
//!   one (each `apply` draws at most once — either the redeploy gate's
//!   deadline or a backoff delay, never both).
//! - Every effect the shell must answer (`Attempt`/`Redeploy`/`Backoff`)
//!   carries a [`Token`]; an answering event whose token isn't the
//!   outstanding one is ignored without changing state or emitting effects
//!   (§2.2 stale-guard).
//! - Values are unchanged from the two loops it replaces:
//!   [`MAX_LIGHTWEIGHT_RETRIES`] = 5, and the backoff/budget/redeploy-gate
//!   policy of `reconnect_backoff`. `native::mux::mod`'s separate reconnect
//!   loop (and its deliberately different 60s `RECONNECT_STABLE_THRESHOLD`,
//!   ADR D4) is *not* part of this reducer.
// 純粋モジュール(`pure_modules.toml`登録、ADR_FUNCTIONAL_CORE_EFFECTS.md §2.3)。
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::time::Duration;

use isekai_pipe_core::ConnectOutcomeClass;
use isekai_protocol::Millis;

use crate::reconnect_backoff::{RecoveryBudget, RedeployGate};

/// STUN P2P's lightweight reconnect claims a fresh `AttachArbiter` session
/// slot on every attempt; capped well short of the server's
/// `--max-sessions` default (16) so a long run of lightweight retries can
/// never evict another tab's genuinely parked session (ADR round 1, S2).
/// Once exceeded, the loop falls back to one `RebootstrapAndRetry`-style
/// attempt (gated on `should_bootstrap`, unlike the lightweight path
/// itself) rather than lightweight-retrying forever. Formerly duplicated as
/// `wrapper.rs::MAX_LIGHTWEIGHT_RETRIES` and
/// `native::connect::MAX_LIGHTWEIGHT_RETRIES` (both 5).
pub(crate) const MAX_LIGHTWEIGHT_RETRIES: u32 = 5;

/// The four ways a failed `ssh` attempt can be handled, given whether
/// `isekai-pipe connect` left behind a `ConnectOutcome` side-channel signal
/// and whether auto-bootstrap is currently allowed. Pure decision, no I/O.
/// Moved here from `wrapper.rs` in Step 6+7 (re-exported there unchanged) so
/// the reducer can use it without importing an impure module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectFailureRecoveryAction {
    /// No connect-failure signal for this attempt — return the exit code
    /// as-is (e.g. the remote shell command itself exited non-zero; that
    /// never touches `isekai-pipe connect`'s own error path at all).
    NoRecoverableSignal,
    /// A signal was found, but auto-bootstrap is disabled
    /// (`--isekai-no-bootstrap` / `#@isekai bootstrap-policy never`) —
    /// return the exit code as-is, with guidance to run `isekai-ssh init`.
    AutoBootstrapDisabled,
    /// A signal was found and auto-bootstrap is allowed — attempt a silent
    /// re-bootstrap (gated by `RedeployGate`) and retry.
    RebootstrapAndRetry,
    /// The connect-time handshake had already succeeded before this attempt
    /// failed (`ConnectOutcomeClass::MidSessionDisconnect`, Epic R PR2) —
    /// retry by re-dialing the already-deployed helper, with no re-deploy
    /// step. Unlike `RebootstrapAndRetry`, this is attempted regardless of
    /// `should_bootstrap`: it never touches SSH-based re-deployment, so
    /// `--isekai-no-bootstrap`'s contract ("don't silently re-deploy over
    /// SSH") isn't violated by trying it (ADR round 1, Q2 — both opus
    /// reviewers agreed on this reading).
    RetryConnectLightweight,
}

/// Epic R PR2 changed this from a plain `bool` to
/// `Option<&ConnectOutcomeClass>` so `MidSessionDisconnect` can route to its
/// own `RetryConnectLightweight` action instead of `RebootstrapAndRetry`.
/// **`Unknown` must behave like `Unreachable`, not like `None`** — round 1
/// review (R1-B1) caught an earlier draft doing exactly that, which would
/// have been a regression against `.claude/rules/always-connects.md`:
/// `Unknown` existing at all already means "some outcome was recorded".
#[deny(clippy::wildcard_enum_match_arm)]
pub(crate) fn decide_connect_failure_recovery(outcome_class: Option<&ConnectOutcomeClass>, should_bootstrap: bool) -> ConnectFailureRecoveryAction {
    match outcome_class {
        None => ConnectFailureRecoveryAction::NoRecoverableSignal,
        Some(ConnectOutcomeClass::MidSessionDisconnect) => ConnectFailureRecoveryAction::RetryConnectLightweight,
        // Explicit per-variant arms (no `Some(_)` wildcard) so adding a
        // `ConnectOutcomeClass` forces a conscious recovery decision here
        // (always-connects, Step 12). `Unknown` must behave like `Unreachable`.
        Some(ConnectOutcomeClass::StaleTrust | ConnectOutcomeClass::Unreachable | ConnectOutcomeClass::Unknown) if !should_bootstrap => {
            ConnectFailureRecoveryAction::AutoBootstrapDisabled
        }
        Some(ConnectOutcomeClass::StaleTrust | ConnectOutcomeClass::Unreachable | ConnectOutcomeClass::Unknown) => {
            ConnectFailureRecoveryAction::RebootstrapAndRetry
        }
    }
}

/// The non-idempotent remote command guard (Epic R PR2 B5, opus review round
/// 2 SHOULD-FIX R2-4): `isekai-ssh host -- ./deploy.sh` interrupted
/// mid-run must not be silently re-run from scratch. Returns `true` when an
/// automatic retry must be refused for this class.
///
/// - `MidSessionDisconnect`: SSH bytes were flowing, so the remote command
///   may already have run.
/// - `Unknown`: this build doesn't recognize the class a *newer*
///   `isekai-pipe` wrote (independently versioned binaries), so it could be
///   a future mid-session class — treat it like one.
/// - `StaleTrust`/`Unreachable`: pre-handshake failures — no SSH bytes ever
///   flowed, so the remote command never started and retrying is safe.
///
/// Formerly two inline conditions duplicated in each loop (`wrapper.rs:727`/
/// `:789`, `native/connect.rs:454`/`:503`); this is exactly equivalent: the
/// lightweight arm only ever sees `MidSessionDisconnect`, and the rebootstrap
/// arm only `StaleTrust`/`Unreachable`/`Unknown`.
#[deny(clippy::wildcard_enum_match_arm)]
pub(crate) fn remote_command_forbids_retry(class: &ConnectOutcomeClass, has_remote_command: bool) -> bool {
    match class {
        ConnectOutcomeClass::MidSessionDisconnect | ConnectOutcomeClass::Unknown => has_remote_command,
        ConnectOutcomeClass::StaleTrust | ConnectOutcomeClass::Unreachable => false,
    }
}

/// Generation token of the one effect the shell currently owes an answer
/// for (§2.2 stale-guard). Issued by the reducer, strictly increasing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Token(pub(crate) u64);

/// How a [`RecoveryEffect::Redeploy`] ended. Which errors count as
/// retryable is the shell's call (`BootstrapFailure::may_retry`, applied
/// per platform exactly as before Step 6+7); the reducer only decides what
/// to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RedeployResult {
    /// Redeployed and rebuilt the intent from the refreshed trust material.
    Succeeded,
    /// Failed, but with evidence it was transient — fall through to a
    /// plain lightweight reconnect wait, as a closed gate would.
    RetryableFailure,
    /// Failed for good — the shell returns that error.
    Fatal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryEvent {
    /// The attempt issued by `RecoveryEffect::Attempt { token, .. }` failed
    /// (an attempt that finishes never reaches the reducer — the shell
    /// returns its exit code directly).
    AttemptFailed {
        token: Token,
        /// Stamped right before the attempt started.
        started: Millis,
        /// Stamped after the outcome file was claimed.
        now: Millis,
        /// The claimed `ConnectOutcome`'s class (`None`: no signal, or the
        /// claim itself failed — `wrapper::resolve_claimed_outcome`).
        class: Option<ConnectOutcomeClass>,
        should_bootstrap: bool,
        has_remote_command: bool,
        seed: u64,
    },
    RedeployFinished { token: Token, now: Millis, seed: u64, result: RedeployResult },
    BackoffElapsed { token: Token },
}

/// Log lines whose wording is fixed by the shell (some need the claimed
/// `ConnectOutcome`'s detail/profile, which the reducer deliberately never
/// sees).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryLog {
    /// `wrapper::log_auto_bootstrap_disabled`.
    AutoBootstrapDisabled,
    /// `wrapper::log_rebootstrap_and_retry_decision` (platform retry note).
    RebootstrapDecision,
    /// "connection lost while running a remote command; not auto-retrying".
    RemoteCommandNotRetried,
    /// "gave up on N lightweight reconnect attempts and auto-bootstrap is disabled".
    LightweightExhaustedNoBootstrap,
    /// "gave up on N lightweight reconnect attempts; trying a full re-deploy instead".
    LightweightExhaustedRedeploying,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryEffect {
    /// Run one connect attempt against the shell's current intent; answer
    /// with `AttemptFailed { token, .. }` (or return on success). `silent`
    /// is `true` only for the attempt right after a successful redeploy
    /// (`native::connect`'s host-key TOFU must not prompt then; the Unix
    /// path ignores it).
    Attempt { token: Token, silent: bool },
    /// Silent re-deploy + intent rebuild; answer with `RedeployFinished`.
    Redeploy { token: Token },
    /// Sleep `delay`; answer with `BackoffElapsed { token }`.
    Backoff { token: Token, delay: Duration },
    /// Rebuild the intent from already-trusted material (a retried attempt
    /// always gets a fresh `intent_id`, round 2 review SHOULD-FIX R2-3). A
    /// failure here aborts the whole recovery with that error, as before.
    RebuildIntent,
    /// "connection lost, reconnecting... (attempt {attempt})", printed
    /// *before* the backoff wait.
    AnnounceReconnect { attempt: u32 },
    Log(RecoveryLog),
    /// Stop and return the last attempt's own failure (Unix: its exit code;
    /// native: its error).
    GiveUp,
    /// Stop and return the fatal redeploy error.
    FailWithRedeployError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedeployOrigin {
    /// From `RebootstrapAndRetry`.
    Rebootstrap,
    /// From `RetryConnectLightweight` after `MAX_LIGHTWEIGHT_RETRIES`; a
    /// successful redeploy from here also resets `lightweight_retries`.
    LightweightExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Awaiting {
    NotStarted,
    Attempt(Token),
    Redeploy(Token, RedeployOrigin),
    Backoff(Token),
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectRecoveryFsm {
    budget: RecoveryBudget,
    gate: RedeployGate,
    next_token: u64,
    awaiting: Awaiting,
}

impl ConnectRecoveryFsm {
    pub(crate) fn new() -> Self {
        Self { budget: RecoveryBudget::default(), gate: RedeployGate::new(), next_token: 0, awaiting: Awaiting::NotStarted }
    }

    /// The first effect: one attempt against the caller's initial intent.
    /// A second call returns nothing.
    pub(crate) fn start(&mut self) -> Vec<RecoveryEffect> {
        if self.awaiting != Awaiting::NotStarted {
            return Vec::new();
        }
        vec![self.attempt(false)]
    }

    pub(crate) fn apply(&mut self, event: RecoveryEvent) -> Vec<RecoveryEffect> {
        match (self.awaiting, event) {
            (
                Awaiting::Attempt(current),
                RecoveryEvent::AttemptFailed { token, started, now, class, should_bootstrap, has_remote_command, seed },
            ) if token == current => self.on_attempt_failed(started, now, class, should_bootstrap, has_remote_command, seed),
            (Awaiting::Redeploy(current, origin), RecoveryEvent::RedeployFinished { token, now, seed, result }) if token == current => {
                self.on_redeploy_finished(origin, now, seed, result)
            }
            (Awaiting::Backoff(current), RecoveryEvent::BackoffElapsed { token }) if token == current => {
                vec![RecoveryEffect::RebuildIntent, self.attempt(false)]
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

    fn attempt(&mut self, silent: bool) -> RecoveryEffect {
        let token = self.issue();
        self.awaiting = Awaiting::Attempt(token);
        RecoveryEffect::Attempt { token, silent }
    }

    fn redeploy(&mut self, origin: RedeployOrigin) -> RecoveryEffect {
        let token = self.issue();
        self.awaiting = Awaiting::Redeploy(token, origin);
        RecoveryEffect::Redeploy { token }
    }

    fn finish(&mut self, mut effects: Vec<RecoveryEffect>, last: RecoveryEffect) -> Vec<RecoveryEffect> {
        self.awaiting = Awaiting::Done;
        effects.push(last);
        effects
    }

    /// Announce, then either give up (budget exhausted) or arm a backoff.
    fn backoff(&mut self, now: Millis, seed: u64) -> Vec<RecoveryEffect> {
        // `attempt + 1` is the attempt number `next_backoff` is about to
        // record — printed *before* the wait (round 2 review finding).
        let announce = RecoveryEffect::AnnounceReconnect { attempt: self.budget.attempt + 1 };
        match self.budget.next_backoff(now, seed) {
            None => self.finish(vec![announce], RecoveryEffect::GiveUp),
            Some(delay) => {
                let token = self.issue();
                self.awaiting = Awaiting::Backoff(token);
                vec![announce, RecoveryEffect::Backoff { token, delay }]
            }
        }
    }

    fn on_attempt_failed(
        &mut self,
        started: Millis,
        now: Millis,
        class: Option<ConnectOutcomeClass>,
        should_bootstrap: bool,
        has_remote_command: bool,
        seed: u64,
    ) -> Vec<RecoveryEffect> {
        // Any failure this long after the previous attempt started counts as
        // a fresh, unrelated event — same heuristic for the redeploy gate and
        // the budget, applied before the decision so it covers both arms
        // (opus review round 2, SHOULD-FIX R2-2).
        self.gate.reset_if_stable(started, now);
        self.budget.reset_if_stable(started, now);

        let action = decide_connect_failure_recovery(class.as_ref(), should_bootstrap);
        match (action, class) {
            (ConnectFailureRecoveryAction::NoRecoverableSignal, _) | (_, None) => self.finish(Vec::new(), RecoveryEffect::GiveUp),
            (ConnectFailureRecoveryAction::AutoBootstrapDisabled, Some(_)) => {
                self.finish(vec![RecoveryEffect::Log(RecoveryLog::AutoBootstrapDisabled)], RecoveryEffect::GiveUp)
            }
            (ConnectFailureRecoveryAction::RebootstrapAndRetry, Some(class)) => {
                if remote_command_forbids_retry(&class, has_remote_command) {
                    return self.finish(vec![RecoveryEffect::Log(RecoveryLog::RemoteCommandNotRetried)], RecoveryEffect::GiveUp);
                }
                // `StaleTrust`/`Unreachable`/`Unknown` have no lightweight
                // tier of their own, so the gate (not a one-shot) is what
                // keeps this from redeploying on every single retry.
                if self.gate.try_consume(now, seed) {
                    return vec![RecoveryEffect::Log(RecoveryLog::RebootstrapDecision), self.redeploy(RedeployOrigin::Rebootstrap)];
                }
                // Gate closed: plain reconnect against the existing deployment.
                self.backoff(now, seed)
            }
            (ConnectFailureRecoveryAction::RetryConnectLightweight, Some(class)) => {
                if remote_command_forbids_retry(&class, has_remote_command) {
                    return self.finish(vec![RecoveryEffect::Log(RecoveryLog::RemoteCommandNotRetried)], RecoveryEffect::GiveUp);
                }
                self.budget.lightweight_retries += 1;
                if self.budget.lightweight_retries > MAX_LIGHTWEIGHT_RETRIES {
                    if !should_bootstrap {
                        return self.finish(vec![RecoveryEffect::Log(RecoveryLog::LightweightExhaustedNoBootstrap)], RecoveryEffect::GiveUp);
                    }
                    // Same gate as the rebootstrap arm — one decision
                    // authority for "is a redeploy allowed right now". The
                    // log line only fires when a redeploy actually happens
                    // (round 2 `/code-review`).
                    if self.gate.try_consume(now, seed) {
                        return vec![
                            RecoveryEffect::Log(RecoveryLog::LightweightExhaustedRedeploying),
                            self.redeploy(RedeployOrigin::LightweightExhausted),
                        ];
                    }
                }
                self.backoff(now, seed)
            }
        }
    }

    fn on_redeploy_finished(&mut self, origin: RedeployOrigin, now: Millis, seed: u64, result: RedeployResult) -> Vec<RecoveryEffect> {
        match result {
            RedeployResult::Succeeded => {
                match origin {
                    RedeployOrigin::LightweightExhausted => self.budget.lightweight_retries = 0,
                    RedeployOrigin::Rebootstrap => {}
                }
                vec![self.attempt(true)]
            }
            // Retryable redeploy failure: the same lightweight wait a closed
            // gate would take, not a second, separate backoff.
            RedeployResult::RetryableFailure => self.backoff(now, seed),
            RedeployResult::Fatal => self.finish(Vec::new(), RecoveryEffect::FailWithRedeployError),
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    use super::*;
    use crate::reconnect_backoff::{RECONNECT_BUDGET, RECONNECT_STABLE_THRESHOLD};
    use proptest::prelude::*;

    use isekai_pipe_core::ConnectOutcomeClass as C;

    fn ms(d: Duration) -> u64 {
        u64::try_from(d.as_millis()).unwrap()
    }

    fn all_classes() -> Vec<ConnectOutcomeClass> {
        // Explicit match: adding a variant fails compilation here.
        match C::Unreachable {
            C::StaleTrust | C::Unreachable | C::MidSessionDisconnect | C::Unknown => {}
        }
        vec![C::StaleTrust, C::Unreachable, C::MidSessionDisconnect, C::Unknown]
    }

    fn outstanding(effects: &[RecoveryEffect]) -> Option<&RecoveryEffect> {
        effects.last()
    }

    fn token_of(effect: &RecoveryEffect) -> Token {
        match effect {
            RecoveryEffect::Attempt { token, .. } | RecoveryEffect::Redeploy { token } | RecoveryEffect::Backoff { token, .. } => *token,
            other => panic!("no token on {other:?}"),
        }
    }

    fn failed(token: Token, now: u64, class: Option<ConnectOutcomeClass>, should_bootstrap: bool, remote: bool) -> RecoveryEvent {
        RecoveryEvent::AttemptFailed {
            token,
            started: Millis(now),
            now: Millis(now),
            class,
            should_bootstrap,
            has_remote_command: remote,
            seed: 0,
        }
    }

    // ---- the guard the Step 12 table could not express ------------------

    /// Step 12's exhaustive table (`wrapper.rs`) noted that "`Unknown` +
    /// remote command is refused *after* the decision, by a guard duplicated
    /// in both loops" and could not be expressed there. It is now one
    /// predicate; this pins its full table.
    #[test]
    fn remote_command_guard_table() {
        for class in all_classes() {
            for remote in [false, true] {
                let expected = match class {
                    C::MidSessionDisconnect | C::Unknown => remote,
                    C::StaleTrust | C::Unreachable => false,
                };
                assert_eq!(remote_command_forbids_retry(&class, remote), expected, "{class:?}, remote {remote}");
            }
        }
    }

    /// Reducer-level: `Unknown` + remote command gives up with the
    /// "not auto-retrying" log, never redeploys — while `Unknown` without a
    /// remote command, and `Unreachable` with one, still recover.
    #[test]
    fn unknown_class_with_a_remote_command_gives_up_instead_of_redeploying() {
        let mut fsm = ConnectRecoveryFsm::new();
        let t = token_of(&fsm.start()[0]);
        let effects = fsm.apply(failed(t, 0, Some(C::Unknown), true, true));
        assert_eq!(effects, vec![RecoveryEffect::Log(RecoveryLog::RemoteCommandNotRetried), RecoveryEffect::GiveUp]);

        let mut fsm = ConnectRecoveryFsm::new();
        let t = token_of(&fsm.start()[0]);
        let effects = fsm.apply(failed(t, 0, Some(C::Unknown), true, false));
        assert!(matches!(effects.last(), Some(RecoveryEffect::Redeploy { .. })), "{effects:?}");

        let mut fsm = ConnectRecoveryFsm::new();
        let t = token_of(&fsm.start()[0]);
        let effects = fsm.apply(failed(t, 0, Some(C::Unreachable), true, true));
        assert!(matches!(effects.last(), Some(RecoveryEffect::Redeploy { .. })), "pre-handshake classes are safe to retry: {effects:?}");
    }

    #[test]
    fn first_rebootstrap_is_immediate_and_retry_after_it_is_silent() {
        let mut fsm = ConnectRecoveryFsm::new();
        assert!(matches!(fsm.start()[..], [RecoveryEffect::Attempt { silent: false, .. }]));
        assert!(fsm.start().is_empty(), "start is one-shot");
        let effects = fsm.apply(failed(Token(0), 0, Some(C::StaleTrust), true, false));
        assert_eq!(effects[0], RecoveryEffect::Log(RecoveryLog::RebootstrapDecision));
        let t = token_of(&effects[1]);
        let effects = fsm.apply(RecoveryEvent::RedeployFinished { token: t, now: Millis(10), seed: 0, result: RedeployResult::Succeeded });
        assert!(matches!(effects[..], [RecoveryEffect::Attempt { silent: true, .. }]), "{effects:?}");
    }

    #[test]
    fn a_closed_gate_falls_through_to_announce_then_backoff_then_rebuild_then_attempt() {
        let mut fsm = ConnectRecoveryFsm::new();
        let t = token_of(&fsm.start()[0]);
        let t = token_of(&fsm.apply(failed(t, 0, Some(C::Unreachable), true, false))[1]);
        let effects = fsm.apply(RecoveryEvent::RedeployFinished { token: t, now: Millis(0), seed: 0, result: RedeployResult::Succeeded });
        let effects = fsm.apply(failed(token_of(&effects[0]), 1_000, Some(C::Unreachable), true, false));
        assert_eq!(effects[0], RecoveryEffect::AnnounceReconnect { attempt: 1 });
        let RecoveryEffect::Backoff { token, delay } = effects[1] else { panic!("{effects:?}") };
        assert!(delay <= Duration::from_millis(625), "attempt 0 backoff is 500ms ±25%: {delay:?}");
        let effects = fsm.apply(RecoveryEvent::BackoffElapsed { token });
        assert!(matches!(effects[..], [RecoveryEffect::RebuildIntent, RecoveryEffect::Attempt { silent: false, .. }]), "{effects:?}");
    }

    #[test]
    fn lightweight_retries_fall_back_to_a_redeploy_only_after_the_cap() {
        let mut fsm = ConnectRecoveryFsm::new();
        let mut t = token_of(&fsm.start()[0]);
        for n in 1..=MAX_LIGHTWEIGHT_RETRIES {
            let effects = fsm.apply(failed(t, 0, Some(C::MidSessionDisconnect), true, false));
            assert_eq!(effects[0], RecoveryEffect::AnnounceReconnect { attempt: n }, "retry {n}");
            let effects = fsm.apply(RecoveryEvent::BackoffElapsed { token: token_of(&effects[1]) });
            t = token_of(&effects[1]);
        }
        let effects = fsm.apply(failed(t, 0, Some(C::MidSessionDisconnect), true, false));
        assert_eq!(effects[0], RecoveryEffect::Log(RecoveryLog::LightweightExhaustedRedeploying));
        let effects = fsm.apply(RecoveryEvent::RedeployFinished { token: token_of(&effects[1]), now: Millis(0), seed: 0, result: RedeployResult::Succeeded });
        assert_eq!(fsm.budget.lightweight_retries, 0, "a redeploy from the lightweight cap resets the cap");
        assert!(matches!(effects[..], [RecoveryEffect::Attempt { silent: true, .. }]));
    }

    #[test]
    fn lightweight_cap_without_bootstrap_gives_up() {
        let mut fsm = ConnectRecoveryFsm::new();
        let mut t = token_of(&fsm.start()[0]);
        for _ in 0..MAX_LIGHTWEIGHT_RETRIES {
            let effects = fsm.apply(failed(t, 0, Some(C::MidSessionDisconnect), false, false));
            t = token_of(&fsm.apply(RecoveryEvent::BackoffElapsed { token: token_of(&effects[1]) })[1]);
        }
        let effects = fsm.apply(failed(t, 0, Some(C::MidSessionDisconnect), false, false));
        assert_eq!(effects, vec![RecoveryEffect::Log(RecoveryLog::LightweightExhaustedNoBootstrap), RecoveryEffect::GiveUp]);
    }

    #[test]
    fn budget_exhaustion_gives_up_after_announcing() {
        let mut fsm = ConnectRecoveryFsm::new();
        let t = token_of(&fsm.start()[0]);
        let effects = fsm.apply(failed(t, 0, Some(C::MidSessionDisconnect), true, false));
        let t = token_of(&fsm.apply(RecoveryEvent::BackoffElapsed { token: token_of(&effects[1]) })[1]);
        // Same storm (started == now, never stable), 24h later.
        let late = ms(RECONNECT_BUDGET) + 1;
        let effects = fsm.apply(failed(t, late, Some(C::MidSessionDisconnect), true, false));
        assert_eq!(effects, vec![RecoveryEffect::AnnounceReconnect { attempt: 2 }, RecoveryEffect::GiveUp]);
    }

    #[test]
    fn a_stable_attempt_reopens_the_gate_and_resets_the_budget() {
        let mut fsm = ConnectRecoveryFsm::new();
        let t = token_of(&fsm.start()[0]);
        let t = token_of(&fsm.apply(failed(t, 0, Some(C::Unreachable), true, false))[1]);
        let effects = fsm.apply(RecoveryEvent::RedeployFinished { token: t, now: Millis(0), seed: 0, result: RedeployResult::Succeeded });
        // This attempt stayed up past the stable threshold before failing.
        let t = token_of(&effects[0]);
        let stable_end = ms(RECONNECT_STABLE_THRESHOLD) + 1;
        let effects = fsm.apply(RecoveryEvent::AttemptFailed {
            token: t,
            started: Millis(0),
            now: Millis(stable_end),
            class: Some(C::Unreachable),
            should_bootstrap: true,
            has_remote_command: false,
            seed: 0,
        });
        assert_eq!(effects[0], RecoveryEffect::Log(RecoveryLog::RebootstrapDecision), "gate must be fresh again: {effects:?}");
    }

    #[test]
    fn retryable_redeploy_failure_falls_through_to_backoff_and_fatal_stops() {
        let mut fsm = ConnectRecoveryFsm::new();
        let t = token_of(&fsm.start()[0]);
        let t = token_of(&fsm.apply(failed(t, 0, Some(C::Unreachable), true, false))[1]);
        let effects = fsm.apply(RecoveryEvent::RedeployFinished { token: t, now: Millis(0), seed: 0, result: RedeployResult::RetryableFailure });
        assert!(matches!(effects[..], [RecoveryEffect::AnnounceReconnect { attempt: 1 }, RecoveryEffect::Backoff { .. }]), "{effects:?}");

        let mut fsm = ConnectRecoveryFsm::new();
        let t = token_of(&fsm.start()[0]);
        let t = token_of(&fsm.apply(failed(t, 0, Some(C::Unreachable), true, false))[1]);
        let effects = fsm.apply(RecoveryEvent::RedeployFinished { token: t, now: Millis(0), seed: 0, result: RedeployResult::Fatal });
        assert_eq!(effects, vec![RecoveryEffect::FailWithRedeployError]);
    }

    // ---- proptests (§3-2, §3-7) ------------------------------------------

    #[derive(Debug, Clone)]
    struct Step {
        stale: bool,
        stale_kind: u8,
        stale_offset: u64,
        now: u64,
        stable: bool,
        class: Option<u8>,
        should_bootstrap: bool,
        remote: bool,
        seed: u64,
        redeploy: u8,
    }

    fn step(max_now: u64) -> impl Strategy<Value = Step> {
        (
            (any::<bool>(), 0u8..3, 1u64..1_000),
            0..max_now,
            any::<bool>(),
            proptest::option::weighted(0.9, 0u8..4),
            (any::<bool>(), any::<bool>()),
            any::<u64>(),
            0u8..3,
        )
            .prop_map(|((stale, stale_kind, stale_offset), now, stable, class, (should_bootstrap, remote), seed, redeploy)| Step {
                stale,
                stale_kind,
                stale_offset,
                now,
                stable,
                class,
                should_bootstrap,
                remote,
                seed,
                redeploy,
            })
    }

    fn class_of(i: u8) -> ConnectOutcomeClass {
        all_classes()[usize::from(i) % 4].clone()
    }

    /// Answers the outstanding effect per `s` (or a stale event when
    /// `s.stale`). Returns `None` once the reducer has finished.
    fn answer(outstanding: &RecoveryEffect, s: &Step) -> Option<RecoveryEvent> {
        let class = s.class.map(class_of);
        let started = if s.stable { Millis(s.now.saturating_sub(ms(RECONNECT_STABLE_THRESHOLD))) } else { Millis(s.now) };
        let ev = |token: Token, kind: u8| match kind {
            0 => RecoveryEvent::AttemptFailed {
                token,
                started,
                now: Millis(s.now),
                class: class.clone(),
                should_bootstrap: s.should_bootstrap,
                has_remote_command: s.remote,
                seed: s.seed,
            },
            1 => RecoveryEvent::RedeployFinished {
                token,
                now: Millis(s.now),
                seed: s.seed,
                result: [RedeployResult::Succeeded, RedeployResult::RetryableFailure, RedeployResult::Fatal][usize::from(s.redeploy)],
            },
            _ => RecoveryEvent::BackoffElapsed { token },
        };
        match outstanding {
            RecoveryEffect::Attempt { token, .. } => Some(ev(*token, 0)),
            RecoveryEffect::Redeploy { token } => Some(ev(*token, 1)),
            RecoveryEffect::Backoff { token, .. } => Some(ev(*token, 2)),
            RecoveryEffect::GiveUp | RecoveryEffect::FailWithRedeployError => None,
            other => panic!("non-terminal, non-awaiting effect last: {other:?}"),
        }
    }

    fn stale_event(outstanding: &RecoveryEffect, s: &Step) -> RecoveryEvent {
        let base = match outstanding {
            RecoveryEffect::Attempt { token, .. } | RecoveryEffect::Redeploy { token } | RecoveryEffect::Backoff { token, .. } => token.0,
            _ => 0,
        };
        // A token that is not the outstanding one (older or never issued).
        let token = Token(if s.stale_offset % 2 == 0 { base.wrapping_add(s.stale_offset) } else { base.wrapping_sub(s.stale_offset) });
        let probe = match s.stale_kind {
            0 => RecoveryEffect::Attempt { token, silent: false },
            1 => RecoveryEffect::Redeploy { token },
            _ => RecoveryEffect::Backoff { token, delay: Duration::ZERO },
        };
        answer(&probe, s).expect("probe is an awaiting effect")
    }

    /// Effect lists are well-formed: non-empty, exactly one awaiting or
    /// terminal effect, and it is last (the shell can never stall).
    fn assert_well_formed(effects: &[RecoveryEffect]) {
        assert!(!effects.is_empty(), "a current-token event must always produce a next step");
        let is_next = |e: &RecoveryEffect| {
            matches!(
                e,
                RecoveryEffect::Attempt { .. }
                    | RecoveryEffect::Redeploy { .. }
                    | RecoveryEffect::Backoff { .. }
                    | RecoveryEffect::GiveUp
                    | RecoveryEffect::FailWithRedeployError
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
            let mut fsm = ConnectRecoveryFsm::new();
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
                    // Done: every further event is ignored.
                    let before = fsm.clone();
                    prop_assert!(fsm.apply(stale_event(&last, s)).is_empty());
                    prop_assert_eq!(&fsm, &before);
                    break;
                };
                effects = fsm.apply(ev);
                assert_well_formed(&effects);
            }
        }

        /// always-connects: a recoverable failure (a recorded class,
        /// auto-bootstrap allowed, no remote command, no fatal redeploy)
        /// never ends the recovery while inside `RECONNECT_BUDGET` — even
        /// with `now` jumping backwards and forwards.
        #[test]
        fn recoverable_failures_never_give_up_within_the_budget(steps in proptest::collection::vec(step(ms(RECONNECT_BUDGET) - 1), 1..120)) {
            let mut fsm = ConnectRecoveryFsm::new();
            let mut effects = fsm.start();
            for s in &steps {
                let s = Step { stale: false, class: Some(s.class.unwrap_or(0)), should_bootstrap: true, remote: false, redeploy: s.redeploy % 2, ..s.clone() };
                let ev = answer(outstanding(&effects).unwrap(), &s).expect("must not have finished");
                effects = fsm.apply(ev);
                assert_well_formed(&effects);
                prop_assert!(
                    !matches!(effects.last(), Some(RecoveryEffect::GiveUp | RecoveryEffect::FailWithRedeployError)),
                    "gave up on a recoverable failure within budget: {effects:?}"
                );
            }
        }

        /// Storm protection: without a stable attempt in between, two
        /// redeploys are always at least the minimum first-attempt gate delay
        /// (60s − 25% = 45s) apart *by the stamped `now`* — whatever order
        /// `now` arrives in.
        #[test]
        fn redeploys_are_spaced_by_the_gate(steps in proptest::collection::vec(step(10 * 60 * 60 * 1_000), 1..200)) {
            let mut fsm = ConnectRecoveryFsm::new();
            let mut effects = fsm.start();
            let mut last_redeploy_now: Option<u64> = None;
            for s in &steps {
                let s = Step { stale: false, stable: false, should_bootstrap: true, remote: false, redeploy: s.redeploy % 2, ..s.clone() };
                let Some(ev) = answer(outstanding(&effects).unwrap(), &s) else { break };
                let now = match &ev {
                    RecoveryEvent::AttemptFailed { now, .. } | RecoveryEvent::RedeployFinished { now, .. } => Some(now.0),
                    RecoveryEvent::BackoffElapsed { .. } => None,
                };
                effects = fsm.apply(ev);
                if effects.iter().any(|e| matches!(e, RecoveryEffect::Redeploy { .. })) {
                    let now = now.expect("only Attempt/Redeploy answers can redeploy");
                    if let Some(prev) = last_redeploy_now {
                        prop_assert!(now >= prev + 45_000, "redeploy at {now} only {}ms after one at {prev}", now.saturating_sub(prev));
                    }
                    last_redeploy_now = Some(now);
                }
            }
        }
    }
}
