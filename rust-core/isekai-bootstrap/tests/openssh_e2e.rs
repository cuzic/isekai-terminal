//! End-to-end tests for `OpenSshBackend` against a real `ssh(1)` subprocess
//! talking to an in-process mock SSH server (`russh::server`, following the
//! pattern in `rust-core/src/transport.rs`'s `local_forward_e2e_tests` /
//! `proxy_jump_e2e_tests`).
//!
//! Unlike those mocks (which only exercise `russh`'s own client/server
//! wire protocol), the point here is to exercise the *real* `ssh(1)* binary
//! as `OpenSshBackend` actually spawns it, so the mock server's
//! `exec_request` handler genuinely runs the received command string via
//! `sh -c` (with `HOME` pointed at a scratch temp dir), exactly like a real
//! sshd would. The "isekai-helper" binary `OpenSshBackend` uploads and
//! launches is, in these tests, a tiny shell script (valid due to the
//! shebang line) that just echoes canned handshake output — that's enough
//! to exercise upload -> chmod -> setsid launch -> poll -> `cat` end to end
//! without needing a real isekai-helper binary.
//!
//! Skips itself (rather than failing) when no `ssh(1)` binary is available,
//! per `ISEKAI_SSH_DESIGN.md`'s acceptance criteria for this phase.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio as StdStdio;

use isekai_bootstrap::{BootstrapBackend, BootstrapError, HostSpec, OpenSshBackend, RelayLaunchSpec};
use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
use russh::{Channel as RusshChannel, ChannelId, CryptoVec};
use russh_keys::ssh_key::private::Ed25519Keypair;
use russh_keys::{PrivateKey, PublicKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener as TokioTcpListener;
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc;

/// `ssh(1)` is a hard requirement for this crate's e2e tests but isn't
/// guaranteed to exist in every sandboxed dev/CI environment — skip cleanly
/// instead of failing when it's missing.
fn ssh_binary_available() -> bool {
    std::process::Command::new("ssh")
        .arg("-V")
        .stdin(StdStdio::null())
        .stdout(StdStdio::null())
        .stderr(StdStdio::null())
        .status()
        .map(|s| s.success() || s.code().is_some())
        .unwrap_or(false)
}

/// Generates a fresh ed25519 keypair via the system `ssh-keygen(1)` (the
/// exact tool real users use), returning (private key path, loaded public
/// key). Panics (rather than skipping) if `ssh-keygen` itself is missing,
/// since any environment with `ssh(1)` is expected to ship it too.
fn generate_client_keypair(dir: &Path) -> (PathBuf, PublicKey) {
    let key_path = dir.join("client_id_ed25519");
    let status = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "", "-q", "-f"])
        .arg(&key_path)
        .status()
        .expect("failed to run ssh-keygen (expected alongside ssh(1))");
    assert!(status.success(), "ssh-keygen exited non-zero");

    let pub_path = dir.join("client_id_ed25519.pub");
    let pub_text = std::fs::read_to_string(&pub_path).expect("failed to read generated .pub file");
    let public_key = PublicKey::from_openssh(pub_text.trim()).expect("failed to parse generated public key");
    (key_path, public_key)
}

async fn read_all<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).await?;
    Ok(buf)
}

/// The "remote host"'s SSH server. Accepts only the one test client pubkey,
/// and runs whatever command the client execs via a real `sh -c` subprocess
/// with `HOME` pinned to a scratch temp dir — standing in for a real sshd +
/// real remote filesystem, without touching the test runner's actual home
/// directory.
#[derive(Clone)]
struct FakeShellServer {
    home: PathBuf,
    accepted_client_key: PublicKey,
}

impl server::Server for FakeShellServer {
    type Handler = FakeShellHandler;
    fn new_client(&mut self, _: Option<SocketAddr>) -> FakeShellHandler {
        FakeShellHandler {
            home: self.home.clone(),
            accepted_client_key: self.accepted_client_key.clone(),
            stdin_senders: HashMap::new(),
        }
    }
}

struct FakeShellHandler {
    home: PathBuf,
    accepted_client_key: PublicKey,
    stdin_senders: HashMap<ChannelId, mpsc::UnboundedSender<Vec<u8>>>,
}

#[async_trait::async_trait]
impl server::Handler for FakeShellHandler {
    type Error = russh::Error;

    async fn auth_publickey(&mut self, _user: &str, public_key: &PublicKey) -> Result<Auth, Self::Error> {
        if public_key.key_data() == self.accepted_client_key.key_data() {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::Reject { proceed_with_methods: None })
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: RusshChannel<ServerMsg>,
        _session: &mut ServerSession,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        let handle = session.handle();
        let home = self.home.clone();

        let mut child = TokioCommand::new("sh")
            .arg("-c")
            .arg(&command)
            .env("HOME", &home)
            .stdin(StdStdio::piped())
            .stdout(StdStdio::piped())
            .stderr(StdStdio::piped())
            .spawn()
            .expect("mock server failed to spawn sh -c for exec_request");

        let mut child_stdin = child.stdin.take().expect("stdin piped");
        let mut child_stdout = child.stdout.take().expect("stdout piped");
        let mut child_stderr = child.stderr.take().expect("stderr piped");

        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        self.stdin_senders.insert(channel, tx);

        // Forward client -> server channel data (the base64 payload for the
        // upload command) into the real child process's stdin.
        tokio::spawn(async move {
            while let Some(chunk) = rx.recv().await {
                if child_stdin.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            let _ = child_stdin.shutdown().await;
        });

        // Drain the child's stdout/stderr, then report exit status/EOF/close
        // back to the client — mirroring what a real sshd does once the
        // remote command finishes.
        tokio::spawn(async move {
            let (stdout_res, stderr_res, wait_res) =
                tokio::join!(read_all(&mut child_stdout), read_all(&mut child_stderr), child.wait());

            if let Ok(out) = stdout_res {
                if !out.is_empty() {
                    let _ = handle.data(channel, CryptoVec::from(out)).await;
                }
            }
            if let Ok(err) = stderr_res {
                if !err.is_empty() {
                    let _ = handle.extended_data(channel, 1, CryptoVec::from(err)).await;
                }
            }
            let code = wait_res.ok().and_then(|s| s.code()).unwrap_or(1) as u32;
            let _ = handle.exit_status_request(channel, code).await;
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
        });

        session.channel_success(channel)?;
        Ok(())
    }

    async fn data(&mut self, channel: ChannelId, data: &[u8], _session: &mut ServerSession) -> Result<(), Self::Error> {
        if let Some(tx) = self.stdin_senders.get(&channel) {
            let _ = tx.send(data.to_vec());
        }
        Ok(())
    }

    async fn channel_eof(&mut self, channel: ChannelId, _session: &mut ServerSession) -> Result<(), Self::Error> {
        // Dropping the sender ends the forwarding task's `while let` loop,
        // which then shuts down the child's stdin — the real-sshd-equivalent
        // of "client sent EOF, so close stdin".
        self.stdin_senders.remove(&channel);
        Ok(())
    }
}

async fn spawn_fake_ssh_server(home: PathBuf, accepted_client_key: PublicKey) -> SocketAddr {
    let keypair = Ed25519Keypair::from_seed(&[42u8; 32]);
    let host_key = PrivateKey::from(keypair);
    let config = std::sync::Arc::new(server::Config {
        keys: vec![host_key],
        ..Default::default()
    });
    let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut sh = FakeShellServer { home, accepted_client_key };
    tokio::spawn(async move {
        use server::Server as _;
        let _ = sh.run_on_socket(config, &listener).await;
    });
    addr
}

/// Builds an `OpenSshBackend` wired to talk to `server_addr` using the
/// generated test identity, bypassing host-key verification (test-only —
/// see the crate's module docs for why production code must never do this).
fn test_backend(key_path: &Path) -> OpenSshBackend {
    OpenSshBackend::new().with_extra_ssh_args(vec![
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=/dev/null".to_string(),
        "-o".to_string(),
        "IdentitiesOnly=yes".to_string(),
        "-o".to_string(),
        format!("IdentityFile={}", key_path.display()),
    ])
}

fn dummy_relay_spec() -> RelayLaunchSpec {
    RelayLaunchSpec {
        relay_addr: "127.0.0.1:1".parse().unwrap(),
        relay_sni: "relay.isekai-ssh.test".to_string(),
        relay_jwt: "test-jwt-token".to_string(),
        idle_lifetime_secs: 2_592_000,
    }
}

const VALID_HANDSHAKE_JSON: &str = r#"{"v":1,"listen_port":45231,"cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb","session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE="}"#;

#[tokio::test]
async fn install_and_start_gets_a_real_handshake_over_a_real_ssh_subprocess() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home, client_pubkey).await;

    // Stands in for the isekai-helper binary: a shell script that ignores
    // its args and just emits exactly one line of valid handshake JSON,
    // exactly like the real isekai-helper does on stdout at startup.
    let fake_helper_script = format!("#!/bin/sh\necho '{VALID_HANDSHAKE_JSON}'\n");

    let backend = test_backend(&key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");

    let report = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(&target, None, fake_helper_script.as_bytes(), &dummy_relay_spec()),
    )
    .await
    .expect("install_and_start should not hang")
    .expect("install_and_start should succeed against the mock server");

    assert_eq!(report.handshake.v, 1);
    assert_eq!(report.handshake.listen_port, 45231);
    assert_eq!(report.handshake.cert_sha256.len(), 64);
}

/// `RelayLaunchSpec::idle_lifetime_secs` must actually reach the launched
/// `isekai-helper` process's argv as `--max-idle-lifetime <SECS>` — this is
/// the fix for the `ISEKAI_SSH_DESIGN.md` "引き続き未決の項目" gap where a
/// helper deployed via `isekai-ssh init` would inherit `isekai-helper`'s own
/// short default (600s, tuned for `isekai-terminal-core`'s per-session bootstrap) and
/// self-exit long before a `connect` invocation hours/days later could reach
/// it again.
#[tokio::test]
async fn install_and_start_passes_idle_lifetime_to_the_launched_helper() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home.clone(), client_pubkey).await;

    // Same stand-in as the other tests, but it also records its own argv to
    // a side file so this test can inspect exactly what `OpenSshBackend`
    // launched it with (its stdout must stay pure JSON, matching the real
    // isekai-helper's contract, so argv can't be echoed there instead).
    let argv_log = home.join("argv.log");
    let fake_helper_script =
        format!("#!/bin/sh\necho \"$@\" > {}\necho '{VALID_HANDSHAKE_JSON}'\n", argv_log.display());

    let backend = test_backend(&key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");
    let relay = RelayLaunchSpec {
        relay_addr: "127.0.0.1:1".parse().unwrap(),
        relay_sni: "relay.isekai-ssh.test".to_string(),
        relay_jwt: "test-jwt-token".to_string(),
        idle_lifetime_secs: 2_592_000,
    };

    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(&target, None, fake_helper_script.as_bytes(), &relay),
    )
    .await
    .expect("install_and_start should not hang")
    .expect("install_and_start should succeed against the mock server");

    let argv = std::fs::read_to_string(&argv_log).expect("stand-in script should have recorded its argv");
    assert!(
        argv.contains("--max-idle-lifetime 2592000"),
        "expected the launched isekai-helper's argv to contain '--max-idle-lifetime 2592000', got: {argv:?}"
    );
}

#[tokio::test]
async fn install_and_start_fails_closed_when_stdout_has_extra_lines() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home, client_pubkey).await;

    // Same as above, but the fake helper also prints an extra line — as if
    // isekai-helper (or something on the remote system) polluted stdout
    // with a warning/log line instead of keeping it purely the handshake
    // JSON. `OpenSshBackend` must reject this rather than guess which line
    // is "the real one".
    let fake_helper_script =
        format!("#!/bin/sh\necho '{VALID_HANDSHAKE_JSON}'\necho 'unexpected warning: something else happened'\n");

    let backend = test_backend(&key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(&target, None, fake_helper_script.as_bytes(), &dummy_relay_spec()),
    )
    .await
    .expect("install_and_start should not hang");

    match result {
        Err(BootstrapError::UnexpectedStdout { extra_lines }) => {
            assert_eq!(extra_lines, 1);
        }
        other => panic!("expected UnexpectedStdout, got: {other:?}"),
    }
}
