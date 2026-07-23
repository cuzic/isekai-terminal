//! Windows-native connect entrypoint (M1): ties together `openssh-config`
//! (host resolution), the existing `#@isekai` directive resolution
//! (`crate::wrapper::resolve_for_native`), [`super::child_stdio`] (spawns
//! `isekai-pipe connect --stdio`), `russh_stream_session` (M0, the actual
//! SSH protocol), [`super::host_key_trust`] (TOFU), [`super::private_key`]/
//! [`super::agent_auth`] (authentication), and [`super::console`] (raw mode
//! + terminal size) into one working `isekai-ssh <destination>` path that
//! never shells out to `ssh(1)`.
//!
//! **Scope note**: the `ConnectOutcome`-driven re-bootstrap retry
//! (`always-connects.md`) *is* implemented here, mirroring
//! `wrapper.rs::run_ssh_with_connect_failure_recovery` — an already-trusted
//! destination whose cached deployment went stale/unreachable self-heals
//! without the user running `isekai-ssh init`/`doctor --fix` manually (the
//! re-deploy goes through `bootstrap_and_register`, which dispatches to M3's
//! `RusshBackend` on Windows, so it no longer shells out to `ssh(1)`). The
//! helper re-deploy itself is silent (no `[y/N]` trust confirmation — the
//! profile was already trusted once), but this is not "zero prompts": if the
//! bootstrap host's own SSH host key isn't trusted yet, `RusshBackend`'s
//! host-key TOFU still confirms it — a separate, orthogonal prompt that is
//! `always-connects.md`'s stated first-time-TOFU exception (see
//! [`run_native_connect_with_recovery`]). [`prepare`] *also* auto-bootstraps
//! a *brand-new* (never-registered) destination on first contact, inline
//! with a TOFU confirmation prompt — mirroring `wrapper::run`'s own
//! `Err(err) if should_bootstrap(...)` arm on Unix (an ultrareview-confirmed,
//! real-Windows-CI-reproduced gap this path used to have: it required a
//! separate manual `isekai-ssh init` first, even though
//! `always-connects.md`'s TOFU exception exempts only the interactive
//! confirmation itself, not a requirement to run a separate command).
//! Likewise, a destination with `#@isekai enabled no` (direct, non-isekai
//! SSH) isn't supported by this path yet — that's a plain
//! `connect_via_jump_or_direct` call away, but is left for a follow-up since
//! every destination this project's users actually run through `isekai-ssh`
//! has isekai routing enabled.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use isekai_pipe_core::{claim_connect_outcome, default_runtime_dir, ConnectionIntent};
use russh::client;
use russh_stream_session::{
    authenticate_keyboard_interactive, authenticate_openssh_cert_with_passphrase, authenticate_publickey_with_passphrase,
    authenticate_session, establish_over_stream, open_channel, verifying_handler_with_routes_and_reason, Credential,
    ForwardRoutes, KeyboardInteractivePrompt, RejectionReason, SessionError, SessionKind,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::mux::ctl_forward;
use super::mux::handoff::HandoffCredentials;

use crate::log_file::log_line;
use crate::wrapper::{
    bootstrap_and_register, build_connection_intent, decide_connect_failure_recovery, outcome_summary,
    print_bootstrap_failure_guidance, should_bootstrap, ConnectFailureRecoveryAction, TofuConfirmation, WrapperPlan,
    WrapperResolution,
};

#[cfg(windows)]
use super::agent_auth;
use super::child_stdio::{spawn_isekai_pipe_connect, ChildStdio};
use super::console;
use super::console_stdin;
use super::escape::{process_stdin_bytes, EscapeAction};
use super::host_key_trust::FileBackedHostKeyVerifier;
use super::keyboard_interactive;
use super::private_key;

/// The concrete `russh` client handle this native path establishes — an
/// already-authenticated, still-live SSH connection. `native/mux` shares a
/// clone of this across the owner's own session and every relayed client
/// (see [`OwnerHook`]).
pub(crate) type NativeHandle = client::Handle<russh_stream_session::VerifyingHandler<FileBackedHostKeyVerifier>>;

/// A hook the mux holder path ([`super::mux`]) supplies so that, the moment
/// the shared SSH session is authenticated, it can start accepting local IPC
/// clients on the shared handle — without the connect+auth+recovery machinery
/// here having to know anything about `local-ipc-mux`. It receives an
/// [`Arc`]-shared, [`Mutex`](tokio::sync::Mutex)-guarded handle: `channel_open_session`
/// only needs `&self`, but `streamlocal_forward` (the `#@isekai ctl-socket`
/// remote forward, M5) needs `&mut self`, so the shared handle is behind a
/// mutex that is held only for the brief open/forward calls and never across
/// the per-channel I/O loop. It runs at most once, on the *successful* connect
/// attempt (a failed attempt errors before the handle exists, leaving the hook
/// intact for the re-bootstrap retry). Boxed `FnOnce` + `Send` because it
/// `tokio::spawn`s the accept loop and hands back its [`tokio::task::JoinHandle`]:
/// a holder process has **no foreground shell of its own** (unlike the old,
/// removed "owner" role) — [`run_authenticated_session`] awaits this handle as
/// the *entire* session body instead, so the holder's lifetime is exactly the
/// accept loop's own (idle-exit or a fatal local-IPC error), decoupled from
/// any particular tab — the `ControlPersist`-equivalent redesign this exists
/// for (`super::mux`'s module docs).
pub(crate) type SharedHandle = Arc<tokio::sync::Mutex<NativeHandle>>;
/// Receives the shared handle plus, when `#@isekai ctl-socket` is enabled for
/// this invocation, the [`ForwardRoutes`] the connection's handler dispatches
/// forwarded-streamlocal channels through — so the holder can set up a *private*
/// per-tab ctl forward for each mux client (M5). `None` means ctl-socket is off
/// and no per-client forward should be requested.
pub(crate) type OwnerHook = Box<dyn FnOnce(SharedHandle, Option<ForwardRoutes>) -> tokio::task::JoinHandle<()> + Send>;

/// Everything [`run_prepared`] needs, resolved once up front so the mux
/// dispatch ([`super::mux::run`]) can compute the channel name from the same
/// resolution before deciding whether to become owner or client.
pub(crate) struct Prepared {
    plan: WrapperPlan,
    resolution: WrapperResolution,
    host_config: openssh_config::HostConfig,
    intent: ConnectionIntent,
    runtime_dir: PathBuf,
}

impl Prepared {
    pub(crate) fn plan(&self) -> &WrapperPlan {
        &self.plan
    }
    pub(crate) fn resolution(&self) -> &WrapperResolution {
        &self.resolution
    }
    pub(crate) fn host_config(&self) -> &openssh_config::HostConfig {
        &self.host_config
    }
    pub(crate) fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }
}

#[cfg(test)]
impl Prepared {
    /// Assembles a [`Prepared`] directly from already-resolved parts, for
    /// tests (here and in `native/mux/mod.rs`) that need one without going
    /// through the real trust-store/TOFU machinery [`prepare`] drives —
    /// mirrors the plan/resolution/host_config/intent construction this
    /// module's own `bogus_pipe`-based recovery tests already use.
    pub(crate) fn for_test(
        plan: WrapperPlan,
        resolution: WrapperResolution,
        host_config: openssh_config::HostConfig,
        intent: ConnectionIntent,
        runtime_dir: PathBuf,
    ) -> Self {
        Prepared { plan, resolution, host_config, intent, runtime_dir }
    }

    /// Mutable access for tests that need to tweak the resolved `HostConfig`
    /// after construction (e.g. pointing `identity_file` at a throwaway
    /// encrypted key) without threading a whole new `for_test` parameter
    /// combination through every caller that doesn't need it.
    pub(crate) fn host_config_mut(&mut self) -> &mut openssh_config::HostConfig {
        &mut self.host_config
    }
}

/// Resolves argv into a [`Prepared`] (config resolution, `--isekai-log-file`
/// init, trust-store lookup — auto-bootstrapping a brand-new destination
/// inline if needed) without yet establishing the SSH session itself — the
/// shared front half of both the single-process path ([`run`]) and the mux
/// dispatch ([`super::mux::run`]).
pub(crate) async fn prepare(args: Vec<String>) -> Result<Prepared> {
    let plan = crate::wrapper::parse_wrapper(args)?;
    // `--isekai-log-file` must be honored on the native path too — the Unix
    // path opens it at the top of `wrapper::run`; without this the flag was
    // silently ignored on Windows (Codex review finding). Opened before any
    // connection attempt so every diagnostic line below is captured.
    //
    // The `else` arm mirrors `wrapper::run`'s own default-verbose-log
    // initialization, which this native path was missing entirely (real
    // Windows CI regression found while investigating a post-merge e2e
    // failure): `log_line_verbose!` (`bootstrap_and_register`'s "Registered
    // ... in ..." line among others) silently drops every line whenever
    // `log_file::init_verbose` was never called — `append_verbose_line`'s
    // own doc comment confirms this is a deliberate best-effort no-op, not a
    // panic, so the gap produced no visible symptom on its own. It only
    // surfaced once a test started depending on that line appearing in the
    // default verbose log instead of stderr.
    if let Some(log_file) = plan.log_file() {
        crate::log_file::init(log_file)
            .with_context(|| format!("isekai-ssh: failed to open --isekai-log-file at {}", log_file.display()))?;
    } else if let Ok(verbose_log_file) = isekai_pipe_core::default_log_file() {
        // Best-effort, same as `wrapper::run`: verbose bootstrap/diagnostic
        // detail is a nicety, never worth failing the connection over a
        // permissions/read-only-filesystem error opening its own log file.
        let _ = crate::log_file::init_verbose(&verbose_log_file);
    }
    let (resolution, host_config) = crate::wrapper::resolve_for_native(&plan)?;
    if !resolution.isekai_enabled() {
        return Err(anyhow!(
            "isekai-ssh: {:?} has isekai routing disabled (#@isekai enabled no / --isekai-direct); \
             the native Windows path doesn't support plain direct SSH yet — see native/connect.rs's module docs.",
            plan.destination_host()
        ));
    }
    let intent = match build_connection_intent(&resolution) {
        Ok(intent) => intent,
        // A brand-new (never-registered) destination: auto-bootstrap inline
        // with a TOFU confirmation prompt, exactly mirroring
        // `wrapper::run`'s own `Err(err) if should_bootstrap(...)` arm on
        // Unix — the only difference is which `BootstrapBackend` performs the
        // actual deploy (`RusshBackend` here via
        // `native::bootstrap_backend::resolve_backend`, vs. `OpenSshBackend`
        // on Unix; `bootstrap_and_register` itself already dispatches on
        // platform). `always-connects.md`'s stated TOFU exception exempts the
        // *interactive confirmation itself* being un-automatable, not a
        // requirement that the user run a separate `isekai-ssh init` command
        // first — this native path used to require exactly that, an
        // ultrareview-confirmed, real-Windows-CI-reproduced divergence from
        // the Unix path's behavior.
        Err(err) if should_bootstrap(&plan, &resolution) => {
            if let Err(bootstrap_err) = bootstrap_and_register(&plan, &resolution, TofuConfirmation::AlwaysPrompt).await {
                print_bootstrap_failure_guidance(&bootstrap_err);
                return Err(bootstrap_err.context(format!("{err}\nisekai-ssh: auto-bootstrap failed")));
            }
            build_connection_intent(&resolution).context("isekai-ssh: still not trusted after auto-bootstrap")?
        }
        Err(err) => {
            return Err(err.context(format!(
                "isekai-ssh: {:?} is not set up yet — run `isekai-ssh init {}` first \
                 (auto-bootstrap is disabled: --isekai-no-bootstrap / #@isekai bootstrap-policy never)",
                plan.destination(),
                plan.destination()
            )))
        }
    };
    let runtime_dir = default_runtime_dir()?;
    Ok(Prepared { plan, resolution, host_config, intent, runtime_dir })
}

/// `isekai-ssh <destination>` entrypoint for the native path — the
/// `cfg(windows)`-gated alternative `main.rs` dispatches to instead of
/// `wrapper::run`. Takes the same raw argv `wrapper::run` does. The mux
/// dispatch ([`super::mux::run`]) is what `main.rs` actually calls on Windows;
/// this remains the single-process path it falls back to (and the only path
/// exercised on non-Windows unit tests).
pub(crate) async fn run(args: Vec<String>) -> Result<u8> {
    let prepared = prepare(args).await?;
    run_prepared(prepared, None, HandoffCredentials::default()).await
}

/// Drives a [`Prepared`] connection through the always-connects recovery.
/// `owner_hook` is `None` for the single-process path and `Some` for the mux
/// holder (see [`OwnerHook`]). `handoff` is the passphrase hand-off set (Phase
/// 1b, see `super::mux::handoff`'s docs) — empty for every path except a
/// holder that was spawned with one, or a client falling back to a direct
/// connect after reusing the set it resolved before spawning that holder.
pub(crate) async fn run_prepared(prepared: Prepared, owner_hook: Option<OwnerHook>, handoff: HandoffCredentials) -> Result<u8> {
    let Prepared { plan, resolution, host_config, intent, runtime_dir } = prepared;
    run_native_connect_with_recovery(&plan, &resolution, &host_config, intent, &runtime_dir, owner_hook, handoff).await
}

/// Mirrors `wrapper.rs::run_ssh_with_connect_failure_recovery` for the
/// Windows-native path (`always-connects.md`): runs one connect attempt; if
/// it fails *and* the `isekai-pipe connect` child left behind a
/// `ConnectOutcome` side-channel signal for this exact attempt
/// (`isekai_pipe_core::claim_connect_outcome`), re-deploys the helper for the
/// (already-trusted) profile and retries exactly once more. Structurally at
/// most two connect attempts ever happen — no loop, no recursion — matching
/// that function's own "at most two attempts" property, so it cannot run
/// away even if the retry also fails.
///
/// "Silent" here means the *helper re-deploy* takes no `[y/N]` trust
/// confirmation (`TofuConfirmation::Silent`) — the profile was already
/// trusted once. It does **not** mean zero prompts ever: the re-deploy dials
/// the bootstrap host over SSH, and if that host's own SSH host key isn't yet
/// in the trust store, `RusshBackend`'s host-key TOFU
/// (`isekai_trust::FileBackedHostKeyVerifier`) still asks the user to confirm
/// it. That's a separate, orthogonal first-time-TOFU prompt, and it's the
/// stated exception in `always-connects.md` (a genuinely new host key needs a
/// human), not a violation of the "always-connects" principle.
///
/// The connect-failure *decision* is single-sourced with the Unix path via
/// `crate::wrapper::decide_connect_failure_recovery` (unit-tested there), and
/// the *sequencing* (attempt → claim → maybe re-bootstrap → retry once) is
/// factored into [`drive_connect_recovery`] over the [`ConnectRecoveryOps`]
/// trait so it's unit-tested here against a fake, apart from the full e2e
/// flow — keeping the policy from drifting between the two paths.
///
/// Only reachable *after* `build_connection_intent` already succeeded once
/// in [`run`] — a brand-new (never-registered) profile's own interactive
/// bootstrap is out of scope for this path (see the module docs).
///
/// **What stays e2e-untested on the native side**: only the real
/// [`ConnectRecoveryOps`] implementation ([`NativeConnectOps`]) — i.e. that a
/// *real* `isekai-pipe connect` child + mock `sshd` deploy actually wire
/// through — is left to the harness `tests/wrapper_stale_trust_auto_recovery_e2e.rs`
/// already builds for the Unix path, whose `bootstrap_and_register`/
/// `claim_connect_outcome` this path shares verbatim. The sequencing logic
/// itself is fully unit-tested below via a fake.
async fn run_native_connect_with_recovery(
    plan: &WrapperPlan,
    resolution: &WrapperResolution,
    host_config: &openssh_config::HostConfig,
    intent: ConnectionIntent,
    runtime_dir: &Path,
    owner_hook: Option<OwnerHook>,
    handoff: HandoffCredentials,
) -> Result<u8> {
    let mut ops = NativeConnectOps { plan, resolution, host_config, runtime_dir, owner_hook, handoff };
    drive_connect_recovery(&mut ops, intent).await
}

/// The I/O-bound operations [`drive_connect_recovery`] sequences, factored
/// into a trait so the recovery *sequencing* is unit-testable against a fake
/// that records calls, without a real `isekai-pipe connect` child or a mock
/// `sshd` deploy target — mirroring how `wrapper.rs::decide_connect_failure_recovery`
/// is unit-tested apart from the full e2e flow. `?Send` because the real
/// attempt future holds non-`Send` terminal state (a `RawModeGuard`) across
/// await points and is only ever `block_on`'d, never `spawn`ed.
#[async_trait(?Send)]
trait ConnectRecoveryOps {
    /// One full connect attempt against `intent` (spawn + auth + shell + I/O
    /// loop). `Err` means the connection could never be established — the
    /// failure mode a `ConnectOutcome` signal accompanies. `silent` is true
    /// only for the retry-after-rebootstrap attempt (`always-connects.md`'s
    /// `TofuConfirmation::Silent` re-deploy promises no interaction is
    /// needed) — when true, this attempt's own SSH-target host-key TOFU
    /// (a *separate* check from the re-deploy's own, already-silent-aware
    /// bootstrap dial) must refuse a never-before-seen key immediately
    /// instead of prompting, so a live-but-answerless stdin can't hang it
    /// (Codex review finding on the always-connects audit follow-up: the
    /// first attempt is exempt from this — an interactive first contact is
    /// the documented TOFU exception — but the silent retry is not).
    async fn attempt(&mut self, intent: &ConnectionIntent, silent: bool) -> Result<u8>;
    /// Claims the `ConnectOutcome` signal `isekai-pipe connect` may have left
    /// behind for this exact attempt, if any.
    fn claim_outcome(&self, intent_id: &str) -> Result<Option<isekai_pipe_core::ConnectOutcome>>;
    /// Whether auto-bootstrap is currently allowed (`--isekai-no-bootstrap` /
    /// `#@isekai bootstrap-policy never` turn it off).
    fn should_bootstrap(&self) -> bool;
    /// Re-deploys the helper for the already-trusted profile (no `[y/N]` trust
    /// confirmation — see [`run_native_connect_with_recovery`]'s docs on the
    /// separate host-key TOFU prompt), then rebuilds the intent from the
    /// refreshed trust material.
    async fn rebootstrap_and_rebuild_intent(&mut self) -> Result<ConnectionIntent>;
}

/// Pure sequencing of the "always-connects" recovery, generic over
/// [`ConnectRecoveryOps`] so the retry path is testable without real I/O.
/// At most two `attempt`s ever happen (see [`run_native_connect_with_recovery`]).
async fn drive_connect_recovery<O: ConnectRecoveryOps>(ops: &mut O, intent: ConnectionIntent) -> Result<u8> {
    let first_error = match ops.attempt(&intent, false).await {
        Ok(exit_code) => return Ok(exit_code),
        Err(e) => e,
    };

    let outcome = ops.claim_outcome(&intent.intent_id)?;

    match decide_connect_failure_recovery(outcome.is_some(), ops.should_bootstrap()) {
        ConnectFailureRecoveryAction::NoRecoverableSignal => Err(first_error),
        ConnectFailureRecoveryAction::AutoBootstrapDisabled => {
            let outcome = outcome.expect("AutoBootstrapDisabled only returned when a connect-failure signal was found");
            log_line!(
                "isekai-ssh: {} for {:?} ({}), but auto-bootstrap is disabled \
                 (--isekai-no-bootstrap / #@isekai bootstrap-policy never) — run `isekai-ssh init` manually.",
                outcome_summary(&outcome.class),
                outcome.profile,
                outcome.detail
            );
            Err(first_error)
        }
        ConnectFailureRecoveryAction::RebootstrapAndRetry => {
            let outcome = outcome.expect("RebootstrapAndRetry only returned when a connect-failure signal was found");
            log_line!(
                "isekai-ssh: {} for {:?} ({}); re-deploying the helper automatically \
                 (if the SSH host key isn't trusted yet, host-key confirmation is a separate prompt)...",
                outcome_summary(&outcome.class),
                outcome.profile,
                outcome.detail
            );
            let intent2 = ops.rebootstrap_and_rebuild_intent().await?;
            ops.attempt(&intent2, true).await
        }
    }
}

/// The real [`ConnectRecoveryOps`] backed by an actual `isekai-pipe connect`
/// child, the on-disk `ConnectOutcome` side channel, and
/// `bootstrap_and_register`.
struct NativeConnectOps<'a> {
    plan: &'a WrapperPlan,
    resolution: &'a WrapperResolution,
    host_config: &'a openssh_config::HostConfig,
    runtime_dir: &'a Path,
    /// Consumed (via `take`) only on the attempt whose SSH session actually
    /// authenticates — `connect_attempt` takes it out of this `Option` at the
    /// moment it invokes the hook (right after auth succeeds), so a failed
    /// attempt (which errors before that point) leaves it intact for the
    /// re-bootstrap retry. Passed by `&mut` for exactly that reason: a plain
    /// `take()` here would consume the hook on *every* attempt, dropping the
    /// mux owner role the moment the first attempt failed even though the
    /// retry is what actually succeeds. `None` for the single-process path.
    owner_hook: Option<OwnerHook>,
    /// The passphrase hand-off set (Phase 1b) — empty except for a spawned
    /// holder consuming one, or a client reusing the one it resolved before
    /// spawning that holder for its own fallback direct connect (see
    /// `super::mux::handoff`'s docs).
    handoff: HandoffCredentials,
}

#[async_trait(?Send)]
impl ConnectRecoveryOps for NativeConnectOps<'_> {
    async fn attempt(&mut self, intent: &ConnectionIntent, silent: bool) -> Result<u8> {
        connect_attempt(self.plan, self.resolution, self.host_config, intent, self.runtime_dir, &mut self.owner_hook, &self.handoff, silent).await
    }

    fn claim_outcome(&self, intent_id: &str) -> Result<Option<isekai_pipe_core::ConnectOutcome>> {
        claim_connect_outcome(self.runtime_dir, intent_id)
            .map_err(|e| anyhow!("isekai-ssh: failed to check for a connect-failure signal: {e}"))
    }

    fn should_bootstrap(&self) -> bool {
        should_bootstrap(self.plan, self.resolution)
    }

    async fn rebootstrap_and_rebuild_intent(&mut self) -> Result<ConnectionIntent> {
        bootstrap_and_register(self.plan, self.resolution, TofuConfirmation::Silent)
            .await
            .map_err(|e| {
                print_bootstrap_failure_guidance(&e);
                e.context("isekai-ssh: automatic re-bootstrap after a connect failure failed")
            })?;
        build_connection_intent(self.resolution).context("isekai-ssh: still not trusted after automatic re-bootstrap")
    }
}

/// One full connect attempt against `intent`: spawn `isekai-pipe connect
/// --stdio`, run the SSH handshake + authentication over its stdio, open an
/// interactive shell channel, and relay bytes until the session ends —
/// returning the remote exit code. An `Err` here means the connection could
/// never be established (the failure mode
/// [`run_native_connect_with_recovery`] inspects for a `ConnectOutcome`
/// signal). Called at most twice by that function, mirroring
/// `wrapper.rs::run_ssh_once`'s role.
async fn connect_attempt(
    plan: &WrapperPlan,
    resolution: &WrapperResolution,
    host_config: &openssh_config::HostConfig,
    intent: &ConnectionIntent,
    runtime_dir: &Path,
    owner_hook: &mut Option<OwnerHook>,
    handoff: &HandoffCredentials,
    silent: bool,
) -> Result<u8> {
    let mut child = spawn_isekai_pipe_connect(plan.pipe_path(), runtime_dir, intent)?;
    let stdio = ChildStdio::take_from(&mut child)
        .ok_or_else(|| anyhow!("isekai-ssh: spawned isekai-pipe connect without piped stdin/stdout (internal bug)"))?;

    let result = run_authenticated_session(stdio, plan, resolution, host_config, owner_hook, handoff, silent).await;

    if result.is_err() {
        // The `isekai-pipe connect` child writes its `ConnectOutcome`
        // side-channel (`always-connects.md`) as it fails and exits — the
        // exact signal `drive_connect_recovery` reads back via
        // `claim_connect_outcome` (which does *not* retry) to decide whether to
        // auto re-bootstrap. `spawn_isekai_pipe_connect` sets `kill_on_drop`,
        // so dropping `child` the instant the SSH layer errors here can cut the
        // child off *mid-write*, leaving no outcome to claim and turning a
        // recoverable stale-trust/unreachable failure into an unrecoverable
        // one. Give the child a brief, best-effort window to exit on its own
        // (finishing that write) before `kill_on_drop` takes over. Timing out
        // is fine — we fall through to the kill regardless; the point is only
        // to *let* a nearly-done child finish, never to wait on a hung one.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), child.wait()).await;
    }

    // On success `child` (the isekai-pipe connect process) was kept alive for
    // the whole session by this binding; dropping it here tears the subprocess
    // down via `kill_on_drop` (per `spawn_isekai_pipe_connect`'s docs).
    drop(child);
    result
}

/// Decide whether to open a non-interactive `Exec` channel or a PTY+`Shell`
/// channel, mirroring `ssh(1)`'s `-t`/`-T` semantics: an explicit `-T`
/// (`RequestTty::No`) with a remote command skips the PTY entirely and runs
/// the command directly via `SessionKind::Exec`. Every other combination
/// (no command at all, or a command with `Auto`/`Yes`/`Force`) opens a
/// PTY+shell instead — when a command is also present, the caller `exec`s it
/// over that PTY afterwards (see the `remote_cmd`/`request_tty` check right
/// after the channel opens in [`run_authenticated_session`]), since
/// `SessionKind::Exec` itself never allocates a PTY.
fn decide_session_kind(
    remote_cmd: Option<&[String]>,
    request_tty: crate::wrapper::RequestTty,
    term: &str,
    cols: u32,
    rows: u32,
    terminal_modes: &[(russh::Pty, u32)],
) -> SessionKind {
    match remote_cmd {
        Some(cmd) if request_tty == crate::wrapper::RequestTty::No => SessionKind::Exec { command: cmd.join(" ") },
        _ => SessionKind::Shell { term: term.to_string(), cols, rows, terminal_modes: terminal_modes.to_vec() },
    }
}

/// The authenticated-session half of [`connect_attempt`], split out so
/// [`connect_attempt`] can retain the `isekai-pipe connect` `Child` and, on an
/// error, give it a brief best-effort window to finish writing its
/// `ConnectOutcome` side channel before `kill_on_drop` tears it down. Owns
/// `stdio` (the child's piped stdin/stdout) for the whole session and, on
/// success, runs the shell I/O loop to completion, returning the remote exit
/// code. Takes `owner_hook` by `&mut` so a failure here (which returns before
/// the hook is `take`n) leaves it intact for the always-connects retry.
async fn run_authenticated_session(
    stdio: ChildStdio,
    plan: &WrapperPlan,
    resolution: &WrapperResolution,
    host_config: &openssh_config::HostConfig,
    owner_hook: &mut Option<OwnerHook>,
    handoff: &HandoffCredentials,
    silent: bool,
) -> Result<u8> {
    // A holder process is a detached background invocation with no attached
    // console — it can never prompt for anything (host-key TOFU, a
    // passphrase, keyboard-interactive), regardless of which connect attempt
    // this is. `silent` normally reflects only the retry-after-rebootstrap
    // attempt number; holder mode forces it on unconditionally.
    let silent = silent || owner_hook.is_some();
    let (host, port) = resolution.native_host_port(plan.destination_host());
    let host_port = format!("{host}:{port}");
    // Username precedence: destination user@ part > ssh_config User > local username
    let username = plan
        .destination_user()
        .map(String::from)
        .or_else(|| host_config.user.clone())
        .or_else(local_username)
        .ok_or_else(|| anyhow!("isekai-ssh: no username configured (ssh_config User, $USER, %USERNAME%) for {host_port}"))?;

    let store_path = isekai_trust::default_ssh_host_key_trust_store_path()
        .map_err(|e| anyhow!("isekai-ssh: could not determine the SSH host key trust store path: {e}"))?;
    let confirm_host_port = host_port.clone();
    // `silent` (true only for the retry-after-rebootstrap attempt) must
    // never prompt — the confirmation would otherwise block on a
    // live-but-answerless stdin, the exact `always-connects.md` gap the
    // sibling `RusshBackend::with_unattended_new_host_policy` closure
    // exists to close on the bootstrap-dial side. This attempt's own
    // SSH-target host key is a separate check from that dial's, so it needs
    // its own silent-aware refusal.
    let confirm_new_host: Arc<dyn Fn(&str) -> bool + Send + Sync> = if silent {
        Arc::new(move |fingerprint: &str| {
            eprintln!(
                "isekai-ssh: unknown SSH host key for {confirm_host_port:?} (fingerprint {fingerprint}) in a \
                 silent/automated re-bootstrap retry — refusing without prompting. Run this connection from an \
                 interactive terminal once to confirm it."
            );
            false
        })
    } else {
        Arc::new(move |fingerprint: &str| prompt_new_host_confirmation(&confirm_host_port, fingerprint))
    };
    let verifier = Arc::new(FileBackedHostKeyVerifier::new(store_path, host_port.clone(), confirm_new_host));

    // Same silent-aware seam as `confirm_new_host` above, for a
    // passphrase-protected identity file: a silent/automated retry must never
    // block on a live-but-answerless stdin, so it just logs an actionable
    // message and lets the candidate loop move on (next identity, then the
    // SSH agent) instead of prompting.
    let prompt_passphrase: Arc<dyn Fn(&Path, u32) -> Option<String> + Send + Sync> = if silent {
        Arc::new(|path: &Path, _attempt: u32| {
            log_line!(
                "isekai-ssh: identity file {} is passphrase-protected; skipping in a silent/automated retry \
                 — run this connection from an interactive terminal once to unlock it.",
                path.display()
            );
            None
        })
    } else {
        Arc::new(console::prompt_passphrase)
    };

    // Same silent-aware seam again, for keyboard-interactive (PAM/OTP/2FA):
    // a silent/automated retry must never block waiting on a live server
    // prompt it can't answer.
    let kbi_responder: Arc<dyn Fn(&[KeyboardInteractivePrompt]) -> Vec<String> + Send + Sync> = if silent {
        Arc::new(|prompts: &[KeyboardInteractivePrompt]| {
            log_line!(
                "isekai-ssh: server requested keyboard-interactive authentication in a silent/automated retry \
                 — refusing without prompting. Run this connection from an interactive terminal once."
            );
            vec![String::new(); prompts.len()]
        })
    } else {
        Arc::new(keyboard_interactive::console_responder)
    };
    let prompts = InteractivePrompts { passphrase: &*prompt_passphrase, keyboard_interactive: &*kbi_responder, handoff };

    // The ctl-socket route table the handler dispatches forwarded-streamlocal
    // channels through — one per connection, shared by this process's own
    // foreground shell and (via the owner hook) every mux client's per-tab
    // forward. Built before the handshake so it can be installed on the handler.
    let forward_routes = ForwardRoutes::new();
    let handle = connect_and_authenticate(stdio, &username, host_config, &verifier, &forward_routes, &prompts)
        .await
        .with_context(|| format!("isekai-ssh: failed to connect to {username}@{host_port}"))?;

    // Shared behind a mutex so the mux accept loop (below) and this process's
    // own foreground shell can each open independent channels on the one
    // connection, and so `streamlocal_forward` (which needs `&mut self`) can
    // be called for the ctl-socket forward (M5).
    let handle: SharedHandle = Arc::new(tokio::sync::Mutex::new(handle));

    let ctl_enabled = ctl_forward::should_forward(plan, resolution);

    // The SSH session is now authenticated. If this is the mux holder path,
    // hand a shared clone of the handle to the accept loop so sibling tabs can
    // start opening their own channels — and, since a holder has no
    // foreground shell of its own, await the accept loop's own `JoinHandle` as
    // this session's entire body, skipping everything below. When ctl-socket
    // is enabled, also hand over the route table so each client gets its own
    // private forward. Reached only on success, so a failed attempt (which
    // returns above) never `take`s the hook out of the caller's `Option` — it
    // stays available for the always-connects re-bootstrap retry.
    if let Some(hook) = owner_hook.take() {
        let serve_handle = hook(handle.clone(), ctl_enabled.then(|| forward_routes.clone()));
        let _ = serve_handle.await;
        return Ok(0);
    }

    let (cols, rows) = console::terminal_size();
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    let terminal_modes = console::build_terminal_modes();

    // Decide SessionKind: remote command vs interactive shell, with -t/-T support.
    let remote_cmd = plan.remote_command();
    let session_kind = decide_session_kind(remote_cmd, plan.request_tty, &term, cols, rows, &terminal_modes);

    // This process's own foreground shell's ctl-socket forward (owner or
    // single-process). Opportunistic: a forward that fails to set up just
    // leaves `ctl` as `None` and the shell opens without it.
    let ctl = if ctl_enabled && remote_cmd.is_none() {
        ctl_forward::request(&handle, &forward_routes).await
    } else {
        None
    };

    let open_result = {
        // Held only for the open; released before the I/O loop so sibling
        // channels and forwards aren't blocked behind this session's traffic.
        // The guard is dropped at the end of this block, before any
        // `ctl_forward` cleanup below re-locks the handle.
        let guard = handle.lock().await;
        match &ctl {
            Some(fwd) => ctl_forward::open_login_shell(&guard, &term, cols, rows, &fwd.remote_path)
                .await
                .context("isekai-ssh: failed to open a ctl-socket login shell"),
            None => open_channel(&guard, &session_kind).await.context("isekai-ssh: failed to open a session channel"),
        }
    };
    let mut channel = match open_result {
        Ok(channel) => channel,
        Err(e) => {
            // The channel open failed *after* we'd already requested this
            // tab's private ctl-socket forward (mirrors `owner.rs::relay_client`'s
            // identical cleanup) — tear it down before bailing so it doesn't
            // leak on the remote (and its route entry linger locally). Every
            // exit path must release a requested forward.
            if let Some(fwd) = &ctl {
                ctl_forward::cancel(&handle, &forward_routes, &fwd.remote_path).await;
            }
            return Err(e);
        }
    };

    // ForwardAgent: wire up agent forwarding if the config enables it. A failure here
    // (e.g. the server rejects agent forwarding, or the local agent connection dropped)
    // shouldn't abort the session — the shell/exec still works without it — but it
    // should be visible instead of silently vanishing (previously discarded via `let _ =`).
    if matches!(host_config.forward_agent, Some(openssh_config::ForwardAgent::Yes)) {
        if let Err(e) = channel.agent_forward(true).await {
            log_line!("isekai-ssh: agent forwarding request failed: {e}");
        }
    }

    // SendEnv: forward LANG and LC_* environment variables (the most common
    // ones). Full `SendEnv`/`AcceptEnv` matching against ssh_config would
    // require the remote to opt in via sshd_config; these are the ones that
    // are commonly accepted by default on most servers.
    for (key, value) in std::env::vars() {
        if key == "LANG" || key.starts_with("LC_") {
            let _ = channel.set_env(false, &key, &value).await;
        }
    }

    // If a remote command was requested with a PTY, exec it now.
    if let (Some(cmd), true) = (remote_cmd, plan.request_tty != crate::wrapper::RequestTty::No) {
        let command = cmd.join(" ");
        let _ = channel.exec(false, command.as_str()).await;
    }

    // Apply ctl messages this tab receives over its forward directly to the
    // local terminal (OSC title/clipboard on stderr).
    let ctl_remote_path = ctl.as_ref().map(|fwd| fwd.remote_path.clone());
    if let Some(fwd) = ctl {
        tokio::spawn(ctl_forward::pump_to_stderr(fwd.channels, resolution.profile().to_string()));
    }

    let _raw_mode = console::RawModeGuard::enable().context("isekai-ssh: failed to enable raw terminal mode")?;
    let exit_code = run_shell_io_loop(&mut channel).await?;

    // Best-effort teardown of this tab's forward before the handle is dropped.
    if let Some(path) = &ctl_remote_path {
        ctl_forward::cancel(&handle, &forward_routes, path).await;
    }

    // Keeps the compiler from complaining that `handle` is unused past this
    // point — it must stay alive for the duration of the I/O loop above.
    // Dropping this `Arc<handle>` only tears the SSH session down once every
    // clone is gone (a mux accept loop, if any, holds another) — a deliberate
    // keep-alive, not a no-op. The `isekai-pipe connect` `Child` is held (and
    // torn down) by the caller [`connect_attempt`], which needs it on the
    // error path to let a failing child finish writing its `ConnectOutcome`.
    drop(handle);

    Ok(exit_code)
}

fn local_username() -> Option<String> {
    std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok())
}

/// Real interactive TOFU prompt for a never-before-seen host key —
/// `ssh(1)`'s own wording, adapted. Runs on a `spawn_blocking` thread (see
/// `host_key_trust.rs::verify`'s docs), so a plain blocking stdin read is
/// safe here.
///
/// Deliberately does **not** gate on `std::io::IsTerminal` (a real Windows
/// CI regression: an earlier version of this guard refused *any* non-tty
/// stdin, which broke every e2e test's — and any real non-interactive
/// automation's — legitimate pattern of piping a real answer to this
/// prompt, since a piped stdin is never a terminal even when something on
/// the other end genuinely is answering it). This function is only ever
/// reached in contexts `always-connects.md` already documents as exempt
/// (a genuinely new host key needs a human) — the caller that actually
/// needs a non-interactive, never-prompt guarantee
/// (`TofuConfirmation::Silent`'s `bootstrap_and_register` re-deploy) gets
/// it further down the stack, via
/// `isekai_bootstrap::RusshBackend::with_unattended_new_host_policy`
/// (installed by `native::bootstrap_backend::default_bootstrap_backend`
/// when `silent` is true), not by refusing to read stdin here.
fn prompt_new_host_confirmation(host_port: &str, fingerprint: &str) -> bool {
    use std::io::Write as _;
    eprint!(
        "The authenticity of host '{host_port}' can't be established.\n\
         Key fingerprint is {fingerprint}.\n\
         Are you sure you want to continue connecting (yes/no)? "
    );
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "yes" | "y" | "Y")
}

/// Bundles the two callbacks [`connect_and_authenticate`] needs for
/// authentication methods that may require live user input: a
/// passphrase-protected identity file, and `keyboard-interactive` (PAM/OTP/2FA
/// servers that don't negotiate plain `password`). Both are built once by
/// `run_authenticated_session` and, when built for a silent/automated retry,
/// both refuse to prompt at all rather than block on a live-but-answerless
/// stdin — the same seam `confirm_new_host` already uses for host-key TOFU
/// confirmation. Bundled into one struct instead of two more `&dyn Fn`
/// parameters purely to keep [`connect_and_authenticate`]'s already-long
/// signature from growing further.
struct InteractivePrompts<'a> {
    passphrase: &'a (dyn Fn(&Path, u32) -> Option<String> + Send + Sync),
    keyboard_interactive: &'a (dyn Fn(&[KeyboardInteractivePrompt]) -> Vec<String> + Send + Sync),
    /// Already-decrypted identities (Phase 1b passphrase hand-off, see
    /// `super::mux::handoff`'s docs) — `connect_and_authenticate`'s candidate
    /// loop consults this *before* the on-disk `SessionError::EncryptedPrivateKey`
    /// path, so a holder mode session (which forces `silent = true` and would
    /// otherwise just skip every encrypted candidate) can still authenticate
    /// with one. Empty (never populated) for every non-holder, non-fallback
    /// path.
    handoff: &'a HandoffCredentials,
}

/// Establishes the SSH handshake over `stream` and authenticates as
/// `username`, trying (in order) *every* configured/default private key from
/// `host_config::identity_file`/the default `id_ed25519`→`id_rsa`→`id_ecdsa`
/// probe, then an SSH agent (Windows-only — see [`agent_auth::connect_agent`]).
///
/// Like real `ssh(1)`, each configured identity is offered in turn: a key
/// the server *rejects* (`Ok(false)`) or one that fails to *parse*
/// (`SessionError::InvalidPrivateKey`, e.g. a passphrase-protected key —
/// M1's documented non-compat case) just moves on to the next candidate, and
/// then to the SSH-agent fallback, rather than aborting the whole
/// authentication (Codex review finding: the old code tried only the first
/// *existing* file, and a parse failure there propagated straight out,
/// skipping both the remaining keys and the agent entirely). Only a genuine
/// transport/protocol error (any other `SessionError`) aborts — those are not
/// "try the next key" situations.
///
/// Deliberately generic over `stream`/`verifier` so it's testable against an
/// in-process mock SSH server without a real `isekai-pipe connect`
/// subprocess or trust store — the same technique every other `native/*.rs`
/// module in this crate uses. Everything in [`connect_attempt`] above this
/// call (real subprocess, real trust store, real terminal I/O) is not
/// unit-tested.
async fn connect_and_authenticate<S, V>(
    stream: S,
    username: &str,
    host_config: &openssh_config::HostConfig,
    verifier: &Arc<V>,
    forward_routes: &ForwardRoutes,
    prompts: &InteractivePrompts<'_>,
) -> Result<client::Handle<russh_stream_session::VerifyingHandler<V>>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    V: russh_stream_session::HostKeyVerifier + 'static,
{
    // Never surfaced past this function (the `?` a few lines below either
    // consumes it into the returned error or it's dropped unused on
    // success) — simplification (Codex review finding): earlier this was a
    // caller-supplied parameter every call site had to construct and pass
    // just to satisfy the signature, even though no caller ever inspected
    // it afterward.
    let rejection = RejectionReason::new();
    let mut config = client::Config::default();
    config.keepalive_interval = Some(std::time::Duration::from_secs(60));
    config.keepalive_max = 3;
    let config = Arc::new(config);
    // Install the ctl-socket route table on the handler so server-initiated
    // `forwarded-streamlocal` channels (from `streamlocal_forward` below) are
    // delivered in-process. Harmless (and unused) when ctl-socket is off — no
    // forward is ever requested, so no channel is ever routed. Also installs
    // `rejection` so a host-key rejection's reason (a plain `bool` alone
    // can't carry — see `RejectionReason`'s docs) survives past this
    // function's `?` into the caller's error context.
    let handler = verifying_handler_with_routes_and_reason(verifier, forward_routes, &rejection);
    let mut handle = establish_over_stream(config, stream, handler).await.map_err(|e| {
        let base = anyhow::Error::from(e);
        match rejection.take() {
            Some(reason) => base.context(reason),
            None => base,
        }
    })?;

    let home = isekai_fs_guard::resolve_home_dir().unwrap_or_else(|| PathBuf::from("."));
    let candidates = private_key::identity_file_candidates(&host_config.identity_file, &home);
    log_line!(
        "isekai-ssh: identity_file candidates (home={}): {:?}",
        home.display(),
        candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
    );
    // Read + authenticate one candidate at a time, lazily (Codex review
    // finding): an unreadable/unusable candidate — missing, permission-denied,
    // or unparseable — is skipped, never fatal, so it can't block a
    // perfectly-good candidate listed before *or* after it, nor the SSH-agent
    // fallback below. Only a genuine transport/protocol error aborts.
    for (index, candidate) in candidates.iter().enumerate() {
        // A passphrase hand-off entry (Phase 1b) means this exact candidate
        // was already decrypted — by the process that spawned *this* one, if
        // it's a holder — so it's used directly, skipping the on-disk read
        // and the `EncryptedPrivateKey`/prompt path below entirely (a holder
        // can't prompt anyway; see `InteractivePrompts::handoff`'s docs).
        let (credential, already_decrypted) = if let Some(handed_off) = prompts.handoff.get(candidate) {
            let credential = match &handed_off.certificate_pem {
                Some(cert) => Credential::PublicKeyWithCertificate { private_key_pem: handed_off.private_key_pem.to_vec(), certificate_pem: cert.clone() },
                None => Credential::PublicKey { private_key_pem: handed_off.private_key_pem.to_vec() },
            };
            (credential, true)
        } else {
            // `read_credential_with_certificate` also resolves a paired
            // `CertificateFile` (an explicit one positionally, else `ssh(1)`'s
            // own `<candidate>-cert.pub` default convention) and upgrades to
            // `Credential::PublicKeyWithCertificate` when one is found and
            // readable — see its own docs for the pairing rules.
            let Some(credential) = private_key::read_credential_with_certificate(host_config, candidate, index) else {
                log_line!("isekai-ssh: no key at {}", candidate.display());
                continue;
            };
            (credential, false)
        };
        log_line!("isekai-ssh: trying key {}", candidate.display());
        match authenticate_session(&mut handle, username, &credential).await {
            Ok(true) => return Ok(handle),
            Ok(false) => continue,
            Err(SessionError::InvalidPrivateKey(_) | SessionError::InvalidCertificate(_)) => continue,
            // Unreachable in practice: a hand-off entry is already cleartext,
            // so `authenticate_session` would never re-report it as
            // encrypted. Guarded explicitly anyway rather than assumed, so a
            // future bug in the hand-off's own decrypt step fails safe (skips
            // this candidate) instead of looping.
            Err(SessionError::EncryptedPrivateKey) if already_decrypted => continue,
            Err(SessionError::EncryptedPrivateKey) => {
                let (private_key_pem, certificate_pem): (&[u8], Option<&[u8]>) = match &credential {
                    Credential::PublicKey { private_key_pem } => (private_key_pem, None),
                    Credential::PublicKeyWithCertificate { private_key_pem, certificate_pem } => {
                        (private_key_pem, Some(certificate_pem))
                    }
                    Credential::Password(_) => continue, // unreachable: EncryptedPrivateKey never comes from a password
                };
                if try_encrypted_identity(&mut handle, username, candidate, private_key_pem, certificate_pem, prompts.passphrase)
                    .await?
                {
                    return Ok(handle);
                }
            }
            Err(e) => return Err(anyhow::Error::new(e).context("SSH authentication request failed")),
        }
    }

    if try_agent_auth(&mut handle, username, host_config).await? {
        return Ok(handle);
    }

    // Last resort: keyboard-interactive (PAM/OTP/2FA-style servers that don't
    // negotiate plain `password` at all). Tried after every key and the agent
    // since it's the one method that's neither "prove possession of a key
    // file" nor silent — matches `ssh(1)`'s own `PreferredAuthentications`
    // ordering (publickey before keyboard-interactive/password).
    if authenticate_keyboard_interactive(&mut handle, username, |server_prompts| {
        (prompts.keyboard_interactive)(server_prompts)
    })
    .await
    .map_err(|e| anyhow::Error::new(e).context("SSH authentication request failed"))?
    {
        return Ok(handle);
    }

    Err(anyhow!(
        "no configured private key or SSH agent identity was accepted for {username}"
    ))
}

/// Handles one candidate identity file that [`authenticate_session`] has
/// reported as passphrase-protected (`SessionError::EncryptedPrivateKey`).
/// Prompts (via `prompt_passphrase`) up to 3 times — matching `ssh(1)`'s own
/// passphrase-retry convention — trying [`authenticate_publickey_with_passphrase`]
/// (or, when `certificate_pem` is `Some`, [`authenticate_openssh_cert_with_passphrase`]
/// instead) with each answer. `prompt_passphrase` returning `None` (the caller
/// gave up, or — in silent/automated mode — refuses to prompt at all, see
/// `run_authenticated_session`'s construction of it) stops the retry loop
/// immediately and moves on to the next candidate/the SSH agent, exactly like
/// exhausting the retry count does.
async fn try_encrypted_identity<H: client::Handler>(
    handle: &mut client::Handle<H>,
    username: &str,
    path: &Path,
    private_key_pem: &[u8],
    certificate_pem: Option<&[u8]>,
    prompt_passphrase: &(dyn Fn(&Path, u32) -> Option<String> + Send + Sync),
) -> Result<bool> {
    for attempt in 1..=3 {
        let Some(passphrase) = prompt_passphrase(path, attempt) else {
            return Ok(false);
        };
        let result = match certificate_pem {
            Some(cert) => authenticate_openssh_cert_with_passphrase(handle, username, private_key_pem, cert, &passphrase).await,
            None => authenticate_publickey_with_passphrase(handle, username, private_key_pem, &passphrase).await,
        };
        match result {
            Ok(true) => return Ok(true),
            Ok(false) => return Ok(false), // server rejected the (successfully decrypted) key/cert — not a passphrase problem
            // Wrong passphrase (or truly malformed key/cert): retry with a fresh prompt.
            Err(SessionError::InvalidPrivateKey(_) | SessionError::InvalidCertificate(_)) => continue,
            Err(e) => return Err(anyhow::Error::new(e).context("SSH authentication request failed")),
        }
    }
    log_line!("isekai-ssh: too many failed passphrase attempts for {}", path.display());
    Ok(false)
}

#[cfg(windows)]
async fn try_agent_auth<H: client::Handler>(
    handle: &mut client::Handle<H>,
    username: &str,
    host_config: &openssh_config::HostConfig,
) -> Result<bool> {
    let target = agent_auth::resolve_agent_target(host_config.identity_agent.as_deref());
    let mut agent = match agent_auth::connect_agent(&target).await {
        Ok(Some(agent)) => agent,
        Ok(None) => return Ok(false),
        // A default agent that isn't running is the normal Windows state, not
        // a configuration error: swallow its connect failure so we reach the
        // final, actionable "no ... identity was accepted" message instead of
        // a confusing error about an agent the user never set up. An explicit
        // `IdentityAgent <path>` keeps its hard error (see
        // `agent_auth::agent_connect_failure_is_benign`).
        Err(_) if agent_auth::agent_connect_failure_is_benign(&target) => return Ok(false),
        Err(e) => return Err(e),
    };
    let identities = agent.request_identities().await.context("failed to list SSH agent identities")?;
    Ok(agent_auth::try_each_identity(handle, username, &identities, &mut agent).await?)
}

/// Non-Windows builds have no agent transport wired up yet
/// (`agent_auth::connect_agent` is `cfg(windows)`-only — see its docs) —
/// this stub exists purely so [`connect_and_authenticate`] compiles and is
/// unit-testable on Linux too; it's never reached from a real `run()` call
/// since `main.rs` only dispatches to this module on `cfg(windows)`.
#[cfg(not(windows))]
async fn try_agent_auth<H: client::Handler>(
    _handle: &mut client::Handle<H>,
    _username: &str,
    _host_config: &openssh_config::HostConfig,
) -> Result<bool> {
    Ok(false)
}

/// Relays bytes between the local terminal (raw mode, already enabled by
/// the caller) and the remote shell channel until the channel closes,
/// returning the remote exit status as this process's own exit code (`ssh(1)`'s
/// own convention) — or **255** if the channel closed without ever sending
/// one (Codex review finding: an abnormal disconnect — network loss, the
/// `isekai-pipe connect` child dying — must not be reported as a successful
/// exit just because `exit_code`'s initial value happened to be `0`; `255`
/// matches real `ssh(1)`'s own exit code for "connection lost/could not
/// execute command"). Local stdin EOF (Ctrl-D redirected from a non-tty, or
/// a real EOF) sends a channel EOF rather than closing the channel outright,
/// so any buffered remote output still in flight is not lost.
///
/// Propagates local terminal resize events to the remote PTY via
/// `channel.window_change` (when a resize watcher is available — spawned by
/// the real-terminal path; tests pass `None`).
/// Also handles `ssh(1)`-style escape sequences (`~.` to disconnect,
/// `~~` for a literal tilde, `~?` for help, `~^Z` to suspend).
/// Delegates to [`run_shell_io_loop_inner`] (which the tests drive against
/// in-memory buffers) with the real local `stdin`/`stdout`/`stderr`; driving
/// a real terminal stdin/stdout pair isn't practical in a unit test.
async fn run_shell_io_loop(channel: &mut russh::Channel<client::Msg>) -> Result<u8> {
    let resize_rx = console::spawn_resize_watcher();
    run_shell_io_loop_inner(channel, console_stdin::ConsoleStdin::open(), tokio::io::stdout(), tokio::io::stderr(), resize_rx).await
}

/// The body of [`run_shell_io_loop`] with the three local streams plus an
/// optional resize event channel injected, so tests can substitute in-memory
/// buffers for a real terminal. `stdin` feeds the remote channel; remote
/// `Data` goes to `stdout` and remote `ExtendedData` (stderr) goes to
/// `stderr` — real `ssh(1)` keeps the two separate (Codex review finding:
/// they were both written to `stdout`).
///
/// When `resize_rx` is `Some`, local terminal resize events are forwarded to
/// the remote PTY via `channel.window_change` in a third `select!` branch.
/// `ssh(1)`-style escape sequences (`~.` to disconnect, `~~` for a literal
/// tilde, `~?` for help, `~^Z` to suspend on Unix) are also detected in the
/// stdin path.
async fn run_shell_io_loop_inner<I, O, E>(
    channel: &mut russh::Channel<client::Msg>,
    mut stdin: I,
    mut stdout: O,
    mut stderr: E,
    mut resize_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(u32, u32)>>,
) -> Result<u8>
where
    I: tokio::io::AsyncRead + Unpin,
    O: tokio::io::AsyncWrite + Unpin,
    E: tokio::io::AsyncWrite + Unpin,
{
    /// `ssh(1)`'s own exit code for "the connection was lost, or the remote
    /// command couldn't be run" — used here when the channel closes without
    /// ever delivering a `ChannelMsg::ExitStatus`.
    const NO_EXIT_STATUS_RECEIVED: u8 = 255;

    let mut buf = [0u8; 8192];
    let mut exit_code: Option<u8> = None;
    let mut stdin_open = true;

    // Escape sequence state: `ssh(1)`-style `~` commands only at the start of
    // a line (after `\r` or `\n`). `pending_escape` means the previous byte
    // was `~` at line start and we're waiting for the command character.
    let mut at_line_start = true;
    let mut pending_escape = false;

    loop {
        tokio::select! {
            n = stdin.read(&mut buf), if stdin_open => {
                match n {
                    Ok(0) => {
                        stdin_open = false;
                        let _ = channel.eof().await;
                    }
                    Ok(n) => {
                        let (to_send, action) = process_stdin_bytes(&buf[..n], &mut at_line_start, &mut pending_escape);
                        if !to_send.is_empty() {
                            if channel.data(&to_send[..]).await.is_err() {
                                break;
                            }
                        }
                        match action {
                            EscapeAction::Disconnect => break,
                            EscapeAction::Suspend => {
                                #[cfg(unix)]
                                {
                                    // SAFETY: `SIGTSTP` is a standard POSIX signal; raising it
                                    // from the foreground process group is the same thing
                                    // `ssh(1)`'s `~^Z` does.
                                    unsafe { libc::raise(libc::SIGTSTP); }
                                }
                                #[cfg(not(unix))]
                                {
                                    let _ = stderr.write_all(b"\r\n~^Z (suspend) is not supported on this platform.\r\n").await;
                                }
                            }
                            EscapeAction::Help => {
                                let _ = stderr.write_all(
                                    b"\r\nSupported escape sequences:\r\n\
                                      ~.  - terminate connection\r\n\
                                      ~^Z - suspend isekai-ssh\r\n\
                                      ~~  - send a literal tilde\r\n\
                                      ~?  - this message\r\n\
                                      ~#  - list forwarded connections (not yet)\r\n"
                                ).await;
                            }
                            EscapeAction::None => {}
                        }
                    }
                    Err(_) => {
                        stdin_open = false;
                        let _ = channel.eof().await;
                    }
                }
            }
            msg = channel.wait() => {
                match msg {
                    Some(russh::ChannelMsg::Data { data }) => {
                        let _ = stdout.write_all(&data).await;
                        let _ = stdout.flush().await;
                    }
                    Some(russh::ChannelMsg::ExtendedData { data, .. }) => {
                        let _ = stderr.write_all(&data).await;
                        let _ = stderr.flush().await;
                    }
                    Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                        exit_code = Some(exit_status as u8);
                    }
                    // A server may legally send `CHANNEL_EOF` *before* the
                    // `exit-status` channel request — RFC 4254 doesn't mandate
                    // the order (Codex review finding). Breaking on `Eof` here
                    // would drop a still-pending `ExitStatus` and mis-report a
                    // successful command as 255, so `Eof` is a no-op (via the
                    // catch-all below): data never arrives after it, but
                    // `ExitStatus` still can. Only `Close`/`None` — the channel
                    // truly ending — break the loop.
                    Some(russh::ChannelMsg::Close) | None => break,
                    _ => {}
                }
            }
            resize = recv_resize(&mut resize_rx) => {
                if let Some((cols, rows)) = resize {
                    let _ = channel.window_change(cols, rows, 0, 0).await;
                }
            }
        }
    }

    Ok(exit_code.unwrap_or(NO_EXIT_STATUS_RECEIVED))
}

/// `recv` on the optional resize channel, or a future that never resolves
/// when there is no watcher (so the `select!` branch is inert).
async fn recv_resize(
    rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<(u32, u32)>>,
) -> Option<(u32, u32)> {
    match rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod recv_resize_tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn recv_resize_with_none_never_resolves() {
        // When the channel is None, recv_resize should return pending forever.
        let mut rx: Option<mpsc::UnboundedReceiver<(u32, u32)>> = None;
        let result = tokio::time::timeout(std::time::Duration::from_millis(10), recv_resize(&mut rx)).await;
        assert!(result.is_err(), "recv_resize with None should never resolve (timeout expected)");
    }

    #[tokio::test]
    async fn recv_resize_with_some_receives_value() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut rx = Some(rx);
        tx.send((120, 40)).unwrap();
        let result = recv_resize(&mut rx).await;
        assert_eq!(result, Some((120, 40)));
    }

    #[tokio::test]
    async fn recv_resize_with_some_returns_none_when_sender_dropped() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut rx = Some(rx);
        drop(tx);
        let result = recv_resize(&mut rx).await;
        assert_eq!(result, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
    use russh::{Channel as RusshChannel, CryptoVec};
    use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey as SshPrivateKey};
    use russh_keys::ssh_key::{LineEnding, PublicKey as SshPublicKey};
    use russh_stream_session::{verifying_handler, Credential, HostKeyVerifier, VerifyOutcome};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    /// A [`InteractivePrompts`] that never prompts (passphrase: `None`;
    /// keyboard-interactive: no answers) — the right default for every test
    /// that isn't itself exercising one of these two interactive paths, same
    /// as a silent/automated retry in production.
    fn no_passphrase_prompt(_path: &Path, _attempt: u32) -> Option<String> {
        None
    }
    fn no_kbi_responder(_prompts: &[KeyboardInteractivePrompt]) -> Vec<String> {
        Vec::new()
    }
    fn empty_handoff() -> &'static HandoffCredentials {
        static EMPTY: std::sync::OnceLock<HandoffCredentials> = std::sync::OnceLock::new();
        EMPTY.get_or_init(HandoffCredentials::default)
    }
    fn no_interactive_prompts() -> InteractivePrompts<'static> {
        InteractivePrompts { passphrase: &no_passphrase_prompt, keyboard_interactive: &no_kbi_responder, handoff: empty_handoff() }
    }

    struct AcceptAllHostKeys;
    #[async_trait]
    impl HostKeyVerifier for AcceptAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> VerifyOutcome {
            VerifyOutcome::Accepted
        }
    }

    struct RejectAllHostKeys;
    #[async_trait]
    impl HostKeyVerifier for RejectAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> VerifyOutcome {
            VerifyOutcome::Rejected("rejected by test double".to_string())
        }
    }

    #[derive(Clone)]
    struct PasswordServer {
        accepted_password: String,
    }

    impl server::Server for PasswordServer {
        type Handler = PasswordHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> PasswordHandler {
            PasswordHandler { accepted_password: self.accepted_password.clone() }
        }
    }

    #[derive(Clone)]
    struct PasswordHandler {
        accepted_password: String,
    }

    #[async_trait]
    impl server::Handler for PasswordHandler {
        type Error = russh::Error;

        async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
            Ok(if password == self.accepted_password { Auth::Accept } else { Auth::Reject { proceed_with_methods: None } })
        }

        async fn channel_open_session(
            &mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
    }

    /// Accepts *only* keyboard-interactive (rejects password/publickey
    /// outright), with a single non-echoed prompt — stands in for a
    /// PAM-backed sshd that doesn't negotiate plain `password` at all, so
    /// `connect_and_authenticate` must fall all the way through to its
    /// keyboard-interactive last resort.
    #[derive(Clone)]
    struct KeyboardInteractiveOnlyServer {
        accepted_answer: String,
    }

    impl server::Server for KeyboardInteractiveOnlyServer {
        type Handler = KeyboardInteractiveOnlyHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> KeyboardInteractiveOnlyHandler {
            KeyboardInteractiveOnlyHandler { accepted_answer: self.accepted_answer.clone() }
        }
    }

    #[derive(Clone)]
    struct KeyboardInteractiveOnlyHandler {
        accepted_answer: String,
    }

    #[async_trait]
    impl server::Handler for KeyboardInteractiveOnlyHandler {
        type Error = russh::Error;

        async fn auth_keyboard_interactive(
            &mut self, _user: &str, _submethods: &str, response: Option<server::Response<'async_trait>>,
        ) -> Result<Auth, Self::Error> {
            match response {
                None => Ok(Auth::Partial {
                    name: "".into(),
                    instructions: "".into(),
                    prompts: std::borrow::Cow::Owned(vec![("Password: ".into(), false)]),
                }),
                Some(resp) => {
                    let answers: Vec<Vec<u8>> = resp.map(|b| b.to_vec()).collect();
                    let accepted = answers.first().map(|a| a.as_slice()) == Some(self.accepted_answer.as_bytes());
                    Ok(if accepted { Auth::Accept } else { Auth::Reject { proceed_with_methods: None } })
                }
            }
        }

        async fn channel_open_session(
            &mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
    }

    /// Writes a fresh ed25519 OpenSSH private key (deterministic from `seed`)
    /// to `dir/name` and returns its path plus the matching public key — so a
    /// test can point `HostConfig::identity_file` at a real key file and
    /// configure a server to accept (only) that key.
    fn write_ed25519_identity(dir: &Path, name: &str, seed: u8) -> (PathBuf, SshPublicKey) {
        let keypair = Ed25519Keypair::from_seed(&[seed; 32]);
        let private = SshPrivateKey::from(keypair);
        let pem = private.to_openssh(LineEnding::LF).expect("serialize ed25519 key to OpenSSH PEM");
        let path = dir.join(name);
        std::fs::write(&path, pem.as_bytes()).unwrap();
        (path, private.public_key().clone())
    }

    /// Writes an ed25519 identity file (like [`write_ed25519_identity`]) plus
    /// a certificate for it, signed by a freshly generated CA key, at the
    /// `ssh(1)`-default `<name>-cert.pub` sibling path — so a test can prove
    /// `read_credential_with_certificate`'s default-discovery convention
    /// (no explicit `CertificateFile` configured) actually authenticates
    /// end-to-end. Returns the identity path and the CA's public key (to
    /// configure a server that trusts it).
    fn write_ed25519_identity_with_default_cert(dir: &Path, name: &str, seed: u8, ca_seed: u8) -> (PathBuf, SshPublicKey) {
        let (identity_path, _subject_public) = write_ed25519_identity(dir, name, seed);
        let subject_private = SshPrivateKey::from_openssh(&std::fs::read(&identity_path).unwrap()).unwrap();
        let ca_key = SshPrivateKey::from(Ed25519Keypair::from_seed(&[ca_seed; 32]));

        let mut builder = russh_keys::ssh_key::certificate::Builder::new_with_random_nonce(
            &mut rand::rngs::OsRng,
            subject_private.public_key().key_data().clone(),
            0,
            u32::MAX as u64,
        )
        .unwrap();
        builder.cert_type(russh_keys::ssh_key::certificate::CertType::User).unwrap();
        builder.valid_principal("tester").unwrap();
        let cert = builder.sign(&ca_key).unwrap();

        let mut cert_path = identity_path.as_os_str().to_owned();
        cert_path.push("-cert.pub");
        std::fs::write(PathBuf::from(cert_path), cert.to_openssh().unwrap().as_bytes()).unwrap();

        (identity_path, ca_key.public_key().clone())
    }

    /// Accepts an OpenSSH certificate authentication iff it's signed by
    /// `trusted_ca` (mirrors `russh_stream_session`'s own `CertificateServer`
    /// test double). Rejects plain publickey/password outright, so a test
    /// using this server proves the connection actually went through the
    /// certificate path, not a lucky plain-pubkey fallback.
    #[derive(Clone)]
    struct CertificateOnlyServer {
        trusted_ca: SshPublicKey,
    }

    impl server::Server for CertificateOnlyServer {
        type Handler = CertificateOnlyHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> CertificateOnlyHandler {
            CertificateOnlyHandler { trusted_ca: self.trusted_ca.clone() }
        }
    }

    #[derive(Clone)]
    struct CertificateOnlyHandler {
        trusted_ca: SshPublicKey,
    }

    #[async_trait]
    impl server::Handler for CertificateOnlyHandler {
        type Error = russh::Error;

        async fn auth_openssh_certificate(
            &mut self, _user: &str, certificate: &russh_keys::ssh_key::Certificate,
        ) -> Result<Auth, Self::Error> {
            Ok(if certificate.signature_key() == self.trusted_ca.key_data() {
                Auth::Accept
            } else {
                Auth::Reject { proceed_with_methods: None }
            })
        }

        async fn channel_open_session(
            &mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
    }

    #[tokio::test]
    async fn connect_and_authenticate_succeeds_via_the_default_discovered_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let (identity_path, ca_public) = write_ed25519_identity_with_default_cert(dir.path(), "id_ed25519", 230, 231);
        let addr = spawn_server(CertificateOnlyServer { trusted_ca: ca_public }, 230).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        // No CertificateFile configured explicitly: this must be found via
        // ssh(1)'s own default `<identity>-cert.pub` discovery convention.
        let host_config = openssh_config::HostConfig { identity_file: vec![identity_path], ..Default::default() };

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier, &ForwardRoutes::new(), &no_interactive_prompts()).await;
        assert!(
            result.is_ok(),
            "the default-discovered certificate must authenticate: {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    /// Like [`write_ed25519_identity_with_default_cert`], but encrypts the
    /// identity key with `passphrase` — so a test can prove the
    /// encrypted-key retry path (`try_encrypted_identity`) correctly carries
    /// the paired certificate through, not just plain pubkey auth.
    fn write_encrypted_ed25519_identity_with_default_cert(
        dir: &Path, name: &str, seed: u8, ca_seed: u8, passphrase: &str,
    ) -> (PathBuf, SshPublicKey) {
        let (identity_path, ca_public) = write_ed25519_identity_with_default_cert(dir, name, seed, ca_seed);
        let plain = SshPrivateKey::from_openssh(&std::fs::read(&identity_path).unwrap()).unwrap();
        let encrypted = plain.encrypt(&mut rand::rngs::OsRng, passphrase).unwrap();
        std::fs::write(&identity_path, encrypted.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
        (identity_path, ca_public)
    }

    #[tokio::test]
    async fn connect_and_authenticate_succeeds_via_an_encrypted_identity_with_a_paired_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let (identity_path, ca_public) =
            write_encrypted_ed25519_identity_with_default_cert(dir.path(), "id_ed25519", 232, 233, "hunter2");
        let addr = spawn_server(CertificateOnlyServer { trusted_ca: ca_public }, 231).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let host_config = openssh_config::HostConfig { identity_file: vec![identity_path], ..Default::default() };
        let passphrase_prompt = |_path: &Path, _attempt: u32| Some("hunter2".to_string());
        let prompts = InteractivePrompts { passphrase: &passphrase_prompt, keyboard_interactive: &no_kbi_responder, handoff: empty_handoff() };

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier, &ForwardRoutes::new(), &prompts).await;
        assert!(
            result.is_ok(),
            "the right passphrase must decrypt the key and authenticate via its paired certificate: {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    /// Like [`write_ed25519_identity`], but encrypts the key with `passphrase`
    /// first — a real encrypted OpenSSH key, exercising the same
    /// `is_encrypted`/`decrypt` code path a real user's passphrase-protected
    /// `~/.ssh/id_ed25519` would.
    fn write_encrypted_ed25519_identity(dir: &Path, name: &str, seed: u8, passphrase: &str) -> (PathBuf, SshPublicKey) {
        let keypair = Ed25519Keypair::from_seed(&[seed; 32]);
        let private = SshPrivateKey::from(keypair);
        let public = private.public_key().clone();
        let encrypted = private.encrypt(&mut rand::rngs::OsRng, passphrase).expect("encrypt a freshly generated key");
        let pem = encrypted.to_openssh(LineEnding::LF).expect("serialize encrypted ed25519 key to OpenSSH PEM");
        let path = dir.join(name);
        std::fs::write(&path, pem.as_bytes()).unwrap();
        (path, public)
    }

    /// Accepts publickey auth for exactly one configured public key (rejecting
    /// every other key), and accepts session-channel opens. Lets a test prove
    /// `connect_and_authenticate` offers each configured identity in turn: a
    /// first key this server rejects must not stop the (accepted) second one.
    #[derive(Clone)]
    struct AcceptOneKeyServer {
        accepted: SshPublicKey,
    }

    impl server::Server for AcceptOneKeyServer {
        type Handler = AcceptOneKeyHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> AcceptOneKeyHandler {
            AcceptOneKeyHandler { accepted: self.accepted.clone() }
        }
    }

    #[derive(Clone)]
    struct AcceptOneKeyHandler {
        accepted: SshPublicKey,
    }

    #[async_trait]
    impl server::Handler for AcceptOneKeyHandler {
        type Error = russh::Error;

        async fn auth_publickey(&mut self, _user: &str, public_key: &SshPublicKey) -> Result<Auth, Self::Error> {
            Ok(if public_key.key_data() == self.accepted.key_data() {
                Auth::Accept
            } else {
                Auth::Reject { proceed_with_methods: None }
            })
        }

        async fn channel_open_session(
            &mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
    }

    /// On a shell request, sends stdout data, then stderr (extended) data,
    /// then `CHANNEL_EOF`, then the `exit-status` request, then `close` — in
    /// that exact order. Exercises two Codex review findings at once: (1) the
    /// client must not break on `Eof` and lose the `exit-status` that legally
    /// follows it (RFC 4254 leaves their order unspecified), and (2) extended
    /// (stderr) data must be routed to local stderr, not stdout. Sends
    /// synchronously from `shell_request` (like `russh-stream-session`'s own
    /// `EchoExecServer`) so the on-wire ordering is deterministic.
    #[derive(Clone)]
    struct EofThenExitStatusServer;

    impl server::Server for EofThenExitStatusServer {
        type Handler = EofThenExitStatusHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> EofThenExitStatusHandler {
            EofThenExitStatusHandler
        }
    }

    #[derive(Clone)]
    struct EofThenExitStatusHandler;

    #[async_trait]
    impl server::Handler for EofThenExitStatusHandler {
        type Error = russh::Error;

        async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }

        async fn shell_request(&mut self, channel: russh::ChannelId, session: &mut ServerSession) -> Result<(), Self::Error> {
            session.data(channel, CryptoVec::from(b"hello-stdout".to_vec()))?;
            session.extended_data(channel, 1, CryptoVec::from(b"hello-stderr".to_vec()))?;
            session.eof(channel)?;
            session.exit_status_request(channel, 42)?;
            session.close(channel)?;
            Ok(())
        }
    }

    /// Accepts any password and any channel open, then closes the channel
    /// the moment a shell is requested — without ever sending
    /// `ChannelMsg::ExitStatus` first — standing in for an abnormal
    /// disconnect (network loss, the `isekai-pipe connect` child dying
    /// mid-session) rather than a clean remote shell exit. Deliberately does
    /// **not** close from `channel_open_session` itself: that runs before
    /// russh has sent the channel-open confirmation back to the client, so a
    /// close issued there races the confirmation and the client can hang
    /// waiting for a channel it never learns is open (this was tried first
    /// and produced exactly that hang) — `shell_request` only fires after
    /// the channel is genuinely established, matching how
    /// `russh-stream-session`'s own `EchoExecServer` test closes from
    /// `exec_request`, not `channel_open_session`.
    #[derive(Clone)]
    struct CloseWithoutExitStatusServer;

    impl server::Server for CloseWithoutExitStatusServer {
        type Handler = CloseWithoutExitStatusHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> CloseWithoutExitStatusHandler {
            CloseWithoutExitStatusHandler
        }
    }

    #[derive(Clone)]
    struct CloseWithoutExitStatusHandler;

    #[async_trait]
    impl server::Handler for CloseWithoutExitStatusHandler {
        type Error = russh::Error;

        async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }

        async fn shell_request(&mut self, channel: russh::ChannelId, session: &mut ServerSession) -> Result<(), Self::Error> {
            session.close(channel)?;
            Ok(())
        }
    }

    async fn spawn_server<S, H>(mut server: S, seed: u8) -> SocketAddr
    where
        S: server::Server<Handler = H> + Send + 'static,
        H: server::Handler + Send + 'static,
    {
        let keypair = Ed25519Keypair::from_seed(&[seed; 32]);
        let host_key = SshPrivateKey::from(keypair);
        let config = Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });
        addr
    }

    /// README regression test (the stale README §"対応していない・非互換の事項" used to
    /// claim `isekai-ssh <host> 'cmd'` was unwired on the native path and always opened
    /// an interactive shell instead — it has actually dispatched to
    /// `SessionKind::Exec` since this was written; this locks the decision in so it
    /// can't silently regress back to always-Shell without a failing test).
    #[test]
    fn decide_session_kind_uses_exec_for_a_command_with_no_pty() {
        let cmd = vec!["echo".to_string(), "hi".to_string()];
        let kind = decide_session_kind(Some(&cmd), crate::wrapper::RequestTty::No, "xterm", 80, 24, &[]);
        match kind {
            SessionKind::Exec { command } => assert_eq!(command, "echo hi"),
            SessionKind::Shell { .. } => panic!("-T with a remote command must skip the PTY (SessionKind::Exec)"),
        }
    }

    /// `Auto`/`Yes`/`Force` all open a PTY+shell even when a command is present — the
    /// command itself is `exec`'d over that PTY separately by the caller (see the
    /// `remote_cmd`/`request_tty` check right after the channel opens in
    /// `run_authenticated_session`), since `SessionKind::Exec` never allocates a PTY.
    #[test]
    fn decide_session_kind_uses_shell_for_a_command_with_a_pty_requested() {
        let cmd = vec!["echo".to_string(), "hi".to_string()];
        for request_tty in [crate::wrapper::RequestTty::Auto, crate::wrapper::RequestTty::Yes, crate::wrapper::RequestTty::Force] {
            let kind = decide_session_kind(Some(&cmd), request_tty, "xterm", 80, 24, &[]);
            assert!(
                matches!(kind, SessionKind::Shell { .. }),
                "{request_tty:?} with a remote command must still allocate a PTY"
            );
        }
    }

    #[test]
    fn decide_session_kind_uses_shell_when_there_is_no_remote_command() {
        for request_tty in [crate::wrapper::RequestTty::Auto, crate::wrapper::RequestTty::Yes, crate::wrapper::RequestTty::No, crate::wrapper::RequestTty::Force] {
            let kind = decide_session_kind(None, request_tty, "xterm", 80, 24, &[]);
            assert!(matches!(kind, SessionKind::Shell { .. }), "no remote command always means an interactive shell");
        }
    }

    /// `connect_and_authenticate` has no private key or agent to offer in
    /// this test (no identity files exist at the tempdir `home` used, and
    /// there's no agent on Linux), so this only proves the "everything was
    /// tried and rejected" error path — the accept path is already covered
    /// end-to-end by `russh_stream_session`'s and `private_key.rs`'s own
    /// tests; wiring them together here would just re-test those crates'
    /// logic under a different name.
    #[tokio::test]
    async fn connect_and_authenticate_fails_cleanly_when_no_credential_is_available() {
        let addr = spawn_server(PasswordServer { accepted_password: "unused".to_string() }, 200).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let host_config = openssh_config::HostConfig::default();

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier, &ForwardRoutes::new(), &no_interactive_prompts()).await;
        assert!(result.is_err(), "no identity file and no agent means nothing to authenticate with");
    }

    #[tokio::test]
    async fn connect_and_authenticate_rejects_when_the_host_key_verifier_refuses() {
        let addr = spawn_server(PasswordServer { accepted_password: "unused".to_string() }, 201).await;
        let verifier = Arc::new(RejectAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let host_config = openssh_config::HostConfig::default();

        let Err(err) = connect_and_authenticate(stream, "tester", &host_config, &verifier, &ForwardRoutes::new(), &no_interactive_prompts()).await
        else {
            panic!("a rejected host key must fail the connection before any auth attempt");
        };
        // Regression test for the always-connects-review error-UX fix: the
        // verifier's rejection reason (not just a bare "Unknown server key"
        // from russh) must reach the top-level error chain.
        assert!(
            format!("{err:#}").contains("rejected by test double"),
            "expected the verifier's rejection reason in the error chain, got: {err:#}"
        );
    }

    /// Codex review finding: `run_shell_io_loop` used to initialize
    /// `exit_code` to `0` and only ever overwrite it on
    /// `ChannelMsg::ExitStatus` — a channel that closes abnormally (network
    /// loss, the `isekai-pipe connect` child dying) without ever sending one
    /// would silently report success. `CloseWithoutExitStatusServer` closes
    /// the channel immediately after opening it, before any exit status is
    /// ever sent, standing in for exactly that scenario.
    #[tokio::test]
    async fn run_shell_io_loop_reports_255_when_the_channel_closes_without_an_exit_status() {
        let addr = spawn_server(CloseWithoutExitStatusServer, 202).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let config = Arc::new(client::Config::default());
        let handler = verifying_handler(&verifier);
        let mut handle = establish_over_stream(config, stream, handler).await.unwrap();
        let authed = authenticate_session(&mut handle, "tester", &Credential::Password("unused".to_string()))
            .await
            .unwrap();
        assert!(authed, "CloseWithoutExitStatusServer accepts any password");

        let mut channel = open_channel(&handle, &SessionKind::Shell { term: "xterm".to_string(), cols: 80, rows: 24, terminal_modes: vec![] })
            .await
            .unwrap();

        let exit_code = run_shell_io_loop(&mut channel).await.unwrap();
        assert_eq!(exit_code, 255, "an abnormal disconnect must not be reported as a successful (0) exit");
    }

    /// Connects to `addr`, accepts its host key, authenticates with a
    /// throwaway password (the shell-request test servers accept any), and
    /// opens a shell channel. Returns the `handle` *and* the channel — the
    /// caller must keep `handle` alive for the duration of the I/O loop, else
    /// dropping it tears the session down mid-test.
    async fn open_authed_shell(
        addr: SocketAddr,
    ) -> (client::Handle<russh_stream_session::VerifyingHandler<AcceptAllHostKeys>>, russh::Channel<client::Msg>) {
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let config = Arc::new(client::Config::default());
        let handler = verifying_handler(&verifier);
        let mut handle = establish_over_stream(config, stream, handler).await.unwrap();
        let authed = authenticate_session(&mut handle, "tester", &Credential::Password("unused".to_string())).await.unwrap();
        assert!(authed, "the shell-request test server accepts any password");
        let channel = open_channel(&handle, &SessionKind::Shell { term: "xterm".to_string(), cols: 80, rows: 24, terminal_modes: vec![] })
            .await
            .unwrap();
        (handle, channel)
    }

    /// Codex review finding: a server may legally send `CHANNEL_EOF` before
    /// the `exit-status` request (RFC 4254 doesn't fix the order). The loop
    /// used to break on `Eof`, so it never saw the `ExitStatus` that followed
    /// and returned its 255 "no exit status" fallback even though the command
    /// actually exited 42. `EofThenExitStatusServer` sends eof *then*
    /// exit-status; the loop must report 42.
    #[tokio::test]
    async fn run_shell_io_loop_honors_exit_status_sent_after_eof() {
        let addr = spawn_server(EofThenExitStatusServer, 203).await;
        let (_handle, mut channel) = open_authed_shell(addr).await;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit_code = run_shell_io_loop_inner(&mut channel, tokio::io::empty(), &mut stdout, &mut stderr, None).await.unwrap();
        assert_eq!(exit_code, 42, "an exit-status arriving after CHANNEL_EOF must still be honored, not reported as 255");
    }

    /// Codex review finding: remote stderr (`ChannelMsg::ExtendedData`) was
    /// being written to local stdout — real `ssh(1)` keeps the two streams
    /// separate. `EofThenExitStatusServer` sends `hello-stdout` as data and
    /// `hello-stderr` as extended data; each must land on its own stream.
    #[tokio::test]
    async fn run_shell_io_loop_routes_extended_data_to_stderr_not_stdout() {
        let addr = spawn_server(EofThenExitStatusServer, 204).await;
        let (_handle, mut channel) = open_authed_shell(addr).await;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let _ = run_shell_io_loop_inner(&mut channel, tokio::io::empty(), &mut stdout, &mut stderr, None).await.unwrap();
        assert_eq!(stdout, b"hello-stdout", "remote stdout (Data) must land on local stdout");
        assert_eq!(stderr, b"hello-stderr", "remote stderr (ExtendedData) must land on local stderr, not stdout");
    }

    /// Codex review finding: only the first *existing* `IdentityFile` was ever
    /// tried, so a first key the server *rejects* blocked every later
    /// configured identity. Here the first key is present but unauthorized and
    /// the second is accepted — the connection must still succeed.
    #[tokio::test]
    async fn connect_and_authenticate_tries_the_next_identity_when_the_first_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (first_key, _first_pub) = write_ed25519_identity(dir.path(), "id_first", 71);
        let (second_key, second_pub) = write_ed25519_identity(dir.path(), "id_second", 72);
        let addr = spawn_server(AcceptOneKeyServer { accepted: second_pub }, 205).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let host_config = openssh_config::HostConfig { identity_file: vec![first_key, second_key], ..Default::default() };

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier, &ForwardRoutes::new(), &no_interactive_prompts()).await;
        assert!(
            result.is_ok(),
            "the second configured identity is accepted, so the connection must succeed despite the first being rejected"
        );
    }

    /// Codex review finding: a first `IdentityFile` that fails to *parse*
    /// (e.g. a passphrase-protected or corrupt key —
    /// `SessionError::InvalidPrivateKey`) used to propagate straight out,
    /// skipping both the remaining keys and the SSH-agent fallback. Here the
    /// first file is unparseable garbage and the second is a valid, accepted
    /// key — the parse failure must be treated like a rejection and the
    /// connection must still succeed.
    #[tokio::test]
    async fn connect_and_authenticate_skips_an_unparseable_identity_and_tries_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = dir.path().join("id_garbage");
        std::fs::write(&garbage, b"this is not a valid OpenSSH private key\n").unwrap();
        let (valid_key, valid_pub) = write_ed25519_identity(dir.path(), "id_valid", 73);
        let addr = spawn_server(AcceptOneKeyServer { accepted: valid_pub }, 206).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let host_config = openssh_config::HostConfig { identity_file: vec![garbage, valid_key], ..Default::default() };

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier, &ForwardRoutes::new(), &no_interactive_prompts()).await;
        assert!(result.is_ok(), "an unparseable first identity (InvalidPrivateKey) must not block the valid second one");
    }

    /// Codex review finding (regression in the first-cut #11 fix): an
    /// *unreadable* candidate — here a directory, which reliably fails to
    /// `read()` as a non-`NotFound` error regardless of uid — used to be read
    /// eagerly and its error propagated, aborting the whole attempt. Listed
    /// *after* a readable, accepted key it must never even be reached, so this
    /// must authenticate via the first key. (A directory stands in for a
    /// permission-denied file because CI often runs as root, where a chmod-000
    /// file is still readable.)
    #[tokio::test]
    async fn connect_and_authenticate_succeeds_via_first_key_when_a_later_candidate_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let (first_key, first_pub) = write_ed25519_identity(dir.path(), "id_first", 74);
        let unreadable = dir.path().join("id_unreadable");
        std::fs::create_dir(&unreadable).unwrap();
        let addr = spawn_server(AcceptOneKeyServer { accepted: first_pub }, 207).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let host_config = openssh_config::HostConfig { identity_file: vec![first_key, unreadable], ..Default::default() };

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier, &ForwardRoutes::new(), &no_interactive_prompts()).await;
        assert!(
            result.is_ok(),
            "a readable, accepted first key must authenticate; an unreadable *later* candidate must not abort the attempt"
        );
    }

    /// The mirror case: an unreadable *first* candidate must be skipped so a
    /// readable, accepted second candidate still authenticates — the lazy
    /// read+auth loop tolerates an unreadable identity in any position.
    #[tokio::test]
    async fn connect_and_authenticate_skips_an_unreadable_first_candidate_and_tries_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let unreadable = dir.path().join("id_unreadable");
        std::fs::create_dir(&unreadable).unwrap();
        let (valid_key, valid_pub) = write_ed25519_identity(dir.path(), "id_valid", 75);
        let addr = spawn_server(AcceptOneKeyServer { accepted: valid_pub }, 208).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let host_config = openssh_config::HostConfig { identity_file: vec![unreadable, valid_key], ..Default::default() };

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier, &ForwardRoutes::new(), &no_interactive_prompts()).await;
        assert!(result.is_ok(), "an unreadable first candidate must be skipped, then the valid second one authenticates");
    }

    #[tokio::test]
    async fn connect_and_authenticate_succeeds_with_an_encrypted_identity_via_the_right_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let (path, public) = write_encrypted_ed25519_identity(dir.path(), "id_encrypted", 210, "hunter2");
        let addr = spawn_server(AcceptOneKeyServer { accepted: public }, 209).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let host_config = openssh_config::HostConfig { identity_file: vec![path], ..Default::default() };

        let passphrase_prompt = |_path: &Path, _attempt: u32| Some("hunter2".to_string());
        let prompts = InteractivePrompts { passphrase: &passphrase_prompt, keyboard_interactive: &no_kbi_responder, handoff: empty_handoff() };
        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier, &ForwardRoutes::new(), &prompts).await;
        assert!(result.is_ok(), "the right passphrase must decrypt and authenticate: {}", result.err().map(|e| e.to_string()).unwrap_or_default());
    }

    #[tokio::test]
    async fn connect_and_authenticate_fails_cleanly_when_the_passphrase_prompt_is_refused() {
        // Stands in for `run_authenticated_session`'s silent/automated retry
        // seam: the prompt closure always returns `None` (never blocks on
        // stdin), so an encrypted identity is simply skipped and the overall
        // attempt fails with the same "nothing was accepted" error a plain
        // missing key would produce — no hang, no panic.
        let dir = tempfile::tempdir().unwrap();
        let (path, public) = write_encrypted_ed25519_identity(dir.path(), "id_encrypted", 211, "hunter2");
        let addr = spawn_server(AcceptOneKeyServer { accepted: public }, 210).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let host_config = openssh_config::HostConfig { identity_file: vec![path], ..Default::default() };

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier, &ForwardRoutes::new(), &no_interactive_prompts()).await;
        assert!(result.is_err(), "a silently-refused passphrase prompt must fail cleanly, not hang or panic");
    }

    /// Simulates the holder-mode path (Phase 1b passphrase hand-off): the
    /// passphrase was already resolved by the spawning client before this
    /// connect ever started (`resolve_handoff_credentials`, exercised here
    /// exactly as `mux::dispatch` would use it), so `connect_and_authenticate`
    /// must authenticate straight from that hand-off set and never touch the
    /// (here, silently-refusing) on-disk passphrase prompt at all — the whole
    /// point being that a detached holder has no console to prompt with.
    #[tokio::test]
    async fn connect_and_authenticate_uses_a_handoff_credential_without_ever_prompting() {
        let dir = tempfile::tempdir().unwrap();
        let (path, public) = write_encrypted_ed25519_identity(dir.path(), "id_encrypted", 212, "hunter2");
        let addr = spawn_server(AcceptOneKeyServer { accepted: public }, 212).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let host_config = openssh_config::HostConfig { identity_file: vec![path], ..Default::default() };

        let resolved = super::super::mux::handoff::resolve_handoff_credentials(&host_config, dir.path(), &|_path, _attempt| Some("hunter2".to_string()));

        let prompted = std::sync::atomic::AtomicBool::new(false);
        let refusing_prompt = |path: &Path, attempt: u32| {
            prompted.store(true, std::sync::atomic::Ordering::SeqCst);
            no_passphrase_prompt(path, attempt)
        };
        let prompts = InteractivePrompts { passphrase: &refusing_prompt, keyboard_interactive: &no_kbi_responder, handoff: &resolved };

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier, &ForwardRoutes::new(), &prompts).await;
        assert!(result.is_ok(), "a hand-off credential must authenticate directly: {}", result.err().map(|e| e.to_string()).unwrap_or_default());
        assert!(!prompted.load(std::sync::atomic::Ordering::SeqCst), "a hand-off credential must never trigger the on-disk passphrase prompt");
    }

    #[tokio::test]
    async fn try_encrypted_identity_succeeds_on_the_first_correct_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let (path, public) = write_encrypted_ed25519_identity(dir.path(), "id_encrypted", 212, "hunter2");
        let pem = std::fs::read(&path).unwrap();
        let addr = spawn_server(AcceptOneKeyServer { accepted: public }, 211).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let config = Arc::new(client::Config::default());
        let mut handle = establish_over_stream(config, stream, verifying_handler(&verifier)).await.unwrap();

        let calls = std::sync::Mutex::new(0u32);
        let prompt = |_path: &Path, _attempt: u32| -> Option<String> {
            *calls.lock().unwrap() += 1;
            Some("hunter2".to_string())
        };
        let ok = try_encrypted_identity(&mut handle, "tester", &path, &pem, None, &prompt).await.unwrap();
        assert!(ok);
        assert_eq!(*calls.lock().unwrap(), 1, "the first correct passphrase must succeed without retrying");
    }

    #[tokio::test]
    async fn try_encrypted_identity_retries_on_a_wrong_passphrase_then_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let (path, public) = write_encrypted_ed25519_identity(dir.path(), "id_encrypted", 213, "hunter2");
        let pem = std::fs::read(&path).unwrap();
        let addr = spawn_server(AcceptOneKeyServer { accepted: public }, 212).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let config = Arc::new(client::Config::default());
        let mut handle = establish_over_stream(config, stream, verifying_handler(&verifier)).await.unwrap();

        let last_attempt = std::sync::Mutex::new(0u32);
        let prompt = |_path: &Path, attempt: u32| -> Option<String> {
            *last_attempt.lock().unwrap() = attempt;
            if attempt < 3 { Some("wrong".to_string()) } else { Some("hunter2".to_string()) }
        };
        let ok = try_encrypted_identity(&mut handle, "tester", &path, &pem, None, &prompt).await.unwrap();
        assert!(ok, "the 3rd attempt has the right passphrase");
        assert_eq!(*last_attempt.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn try_encrypted_identity_gives_up_immediately_when_the_prompt_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let (path, public) = write_encrypted_ed25519_identity(dir.path(), "id_encrypted", 214, "hunter2");
        let pem = std::fs::read(&path).unwrap();
        let addr = spawn_server(AcceptOneKeyServer { accepted: public }, 213).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let config = Arc::new(client::Config::default());
        let mut handle = establish_over_stream(config, stream, verifying_handler(&verifier)).await.unwrap();

        let ok = try_encrypted_identity(&mut handle, "tester", &path, &pem, None, &|_, _| None).await.unwrap();
        assert!(!ok, "a refused prompt (e.g. silent mode) must give up, not hang or error");
    }

    #[tokio::test]
    async fn try_encrypted_identity_gives_up_after_three_wrong_passphrases() {
        let dir = tempfile::tempdir().unwrap();
        let (path, public) = write_encrypted_ed25519_identity(dir.path(), "id_encrypted", 215, "hunter2");
        let pem = std::fs::read(&path).unwrap();
        let addr = spawn_server(AcceptOneKeyServer { accepted: public }, 214).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let config = Arc::new(client::Config::default());
        let mut handle = establish_over_stream(config, stream, verifying_handler(&verifier)).await.unwrap();

        let calls = std::sync::Mutex::new(0u32);
        let prompt = |_path: &Path, _attempt: u32| -> Option<String> {
            *calls.lock().unwrap() += 1;
            Some("still-wrong".to_string())
        };
        let ok = try_encrypted_identity(&mut handle, "tester", &path, &pem, None, &prompt).await.unwrap();
        assert!(!ok, "3 wrong passphrases in a row must give up, not retry forever");
        assert_eq!(*calls.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn connect_and_authenticate_falls_through_to_keyboard_interactive_when_nothing_else_is_configured() {
        let addr = spawn_server(KeyboardInteractiveOnlyServer { accepted_answer: "hunter2".to_string() }, 220).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        // No identity_file configured and no agent on Linux, so this proves
        // the fall-through all the way to keyboard-interactive.
        let host_config = openssh_config::HostConfig::default();
        let kbi_responder = |_prompts: &[KeyboardInteractivePrompt]| vec!["hunter2".to_string()];
        let prompts = InteractivePrompts { passphrase: &no_passphrase_prompt, keyboard_interactive: &kbi_responder, handoff: empty_handoff() };

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier, &ForwardRoutes::new(), &prompts).await;
        assert!(result.is_ok(), "keyboard-interactive with the right answer must authenticate: {}", result.err().map(|e| e.to_string()).unwrap_or_default());
    }

    #[tokio::test]
    async fn connect_and_authenticate_fails_cleanly_when_keyboard_interactive_is_silently_refused() {
        // Stands in for `run_authenticated_session`'s silent/automated retry
        // seam: the kbi responder always returns empty answers (never blocks
        // on a live server prompt it can't answer), so the overall attempt
        // fails cleanly rather than hanging.
        let addr = spawn_server(KeyboardInteractiveOnlyServer { accepted_answer: "hunter2".to_string() }, 221).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let host_config = openssh_config::HostConfig::default();

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier, &ForwardRoutes::new(), &no_interactive_prompts()).await;
        assert!(result.is_err(), "a silently-refused keyboard-interactive prompt must fail cleanly, not hang or panic");
    }

    /// The cheap, reliable branch of the "always-connects" recovery
    /// orchestration (`run_native_connect_with_recovery`): when the connect
    /// attempt fails but `isekai-pipe connect` left *no* `ConnectOutcome`
    /// signal behind (here it never even ran — the pipe binary path is
    /// bogus), the original error must propagate rather than being swallowed
    /// or triggering a spurious re-bootstrap. The full
    /// spawn→outcome→rebootstrap→retry path is exercised e2e for the Unix
    /// path in `tests/wrapper_stale_trust_auto_recovery_e2e.rs`, whose
    /// `bootstrap_and_register`/`claim_connect_outcome` this path reuses
    /// verbatim (see the function's own docs for why the native side stops
    /// short of that heavy harness).
    #[tokio::test]
    async fn native_connect_recovery_propagates_error_when_no_outcome_signal_is_present() {
        use isekai_pipe_core::{BootstrapProvenance, IntentTransport, ServerIdentity};

        let bogus_pipe = std::env::temp_dir().join("isekai-native-test-nonexistent-pipe-binary");
        let plan = crate::wrapper::parse_wrapper(vec![
            "--isekai-pipe-path".to_string(),
            bogus_pipe.display().to_string(),
            "isekai-native-recovery-test-host".to_string(),
        ])
        .expect("parse_wrapper");
        let (resolution, host_config) = crate::wrapper::resolve_for_native(&plan).expect("resolve_for_native");
        let intent = ConnectionIntent::new(
            "isekai-native-recovery-test-host",
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::Relay {
                helper_addr: "203.0.113.5:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "example.com:22".to_string() },
        );
        let runtime_dir = tempfile::tempdir().unwrap();

        let result =
            run_native_connect_with_recovery(&plan, &resolution, &host_config, intent, runtime_dir.path(), None, HandoffCredentials::default()).await;
        assert!(result.is_err(), "a connect failure with no ConnectOutcome signal must propagate, not be swallowed");
    }

    /// Regression for the owner-hook consumption bug: a *failed* connect
    /// attempt must not consume the `owner_hook` — it has to survive so the
    /// always-connects re-bootstrap retry can still become the mux owner. The
    /// old code did `connect_attempt(..., self.owner_hook.take())`, taking the
    /// hook out of the `Option` the moment `attempt` was *called*, regardless
    /// of whether the attempt then succeeded; a failed first attempt therefore
    /// dropped the hook and the (successful) retry never started `serve_clients`.
    /// Here a bogus pipe binary path makes the attempt fail before any handle
    /// exists; the hook must be left `Some` and never invoked. (The mirror
    /// case — the hook *is* invoked on success — is covered end-to-end by the
    /// mux owner path; a real success needs a live `isekai-pipe connect` child
    /// and sshd, out of scope for a unit test.)
    #[tokio::test]
    async fn connect_attempt_leaves_the_owner_hook_intact_when_the_attempt_fails() {
        use isekai_pipe_core::{BootstrapProvenance, IntentTransport, ServerIdentity};
        use std::sync::atomic::{AtomicBool, Ordering};

        let bogus_pipe = std::env::temp_dir().join("isekai-native-owner-hook-test-nonexistent-pipe-binary");
        let plan = crate::wrapper::parse_wrapper(vec![
            "--isekai-pipe-path".to_string(),
            bogus_pipe.display().to_string(),
            "isekai-native-owner-hook-test-host".to_string(),
        ])
        .expect("parse_wrapper");
        let (resolution, host_config) = crate::wrapper::resolve_for_native(&plan).expect("resolve_for_native");
        let intent = ConnectionIntent::new(
            "isekai-native-owner-hook-test-host",
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::Relay {
                helper_addr: "203.0.113.5:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "example.com:22".to_string() },
        );
        let runtime_dir = tempfile::tempdir().unwrap();

        let fired = std::sync::Arc::new(AtomicBool::new(false));
        let fired_in_hook = fired.clone();
        let mut owner_hook: Option<OwnerHook> = Some(Box::new(move |_handle, _routes| {
            fired_in_hook.store(true, Ordering::SeqCst);
            tokio::spawn(async {})
        }));

        let result = connect_attempt(
            &plan,
            &resolution,
            &host_config,
            &intent,
            runtime_dir.path(),
            &mut owner_hook,
            &HandoffCredentials::default(),
            false,
        )
        .await;

        assert!(result.is_err(), "a bogus pipe binary path must make the connect attempt fail");
        assert!(owner_hook.is_some(), "a failed attempt must leave the owner hook intact for the re-bootstrap retry");
        assert!(!fired.load(Ordering::SeqCst), "the owner hook must not be invoked on a failed attempt");
    }

    /// Regression for the "--isekai-log-file silently ignored on Windows"
    /// finding: `run` reads `WrapperPlan::log_file()` to call
    /// `crate::log_file::init`, so the getter must faithfully surface the
    /// parsed flag (and `None` when it's absent).
    #[test]
    fn wrapper_plan_exposes_isekai_log_file_for_the_native_path() {
        let with_flag = crate::wrapper::parse_wrapper(vec![
            "--isekai-log-file".to_string(),
            "/tmp/isekai-native.log".to_string(),
            "somehost".to_string(),
        ])
        .unwrap();
        assert_eq!(with_flag.log_file(), Some(Path::new("/tmp/isekai-native.log")));

        let without_flag = crate::wrapper::parse_wrapper(vec!["somehost".to_string()]).unwrap();
        assert_eq!(without_flag.log_file(), None);
    }

    // -----------------------------------------------------------------
    // always-connects recovery *sequencing* tests (Finding 3): drive
    // `drive_connect_recovery` against a fake `ConnectRecoveryOps` so the
    // attempt→claim→(maybe re-bootstrap)→retry-once wiring is covered without
    // a real isekai-pipe child or a mock sshd deploy. The real
    // `NativeConnectOps` wiring is covered by the Unix e2e harness this path
    // shares (`wrapper_stale_trust_auto_recovery_e2e.rs`).
    // -----------------------------------------------------------------

    /// A fake [`ConnectRecoveryOps`] that returns queued `attempt` results and
    /// records how many times `attempt`/`rebootstrap_and_rebuild_intent` ran.
    struct FakeRecoveryOps {
        attempt_results: std::collections::VecDeque<std::result::Result<u8, String>>,
        attempt_calls: usize,
        /// The `silent` flag `drive_connect_recovery` passed on each `attempt`
        /// call, in order — see `attempt`'s doc comment: the first attempt
        /// must be `false` (interactive first contact is exempt), the
        /// retry-after-rebootstrap attempt must be `true` (never prompt).
        silent_flags_seen: Vec<bool>,
        outcome: Option<isekai_pipe_core::ConnectOutcome>,
        should_bootstrap: bool,
        rebootstrap_calls: usize,
        rebootstrap_ok: bool,
    }

    fn fake_outcome(class: isekai_pipe_core::ConnectOutcomeClass) -> isekai_pipe_core::ConnectOutcome {
        isekai_pipe_core::ConnectOutcome {
            schema_version: 1,
            intent_id: "deadbeefdeadbeefdeadbeefdeadbeef".to_string(),
            profile: "prod".to_string(),
            class,
            detail: "test detail".to_string(),
        }
    }

    fn fake_intent() -> ConnectionIntent {
        use isekai_pipe_core::{BootstrapProvenance, IntentTransport, ServerIdentity};
        ConnectionIntent::new(
            "prod",
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::Relay {
                helper_addr: "203.0.113.5:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "example.com:22".to_string() },
        )
    }

    #[async_trait(?Send)]
    impl ConnectRecoveryOps for FakeRecoveryOps {
        async fn attempt(&mut self, _intent: &ConnectionIntent, silent: bool) -> Result<u8> {
            self.attempt_calls += 1;
            self.silent_flags_seen.push(silent);
            match self.attempt_results.pop_front().expect("attempt called more times than the test queued results for") {
                Ok(code) => Ok(code),
                Err(msg) => Err(anyhow!(msg)),
            }
        }
        fn claim_outcome(&self, _intent_id: &str) -> Result<Option<isekai_pipe_core::ConnectOutcome>> {
            Ok(self.outcome.clone())
        }
        fn should_bootstrap(&self) -> bool {
            self.should_bootstrap
        }
        async fn rebootstrap_and_rebuild_intent(&mut self) -> Result<ConnectionIntent> {
            self.rebootstrap_calls += 1;
            if self.rebootstrap_ok {
                Ok(fake_intent())
            } else {
                Err(anyhow!("re-bootstrap failed"))
            }
        }
    }

    /// The important path Codex flagged as untested: a first attempt fails, a
    /// `ConnectOutcome` signal is present, auto-bootstrap is allowed → the
    /// helper is re-deployed exactly once and the connection is retried
    /// exactly once, returning the retry's success.
    #[tokio::test]
    async fn recovery_rebootstraps_once_and_retries_once_on_a_signal() {
        let mut ops = FakeRecoveryOps {
            attempt_results: [Err("first attempt failed".to_string()), Ok(7)].into_iter().collect(),
            attempt_calls: 0,
            silent_flags_seen: Vec::new(),
            outcome: Some(fake_outcome(isekai_pipe_core::ConnectOutcomeClass::Unreachable)),
            should_bootstrap: true,
            rebootstrap_calls: 0,
            rebootstrap_ok: true,
        };
        let result = drive_connect_recovery(&mut ops, fake_intent()).await;
        assert_eq!(result.unwrap(), 7, "the retry's exit code must be returned");
        assert_eq!(ops.attempt_calls, 2, "exactly one retry after the first failure");
        assert_eq!(ops.rebootstrap_calls, 1, "the helper must be re-deployed exactly once");
        // Codex review finding (always-connects audit follow-up): the first
        // attempt is the documented interactive-TOFU-exempt case, but the
        // retry-after-rebootstrap attempt must never prompt (a live-but-
        // answerless stdin must not hang it).
        assert_eq!(
            ops.silent_flags_seen,
            vec![false, true],
            "first attempt must allow interactive TOFU; the retry must be silent"
        );
    }

    /// If the automatic re-bootstrap itself fails, its error propagates and
    /// there is no second connect attempt (structurally ≤2 attempts, and the
    /// retry is gated on a successful re-bootstrap).
    #[tokio::test]
    async fn recovery_propagates_a_failed_rebootstrap_without_retrying() {
        let mut ops = FakeRecoveryOps {
            attempt_results: [Err("first attempt failed".to_string())].into_iter().collect(),
            attempt_calls: 0,
            silent_flags_seen: Vec::new(),
            outcome: Some(fake_outcome(isekai_pipe_core::ConnectOutcomeClass::StaleTrust)),
            should_bootstrap: true,
            rebootstrap_calls: 0,
            rebootstrap_ok: false,
        };
        let result = drive_connect_recovery(&mut ops, fake_intent()).await;
        assert!(result.is_err(), "a failed re-bootstrap must surface as an error");
        assert_eq!(ops.rebootstrap_calls, 1, "re-bootstrap was attempted once");
        assert_eq!(ops.attempt_calls, 1, "no retry happens when the re-bootstrap failed");
    }

    /// A signal is present but auto-bootstrap is disabled → the original
    /// connect error propagates, no re-bootstrap, no retry.
    #[tokio::test]
    async fn recovery_does_not_rebootstrap_when_auto_bootstrap_is_disabled() {
        let mut ops = FakeRecoveryOps {
            attempt_results: [Err("first attempt failed".to_string())].into_iter().collect(),
            attempt_calls: 0,
            silent_flags_seen: Vec::new(),
            outcome: Some(fake_outcome(isekai_pipe_core::ConnectOutcomeClass::Unreachable)),
            should_bootstrap: false,
            rebootstrap_calls: 0,
            rebootstrap_ok: true,
        };
        let result = drive_connect_recovery(&mut ops, fake_intent()).await;
        assert!(result.is_err(), "with auto-bootstrap disabled the original error must propagate");
        assert_eq!(ops.rebootstrap_calls, 0, "auto-bootstrap disabled means no re-deploy");
        assert_eq!(ops.attempt_calls, 1, "no retry when auto-bootstrap is disabled");
    }

    /// No signal at all → the original error propagates unchanged, no
    /// re-bootstrap, no retry (a remote command that merely exited non-zero,
    /// or a failure `isekai-pipe connect` didn't classify).
    #[tokio::test]
    async fn recovery_propagates_original_error_without_a_signal() {
        let mut ops = FakeRecoveryOps {
            attempt_results: [Err("first attempt failed".to_string())].into_iter().collect(),
            attempt_calls: 0,
            silent_flags_seen: Vec::new(),
            outcome: None,
            should_bootstrap: true,
            rebootstrap_calls: 0,
            rebootstrap_ok: true,
        };
        let result = drive_connect_recovery(&mut ops, fake_intent()).await;
        assert!(result.is_err(), "no signal means the original error propagates");
        assert_eq!(ops.rebootstrap_calls, 0, "no signal means no re-deploy");
        assert_eq!(ops.attempt_calls, 1, "no signal means no retry");
    }
}
