//! Windows-native connect entrypoint (M1): ties together `openssh-config`
//! (host resolution), the existing `#@isekai` directive resolution
//! (`crate::wrapper::resolve_for_native`), [`super::child_stdio`] (spawns
//! `isekai-pipe connect --stdio`), `russh_stream_session` (M0, the actual
//! SSH protocol), [`super::host_key_trust`] (TOFU), [`super::private_key`]/
//! [`super::agent_auth`] (authentication), and [`super::console`] (raw mode
//! + terminal size) into one working `isekai-ssh <destination>` path that
//! never shells out to `ssh(1)`.
//!
//! **Scope note**: unlike `wrapper::run`, this path does not attempt
//! auto-bootstrap or the `ConnectOutcome`-driven silent re-bootstrap retry
//! (`always-connects.md`) yet. Both currently go through
//! `isekai_bootstrap::OpenSshBackend`, which itself shells out to a real
//! `ssh(1)` to deploy `isekai-pipe serve` on first contact — reusing it here
//! would defeat the point of this module. Closing that gap is M3's
//! `RusshBackend` (`fancy-humming-pnueli.md` M3); until then, a
//! not-yet-trusted destination fails with guidance to run `isekai-ssh init`
//! manually instead of silently falling back to `ssh(1)`. Likewise, a
//! destination with `#@isekai enabled no` (direct, non-isekai SSH) isn't
//! supported by this path yet — that's a plain `connect_via_jump_or_direct`
//! call away, but is left for a follow-up since every destination this
//! project's users actually run through `isekai-ssh` has isekai routing
//! enabled.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use isekai_pipe_core::default_runtime_dir;
use russh::client;
use russh_stream_session::{authenticate_session, establish_over_stream, open_channel, verifying_handler, SessionKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    let mut child = spawn_isekai_pipe_connect(plan.pipe_path(), &runtime_dir, &intent)?;
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

    let mut handle = connect_and_authenticate(stdio, &username, &host_config, &verifier)
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
/// `username`, trying (in order) a private key from `host_config
/// ::identity_file`/the default `id_ed25519`→`id_rsa`→`id_ecdsa` probe, then
/// an SSH agent (Windows-only — see [`agent_auth::connect_agent`]).
/// Deliberately generic over `stream`/`verifier` so it's testable against an
/// in-process mock SSH server without a real `isekai-pipe connect`
/// subprocess or trust store — the same technique every other `native/*.rs`
/// module in this crate uses. Everything in [`run`] above this call (real
/// subprocess, real trust store, real terminal I/O) is not unit-tested.
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
    if let Ok(credential) = private_key::load_first_existing(&candidates) {
        if authenticate_session(&mut handle, username, &credential).await? {
            return Ok(handle);
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
/// the caller) and the remote shell channel until either side closes,
/// returning the remote exit status as this process's own exit code (`ssh(1)`'s
/// own convention). Local stdin EOF (Ctrl-D redirected from a non-tty, or a
/// real EOF) sends a channel EOF rather than closing the channel outright,
/// so any buffered remote output still in flight is not lost.
///
/// **Known limitation**: does not yet propagate local terminal resize
/// events to the remote PTY (`channel.window_change`) — the channel is
/// opened with the size at connect time and stays fixed for the session.
/// Not covered by any test in this crate: driving a real local
/// stdin/stdout pair isn't practical in a unit test; `russh-stream-session`'s
/// own tests already cover the underlying `Channel`/`ChannelMsg` plumbing
/// this loop drives.
async fn run_shell_io_loop(channel: &mut russh::Channel<client::Msg>) -> Result<u8> {
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut buf = [0u8; 8192];
    let mut exit_code: u8 = 0;
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
                        let _ = stdout.write_all(&data).await;
                        let _ = stdout.flush().await;
                    }
                    Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                        exit_code = exit_status as u8;
                    }
                    Some(russh::ChannelMsg::Eof) | Some(russh::ChannelMsg::Close) => {
                        break;
                    }
                    None => break,
                    _ => {}
                }
            }
        }
    }

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
    use russh::Channel as RusshChannel;
    use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey as SshPrivateKey};
    use russh_stream_session::HostKeyVerifier;
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
}
