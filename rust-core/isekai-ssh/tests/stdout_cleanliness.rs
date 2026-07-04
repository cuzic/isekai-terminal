//! Consolidated "stdout purity" test suite for `isekai-ssh connect`
//! (`ISEKAI_SSH_DESIGN.md` フェーズ分割案 S-X: "connectの正常系・trust未登録・
//! auth未設定・relay失敗・bootstrap失敗の全ケースでSSHペイロード以外がstdoutに
//! 1バイトも出ないことを検証").
//!
//! `connect` is invoked as `ssh`'s `ProxyCommand`, so its stdout must never
//! carry anything but raw bytes read from the QUIC stream to isekai-helper
//! (`connect.rs`'s own module docs call this the load-bearing invariant of
//! that whole module). This file is the single place that walks every path
//! `connect` (`rust-core/isekai-ssh/src/connect.rs`, read as of this
//! writing) can actually take today, and asserts that invariant end to end
//! by spawning the real `isekai-ssh connect` subprocess *directly* — via
//! this test's own `Stdio::piped()`, not through a real `ssh` process — and
//! inspecting its raw stdout bytes.
//!
//! ## Scope: what `connect` can actually fail on today
//!
//! Reading `connect.rs` (`run` / `resolve_target` / `resolve_from_trust_store`),
//! `connect` has exactly two decision points before it ever touches stdout:
//!
//! 1. **Trust store lookup** (`resolve_from_trust_store`) — pure, no network
//!    I/O. An unregistered host fails closed via the `TrustNotInitialized`
//!    marker error before any socket is even opened.
//! 2. **`isekai_transport::connect_via_relay`** — QUIC connect (with
//!    cert-pin verification) followed by the HELLO/proof/ACK exchange. Any
//!    failure here (wrong `cached_cert_sha256`, wrong `cached_session_secret`,
//!    an unreachable `cached_relay_addr`) surfaces as an `anyhow::Error`
//!    from `run` — never a partial stdout write, because `relay_stdio` (the
//!    *only* stdout writer in this module) is called only after
//!    `connect_via_relay` has already returned `Ok`.
//!
//! `connect` does **not** use `isekai-auth` at all — JWT acquisition is
//! entirely `init`'s concern (a `--relay-jwt` obtained out of band and
//! passed to `init` explicitly; see `cli.rs`'s `InitArgs::relay_jwt` doc
//! comment), and `connect::run` never calls into `isekai_auth`. Nor does
//! `connect` wire up any `--via`-driven automatic re-deployment fallback
//! yet: `ConnectArgs::via` is accepted (so existing `~/.ssh/config` entries
//! already parse) but is **not read anywhere in `connect.rs`** — the doc
//! comment on that field and `ISEKAI_SSH_DESIGN.md`'s "CLIコマンド構成"
//! section both explicitly reserve that wiring for S-3.
//!
//! So, as of this writing, "auth未設定" and "bootstrap失敗" are not real
//! `connect` code paths — there is nothing there yet to fail-closed-test,
//! and inventing a synthetic scenario for either would not exercise any
//! actual logic in `connect.rs`. **This is a deliberate omission, not an
//! oversight**: once S-3 wires up `--via` re-deployment and/or `connect`
//! starts consuming `isekai-auth`, add the corresponding cases here.
//!
//! ## Why this file duplicates rather than shares helpers
//!
//! `init_e2e.rs` documents this crate's existing convention: one
//! self-contained end-to-end test file per scenario, with the mock-sshd /
//! binary-locating plumbing duplicated across files rather than factored
//! into a shared `tests/` support module. This file follows that same
//! convention (its mock sshd and trust-store fixture helpers are adapted
//! from `trust_store_e2e.rs`), so it can be read and understood on its own.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio as StdStdio;
use std::time::Duration;

use base64::Engine as _;
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
// Shared plumbing (adapted from trust_store_e2e.rs; see this file's module
// docs for why it's duplicated rather than factored out).
// ---------------------------------------------------------------------

/// Only the two success-path tests need `ssh-keygen(1)` (to mint the mock
/// sshd's accepted client key) — every fail-closed/relay-failure test never
/// runs a real `ssh` client and doesn't need it. Mirrors
/// `trust_store_e2e.rs::ssh_binary_available`'s technique: just prove the
/// binary can be spawned at all, ignoring its exit code (an unrecognized
/// flag is still fine for an availability probe).
fn ssh_keygen_available() -> bool {
    std::process::Command::new("ssh-keygen")
        .arg("--help")
        .stdin(StdStdio::null())
        .stdout(StdStdio::null())
        .stderr(StdStdio::null())
        .status()
        .is_ok()
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
            stdin_senders: HashMap::new(),
        }
    }
}

struct FakeShellHandler {
    home: PathBuf,
    accepted_client_key: PublicKey,
    stdin_senders: HashMap<ChannelId, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
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

/// A real `russh::server`, speaking the real SSH wire protocol (sends its
/// version-identification banner unprompted, per RFC 4253 §4.2, the instant
/// a TCP connection lands). `isekai-helper` treats this purely as an opaque
/// `--target` TCP endpoint.
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

/// Generates a fresh ed25519 keypair via the system `ssh-keygen(1)`. Only
/// used to populate `FakeShellServer::accepted_client_key`'s type — no test
/// in this file ever actually completes a publickey auth exchange (they all
/// capture `isekai-ssh connect`'s own stdout directly, never running a real
/// `ssh` client), so the *value* is never checked, only its type.
fn generate_client_keypair(dir: &Path) -> PublicKey {
    let key_path = dir.join("client_id_ed25519");
    let status = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "", "-q", "-f"])
        .arg(&key_path)
        .status()
        .expect("failed to run ssh-keygen");
    assert!(status.success(), "ssh-keygen exited non-zero");

    let pub_path = dir.join("client_id_ed25519.pub");
    let pub_text = std::fs::read_to_string(&pub_path).expect("failed to read generated .pub file");
    PublicKey::from_openssh(pub_text.trim()).expect("failed to parse generated public key")
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

/// Spawns the real compiled `isekai-helper` binary, `--target`ing
/// `target_addr` directly (no `--relay`). For the relay-failure scenarios in
/// this file, `target_addr` is never actually dialed by isekai-helper (the
/// HELLO/proof check that isekai-ssh's tampered trust-store entries are
/// designed to fail happens *before* isekai-helper would connect to its
/// target — see `isekai-helper/src/main.rs::handle_stream`), so a
/// definitely-nothing-listening address is fine there.
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
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    use std::io::BufRead;
    reader.read_line(&mut line).expect("failed to read handshake line from isekai-helper stdout");
    let handshake = decode_handshake_json(line.trim().as_bytes()).expect("failed to parse/validate handshake JSON");

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

// ---------------------------------------------------------------------
// Trust store fixture helpers
// ---------------------------------------------------------------------

/// `$HOME/.config/isekai-ssh/known_helpers.toml`, mirroring
/// `isekai_trust::store::default_trust_store_path`'s own layout.
fn trust_store_path_under(home: &Path) -> PathBuf {
    home.join(".config").join(isekai_trust::store::CONFIG_DIR_NAME).join(isekai_trust::store::TRUST_STORE_FILE_NAME)
}

fn sample_trust_entry(cached_relay_addr: SocketAddr, cached_cert_sha256: String, cached_session_secret: String) -> HelperTrust {
    HelperTrust {
        identity_pubkey: "pk-test".to_string(),
        trusted_helper_sha256: "a".repeat(64),
        trusted_helper_version: "0.0.0-test".to_string(),
        update_policy: UpdatePolicy::ExactDigestOnly,
        release_channel: None,
        last_via: None,
        trusted_at: "2026-07-04T00:00:00Z".to_string(),
        last_seen_at: "2026-07-04T00:00:00Z".to_string(),
        cached_relay_addr: cached_relay_addr.to_string(),
        cached_cert_sha256,
        cached_session_secret,
    }
}

/// Writes a single-entry trust store for `host` under `home`, the same way
/// `isekai-ssh init` (S-3, not implemented yet) eventually will.
fn register_trust(home: &Path, host: &str, entry: HelperTrust) {
    let trust_store_path = trust_store_path_under(home);
    let key = isekai_trust::normalize_host_port(host).unwrap();
    let mut store = TrustStore::default();
    store.insert(key, entry);
    isekai_trust::save_trust_store(&trust_store_path, &store).expect("failed to write trust store fixture");
}

/// Runs `isekai-ssh connect <host>` directly (this test's own
/// `Stdio::piped()`, *not* through a real `ssh` process) and waits for it to
/// exit on its own, bounded by a generous timeout. Only suitable for paths
/// that fail closed quickly (trust store miss, TLS cert-pin rejection,
/// HELLO/proof rejection) — the "unreachable relay address" scenario uses
/// its own kill-early variant below instead, since it has no natural quick
/// exit to wait for.
async fn run_connect_to_completion(home: &Path, host: &str, rust_log: Option<&str>) -> std::process::Output {
    let mut cmd = TokioCommand::new(isekai_ssh_bin_path());
    cmd.arg("connect")
        .arg(host)
        .env("HOME", home)
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped());
    match rust_log {
        Some(level) => {
            cmd.env("RUST_LOG", level);
        }
        None => {
            cmd.env_remove("RUST_LOG");
        }
    }

    tokio::time::timeout(Duration::from_secs(15), cmd.output())
        .await
        .expect("connect should fail closed quickly on this path, not hang")
        .expect("failed to spawn isekai-ssh connect")
}

/// Asserts none of the log-marker substrings `connect_stdout_is_pure_ssh_bytes_even_with_trace_logging`
/// (`connect_e2e.rs`) checks for ever leak into `stdout_bytes`.
fn assert_no_log_markers_in_stdout(stdout_bytes: &[u8]) {
    let stdout_lossy = String::from_utf8_lossy(stdout_bytes);
    for marker in ["isekai_ssh", "isekai-ssh:", "TRACE", "DEBUG "] {
        assert!(
            !stdout_lossy.contains(marker),
            "stdout polluted with log-like text (found {marker:?}); full stdout: {stdout_lossy:?}"
        );
    }
}

// ---------------------------------------------------------------------
// Scenario 1: success (real mock sshd + real isekai-helper + trust store
// registered) — direct capture of `isekai-ssh connect`'s own stdout.
// ---------------------------------------------------------------------

/// Spawns `isekai-ssh connect <host>` with piped stdio and reads from its
/// stdout until at least one full line has arrived (the mock sshd's banner
/// ends in `\r\n`) or a generous timeout elapses, before force-killing it and
/// returning whatever made it to stdout/stderr. Polling for actual output
/// rather than sleeping a fixed duration keeps this fast on an idle machine
/// and non-flaky on a loaded one (e.g. running alongside other `cargo test`
/// jobs).
///
/// Deliberately does **not** close our end of `child`'s stdin (unlike
/// `connect_e2e.rs`'s equivalent trace test): `relay_stdio`
/// (`connect.rs`) races its stdin->QUIC and QUIC->stdout copy tasks via
/// `tokio::select!` and aborts whichever loses, so closing stdin
/// immediately makes the stdin->QUIC side likely to hit EOF and win that
/// race *before* the QUIC->stdout side has had a chance to relay the
/// banner it's concurrently receiving — observed directly (empirically
/// ~30-40% of runs) as this test flaking with a completely empty stdout
/// despite the HELLO/ACK handshake having genuinely succeeded (confirmed
/// via `RUST_LOG=debug` on a failing run). Keeping stdin open removes that
/// race entirely: the stdin->QUIC side then simply blocks forever waiting
/// for input that never comes, so only the QUIC->stdout side can ever
/// finish (by us killing the process once we've read the banner).
async fn capture_success_stdout(home: &Path, host: &str, rust_log: Option<&str>) -> std::process::Output {
    let mut cmd = TokioCommand::new(isekai_ssh_bin_path());
    cmd.arg("connect")
        .arg(host)
        .env("HOME", home)
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped());
    match rust_log {
        Some(level) => {
            cmd.env("RUST_LOG", level);
        }
        None => {
            cmd.env_remove("RUST_LOG");
        }
    }
    let mut child = cmd.spawn().expect("failed to spawn isekai-ssh connect");
    // Keep `child.stdin` alive (still `Some(..)` inside `child`) rather than
    // taking and dropping it — see this function's doc comment above.
    let mut stdout = child.stdout.take().expect("stdout piped");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut stdout_buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stdout.read(&mut chunk)).await {
            Ok(Ok(0)) => break,     // EOF
            Ok(Ok(n)) => {
                stdout_buf.extend_from_slice(&chunk[..n]);
                if stdout_buf.contains(&b'\n') {
                    break; // got (at least) the banner line
                }
            }
            Ok(Err(_)) | Err(_) => break, // read error or timed out waiting
        }
    }

    let _ = child.start_kill();
    let mut stderr = child.stderr.take().expect("stderr piped");
    let mut stderr_buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stderr.read_to_end(&mut stderr_buf)).await;
    let status = child.wait().await.expect("failed to wait for isekai-ssh connect");

    std::process::Output { status, stdout: stdout_buf, stderr: stderr_buf }
}

async fn success_fixture() -> (tempfile::TempDir, PathBuf, HelperProcess) {
    let tmp = tempfile::tempdir().unwrap();
    let client_pubkey = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home, client_pubkey).await;
    let helper = spawn_helper(mock_sshd_addr);
    let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port).parse().unwrap();

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    register_trust(
        &home,
        "success-host",
        sample_trust_entry(helper_addr, helper.handshake.cert_sha256.clone(), helper.handshake.session_secret.clone()),
    );

    (tmp, home, helper)
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_relays_only_raw_bytes_on_success_default_logging() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen(1) not available in this environment");
        return;
    }
    let (_tmp, home, _helper) = success_fixture().await;

    let output = capture_success_stdout(&home, "success-host", None).await;

    assert!(
        output.stdout.starts_with(b"SSH-2.0-"),
        "expected the mock sshd's real SSH version banner on stdout, got {:?}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_no_log_markers_in_stdout(&output.stdout);
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_relays_only_raw_bytes_on_success_with_trace_logging() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen(1) not available in this environment");
        return;
    }
    let (_tmp, home, _helper) = success_fixture().await;

    let output = capture_success_stdout(&home, "success-host", Some("trace")).await;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.is_empty(), "expected RUST_LOG=trace to actually produce stderr output, got none");
    assert!(stderr.contains("isekai_ssh") || stderr.contains("isekai-ssh"), "expected isekai-ssh's own log lines on stderr, got: {stderr}");

    assert!(
        output.stdout.starts_with(b"SSH-2.0-"),
        "expected the mock sshd's real SSH version banner on stdout even under RUST_LOG=trace, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_no_log_markers_in_stdout(&output.stdout);
}

// ---------------------------------------------------------------------
// Scenario 2: trust store miss (fail closed before any network I/O).
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn connect_stdout_empty_for_unregistered_host_default_logging() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // Deliberately do *not* create known_helpers.toml at all — the normal
    // "never initialized" state.

    let output = run_connect_to_completion(&home, "unknown-host", None).await;

    assert!(output.stdout.is_empty(), "stdout must stay empty for an unregistered host, got {:?}", String::from_utf8_lossy(&output.stdout));
    assert!(!output.status.success(), "connect must exit non-zero for an unregistered host");
    // main.rs::EXIT_TRUST_NOT_INITIALIZED.
    assert_eq!(output.status.code(), Some(10), "expected the dedicated trust-not-initialized exit code");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("isekai-ssh init"), "stderr should point the user at `isekai-ssh init`, got: {stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_stdout_empty_for_unregistered_host_with_trace_logging() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let output = run_connect_to_completion(&home, "unknown-host", Some("trace")).await;

    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty for an unregistered host even under RUST_LOG=trace, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!output.status.success(), "connect must exit non-zero for an unregistered host");
    assert_eq!(output.status.code(), Some(10));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.is_empty(), "expected RUST_LOG=trace to actually produce stderr output, got none");
}

// ---------------------------------------------------------------------
// Scenario 3: registered but broken relay credentials/address — the
// HELLO/proof/ACK exchange (or the TLS handshake underneath it) fails, but
// stdout must never see a single byte because `relay_stdio` only runs after
// `connect_via_relay` returns `Ok` (see `connect.rs::run`).
// ---------------------------------------------------------------------

fn dummy_unused_target() -> SocketAddr {
    // Never actually dialed by isekai-helper in either of the two
    // credential-mismatch scenarios below: the HELLO/proof check that's
    // designed to fail happens strictly before isekai-helper would connect
    // to its `--target` (`isekai-helper/src/main.rs::handle_stream`).
    "127.0.0.1:1".parse().unwrap()
}

async fn assert_relay_failure_stdout_stays_empty(home: &Path, host: &str, rust_log: Option<&str>) {
    let output = run_connect_to_completion(home, host, rust_log).await;

    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty when the relay handshake fails (host={host}, RUST_LOG={rust_log:?}), got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!output.status.success(), "connect must exit non-zero when the relay handshake fails");
    // main.rs::EXIT_OTHER_ERROR — this is not a trust-store-miss, so it must
    // *not* get the dedicated exit code 10 from the previous scenario.
    assert_eq!(output.status.code(), Some(1));

    if rust_log == Some("trace") {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.is_empty(), "expected RUST_LOG=trace to actually produce stderr output, got none");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_stdout_empty_when_cached_cert_sha256_is_wrong_default_logging() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let helper = spawn_helper(dummy_unused_target());
    let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port).parse().unwrap();
    // Deliberately wrong: a well-formed-looking but different fingerprint,
    // so the client's cert-pin check rejects the real (correct) cert the
    // helper presents.
    register_trust(&home, "cert-mismatch-host", sample_trust_entry(helper_addr, "f".repeat(64), helper.handshake.session_secret.clone()));

    assert_relay_failure_stdout_stays_empty(&home, "cert-mismatch-host", None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_stdout_empty_when_cached_cert_sha256_is_wrong_with_trace_logging() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let helper = spawn_helper(dummy_unused_target());
    let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port).parse().unwrap();
    register_trust(&home, "cert-mismatch-host", sample_trust_entry(helper_addr, "f".repeat(64), helper.handshake.session_secret.clone()));

    assert_relay_failure_stdout_stays_empty(&home, "cert-mismatch-host", Some("trace")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_stdout_empty_when_cached_session_secret_is_wrong_default_logging() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let helper = spawn_helper(dummy_unused_target());
    let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port).parse().unwrap();
    // Cert fingerprint is correct (TLS succeeds) but the session secret is
    // wrong, so the HELLO proof isekai-ssh computes won't match what
    // isekai-helper expects -> RejectAuth.
    let wrong_secret = base64::engine::general_purpose::STANDARD.encode([0xEEu8; 32]);
    register_trust(&home, "secret-mismatch-host", sample_trust_entry(helper_addr, helper.handshake.cert_sha256.clone(), wrong_secret));

    assert_relay_failure_stdout_stays_empty(&home, "secret-mismatch-host", None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_stdout_empty_when_cached_session_secret_is_wrong_with_trace_logging() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let helper = spawn_helper(dummy_unused_target());
    let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port).parse().unwrap();
    let wrong_secret = base64::engine::general_purpose::STANDARD.encode([0xEEu8; 32]);
    register_trust(&home, "secret-mismatch-host", sample_trust_entry(helper_addr, helper.handshake.cert_sha256.clone(), wrong_secret));

    assert_relay_failure_stdout_stays_empty(&home, "secret-mismatch-host", Some("trace")).await;
}

/// Unlike the two scenarios above, an unreachable address has no fast,
/// well-defined failure — the QUIC client just keeps retransmitting into the
/// void until its own internal timeout. Rather than wait that out, this
/// binds a local UDP socket that never responds and is never read from (a
/// deterministic local "black hole" — no ICMP port-unreachable is generated
/// because *something* is bound there), then kills `connect` after a short
/// window and asserts stdout stayed empty the whole time: the invariant
/// under test ("no stdout write before `connect_via_relay` resolves") holds
/// regardless of whether/when the attempt eventually gives up.
async fn assert_unreachable_relay_stdout_stays_empty(rust_log: Option<&str>) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let black_hole = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let black_hole_addr = black_hole.local_addr().unwrap();

    register_trust(
        &home,
        "unreachable-host",
        sample_trust_entry(black_hole_addr, "a".repeat(64), base64::engine::general_purpose::STANDARD.encode([0u8; 32])),
    );

    let mut cmd = TokioCommand::new(isekai_ssh_bin_path());
    cmd.arg("connect")
        .arg("unreachable-host")
        .env("HOME", &home)
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped());
    match rust_log {
        Some(level) => {
            cmd.env("RUST_LOG", level);
        }
        None => {
            cmd.env_remove("RUST_LOG");
        }
    }
    let mut child = cmd.spawn().expect("failed to spawn isekai-ssh connect");

    tokio::time::sleep(Duration::from_millis(1000)).await;
    let _ = child.start_kill();
    let output = child.wait_with_output().await.expect("failed to wait for isekai-ssh connect");

    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty while a QUIC connect attempt to an unreachable relay address is still in flight, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );

    // Keep the black hole alive for the whole attempt window so the port
    // stays bound (nothing responds, but nothing generates an ICMP
    // unreachable either).
    drop(black_hole);
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_stdout_empty_when_cached_relay_addr_is_unreachable_default_logging() {
    assert_unreachable_relay_stdout_stays_empty(None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_stdout_empty_when_cached_relay_addr_is_unreachable_with_trace_logging() {
    assert_unreachable_relay_stdout_stays_empty(Some("trace")).await;
}
