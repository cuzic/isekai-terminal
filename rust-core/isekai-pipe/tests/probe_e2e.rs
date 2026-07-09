//! `isekai-pipe probe` E2E (`ISEKAI_PIPE_DESIGN.md` §8, the paragraph right
//! before Epic K). Spawns a real `isekai-pipe serve` subprocess against a
//! real local TCP listener, writes a `PersistentProfile` pointing at it
//! directly (bypassing SSH bootstrap entirely — the trust material is
//! constructed straight from the real handshake, matching
//! `isekai-ssh/tests/init_e2e.rs`'s `spawn_helper`/`stand_in_helper_script`
//! pattern for "an already-deployed real isekai-helper instance"), then runs
//! the real `isekai-pipe probe` binary against it and checks both the happy
//! path (reachable target) and a failure path (the cached target port
//! closed after `serve` exits) report the right stages.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

use isekai_protocol::handshake::{decode_handshake_json, HandshakeJson};
use isekai_pipe_core::{write_persistent_profile, PersistentProfile};
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

/// Spawns a real `isekai-pipe serve --target <target_addr>` subprocess and
/// reads its one-line handshake JSON off stdout — the same "a real,
/// already-deployed helper" shape `isekai-ssh/tests/init_e2e.rs::spawn_helper`
/// uses, duplicated here per this crate's self-contained-test-file
/// convention (`isekai-ssh-e2e-test-self-containment-convention`).
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

fn write_probe_profile(profiles_dir: &std::path::Path, key: &str, helper_addr: SocketAddr, handshake: &HandshakeJson) {
    let trust = HelperTrust {
        identity_pubkey: handshake.cert_sha256().to_string(),
        trusted_helper_sha256: "a".repeat(64),
        trusted_helper_version: "test".to_string(),
        update_policy: UpdatePolicy::ExactDigestOnly,
        release_channel: None,
        last_via: None,
        trusted_at: "2026-07-09T00:00:00Z".to_string(),
        last_seen_at: "2026-07-09T00:00:00Z".to_string(),
        cached_relay_addr: helper_addr.to_string(),
        cached_cert_sha256: handshake.cert_sha256().to_string(),
        cached_session_secret: handshake.session_secret.clone(),
        cached_stun_observed_addr: None,
    };
    let profile = PersistentProfile::migrate_legacy_helper_trust(key, &trust);
    write_persistent_profile(profiles_dir, &profile).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn probe_reports_every_stage_ok_for_a_reachable_target() {
    let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target_listener.local_addr().unwrap();
    // Real isekai-pipe serve needs something listening at --target that will
    // actually accept a TCP connection when it dials it post-attach; this
    // task just accepts and idles, matching `serve_e2e.rs`'s minimal echo
    // targets for tests that don't care about the byte stream itself.
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = target_listener.accept().await else { break };
            std::mem::forget(stream);
        }
    });

    let helper = spawn_helper(target_addr);
    let helper_addr: SocketAddr =
        format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap()).parse().unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let profiles_dir = tmp.path().join("profiles");
    std::fs::create_dir_all(&profiles_dir).unwrap();
    write_probe_profile(&profiles_dir, "probe-target:22", helper_addr, &helper.handshake);

    let output = tokio::time::timeout(
        Duration::from_secs(20),
        TokioCommand::new(isekai_pipe_bin_path())
            .arg("probe")
            .arg("--profile")
            .arg("probe-target:22")
            .arg("--json")
            .env("ISEKAI_PIPE_PROFILES_DIR", &profiles_dir)
            .env_remove("RUST_LOG")
            .output(),
    )
    .await
    .expect("isekai-pipe probe should not hang")
    .expect("failed to spawn isekai-pipe probe");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("probe stdout:\n{stdout}\nprobe stderr:\n{stderr}");
    assert!(output.status.success(), "expected probe to exit 0 for a reachable target: {output:?}");

    let report: serde_json::Value = serde_json::from_str(&stdout).expect("probe --json should print valid JSON");
    assert_eq!(report["transport"], "relay");
    assert_eq!(report["dns_resolution"]["status"], "skipped");
    assert_eq!(report["stun_discovery"]["status"], "skipped");
    assert_eq!(report["handshake"]["status"], "ok", "{report}");
    assert_eq!(report["target_reachability"]["status"], "ok", "{report}");
}

#[tokio::test(flavor = "multi_thread")]
async fn probe_reports_a_failed_handshake_when_the_helper_is_gone() {
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

    let tmp = tempfile::tempdir().unwrap();
    let profiles_dir = tmp.path().join("profiles");
    std::fs::create_dir_all(&profiles_dir).unwrap();
    write_probe_profile(&profiles_dir, "probe-target:22", helper_addr, &helper.handshake);

    // Kill the real helper before probing — the cached `helper_addr` now has
    // nothing listening on it at all, so the QUIC connect itself should fail
    // (pre-attach, nothing ever reaches ATTACH_HELLO).
    drop(helper);

    let output = tokio::time::timeout(
        Duration::from_secs(20),
        TokioCommand::new(isekai_pipe_bin_path())
            .arg("probe")
            .arg("--profile")
            .arg("probe-target:22")
            .arg("--json")
            .env("ISEKAI_PIPE_PROFILES_DIR", &profiles_dir)
            .env_remove("RUST_LOG")
            .output(),
    )
    .await
    .expect("isekai-pipe probe should not hang")
    .expect("failed to spawn isekai-pipe probe");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("probe stdout:\n{stdout}\nprobe stderr:\n{stderr}");
    assert!(!output.status.success(), "expected probe to exit non-zero once the helper is gone: {output:?}");

    let report: serde_json::Value = serde_json::from_str(&stdout).expect("probe --json should print valid JSON even on failure");
    assert_eq!(report["handshake"]["status"], "failed", "{report}");
    assert_eq!(report["target_reachability"]["status"], "not_attempted", "{report}");
}
