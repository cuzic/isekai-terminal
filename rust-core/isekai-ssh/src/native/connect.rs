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
//! [`run_native_connect_with_recovery`]). What
//! this path still does *not* do is auto-bootstrap a *brand-new*
//! (never-registered) destination on first contact: a trust-store miss still
//! fails with guidance to run `isekai-ssh init` manually (the initial TOFU
//! confirmation is inherently interactive — `always-connects.md`'s stated
//! exception). Likewise, a destination with `#@isekai enabled no` (direct,
//! non-isekai SSH) isn't supported by this path yet — that's a plain
//! `connect_via_jump_or_direct` call away, but is left for a follow-up since
//! every destination this project's users actually run through `isekai-ssh`
//! has isekai routing enabled.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use isekai_pipe_core::{claim_connect_outcome, default_runtime_dir, ConnectionIntent};
use russh::client;
use russh_stream_session::{authenticate_session, establish_over_stream, open_channel, verifying_handler, SessionError, SessionKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
use super::host_key_trust::FileBackedHostKeyVerifier;
use super::private_key;

/// `isekai-ssh <destination>` entrypoint for the native path — the
/// `cfg(windows)`-gated alternative `main.rs` dispatches to instead of
/// `wrapper::run`. Takes the same raw argv `wrapper::run` does.
pub(crate) async fn run(args: Vec<String>) -> Result<u8> {
    let plan = crate::wrapper::parse_wrapper(args)?;
    // `--isekai-log-file` must be honored on the native path too — the Unix
    // path opens it at the top of `wrapper::run`; without this the flag was
    // silently ignored on Windows (Codex review finding). Opened before any
    // connection attempt so every diagnostic line below is captured.
    if let Some(log_file) = plan.log_file() {
        crate::log_file::init(log_file)
            .with_context(|| format!("isekai-ssh: failed to open --isekai-log-file at {}", log_file.display()))?;
    }
    let (resolution, host_config) = crate::wrapper::resolve_for_native(&plan)?;
    if !resolution.isekai_enabled() {
        return Err(anyhow!(
            "isekai-ssh: {:?} has isekai routing disabled (#@isekai enabled no / --isekai-direct); \
             the native Windows path doesn't support plain direct SSH yet — see native/connect.rs's module docs.",
            plan.destination()
        ));
    }
    let intent = crate::wrapper::build_connection_intent(&resolution).with_context(|| {
        format!(
            "isekai-ssh: {:?} is not set up yet for the native path — run `isekai-ssh init {}` first",
            plan.destination(),
            plan.destination()
        )
    })?;

    let runtime_dir = default_runtime_dir()?;
    run_native_connect_with_recovery(&plan, &resolution, &host_config, intent, &runtime_dir).await
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
) -> Result<u8> {
    let mut ops = NativeConnectOps { plan, resolution, host_config, runtime_dir };
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
    /// failure mode a `ConnectOutcome` signal accompanies.
    async fn attempt(&mut self, intent: &ConnectionIntent) -> Result<u8>;
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
    let first_error = match ops.attempt(&intent).await {
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
            ops.attempt(&intent2).await
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
}

#[async_trait(?Send)]
impl ConnectRecoveryOps for NativeConnectOps<'_> {
    async fn attempt(&mut self, intent: &ConnectionIntent) -> Result<u8> {
        connect_attempt(self.plan, self.resolution, self.host_config, intent, self.runtime_dir).await
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
) -> Result<u8> {
    let mut child = spawn_isekai_pipe_connect(plan.pipe_path(), runtime_dir, intent)?;
    let stdio = ChildStdio::take_from(&mut child)
        .ok_or_else(|| anyhow!("isekai-ssh: spawned isekai-pipe connect without piped stdin/stdout (internal bug)"))?;

    let (host, port) = resolution.native_host_port(plan.destination());
    let host_port = format!("{host}:{port}");
    let username = host_config
        .user
        .clone()
        .or_else(local_username)
        .ok_or_else(|| anyhow!("isekai-ssh: no username configured (ssh_config User, $USER, %USERNAME%) for {host_port}"))?;

    let store_path = isekai_trust::default_ssh_host_key_trust_store_path()
        .map_err(|e| anyhow!("isekai-ssh: could not determine the SSH host key trust store path: {e}"))?;
    let confirm_host_port = host_port.clone();
    let confirm_new_host: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(move |fingerprint: &str| {
        prompt_new_host_confirmation(&confirm_host_port, fingerprint)
    });
    let verifier = Arc::new(FileBackedHostKeyVerifier::new(store_path, host_port.clone(), confirm_new_host));

    let handle = connect_and_authenticate(stdio, &username, host_config, &verifier)
        .await
        .with_context(|| format!("isekai-ssh: failed to connect to {username}@{host_port}"))?;

    let (cols, rows) = console::terminal_size();
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    let mut channel = open_channel(&handle, &SessionKind::Shell { term, cols, rows })
        .await
        .context("isekai-ssh: failed to open a shell channel")?;

    let _raw_mode = console::RawModeGuard::enable().context("isekai-ssh: failed to enable raw terminal mode")?;
    let exit_code = run_shell_io_loop(&mut channel).await?;

    // Keeps the compiler from complaining that `handle`/`child` are unused
    // past this point — both must stay alive for the duration of the I/O
    // loop above (dropping `handle` would tear down the SSH session,
    // dropping `child` kills the `isekai-pipe connect` subprocess, per
    // `ChildStdio`'s own docs), so this is a deliberate keep-alive, not a
    // no-op.
    drop(handle);
    drop(child);

    Ok(exit_code)
}

fn local_username() -> Option<String> {
    std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok())
}

/// Real interactive TOFU prompt for a never-before-seen host key —
/// `ssh(1)`'s own wording, adapted. Runs on a `spawn_blocking` thread (see
/// `host_key_trust.rs::verify`'s docs), so a plain blocking stdin read is
/// safe here.
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
) -> Result<client::Handle<russh_stream_session::VerifyingHandler<V>>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    V: russh_stream_session::HostKeyVerifier + 'static,
{
    let config = Arc::new(client::Config::default());
    let handler = verifying_handler(verifier);
    let mut handle = establish_over_stream(config, stream, handler).await?;

    let home = isekai_fs_guard::resolve_home_dir().unwrap_or_else(|| PathBuf::from("."));
    let candidates = private_key::identity_file_candidates(&host_config.identity_file, &home);
    // Read + authenticate one candidate at a time, lazily (Codex review
    // finding): an unreadable/unusable candidate — missing, permission-denied,
    // or unparseable — is skipped, never fatal, so it can't block a
    // perfectly-good candidate listed before *or* after it, nor the SSH-agent
    // fallback below. Only a genuine transport/protocol error aborts.
    for candidate in &candidates {
        let Some(credential) = private_key::read_credential(candidate) else {
            continue;
        };
        match authenticate_session(&mut handle, username, &credential).await {
            Ok(true) => return Ok(handle),
            Ok(false) => continue,
            Err(SessionError::InvalidPrivateKey(_)) => continue,
            Err(e) => return Err(anyhow::Error::new(e).context("SSH authentication request failed")),
        }
    }

    if try_agent_auth(&mut handle, username, host_config).await? {
        return Ok(handle);
    }

    Err(anyhow!(
        "no configured private key or SSH agent identity was accepted for {username}"
    ))
}

#[cfg(windows)]
async fn try_agent_auth<H: client::Handler>(
    handle: &mut client::Handle<H>,
    username: &str,
    host_config: &openssh_config::HostConfig,
) -> Result<bool> {
    let target = agent_auth::resolve_agent_target(host_config.identity_agent.as_deref());
    let Some(mut agent) = agent_auth::connect_agent(&target).await? else {
        return Ok(false);
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
/// **Known limitation**: does not yet propagate local terminal resize
/// events to the remote PTY (`channel.window_change`) — the channel is
/// opened with the size at connect time and stays fixed for the session.
/// Delegates to [`run_shell_io_loop_inner`] (which the tests drive against
/// in-memory buffers) with the real local `stdin`/`stdout`/`stderr`; driving
/// a real terminal stdin/stdout pair isn't practical in a unit test.
async fn run_shell_io_loop(channel: &mut russh::Channel<client::Msg>) -> Result<u8> {
    run_shell_io_loop_inner(channel, tokio::io::stdin(), tokio::io::stdout(), tokio::io::stderr()).await
}

/// The body of [`run_shell_io_loop`] with the three local streams injected,
/// so tests can substitute in-memory buffers for a real terminal. `stdin`
/// feeds the remote channel; remote `Data` goes to `stdout` and remote
/// `ExtendedData` (stderr) goes to `stderr` — real `ssh(1)` keeps the two
/// separate (Codex review finding: they were both written to `stdout`).
async fn run_shell_io_loop_inner<I, O, E>(
    channel: &mut russh::Channel<client::Msg>,
    mut stdin: I,
    mut stdout: O,
    mut stderr: E,
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

    loop {
        tokio::select! {
            n = stdin.read(&mut buf), if stdin_open => {
                match n {
                    Ok(0) => {
                        stdin_open = false;
                        let _ = channel.eof().await;
                    }
                    Ok(n) => {
                        if channel.data(&buf[..n]).await.is_err() {
                            break;
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
        }
    }

    Ok(exit_code.unwrap_or(NO_EXIT_STATUS_RECEIVED))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
    use russh::{Channel as RusshChannel, CryptoVec};
    use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey as SshPrivateKey};
    use russh_keys::ssh_key::{LineEnding, PublicKey as SshPublicKey};
    use russh_stream_session::{Credential, HostKeyVerifier};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    struct AcceptAllHostKeys;
    #[async_trait]
    impl HostKeyVerifier for AcceptAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> bool {
            true
        }
    }

    struct RejectAllHostKeys;
    #[async_trait]
    impl HostKeyVerifier for RejectAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> bool {
            false
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

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier).await;
        assert!(result.is_err(), "no identity file and no agent means nothing to authenticate with");
    }

    #[tokio::test]
    async fn connect_and_authenticate_rejects_when_the_host_key_verifier_refuses() {
        let addr = spawn_server(PasswordServer { accepted_password: "unused".to_string() }, 201).await;
        let verifier = Arc::new(RejectAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let host_config = openssh_config::HostConfig::default();

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier).await;
        assert!(result.is_err(), "a rejected host key must fail the connection before any auth attempt");
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

        let mut channel = open_channel(&handle, &SessionKind::Shell { term: "xterm".to_string(), cols: 80, rows: 24 })
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
        let channel = open_channel(&handle, &SessionKind::Shell { term: "xterm".to_string(), cols: 80, rows: 24 })
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
        let exit_code = run_shell_io_loop_inner(&mut channel, tokio::io::empty(), &mut stdout, &mut stderr).await.unwrap();
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
        let _ = run_shell_io_loop_inner(&mut channel, tokio::io::empty(), &mut stdout, &mut stderr).await.unwrap();
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

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier).await;
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

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier).await;
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

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier).await;
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

        let result = connect_and_authenticate(stream, "tester", &host_config, &verifier).await;
        assert!(result.is_ok(), "an unreadable first candidate must be skipped, then the valid second one authenticates");
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

        let result = run_native_connect_with_recovery(&plan, &resolution, &host_config, intent, runtime_dir.path()).await;
        assert!(result.is_err(), "a connect failure with no ConnectOutcome signal must propagate, not be swallowed");
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
        async fn attempt(&mut self, _intent: &ConnectionIntent) -> Result<u8> {
            self.attempt_calls += 1;
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
            outcome: Some(fake_outcome(isekai_pipe_core::ConnectOutcomeClass::Unreachable)),
            should_bootstrap: true,
            rebootstrap_calls: 0,
            rebootstrap_ok: true,
        };
        let result = drive_connect_recovery(&mut ops, fake_intent()).await;
        assert_eq!(result.unwrap(), 7, "the retry's exit code must be returned");
        assert_eq!(ops.attempt_calls, 2, "exactly one retry after the first failure");
        assert_eq!(ops.rebootstrap_calls, 1, "the helper must be re-deployed exactly once");
    }

    /// If the automatic re-bootstrap itself fails, its error propagates and
    /// there is no second connect attempt (structurally ≤2 attempts, and the
    /// retry is gated on a successful re-bootstrap).
    #[tokio::test]
    async fn recovery_propagates_a_failed_rebootstrap_without_retrying() {
        let mut ops = FakeRecoveryOps {
            attempt_results: [Err("first attempt failed".to_string())].into_iter().collect(),
            attempt_calls: 0,
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
