//! Windows-native `BuildRequest` handling (`ISEKAI_PIPE_DESIGN.md` §8 Epic P
//! Phase 2) — the counterpart of the Unix `ssh(1)` path's
//! [`crate::ctl_forward::run_build`] for the `russh`-based owner/client mux.
//!
//! [`run_build_over_channel`] is used only by the owner's own foreground
//! shell (`native/connect.rs`, via [`super::ctl_forward::pump_to_stderr`]):
//! that process already holds the real forwarded ctl-socket channel *and* is
//! the one that should run the build (it's the tab the user is actually
//! looking at), so no cross-process relay is needed — it decodes, spawns,
//! streams, and detects disconnection (a failed write *or* the channel
//! itself reporting `Eof`/`Close`) all in one place.
//!
//! A *mux client* tab has no direct access to the real SSH channel (only the
//! owner does), so it cannot reuse this function directly — see
//! `super::owner`'s and `super::client`'s module docs for how the two
//! directions of that relay work. Only the byte-encoding
//! ([`crate::build_exec::encode_build_output_chunk`]/`encode_build_finished`)
//! and child-spawn/stdout-stderr-pump ([`crate::build_exec::pump_bytes`])
//! pieces are shared between this function and the mux-client path
//! (`super::client::spawn_client_build`) — the surrounding orchestration
//! (where output goes, how disconnection is detected) differs enough by sink
//! type that forcing one shared function would obscure more than it'd share
//! (see `build_exec.rs`'s module docs).

use anyhow::{bail, Context, Result};
use russh::client;
use tokio::sync::{mpsc, oneshot};

use crate::log_file::log_line;

/// Sent in place of a real `BuildFinished` (`ISEKAI_PIPE_DESIGN.md` §8 Epic P
/// Phase 2) when a mux client's build must be aborted because the *remote*
/// side of the ctl channel went away mid-build (not because the build
/// itself finished) — see `super::owner`/`super::client` for where this is
/// produced and consumed. Not a wire-format change: `BuildFinished`'s
/// `exit_code` was always a plain `i32` with no range restriction, so this
/// sentinel round-trips like any other value (see `build_exec.rs`'s
/// `encode_build_finished_accepts_the_i32_min_abort_sentinel` test) — it's a
/// convention `native/mux` establishes, not a new `isekai_protocol` variant.
/// `i32::MIN` can never collide with a real process exit code (0–255 on
/// every platform).
pub(crate) const BUILD_ABORTED_SENTINEL: i32 = i32::MIN;

/// Runs the build profile `(host, profile_name)` resolves to and streams its
/// output back over `channel` (a forwarded ctl-socket channel this process
/// already holds) as `BuildOutputChunk`s, finishing with `BuildFinished`.
///
/// Detects the remote disconnecting mid-build two ways at once — a failed
/// `channel.data()` write, or `channel.wait()` itself reporting `Eof`/
/// `Close`/`None` — and kills the child immediately either way (the same
/// "every session must have a guaranteed cleanup path" principle
/// `.claude/rules/always-connects.md` documents for the fencing-slot lesson,
/// applied to a local child process). Watching `channel.wait()` concurrently
/// (rather than only reacting to the next failed write, as the simpler Unix
/// single-process path does) means a build that goes briefly quiet doesn't
/// delay detecting a disconnect that already happened.
pub(crate) async fn run_build_over_channel(channel: &mut russh::Channel<client::Msg>, host: &str, profile_name: &str) -> Result<()> {
    let profile = crate::build_profile::default_build_profiles_path()
        .and_then(|path| crate::build_profile::load_build_profiles(&path))
        .ok()
        .and_then(|store| crate::build_profile::find_profile(&store, host, profile_name).cloned());

    let Some(profile) = profile else {
        let chunk = crate::build_exec::encode_build_output_chunk(
            isekai_protocol::BuildOutputStream::Stderr,
            format!("isekai-ssh: no build profile registered for {host:?}/{profile_name:?}\n").into_bytes(),
        )?;
        let _ = channel.data(&chunk[..]).await;
        let finished = crate::build_exec::encode_build_finished(127, Vec::new())?;
        let _ = channel.data(&finished[..]).await;
        return Ok(());
    };

    let mut child = crate::build_exec::spawn_shell_command(&profile.command, &profile.dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("isekai-ssh: failed to spawn build profile {host:?}/{profile_name:?}"))?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let (tx, mut rx) = tokio::sync::mpsc::channel::<(isekai_protocol::BuildOutputStream, Vec<u8>)>(32);
    let stdout_task = tokio::spawn(crate::build_exec::pump_bytes(stdout, isekai_protocol::BuildOutputStream::Stdout, tx.clone()));
    let stderr_task = tokio::spawn(crate::build_exec::pump_bytes(stderr, isekai_protocol::BuildOutputStream::Stderr, tx.clone()));
    drop(tx);

    let mut disconnected = false;
    loop {
        tokio::select! {
            recv = rx.recv() => {
                match recv {
                    Some((stream, bytes)) => {
                        let chunk = crate::build_exec::encode_build_output_chunk(stream, bytes)?;
                        if channel.data(&chunk[..]).await.is_err() {
                            disconnected = true;
                            break;
                        }
                    }
                    None => break,
                }
            }
            msg = channel.wait() => {
                if matches!(msg, None | Some(russh::ChannelMsg::Eof) | Some(russh::ChannelMsg::Close)) {
                    disconnected = true;
                    break;
                }
            }
        }
    }
    // See `crate::ctl_forward::run_build`'s identical comment: dropping `rx`
    // unblocks any pump task still waiting on a full channel so awaiting
    // them below can't deadlock.
    drop(rx);

    if disconnected {
        let _ = child.start_kill();
        let _ = child.wait().await;
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        bail!("isekai-ssh: ctl channel closed before the build finished; killed the child process");
    }
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let status = child.wait().await.context("isekai-ssh: failed to wait for the build child process")?;
    let exit_code = status.code().unwrap_or(-1);

    let result_paths: Vec<String> = match (&profile.result_glob, &profile.dest_dir) {
        (Some(glob), Some(_dest_dir)) => crate::build_exec::glob_results(&profile.dir, glob)
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        _ => Vec::new(),
    };
    if let Some(dest_dir) = &profile.dest_dir {
        crate::build_exec::spawn_result_push(host.to_string(), dest_dir.clone(), result_paths.clone());
    }
    let finished = crate::build_exec::encode_build_finished(exit_code, result_paths)?;
    let _ = channel.data(&finished[..]).await;
    Ok(())
}

/// A build `client.rs::run_inner` is currently streaming, so it can abort it
/// (kill the child) when the owner connection is lost or the owner relays a
/// [`BUILD_ABORTED_SENTINEL`]. Dropping an `ActiveBuild` without calling
/// [`abort`](Self::abort) leaves the task to finish on its own — fine for the
/// normal "it already reached `BuildFinished`" case, since by then there is
/// nothing left to kill.
pub(crate) struct ActiveBuild {
    abort_tx: Option<oneshot::Sender<()>>,
    #[allow(dead_code)] // kept so the task itself isn't detached from `ActiveBuild`'s lifetime in spirit; not currently awaited
    task: tokio::task::JoinHandle<()>,
}

impl ActiveBuild {
    /// Signals the build task to kill its child and stop. A no-op if the
    /// build already finished on its own (the signal is only sent once).
    pub(crate) fn abort(&mut self) {
        if let Some(tx) = self.abort_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Spawns the profile `(host, profile_name)` resolves to and streams its
/// `BuildOutputChunk`/`BuildFinished` bytes into `build_out_tx` — the
/// mux-client counterpart of [`run_build_over_channel`]. Unlike that
/// function, this cannot detect the *remote* disconnecting on its own (only
/// the owner holds the real ctl channel — see `super::owner`'s module docs
/// for the relay that tells this side about it, arriving back here as the
/// `BUILD_ABORTED_SENTINEL` `client.rs` reacts to); it can only be told to
/// stop externally via the returned [`ActiveBuild::abort`].
pub(crate) fn spawn_client_build(host: String, profile_name: String, build_out_tx: mpsc::UnboundedSender<Vec<u8>>) -> ActiveBuild {
    let (abort_tx, abort_rx) = oneshot::channel();
    let task = tokio::spawn(run_client_build(host, profile_name, build_out_tx, abort_rx));
    ActiveBuild { abort_tx: Some(abort_tx), task }
}

async fn run_client_build(host: String, profile_name: String, build_out_tx: mpsc::UnboundedSender<Vec<u8>>, mut abort_rx: oneshot::Receiver<()>) {
    let profile = crate::build_profile::default_build_profiles_path()
        .and_then(|path| crate::build_profile::load_build_profiles(&path))
        .ok()
        .and_then(|store| crate::build_profile::find_profile(&store, &host, &profile_name).cloned());

    let Some(profile) = profile else {
        if let Ok(chunk) = crate::build_exec::encode_build_output_chunk(
            isekai_protocol::BuildOutputStream::Stderr,
            format!("isekai-ssh: no build profile registered for {host:?}/{profile_name:?}\n").into_bytes(),
        ) {
            let _ = build_out_tx.send(chunk);
        }
        if let Ok(finished) = crate::build_exec::encode_build_finished(127, Vec::new()) {
            let _ = build_out_tx.send(finished);
        }
        return;
    };

    let mut child = match crate::build_exec::spawn_shell_command(&profile.command, &profile.dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            log_line!("isekai-ssh: failed to spawn build profile {host:?}/{profile_name:?}: {e}");
            return;
        }
    };
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let (tx, mut rx) = mpsc::channel::<(isekai_protocol::BuildOutputStream, Vec<u8>)>(32);
    let stdout_task = tokio::spawn(crate::build_exec::pump_bytes(stdout, isekai_protocol::BuildOutputStream::Stdout, tx.clone()));
    let stderr_task = tokio::spawn(crate::build_exec::pump_bytes(stderr, isekai_protocol::BuildOutputStream::Stderr, tx.clone()));
    drop(tx);

    let mut aborted = false;
    loop {
        tokio::select! {
            recv = rx.recv() => {
                match recv {
                    Some((stream, bytes)) => {
                        let Ok(chunk) = crate::build_exec::encode_build_output_chunk(stream, bytes) else {
                            continue;
                        };
                        // A send failure means `run_inner`'s own select loop
                        // already ended (the owner connection was lost) —
                        // there is nowhere left for this output to go.
                        if build_out_tx.send(chunk).is_err() {
                            aborted = true;
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = &mut abort_rx => {
                aborted = true;
                break;
            }
        }
    }
    // See `run_build_over_channel`'s identical comment: dropping `rx`
    // unblocks any pump task still waiting on a full channel so awaiting
    // them below can't deadlock.
    drop(rx);

    if aborted {
        let _ = child.start_kill();
        let _ = child.wait().await;
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        return;
    }
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let Ok(status) = child.wait().await else {
        return;
    };
    let exit_code = status.code().unwrap_or(-1);

    let result_paths: Vec<String> = match (&profile.result_glob, &profile.dest_dir) {
        (Some(glob), Some(_dest_dir)) => crate::build_exec::glob_results(&profile.dir, glob)
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        _ => Vec::new(),
    };
    if let Some(dest_dir) = &profile.dest_dir {
        crate::build_exec::spawn_result_push(host, dest_dir.clone(), result_paths.clone());
    }
    if let Ok(finished) = crate::build_exec::encode_build_finished(exit_code, result_paths) {
        let _ = build_out_tx.send(finished);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use russh::server::{self, Auth, Msg as ServerMsg, Server as _, Session as ServerSession};
    use russh::Channel as RusshChannel;
    use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey as SshPrivateKey};
    use russh_stream_session::{authenticate_session, establish_over_stream, verifying_handler_with_routes, Credential, ForwardRoutes, HostKeyVerifier, VerifyOutcome};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    struct AcceptAllHostKeys;
    #[async_trait]
    impl HostKeyVerifier for AcceptAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> VerifyOutcome {
            VerifyOutcome::Accepted
        }
    }

    /// A mock sshd that, on `streamlocal_forward`, opens a
    /// `forwarded-streamlocal` channel back — same pattern as
    /// `super::ctl_forward`'s own tests (`CtlPushServer`) — but hands the
    /// *server*-side channel object out via `channel_tx` instead of writing
    /// to it itself, so this test can drive both ends: write the initial
    /// request from the server side (standing in for the remote's
    /// `isekai-pipe ctl build`), then read `run_build_over_channel`'s
    /// replies back off that same server-side handle.
    #[derive(Clone)]
    struct EchoForwardServer {
        channel_tx: mpsc::UnboundedSender<RusshChannel<ServerMsg>>,
    }
    impl server::Server for EchoForwardServer {
        type Handler = EchoForwardHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> EchoForwardHandler {
            EchoForwardHandler(self.channel_tx.clone())
        }
    }
    #[derive(Clone)]
    struct EchoForwardHandler(mpsc::UnboundedSender<RusshChannel<ServerMsg>>);
    #[async_trait]
    impl server::Handler for EchoForwardHandler {
        type Error = russh::Error;
        async fn auth_password(&mut self, _u: &str, _p: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }
        async fn channel_open_session(&mut self, _c: RusshChannel<ServerMsg>, _s: &mut ServerSession) -> Result<bool, Self::Error> {
            Ok(true)
        }
        async fn streamlocal_forward(&mut self, socket_path: &str, session: &mut ServerSession) -> Result<bool, Self::Error> {
            let handle = session.handle();
            let path = socket_path.to_string();
            let tx = self.0.clone();
            tokio::spawn(async move {
                if let Ok(channel) = handle.channel_open_forwarded_streamlocal(path).await {
                    let _ = tx.send(channel);
                }
            });
            Ok(true)
        }
    }

    /// Sets up a real (in-process, TCP-loopback) SSH client/server pair and
    /// requests one streamlocal forward, returning the *client* handle and
    /// forward-routes table (the caller must keep both alive for the whole
    /// test — dropping either tears down the underlying connection/forward),
    /// the *client*-side forwarded channel (what `run_build_over_channel`
    /// operates on, exactly as `pump_to_stderr` would hand it), and the
    /// *server*-side channel object for the same logical channel (what a
    /// real remote's `isekai-pipe ctl build` would see).
    async fn forwarded_channel_pair() -> (
        client::Handle<russh_stream_session::VerifyingHandler<AcceptAllHostKeys>>,
        ForwardRoutes,
        russh::Channel<client::Msg>,
        RusshChannel<ServerMsg>,
    ) {
        let keypair = Ed25519Keypair::from_seed(&[151; 32]);
        let host_key = SshPrivateKey::from(keypair);
        let config = Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (channel_tx, mut channel_rx) = mpsc::unbounded_channel();
        let mut srv = EchoForwardServer { channel_tx };
        tokio::spawn(async move {
            let _ = srv.run_on_socket(config, &listener).await;
        });

        let routes = ForwardRoutes::new();
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let handler = verifying_handler_with_routes(&verifier, &routes);
        let mut client_handle = establish_over_stream(Arc::new(client::Config::default()), stream, handler).await.unwrap();
        assert!(authenticate_session(&mut client_handle, "tester", &Credential::Password("x".to_string())).await.unwrap());

        let remote_path = "/tmp/isekai-pipe-ctl-build-relay-test.sock".to_string();
        let mut client_channels = routes.register(&remote_path);
        client_handle.streamlocal_forward(remote_path).await.unwrap();

        let client_channel = client_channels.recv().await.expect("forward should deliver a client-side channel");
        let server_channel = channel_rx.recv().await.expect("the mock sshd should deliver the server-side channel");
        (client_handle, routes, client_channel, server_channel)
    }

    /// Reads one `Data` event off `channel` and decodes it as a
    /// `CtlMessage` — the same shape `read_ctl_line` extracts in production,
    /// simplified here since the test always sends exactly one line per
    /// `Data` event.
    async fn recv_ctl_message(channel: &mut RusshChannel<ServerMsg>) -> isekai_protocol::CtlMessage {
        match channel.wait().await {
            Some(russh::ChannelMsg::Data { data }) => {
                let line = data.strip_suffix(b"\n").unwrap_or(&data);
                isekai_protocol::decode_ctl_message(line).expect("test only sends well-formed CtlMessage lines")
            }
            other => panic!("expected a Data event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_build_over_channel_reports_an_unknown_profile() {
        let (_client_handle, _routes, mut client_channel, mut server_channel) = forwarded_channel_pair().await;

        run_build_over_channel(&mut client_channel, "mybox", "nope").await.unwrap();

        let stderr_msg = recv_ctl_message(&mut server_channel).await;
        let stderr_text = match stderr_msg {
            isekai_protocol::CtlMessage::BuildOutputChunk { stream, data_b64 } => {
                assert_eq!(stream, isekai_protocol::BuildOutputStream::Stderr);
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64).unwrap()
            }
            other => panic!("expected BuildOutputChunk, got {other:?}"),
        };
        assert!(String::from_utf8_lossy(&stderr_text).contains("no build profile registered"));

        match recv_ctl_message(&mut server_channel).await {
            isekai_protocol::CtlMessage::BuildFinished { exit_code, result_paths } => {
                assert_eq!(exit_code, 127);
                assert!(result_paths.is_empty());
            }
            other => panic!("expected BuildFinished, got {other:?}"),
        }
    }

    /// Points `$HOME` at a fresh tempdir and writes `profiles` to
    /// `build_profiles.toml` there — same `HOME_ENV_LOCK`-guarded pattern
    /// `ctl_forward.rs`'s own Unix tests use.
    fn with_build_profiles(profiles: Vec<crate::build_profile::BuildProfile>) -> (tempfile::TempDir, HomeRestoreGuard) {
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let mut store = crate::build_profile::BuildProfileStore::default();
        for profile in profiles {
            crate::build_profile::upsert_profile(&mut store, profile).unwrap();
        }
        let path = crate::build_profile::default_build_profiles_path().unwrap();
        crate::build_profile::save_build_profiles(&path, &store).unwrap();
        (home, HomeRestoreGuard(old_home))
    }

    struct HomeRestoreGuard(Option<std::ffi::OsString>);
    impl Drop for HomeRestoreGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(old) => std::env::set_var("HOME", old),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[tokio::test]
    async fn run_build_over_channel_streams_output_and_reports_exit_code_and_results() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let workdir = tempfile::tempdir().unwrap();
        let (_home, _restore) = with_build_profiles(vec![crate::build_profile::BuildProfile {
            host: "mybox".to_string(),
            name: "t".to_string(),
            dir: workdir.path().to_string_lossy().into_owned(),
            command: if cfg!(windows) {
                "echo out-line& echo err-line 1>&2& type nul > out.bin& exit 5".to_string()
            } else {
                "printf 'out-line\\n'; printf 'err-line\\n' 1>&2; touch out.bin; exit 5".to_string()
            },
            result_glob: Some("out.bin".to_string()),
            dest_dir: Some("~/dest".to_string()),
        }]);

        let (_client_handle, _routes, mut client_channel, mut server_channel) = forwarded_channel_pair().await;
        run_build_over_channel(&mut client_channel, "mybox", "t").await.unwrap();

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        loop {
            match recv_ctl_message(&mut server_channel).await {
                isekai_protocol::CtlMessage::BuildOutputChunk { stream, data_b64 } => {
                    let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64).unwrap();
                    match stream {
                        isekai_protocol::BuildOutputStream::Stdout => stdout.extend(decoded),
                        isekai_protocol::BuildOutputStream::Stderr => stderr.extend(decoded),
                    }
                }
                isekai_protocol::CtlMessage::BuildFinished { exit_code, result_paths } => {
                    assert_eq!(exit_code, 5);
                    assert_eq!(result_paths.len(), 1);
                    assert!(result_paths[0].ends_with("out.bin"));
                    break;
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
        assert!(String::from_utf8_lossy(&stdout).contains("out-line"));
        assert!(String::from_utf8_lossy(&stderr).contains("err-line"));
    }

    /// Mirrors the Unix path's
    /// `run_build_kills_the_child_when_the_connection_breaks_mid_build`: the
    /// build child produces output forever, so `run_build_over_channel` can
    /// only return if it actually detected the closed channel and killed the
    /// child — otherwise it would hang on `child.wait()` forever, which
    /// `tokio::time::timeout` turns into a visible test failure instead of a
    /// real hang. Exercises detection via `channel.wait()` reporting the
    /// close (not merely a failed write, which the Unix test covers).
    #[tokio::test]
    async fn run_build_over_channel_kills_the_child_when_the_channel_closes_mid_build() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let workdir = tempfile::tempdir().unwrap();
        let (_home, _restore) = with_build_profiles(vec![crate::build_profile::BuildProfile {
            host: "mybox".to_string(),
            name: "infinite".to_string(),
            dir: workdir.path().to_string_lossy().into_owned(),
            command: if cfg!(windows) {
                ":loop& echo x& goto loop".to_string()
            } else {
                "while true; do printf x; sleep 0.01; done".to_string()
            },
            result_glob: None,
            dest_dir: None,
        }]);

        let (_client_handle, _routes, mut client_channel, server_channel) = forwarded_channel_pair().await;
        // Close the server's end right away rather than reading anything —
        // the client-side `channel.wait()` in `run_build_over_channel`'s
        // select loop must observe this and kill the still-running child.
        let _ = server_channel.close().await;
        drop(server_channel);

        let result = tokio::time::timeout(std::time::Duration::from_secs(10), run_build_over_channel(&mut client_channel, "mybox", "infinite")).await;
        let result = result.expect("run_build_over_channel must not hang after the channel closes");
        let err = result.unwrap_err();
        assert!(format!("{err:#}").contains("closed") || format!("{err:#}").contains("killed"));
    }

    /// The mux-client counterpart of
    /// `run_build_over_channel_kills_the_child_when_the_channel_closes_mid_build`:
    /// `spawn_client_build` has no channel of its own to watch for a remote
    /// disconnect, only the external `ActiveBuild::abort` signal
    /// `client.rs::run_inner` fires on an owner-relayed abort sentinel or a
    /// lost owner connection. An infinite-output child that only stops if
    /// actually killed, awaited under a timeout, turns "did abort really
    /// kill it" into a assertion instead of a hang.
    #[tokio::test]
    async fn spawn_client_build_kills_the_child_when_aborted() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let workdir = tempfile::tempdir().unwrap();
        let (_home, _restore) = with_build_profiles(vec![crate::build_profile::BuildProfile {
            host: "mybox".to_string(),
            name: "infinite".to_string(),
            dir: workdir.path().to_string_lossy().into_owned(),
            command: if cfg!(windows) {
                ":loop& echo x& goto loop".to_string()
            } else {
                "while true; do printf x; sleep 0.01; done".to_string()
            },
            result_glob: None,
            dest_dir: None,
        }]);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut build = spawn_client_build("mybox".to_string(), "infinite".to_string(), tx);

        // Wait for at least one real chunk to prove the child actually started
        // before telling it to abort.
        rx.recv().await.expect("the build must produce at least one output chunk");
        build.abort();

        let task = build.task;
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("the build task must finish promptly after abort, not hang on an unkilled child")
            .unwrap();
    }
}
