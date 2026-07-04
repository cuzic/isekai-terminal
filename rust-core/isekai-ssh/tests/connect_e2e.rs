//! End-to-end test for `isekai-ssh connect` (`ISEKAI_SSH_DESIGN.md` フェーズ分割案 S-1's
//! acceptance criterion). Exercises the real chain end to end:
//!
//! ```text
//! ssh(1, real)  --ProxyCommand-->  isekai-ssh connect (this crate's real binary)
//!               --stdin/stdout <-> QUIC-->  isekai-helper (real compiled binary, subprocess)
//!               --QUIC <-> TCP-->  mock sshd (in-process russh::server)
//! ```
//!
//! Nothing here is a type-checking-only mock: the mock sshd is a real
//! `russh::server` speaking the real SSH wire protocol, `isekai-helper` is
//! the actual compiled binary (not a stand-in), and the outer `ssh` is the
//! real system binary. A successful run proves `isekai-ssh connect` performs
//! a real HELLO/proof/ACK handshake and then correctly relays raw SSH bytes
//! in both directions without corrupting them.
//!
//! Requires the `dev-insecure` feature — this phase (S-1) has no trust store
//! yet (that's S-2/S-3), so `--dev-insecure-*` is the only way to tell
//! `connect` where isekai-helper is: `cargo test -p isekai-ssh --features
//! dev-insecure`.
//!
//! Skips itself (rather than failing) when `ssh(1)`/`ssh-keygen(1)` are
//! unavailable, matching `isekai-bootstrap/tests/openssh_e2e.rs`'s
//! convention for the same reason.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio as StdStdio;
use std::time::Duration;

use isekai_protocol::handshake::{decode_handshake_json, HandshakeJson};
use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
use russh::{Channel as RusshChannel, ChannelId, CryptoVec};
use russh_keys::ssh_key::private::Ed25519Keypair;
use russh_keys::{PrivateKey, PublicKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener as TokioTcpListener;
use tokio::process::{Child as TokioChild, Command as TokioCommand};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------
// Shared plumbing: locating binaries, availability checks
// ---------------------------------------------------------------------

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

/// This test file itself is compiled as part of the `isekai-ssh` package, so
/// Cargo sets `CARGO_BIN_EXE_isekai-ssh` for it — and, crucially, the binary
/// it points to is built with the *same* feature set as this test (including
/// `dev-insecure`, since we can only even compile this file with that
/// feature on; see the `required-features` entry in `Cargo.toml`).
fn isekai_ssh_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-ssh"))
}

/// `isekai-helper` is a *different* workspace package (no `[lib]`, so it
/// can't be reached via `CARGO_BIN_EXE_*` the way `isekai-ssh`'s own binary
/// can — that mechanism only covers binaries of the package currently being
/// compiled). It does, however, always land in the same `target/{profile}/`
/// directory as this test binary, so we can find it by walking up from
/// `current_exe()` (mirrors `isekai-helper/tests/e2e.rs::helper_bin_path`).
/// If it isn't there yet (e.g. a from-scratch `cargo test -p isekai-ssh`
/// that never built its sibling package), build it on demand so this test
/// doesn't depend on invocation order.
fn isekai_helper_bin_path() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // this test binary itself
    if path.ends_with("deps") {
        path.pop();
    }
    let is_release = path.file_name().map(|n| n == "release").unwrap_or(false);
    path.push("isekai-helper");

    if !path.exists() {
        eprintln!("isekai-helper binary not found at {path:?}; building it now");
        let mut cmd = std::process::Command::new(env!("CARGO"));
        cmd.args(["build", "-p", "isekai-helper"]);
        if is_release {
            cmd.arg("--release");
        }
        let status = cmd.status().expect("failed to invoke `cargo build -p isekai-helper`");
        assert!(status.success(), "`cargo build -p isekai-helper` failed");
        assert!(path.exists(), "isekai-helper binary still missing at {path:?} after building it");
    }
    path
}

// ---------------------------------------------------------------------
// Mock sshd: a real russh::server, adapted from
// isekai-bootstrap/tests/openssh_e2e.rs's FakeShellServer. isekai-helper
// treats this purely as an opaque `--target` TCP endpoint; it never knows
// or cares that the bytes it relays are SSH.
// ---------------------------------------------------------------------

async fn read_all<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).await?;
    Ok(buf)
}

#[derive(Clone)]
struct FakeShellServer {
    home: PathBuf,
    accepted_client_key: PublicKey,
}

impl server::Server for FakeShellServer {
    type Handler = FakeShellHandler;
    fn new_client(&mut self, _: Option<SocketAddr>) -> FakeShellHandler {
        FakeShellHandler { home: self.home.clone(), accepted_client_key: self.accepted_client_key.clone(), stdin_senders: HashMap::new() }
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

    async fn channel_open_session(&mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn exec_request(&mut self, channel: ChannelId, data: &[u8], session: &mut ServerSession) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        let handle = session.handle();
        let home = self.home.clone();

        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .env("HOME", &home)
            .stdin(StdStdio::piped())
            .stdout(StdStdio::piped())
            .stderr(StdStdio::piped())
            .spawn()
            .expect("mock sshd failed to spawn sh -c for exec_request");

        let mut child_stdin = child.stdin.take().expect("stdin piped");
        let mut child_stdout = child.stdout.take().expect("stdout piped");
        let mut child_stderr = child.stderr.take().expect("stderr piped");

        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        self.stdin_senders.insert(channel, tx);

        tokio::spawn(async move {
            while let Some(chunk) = rx.recv().await {
                if child_stdin.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            let _ = child_stdin.shutdown().await;
        });

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
        self.stdin_senders.remove(&channel);
        Ok(())
    }
}

async fn spawn_fake_ssh_server(home: PathBuf, accepted_client_key: PublicKey) -> SocketAddr {
    let keypair = Ed25519Keypair::from_seed(&[7u8; 32]);
    let host_key = PrivateKey::from(keypair);
    let config = std::sync::Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
    let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut sh = FakeShellServer { home, accepted_client_key };
    tokio::spawn(async move {
        use server::Server as _;
        let _ = sh.run_on_socket(config, &listener).await;
    });
    addr
}

/// Generates a fresh ed25519 keypair via the system `ssh-keygen(1)`.
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

// ---------------------------------------------------------------------
// Real isekai-helper subprocess
// ---------------------------------------------------------------------

struct HelperProcess {
    child: std::process::Child,
    handshake: HandshakeJson,
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawns the real compiled `isekai-helper` binary, `--target`ing
/// `target_addr` directly (no `--relay`) and letting it pick its own QUIC
/// port via `--bind 127.0.0.1:0` — from the client's point of view this is
/// indistinguishable from a relay-fronted deployment (`ISEKAI_SSH_DESIGN.md`
/// "接続シーケンス" note).
fn spawn_helper(target_addr: SocketAddr) -> HelperProcess {
    let mut cmd = std::process::Command::new(isekai_helper_bin_path());
    cmd.arg("--target")
        .arg(target_addr.to_string())
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--log-level")
        .arg("debug")
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn isekai-helper");
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read handshake line from isekai-helper stdout");
    let handshake = decode_handshake_json(line.trim().as_bytes()).expect("failed to parse/validate handshake JSON");

    // Drain stderr on a background thread so isekai-helper never blocks on a
    // full pipe; isekai-helper doesn't write anything further to stdout
    // after the handshake line, so simply leaking the reader (keeping the
    // pipe open without anyone reading it) is fine, mirroring
    // isekai-helper/tests/e2e.rs's own `spawn_helper`.
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut r = BufReader::new(stderr);
            let mut buf = String::new();
            loop {
                buf.clear();
                if r.read_line(&mut buf).unwrap_or(0) == 0 {
                    break;
                }
            }
        });
    }
    std::mem::forget(reader);

    HelperProcess { child, handshake }
}

/// The three `--dev-insecure-*` flags needed to point `connect` at a running
/// helper, given its parsed handshake JSON and its actual bind address
/// (`HandshakeJson` only carries the port, not the host).
fn dev_insecure_args(helper_addr: SocketAddr, handshake: &HandshakeJson) -> Vec<String> {
    vec![
        "--dev-insecure-target".to_string(),
        helper_addr.to_string(),
        "--dev-insecure-cert-sha256".to_string(),
        handshake.cert_sha256.clone(),
        "--dev-insecure-session-secret".to_string(),
        handshake.session_secret.clone(),
    ]
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn connect_reaches_mock_sshd_end_to_end_via_real_ssh() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(home, client_pubkey).await;
    let helper = spawn_helper(mock_sshd_addr);
    let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port).parse().unwrap();

    let proxy_command = format!(
        "{} connect dummy-host {}",
        isekai_ssh_bin_path().display(),
        dev_insecure_args(helper_addr, &helper.handshake).join(" "),
    );

    let output = tokio::time::timeout(
        Duration::from_secs(30),
        TokioCommand::new("ssh")
            .arg("-o")
            .arg(format!("ProxyCommand={proxy_command}"))
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=no")
            .arg("-o")
            .arg("UserKnownHostsFile=/dev/null")
            .arg("-o")
            .arg("IdentitiesOnly=yes")
            .arg("-o")
            .arg(format!("IdentityFile={}", key_path.display()))
            .arg("testuser@dummy-host")
            .arg("echo hello-from-mock-sshd")
            .output(),
    )
    .await
    .expect("ssh should not hang")
    .expect("failed to spawn ssh(1)");

    eprintln!("ssh stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "ssh exited with {:?}; stdout={stdout:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("hello-from-mock-sshd"), "unexpected ssh stdout: {stdout:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_stdout_is_pure_ssh_bytes_even_with_trace_logging() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    // Reuses the same mock-sshd setup as the main test, but this time drives
    // `isekai-ssh connect` directly (not through a real `ssh` process) so we
    // can inspect exactly what it writes to its own stdout.
    let (_key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(home, client_pubkey).await;
    let helper = spawn_helper(mock_sshd_addr);
    let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port).parse().unwrap();

    let mut child: TokioChild = TokioCommand::new(isekai_ssh_bin_path())
        .arg("connect")
        .arg("dummy-host")
        .args(dev_insecure_args(helper_addr, &helper.handshake))
        .env("RUST_LOG", "trace")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh connect");

    // Close our end of its stdin immediately so the C2H direction ends
    // quickly; the mock sshd still sends its SSH banner unprompted (real SSH
    // servers speak first), which is enough real protocol traffic to prove
    // stdout is carrying only that, not log noise.
    drop(child.stdin.take());

    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = child.start_kill();
    let output = child.wait_with_output().await.expect("failed to wait for isekai-ssh connect");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.is_empty(), "expected RUST_LOG=trace to actually produce stderr output, got none");
    assert!(stderr.contains("isekai_ssh") || stderr.contains("isekai-ssh"), "expected isekai-ssh's own log lines on stderr, got: {stderr}");

    let stdout_lossy = String::from_utf8_lossy(&output.stdout);
    for marker in ["isekai_ssh", "isekai-ssh:", "TRACE", "DEBUG "] {
        assert!(
            !stdout_lossy.contains(marker),
            "stdout polluted with log-like text (found {marker:?}); full stdout: {stdout_lossy:?}"
        );
    }
}
