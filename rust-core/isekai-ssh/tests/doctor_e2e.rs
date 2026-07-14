//! `isekai-ssh doctor <host> [--fix]` E2E (`doctor.rs`, `ISEKAI_PIPE_DESIGN.md`
//! §8 Epic N). Three scenarios: never-bootstrapped, stale-without-`--fix`,
//! and stale-with-`--fix`. Reuses the same real-`isekai-pipe-serve` +
//! mock-`sshd` harness shape as `wrapper_stale_trust_auto_recovery_e2e.rs`,
//! duplicated per this crate's self-contained-test-file convention.

use std::io::{BufRead, BufReader as StdBufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;

use base64::Engine as _;
use isekai_protocol::handshake::{decode_handshake_json, HandshakeJson};
use isekai_trust::{HelperTrust, UpdatePolicy};
use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
use russh::{Channel as RusshChannel, ChannelId, CryptoVec};
use russh_keys::ssh_key::private::Ed25519Keypair;
use russh_keys::{PrivateKey, PublicKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    // check afterward (confirmed via a real `test-windows` CI failure).
    path.push(if cfg!(windows) { "isekai-pipe.exe" } else { "isekai-pipe" });

    if !path.exists() {
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

#[derive(Clone)]
struct FakeShellServer {
    home: PathBuf,
    accepted_client_key: PublicKey,
}

impl server::Server for FakeShellServer {
    type Handler = FakeShellHandler;
    fn new_client(&mut self, _: Option<SocketAddr>) -> FakeShellHandler {
        FakeShellHandler { home: self.home.clone(), accepted_client_key: self.accepted_client_key.clone(), stdin_senders: std::collections::HashMap::new() }
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
            let (stdout_res, stderr_res, wait_res) = tokio::join!(read_all(&mut child_stdout), read_all(&mut child_stderr), child.wait());
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
    let keypair = Ed25519Keypair::from_seed(&[17u8; 32]);
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
    let status = std::process::Command::new("ssh-keygen").args(["-t", "ed25519", "-N", "", "-C", "", "-q", "-f"]).arg(&key_path).status().expect("failed to run ssh-keygen");
    assert!(status.success(), "ssh-keygen exited non-zero");
    let pub_text = std::fs::read_to_string(dir.join("client_id_ed25519.pub")).expect("failed to read generated .pub file");
    let public_key = PublicKey::from_openssh(pub_text.trim()).expect("failed to parse generated public key");
    (key_path, public_key)
}

#[cfg(unix)]
fn real_ssh_path() -> PathBuf {
    let out = std::process::Command::new("sh").arg("-c").arg("command -v ssh").output().expect("failed to run `command -v ssh`");
    assert!(out.status.success(), "ssh(1) not found on PATH");
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim().to_string())
}

/// See `wrapper_auto_bootstrap_e2e.rs::real_ssh_path`'s Windows variant for
/// why this needs a different implementation from the Unix one above.
#[cfg(windows)]
fn real_ssh_path() -> PathBuf {
    let out = std::process::Command::new("where").arg("ssh.exe").output().expect("failed to run `where ssh.exe`");
    assert!(out.status.success(), "ssh.exe not found on PATH");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let first = stdout.lines().next().expect("`where ssh.exe` produced no output");
    PathBuf::from(first.trim())
}

/// Everything needed to point `isekai-ssh` (and `isekai-bootstrap::OpenSshBackend`'s
/// own deploy dial, via `--isekai-ssh-path`) at a stand-in `ssh(1)` that
/// injects `-F <config_path>`. See `wrapper_auto_bootstrap_e2e.rs::SshShim`
/// and `ssh_test_shim`'s module docs for why Windows needs a compiled `.exe`
/// shim (not a `.cmd` batch file) and Unix a `#!/bin/sh` script.
struct SshShim {
    isekai_ssh_path_arg: PathBuf,
    extra_env: Vec<(&'static str, PathBuf)>,
    path_env: std::ffi::OsString,
}

/// See `wrapper_stale_trust_auto_recovery_e2e.rs::expose_msys_dll_next_to`'s
/// docs for why this is needed.
#[cfg(windows)]
fn expose_msys_dll_next_to(shim_path: &std::path::Path, real_ssh: &std::path::Path) {
    let Some(real_ssh_dir) = real_ssh.parent() else { return };
    let Some(shim_dir) = shim_path.parent() else { return };
    for dll in ["msys-2.0.dll", "cygwin1.dll"] {
        let src = real_ssh_dir.join(dll);
        if src.is_file() {
            let _ = std::fs::copy(&src, shim_dir.join(dll));
        }
    }
}

fn shim_ssh_with_bootstrap_config(tmp: &std::path::Path, alias: &str, mock_sshd_addr: SocketAddr, key_path: &std::path::Path) -> SshShim {
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
    let real_ssh = real_ssh_path();

    #[cfg(unix)]
    let (isekai_ssh_path_arg, extra_env) = {
        let shim_path = bin_dir.join("ssh");
        let shim = format!("#!/bin/sh\nexec {real_ssh} -F {config} \"$@\"\n", real_ssh = real_ssh.display(), config = config_path.display());
        std::fs::write(&shim_path, shim).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        (shim_path, Vec::new())
    };
    // Windows: `ssh_test_shim` (a real compiled `.exe`, `src/bin/ssh_test_shim.rs`)
    // instead of a `.cmd` batch file — see that file's module docs (a batch
    // shim can't carry the real deploy step's multi-line remote command,
    // confirmed via a real `test-windows` CI failure on
    // `wrapper_auto_bootstrap_e2e.rs`, which this mirrors).
    #[cfg(windows)]
    let (isekai_ssh_path_arg, extra_env) = {
        let shim_path = PathBuf::from(env!("CARGO_BIN_EXE_ssh_test_shim"));
        expose_msys_dll_next_to(&shim_path, &real_ssh);
        (shim_path, vec![("ISEKAI_SSH_TEST_SHIM_REAL_SSH", real_ssh), ("ISEKAI_SSH_TEST_SHIM_CONFIG", config_path)])
    };

    let mut paths = vec![bin_dir];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let path_env = std::env::join_paths(paths).unwrap();
    SshShim { isekai_ssh_path_arg, extra_env, path_env }
}

fn profiles_dir_under(home: &std::path::Path) -> PathBuf {
    home.join(".local").join("state").join("isekai").join("profiles")
}

fn valid_bootstrap_report_json(refreshed_session_secret_b64: &str) -> String {
    format!(
        r#"{{"v":2,"session_id":"00000000000000000000000000000000","bootstrap_attempt_id":"11111111111111111111111111111111","handshake":{{"v":1,"session_secret":"{refreshed_session_secret_b64}","protocol":{{"name":"isekai-pipe","alpn":"isekai-pipe/1"}},"peer":{{"server_identity":{{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}}}},"candidates":[{{"kind":"direct-by-bootstrap-host","port":45231,"source":"bootstrap-ssh"}}]}}}}"#
    )
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
async fn doctor_reports_never_bootstrapped_for_an_unknown_host() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();

    let output = TokioCommand::new(isekai_ssh_bin_path())
        .arg("doctor")
        .arg("never-bootstrapped-host")
        .env("HOME", &home)
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        .env_remove("RUST_LOG")
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .output()
        .await
        .expect("failed to spawn isekai-ssh doctor");

    assert!(!output.status.success(), "doctor must exit non-zero for a never-bootstrapped host");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("never been bootstrapped"), "expected a 'never been bootstrapped' message, got: {stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_reports_stale_trust_without_fixing_it() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("client-home");
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
    let wrong_secret_b64 = base64::engine::general_purpose::STANDARD.encode([0xEEu8; 32]);

    let key = isekai_trust::normalize_host_port("doctor-stale-host").unwrap();
    register_stale_profile(&profiles_dir_under(&home), &key, helper_addr, &real_cert, &wrong_secret_b64);

    let output = TokioCommand::new(isekai_ssh_bin_path())
        .arg("doctor")
        .arg("doctor-stale-host")
        .env("HOME", &home)
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        .env_remove("RUST_LOG")
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .output()
        .await
        .expect("failed to spawn isekai-ssh doctor");

    assert!(!output.status.success(), "doctor must exit non-zero when stale trust is found and --fix wasn't given");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("looks like the cached trust"), "expected doctor to report the stale-trust diagnosis, got stdout: {stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--fix"), "expected doctor to suggest --fix, got stderr: {stderr}");

    let unchanged = isekai_pipe_core::load_persistent_profile(&profiles_dir_under(&home), &key).unwrap().expect("profile should still exist, unmodified");
    let legacy_relay = unchanged.legacy_relay_transport.as_ref().unwrap();
    assert_eq!(legacy_relay.session_secret_b64, wrong_secret_b64, "doctor without --fix must never modify the profile");
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_fixes_stale_trust_when_given_fix_flag() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();
    let mock_sshd_addr = spawn_fake_ssh_server(remote_home.clone(), client_pubkey).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let shim = shim_ssh_with_bootstrap_config(tmp.path(), "doctor-fix-host", mock_sshd_addr, &key_path);

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

    let key = isekai_trust::normalize_host_port("doctor-fix-host").unwrap();
    register_stale_profile(&profiles_dir_under(&home), &key, helper_addr, &real_cert, &wrong_secret_b64);

    let refreshed_secret_b64 = base64::engine::general_purpose::STANDARD.encode([0xBBu8; 32]);
    let report = valid_bootstrap_report_json(&refreshed_secret_b64);
    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(&helper_script_path, format!("#!/bin/sh\necho '{report}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper_script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let output = TokioCommand::new(isekai_ssh_bin_path())
        .arg("doctor")
        .arg("doctor-fix-host")
        .arg("--fix")
        .arg("--ssh-path")
        .arg(&shim.isekai_ssh_path_arg)
        .arg("--helper-binary")
        .arg(&helper_script_path)
        .env("HOME", &home)
        .env("PATH", &shim.path_env)
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        .envs(shim.extra_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .output()
        .await
        .expect("failed to spawn isekai-ssh doctor");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "doctor --fix should exit 0 after a successful refresh; stdout={stdout}\nstderr={stderr}");
    assert!(stdout.contains("Refreshed"), "expected doctor to report the refresh, got stdout: {stdout}");
    assert!(!stdout.contains("[y/N]"), "doctor --fix must never show the TOFU prompt, got stdout: {stdout}");

    let refreshed = isekai_pipe_core::load_persistent_profile(&profiles_dir_under(&home), &key).unwrap().expect("profile should still exist after refresh");
    let legacy_relay = refreshed.legacy_relay_transport.as_ref().unwrap();
    assert_ne!(legacy_relay.session_secret_b64, wrong_secret_b64, "the cached session_secret must have been replaced");
}
