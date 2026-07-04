//! End-to-end tests for `isekai-ssh init` (`ISEKAI_SSH_DESIGN.md` フェーズ分割案
//! S-3's acceptance criteria: "isekai-helper未配置ホストに対しinit→connectの
//! 一連が通ること").
//!
//! ## Why the deploy step uses a stand-in script, not a live `--relay` handshake
//!
//! `init` always launches the uploaded binary with `--relay <addr> --relay-sni
//! <name> --relay-jwt <token>` (`isekai-bootstrap::openssh::OpenSshBackend`,
//! unchanged by this phase). The *real* `isekai-helper --relay` path
//! (`isekai_link_masque::connect_relay_agent`) validates the relay's
//! certificate against the real `webpki-roots` CA set — by design, so a
//! production relay's ACME-issued cert is verified for real. That makes it
//! impossible for *any* locally-run mock relay (necessarily self-signed) to
//! complete a real handshake with the actual compiled `isekai-helper`
//! binary in an offline test, regardless of how faithfully the mock
//! reimplements the MASQUE wire protocol.
//!
//! `isekai-bootstrap/tests/openssh_e2e.rs` hits the exact same wall and
//! solves it the same way: the binary `install_and_start` uploads and
//! launches is a tiny shell script that ignores the `--relay-*` flags and
//! just echoes canned handshake JSON, proving the upload/launch/poll/capture
//! *plumbing* end to end over a real `ssh(1)` subprocess without requiring a
//! live relay. This test file follows that precedent for the same reason.
//!
//! To still prove `connect` (the second half of "init→connect") against a
//! *genuinely running* `isekai-helper`, the stand-in script echoes the
//! handshake of a real, independently-spawned `isekai-helper` process
//! (bound directly, no `--relay` — exactly like `connect_e2e.rs`/
//! `trust_store_e2e.rs`'s own real-helper setup) with `relay_public_addr`
//! pointed at that real instance's listen address. So: `init`'s own
//! CLI/bootstrap/trust-store-write logic runs for real against a real `ssh`
//! subprocess and a real mock sshd, and the trust store entry it produces
//! then lets `connect` (also unmodified, no `--dev-insecure-*`) drive a real
//! HELLO/proof/ACK and SSH session against a real `isekai-helper` process.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

use isekai_protocol::handshake::{decode_handshake_json, HandshakeJson};
use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
use russh::{Channel as RusshChannel, ChannelId, CryptoVec};
use russh_keys::ssh_key::private::Ed25519Keypair;
use russh_keys::{PrivateKey, PublicKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener as TokioTcpListener;
use tokio::process::Command as TokioCommand;

// ---------------------------------------------------------------------
// Shared plumbing (mirrors connect_e2e.rs/trust_store_e2e.rs; duplicated
// rather than factored into a shared test-support module, matching this
// crate's existing convention of one self-contained file per scenario).
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
/// `current_exe()` rather than using a `CARGO_BIN_EXE_*` variable (that
/// mechanism only covers binaries of the package currently being compiled,
/// and `isekai-helper` is a separate workspace package).
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
    let keypair = Ed25519Keypair::from_seed(&[9u8; 32]);
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

/// Spawns the real compiled `isekai-helper` binary directly (no `--relay`),
/// standing in for "the isekai-helper instance that a real deployment would
/// have left running" — see this file's module docs for why `init`'s own
/// deploy step can't drive this through a live `--relay` handshake.
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

/// Locates the real system `ssh(1)` via `PATH` (before this test starts
/// shadowing `PATH` with the wrapper below).
fn real_ssh_path() -> PathBuf {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v ssh")
        .output()
        .expect("failed to run `command -v ssh`");
    assert!(out.status.success(), "ssh(1) not found on PATH");
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim().to_string())
}

/// `isekai-bootstrap::OpenSshBackend` (as driven through `isekai-ssh init`'s
/// CLI, with no test hook for extra `ssh(1)` args) spawns plain `ssh`,
/// resolved via `PATH`, with no way to point it at a throwaway config file
/// short of `-F`. This environment's `ssh(1)` build resolves its *default*
/// per-user config path via the real passwd-database home directory rather
/// than the `HOME` environment variable actually passed to the child
/// process, so overriding `$HOME` alone (as `trust_store_e2e.rs` does for
/// the trust store lookup) does not work for the config file. Instead, this
/// installs a tiny `ssh` shim ahead of the real one on `$PATH` that always
/// adds `-F <this test's throwaway config>` — functionally identical to
/// what a real user's own `~/.ssh/config` would provide for a
/// freshly-provisioned host (`ISEKAI_SSH_DESIGN.md`'s own recommended
/// `~/.ssh/config` stanza), just injected without touching the test
/// runner's actual home directory.
///
/// Returns `(bin_dir, path_env)`: `bin_dir` must outlive the `isekai-ssh
/// init` subprocess (it contains the shim), and `path_env` is the `PATH`
/// value to set on that subprocess.
fn shim_ssh_with_bootstrap_config(
    tmp: &std::path::Path,
    alias: &str,
    mock_sshd_addr: SocketAddr,
    key_path: &std::path::Path,
) -> (PathBuf, std::ffi::OsString) {
    let config_path = tmp.join("ssh_config_bootstrap");
    let config = format!(
        "Host {alias}\n\
         \x20\x20\x20\x20HostName 127.0.0.1\n\
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
    let shim = format!(
        "#!/bin/sh\nexec {real_ssh} -F {config} \"$@\"\n",
        real_ssh = real_ssh_path().display(),
        config = config_path.display(),
    );
    std::fs::write(&shim_path, shim).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let path_env = {
        let mut paths = vec![bin_dir.clone()];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        std::env::join_paths(paths).unwrap()
    };
    (bin_dir, path_env)
}

fn trust_store_path_under(home: &std::path::Path) -> PathBuf {
    home.join(".config").join(isekai_trust::store::CONFIG_DIR_NAME).join(isekai_trust::store::TRUST_STORE_FILE_NAME)
}

/// Builds the stand-in "isekai-helper" script `init --helper-binary` is
/// pointed at: ignores every argument (including the `--relay-*` flags
/// `OpenSshBackend` always passes) and echoes `real_helper`'s actual
/// handshake JSON, with `relay_public_addr` set to that real process's own
/// listen address — see this file's module docs.
fn stand_in_helper_script(real_helper_addr: SocketAddr, real_handshake: &HandshakeJson) -> Vec<u8> {
    let mut handshake = real_handshake.clone();
    handshake.relay_public_addr = Some(real_helper_addr.to_string());
    let json_line = serde_json::to_string(&handshake).unwrap();
    format!("#!/bin/sh\necho '{json_line}'\n").into_bytes()
}

async fn spawn_init(
    home: &std::path::Path,
    host_alias: &str,
    helper_binary_path: &std::path::Path,
    path_env: &std::ffi::OsStr,
    stdin_line: &str,
) -> std::process::Output {
    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .arg("init")
        .arg(host_alias)
        .arg("--helper-binary")
        .arg(helper_binary_path)
        .arg("--relay-addr")
        .arg("127.0.0.1:1")
        .arg("--relay-sni")
        .arg("relay.isekai-ssh.test")
        .arg("--relay-jwt")
        .arg("test-jwt-token")
        .env("HOME", home)
        .env("PATH", path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh init");

    child.stdin.take().unwrap().write_all(stdin_line.as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("isekai-ssh init should not hang")
        .expect("failed to wait for isekai-ssh init")
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

/// Full acceptance scenario: `init` deploys+registers trust for a
/// never-before-seen host, then `connect` (no `--dev-insecure-*`) uses that
/// freshly-written trust store entry to drive a real SSH session through a
/// real `isekai-helper` process.
#[tokio::test(flavor = "multi_thread")]
async fn init_then_connect_succeeds_for_a_freshly_deployed_host() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home, client_pubkey).await;

    // The "already deployed" real isekai-helper instance init's stand-in
    // script will hand back the credentials for (see module docs).
    let real_helper = spawn_helper(mock_sshd_addr);
    let real_helper_addr: SocketAddr = format!("127.0.0.1:{}", real_helper.handshake.listen_port).parse().unwrap();

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let (_bin_dir, path_env) = shim_ssh_with_bootstrap_config(tmp.path(), "dummy-host", mock_sshd_addr, &key_path);

    let helper_script = stand_in_helper_script(real_helper_addr, &real_helper.handshake);
    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(&helper_script_path, &helper_script).unwrap();

    let output = spawn_init(&home, "dummy-host", &helper_script_path, &path_env, "y\n").await;
    eprintln!("init stdout:\n{}", String::from_utf8_lossy(&output.stdout));
    eprintln!("init stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "isekai-ssh init failed: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Helper identity:"), "expected identity line in init output: {stdout}");
    assert!(stdout.contains(&real_helper.handshake.cert_sha256), "expected cert_sha256 to appear in init output: {stdout}");
    assert!(stdout.contains("Registered"), "expected a confirmation of trust-store registration: {stdout}");

    let trust_store_path = trust_store_path_under(&home);
    assert!(trust_store_path.exists(), "expected trust store to be written at {trust_store_path:?}");
    let store = isekai_trust::load_trust_store(&trust_store_path).unwrap();
    let entry = store.get("dummy-host:22").expect("expected a trust entry for dummy-host:22");
    assert_eq!(entry.cached_relay_addr, real_helper_addr.to_string());
    assert_eq!(entry.cached_cert_sha256, real_helper.handshake.cert_sha256);
    assert_eq!(entry.cached_session_secret, real_helper.handshake.session_secret);
    assert_eq!(entry.update_policy, isekai_trust::UpdatePolicy::ExactDigestOnly);

    // Second half: `connect` (default build, no --dev-insecure-*) drives a
    // real SSH login through the real isekai-helper process using exactly
    // the trust store entry `init` just wrote.
    let proxy_command = format!("{} connect dummy-host", isekai_ssh_bin_path().display());
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
            .arg("echo hello-from-init-then-connect")
            .env("HOME", &home)
            .output(),
    )
    .await
    .expect("ssh should not hang")
    .expect("failed to spawn ssh(1)");

    eprintln!("connect ssh stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "ssh exited with {:?}; stdout={stdout:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("hello-from-init-then-connect"), "unexpected ssh stdout: {stdout:?}");
}

/// Declining the `[y/N]` prompt must leave the trust store untouched.
#[tokio::test(flavor = "multi_thread")]
async fn init_writes_nothing_when_confirmation_is_declined() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home, client_pubkey).await;
    let real_helper = spawn_helper(mock_sshd_addr);
    let real_helper_addr: SocketAddr = format!("127.0.0.1:{}", real_helper.handshake.listen_port).parse().unwrap();

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let (_bin_dir, path_env) = shim_ssh_with_bootstrap_config(tmp.path(), "dummy-host", mock_sshd_addr, &key_path);

    let helper_script = stand_in_helper_script(real_helper_addr, &real_helper.handshake);
    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(&helper_script_path, &helper_script).unwrap();

    let trust_store_path = trust_store_path_under(&home);
    assert!(!trust_store_path.exists(), "trust store must not exist before this test runs");

    let output = spawn_init(&home, "dummy-host", &helper_script_path, &path_env, "n\n").await;
    eprintln!("init stdout:\n{}", String::from_utf8_lossy(&output.stdout));
    eprintln!("init stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "declining the prompt should not itself be an error: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Aborted"), "expected an explicit abort message: {stdout}");

    assert!(
        !trust_store_path.exists(),
        "declining the confirmation must not create a trust store file at {trust_store_path:?}"
    );
}
