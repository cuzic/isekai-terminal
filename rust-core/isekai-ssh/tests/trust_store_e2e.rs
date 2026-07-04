//! End-to-end tests for `isekai-ssh connect`'s trust store integration
//! (`ISEKAI_SSH_DESIGN.md` フェーズ分割案 S-2's acceptance criteria).
//!
//! Unlike `connect_e2e.rs` (which requires the `dev-insecure` feature and
//! exercises the dev-only bypass), everything here runs against a plain,
//! default-feature build of the `isekai-ssh` binary — proving the actual
//! production path: `connect` resolves its target purely from
//! `~/.config/isekai-ssh/known_helpers.toml` (`isekai-trust`), with no
//! `--dev-insecure-*` flags involved.
//!
//! Two scenarios:
//! - **Unregistered host**: `connect` must fail closed before any network
//!   I/O — stdout stays completely empty, stderr explains that
//!   `isekai-ssh init <host>` needs to be run, and the process exits
//!   non-zero.
//! - **Registered host**: same real chain as `connect_e2e.rs` (real mock
//!   `russh::server`, real compiled `isekai-helper` binary, real `ssh(1)`),
//!   except the isekai-helper endpoint/credentials are looked up from a
//!   trust store file written via `isekai-trust`'s own `save_trust_store`
//!   API instead of being passed on the command line.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

use isekai_protocol::handshake::{decode_handshake_json, HandshakeJson};
use isekai_trust::schema::{HelperTrust, TrustStore, UpdatePolicy};
use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
use russh::{Channel as RusshChannel, ChannelId, CryptoVec};
use russh_keys::ssh_key::private::Ed25519Keypair;
use russh_keys::{PrivateKey, PublicKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener as TokioTcpListener;
use tokio::process::Command as TokioCommand;

// ---------------------------------------------------------------------
// Shared plumbing (mirrors connect_e2e.rs; kept separate on purpose so
// this file compiles and runs with a *default*-feature build, unlike
// connect_e2e.rs which requires `dev-insecure`).
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

fn isekai_ssh_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-ssh"))
}

/// See `connect_e2e.rs::isekai_helper_bin_path` for why this walks up from
/// `current_exe()` rather than using a `CARGO_BIN_EXE_*` variable.
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
        FakeShellHandler {
            home: self.home.clone(),
            accepted_client_key: self.accepted_client_key.clone(),
            stdin_senders: std::collections::HashMap::new(),
        }
    }
}

struct FakeShellHandler {
    home: PathBuf,
    accepted_client_key: PublicKey,
    stdin_senders: std::collections::HashMap<ChannelId, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
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

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
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

fn generate_client_keypair(dir: &std::path::Path) -> (PathBuf, PublicKey) {
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

// ---------------------------------------------------------------------
// Trust store fixture helpers
// ---------------------------------------------------------------------

/// `$HOME/.config/isekai-ssh/known_helpers.toml`, mirroring
/// `isekai_trust::store::default_trust_store_path`'s own layout (built from
/// the same public path-segment constants) so writing here with an explicit
/// path and pointing the `isekai-ssh` subprocess at `home` via `HOME=` land
/// on the exact same file.
fn trust_store_path_under(home: &std::path::Path) -> PathBuf {
    home.join(".config").join(isekai_trust::store::CONFIG_DIR_NAME).join(isekai_trust::store::TRUST_STORE_FILE_NAME)
}

fn sample_trust_entry(helper_addr: SocketAddr, handshake: &HandshakeJson) -> HelperTrust {
    HelperTrust {
        identity_pubkey: "pk-test".to_string(),
        trusted_helper_sha256: "a".repeat(64),
        trusted_helper_version: "0.0.0-test".to_string(),
        update_policy: UpdatePolicy::ExactDigestOnly,
        release_channel: None,
        last_via: None,
        trusted_at: "2026-07-04T00:00:00Z".to_string(),
        last_seen_at: "2026-07-04T00:00:00Z".to_string(),
        cached_relay_addr: helper_addr.to_string(),
        cached_cert_sha256: handshake.cert_sha256.clone(),
        cached_session_secret: handshake.session_secret.clone(),
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

/// Acceptance criterion 1: an unregistered host must fail closed with a
/// completely empty stdout, a non-zero exit code, and an actionable
/// `isekai-ssh init` message on stderr — all without ever touching the
/// network (there is no relay/helper running in this test at all).
#[tokio::test(flavor = "multi_thread")]
async fn connect_fails_closed_for_unregistered_host() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // Deliberately do *not* create `~/.config/isekai-ssh/known_helpers.toml`
    // at all — "the file doesn't exist yet" is the normal never-initialized
    // state (`isekai_trust::store::load_trust_store`'s own docs).

    let output = tokio::time::timeout(
        Duration::from_secs(10),
        TokioCommand::new(isekai_ssh_bin_path())
            .arg("connect")
            .arg("unknown-host")
            .env("HOME", &home)
            .env_remove("RUST_LOG")
            .stdin(StdStdio::null())
            .stdout(StdStdio::piped())
            .stderr(StdStdio::piped())
            .output(),
    )
    .await
    .expect("isekai-ssh connect should fail closed quickly, not hang")
    .expect("failed to spawn isekai-ssh connect");

    assert!(
        output.stdout.is_empty(),
        "stdout must stay completely empty on an unregistered host, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!output.status.success(), "connect must exit non-zero for an unregistered host");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("isekai-ssh init"),
        "stderr should point the user at `isekai-ssh init`, got: {stderr}"
    );
    assert!(stderr.contains("unknown-host"), "stderr should mention the host it looked up, got: {stderr}");
}

/// Acceptance criterion 2: same real chain as `connect_e2e.rs`'s S-1 test
/// (real mock sshd, real isekai-helper subprocess, real `ssh(1)`), but the
/// isekai-helper endpoint/credentials come entirely from a trust store file
/// written via `isekai-trust::save_trust_store` — no `--dev-insecure-*` flags
/// anywhere, and this test's own binary is built with isekai-ssh's default
/// feature set (no `dev-insecure`).
#[tokio::test(flavor = "multi_thread")]
async fn connect_succeeds_with_trust_store_registered_host() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home, client_pubkey).await;
    let helper = spawn_helper(mock_sshd_addr);
    let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port).parse().unwrap();

    // Register trust for "dummy-host" (default port 22, matching
    // `isekai_trust::normalize_host_port`'s normalization of a bare host)
    // entirely through the isekai-trust crate — this is standing in for
    // what `isekai-ssh init` (S-3) will do.
    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let trust_store_path = trust_store_path_under(&home);
    let key = isekai_trust::normalize_host_port("dummy-host").unwrap();
    assert_eq!(key, "dummy-host:22");
    let mut store = TrustStore::default();
    store.insert(key, sample_trust_entry(helper_addr, &helper.handshake));
    isekai_trust::save_trust_store(&trust_store_path, &store).expect("failed to write trust store fixture");

    let proxy_command = format!("{} connect dummy-host", isekai_ssh_bin_path().display());

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
            .arg("echo hello-from-trust-store")
            // `ssh` inherits this HOME and, crucially, so does the
            // ProxyCommand subprocess it spawns (`isekai-ssh connect`),
            // which is how `connect` finds the trust store fixture above
            // via its own `isekai_trust::default_trust_store_path()` call.
            .env("HOME", &home)
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
    assert!(stdout.contains("hello-from-trust-store"), "unexpected ssh stdout: {stdout:?}");
}
