//! The one shell (interpreter) for `connect_recovery_fsm::ConnectRecoveryFsm`
//! (ADR_FUNCTIONAL_CORE_EFFECTS.md §6 Step 6+7), shared by the Unix
//! (`wrapper.rs::run_ssh_with_connect_failure_recovery`) and Windows-native
//! (`native::connect::run_native_connect_with_recovery`) recovery loops,
//! which used to be two hand-maintained copies of the same loop body.
//!
//! Everything platform-specific sits behind [`ConnectRecoveryOps`]: how one
//! attempt runs and what "giving up" returns (Unix: the `ssh(1)` exit code as
//! `Ok`; native: the connect error as `Err`), how a redeploy's failure is
//! classified, and the platform-specific log wording. Everything else —
//! which branch to take, when a redeploy is allowed, backoff/budget/
//! lightweight-retry accounting — is the reducer's.
//!
//! The shell stamps `Millis` from a `tokio::time::Instant` epoch taken when
//! the loop starts (so `#[tokio::test(start_paused = true)]` drives it
//! deterministically, §2.2) and draws one fresh `rand::random()` seed per
//! event that may need jitter.

use std::collections::VecDeque;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use isekai_pipe_core::{ConnectOutcome, ConnectionIntent, IntentError};
use isekai_protocol::Millis;

use crate::connect_recovery_fsm::{ConnectRecoveryFsm, RecoveryEffect, RecoveryEvent, RecoveryLog, RedeployResult, MAX_LIGHTWEIGHT_RETRIES};
use crate::log_file::log_line;
use crate::wrapper::{log_auto_bootstrap_disabled, log_rebootstrap_and_retry_decision, resolve_claimed_outcome};

/// How one connect attempt ended, short of a hard error.
pub(crate) enum AttemptOutcome<F> {
    /// The session ran and ended; return this exit code as-is.
    Finished(u8),
    /// The connection failed; recovery inspects the `ConnectOutcome` signal.
    /// `F` is what [`ConnectRecoveryOps::give_up`] turns into the final
    /// result if recovery stops here.
    Failed(F),
}

/// How a silent re-deploy ended, already classified by the platform
/// (`BootstrapFailure::may_retry`, exactly as each path did before).
pub(crate) enum RedeployOutcome {
    Redeployed(ConnectionIntent),
    RetryableFailure,
    Fatal(anyhow::Error),
}

/// The I/O-bound operations the recovery loop sequences, factored into a
/// trait so the sequencing is unit-testable against a fake that records
/// calls (`native::connect`'s tests), without a real `isekai-pipe connect`
/// child or a mock `sshd` deploy target. `?Send` because the native attempt
/// future holds non-`Send` terminal state (a `RawModeGuard`) across await
/// points and is only ever `block_on`'d, never `spawn`ed.
#[async_trait(?Send)]
pub(crate) trait ConnectRecoveryOps {
    /// What a failed attempt carries until recovery decides to give up.
    type Failure;
    /// One full connect attempt against `intent`. `Err` is a hard error
    /// that aborts recovery immediately (e.g. the Unix path failing to even
    /// spawn `ssh(1)`). `silent` is true only for the retry right after a
    /// successful re-deploy — the native path's own SSH-target host-key
    /// TOFU must then refuse a never-before-seen key instead of prompting
    /// (Codex review finding, always-connects audit follow-up).
    async fn attempt(&mut self, intent: &ConnectionIntent, silent: bool) -> Result<AttemptOutcome<Self::Failure>>;
    /// The final result when recovery gives up on `failure`.
    fn give_up(&self, failure: Self::Failure) -> Result<u8>;
    /// Claims the `ConnectOutcome` signal `isekai-pipe connect` may have left
    /// behind for this exact attempt. Returns the raw `IntentError` so the
    /// shared `resolve_claimed_outcome` degrade-to-no-signal policy applies
    /// (Epic R PR1 code review).
    fn claim_outcome(&self, intent_id: &str) -> std::result::Result<Option<ConnectOutcome>, IntentError>;
    /// Whether auto-bootstrap is currently allowed (`--isekai-no-bootstrap` /
    /// `#@isekai bootstrap-policy never` turn it off).
    fn should_bootstrap(&self) -> bool;
    /// Whether this invocation runs a one-shot remote command
    /// (`isekai-ssh host -- cmd`) — see `remote_command_forbids_retry`.
    fn has_remote_command(&self) -> bool;
    /// Rebuilds the connection intent from already-trusted material, without
    /// re-deploying anything over SSH.
    fn build_intent(&self) -> Result<ConnectionIntent>;
    /// Re-deploys the helper for the already-trusted profile
    /// (`TofuConfirmation::Silent`), then rebuilds the intent.
    async fn redeploy(&mut self) -> RedeployOutcome;
    /// The profile name the shared log lines name (Unix: the resolved
    /// profile; native: the one recorded in the outcome).
    fn log_profile<'a>(&'a self, outcome: &'a ConnectOutcome) -> &'a str;
    /// The platform-specific trailing clause of
    /// `wrapper::log_rebootstrap_and_retry_decision`.
    fn rebootstrap_retry_note(&self) -> &'static str;
    /// The per-attempt "connection lost, reconnecting... (attempt N)" line.
    fn announce_reconnect(&self, attempt: u32);
}

fn stamp(epoch: tokio::time::Instant) -> Millis {
    Millis(u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX))
}

/// Runs the always-connects recovery loop: interprets the reducer's effects
/// in order, feeding each I/O result back as a new `apply` (§2.4-3, never
/// nested). Returns on a finished attempt, a give-up, or a hard error.
#[deny(clippy::wildcard_enum_match_arm)]
pub(crate) async fn drive_connect_recovery<O: ConnectRecoveryOps>(ops: &mut O, intent: ConnectionIntent) -> Result<u8> {
    let epoch = tokio::time::Instant::now();
    let mut fsm = ConnectRecoveryFsm::new();
    let mut intent = intent;
    // The last failed attempt's own failure, and the outcome claimed for it
    // (its detail/profile feed the log lines; the reducer only sees its class).
    let mut last_failure: Option<O::Failure> = None;
    let mut last_outcome: Option<ConnectOutcome> = None;
    let mut fatal_redeploy_error: Option<anyhow::Error> = None;
    let mut queue: VecDeque<RecoveryEffect> = fsm.start().into();

    while let Some(effect) = queue.pop_front() {
        match effect {
            RecoveryEffect::Attempt { token, silent } => {
                let started = stamp(epoch);
                match ops.attempt(&intent, silent).await? {
                    AttemptOutcome::Finished(exit_code) => return Ok(exit_code),
                    AttemptOutcome::Failed(failure) => {
                        last_failure = Some(failure);
                        // Epic R PR1 (S4): a claim failure degrades to "no
                        // signal" and never replaces the attempt's own failure.
                        last_outcome = resolve_claimed_outcome(ops.claim_outcome(&intent.intent_id));
                        let event = RecoveryEvent::AttemptFailed {
                            token,
                            started,
                            now: stamp(epoch),
                            class: last_outcome.as_ref().map(|o| o.class.clone()),
                            should_bootstrap: ops.should_bootstrap(),
                            has_remote_command: ops.has_remote_command(),
                            seed: rand::random(),
                        };
                        queue.extend(fsm.apply(event));
                    }
                }
            }
            RecoveryEffect::Redeploy { token } => {
                let result = match ops.redeploy().await {
                    RedeployOutcome::Redeployed(new_intent) => {
                        intent = new_intent;
                        RedeployResult::Succeeded
                    }
                    RedeployOutcome::RetryableFailure => RedeployResult::RetryableFailure,
                    RedeployOutcome::Fatal(err) => {
                        fatal_redeploy_error = Some(err);
                        RedeployResult::Fatal
                    }
                };
                queue.extend(fsm.apply(RecoveryEvent::RedeployFinished { token, now: stamp(epoch), seed: rand::random(), result }));
            }
            RecoveryEffect::Backoff { token, delay } => {
                tokio::time::sleep(delay).await;
                queue.extend(fsm.apply(RecoveryEvent::BackoffElapsed { token }));
            }
            RecoveryEffect::RebuildIntent => {
                intent = ops.build_intent().context("isekai-ssh: could not rebuild the connection intent for a reconnect")?;
            }
            RecoveryEffect::AnnounceReconnect { attempt } => ops.announce_reconnect(attempt),
            RecoveryEffect::Log(line) => match line {
                RecoveryLog::AutoBootstrapDisabled => {
                    let outcome = last_outcome.as_ref().expect("AutoBootstrapDisabled only returned when a connect-failure signal was found");
                    log_auto_bootstrap_disabled(&outcome.class, ops.log_profile(outcome), &outcome.detail);
                }
                RecoveryLog::RebootstrapDecision => {
                    let outcome = last_outcome.as_ref().expect("RebootstrapAndRetry only returned when a connect-failure signal was found");
                    log_rebootstrap_and_retry_decision(&outcome.class, ops.log_profile(outcome), &outcome.detail, ops.rebootstrap_retry_note());
                }
                RecoveryLog::RemoteCommandNotRetried => log_line!(
                    "isekai-ssh: connection lost while running a remote command; not auto-retrying \
                     (rerunning it could repeat a non-idempotent action)."
                ),
                RecoveryLog::LightweightExhaustedNoBootstrap => {
                    log_line!("isekai-ssh: gave up on {MAX_LIGHTWEIGHT_RETRIES} lightweight reconnect attempts and auto-bootstrap is disabled; giving up")
                }
                RecoveryLog::LightweightExhaustedRedeploying => {
                    log_line!("isekai-ssh: gave up on {MAX_LIGHTWEIGHT_RETRIES} lightweight reconnect attempts; trying a full re-deploy instead")
                }
            },
            RecoveryEffect::GiveUp => {
                let failure = last_failure.take().expect("GiveUp only follows a failed attempt");
                return ops.give_up(failure);
            }
            RecoveryEffect::FailWithRedeployError => {
                return Err(fatal_redeploy_error.take().expect("FailWithRedeployError only follows a fatal redeploy"));
            }
        }
    }
    // Unreachable: every reducer answer ends in an awaiting or terminal
    // effect (`connect_recovery_fsm`'s well-formedness proptest).
    Err(anyhow!("isekai-ssh: internal error: connect-failure recovery stalled"))
}
