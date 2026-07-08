//! stdout purity for `isekai-pipe connect`/`isekai-pipe serve`
//! (`archive/ISEKAI_PIPE_MIGRATION.md` P4's last bullet: "OpenSSH の byte stream は
//! `isekai-pipe connect` の stdout のみ"), mirroring the test shape
//! `isekai-ssh/tests/stdout_cleanliness.rs` already established for the
//! legacy `isekai-ssh connect` subcommand.
//!
//! `connect`'s only stdout writer is `pump_h2c`/`relay_stdio` (`main.rs`),
//! and both are only ever reached after the QUIC HELLO/proof/ACK exchange
//! has already succeeded -- every failure before that point (missing trust
//! store entry, wrong cached credentials) must surface as a non-zero exit
//! with stderr-only diagnostics and a completely untouched stdout.
//!
//! `serve` is a thin CLI translation layer over the `engine` module's
//! `run_from_args` (formerly the standalone `isekai-helper` crate,
//! `archive/ISEKAI_PIPE_MIGRATION.md` P5)
//! (`main.rs::serve_command`); its stdout contract is the single-line
//! handshake JSON documented in `archive/HELPER_PROTOCOL.md` §2 -- nothing else may
//! ever reach stdout, on this process's own well-formed startup path.

use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

use base64::Engine as _;
use isekai_trust::schema::{HelperTrust, TrustStore, UpdatePolicy};
use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;

fn isekai_pipe_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-pipe"))
}

fn trust_store_path_under(home: &std::path::Path) -> PathBuf {
    home.join(".config")
        .join(isekai_trust::store::CONFIG_DIR_NAME)
        .join(isekai_trust::store::TRUST_STORE_FILE_NAME)
}

fn sample_trust_entry(
    cached_relay_addr: std::net::SocketAddr,
    cached_cert_sha256: String,
    cached_session_secret: String,
) -> HelperTrust {
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

fn register_trust(home: &std::path::Path, host: &str, entry: HelperTrust) {
    let trust_store_path = trust_store_path_under(home);
    let key = isekai_trust::normalize_host_port(host).unwrap();
    let mut store = TrustStore::default();
    store.insert(key, entry);
    isekai_trust::save_trust_store(&trust_store_path, &store)
        .expect("failed to write trust store fixture");
}

async fn run_connect_to_completion(
    home: &std::path::Path,
    profile: &str,
    rust_log: Option<&str>,
) -> std::process::Output {
    let mut cmd = TokioCommand::new(isekai_pipe_bin_path());
    cmd.args(["connect", "--profile", profile, "--service", "ssh", "--stdio"])
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
        .expect("connect should fail closed quickly on these paths, not hang")
        .expect("failed to spawn isekai-pipe connect")
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_stdout_empty_for_untrusted_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // Deliberately no known_helpers.toml at all.

    let output = run_connect_to_completion(&home, "unknown-profile", None).await;

    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty for an untrusted profile, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!output.status.success(), "connect must exit non-zero for an untrusted profile");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not trusted"),
        "stderr should explain the trust-store miss, got: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_stdout_empty_for_untrusted_profile_with_trace_logging() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let output = run_connect_to_completion(&home, "unknown-profile", Some("trace")).await;

    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty for an untrusted profile even under RUST_LOG=trace, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!output.status.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_stdout_empty_when_cached_session_secret_is_wrong() {
    // A well-formed but unreachable relay address is fine: the wrong secret
    // means the HELLO proof isekai-pipe computes cannot possibly be
    // accepted, and the point of this test is only that connect never
    // writes to stdout before that (or any) relay outcome is known.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let black_hole = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let black_hole_addr = black_hole.local_addr().unwrap();
    register_trust(
        &home,
        "secret-mismatch-host",
        sample_trust_entry(
            black_hole_addr,
            "a".repeat(64),
            base64::engine::general_purpose::STANDARD.encode([0xEEu8; 32]),
        ),
    );

    let mut cmd = TokioCommand::new(isekai_pipe_bin_path());
    cmd.args([
        "connect",
        "--profile",
        "secret-mismatch-host",
        "--service",
        "ssh",
        "--stdio",
    ])
    .env("HOME", &home)
    .stdin(StdStdio::null())
    .stdout(StdStdio::piped())
    .stderr(StdStdio::piped());
    let mut child = cmd.spawn().expect("failed to spawn isekai-pipe connect");

    tokio::time::sleep(Duration::from_millis(800)).await;
    let _ = child.start_kill();
    let output = child.wait_with_output().await.expect("failed to wait for isekai-pipe connect");

    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty while a QUIC connect attempt to an unreachable relay address is \
         still in flight, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    drop(black_hole);
}

// ---------------------------------------------------------------------
// serve: stdout must contain exactly the one-line handshake JSON.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn serve_stdout_is_a_single_handshake_json_line() {
    let mut cmd = TokioCommand::new(isekai_pipe_bin_path());
    cmd.args(["serve", "--service", "ssh=127.0.0.1:1", "--bind", "127.0.0.1:0", "--once"])
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped());
    let mut child = cmd.spawn().expect("failed to spawn isekai-pipe serve");

    let mut stdout = child.stdout.take().unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stdout.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') {
                    break;
                }
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.lines();
    let first_line = lines.next().expect("serve should print a handshake JSON line to stdout");
    let handshake = isekai_protocol::handshake::decode_handshake_json(first_line.as_bytes())
        .expect("first stdout line must be valid handshake JSON");
    assert_eq!(handshake.protocol.alpn, "isekai-pipe/1");
    assert_eq!(
        lines.next(),
        None,
        "no second line: serve's stdout must carry only the handshake JSON, got: {text:?}"
    );
}
