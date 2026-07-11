//! End-to-end test for the wrapper's connect-failure auto-recovery path
//! (`wrapper.rs::run_ssh_with_connect_failure_recovery`, `ISEKAI_PIPE_DESIGN.md`
//! §8 Epic N's "always-connects" principle): whatever state an
//! *already-trusted* destination's cached deployment is in, `isekai-ssh
//! <destination>` must self-heal (silently, no `[y/N]` prompt — this
//! profile was already trusted once) rather than requiring the user to
//! notice and run `isekai-ssh doctor --fix`/`init` manually. Two scenarios,
//! both driving the same recovery code path with a different
//! `ConnectOutcomeClass`:
//!
//! - `wrapper_silently_recovers_from_a_stale_trust_signal_and_reconnects`:
//!   the cached `session_secret` no longer matches the real, currently-running
//!   `isekai-pipe serve` (the exact shape a helper restart produces in
//!   practice — see `isekai-pipe/src/engine/mod.rs`'s "起動のたびにランダム
//!   生成する" comment) — `ConnectOutcomeClass::StaleTrust`.
//! - `wrapper_silently_recovers_from_an_unreachable_cached_endpoint_and_reconnects`:
//!   the cached endpoint simply has nothing listening any more (e.g. the
//!   previously-deployed `isekai-pipe serve` process was killed) — a plain
//!   QUIC-connect idle timeout, `ConnectOutcomeClass::Unreachable`.
//!
//! Combines two existing harness patterns from this crate's e2e tests: a
//! *real* `isekai-pipe serve` process standing in for "the already-deployed,
//! now-stale helper" (`isekai-pipe/tests/probe_e2e.rs`'s `spawn_helper`
//! shape, duplicated here per this crate's self-contained-test-file
//! convention), and the mock-`sshd` `FakeShellServer` harness for the
//! re-bootstrap deploy step (`wrapper_auto_bootstrap_e2e.rs`, also
//! duplicated).

use std::io::{BufRead, BufReader as StdBufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

use base64::Engine as _;
use isekai_protocol::handshake::{decode_handshake_json, HandshakeJson};
use isekai_trust::{HelperTrust, UpdatePolicy};
use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
use russh::{Channel as RusshChannel, ChannelId, CryptoVec};
use russh_keys::ssh_key::private::Ed25519Keypair;
use russh_keys::{PrivateKey, PublicKey};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener as TokioTcpListener;
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

/// Locates the sibling `isekai-pipe` binary by walking up from
/// `current_exe()`, building it if missing — duplicated from
/// `real_sshd_multihop_bootstrap_e2e.rs::sibling_bin_path` per this crate's
/// self-contained-test-file convention.
fn isekai_pipe_bin_path() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let is_release = path.file_name().map(|n| n == "release").unwrap_or(false);
    path.push("isekai-pipe");

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

async fn read_all<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).await?;
    Ok(buf)
}

// ---------------------------------------------------------------------
// Mock sshd for the re-bootstrap deploy step (duplicated from
// `wrapper_auto_bootstrap_e2e.rs`).
// ---------------------------------------------------------------------

#[derive(Clone)]
struct FakeShellServer {
    home: PathBuf,
    accepted_client_key: PublicKey,
    deploy_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl server::Server for FakeShellServer {
    type Handler = FakeShellHandler;
    fn new_client(&mut self, _: Option<SocketAddr>) -> FakeShellHandler {
        FakeShellHandler {
            home: self.home.clone(),
            accepted_client_key: self.accepted_client_key.clone(),
            stdin_senders: std::collections::HashMap::new(),
            deploy_count: self.deploy_count.clone(),
        }
    }
}

struct FakeShellHandler {
    home: PathBuf,
    accepted_client_key: PublicKey,
    stdin_senders: std::collections::HashMap<ChannelId, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    deploy_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
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
        self.deploy_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

async fn spawn_fake_ssh_server(
    home: PathBuf,
    accepted_client_key: PublicKey,
    deploy_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> SocketAddr {
    let keypair = Ed25519Keypair::from_seed(&[13u8; 32]);
    let host_key = PrivateKey::from(keypair);
    let config = std::sync::Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
    let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut sh = FakeShellServer { home, accepted_client_key, deploy_count };
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
        .expect("failed to run ssh-keygen");
    assert!(status.success(), "ssh-keygen exited non-zero");

    let pub_path = dir.join("client_id_ed25519.pub");
    let pub_text = std::fs::read_to_string(&pub_path).expect("failed to read generated .pub file");
    let public_key = PublicKey::from_openssh(pub_text.trim()).expect("failed to parse generated public key");
    (key_path, public_key)
}

fn real_ssh_path() -> PathBuf {
    let out = std::process::Command::new("sh").arg("-c").arg("command -v ssh").output().expect("failed to run `command -v ssh`");
    assert!(out.status.success(), "ssh(1) not found on PATH");
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim().to_string())
}

/// Same shape as `wrapper_auto_bootstrap_e2e.rs::shim_ssh_with_bootstrap_config`.
fn shim_ssh_with_bootstrap_config(tmp: &std::path::Path, alias: &str, mock_sshd_addr: SocketAddr, key_path: &std::path::Path) -> std::ffi::OsString {
    let config_path = tmp.join("ssh_config_bootstrap");
    let config = format!(
        "Host {alias}\n\
         \x20\x20\x20\x20HostName 127.0.0.1\n\
         \x20\x20\x20\x20Port {port}\n\
         \x20\x20\x20\x20User tester\n\
         \x20\x20\x20\x20IdentityFile {key}\n\
         \x20\x20\x20\x20IdentitiesOnly yes\n\
         \x20\x20\x20\x20StrictHostKeyChecking no\n\
         \x20\x20\x20\x20UserKnownHostsFile /dev/null\n\
         \n\
         Host 127.0.0.1\n\
         \x20\x20\x20\x20Port {port}\n\
         \x20\x20\x20\x20User tester\n\
         \x20\x20\x20\x20IdentityFile {key}\n\
         \x20\x20\x20\x20IdentitiesOnly yes\n\
         \x20\x20\x20\x20StrictHostKeyChecking no\n\
         \x20\x20\x20\x20UserKnownHostsFile /dev/null\n",
        port = mock_sshd_addr.port(),
        key = key_path.display(),
    );
    std::fs::write(&config_path, config).unwrap();

    let bin_dir = tmp.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let shim_path = bin_dir.join("ssh");
    let shim = format!("#!/bin/sh\nexec {real_ssh} -F {config} \"$@\"\n", real_ssh = real_ssh_path().display(), config = config_path.display());
    std::fs::write(&shim_path, shim).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut paths = vec![bin_dir];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).unwrap()
}

fn profiles_dir_under(home: &std::path::Path) -> PathBuf {
    home.join(".local").join("state").join("isekai").join("profiles")
}

fn valid_bootstrap_report_json(refreshed_session_secret_b64: &str) -> String {
    format!(
        r#"{{"v":2,"session_id":"00000000000000000000000000000000","bootstrap_attempt_id":"11111111111111111111111111111111","handshake":{{"v":1,"session_secret":"{refreshed_session_secret_b64}","protocol":{{"name":"isekai-pipe","alpn":"isekai-pipe/1"}},"peer":{{"server_identity":{{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}}}},"candidates":[{{"kind":"direct-by-bootstrap-host","port":45231,"source":"bootstrap-ssh"}}]}}}}"#
    )
}

// ---------------------------------------------------------------------
// Real `isekai-pipe serve` standing in for "the already-deployed, now
// stale" helper (duplicated from `isekai-pipe/tests/probe_e2e.rs::spawn_helper`).
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
    let mut reader = StdBufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read handshake line from isekai-pipe serve stdout");
    let handshake = decode_handshake_json(line.trim().as_bytes()).expect("failed to parse/validate handshake JSON");

    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut r = StdBufReader::new(stderr);
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

fn register_stale_profile(profiles_dir: &std::path::Path, key: &str, helper_addr: SocketAddr, real_cert_sha256_hex: &str, wrong_session_secret_b64: &str) {
    let trust = HelperTrust {
        identity_pubkey: real_cert_sha256_hex.to_string(),
        trusted_helper_sha256: "a".repeat(64),
        trusted_helper_version: "test".to_string(),
        update_policy: UpdatePolicy::ExactDigestOnly,
        release_channel: None,
        last_via: None,
        trusted_at: "2026-07-10T00:00:00Z".to_string(),
        last_seen_at: "2026-07-10T00:00:00Z".to_string(),
        cached_relay_addr: helper_addr.to_string(),
        cached_cert_sha256: real_cert_sha256_hex.to_string(),
        cached_session_secret: wrong_session_secret_b64.to_string(),
        cached_stun_observed_addr: None,
    };
    let profile = isekai_pipe_core::PersistentProfile::migrate_legacy_helper_trust(key, &trust);
    isekai_pipe_core::write_persistent_profile(profiles_dir, &profile).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn wrapper_silently_recovers_from_a_stale_trust_signal_and_reconnects() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();
    let deploy_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mock_sshd_addr = spawn_fake_ssh_server(remote_home.clone(), client_pubkey, deploy_count.clone()).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let path_env = shim_ssh_with_bootstrap_config(tmp.path(), "stale-trust-host", mock_sshd_addr, &key_path);

    // A real, currently-running isekai-pipe serve -- the "already deployed"
    // helper whose cached trust material has gone stale.
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
    let wrong_secret_b64 = base64::engine::general_purpose::STANDARD.encode([0xEEu8; 32]);

    let key = isekai_trust::normalize_host_port("stale-trust-host").unwrap();
    register_stale_profile(&profiles_dir_under(&home), &key, helper_addr, &real_cert, &wrong_secret_b64);

    // Stand-in helper script for the re-bootstrap deploy over the mock sshd
    // -- doesn't need to be a real, reachable server: the wrapper's *second*
    // `ssh` attempt (against this canned report's address) is expected to
    // fail on its own too, exactly like the plain auto-bootstrap tests. This
    // test only cares that (a) the deploy happened exactly once, silently,
    // and (b) the profile's session_secret actually changed.
    let refreshed_secret_b64 = base64::engine::general_purpose::STANDARD.encode([0xAAu8; 32]);
    let report = valid_bootstrap_report_json(&refreshed_secret_b64);
    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(&helper_script_path, format!("#!/bin/sh\necho '{report}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper_script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .arg("--isekai-helper-binary")
        .arg(&helper_script_path)
        .arg("stale-trust-host")
        .env("HOME", &home)
        .env("PATH", &path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped()) // deliberately never written to -- Silent mode must not read it
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");
    drop(child.stdin.take());

    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut stderr_log = String::new();
    let mut saw_stale_notice = false;
    let mut saw_second_registration = false;
    let mut registered_count = 0;
    for _ in 0..400 {
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_secs(20), stderr.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                eprint!("[isekai-ssh stderr] {line}");
                stderr_log.push_str(&line);
                if line.contains("looks stale") {
                    saw_stale_notice = true;
                }
                if line.contains("Registered") {
                    registered_count += 1;
                    if registered_count >= 1 && saw_stale_notice {
                        saw_second_registration = true;
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    assert!(saw_stale_notice, "expected wrapper stderr to report a detected stale-trust signal:\n{stderr_log}");
    assert!(saw_second_registration, "expected the re-bootstrap to complete and register a refreshed profile:\n{stderr_log}");
    assert!(!stderr_log.contains("[y/N]"), "the automatic re-bootstrap must never show the TOFU prompt:\n{stderr_log}");
    // `OpenSshBackend::install_and_start` performs exactly two `ssh(1)`
    // invocations per deploy (upload_binary + launch_and_capture_handshake,
    // `isekai-bootstrap/src/openssh.rs`'s module docs) — two `exec_request`
    // calls here means the re-bootstrap happened exactly once, not that it
    // was retried an extra time.
    assert_eq!(
        deploy_count.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "expected exactly one re-bootstrap deploy (2 ssh exec calls: upload + launch)"
    );

    let refreshed = isekai_pipe_core::load_persistent_profile(&profiles_dir_under(&home), &key).unwrap().expect("profile should still exist after refresh");
    let legacy_relay = refreshed.legacy_relay_transport.as_ref().expect("expected a cached relay transport");
    assert_ne!(
        legacy_relay.session_secret_b64, wrong_secret_b64,
        "the cached session_secret must have been replaced by the re-bootstrap"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wrapper_does_not_auto_recover_when_no_bootstrap_is_set() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();
    let deploy_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mock_sshd_addr = spawn_fake_ssh_server(remote_home.clone(), client_pubkey, deploy_count.clone()).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let path_env = shim_ssh_with_bootstrap_config(tmp.path(), "stale-no-recover-host", mock_sshd_addr, &key_path);

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
    let wrong_secret_b64 = base64::engine::general_purpose::STANDARD.encode([0xEEu8; 32]);

    let key = isekai_trust::normalize_host_port("stale-no-recover-host").unwrap();
    register_stale_profile(&profiles_dir_under(&home), &key, helper_addr, &real_cert, &wrong_secret_b64);

    let output = tokio::time::timeout(
        Duration::from_secs(20),
        TokioCommand::new(isekai_ssh_bin_path())
            .arg("--isekai-no-bootstrap")
            .arg("stale-no-recover-host")
            .env("HOME", &home)
            .env("PATH", &path_env)
            .env_remove("RUST_LOG")
            .stdin(StdStdio::null())
            .stdout(StdStdio::piped())
            .stderr(StdStdio::piped())
            .output(),
    )
    .await
    .expect("isekai-ssh should fail closed quickly, not hang")
    .expect("failed to spawn isekai-ssh");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("auto-bootstrap is disabled"),
        "expected the wrapper to report that auto-recovery is disabled, got:\n{stderr}"
    );
    assert_eq!(deploy_count.load(std::sync::atomic::Ordering::SeqCst), 0, "--isekai-no-bootstrap must prevent any re-deploy attempt");

    let unchanged = isekai_pipe_core::load_persistent_profile(&profiles_dir_under(&home), &key).unwrap().expect("profile should still exist, unmodified");
    let legacy_relay = unchanged.legacy_relay_transport.as_ref().unwrap();
    assert_eq!(legacy_relay.session_secret_b64, wrong_secret_b64, "the stale profile must be left untouched");
}

/// The "always-connects" principle's other half: the cached endpoint isn't
/// rejecting us (that's the stale-trust scenario above) — it simply has
/// nothing listening any more (the exact shape "the previously-deployed
/// `isekai-pipe serve` process was killed" produces), so the first `ssh`
/// attempt fails with a plain QUIC-connect idle timeout
/// (`ConnectOutcomeClass::Unreachable`, not `StaleTrust`). The wrapper must
/// still silently re-bootstrap and retry — this is the regression test for
/// that generalization (previously, only `StaleTrust` triggered recovery,
/// so this exact scenario required the user to manually run `isekai-ssh
/// doctor --fix`/`init`).
#[tokio::test(flavor = "multi_thread")]
async fn wrapper_silently_recovers_from_an_unreachable_cached_endpoint_and_reconnects() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();
    let deploy_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mock_sshd_addr = spawn_fake_ssh_server(remote_home.clone(), client_pubkey, deploy_count.clone()).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let path_env = shim_ssh_with_bootstrap_config(tmp.path(), "unreachable-host", mock_sshd_addr, &key_path);

    // A `helper_addr` with nothing listening: bind a UDP socket, note its
    // port, then drop it immediately -- any QUIC dial to it gets no
    // response at all (a plain idle timeout), unlike the stale-trust
    // scenario's real-but-wrong-secret helper.
    let dead_addr = {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        drop(socket);
        addr
    };
    let bogus_cert_sha256 = "a".repeat(64);
    let bogus_secret_b64 = base64::engine::general_purpose::STANDARD.encode([0x11u8; 32]);

    let key = isekai_trust::normalize_host_port("unreachable-host").unwrap();
    register_stale_profile(&profiles_dir_under(&home), &key, dead_addr, &bogus_cert_sha256, &bogus_secret_b64);

    let refreshed_secret_b64 = base64::engine::general_purpose::STANDARD.encode([0x22u8; 32]);
    let report = valid_bootstrap_report_json(&refreshed_secret_b64);
    let helper_script_path = tmp.path().join("fake-isekai-helper-unreachable.sh");
    std::fs::write(&helper_script_path, format!("#!/bin/sh\necho '{report}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper_script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .arg("--isekai-helper-binary")
        .arg(&helper_script_path)
        .arg("unreachable-host")
        .env("HOME", &home)
        .env("PATH", &path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped()) // deliberately never written to -- Silent mode must not read it
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");
    drop(child.stdin.take());

    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut stderr_log = String::new();
    let mut saw_unreachable_notice = false;
    let mut saw_second_registration = false;
    let mut registered_count = 0;
    // The dead endpoint's QUIC dial has to actually time out
    // (`isekai-transport::system::CLIENT_MAX_IDLE_TIMEOUT`, 15s) before the
    // first `ssh` attempt fails at all, so this loop's per-line timeout is
    // generous.
    for _ in 0..400 {
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_secs(25), stderr.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                eprint!("[isekai-ssh stderr] {line}");
                stderr_log.push_str(&line);
                if line.contains("could not be reached") {
                    saw_unreachable_notice = true;
                }
                if line.contains("Registered") {
                    registered_count += 1;
                    if registered_count >= 1 && saw_unreachable_notice {
                        saw_second_registration = true;
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    assert!(
        saw_unreachable_notice,
        "expected wrapper stderr to report a detected connect-failure (unreachable) signal:\n{stderr_log}"
    );
    assert!(saw_second_registration, "expected the re-bootstrap to complete and register a refreshed profile:\n{stderr_log}");
    assert!(!stderr_log.contains("[y/N]"), "the automatic re-bootstrap must never show the TOFU prompt:\n{stderr_log}");
    assert!(
        !stderr_log.contains("looks stale"),
        "an Unreachable (not StaleTrust) signal must use the unreachable-specific message, not the stale-trust one:\n{stderr_log}"
    );

    let refreshed = isekai_pipe_core::load_persistent_profile(&profiles_dir_under(&home), &key).unwrap().expect("profile should still exist after refresh");
    let legacy_relay = refreshed.legacy_relay_transport.as_ref().expect("expected a cached relay transport");
    assert_ne!(
        legacy_relay.session_secret_b64, bogus_secret_b64,
        "the cached session_secret must have been replaced by the re-bootstrap"
    );
}
