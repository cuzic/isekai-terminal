//! `isekai-pipe connect`'s `ConnectOutcome` side-channel (`ISEKAI_PIPE_DESIGN.md`
//! §8 Epic N's "always-connects" principle). Spawns a real `isekai-pipe
//! serve` process (real ephemeral session_secret + TLS cert, matching
//! production exactly — see `engine/mod.rs`'s "起動のたびにランダム生成する"
//! comment, the root cause this whole feature exists to paper over), then
//! deliberately registers a `PersistentProfile` whose `cached_session_secret`
//! doesn't match the real helper's — the same shape a helper restart
//! produces in practice. Confirms `connect` still writes zero bytes to
//! stdout (the hard, separately-tested stdout-purity invariant), exits
//! `EX_UNAVAILABLE`, and leaves a `ConnectOutcome::StaleTrust` file behind
//! for `isekai-ssh`'s wrapper to notice. A companion confirms a plain
//! unreachable target (no response at all, not a rejection) *also* leaves a
//! `ConnectOutcome` behind — every `run_connect` failure does, since a
//! failure here only ever happens before any SSH byte ever flows — but
//! classified `Unreachable`, not `StaleTrust` (that narrow classification
//! stays reserved for an explicit cert-pin-mismatch/Auth rejection,
//! `isekai-transport::TransportError::is_stale_trust_signal`). Both classes
//! drive the exact same `isekai-ssh` wrapper recovery action; only the
//! logged message differs (`wrapper.rs::outcome_summary`).

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

use base64::Engine as _;
use isekai_pipe_core::{
    write_connection_intent, write_persistent_profile, BootstrapProvenance, ConnectOutcomeClass, ConnectionIntent,
    IntentTransport, PersistentProfile, ServerIdentity,
};
use isekai_protocol::handshake::{decode_handshake_json, HandshakeJson};
use isekai_trust::{HelperTrust, UpdatePolicy};
use tokio::process::Command as TokioCommand;

fn isekai_pipe_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-pipe"))
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

/// Same shape as `probe_e2e.rs`'s `spawn_helper`, duplicated per this
/// crate's self-contained-test-file convention.
fn spawn_helper(target_addr: SocketAddr) -> HelperProcess {
    let mut cmd = std::process::Command::new(isekai_pipe_bin_path());
    cmd.arg("serve")
        .arg("--target")
        .arg(target_addr.to_string())
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--log-level")
        .arg("debug")
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

fn register_profile(profiles_dir: &std::path::Path, key: &str, helper_addr: SocketAddr, cert_sha256_hex: &str, session_secret_b64: &str) {
    let trust = HelperTrust {
        identity_pubkey: cert_sha256_hex.to_string(),
        trusted_helper_sha256: "a".repeat(64),
        trusted_helper_version: "test".to_string(),
        update_policy: UpdatePolicy::ExactDigestOnly,
        release_channel: None,
        last_via: None,
        trusted_at: "2026-07-09T00:00:00Z".to_string(),
        last_seen_at: "2026-07-09T00:00:00Z".to_string(),
        cached_relay_addr: helper_addr.to_string(),
        cached_cert_sha256: cert_sha256_hex.to_string(),
        cached_session_secret: session_secret_b64.to_string(),
        cached_stun_observed_addr: None,
    };
    let profile = PersistentProfile::migrate_legacy_helper_trust(key, &trust);
    write_persistent_profile(profiles_dir, &profile).unwrap();
}

/// Builds and writes a `ConnectionIntent` (fresh random `intent_id`) into
/// `runtime_dir`, mirroring `isekai-ssh/src/wrapper.rs::run()`'s own
/// intent-then-ProxyCommand sequencing — `connect` only ever writes a
/// `ConnectOutcome` when `ISEKAI_INTENT_ID` is set, i.e. when invoked this
/// way rather than via bare `--profile`.
fn write_intent(
    runtime_dir: &std::path::Path,
    profile: &str,
    helper_addr: SocketAddr,
    cert_sha256_hex: &str,
    session_secret_b64: &str,
) -> ConnectionIntent {
    let intent = ConnectionIntent::new(
        profile,
        "ssh",
        ServerIdentity { cert_sha256_hex: cert_sha256_hex.to_string() },
        IntentTransport::Relay {
            helper_addr: helper_addr.to_string(),
            server_name: "isekai-helper".to_string(),
            session_secret_b64: session_secret_b64.to_string(),
        },
        BootstrapProvenance::TrustStore { key: profile.to_string() },
    );
    write_connection_intent(runtime_dir, &intent).unwrap();
    intent
}

fn outcomes_dir(runtime_dir: &std::path::Path) -> PathBuf {
    runtime_dir.join("connect-outcomes")
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_records_a_stale_trust_outcome_when_the_cached_session_secret_is_wrong() {
    let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target_listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = target_listener.accept().await else { break };
            std::mem::forget(stream);
        }
    });

    let helper = spawn_helper(target_addr);
    let helper_addr: SocketAddr =
        format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap()).parse().unwrap();
    let real_cert = helper.handshake.cert_sha256().to_string();
    // Deliberately wrong session_secret (real cert, so the TLS handshake
    // succeeds and the failure comes specifically from the ATTACH proof
    // rejection — the exact shape a restarted isekai-pipe serve produces).
    let wrong_secret_b64 = base64::engine::general_purpose::STANDARD.encode([0xEEu8; 32]);

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let profiles_dir = home.join(".local").join("state").join("isekai").join("profiles");
    std::fs::create_dir_all(&profiles_dir).unwrap();
    let runtime_dir = tmp.path().join("runtime");
    std::fs::create_dir_all(&runtime_dir).unwrap();

    let key = isekai_trust::normalize_host_port("stale-secret-host").unwrap();
    register_profile(&profiles_dir, &key, helper_addr, &real_cert, &wrong_secret_b64);
    let intent = write_intent(&runtime_dir, "stale-secret-host", helper_addr, &real_cert, &wrong_secret_b64);

    let mut cmd = TokioCommand::new(isekai_pipe_bin_path());
    cmd.args(["connect", "--service", "ssh", "--stdio"])
        .env("HOME", &home)
        .env("ISEKAI_INTENT_ID", &intent.intent_id)
        .env("ISEKAI_PIPE_RUNTIME_DIR", &runtime_dir)
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped());

    let output = tokio::time::timeout(Duration::from_secs(15), cmd.output())
        .await
        .expect("connect should fail closed quickly on a wrong session_secret, not hang")
        .expect("failed to spawn isekai-pipe connect");

    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty on a stale-trust auth rejection, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!output.status.success(), "connect must exit non-zero on a stale-trust auth rejection");
    assert_eq!(output.status.code(), Some(69), "must exit EX_UNAVAILABLE, unchanged by this feature");

    let outcome_path = outcomes_dir(&runtime_dir).join(format!("{}.json", intent.intent_id));
    let bytes = std::fs::read(&outcome_path)
        .unwrap_or_else(|e| panic!("expected a ConnectOutcome at {}: {e}", outcome_path.display()));
    let outcome: isekai_pipe_core::ConnectOutcome = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(outcome.class, ConnectOutcomeClass::StaleTrust);
    assert_eq!(outcome.intent_id, intent.intent_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_records_an_unreachable_outcome_for_a_plain_unreachable_target() {
    // No real helper at all -- a bound-but-unresponsive UDP socket, so the
    // QUIC handshake never completes (timeout/unreachable), never reaching
    // a definitive auth/cert-pin classification. Let `connect` run to
    // completion (rather than killing it early) so it actually reaches its
    // own error path and writes the outcome file -- this is exactly the
    // "always-connects" scenario (`isekai-ssh/tests/
    // wrapper_stale_trust_auto_recovery_e2e.rs`'s
    // `wrapper_silently_recovers_from_an_unreachable_cached_endpoint_and_reconnects`
    // drives the same thing end to end through the wrapper).
    let black_hole = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let black_hole_addr = black_hole.local_addr().unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let profiles_dir = home.join(".local").join("state").join("isekai").join("profiles");
    std::fs::create_dir_all(&profiles_dir).unwrap();
    let runtime_dir = tmp.path().join("runtime");
    std::fs::create_dir_all(&runtime_dir).unwrap();

    let fake_cert = "a".repeat(64);
    let fake_secret_b64 = base64::engine::general_purpose::STANDARD.encode([0x11u8; 32]);
    let key = isekai_trust::normalize_host_port("unreachable-host").unwrap();
    register_profile(&profiles_dir, &key, black_hole_addr, &fake_cert, &fake_secret_b64);
    let intent = write_intent(&runtime_dir, "unreachable-host", black_hole_addr, &fake_cert, &fake_secret_b64);

    let mut cmd = TokioCommand::new(isekai_pipe_bin_path());
    cmd.args(["connect", "--service", "ssh", "--stdio"])
        .env("HOME", &home)
        .env("ISEKAI_INTENT_ID", &intent.intent_id)
        .env("ISEKAI_PIPE_RUNTIME_DIR", &runtime_dir)
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped());

    // The QUIC dial to `black_hole_addr` has to actually time out
    // (`isekai-transport::system::CLIENT_MAX_IDLE_TIMEOUT`, 15s) before
    // `connect` gives up and writes the outcome file.
    let output = tokio::time::timeout(Duration::from_secs(25), cmd.output())
        .await
        .expect("connect should eventually give up on an unreachable target, not hang forever")
        .expect("failed to spawn isekai-pipe connect");

    assert!(output.stdout.is_empty());
    assert!(!output.status.success(), "connect must exit non-zero when the target is unreachable");

    let outcome_path = outcomes_dir(&runtime_dir).join(format!("{}.json", intent.intent_id));
    let bytes = std::fs::read(&outcome_path).unwrap_or_else(|e| {
        panic!(
            "expected a ConnectOutcome at {} -- every run_connect failure writes one now \
             (the \"always-connects\" principle, ISEKAI_PIPE_DESIGN.md §8 Epic N): {e}",
            outcome_path.display()
        )
    });
    let outcome: isekai_pipe_core::ConnectOutcome = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        outcome.class,
        ConnectOutcomeClass::Unreachable,
        "a plain unreachable target must be classified as Unreachable, never StaleTrust (that narrow \
         classification stays reserved for an explicit cert-pin-mismatch/Auth rejection)"
    );
    assert_eq!(outcome.intent_id, intent.intent_id);
    drop(black_hole);
}
