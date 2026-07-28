//! End-to-end coverage for SSH-agent authentication on the Windows-native
//! (russh) connect path (`native/agent_auth.rs`). That module's own doc
//! comment on `connect_agent` says it is "Not exercised by any test in this
//! codebase... Verified only via `cargo check --target x86_64-pc-windows-gnu`
//! ... a real Windows machine must confirm this actually works" — this file
//! closes that gap by driving a real `isekai-ssh.exe` process against a real
//! named-pipe SSH agent.
//!
//! **Why an in-process `russh_keys::agent::server::serve` instead of the
//! real Windows OpenSSH `ssh-agent.exe`**: unlike Unix's `eval $(ssh-agent)`
//! model, Windows OpenSSH's `ssh-agent.exe` is a Windows *service* bound to
//! the single, fixed, machine-wide pipe `\\.\pipe\openssh-ssh-agent` (no
//! flag to pick a custom pipe name, no `SSH_AUTH_SOCK`-style stdout to
//! parse) — using it would mean a shared, non-isolated, admin-scoped
//! resource unsuitable for a parallel test suite. `russh_keys::agent::server
//! ::serve` (russh-keys 0.48.1) is generic over any `Stream` of accepted
//! connections, so this test stands up its own private named pipe per test
//! run and exercises the *real* wire protocol
//! `native/agent_auth.rs::connect_agent`'s `AgentClient::connect_named_pipe`
//! actually speaks — the only thing not "real" here is which process
//! answers on the other end of the pipe.
//!
//! `russh_keys::agent::server`'s `KeyStore` starts empty with no pre-seed
//! hook — identities only arrive via an `AddIdentity` agent-protocol
//! message. So this test connects its own `AgentClient` to the pipe first
//! and calls `add_identity` before ever spawning the real `isekai-ssh.exe`
//! process that will connect to the same pipe as a client.
//!
//! Mirrors `mux_holder_windows_e2e.rs`'s harness (mock sshd via
//! `tests/support::spawn_sshd`, real `isekai-ssh.exe`/`isekai-pipe.exe`
//! subprocesses, the `"ready\n"`-banner synchronization strategy) and
//! duplicates its boilerplate per this crate's self-contained-test-file
//! convention rather than sharing it.
#![cfg(windows)]

mod support;

use std::io::BufRead as StdBufRead;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use isekai_protocol::handshake::HandshakeJson;
use isekai_trust::{HelperTrust, UpdatePolicy};
use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
use russh::{Channel as RusshChannel, CryptoVec};
use russh_keys::{PrivateKey, PublicKey};
use tokio::io::AsyncReadExt;
use tokio::net::windows::named_pipe::ServerOptions;
use tokio::process::{Child, Command as TokioCommand};
use tokio_stream::wrappers::UnboundedReceiverStream;

fn isekai_ssh_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-ssh"))
}

/// Duplicated from `mux_holder_windows_e2e.rs::isekai_pipe_bin_path` per this
/// crate's self-contained-test-file convention.
fn isekai_pipe_bin_path() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let is_release = path.file_name().map(|n| n == "release").unwrap_or(false);
    path.push("isekai-pipe.exe");

    if !path.exists() {
        eprintln!("isekai-pipe binary not found at {path:?}; building it now");
        let mut cmd = std::process::Command::new(env!("CARGO"));
        cmd.args(["build", "-p", "isekai-pipe"]);
        if is_release {
            cmd.arg("--release");
        }
        let status = cmd.status().unwrap_or_else(|_| panic!("failed to invoke `cargo build -p isekai-pipe`"));
        assert!(status.success(), "`cargo build -p isekai-pipe` failed");
        assert!(path.exists(), "isekai-pipe binary still missing at {path:?} after building it");
    }
    path
}

fn generate_client_keypair(dir: &std::path::Path) -> (PathBuf, PublicKey, PrivateKey) {
    let key_path = dir.join("client_id_ed25519");
    let status = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "", "-q", "-f"])
        .arg(&key_path)
        .status()
        .expect("failed to run ssh-keygen");
    assert!(status.success(), "ssh-keygen exited non-zero");

    let pub_path = dir.join("client_id_ed25519.pub");
    let pub_text = std::fs::read_to_string(&pub_path).expect("failed to read generated .pub file");
    let public_key = PublicKey::from_openssh(pub_text.trim()).expect("failed to parse generated public key");
    let private_pem = std::fs::read_to_string(&key_path).expect("failed to read generated private key");
    let private_key = PrivateKey::from_openssh(&private_pem).expect("failed to parse generated private key");
    (key_path, public_key, private_key)
}

/// Stands up a private, per-test named-pipe SSH agent
/// (`russh_keys::agent::server::serve`) and seeds it with `key` via a
/// throwaway `AgentClient` connection *before* returning — so any later
/// connection (the real `isekai-ssh.exe` under test) always finds the
/// identity already loaded. `KeyStore` has no pre-seed API (confirmed by
/// reading `russh-keys-0.48.1/src/agent/server.rs`), only `AddIdentity`.
async fn spawn_seeded_agent(pipe_name: &str, key: &PrivateKey) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer>>();
    let accept_pipe_name = pipe_name.to_string();
    tokio::spawn(async move {
        let mut first = true;
        loop {
            let server = match ServerOptions::new().first_pipe_instance(first).create(&accept_pipe_name) {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            first = false;
            if server.connect().await.is_err() {
                continue;
            }
            if tx.send(Ok(server)).is_err() {
                return;
            }
        }
    });
    let stream = UnboundedReceiverStream::new(rx);
    tokio::spawn(russh_keys::agent::server::serve(stream, ()));

    // Give the accept loop a moment to create the first pipe instance before
    // dialing it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut seeding_client = russh_keys::agent::client::AgentClient::connect_named_pipe(pipe_name)
        .await
        .expect("failed to connect to the freshly-spawned test agent to seed it");
    seeding_client.add_identity(key, &[]).await.expect("failed to add the test identity to the agent");
}

// ---------------------------------------------------------------------
// Mock sshd: accepts only plain publickey auth for the agent-held key
// (rejects everything else), so this test can only pass if the native path
// actually authenticated via the agent rather than falling back to some
// other credential. Mirrors `mux_holder_windows_e2e.rs`'s `FakeShellServer`/
// `FakeShellHandler` shape.
// ---------------------------------------------------------------------

#[derive(Clone)]
struct AgentOnlyShellServer {
    accepted_client_key: PublicKey,
    connection_count: Arc<AtomicUsize>,
}

impl server::Server for AgentOnlyShellServer {
    type Handler = AgentOnlyShellHandler;
    fn new_client(&mut self, _: Option<SocketAddr>) -> AgentOnlyShellHandler {
        self.connection_count.fetch_add(1, Ordering::SeqCst);
        AgentOnlyShellHandler { accepted_client_key: self.accepted_client_key.clone() }
    }
}

#[derive(Clone)]
struct AgentOnlyShellHandler {
    accepted_client_key: PublicKey,
}

#[async_trait::async_trait]
impl server::Handler for AgentOnlyShellHandler {
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

    async fn shell_request(&mut self, channel: russh::ChannelId, session: &mut ServerSession) -> Result<(), Self::Error> {
        session.data(channel, CryptoVec::from(b"ready\n".to_vec()))?;
        Ok(())
    }
}

async fn spawn_agent_only_ssh_server(accepted_client_key: PublicKey, connection_count: Arc<AtomicUsize>) -> (SocketAddr, String) {
    support::spawn_sshd(103, AgentOnlyShellServer { accepted_client_key, connection_count }).await
}

fn seed_ssh_host_key_trust(home: &std::path::Path, host_port: &str, fingerprint: &str) {
    let path = home.join(".config").join(isekai_trust::store::CONFIG_DIR_NAME).join(isekai_trust::store::SSH_HOST_KEY_TRUST_STORE_FILE_NAME);
    let mut store = isekai_trust::SshHostKeyTrustStore::default();
    store.insert(
        host_port.to_string(),
        isekai_trust::SshHostKeyTrust {
            fingerprint: fingerprint.to_string(),
            trusted_at: "2026-01-01T00:00:00Z".to_string(),
            last_seen_at: "2026-01-01T00:00:00Z".to_string(),
        },
    );
    isekai_trust::save_ssh_host_key_trust_store(&path, &store).unwrap();
}

fn profiles_dir_under(home: &std::path::Path) -> PathBuf {
    home.join(".local").join("state").join("isekai").join("profiles")
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

fn spawn_real_helper(target_addr: SocketAddr) -> HelperProcess {
    let mut cmd = std::process::Command::new(isekai_pipe_bin_path());
    cmd.arg("serve").arg("--target").arg(target_addr.to_string()).arg("--bind").arg("127.0.0.1:0").stdout(StdStdio::piped()).stderr(StdStdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn isekai-pipe serve");
    let stdout = child.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read handshake line from isekai-pipe serve stdout");
    let handshake = isekai_protocol::handshake::decode_handshake_json(line.trim().as_bytes()).expect("failed to parse/validate handshake JSON");

    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(stderr);
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

fn register_correct_profile(profiles_dir: &std::path::Path, key: &str, helper_addr: SocketAddr, cert_sha256_hex: &str, session_secret_b64: &str) {
    let trust = HelperTrust {
        identity_pubkey: cert_sha256_hex.to_string(),
        trusted_helper_sha256: "a".repeat(64),
        trusted_helper_version: "test".to_string(),
        update_policy: UpdatePolicy::ExactDigestOnly,
        release_channel: None,
        last_via: None,
        trusted_at: "2026-01-01T00:00:00Z".to_string(),
        last_seen_at: "2026-01-01T00:00:00Z".to_string(),
        cached_relay_addr: helper_addr.to_string(),
        cached_cert_sha256: cert_sha256_hex.to_string(),
        cached_session_secret: session_secret_b64.to_string(),
        cached_stun_observed_addr: None,
    };
    let profile = isekai_pipe_core::PersistentProfile::migrate_legacy_helper_trust(key, &trust);
    isekai_pipe_core::write_persistent_profile(profiles_dir, &profile).unwrap();
}

async fn wait_for_ready(child: &mut Child, timeout: Duration, label: &str) {
    let mut stderr = child.stderr.take().expect("stderr was piped");
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut seen = Vec::new();
    let mut chunk = [0u8; 4096];
    let result = tokio::time::timeout(timeout, async {
        loop {
            let n = stdout.read(&mut chunk).await.expect("reading tab stdout should not error");
            assert!(n > 0, "{label}: tab's stdout closed before the \"ready\" banner ever appeared, saw {:?}", String::from_utf8_lossy(&seen));
            seen.extend_from_slice(&chunk[..n]);
            if seen.windows(5).any(|w| w == b"ready") {
                return;
            }
        }
    })
    .await;
    assert!(result.is_ok(), "{label}: timed out waiting for the \"ready\" banner, saw so far: {:?}", String::from_utf8_lossy(&seen));
    child.stdout = Some(stdout);
}

/// Spawns a real `isekai-ssh.exe` "tab" with the given ssh_config `Host`
/// block already written to `home/.ssh/config`.
fn spawn_tab(home: &std::path::Path, isekai_pipe_path: &std::path::Path, runtime_dir: &std::path::Path, alias: &str) -> Child {
    TokioCommand::new(isekai_ssh_bin_path())
        .arg("--isekai-pipe-path")
        .arg(isekai_pipe_path)
        .arg(alias)
        .env("HOME", home)
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(home))
        .env("ISEKAI_PIPE_LOG_FILE", home.join("isekai-ssh-verbose.log"))
        .env("ISEKAI_PIPE_RUNTIME_DIR", runtime_dir)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn isekai-ssh")
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_authenticates_when_no_usable_identity_file_is_configured() {
    let tmp = tempfile::tempdir().unwrap();
    let (_key_path, client_pubkey, client_private_key) = generate_client_keypair(tmp.path());

    let pipe_name = format!(r"\\.\pipe\isekai-agent-test-{}", std::process::id());
    spawn_seeded_agent(&pipe_name, &client_private_key).await;

    let connection_count = Arc::new(AtomicUsize::new(0));
    let (mock_sshd_addr, mock_sshd_fingerprint) = spawn_agent_only_ssh_server(client_pubkey, connection_count.clone()).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    seed_ssh_host_key_trust(&home, &format!("127.0.0.1:{}", mock_sshd_addr.port()), &mock_sshd_fingerprint);

    let alias = "native-agent-auth-e2e";
    // Deliberately no `IdentityFile` — only `IdentityAgent` can produce a
    // usable credential here, so a pass proves the agent path is what
    // actually authenticated, not a fallback.
    let host_block = format!(
        "Host {alias}\n\
         \x20\x20\x20\x20HostName 127.0.0.1\n\
         \x20\x20\x20\x20Port {port}\n\
         \x20\x20\x20\x20User tester\n\
         \x20\x20\x20\x20IdentityAgent {pipe_name}\n",
        port = mock_sshd_addr.port(),
    );
    let home_ssh_dir = home.join(".ssh");
    std::fs::create_dir_all(&home_ssh_dir).unwrap();
    std::fs::write(home_ssh_dir.join("config"), &host_block).unwrap();

    let helper = spawn_real_helper(mock_sshd_addr);
    let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap()).parse().unwrap();
    let cert_sha256_hex = helper.handshake.cert_sha256().to_string();
    let profile_key = isekai_trust::normalize_host_port(alias).unwrap();
    register_correct_profile(&profiles_dir_under(&home), &profile_key, helper_addr, &cert_sha256_hex, &helper.handshake.session_secret);

    let runtime_dir = tmp.path().join("runtime");
    std::fs::create_dir_all(&runtime_dir).unwrap();

    let mut tab = spawn_tab(&home, &isekai_pipe_bin_path(), &runtime_dir, alias);
    wait_for_ready(&mut tab, Duration::from_secs(30), "agent-auth tab").await;
    assert_eq!(connection_count.load(Ordering::SeqCst), 1, "the mock sshd (which accepts only the agent-held key) must have accepted exactly one connection authenticated via the agent");

    let _ = tab.start_kill();
    // `helper` (a `HelperProcess`) kills its child in `Drop`.
}
