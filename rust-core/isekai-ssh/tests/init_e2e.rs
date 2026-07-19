//! End-to-end tests for `isekai-ssh init` (`archive/ISEKAI_SSH_DESIGN.md` フェーズ分割案
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

/// Renders `path` with forward slashes, safe to embed inside a hand-built
/// `ProxyCommand=...` string that a POSIX-shell-hosted `ssh(1)` (Git for
/// Windows' bundled one, which a bare `ssh` resolves to on the
/// `test-windows` CI runner) execs via `sh -c`. Needed only on Windows:
/// `Path::display()` there renders `\`-separated components, and an
/// unquoted `\` inside a POSIX shell command string is an escape character
/// — embedding a raw Windows path silently mangles it (confirmed via a real
/// `test-windows` CI failure: `sh -c` reported `exec: <path with every `\`
/// stripped>: not found`). Windows' own filesystem APIs accept forward
/// slashes exactly as well as backslashes, so this is a lossless
/// substitution, not a real path translation. A no-op on Unix, where paths
/// are already `/`-separated (see `wrapper_auto_bootstrap_e2e.rs::posix_safe_path`,
/// same idea, needed there for a different embedded-in-a-shell-script case).
fn posix_safe_path(path: &std::path::Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// Locates a sibling workspace package's binary by walking up from
/// `current_exe()` rather than using a `CARGO_BIN_EXE_*` variable (that
/// mechanism only covers binaries of the package currently being compiled,
/// and `isekai-helper`/`isekai-pipe` are separate workspace packages).
fn sibling_bin_path(package: &str, bin_name: &str) -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // this test binary itself
    if path.ends_with("deps") {
        path.pop();
    }
    let is_release = path.file_name().map(|n| n == "release").unwrap_or(false);
    // Windows binaries carry a `.exe` extension; a bare `bin_name` never
    // exists there, so this would otherwise always fall through to the
    // rebuild-and-recheck path below and still fail the same `path.exists()`
    // check afterward (confirmed via a real `test-windows` CI failure).
    path.push(if cfg!(windows) { format!("{bin_name}.exe") } else { bin_name.to_string() });

    if !path.exists() {
        eprintln!("{bin_name} binary not found at {path:?}; building it now");
        let mut cmd = std::process::Command::new(env!("CARGO"));
        cmd.args(["build", "-p", package]);
        if is_release {
            cmd.arg("--release");
        }
        let status = cmd.status().unwrap_or_else(|_| panic!("failed to invoke `cargo build -p {package}`"));
        assert!(status.success(), "`cargo build -p {package}` failed");
        assert!(path.exists(), "{bin_name} binary still missing at {path:?} after building it");
    }
    path
}

fn isekai_pipe_bin_path() -> PathBuf {
    sibling_bin_path("isekai-pipe", "isekai-pipe")
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

/// Spawns the real compiled `isekai-pipe serve` (no `--relay`), standing in
/// for "the isekai-pipe serve instance that a real deployment would have
/// left running" — see this file's module docs for why `init`'s own deploy
/// step can't drive this through a live `--relay` handshake.
fn spawn_helper(target_addr: SocketAddr) -> HelperProcess {
    spawn_helper_with_args(target_addr, &[])
}

/// Like `spawn_helper`, but forwards `extra_args` to the real `isekai-pipe
/// serve` process (`#20b`: used to pass `--stun-server` so the real
/// handshake this test's stand-in script relays back genuinely carries a
/// `server-reflexive` candidate).
fn spawn_helper_with_args(target_addr: SocketAddr, extra_args: &[&str]) -> HelperProcess {
    let mut cmd = std::process::Command::new(isekai_pipe_bin_path());
    cmd.arg("serve")
        .arg("--target")
        .arg(target_addr.to_string())
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--log-level")
        .arg("debug")
        .args(extra_args)
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn isekai-pipe serve");
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
#[cfg(unix)]
fn real_ssh_path() -> PathBuf {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v ssh")
        .output()
        .expect("failed to run `command -v ssh`");
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

/// Everything needed to point `isekai-ssh init` (and `isekai-bootstrap::OpenSshBackend`'s
/// own deploy dial, via `init`'s `--ssh-path`) at a stand-in `ssh(1)` — see
/// `wrapper_auto_bootstrap_e2e.rs::SshShim` and `ssh_test_shim`'s module
/// docs for why Windows needs a compiled `.exe` shim (not a `.cmd` batch
/// file) and Unix a `#!/bin/sh` script.
struct SshShim {
    isekai_ssh_path_arg: PathBuf,
    extra_env: Vec<(&'static str, PathBuf)>,
    path_env: std::ffi::OsString,
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
/// freshly-provisioned host (`archive/ISEKAI_SSH_DESIGN.md`'s own recommended
/// `~/.ssh/config` stanza), just injected without touching the test
/// runner's actual home directory.
///
/// Returns an [`SshShim`] describing how to point `isekai-ssh init` at it
/// (`--ssh-path` + `PATH` + any extra env vars needed).
/// Also writes the identical `Host` block to `home/.ssh/config`, not just the
/// shim-only throwaway config — the Windows-native path
/// (`isekai-bootstrap::russh_backend::resolve_hop`, reached via `isekai-ssh
/// init`'s `bootstrap_and_register`) does its own `openssh_config::
/// resolve_default` directly against `$HOME/.ssh/config`; it never shells out
/// to `ssh(1)`, so it never sees the shim-only config the `-F` flag points
/// at. See `wrapper_auto_bootstrap_e2e.rs::shim_ssh_with_bootstrap_config`'s
/// doc comment (confirmed via a real `test-windows` CI failure there).
fn shim_ssh_with_bootstrap_config(
    tmp: &std::path::Path,
    home: &std::path::Path,
    alias: &str,
    mock_sshd_addr: SocketAddr,
    key_path: &std::path::Path,
) -> SshShim {
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
    let (isekai_ssh_path_arg, extra_env) = (
        PathBuf::from(env!("CARGO_BIN_EXE_ssh_test_shim")),
        vec![("ISEKAI_SSH_TEST_SHIM_REAL_SSH", real_ssh), ("ISEKAI_SSH_TEST_SHIM_CONFIG", config_path)],
    );

    let path_env = {
        let mut paths = vec![bin_dir];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        std::env::join_paths(paths).unwrap()
    };
    SshShim { isekai_ssh_path_arg, extra_env, path_env }
}

fn profiles_dir_under(home: &std::path::Path) -> PathBuf {
    home.join(".local").join("state").join("isekai").join("profiles")
}

/// Mirrors `isekai_pipe_core::profile::sanitize_filename_component`'s `:` ->
/// `%3A` escaping (private to that crate) — every key this file uses is a
/// plain `host:port` string, so replicating just that one substitution is
/// enough to predict the on-disk filename `write_persistent_profile`
/// actually produces.
fn profile_path_under(home: &std::path::Path, key: &str) -> PathBuf {
    profiles_dir_under(home).join(format!("{}.json", key.replace(':', "%3A")))
}

/// Builds the stand-in "isekai-helper" script `init --helper-binary` is
/// pointed at: ignores every argument (including the `--relay-*` flags and
/// `--bootstrap-request-file` `OpenSshBackend` always passes) and echoes
/// `real_helper`'s actual handshake JSON, with `relay_public_addr` set to
/// that real process's own listen address — see this file's module docs.
///
/// `#20a-4`: `OpenSshBackend::launch_and_capture_handshake` now always
/// decodes stdout as a `BootstrapReportV2` envelope (every real launch sends
/// a `BootstrapRequestV2`, so a compliant `isekai-pipe serve` always echoes
/// one back) — so this stand-in must wrap the handshake the same way, with
/// arbitrary valid `session_id`/`bootstrap_attempt_id` since this test
/// doesn't correlate them against the request the real `OpenSshBackend`
/// sent.
fn stand_in_helper_script(real_helper_addr: SocketAddr, real_handshake: &HandshakeJson) -> Vec<u8> {
    let mut handshake = real_handshake.clone();
    handshake
        .candidates
        .retain(|candidate| candidate.kind != isekai_protocol::handshake::CANDIDATE_RELAYED);
    handshake.candidates.push(isekai_protocol::handshake::HandshakeCandidate {
        kind: isekai_protocol::handshake::CANDIDATE_RELAYED.to_string(),
        endpoint: Some(real_helper_addr.to_string()),
        port: None,
        source: Some("isekai-link-relay".to_string()),
    });
    let report = serde_json::json!({
        "v": isekai_protocol::BOOTSTRAP_PROTOCOL_V2,
        "session_id": "77".repeat(16),
        "bootstrap_attempt_id": "88".repeat(16),
        "handshake": handshake,
    });
    let json_line = serde_json::to_string(&report).unwrap();
    format!("#!/bin/sh\necho '{json_line}'\n").into_bytes()
}

async fn spawn_init(
    home: &std::path::Path,
    host_alias: &str,
    helper_binary_path: &std::path::Path,
    shim: &SshShim,
    stdin_line: &str,
) -> std::process::Output {
    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .arg("init")
        .arg(host_alias)
        .arg("--ssh-path")
        .arg(&shim.isekai_ssh_path_arg)
        .arg("--helper-binary")
        .arg(helper_binary_path)
        .arg("--relay-addr")
        .arg("127.0.0.1:1")
        .arg("--relay-sni")
        .arg("relay.isekai-ssh.test")
        .arg("--relay-jwt")
        .arg("test-jwt-token")
        .env("HOME", home)
        .env("PATH", &shim.path_env)
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(home))
        .envs(shim.extra_env.clone())
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
    let real_helper_addr: SocketAddr = format!("127.0.0.1:{}", real_helper.handshake.direct_by_bootstrap_host_port().unwrap()).parse().unwrap();

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let shim = shim_ssh_with_bootstrap_config(tmp.path(), &home, "dummy-host", mock_sshd_addr, &key_path);

    let helper_script = stand_in_helper_script(real_helper_addr, &real_helper.handshake);
    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(&helper_script_path, &helper_script).unwrap();

    let output = spawn_init(&home, "dummy-host", &helper_script_path, &shim, "y\n").await;
    eprintln!("init stdout:\n{}", String::from_utf8_lossy(&output.stdout));
    eprintln!("init stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "isekai-ssh init failed: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Helper identity:"), "expected identity line in init output: {stdout}");
    assert!(stdout.contains(&real_helper.handshake.cert_sha256()), "expected cert_sha256 to appear in init output: {stdout}");
    assert!(stdout.contains("Registered"), "expected a confirmation of trust-store registration: {stdout}");

    let profile_path = profile_path_under(&home, "dummy-host:22");
    assert!(profile_path.exists(), "expected profile to be written at {profile_path:?}");
    let profile = isekai_pipe_core::load_persistent_profile(&profiles_dir_under(&home), "dummy-host:22")
        .unwrap()
        .expect("expected a profile for dummy-host:22");
    let legacy_relay = profile.legacy_relay_transport.as_ref().expect("expected a cached relay transport");
    assert_eq!(legacy_relay.helper_addr, real_helper_addr.to_string());
    assert_eq!(profile.server_identity.cert_sha256_hex, real_helper.handshake.cert_sha256());
    assert_eq!(legacy_relay.session_secret_b64, real_helper.handshake.session_secret);
    assert_eq!(profile.update_policy, isekai_trust::UpdatePolicy::ExactDigestOnly);

    // Second half: `isekai-pipe connect` drives a real SSH login through the
    // real isekai-helper process using exactly the trust store entry `init`
    // just wrote (the standalone `isekai-ssh connect` subcommand this test
    // used to exercise directly has been removed now that the wrapper +
    // `isekai-pipe connect` cover the same ground, `archive/ISEKAI_PIPE_MIGRATION.md`
    // P5).
    let proxy_command =
        format!("{} connect --profile dummy-host --service ssh --stdio", posix_safe_path(&isekai_pipe_bin_path()));
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
            // `isekai-pipe connect` (spawned as this `ssh`'s `ProxyCommand`
            // child, inheriting this env) looks up the profile `init` just
            // registered via `isekai_pipe_core::default_profiles_dir`,
            // which checks `LOCALAPPDATA` before `HOME` on Windows -- see
            // `wrapper_auto_bootstrap_e2e.rs`'s equivalent comment for why
            // `HOME` alone doesn't redirect it there (confirmed via a real
            // `test-windows` CI failure: without this, `connect` reported
            // the fresh profile as untrusted, looking in the real CI
            // runner's actual `%LOCALAPPDATA%` instead).
            .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
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

/// Minimal mock STUN server (RFC 5389 Binding Request/Response), same shape
/// used throughout this workspace.
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
            resp.extend_from_slice(&0x0101u16.to_be_bytes());
            resp.extend_from_slice(&12u16.to_be_bytes());
            resp.extend_from_slice(&magic_cookie.to_be_bytes());
            resp.extend_from_slice(transaction_id);
            resp.extend_from_slice(&0x0020u16.to_be_bytes());
            resp.extend_from_slice(&8u16.to_be_bytes());
            resp.push(0);
            resp.push(0x01);
            resp.extend_from_slice(&xport.to_be_bytes());
            resp.extend_from_slice(&xaddr.to_be_bytes());

            let _ = server.send_to(&resp, from);
        }
    });
    addr
}

/// `#20b`: `isekai-ssh init --stun-server <addr>` must (a) actually pass the
/// STUN server through the whole bootstrap pipeline down to the real
/// `isekai-pipe serve` process (which then reports a real `server-reflexive`
/// candidate in its handshake) and (b) capture that candidate's endpoint
/// into `HelperTrust.cached_stun_observed_addr`.
#[tokio::test(flavor = "multi_thread")]
async fn init_with_stun_server_saves_the_observed_address_to_the_trust_store() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home, client_pubkey).await;
    let stun_server = spawn_mock_stun_server();

    // A *real* `isekai-pipe serve` process launched with `--stun-server`, so
    // its own handshake genuinely carries a `server-reflexive` candidate —
    // `stand_in_helper_script` below relays this handshake through
    // untouched (aside from re-pointing the `relayed` candidate), so
    // whatever `init` receives is exactly what a real deployment would see.
    let real_helper = spawn_helper_with_args(mock_sshd_addr, &["--stun-server", &stun_server.to_string()]);
    assert!(
        real_helper.handshake.stun_observed_addr().is_some(),
        "the real isekai-pipe serve process should have reported its own STUN-observed address"
    );
    let real_helper_addr: SocketAddr =
        format!("127.0.0.1:{}", real_helper.handshake.direct_by_bootstrap_host_port().unwrap()).parse().unwrap();

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let shim = shim_ssh_with_bootstrap_config(tmp.path(), &home, "stun-host", mock_sshd_addr, &key_path);

    let helper_script = stand_in_helper_script(real_helper_addr, &real_helper.handshake);
    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(&helper_script_path, &helper_script).unwrap();

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .arg("init")
        .arg("stun-host")
        .arg("--ssh-path")
        .arg(&shim.isekai_ssh_path_arg)
        .arg("--helper-binary")
        .arg(&helper_script_path)
        .arg("--relay-addr")
        .arg("127.0.0.1:1")
        .arg("--relay-sni")
        .arg("relay.isekai-ssh.test")
        .arg("--relay-jwt")
        .arg("test-jwt-token")
        .arg("--stun-server")
        .arg(stun_server.to_string())
        .env("HOME", &home)
        .env("PATH", &shim.path_env)
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        .envs(shim.extra_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh init");
    child.stdin.take().unwrap().write_all(b"y\n").await.unwrap();
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("isekai-ssh init should not hang")
        .expect("failed to wait for isekai-ssh init");

    eprintln!("init stdout:\n{}", String::from_utf8_lossy(&output.stdout));
    eprintln!("init stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "isekai-ssh init failed: {output:?}");

    let profile = isekai_pipe_core::load_persistent_profile(&profiles_dir_under(&home), "stun-host:22")
        .unwrap()
        .expect("expected a profile for stun-host:22");
    assert_eq!(
        profile.cached_stun_observed_addr.as_deref(),
        real_helper.handshake.stun_observed_addr(),
        "cached_stun_observed_addr should match the real helper's own server-reflexive candidate"
    );
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
    let real_helper_addr: SocketAddr = format!("127.0.0.1:{}", real_helper.handshake.direct_by_bootstrap_host_port().unwrap()).parse().unwrap();

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let shim = shim_ssh_with_bootstrap_config(tmp.path(), &home, "dummy-host", mock_sshd_addr, &key_path);

    let helper_script = stand_in_helper_script(real_helper_addr, &real_helper.handshake);
    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(&helper_script_path, &helper_script).unwrap();

    let profile_path = profile_path_under(&home, "dummy-host:22");
    assert!(!profile_path.exists(), "profile must not exist before this test runs");

    let output = spawn_init(&home, "dummy-host", &helper_script_path, &shim, "n\n").await;
    eprintln!("init stdout:\n{}", String::from_utf8_lossy(&output.stdout));
    eprintln!("init stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "declining the prompt should not itself be an error: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Aborted"), "expected an explicit abort message: {stdout}");

    assert!(
        !profile_path.exists(),
        "declining the confirmation must not create a profile file at {profile_path:?}"
    );
}
