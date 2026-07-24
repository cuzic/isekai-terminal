//! End-to-end tests for `RusshBackend` against an in-process mock SSH
//! server, mirroring `openssh_e2e.rs`'s `FakeShellServer` pattern (this
//! project's `tests/*_e2e.rs` self-containment convention — see that file's
//! own module docs, and `isekai-ssh-e2e-test-self-containment-convention` —
//! duplicated here rather than shared, deliberately).
//!
//! Unlike `openssh_e2e.rs`, this file never shells out to a real `ssh(1)`/
//! `ssh-keygen` binary at all: `RusshBackend` connects via `russh` directly,
//! and the test keypair is generated in-process via `russh_keys::ssh_key`.
//! So, unlike `openssh_e2e.rs`, these tests never skip themselves for a
//! missing `ssh(1)`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio as StdStdio;

use isekai_bootstrap::{launch_fingerprint, BootstrapBackend, HostSpec, JumpSpec, LaunchSpec, RelayLaunchSpec, RusshBackend};
use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
use russh::{Channel as RusshChannel, ChannelId, CryptoVec};
use russh_keys::ssh_key::private::Ed25519Keypair;
use russh_keys::ssh_key::LineEnding;
use russh_keys::{PrivateKey, PublicKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener as TokioTcpListener;
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc;

/// Generates a fresh in-process ed25519 keypair (no `ssh-keygen` needed —
/// unlike `openssh_e2e.rs`, this crate's `RusshBackend` client never shells
/// out to a real `ssh(1)`, so there's no reason to mirror what a real user's
/// key file looks like beyond "a valid OpenSSH-format private key `russh`
/// can parse"). Returns (private key file path, loaded public key).
fn generate_client_keypair(dir: &Path) -> (PathBuf, PublicKey) {
    generate_named_client_keypair(dir, "client_id_ed25519")
}

/// Like [`generate_client_keypair`] but with a caller-chosen filename, so a
/// test can write two distinct identities into the same dir without the
/// second overwriting the first.
fn generate_named_client_keypair(dir: &Path, filename: &str) -> (PathBuf, PublicKey) {
    let private_key = PrivateKey::random(&mut rand_core_from_rand08(), russh_keys::ssh_key::Algorithm::Ed25519)
        .expect("generating a random ed25519 key must not fail");
    let pem = private_key.to_openssh(LineEnding::LF).expect("encoding a freshly generated key must not fail");
    let key_path = dir.join(filename);
    std::fs::write(&key_path, pem.as_bytes()).unwrap();
    (key_path, private_key.public_key().clone())
}

/// `ssh_key::PrivateKey::random` wants a `rand_core` (0.6-generation) RNG;
/// this workspace's `rand` dependency is the matching 0.8 line, whose
/// `rand::rngs::OsRng` already implements that trait — no extra dependency
/// needed, just naming the type this bluntly to make the version bridge
/// obvious at the call site above.
fn rand_core_from_rand08() -> rand::rngs::OsRng {
    rand::rngs::OsRng
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
/// directory. Verbatim copy of `openssh_e2e.rs`'s `FakeShellServer` (see
/// this file's module docs for why it's duplicated, not shared): this
/// server-side logic is entirely transport-agnostic — it doesn't care
/// whether the connecting client is a real `ssh(1)` or `RusshBackend`.
#[derive(Clone)]
struct FakeShellServer {
    home: PathBuf,
    accepted_client_key: PublicKey,
}

impl server::Server for FakeShellServer {
    type Handler = FakeShellHandler;
    fn new_client(&mut self, _: Option<SocketAddr>) -> FakeShellHandler {
        FakeShellHandler { home: self.home.clone(), accepted_client_key: self.accepted_client_key.clone(), stdin_senders: HashMap::new() }
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
        &mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn exec_request(&mut self, channel: ChannelId, data: &[u8], session: &mut ServerSession) -> Result<(), Self::Error> {
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
    let keypair = Ed25519Keypair::from_seed(&[42u8; 32]);
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

/// Builds a `RusshBackend` wired to talk to the test server using the
/// generated test identity and a throwaway trust store, always accepting
/// the (never-before-seen, by construction) host key — test-only, mirrors
/// `openssh_e2e.rs::test_backend`'s `-o StrictHostKeyChecking=no`.
fn test_backend(tmp: &Path, key_path: &Path) -> RusshBackend {
    RusshBackend::new()
        .expect("RusshBackend::new should succeed (only fails if the real trust store path can't be determined)")
        .with_store_path(tmp.join("known_ssh_hosts.toml"))
        .with_confirm_new_host(std::sync::Arc::new(|_fingerprint| true))
        .with_identity_file(key_path.to_path_buf())
}

fn dummy_relay_spec() -> RelayLaunchSpec {
    RelayLaunchSpec {
        relay_addr: "127.0.0.1:1".parse().unwrap(),
        relay_sni: "relay.isekai-ssh.test".to_string(),
        relay_jwt: "test-jwt-token".to_string(),
        relay_transport: isekai_bootstrap::RelayTransportKind::Udp,
        idle_lifetime_secs: 2_592_000,
        remote_log_level: "info".to_string(),
        resume_window_secs: 864_000,
    }
}

const VALID_BOOTSTRAP_REPORT_JSON: &str = r#"{"v":2,"session_id":"00000000000000000000000000000000","bootstrap_attempt_id":"11111111111111111111111111111111","handshake":{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}},"candidates":[{"kind":"direct-by-bootstrap-host","port":45231,"source":"bootstrap-ssh"}]}}"#;

#[tokio::test(flavor = "multi_thread")]
async fn install_and_start_gets_a_real_handshake_over_russh() {
    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home, client_pubkey).await;

    let fake_helper_script = format!("#!/bin/sh\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\n");

    let backend = test_backend(tmp.path(), &key_path);
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

#[tokio::test(flavor = "multi_thread")]
async fn install_and_start_reuses_an_already_running_helper_on_a_second_call() {
    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home, client_pubkey).await;
    // A real (long-running) helper stand-in: writes its own handshake once,
    // then sleeps, matching real `isekai-pipe serve`'s "print handshake,
    // then keep running" shape closely enough for the reuse-detection path
    // (still-alive pid + matching sha256 + matching fingerprint) to kick in
    // on the second `install_and_start` call.
    let fake_helper_script = format!("#!/bin/sh\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\nsleep 30\n");

    let backend = test_backend(tmp.path(), &key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");
    let launch = LaunchSpec::Relay(dummy_relay_spec());

    let first = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(&target, &[], fake_helper_script.as_bytes(), &launch, None, &[]),
    )
    .await
    .expect("first install_and_start should not hang")
    .expect("first install_and_start should succeed");

    let second = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(&target, &[], fake_helper_script.as_bytes(), &launch, None, &[]),
    )
    .await
    .expect("second install_and_start should not hang")
    .expect("second install_and_start should succeed (reuse path)");

    assert_eq!(first.handshake, second.handshake, "the second call should reuse the still-running helper, not relaunch it");
}

/// `pid_file_path`/`is_pid_alive`/`kill_if_recorded`: verbatim copies of
/// `openssh_e2e.rs`'s own helpers of the same name (this file's module docs
/// explain why duplicated rather than shared), scoped by `home` (this file's
/// mock server pins `HOME` to it, same as `openssh_e2e.rs`'s `FakeShellServer`)
/// and `crate::reuse::launch_fingerprint`'s output.
fn pid_file_path(home: &Path, fingerprint: &str) -> PathBuf {
    home.join(format!(".local/bin/isekai-pipe.{fingerprint}.pid"))
}

fn is_pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(StdStdio::null())
        .stderr(StdStdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn kill_if_recorded(home: &Path, fingerprint: &str) {
    if let Ok(pid_str) = std::fs::read_to_string(pid_file_path(home, fingerprint)) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            let _ = std::process::Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .stdout(StdStdio::null())
                .stderr(StdStdio::null())
                .status();
        }
    }
}

/// `RusshBackend` counterpart to `openssh_e2e.rs`'s
/// `install_and_start_redeploys_when_the_alive_helpers_binary_is_stale` — the
/// two backends generate byte-identical reuse-check shell logic
/// (`openssh.rs`/`russh_backend.rs`), but nothing enforced that outside of
/// manual inspection; this closes that gap for the `RusshBackend` side too.
///
/// Unlike `openssh_e2e.rs`'s ELF-plus-a-trailing-byte trick, the two script
/// variants here differ by an actual extra line (a `# v2` comment) — a shell
/// script's `/proc/<pid>/exe` resolves to the interpreter either way, so
/// there's no ELF-identity subtlety to preserve; only the sha256 needs to
/// differ. Both variants append to `invocations_log` on every real launch
/// (unlike `install_and_start_reuses_an_already_running_helper_on_a_second_call`
/// above, whose script only echoes a fixed JSON literal — reuse vs. relaunch
/// would produce byte-identical handshakes either way, so that test alone
/// can't distinguish them; this test needs to, so it tracks real launches
/// and pids directly instead).
#[tokio::test(flavor = "multi_thread")]
async fn install_and_start_redeploys_when_the_alive_helpers_binary_is_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home.clone(), client_pubkey).await;
    let backend = test_backend(tmp.path(), &key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");
    let launch = LaunchSpec::Relay(dummy_relay_spec());
    let fingerprint = launch_fingerprint(&launch);

    let invocations_log = home.join("fake-helper-invocations.log");
    let old_script = format!(
        "#!/bin/sh\necho started >> {}\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\nsleep 30\n",
        invocations_log.display()
    );
    let new_script = format!(
        "#!/bin/sh\n# v2 — same behavior, different bytes/sha256\necho started >> {}\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\nsleep 30\n",
        invocations_log.display()
    );
    assert_ne!(old_script, new_script, "the two scripts must actually differ for this test to mean anything");

    let first = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(&target, &[], old_script.as_bytes(), &launch, None, &[]),
    )
    .await
    .expect("first install_and_start should not hang")
    .expect("first install_and_start should succeed");

    let first_pid: u32 = std::fs::read_to_string(pid_file_path(&home, &fingerprint))
        .unwrap()
        .trim()
        .parse()
        .expect("pid file should hold a pid");
    assert!(is_pid_alive(first_pid), "the first deployment's helper should still be running");

    let second = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(&target, &[], new_script.as_bytes(), &launch, None, &[]),
    )
    .await
    .expect("second install_and_start should not hang")
    .expect("second install_and_start should succeed with a fresh launch");

    assert_eq!(
        first.handshake, second.handshake,
        "both script variants report the same fixed handshake JSON regardless of reuse"
    );

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(is_pid_alive(first_pid), "detecting a stale binary must not kill the still-alive old helper");

    let second_pid: u32 = std::fs::read_to_string(pid_file_path(&home, &fingerprint))
        .unwrap()
        .trim()
        .parse()
        .expect("pid file should hold a pid");
    assert_ne!(first_pid, second_pid, "a stale binary must trigger a fresh launch, not a reuse");
    assert!(is_pid_alive(second_pid), "the fresh deployment's helper should be running");

    let invocations = std::fs::read_to_string(&invocations_log).unwrap_or_default();
    assert_eq!(invocations.lines().count(), 2, "a stale binary must force a real launch, not a reuse");

    kill_if_recorded(&home, &fingerprint);
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(first_pid.to_string())
        .stdout(StdStdio::null())
        .stderr(StdStdio::null())
        .status();
}

#[tokio::test(flavor = "multi_thread")]
async fn install_and_start_rejects_a_multi_hop_via_chain() {
    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();
    let server_addr = spawn_fake_ssh_server(home, client_pubkey).await;

    let backend = test_backend(tmp.path(), &key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");
    let via = [JumpSpec::new("bastion-a"), JumpSpec::new("bastion-b")];

    let err = backend
        .install_and_start(&target, &via, b"unused", &LaunchSpec::Relay(dummy_relay_spec()), None, &[])
        .await
        .expect_err("a 2-hop via chain must be rejected, not silently truncated to one hop");
    assert!(matches!(err, isekai_bootstrap::BootstrapError::UnsupportedViaChain { hops: 2 }));
}

#[tokio::test(flavor = "multi_thread")]
async fn detect_remote_arch_normalizes_this_machines_own_uname_m() {
    let tmp = tempfile::tempdir().unwrap();
    let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home, client_pubkey).await;
    let backend = test_backend(tmp.path(), &key_path);
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

#[tokio::test(flavor = "multi_thread")]
async fn install_and_start_fails_with_a_wrong_client_key() {
    let tmp = tempfile::tempdir().unwrap();
    let (_accepted_key_path, accepted_pubkey) = generate_client_keypair(tmp.path());
    let (wrong_key_path, _wrong_pubkey) = generate_client_keypair(tmp.path());
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    let server_addr = spawn_fake_ssh_server(home, accepted_pubkey).await;
    let backend = test_backend(tmp.path(), &wrong_key_path);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(&target, &[], b"unused", &LaunchSpec::Relay(dummy_relay_spec()), None, &[]),
    )
    .await
    .expect("install_and_start should not hang even on auth failure");
    assert!(result.is_err(), "a key the server never accepted must fail authentication, not silently proceed");
}

/// Regression for #20 (RusshBackend's own bootstrap auth had #11's
/// "only the first IdentityFile is ever tried" bug): the backend is given two
/// candidate identities and the server accepts only the *second*. The first
/// (rejected) key must not block the accepted second one — bootstrap must
/// succeed. Before the fix, `resolve_hop` loaded only the first readable key
/// and authentication stopped there.
#[tokio::test(flavor = "multi_thread")]
async fn install_and_start_tries_the_second_identity_when_the_first_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (first_key_path, _first_pub) = generate_named_client_keypair(tmp.path(), "id_first");
    let (second_key_path, second_pub) = generate_named_client_keypair(tmp.path(), "id_second");
    let home = tmp.path().join("remote-home");
    std::fs::create_dir_all(&home).unwrap();

    // The server accepts ONLY the second identity.
    let server_addr = spawn_fake_ssh_server(home, second_pub).await;

    let fake_helper_script = format!("#!/bin/sh\necho '{VALID_BOOTSTRAP_REPORT_JSON}'\n");

    let backend = RusshBackend::new()
        .expect("RusshBackend::new should succeed")
        .with_store_path(tmp.path().join("known_ssh_hosts.toml"))
        .with_confirm_new_host(std::sync::Arc::new(|_fingerprint| true))
        .with_identity_files(vec![first_key_path, second_key_path]);
    let target = HostSpec::new("127.0.0.1").with_port(server_addr.port()).with_user("tester");

    let report = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        backend.install_and_start(&target, &[], fake_helper_script.as_bytes(), &LaunchSpec::Relay(dummy_relay_spec()), None, &[]),
    )
    .await
    .expect("install_and_start should not hang")
    .expect("the second configured identity is accepted, so bootstrap must succeed despite the first being rejected");

    assert_eq!(report.handshake.v, 1);
    assert_eq!(report.handshake.direct_by_bootstrap_host_port(), Some(45231));
}
