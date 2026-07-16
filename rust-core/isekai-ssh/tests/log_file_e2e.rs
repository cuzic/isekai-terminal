//! `--isekai-log-file <PATH>` (`log_file.rs`): confirms the resulting file
//! actually accumulates *both* sources `log_file.rs`'s docs promise —
//! `isekai-ssh`'s own status messages (`wrapper.rs`'s `log_line!` calls) and
//! `ssh(1)`'s (here, standing in for its `isekai-pipe connect` `ProxyCommand`
//! grandchild's `env_logger` output too) piped stderr — and that neither
//! reaches the terminal (this test's own captured stderr, standing in for a
//! real terminal) at all while `--isekai-log-file` is active: this is a
//! redirect, not a tee.
//!
//! Reuses the "already-trusted destination whose cached `session_secret`
//! doesn't match the real, currently-running `isekai-pipe serve`" shape
//! `wrapper_stale_trust_auto_recovery_e2e.rs` already established as a fast,
//! deterministic way to reach a real `run_ssh_once` call (so a real `ssh(1)`
//! child actually gets spawned and its stderr actually flows) without
//! needing a mock-sshd deploy step at all — `--isekai-no-bootstrap` stops
//! the wrapper right after the first (real) `ssh` attempt fails, no
//! redeploy ever attempted. Per this crate's self-contained-test-file
//! convention, `spawn_real_helper`/`register_stale_profile` are duplicated
//! from that file rather than shared.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

use base64::Engine as _;
use isekai_protocol::handshake::{decode_handshake_json, HandshakeJson};
use isekai_trust::{HelperTrust, UpdatePolicy};
use tokio::process::Command as TokioCommand;

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

fn isekai_pipe_bin_path() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let is_release = path.file_name().map(|n| n == "release").unwrap_or(false);
    // Windows binaries carry a `.exe` extension; a bare `isekai-pipe` never
    // exists there, so this would otherwise always fall through to the
    // rebuild-and-recheck path below and still fail the same `path.exists()`
    // check afterward (confirmed via a real `test-windows` CI failure on
    // this same bug in `doctor_e2e.rs`).
    path.push(if cfg!(windows) { "isekai-pipe.exe" } else { "isekai-pipe" });

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
    cmd.arg("serve")
        .arg("--target")
        .arg(target_addr.to_string())
        .arg("--bind")
        .arg("127.0.0.1:0")
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn isekai-pipe serve");
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read handshake line from isekai-pipe serve stdout");
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

fn profiles_dir_under(home: &std::path::Path) -> PathBuf {
    home.join(".local").join("state").join("isekai").join("profiles")
}

fn register_stale_profile(profiles_dir: &std::path::Path, key: &str, helper_addr: SocketAddr, real_cert_sha256_hex: &str, wrong_session_secret_b64: &str) {
    let trust = HelperTrust {
        identity_pubkey: real_cert_sha256_hex.to_string(),
        trusted_helper_sha256: "a".repeat(64),
        trusted_helper_version: "test".to_string(),
        update_policy: UpdatePolicy::ExactDigestOnly,
        release_channel: None,
        last_via: None,
        trusted_at: "2026-07-11T00:00:00Z".to_string(),
        last_seen_at: "2026-07-11T00:00:00Z".to_string(),
        cached_relay_addr: helper_addr.to_string(),
        cached_cert_sha256: real_cert_sha256_hex.to_string(),
        cached_session_secret: wrong_session_secret_b64.to_string(),
        cached_stun_observed_addr: None,
    };
    let profile = isekai_pipe_core::PersistentProfile::migrate_legacy_helper_trust(key, &trust);
    isekai_pipe_core::write_persistent_profile(profiles_dir, &profile).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn isekai_log_file_redirects_both_the_wrappers_own_messages_and_the_ssh_childs_stderr_away_from_the_terminal() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target_listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = target_listener.accept().await else { break };
            std::mem::forget(stream);
        }
    });
    let helper = spawn_real_helper(target_addr);
    let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap()).parse().unwrap();
    let real_cert = helper.handshake.cert_sha256().to_string();
    // Deliberately wrong session_secret -- real cert, so the QUIC/TLS
    // handshake succeeds and `isekai-pipe connect` (the real `ssh(1)`
    // child's `ProxyCommand`) fails specifically at the ATTACH proof stage,
    // logging a real "relay transport failed" line to its own stderr.
    let wrong_secret_b64 = base64::engine::general_purpose::STANDARD.encode([0xEEu8; 32]);

    let key = isekai_trust::normalize_host_port("log-file-host").unwrap();
    register_stale_profile(&profiles_dir_under(&home), &key, helper_addr, &real_cert, &wrong_secret_b64);

    let log_path = tmp.path().join("isekai-ssh.log");

    let output = tokio::time::timeout(
        Duration::from_secs(20),
        TokioCommand::new(isekai_ssh_bin_path())
            .arg("--isekai-no-bootstrap")
            .arg("--isekai-log-file")
            .arg(&log_path)
            .arg("log-file-host")
            .env("HOME", &home)
            // `isekai-pipe connect`'s own default level is `warn` (a client-CLI
            // noise-reduction default, unrelated to what this test checks) — set
            // `RUST_LOG=info` explicitly so the `quicmux::noq_backend` line this
            // test looks for below is guaranteed to fire regardless of that
            // default, since what's under test here is the redirect plumbing,
            // not the default log level.
            .env("RUST_LOG", "info")
            .stdin(StdStdio::null())
            .stdout(StdStdio::piped())
            .stderr(StdStdio::piped())
            .output(),
    )
    .await
    .expect("isekai-ssh should fail closed quickly, not hang")
    .expect("failed to spawn isekai-ssh");

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.trim().is_empty(),
        "the terminal (this test's own captured stderr) must see nothing at all while --isekai-log-file is \
         active -- this is a redirect, not a tee -- got:\n{stderr}"
    );

    let log_contents = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("expected --isekai-log-file to have created {}: {e}", log_path.display()));

    assert!(
        log_contents.contains("auto-bootstrap is disabled"),
        "expected the wrapper's own status message (wrapper.rs's log_line! calls) in the log file, got:\n{log_contents}"
    );
    // `quicmux::noq_backend`'s own `log::info!("quicmux(noq): connecting to
    // ...")` line -- deliberately not the (also-present) "auto-bootstrap is
    // disabled" wrapper message's embedded error-chain text (which itself
    // contains the string "isekai-pipe connect" as a `.context()` prefix,
    // copied from the `ConnectOutcome` side-channel file -- *not* proof the
    // child's live stderr was actually redirected). Only a genuinely
    // redirected child stderr can produce this raw, timestamped `env_logger`
    // line.
    assert!(
        log_contents.contains("quicmux::noq_backend"),
        "expected the real ssh(1) child's live stderr (carrying isekai-pipe connect's own env_logger output, not \
         just the wrapper's own error-summary text) to have been redirected into the log file too, got:\n{log_contents}"
    );
}
