//! End-to-end tests for `OpenSshBackend` against a real `ssh(1)` subprocess
//! talking to an in-process mock SSH server (`russh::server`, following the
//! pattern in `rust-core/src/transport.rs`'s `local_forward_e2e_tests` /
//! `proxy_jump_e2e_tests`).
//!
//! Unlike those mocks (which only exercise `russh`'s own client/server
//! wire protocol), the point here is to exercise the *real* `ssh(1)* binary
//! as `OpenSshBackend` actually spawns it, so the mock server's
//! `exec_request` handler genuinely runs the received command string via
//! `sh -c` (with `HOME` pointed at a scratch temp dir), exactly like a real
//! sshd would. The "isekai-helper" binary `OpenSshBackend` uploads and
//! launches is, in these tests, a tiny shell script (valid due to the
//! shebang line) that just echoes canned handshake output — that's enough
//! to exercise upload -> chmod -> setsid launch -> poll -> `cat` end to end
//! without needing a real isekai-helper binary.
//!
//! Skips itself (rather than failing) when no `ssh(1)` binary is available,
//! per `archive/ISEKAI_SSH_DESIGN.md`'s acceptance criteria for this phase.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio as StdStdio;

use isekai_bootstrap::{BootstrapBackend, BootstrapError, HostSpec, LaunchSpec, OpenSshBackend, RelayLaunchSpec};
use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
use russh::{Channel as RusshChannel, ChannelId, CryptoVec};
use russh_keys::ssh_key::private::Ed25519Keypair;
use russh_keys::{PrivateKey, PublicKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener as TokioTcpListener;
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc;

/// `ssh(1)` is a hard requirement for this crate's e2e tests but isn't
/// guaranteed to exist in every sandboxed dev/CI environment — skip cleanly
/// instead of failing when it's missing.
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

/// Generates a fresh ed25519 keypair via the system `ssh-keygen(1)` (the
/// exact tool real users use), returning (private key path, loaded public
/// key). Panics (rather than skipping) if `ssh-keygen` itself is missing,
/// since any environment with `ssh(1)` is expected to ship it too.
fn generate_client_keypair(dir: &Path) -> (PathBuf, PublicKey) {
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

async fn read_all<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).await?;
    Ok(buf)
}

/// The "remote host"'s SSH server. Accepts only the one test client pubkey,
/// and runs whatever command the client execs via a real `sh -c` subprocess
/// with `HOME` pinned to a scratch temp dir — standing in for a real sshd +
/// real remote filesystem, without touching the test runner's actual home
/// directory.
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
            stdin_senders: HashMap::new(),
        }
    }
}

struct FakeShellHandler {
    home: PathBuf,
    accepted_client_key: PublicKey,
    stdin_senders: HashMap<ChannelId, mpsc::UnboundedSender<Vec<u8>>>,
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

    async fn channel_open_session(
        &mut self,
        _channel: RusshChannel<ServerMsg>,
        _session: &mut ServerSession,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        let handle = session.handle();
        let home = self.home.clone();

        let mut child = TokioCommand::new("sh")
            .arg("-c")
            .arg(&command)
            .env("HOME", &home)
            .stdin(StdStdio::piped())
            .stdout(StdStdio::piped())
            .stderr(StdStdio::piped())
            .spawn()
            .expect("mock server failed to spawn sh -c for exec_request");

        let mut child_stdin = child.stdin.take().expect("stdin piped");
        let mut child_stdout = child.stdout.take().expect("stdout piped");
        let mut child_stderr = child.stderr.take().expect("stderr piped");

        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        self.stdin_senders.insert(channel, tx);

        // Forward client -> server channel data (the base64 payload for the
        // upload command) into the real child process's stdin.
        tokio::spawn(async move {
            while let Some(chunk) = rx.recv().await {
                if child_stdin.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            let _ = child_stdin.shutdown().await;
        });

        // Drain the child's stdout/stderr, then report exit status/EOF/close
        // back to the client — mirroring what a real sshd does once the
        // remote command finishes.
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
        // Dropping the sender ends the forwarding task's `while let` loop,
        // which then shuts down the child's stdin — the real-sshd-equivalent
        // of "client sent EOF, so close stdin".
        self.stdin_senders.remove(&channel);
        Ok(())
    }
}

async fn spawn_fake_ssh_server(home: PathBuf, accepted_client_key: PublicKey) -> SocketAddr {
    let keypair = Ed25519Keypair::from_seed(&[42u8; 32]);
    let host_key = PrivateKey::from(keypair);
    let config = std::sync::Arc::new(server::Config {
        keys: vec![host_key],
        ..Default::default()
    });
    let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut sh = FakeShellServer { home, accepted_client_key };
    tokio::spawn(async move {
        use server::Server as _;
        let _ = sh.run_on_socket(config, &listener).await;
    });
    addr
}

/// Builds an `OpenSshBackend` wired to talk to `server_addr` using the
/// generated test identity, bypassing host-key verification (test-only —
/// see the crate's module docs for why production code must never do this).
fn test_backend(key_path: &Path) -> OpenSshBackend {
    OpenSshBackend::new().with_extra_ssh_args(vec![
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=/dev/null".to_string(),
        "-o".to_string(),
        "IdentitiesOnly=yes".to_string(),
        "-o".to_string(),
        format!("IdentityFile={}", key_path.display()),
    ])
}

fn dummy_relay_spec() -> RelayLaunchSpec {
    RelayLaunchSpec {
        relay_addr: "127.0.0.1:1".parse().unwrap(),
        relay_sni: "relay.isekai-ssh.test".to_string(),
        relay_jwt: "test-jwt-token".to_string(),
        relay_transport: isekai_bootstrap::RelayTransportKind::Udp,
        idle_lifetime_secs: 2_592_000,
        remote_log_level: "info".to_string(),
    }
}

/// `#20a-4`: every real `OpenSshBackend` launch now sends a
/// `BootstrapRequestV2` over stdin, so a compliant `isekai-pipe serve`
/// always echoes back a `BootstrapReportV2` envelope (never a bare
/// `HandshakeJson`) on stdout. `session_id`/`bootstrap_attempt_id` here are
/// arbitrary valid hex — these tests don't correlate them against the
/// request the fake script actually received.
const VALID_BOOTSTRAP_REPORT_JSON: &str = r#"{"v":2,"session_id":"00000000000000000000000000000000","bootstrap_attempt_id":"11111111111111111111111111111111","handshake":{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}},"candidates":[{"kind":"direct-by-bootstrap-host","port":45231,"source":"bootstrap-ssh"}]}}"#;

#[tokio::test]
async fn install_and_start_gets_a_real_handshake_over_a_real_ssh_subprocess() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home, client_pubkey).await;

    // Stands in for the isekai-helper binary: a shell script that ignores
    // its args and just emits exactly one line of valid handshake JSON,
    // exactly like the real isekai-helper does on stdout at startup.
    let fake_helper_script = format!("#!/bin/sh\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\n");

    let backend = test_backend(&key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");

    let report = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(&target, &[], fake_helper_script.as_bytes(), &LaunchSpec::Relay(dummy_relay_spec()), None, &[]),
    )
    .await
    .expect("install_and_start should not hang")
    .expect("install_and_start should succeed against the mock server");

    assert_eq!(report.handshake.v, 1);
    assert_eq!(report.handshake.direct_by_bootstrap_host_port(), Some(45231));
    assert_eq!(report.handshake.cert_sha256().len(), 64);
}

/// `detect_remote_arch` over a real `ssh(1)` subprocess against the mock
/// server (which genuinely execs `uname -m` via `sh -c` on this test
/// machine, per this file's module docs) — the "remote" arch is therefore
/// this test machine's own, normalized the same way
/// `std::env::consts::ARCH` would be. Not pinned to `"x86_64"` so this test
/// stays meaningful on aarch64 runners too.
#[tokio::test(flavor = "multi_thread")]
async fn detect_remote_arch_normalizes_this_machines_own_uname_m() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home, client_pubkey).await;
    let backend = test_backend(&key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");

    let arch = tokio::time::timeout(std::time::Duration::from_secs(20), backend.detect_remote_arch(&target, &[]))
        .await
        .expect("detect_remote_arch should not hang")
        .expect("detect_remote_arch should succeed against the mock server");

    let expected = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => panic!("this test machine's own arch {other:?} isn't one this test can assert against"),
    };
    assert_eq!(arch, expected);
}

/// `RelayLaunchSpec::idle_lifetime_secs` must actually reach the launched
/// `isekai-helper` process's argv as `--max-idle-lifetime <SECS>` — this is
/// the fix for the `archive/ISEKAI_SSH_DESIGN.md` "引き続き未決の項目" gap where a
/// helper deployed via `isekai-ssh init` would inherit `isekai-helper`'s own
/// short default (600s, tuned for `isekai-terminal-core`'s per-session bootstrap) and
/// self-exit long before a `connect` invocation hours/days later could reach
/// it again.
#[tokio::test]
async fn install_and_start_passes_idle_lifetime_to_the_launched_helper() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home.clone(), client_pubkey).await;

    // Same stand-in as the other tests, but it also records its own argv to
    // a side file so this test can inspect exactly what `OpenSshBackend`
    // launched it with (its stdout must stay pure JSON, matching the real
    // isekai-helper's contract, so argv can't be echoed there instead).
    let argv_log = home.join("argv.log");
    let fake_helper_script =
        format!("#!/bin/sh\necho \"$@\" > {}\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\n", argv_log.display());

    let backend = test_backend(&key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");
    let relay = RelayLaunchSpec {
        relay_addr: "127.0.0.1:1".parse().unwrap(),
        relay_sni: "relay.isekai-ssh.test".to_string(),
        relay_jwt: "test-jwt-token".to_string(),
        relay_transport: isekai_bootstrap::RelayTransportKind::Udp,
        idle_lifetime_secs: 2_592_000,
        remote_log_level: "info".to_string(),
    };

    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(&target, &[], fake_helper_script.as_bytes(), &LaunchSpec::Relay(relay), None, &[]),
    )
    .await
    .expect("install_and_start should not hang")
    .expect("install_and_start should succeed against the mock server");

    let argv = std::fs::read_to_string(&argv_log).expect("stand-in script should have recorded its argv");
    assert!(
        argv.contains("--max-idle-lifetime 2592000"),
        "expected the launched isekai-helper's argv to contain '--max-idle-lifetime 2592000', got: {argv:?}"
    );
    // `isekai-pipe serve` (the merged binary, `archive/ISEKAI_PIPE_MIGRATION.md` P5)
    // requires an explicit `serve` subcommand and a `--target`/`--service`,
    // unlike the standalone `isekai-helper` binary this replaced (which
    // defaulted `--target` to `127.0.0.1:22`).
    assert!(argv.starts_with("serve "), "expected argv to start with the 'serve' subcommand, got: {argv:?}");
    assert!(argv.contains("--target 127.0.0.1:22"), "expected argv to contain '--target 127.0.0.1:22', got: {argv:?}");
}

/// `RelayLaunchSpec::relay_transport: Qmux` (`#qmux-leg2`) must add
/// `--relay-transport qmux` to the launched isekai-helper's argv; the
/// default (`Udp`) must add nothing (the flag doesn't even exist on older
/// `isekai-pipe serve` builds, so omitting it entirely — not passing
/// `--relay-transport udp` — keeps backward compatibility with them).
#[tokio::test]
async fn install_and_start_relay_transport_qmux_adds_the_flag() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home.clone(), client_pubkey).await;

    let argv_log = home.join("argv.log");
    let fake_helper_script =
        format!("#!/bin/sh\necho \"$@\" > {}\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\n", argv_log.display());

    let backend = test_backend(&key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");
    let relay = RelayLaunchSpec {
        relay_addr: "127.0.0.1:1".parse().unwrap(),
        relay_sni: "relay.isekai-ssh.test".to_string(),
        relay_jwt: "test-jwt-token".to_string(),
        relay_transport: isekai_bootstrap::RelayTransportKind::Qmux,
        idle_lifetime_secs: 2_592_000,
        remote_log_level: "info".to_string(),
    };

    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(&target, &[], fake_helper_script.as_bytes(), &LaunchSpec::Relay(relay), None, &[]),
    )
    .await
    .expect("install_and_start should not hang")
    .expect("install_and_start should succeed against the mock server");

    let argv = std::fs::read_to_string(&argv_log).expect("stand-in script should have recorded its argv");
    assert!(
        argv.contains("--relay-transport qmux"),
        "expected argv to contain '--relay-transport qmux', got: {argv:?}"
    );
}

/// `LaunchSpec::Direct` (the wrapper's auto-bootstrap path,
/// `archive/ISEKAI_PIPE_MIGRATION.md` P4) must never pass any `--relay*` argument —
/// there is no relay JWT to source in that mode — and must still get the
/// idle lifetime through.
#[tokio::test]
async fn install_and_start_direct_never_passes_relay_args() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home.clone(), client_pubkey).await;

    let argv_log = home.join("argv.log");
    let fake_helper_script =
        format!("#!/bin/sh\necho \"$@\" > {}\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\n", argv_log.display());

    let backend = test_backend(&key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");

    let report = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(
            &target,
            &[],
            fake_helper_script.as_bytes(),
            &LaunchSpec::Direct { idle_lifetime_secs: 86_400, remote_log_level: "info".to_string(), bind_port_range: None },
            None,
            &[],
        ),
    )
    .await
    .expect("install_and_start should not hang")
    .expect("install_and_start should succeed against the mock server");

    assert_eq!(report.handshake.direct_by_bootstrap_host_port(), Some(45231));

    let argv = std::fs::read_to_string(&argv_log).expect("stand-in script should have recorded its argv");
    assert!(
        argv.contains("--max-idle-lifetime 86400"),
        "expected argv to contain '--max-idle-lifetime 86400', got: {argv:?}"
    );
    assert!(!argv.contains("--relay"), "direct mode must never pass --relay*, got argv: {argv:?}");
    assert!(argv.starts_with("serve "), "expected argv to start with the 'serve' subcommand, got: {argv:?}");
    assert!(argv.contains("--target 127.0.0.1:22"), "expected argv to contain '--target 127.0.0.1:22', got: {argv:?}");
}

/// `#@isekai remote-path` (`isekai-ssh/src/wrapper.rs`) must actually change
/// where `OpenSshBackend` uploads and launches the binary from — not just
/// get parsed and silently ignored.
#[tokio::test]
async fn install_and_start_uses_custom_remote_binary_path() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home.clone(), client_pubkey).await;

    let fake_helper_script = format!("#!/bin/sh\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\n");

    let backend = test_backend(&key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");

    // A nested, non-default directory: exercises both the `mkdir -p` of the
    // parent directory and the upload/launch path override together.
    let custom_path = "~/custom/nested/dir/isekai-pipe-custom";

    let report = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(
            &target,
            &[],
            fake_helper_script.as_bytes(),
            &LaunchSpec::Direct { idle_lifetime_secs: 86_400, remote_log_level: "info".to_string(), bind_port_range: None },
            Some(custom_path),
            &[],
        ),
    )
    .await
    .expect("install_and_start should not hang")
    .expect("install_and_start should succeed against the mock server");

    assert_eq!(report.handshake.direct_by_bootstrap_host_port(), Some(45231));

    let uploaded = home.join("custom/nested/dir/isekai-pipe-custom");
    assert!(
        uploaded.exists(),
        "expected the binary to be uploaded at the custom remote path {uploaded:?}"
    );
    // Nothing should have been written to the default install dir instead.
    assert!(!home.join(".local/bin").exists(), "must not fall back to the default install dir");
}

#[tokio::test]
async fn install_and_start_fails_closed_when_stdout_has_extra_lines() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home, client_pubkey).await;

    // Same as above, but the fake helper also prints an extra line — as if
    // isekai-helper (or something on the remote system) polluted stdout
    // with a warning/log line instead of keeping it purely the handshake
    // JSON. `OpenSshBackend` must reject this rather than guess which line
    // is "the real one".
    let fake_helper_script =
        format!("#!/bin/sh\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\necho 'unexpected warning: something else happened'\n");

    let backend = test_backend(&key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(&target, &[], fake_helper_script.as_bytes(), &LaunchSpec::Relay(dummy_relay_spec()), None, &[]),
    )
    .await
    .expect("install_and_start should not hang");

    match result {
        Err(BootstrapError::UnexpectedStdout { extra_lines }) => {
            assert_eq!(extra_lines, 1);
        }
        other => panic!("expected UnexpectedStdout, got: {other:?}"),
    }
}

/// `#20a-2`: the `BootstrapRequestV2` JSON travels intact over the same
/// stdin as `relay_jwt`, length-prefixed and split apart remotely — this
/// proves the split actually produces byte-exact, independently-decodable
/// files rather than corrupting either payload (e.g. off-by-one length
/// arithmetic truncating the JSON or bleeding into the JWT file).
#[tokio::test]
async fn install_and_start_delivers_an_intact_bootstrap_request_alongside_relay_jwt() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home.clone(), client_pubkey).await;

    // Stands in for `isekai-pipe serve --bootstrap-request-file <path>`
    // (`#20a-3`, not implemented yet): captures whatever the remote shell
    // wrote to the request/JWT files it was given as its own argv, so this
    // test can inspect both independently of any real parsing logic.
    let request_copy = home.join("captured-request.json");
    let jwt_copy = home.join("captured-relay-jwt");
    let fake_helper_script = format!(
        "#!/bin/sh\n\
         while [ $# -gt 0 ]; do\n\
           case \"$1\" in\n\
             --bootstrap-request-file) cp \"$2\" {request_copy}; shift 2 ;;\n\
             --relay-jwt-file) cp \"$2\" {jwt_copy}; shift 2 ;;\n\
             *) shift ;;\n\
           esac\n\
         done\n\
         echo '{VALID_BOOTSTRAP_REPORT_JSON}'\n",
        request_copy = request_copy.display(),
        jwt_copy = jwt_copy.display(),
    );

    let backend = test_backend(&key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");
    let relay = dummy_relay_spec();

    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(&target, &[], fake_helper_script.as_bytes(), &LaunchSpec::Relay(relay.clone()), None, &[]),
    )
    .await
    .expect("install_and_start should not hang")
    .expect("install_and_start should succeed against the mock server");

    let captured_request = std::fs::read(&request_copy).expect("bootstrap request file should have been captured");
    let request: isekai_protocol::BootstrapRequestV2 =
        serde_json::from_slice(&captured_request).expect("captured bootstrap request should be valid JSON");
    assert_eq!(request.v, isekai_protocol::BOOTSTRAP_PROTOCOL_V2);
    assert!(request.session_id().is_ok(), "session_id should decode as a valid hex SessionId");
    assert!(request.bootstrap_attempt_id().is_ok(), "bootstrap_attempt_id should decode as a valid hex id");
    assert!(request.client_candidates.is_empty(), "#20a-2 sends no client candidates yet (#20b's job)");

    let captured_jwt = std::fs::read_to_string(&jwt_copy).expect("relay_jwt file should have been captured");
    assert_eq!(captured_jwt, relay.relay_jwt, "relay_jwt must arrive byte-exact despite sharing stdin with the request JSON");
}

/// Minimal mock STUN server (RFC 5389 Binding Request/Response), same shape
/// used throughout this workspace.
async fn spawn_mock_stun_server() -> SocketAddr {
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 512];
        loop {
            let Ok((n, from)) = server.recv_from(&mut buf).await else { break };
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

            let _ = server.send_to(&resp, from).await;
        }
    });
    addr
}

/// `#20b`: when `install_and_start` is given real STUN servers, the
/// `BootstrapRequestV2` it sends must carry a real client candidate learned
/// from querying them, and the remote launch command (`LaunchSpec::Direct`)
/// must pass the first one through as `--stun-server` so the remote side
/// reports its own `server-reflexive` candidate back too.
#[tokio::test]
async fn install_and_start_delivers_real_stun_candidates_when_stun_servers_are_configured() {
    if !ssh_binary_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home.clone(), client_pubkey).await;
    let stun_server = spawn_mock_stun_server().await;

    let request_copy = home.join("captured-request.json");
    let argv_log = home.join("argv.log");
    let fake_helper_script = format!(
        "#!/bin/sh\n\
         echo \"$@\" > {argv_log}\n\
         while [ $# -gt 0 ]; do\n\
           case \"$1\" in\n\
             --bootstrap-request-file) cp \"$2\" {request_copy}; shift 2 ;;\n\
             *) shift ;;\n\
           esac\n\
         done\n\
         echo '{VALID_BOOTSTRAP_REPORT_JSON}'\n",
        request_copy = request_copy.display(),
        argv_log = argv_log.display(),
    );

    let backend = test_backend(&key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");

    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(
            &target,
            &[],
            fake_helper_script.as_bytes(),
            &LaunchSpec::Direct { idle_lifetime_secs: 86_400, remote_log_level: "info".to_string(), bind_port_range: None },
            None,
            &[stun_server],
        ),
    )
    .await
    .expect("install_and_start should not hang")
    .expect("install_and_start should succeed against the mock server");

    let captured_request = std::fs::read(&request_copy).expect("bootstrap request file should have been captured");
    let request: isekai_protocol::BootstrapRequestV2 =
        serde_json::from_slice(&captured_request).expect("captured bootstrap request should be valid JSON");
    assert_eq!(request.client_candidates.len(), 1, "querying one real STUN server should yield one client candidate");
    assert_eq!(request.client_candidates[0].route, "stun-p2p");
    let candidate_addr: SocketAddr =
        request.client_candidates[0].endpoint.parse().expect("candidate endpoint should be a valid socket address");
    assert_eq!(candidate_addr.ip(), std::net::Ipv4Addr::LOCALHOST);

    let argv = std::fs::read_to_string(&argv_log).expect("stand-in script should have recorded its argv");
    assert!(
        argv.contains(&format!("--stun-server {stun_server}")),
        "expected argv to contain '--stun-server {stun_server}', got: {argv:?}"
    );
}
