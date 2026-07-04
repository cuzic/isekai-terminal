//! End-to-end tests for `isekai-ssh connect --mode stun`
//! (`ISEKAI_SSH_DESIGN.md` フェーズ分割案 S-6's acceptance criteria).
//!
//! Mirrors `trust_store_e2e.rs`'s "registered host" scenario (real mock
//! `russh::server` sshd, real compiled `isekai-helper` binary, real `ssh(1)`)
//! but exercises the STUN+SSH-rendezvous P2P path instead of relay: a real
//! mock STUN server (same RFC 5389 responder `isekai-transport`'s and
//! `isekai-helper`'s own test suites use) plus `isekai-helper` started with
//! `--stun-server`/`--punch-peer` (confirmed to exist via `isekai-helper
//! --help`; see `HELPER_PROTOCOL.md` and `isekai-helper/tests/e2e.rs`'s
//! `punch_peer_flag_does_not_prevent_normal_startup_or_relay` for why a
//! dummy, unreachable `--punch-peer` value is fine — the probes it sends are
//! fire-and-forget and never gate startup or the relay itself).
//!
//! This is loopback-only (no real NAT to punch through, `ISEKAI_SSH_DESIGN.md`
//! S-7 leaves real multi-network hole punching to a later phase), so it
//! proves the *code path* — `--mode stun` resolves the trust store's cached
//! fields as `StunP2pTarget`, calls `isekai_transport::connect_stun_p2p`,
//! completes HELLO/proof/ACK, and relays real SSH bytes both ways — not that
//! hole punching succeeds against a real NAT.
//!
//! Everything here runs against a plain, default-feature build of the
//! `isekai-ssh` binary (no `dev-insecure`), same as `trust_store_e2e.rs`.

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
// Shared plumbing (mirrors trust_store_e2e.rs; duplicated rather than
// factored into a shared test-support module, following that file's own
// precedent of keeping each integration-test binary self-contained).
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

/// Spawns `isekai-helper --target <target_addr> --bind 127.0.0.1:0`, plus any
/// caller-supplied extra args (this file always passes `--stun-server`/
/// `--punch-peer`, mirroring `isekai-helper/tests/e2e.rs`'s own
/// `spawn_helper` helper).
fn spawn_helper(target_addr: SocketAddr, extra_args: &[&str]) -> HelperProcess {
    let mut cmd = std::process::Command::new(isekai_helper_bin_path());
    cmd.arg("--target")
        .arg(target_addr.to_string())
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--log-level")
        .arg("debug")
        .args(extra_args)
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

/// Minimal mock STUN server (RFC 5389 Binding Request/Response): replies to
/// every Binding Request with a Binding Success Response whose
/// XOR-MAPPED-ADDRESS is the request's observed source address. Same shape as
/// `isekai-transport/tests/stun_p2p_e2e.rs`'s and
/// `isekai-helper/tests/e2e.rs`'s own mock STUN servers. Runs on a plain OS
/// thread with a blocking `std::net::UdpSocket` — `isekai-helper/tests/e2e.rs`'s
/// `spawn_mock_stun_server` docs explain why a `tokio::spawn`-based mock
/// wouldn't reliably get polled while `spawn_helper` above blocks the test's
/// own thread on a synchronous `read_line()`.
fn spawn_mock_stun_server() -> SocketAddr {
    let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        loop {
            let Ok((n, from)) = server.recv_from(&mut buf) else { break };
            if n < 20 {
                continue;
            }
            let transaction_id = &buf[8..20];
            let SocketAddr::V4(from_v4) = from else { continue };

            let magic_cookie: u32 = 0x2112_A442;
            let xport = from_v4.port() ^ ((magic_cookie >> 16) as u16);
            let xaddr = u32::from(*from_v4.ip()) ^ magic_cookie;

            let mut resp = Vec::with_capacity(32);
            resp.extend_from_slice(&0x0101u16.to_be_bytes()); // Binding Success Response
            resp.extend_from_slice(&12u16.to_be_bytes()); // 4(attr header) + 8(attr value)
            resp.extend_from_slice(&magic_cookie.to_be_bytes());
            resp.extend_from_slice(transaction_id);
            resp.extend_from_slice(&0x0020u16.to_be_bytes()); // XOR-MAPPED-ADDRESS
            resp.extend_from_slice(&8u16.to_be_bytes());
            resp.push(0);
            resp.push(0x01); // family: IPv4
            resp.extend_from_slice(&xport.to_be_bytes());
            resp.extend_from_slice(&xaddr.to_be_bytes());

            let _ = server.send_to(&resp, from);
        }
    });
    addr
}

// ---------------------------------------------------------------------
// Trust store fixture helpers
// ---------------------------------------------------------------------

fn trust_store_path_under(home: &std::path::Path) -> PathBuf {
    home.join(".config").join(isekai_trust::store::CONFIG_DIR_NAME).join(isekai_trust::store::TRUST_STORE_FILE_NAME)
}

/// Builds the trust store entry `--mode stun` reads: same three fields as
/// the relay-mode fixture in `trust_store_e2e.rs` (`cached_relay_addr`/
/// `cached_cert_sha256`/`cached_session_secret` — the schema does not change
/// between modes), but `cached_relay_addr` is populated with the peer's
/// *STUN-observed* address (`HandshakeJson::stun_observed_addr`, only present
/// because isekai-helper was started with `--stun-server`) instead of its
/// plain listen address — mirroring `isekai-ssh::connect::resolve_stun_from_trust_store`'s
/// documented field reinterpretation for `--mode stun`.
fn stun_trust_entry(peer_observed_addr: SocketAddr, handshake: &HandshakeJson) -> HelperTrust {
    HelperTrust {
        identity_pubkey: "pk-test".to_string(),
        trusted_helper_sha256: "a".repeat(64),
        trusted_helper_version: "0.0.0-test".to_string(),
        update_policy: UpdatePolicy::ExactDigestOnly,
        release_channel: None,
        last_via: None,
        trusted_at: "2026-07-04T00:00:00Z".to_string(),
        last_seen_at: "2026-07-04T00:00:00Z".to_string(),
        cached_relay_addr: peer_observed_addr.to_string(),
        cached_cert_sha256: handshake.cert_sha256.clone(),
        cached_session_secret: handshake.session_secret.clone(),
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

/// Acceptance criterion 1 (`ISEKAI_SSH_DESIGN.md` フェーズ分割案 S-6): a host
/// whose trust store entry holds STUN-mode credentials, reached via
/// `isekai-ssh connect <host> --mode stun --stun-server <addr>`, completes a
/// real HELLO/proof/ACK over `connect_stun_p2p` and relays a real `ssh(1)`
/// session through to the mock sshd — same "real chain" bar as
/// `trust_store_e2e.rs`'s relay-mode test, minus the relay hop.
#[tokio::test(flavor = "multi_thread")]
async fn connect_mode_stun_succeeds_with_trust_store_registered_host() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home, client_pubkey).await;
    let mock_stun_addr = spawn_mock_stun_server();

    // `--punch-peer` is given a deliberately unreachable dummy value: on
    // loopback there is no second real peer whose address we'd know ahead of
    // time (learning it out-of-band is exactly the "SSHブートストラップ経由での
    // アドレス交換の自動化" scope explicitly deferred past S-6), and
    // `isekai-helper/tests/e2e.rs::punch_peer_flag_does_not_prevent_normal_startup_or_relay`
    // already establishes that a dummy `--punch-peer` value doesn't prevent
    // normal startup or relay — the probes it triggers are fire-and-forget.
    let helper = spawn_helper(
        mock_sshd_addr,
        &["--stun-server", &mock_stun_addr.to_string(), "--punch-peer", "127.0.0.1:1"],
    );
    let peer_observed_addr: SocketAddr = helper
        .handshake
        .stun_observed_addr
        .as_deref()
        .expect("isekai-helper should report stun_observed_addr when started with --stun-server")
        .parse()
        .expect("stun_observed_addr should be a valid socket address");

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let trust_store_path = trust_store_path_under(&home);
    let key = isekai_trust::normalize_host_port("dummy-host").unwrap();
    assert_eq!(key, "dummy-host:22");
    let mut store = TrustStore::default();
    store.insert(key, stun_trust_entry(peer_observed_addr, &helper.handshake));
    isekai_trust::save_trust_store(&trust_store_path, &store).expect("failed to write trust store fixture");

    let proxy_command =
        format!("{} connect dummy-host --mode stun --stun-server {mock_stun_addr}", isekai_ssh_bin_path().display());

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
            .arg("echo hello-from-stun-mode")
            .env("HOME", &home)
            .output(),
    )
    .await
    .expect("ssh should not hang")
    .expect("failed to spawn ssh(1)");

    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("ssh stderr:\n{stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "ssh exited with {:?}; stdout={stdout:?} stderr={stderr}",
        output.status,
    );
    assert!(stdout.contains("hello-from-stun-mode"), "unexpected ssh stdout: {stdout:?}");

    // Acceptance criterion 3: the NAT-mapping-loss caveat must be surfaced on
    // stderr every time `--mode stun` is used (`connect.rs::run`'s
    // `eprintln!`, inherited here through `ssh`'s own stderr since
    // `ProxyCommand` subprocesses are not redirected away from it).
    assert!(
        stderr.contains("cannot recover from NAT mapping loss"),
        "connect should warn about the --mode stun NAT-mapping-loss caveat on stderr, got: {stderr}"
    );
}

/// Acceptance criterion 2: omitting `--mode` must still work (defaults to
/// relay) — already covered end-to-end by `trust_store_e2e.rs`'s
/// `connect_succeeds_with_trust_store_registered_host`; this just pins down
/// that `--mode stun` without `--stun-server` is rejected by argument
/// parsing before any trust store lookup or network I/O happens, so the two
/// modes can never be silently confused.
#[tokio::test(flavor = "multi_thread")]
async fn connect_mode_stun_without_stun_server_is_rejected_by_argument_parsing() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let output = tokio::time::timeout(
        Duration::from_secs(10),
        TokioCommand::new(isekai_ssh_bin_path())
            .arg("connect")
            .arg("dummy-host")
            .arg("--mode")
            .arg("stun")
            .env("HOME", &home)
            .env_remove("RUST_LOG")
            .stdin(StdStdio::null())
            .stdout(StdStdio::piped())
            .stderr(StdStdio::piped())
            .output(),
    )
    .await
    .expect("isekai-ssh connect should fail argument parsing quickly, not hang")
    .expect("failed to spawn isekai-ssh connect");

    assert!(output.stdout.is_empty(), "stdout must stay empty, got {:?}", String::from_utf8_lossy(&output.stdout));
    assert!(!output.status.success(), "connect must exit non-zero when --mode stun lacks --stun-server");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--stun-server"), "clap should mention the missing --stun-server, got: {stderr}");
}

/// Acceptance criterion 4: a HELLO/proof/ACK failure under `--mode stun`
/// produces wording distinct from the relay-mode failure message in
/// `trust_store_e2e.rs`'s sibling scenarios — specifically, it must point at
/// falling back to `--mode relay` rather than at re-running `isekai-ssh
/// init` (the relay-mode message's advice, which doesn't apply here: the
/// most likely cause of a STUN-mode failure is hole punching not succeeding,
/// not isekai-helper having restarted).
#[tokio::test(flavor = "multi_thread")]
async fn connect_mode_stun_failure_suggests_falling_back_to_relay_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let mock_stun_addr = spawn_mock_stun_server();

    // Nothing listens on this address: any real (non-loopback-reachable)
    // port works to force `connect_stun_p2p`'s HELLO to fail — no
    // isekai-helper process needs to run for this test.
    let unreachable_peer = "127.0.0.1:1".to_string();

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let trust_store_path = trust_store_path_under(&home);
    let key = isekai_trust::normalize_host_port("dummy-host").unwrap();
    let mut store = TrustStore::default();
    store.insert(
        key,
        HelperTrust {
            identity_pubkey: "pk-test".to_string(),
            trusted_helper_sha256: "a".repeat(64),
            trusted_helper_version: "0.0.0-test".to_string(),
            update_policy: UpdatePolicy::ExactDigestOnly,
            release_channel: None,
            last_via: None,
            trusted_at: "2026-07-04T00:00:00Z".to_string(),
            last_seen_at: "2026-07-04T00:00:00Z".to_string(),
            cached_relay_addr: unreachable_peer,
            cached_cert_sha256: "a".repeat(64),
            cached_session_secret: "MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=".to_string(),
        },
    );
    isekai_trust::save_trust_store(&trust_store_path, &store).expect("failed to write trust store fixture");

    let output = tokio::time::timeout(
        Duration::from_secs(20),
        TokioCommand::new(isekai_ssh_bin_path())
            .arg("connect")
            .arg("dummy-host")
            .arg("--mode")
            .arg("stun")
            .arg("--stun-server")
            .arg(mock_stun_addr.to_string())
            .env("HOME", &home)
            .env_remove("RUST_LOG")
            .stdin(StdStdio::null())
            .stdout(StdStdio::piped())
            .stderr(StdStdio::piped())
            .output(),
    )
    .await
    .expect("isekai-ssh connect should fail (not hang) when the STUN peer is unreachable")
    .expect("failed to spawn isekai-ssh connect");

    assert!(output.stdout.is_empty(), "stdout must stay empty, got {:?}", String::from_utf8_lossy(&output.stdout));
    assert!(!output.status.success(), "connect must exit non-zero when the STUN P2P handshake fails");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--mode relay"),
        "a --mode stun failure should suggest falling back to --mode relay, got: {stderr}"
    );
    assert!(
        stderr.contains("NAT越え"),
        "a --mode stun failure should mention NAT traversal not succeeding, got: {stderr}"
    );
    // Distinct from the relay-mode message's specific "isekai-helper may have
    // restarted" diagnosis (`connect.rs`'s `TargetSource::TrustStore,
    // ConnectMode::Relay` arm) — a STUN-mode failure is much more likely to
    // mean hole punching didn't succeed than that isekai-helper restarted.
    assert!(
        !stderr.contains("may have restarted since the last"),
        "a --mode stun failure should not reuse the relay-mode 'isekai-helper may have restarted' \
         diagnosis, got: {stderr}"
    );
}
