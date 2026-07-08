//! End-to-end test for the wrapper's auto-bootstrap path
//! (`wrapper.rs::bootstrap_and_register`, `archive/ISEKAI_PIPE_MIGRATION.md` P4's
//! last item): a never-before-seen destination triggers a
//! `direct-by-bootstrap-host` deploy over a real `ssh(1)` subprocess,
//! prompts for confirmation on stderr, and — on `y` — registers the trust
//! store entry the wrapper (and subsequent plain `isekai-ssh <destination>`
//! invocations) will read from then on.
//!
//! Mirrors `init_e2e.rs`'s mock-sshd/stand-in-helper-script plumbing (this
//! crate's convention: one self-contained e2e file per scenario, see
//! `stdout_cleanliness.rs`'s module docs for why that's duplicated rather
//! than shared).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

use isekai_protocol::handshake::decode_handshake_json;
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
    let keypair = Ed25519Keypair::from_seed(&[11u8; 32]);
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
        .expect("failed to run ssh-keygen");
    assert!(status.success(), "ssh-keygen exited non-zero");

    let pub_path = dir.join("client_id_ed25519.pub");
    let pub_text = std::fs::read_to_string(&pub_path).expect("failed to read generated .pub file");
    let public_key = PublicKey::from_openssh(pub_text.trim()).expect("failed to parse generated public key");
    (key_path, public_key)
}

fn real_ssh_path() -> PathBuf {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v ssh")
        .output()
        .expect("failed to run `command -v ssh`");
    assert!(out.status.success(), "ssh(1) not found on PATH");
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim().to_string())
}

/// Same technique as `init_e2e.rs::shim_ssh_with_bootstrap_config`: a tiny
/// `ssh` shim ahead of the real one on `$PATH` that always injects `-F
/// <throwaway config>`, standing in for a real `~/.ssh/config` entry without
/// touching the test runner's actual home directory. Also used for the
/// wrapper's own internal `ssh -G` call (`wrapper.rs::resolve_openssh_effective_config`),
/// so both that call and the real `ssh` exec at the end of `run()` see the
/// same resolved config.
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
         \x20\x20\x20\x20UserKnownHostsFile /dev/null\n\
         \n\
         # The wrapper's auto-bootstrap step dials the *resolved*
         # bootstrap-candidate address directly (`wrapper.rs::bootstrap_and_register`),
         # not the `{alias}` alias above, so it needs its own matching block
         # with the same test identity/trust settings.
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

const VALID_HANDSHAKE_JSON: &str = r#"{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}},"candidates":[{"kind":"direct-by-bootstrap-host","port":45231,"source":"bootstrap-ssh"}]}"#;

#[tokio::test(flavor = "multi_thread")]
async fn wrapper_auto_bootstraps_an_untrusted_destination_on_confirmation() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home, client_pubkey).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let (_bin_dir, path_env) =
        shim_ssh_with_bootstrap_config(tmp.path(), "auto-bootstrap-host", mock_sshd_addr, &key_path);

    // Stand-in for the isekai-helper binary: ignores its args, just emits
    // one line of valid handshake JSON — same technique as
    // `init_e2e.rs`/`isekai-bootstrap/tests/openssh_e2e.rs`.
    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(&helper_script_path, format!("#!/bin/sh\necho '{VALID_HANDSHAKE_JSON}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper_script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let trust_store_path = trust_store_path_under(&home);
    assert!(!trust_store_path.exists(), "trust store must not exist before this test runs");

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .arg("--isekai-helper-binary")
        .arg(&helper_script_path)
        .arg("auto-bootstrap-host")
        .env("HOME", &home)
        .env("PATH", &path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");

    child.stdin.take().unwrap().write_all(b"y\n").await.unwrap();

    // The wrapper proceeds to exec a real `ssh` with `ProxyCommand isekai-pipe
    // connect ...` after bootstrap succeeds; that connect attempt has nothing
    // real to talk to (the stand-in helper script already exited after
    // printing its one line) and will eventually fail/hang on its own, which
    // is fine — this test only cares that bootstrap+registration completed
    // first, so it reads stderr until the registration line shows up rather
    // than waiting for the whole process to exit.
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut saw_registered = false;
    for _ in 0..200 {
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_secs(20), stderr.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                eprint!("[isekai-ssh stderr] {line}");
                if line.contains("Registered") {
                    saw_registered = true;
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    assert!(saw_registered, "expected wrapper stderr to report trust-store registration");
    assert!(trust_store_path.exists(), "expected trust store to be written at {trust_store_path:?}");

    let store = isekai_trust::load_trust_store(&trust_store_path).unwrap();
    let entry = store.get("auto-bootstrap-host:22").expect("expected a trust entry for auto-bootstrap-host:22");
    assert_eq!(entry.cached_relay_addr, "127.0.0.1:45231");
    let handshake = decode_handshake_json(VALID_HANDSHAKE_JSON.as_bytes()).unwrap();
    assert_eq!(entry.cached_cert_sha256, handshake.cert_sha256());
    assert_eq!(entry.update_policy, isekai_trust::UpdatePolicy::ExactDigestOnly);
}

/// `#@isekai remote-path` (`isekai-ssh/src/wrapper.rs::resolve_isekai_config`)
/// must actually reach `OpenSshBackend::install_and_start` — the wrapper's own
/// `$HOME/.ssh/config` (not the mock-sshd shim config) is where this
/// directive is parsed from, mirroring how a real user would configure it.
#[tokio::test(flavor = "multi_thread")]
async fn wrapper_auto_bootstrap_honors_remote_path_directive() {
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
    let (_bin_dir, path_env) =
        shim_ssh_with_bootstrap_config(tmp.path(), "remote-path-host", mock_sshd_addr, &key_path);

    // The wrapper's own directive parsing falls back to `$HOME/.ssh/config`
    // when no `-F` was passed to `isekai-ssh` itself (`wrapper.rs::config_roots`)
    // — independent of the shim config above, which only exists to point the
    // real `ssh(1)` invocations at the mock sshd.
    let ssh_dir = home.join(".ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    std::fs::write(
        ssh_dir.join("config"),
        "Host remote-path-host\n    #@isekai remote-path ~/custom/isekai-pipe-bin\n",
    )
    .unwrap();

    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(&helper_script_path, format!("#!/bin/sh\necho '{VALID_HANDSHAKE_JSON}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper_script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .arg("--isekai-helper-binary")
        .arg(&helper_script_path)
        .arg("remote-path-host")
        .env("HOME", &home)
        .env("PATH", &path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");

    child.stdin.take().unwrap().write_all(b"y\n").await.unwrap();

    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut saw_registered = false;
    for _ in 0..200 {
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_secs(20), stderr.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                eprint!("[isekai-ssh stderr] {line}");
                if line.contains("Registered") {
                    saw_registered = true;
                    break;
                }
            }
            _ => break,
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;

    assert!(saw_registered, "expected wrapper stderr to report trust-store registration");

    let uploaded = remote_home.join("custom/isekai-pipe-bin");
    assert!(
        uploaded.exists(),
        "expected the binary to be uploaded at the #@isekai remote-path override {uploaded:?}"
    );
    assert!(
        !remote_home.join(".local/bin").exists(),
        "must not fall back to the default install dir once remote-path is set"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wrapper_auto_bootstrap_writes_nothing_when_confirmation_is_declined() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1)/ssh-keygen(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let remote_home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&remote_home).unwrap();

    let mock_sshd_addr = spawn_fake_ssh_server(remote_home, client_pubkey).await;

    let home = tmp.path().join("client-home");
    std::fs::create_dir_all(&home).unwrap();
    let (_bin_dir, path_env) =
        shim_ssh_with_bootstrap_config(tmp.path(), "declined-bootstrap-host", mock_sshd_addr, &key_path);

    let helper_script_path = tmp.path().join("fake-isekai-helper.sh");
    std::fs::write(&helper_script_path, format!("#!/bin/sh\necho '{VALID_HANDSHAKE_JSON}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper_script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let trust_store_path = trust_store_path_under(&home);

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .arg("--isekai-helper-binary")
        .arg(&helper_script_path)
        .arg("declined-bootstrap-host")
        .env("HOME", &home)
        .env("PATH", &path_env)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");

    child.stdin.take().unwrap().write_all(b"n\n").await.unwrap();

    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("isekai-ssh should not hang after a declined confirmation")
        .expect("failed to wait for isekai-ssh");

    assert!(!output.status.success(), "wrapper must exit non-zero when the user declines the confirmation");
    assert!(output.stdout.is_empty(), "stdout must stay empty, got {:?}", String::from_utf8_lossy(&output.stdout));
    assert!(
        !trust_store_path.exists(),
        "declining the confirmation must not create a trust store file at {trust_store_path:?}"
    );
}
