//! End-to-end acceptance test for the full "初回接続でも2回目接続でも
//! `isekai-ssh <host>` で意識せず接続したい" story: a brand-new host, with
//! **no** `--isekai-helper-binary` given and **no** per-host `#@isekai
//! bootstrap-relay` directive, still completes relay auto-bootstrap in one
//! `isekai-ssh <destination>` invocation (TOFU `[y/N]` confirmation still
//! shown — that stays intentional).
//!
//! This exercises every piece added for that story together:
//! - a `Host *` catch-all `#@isekai bootstrap-relay`/`remote-path` block in
//!   `~/.ssh/config`, the *existing* (no-code-change) mechanism for a
//!   default relay applying to any host without its own block
//!   (`wrapper.rs::load_isekai_directives_from_file`'s first-match-wins
//!   cascade, same semantics as `ssh_config(5)`);
//! - `isekai-ssh login`'s saved token file, pre-seeded the same way
//!   `wrapper_auto_bootstrap_e2e.rs::wrapper_auto_bootstrap_honors_bootstrap_relay_directive`
//!   does;
//! - `helper_download::resolve_helper_binary`'s arch-detect
//!   (`OpenSshBackend::detect_remote_arch`) + download-and-cache path,
//!   pointed at a local mock HTTP server standing in for GitHub Releases via
//!   `ISEKAI_SSH_HELPER_RELEASE_BASE_URL`.
//!
//! Per this crate's self-contained-test-file convention, the mock-sshd
//! plumbing is duplicated from `wrapper_auto_bootstrap_e2e.rs` and the mock
//! HTTP server is duplicated from `helper_download.rs`'s own tests.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

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
            if let Ok(stdout) = stdout_res {
                if !stdout.is_empty() {
                    let _ = handle.data(channel, CryptoVec::from(stdout)).await;
                }
            }
            if let Ok(stderr) = stderr_res {
                if !stderr.is_empty() {
                    let _ = handle.extended_data(channel, 1, CryptoVec::from(stderr)).await;
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

async fn read_all<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).await?;
    Ok(buf)
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
    let out = std::process::Command::new("sh").arg("-c").arg("command -v ssh").output().expect("failed to run `command -v ssh`");
    assert!(out.status.success(), "ssh(1) not found on PATH");
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim().to_string())
}

/// Same technique as `wrapper_auto_bootstrap_e2e.rs::shim_ssh_with_bootstrap_config`,
/// except the alias's own `Host` block deliberately carries **no** `#@isekai`
/// directives at all — this test's whole point is that the `Host *` block in
/// the *separate* `$HOME/.ssh/config` (read directly by the wrapper's own
/// directive parser, not by this shim) supplies the relay default instead.
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

    let path_env = {
        let mut paths = vec![bin_dir.clone()];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        std::env::join_paths(paths).unwrap()
    };
    (bin_dir, path_env)
}

fn profiles_dir_under(home: &std::path::Path) -> PathBuf {
    home.join(".local").join("state").join("isekai").join("profiles")
}

const VALID_BOOTSTRAP_REPORT_JSON_RELAYED: &str = r#"{"v":2,"session_id":"00000000000000000000000000000000","bootstrap_attempt_id":"11111111111111111111111111111111","handshake":{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}},"candidates":[{"kind":"relayed","endpoint":"198.51.100.99:45900","source":"relay"}]}}"#;

/// Minimal single-request-at-a-time HTTP/1.1 mock server, duplicated from
/// `isekai-ssh/src/helper_download.rs`'s own tests per this crate's
/// self-contained-test-file convention.
fn spawn_mock_release_server(routes: std::collections::HashMap<String, Vec<u8>>) -> SocketAddr {
    use std::io::{BufRead, BufReader as StdBufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = StdBufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                continue;
            }
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) if line == "\r\n" || line == "\n" => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
            match routes.get(&path) {
                Some(body) => {
                    let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(body);
                }
                None => {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
            }
            let _ = stream.flush();
        }
    });
    addr
}

/// This test machine's own `uname -m`, normalized the same way
/// `isekai_bootstrap::openssh::normalize_uname_arch` does — the mock sshd
/// genuinely execs `uname -m` via real `sh -c` (see `FakeShellHandler::exec_request`),
/// so the release asset this test serves must be named for *this* arch, not
/// a hardcoded one, to stay meaningful on aarch64 runners too.
fn this_machines_normalized_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => panic!("this test machine's own arch {other:?} isn't one this test can assert against"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn isekai_ssh_bootstraps_a_brand_new_host_via_relay_with_no_binary_flag_and_no_per_host_directive() {
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
    let (_bin_dir, path_env) = shim_ssh_with_bootstrap_config(tmp.path(), "brand-new-host", mock_sshd_addr, &key_path);

    // The *only* config this test relies on for bootstrap parameters: a
    // `Host *` catch-all, not a block matching `brand-new-host` specifically
    // — proving the default applies to a host nobody configured by name.
    let ssh_dir = home.join(".ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    let remote_path = remote_home.join("deployed-isekai-pipe");
    std::fs::write(
        ssh_dir.join("config"),
        format!(
            "Host *\n    #@isekai bootstrap-relay addr=203.0.113.10:443 sni=relay.example.com\n    #@isekai remote-path {}\n",
            remote_path.display()
        ),
    )
    .unwrap();

    // `isekai-ssh login`'s saved token — the one manual, host-independent
    // prerequisite this story keeps (see this file's module docs).
    let token_path = home.join(".config").join("isekai-ssh").join("token.json");
    isekai_auth::FileTokenProvider::new(token_path).save_relay_jwt("relay-jwt-from-login-store").unwrap();

    // The "release asset": a stand-in shell script identical in spirit to
    // every other e2e test's fake isekai-helper (`wrapper_auto_bootstrap_e2e.rs`'s
    // `VALID_BOOTSTRAP_REPORT_JSON`), served over HTTP instead of passed via
    // `--isekai-helper-binary`.
    let helper_script = format!("#!/bin/sh\necho '{VALID_BOOTSTRAP_REPORT_JSON_RELAYED}'\n");
    let arch = this_machines_normalized_arch();
    let asset_name = format!("isekai-pipe-{arch}-unknown-linux-musl");
    let mut routes = std::collections::HashMap::new();
    routes.insert(format!("/cuzic/isekai-terminal/releases/latest/download/{asset_name}"), helper_script.into_bytes());
    let mock_release_addr = spawn_mock_release_server(routes);

    let helper_cache_dir = tmp.path().join("helper-cache");

    let mut child = TokioCommand::new(isekai_ssh_bin_path())
        .arg("brand-new-host")
        .env("HOME", &home)
        .env("PATH", &path_env)
        .env("ISEKAI_SSH_HELPER_RELEASE_BASE_URL", format!("http://{mock_release_addr}"))
        .env("ISEKAI_SSH_HELPER_CACHE_DIR", &helper_cache_dir)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh");

    child.stdin.take().unwrap().write_all(b"y\n").await.unwrap();

    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut saw_registered = false;
    let mut stderr_text = String::new();
    for _ in 0..200 {
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_secs(20), stderr.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                eprint!("[isekai-ssh stderr] {line}");
                stderr_text.push_str(&line);
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

    assert!(saw_registered, "expected wrapper stderr to report profile registration; stderr so far:\n{stderr_text}");
    assert!(helper_cache_dir.exists(), "the auto-downloaded helper binary should have been cached");

    let profile = isekai_pipe_core::load_persistent_profile(&profiles_dir_under(&home), "brand-new-host:22")
        .unwrap()
        .expect("expected a profile for brand-new-host:22");
    let legacy_relay = profile.legacy_relay_transport.as_ref().expect("expected a cached relay transport");
    assert_eq!(legacy_relay.helper_addr, "198.51.100.99:45900", "should have registered the *relayed* candidate, not a direct one");
}
