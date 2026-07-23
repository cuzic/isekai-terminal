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

/// Returns the mock sshd's address *and* its host key's SHA256 fingerprint
/// (`russh_keys::PublicKey::fingerprint`, the same format
/// `isekai_trust::SshHostKeyTrust::fingerprint` stores) — callers that
/// exercise a `TofuConfirmation::Silent` flow need the fingerprint to
/// pre-seed `known_ssh_hosts.toml` via [`seed_ssh_host_key_trust`], since
/// `RusshBackend`'s own host-key TOFU prompt (distinct from the app-level
/// `[y/N]` trust-registration prompt `TofuConfirmation` controls) has no
/// silent/non-interactive mode and would otherwise block forever on the
/// `Stdio::null()` stdin these tests use.
async fn spawn_fake_ssh_server(
    home: PathBuf,
    accepted_client_key: PublicKey,
    deploy_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> (SocketAddr, String) {
    let keypair = Ed25519Keypair::from_seed(&[13u8; 32]);
    let host_key = PrivateKey::from(keypair);
    let fingerprint = host_key.public_key().fingerprint(russh_keys::HashAlg::Sha256).to_string();
    let config = std::sync::Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
    let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut sh = FakeShellServer { home, accepted_client_key, deploy_count };
    tokio::spawn(async move {
        use server::Server as _;
        let _ = sh.run_on_socket(config, &listener).await;
    });
    (addr, fingerprint)
}

/// Seeds `known_ssh_hosts.toml` under `home/.config/isekai-ssh/` with a
/// pre-trusted entry for `host_port`, mirroring the on-disk state a *real*
/// prior interactive bootstrap (`isekai-ssh init`) would have left behind —
/// see [`spawn_fake_ssh_server`]'s docs for why a `TofuConfirmation::Silent`
/// e2e test needs this instead of feeding a stdin answer.
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

/// Everything needed to point `isekai-ssh` at a stand-in `ssh(1)` — see
/// `wrapper_auto_bootstrap_e2e.rs::SshShim` and `ssh_test_shim`'s module
/// docs for why Windows needs a compiled `.exe` shim (not a `.cmd` batch
/// file) and Unix a `#!/bin/sh` script.
struct SshShim {
    isekai_ssh_path_arg: PathBuf,
    extra_env: Vec<(&'static str, PathBuf)>,
    path_env: std::ffi::OsString,
}

/// `wrapper.rs::proxy_command` decides whether the *real* connect step's
/// `ProxyCommand` needs POSIX single-quoting (`wrapper.rs::is_posix_shell_ssh`)
/// by checking for `msys-2.0.dll`/`cygwin1.dll` next to the *resolved*
/// `--isekai-ssh-path` binary. That binary is `ssh_test_shim.exe` here, not
/// the real MSYS2-hosted `ssh.exe` it execs internally — so without this,
/// the check incorrectly concludes "not POSIX-shell", skips the quoting
/// that assumption requires, and the connect step's embedded Windows path
/// gets its backslashes silently eaten when the real (POSIX-shell) ssh
/// actually execs the `ProxyCommand` via its own `sh -c` (confirmed via a
/// real `test-windows` CI failure: `sh -c` reported `exec: <path with
/// every `\` stripped>: not found`, exactly `wrapper_auto_bootstrap_e2e.rs::posix_safe_path`'s
/// docs describe for a different embedding site). Copying the same
/// companion DLL next to the shim makes that detection see the same thing
/// it would for the real `ssh.exe`. A no-op (and harmless to call
/// repeatedly/concurrently across tests sharing this crate's `target/`) if
/// neither DLL exists next to `real_ssh` at all.
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

/// Same shape as `wrapper_auto_bootstrap_e2e.rs::shim_ssh_with_bootstrap_config`,
/// including also writing the identical `Host` blocks to `home/.ssh/config`
/// for the Windows-native path's own `openssh_config` resolution (see that
/// function's doc comment; confirmed via a real `test-windows` CI failure).
fn shim_ssh_with_bootstrap_config(tmp: &std::path::Path, home: &std::path::Path, alias: &str, mock_sshd_addr: SocketAddr, key_path: &std::path::Path) -> SshShim {
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
    let config_path = tmp.join("ssh_config_bootstrap");
    std::fs::write(&config_path, &config).unwrap();

    let home_ssh_dir = home.join(".ssh");
    std::fs::create_dir_all(&home_ssh_dir).unwrap();
    std::fs::write(home_ssh_dir.join("config"), &config).unwrap();

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

/// Verbose bootstrap-progress messages (including "Registered ... in ...")
/// now default to `isekai-ssh`'s own log file (`log_file.rs::log_line_verbose!`)
/// rather than stderr — these tests point that log file at a known path
/// under the test's own `home` and poll it instead of scanning stderr.
fn verbose_log_path_under(home: &std::path::Path) -> PathBuf {
    home.join("isekai-ssh-verbose-test.log")
}

fn verbose_log_registered_count(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path).map(|s| s.matches("Registered").count()).unwrap_or(0)
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
    let (mock_sshd_addr, mock_sshd_fingerprint) = spawn_fake_ssh_server(remote_home.clone(), client_pubkey, deploy_count.clone()).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    // The silent re-deploy uses `TofuConfirmation::Silent` for the app-level
    // trust-registration prompt, but `RusshBackend`'s underlying SSH
    // host-key TOFU (a separate, non-silenceable prompt) still needs this
    // host key pre-trusted — see `seed_ssh_host_key_trust`'s docs.
    seed_ssh_host_key_trust(&home, &format!("127.0.0.1:{}", mock_sshd_addr.port()), &mock_sshd_fingerprint);
    let shim = shim_ssh_with_bootstrap_config(tmp.path(), &home, "stale-trust-host", mock_sshd_addr, &key_path);

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
        .arg("--isekai-ssh-path")
        .arg(&shim.isekai_ssh_path_arg)
        .arg("--isekai-helper-binary")
        .arg(&helper_script_path)
        .arg("stale-trust-host")
        .env("HOME", &home)
        .env("PATH", &shim.path_env)
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        // Without this, the native connect path's `ConnectOutcome` side
        // channel (`always-connects.md`) falls back to a shared
        // `%TEMP%\isekai-<uid>` runtime dir — a real `test-windows` CI
        // failure (2026-07-23) showed the *sibling* unreachable-endpoint
        // test in this same file misclassify as `StaleTrust` instead of
        // `Unreachable`, consistent with cross-test interference over that
        // shared directory when multiple tests in this file run
        // concurrently (Rust's default test harness). Isolating each test's
        // own runtime dir under its own `tmp`, same as this crate's
        // `mux_holder_windows_e2e.rs`, removes the shared state entirely.
        .env("ISEKAI_PIPE_RUNTIME_DIR", tmp.path().join("runtime"))
        // Verbose bootstrap-progress messages (including "Registered ...
        // in ...", which this test counts below) now default to
        // `isekai-ssh`'s own log file (`log_file.rs::log_line_verbose!`)
        // rather than stderr — "cached trust looks stale" (checked via
        // `saw_stale_notice` below) stays on stderr unchanged, since it's
        // one of the curated connect-failure/recovery summary lines.
        .env("ISEKAI_PIPE_LOG_FILE", verbose_log_path_under(&home))
        .envs(shim.extra_env)
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
                let registered_count = verbose_log_registered_count(&verbose_log_path_under(&home));
                if registered_count >= 1 && saw_stale_notice {
                    saw_second_registration = true;
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    // [diag] temporary: dump the verbose log file too (a lot of the
    // connect/recovery progress lines default there, not stderr) to
    // diagnose a real-CI-only Windows failure whose stderr alone showed
    // nothing past `resolved host_config`.
    let verbose_log_contents = std::fs::read_to_string(verbose_log_path_under(&home)).unwrap_or_else(|e| format!("<failed to read verbose log: {e}>"));

    assert!(saw_stale_notice, "expected wrapper stderr to report a detected stale-trust signal:\n{stderr_log}\n[diag] verbose log:\n{verbose_log_contents}");
    assert!(saw_second_registration, "expected the re-bootstrap to complete and register a refreshed profile:\n{stderr_log}\n[diag] verbose log:\n{verbose_log_contents}");
    assert!(!stderr_log.contains("[y/N]"), "the automatic re-bootstrap must never show the TOFU prompt:\n{stderr_log}");
    // `OpenSshBackend::install_and_launch` performs exactly one combined
    // `ssh(1)` invocation per deploy (check + conditional-upload +
    // conditional-launch under a single held `flock`, commit 3921a43 —
    // this comment/assertion previously said "two ssh(1) invocations
    // (upload_binary + launch_and_capture_handshake)", describing the
    // pre-3921a43 design that no longer matches the code; that stale
    // mismatch, not CI nondeterminism, was the actual cause of this test's
    // long-standing CI-only failure, issue #6). `resolve_helper_binary`
    // also makes zero `ssh(1)` calls here since `--isekai-helper-binary`
    // is explicit (skips `detect_remote_arch`). So exactly one
    // `exec_request` here means the re-bootstrap happened exactly once,
    // not that it was retried an extra time.
    assert_eq!(
        deploy_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "expected exactly one re-bootstrap deploy (1 combined ssh exec: install_and_launch)"
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
    let (mock_sshd_addr, _mock_sshd_fingerprint) = spawn_fake_ssh_server(remote_home.clone(), client_pubkey, deploy_count.clone()).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let shim = shim_ssh_with_bootstrap_config(tmp.path(), &home, "stale-no-recover-host", mock_sshd_addr, &key_path);

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
            .arg("--isekai-ssh-path")
            .arg(&shim.isekai_ssh_path_arg)
            .arg("--isekai-no-bootstrap")
            .arg("stale-no-recover-host")
            .env("HOME", &home)
            .env("PATH", &shim.path_env)
            .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
            .env("ISEKAI_PIPE_RUNTIME_DIR", tmp.path().join("runtime"))
            .envs(shim.extra_env)
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
    let (mock_sshd_addr, mock_sshd_fingerprint) = spawn_fake_ssh_server(remote_home.clone(), client_pubkey, deploy_count.clone()).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    // Same reasoning as `wrapper_silently_recovers_from_a_stale_trust_signal_and_reconnects`:
    // see `seed_ssh_host_key_trust`'s docs.
    seed_ssh_host_key_trust(&home, &format!("127.0.0.1:{}", mock_sshd_addr.port()), &mock_sshd_fingerprint);
    let shim = shim_ssh_with_bootstrap_config(tmp.path(), &home, "unreachable-host", mock_sshd_addr, &key_path);

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
        .arg("--isekai-ssh-path")
        .arg(&shim.isekai_ssh_path_arg)
        .arg("--isekai-helper-binary")
        .arg(&helper_script_path)
        .arg("unreachable-host")
        .env("HOME", &home)
        .env("PATH", &shim.path_env)
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        // See the identical comment in the sibling stale-trust test above:
        // without this, this test shared a `%TEMP%\isekai-<uid>` runtime
        // dir with every other test in this file, which a real
        // `test-windows` CI failure (2026-07-23) showed causing this exact
        // test to misclassify as `StaleTrust` instead of `Unreachable`.
        .env("ISEKAI_PIPE_RUNTIME_DIR", tmp.path().join("runtime"))
        // Verbose bootstrap-progress messages (including "Registered ...
        // in ...", which this test counts below) now default to
        // `isekai-ssh`'s own log file rather than stderr — "the cached
        // deployment could not be reached" (checked via
        // `saw_unreachable_notice` below) stays on stderr unchanged.
        .env("ISEKAI_PIPE_LOG_FILE", verbose_log_path_under(&home))
        .envs(shim.extra_env)
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
                let registered_count = verbose_log_registered_count(&verbose_log_path_under(&home));
                if registered_count >= 1 && saw_unreachable_notice {
                    saw_second_registration = true;
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    // [diag] temporary: see the identical comment in the sibling stale-trust
    // test above.
    let verbose_log_contents = std::fs::read_to_string(verbose_log_path_under(&home)).unwrap_or_else(|e| format!("<failed to read verbose log: {e}>"));

    assert!(
        saw_unreachable_notice,
        "expected wrapper stderr to report a detected connect-failure (unreachable) signal:\n{stderr_log}\n[diag] verbose log:\n{verbose_log_contents}"
    );
    assert!(saw_second_registration, "expected the re-bootstrap to complete and register a refreshed profile:\n{stderr_log}\n[diag] verbose log:\n{verbose_log_contents}");
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
