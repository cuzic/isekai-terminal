//! End-to-end coverage for `CertificateFile` (OpenSSH certificate)
//! authentication on the Windows-native (russh) connect path
//! (`native/private_key.rs::resolve_certificate_file`/
//! `read_credential_with_certificate`, `native/connect.rs`'s candidate loop).
//! `russh-stream-session`'s own unit tests already pin down
//! `authenticate_openssh_cert`'s wire behavior against a bare
//! `client::Handle`; this file closes the gap those can't reach: a real
//! `isekai-ssh.exe` process, driven end to end through host resolution,
//! identity-file discovery (the default `<key>-cert.pub` sibling
//! convention), and the mux/holder dispatch every Windows connection goes
//! through (`main.rs`'s `#[cfg(windows)]` arm) — down to a real shell
//! command actually executing on the mock server.
//!
//! **Scope note (see the e2e test plan this file was written against)**:
//! keyboard-interactive and passphrase-protected-identity authentication are
//! deliberately NOT covered here. Both require a live console prompt
//! round-trip, and on Windows the real authentication for *any* new
//! destination happens inside a detached mux holder process
//! (`native/mux/mod.rs`'s `dispatch`, `native/connect.rs::run_authenticated_session`)
//! which forces `silent = true` unconditionally
//! (`silent = silent || owner_hook.is_some()`) because a detached process has
//! no console to prompt on — regardless of whether the prompt would have had
//! `echo` on or off. There is no realistic way to drive an interactive
//! prompt through a real subprocess e2e without deliberately breaking the
//! mux holder spawn to force the rare foreground-fallback path, which would
//! be testing that fallback, not keyboard-interactive auth itself.
//! `CertificateFile` has no such problem: it's non-interactive (no console
//! needed), so it authenticates the same way whether the holder is prompting
//! or not, and this file exercises it through the real holder-dispatch path.
//!
//! Mirrors `mux_holder_windows_e2e.rs`'s harness (in-process `russh::server`
//! mock, real `isekai-ssh.exe`/`isekai-pipe.exe` subprocesses, the
//! `"ready\n"`-banner synchronization strategy) and duplicates its
//! boilerplate per this crate's self-contained-test-file convention rather
//! than sharing it. The certificate-minting helpers
//! (`write_signed_certificate`, `CertificateServer`) mirror
//! `russh-stream-session/src/lib.rs`'s own test module of the same shape.
#![cfg(windows)]

use std::io::BufRead as StdBufRead;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use isekai_protocol::handshake::HandshakeJson;
use isekai_trust::{HelperTrust, UpdatePolicy};
use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
use russh::{Channel as RusshChannel, CryptoVec};
use russh_keys::ssh_key::certificate::{Builder as CertBuilder, CertType};
use russh_keys::ssh_key::private::Ed25519Keypair;
use russh_keys::ssh_key::Certificate;
use russh_keys::{PrivateKey, PublicKey};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener as TokioTcpListener;
use tokio::process::{Child, Command as TokioCommand};

fn isekai_ssh_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-ssh"))
}

/// Duplicated from `mux_holder_windows_e2e.rs::isekai_pipe_bin_path` per this
/// crate's self-contained-test-file convention.
fn isekai_pipe_bin_path() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let is_release = path.file_name().map(|n| n == "release").unwrap_or(false);
    path.push("isekai-pipe.exe");

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

/// Mints a subject ed25519 keypair and an OpenSSH user certificate for it,
/// signed by a separate CA keypair — same shape as
/// `russh-stream-session/src/lib.rs`'s own `write_signed_certificate` test
/// helper (deliberately duplicated, not shared, per this crate's convention).
/// Returns the subject's OpenSSH-PEM private key bytes, the certificate's
/// OpenSSH-wire-format bytes, and the CA's public key (for the server's trust
/// check).
fn write_signed_certificate(seed: u8, ca_seed: u8, principal: &str) -> (Vec<u8>, Vec<u8>, PublicKey) {
    let ca_key = PrivateKey::from(Ed25519Keypair::from_seed(&[ca_seed; 32]));
    let subject_key = PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]));
    let subject_pem = subject_key.to_openssh(Default::default()).unwrap().as_bytes().to_vec();

    let mut builder = CertBuilder::new_with_random_nonce(&mut rand::rngs::OsRng, subject_key.public_key().key_data().clone(), 0, u32::MAX as u64)
        .expect("build certificate builder");
    builder.cert_type(CertType::User).unwrap();
    builder.valid_principal(principal).unwrap();
    let cert = builder.sign(&ca_key).expect("sign certificate with CA key");
    let cert_pem = cert.to_openssh().unwrap().into_bytes();

    (subject_pem, cert_pem, ca_key.public_key().clone())
}

// ---------------------------------------------------------------------
// Mock sshd: accepts only an OpenSSH certificate signed by `trusted_ca`
// (rejects plain publickey auth entirely, so this test can only pass if the
// native path actually discovered and presented the certificate — not just
// fallen back to the bare key). Mirrors `mux_holder_windows_e2e.rs`'s
// `FakeShellServer`/`FakeShellHandler` shape.
// ---------------------------------------------------------------------

#[derive(Clone)]
struct CertOnlyShellServer {
    trusted_ca: PublicKey,
    connection_count: Arc<AtomicUsize>,
}

impl server::Server for CertOnlyShellServer {
    type Handler = CertOnlyShellHandler;
    fn new_client(&mut self, _: Option<SocketAddr>) -> CertOnlyShellHandler {
        self.connection_count.fetch_add(1, Ordering::SeqCst);
        CertOnlyShellHandler { trusted_ca: self.trusted_ca.clone() }
    }
}

#[derive(Clone)]
struct CertOnlyShellHandler {
    trusted_ca: PublicKey,
}

#[async_trait::async_trait]
impl server::Handler for CertOnlyShellHandler {
    type Error = russh::Error;

    async fn auth_publickey(&mut self, _user: &str, _public_key: &PublicKey) -> Result<Auth, Self::Error> {
        // Deliberately reject plain publickey auth so this test can only
        // succeed via `auth_openssh_certificate` below.
        Ok(Auth::Reject { proceed_with_methods: None })
    }

    async fn auth_openssh_certificate(&mut self, _user: &str, certificate: &Certificate) -> Result<Auth, Self::Error> {
        Ok(if certificate.signature_key() == self.trusted_ca.key_data() { Auth::Accept } else { Auth::Reject { proceed_with_methods: None } })
    }

    async fn channel_open_session(&mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn shell_request(&mut self, channel: russh::ChannelId, session: &mut ServerSession) -> Result<(), Self::Error> {
        session.data(channel, CryptoVec::from(b"ready\n".to_vec()))?;
        Ok(())
    }
}

async fn spawn_cert_only_ssh_server(trusted_ca: PublicKey, connection_count: Arc<AtomicUsize>) -> (SocketAddr, String) {
    let keypair = Ed25519Keypair::from_seed(&[97u8; 32]);
    let host_key = PrivateKey::from(keypair);
    let fingerprint = host_key.public_key().fingerprint(russh_keys::HashAlg::Sha256).to_string();
    let config = std::sync::Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
    let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut sh = CertOnlyShellServer { trusted_ca, connection_count };
    tokio::spawn(async move {
        use server::Server as _;
        let _ = sh.run_on_socket(config, &listener).await;
    });
    (addr, fingerprint)
}

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

fn profiles_dir_under(home: &std::path::Path) -> PathBuf {
    home.join(".local").join("state").join("isekai").join("profiles")
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
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read handshake line from isekai-pipe serve stdout");
    let handshake = isekai_protocol::handshake::decode_handshake_json(line.trim().as_bytes()).expect("failed to parse/validate handshake JSON");

    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(stderr);
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

fn register_correct_profile(profiles_dir: &std::path::Path, key: &str, helper_addr: SocketAddr, cert_sha256_hex: &str, session_secret_b64: &str) {
    let trust = HelperTrust {
        identity_pubkey: cert_sha256_hex.to_string(),
        trusted_helper_sha256: "a".repeat(64),
        trusted_helper_version: "test".to_string(),
        update_policy: UpdatePolicy::ExactDigestOnly,
        release_channel: None,
        last_via: None,
        trusted_at: "2026-01-01T00:00:00Z".to_string(),
        last_seen_at: "2026-01-01T00:00:00Z".to_string(),
        cached_relay_addr: helper_addr.to_string(),
        cached_cert_sha256: cert_sha256_hex.to_string(),
        cached_session_secret: session_secret_b64.to_string(),
        cached_stun_observed_addr: None,
    };
    let profile = isekai_pipe_core::PersistentProfile::migrate_legacy_helper_trust(key, &trust);
    isekai_pipe_core::write_persistent_profile(profiles_dir, &profile).unwrap();
}

/// Polls `child`'s stdout for the mock sshd's `"ready\n"` banner, or panics
/// past `timeout` — same synchronization strategy as
/// `mux_holder_windows_e2e.rs::wait_for_ready` (duplicated, not shared).
async fn wait_for_ready(child: &mut Child, timeout: Duration, label: &str) {
    let mut stderr = child.stderr.take().expect("stderr was piped");
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut seen = Vec::new();
    let mut chunk = [0u8; 4096];
    let result = tokio::time::timeout(timeout, async {
        loop {
            let n = stdout.read(&mut chunk).await.expect("reading tab stdout should not error");
            assert!(n > 0, "{label}: tab's stdout closed before the \"ready\" banner ever appeared, saw {:?}", String::from_utf8_lossy(&seen));
            seen.extend_from_slice(&chunk[..n]);
            if seen.windows(5).any(|w| w == b"ready") {
                return;
            }
        }
    })
    .await;
    assert!(result.is_ok(), "{label}: timed out waiting for the \"ready\" banner, saw so far: {:?}", String::from_utf8_lossy(&seen));
    child.stdout = Some(stdout);
}

/// A destination whose mock sshd only accepts an OpenSSH certificate signed
/// by a specific CA — set up with the subject key at
/// `<home>/.ssh/client_id_ed25519` and its certificate at the `ssh(1)`
/// default-convention sibling path `client_id_ed25519-cert.pub`
/// (`native/private_key.rs::resolve_certificate_file`'s default-discovery
/// path), so a plain `isekai-ssh <alias>` run — with no explicit
/// `CertificateFile` directive — must discover and present it to authenticate
/// at all.
#[tokio::test(flavor = "multi_thread")]
async fn certificate_file_authenticates_over_the_native_windows_path() {
    assert!(
        std::process::Command::new("cmd").arg("/c").arg("ver").status().map(|s| s.success()).unwrap_or(false),
        "sanity check that this test process can spawn a child at all"
    );

    let (subject_pem, cert_pem, ca_public) = write_signed_certificate(60, 160, "tester");
    let connection_count = Arc::new(AtomicUsize::new(0));
    let (mock_sshd_addr, mock_sshd_fingerprint) = spawn_cert_only_ssh_server(ca_public, connection_count.clone()).await;

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("client-home");
    let ssh_dir = home.join(".ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    std::fs::write(ssh_dir.join("client_id_ed25519"), &subject_pem).unwrap();
    std::fs::write(ssh_dir.join("client_id_ed25519-cert.pub"), &cert_pem).unwrap();

    seed_ssh_host_key_trust(&home, &format!("127.0.0.1:{}", mock_sshd_addr.port()), &mock_sshd_fingerprint);

    let alias = "native-auth-e2e-cert-file";
    let host_block = format!(
        "Host {alias}\n\
         \x20\x20\x20\x20HostName 127.0.0.1\n\
         \x20\x20\x20\x20Port {port}\n\
         \x20\x20\x20\x20User tester\n\
         \x20\x20\x20\x20IdentityFile {key}\n\
         \x20\x20\x20\x20IdentitiesOnly yes\n",
        port = mock_sshd_addr.port(),
        key = ssh_dir.join("client_id_ed25519").display(),
    );
    std::fs::write(ssh_dir.join("config"), &host_block).unwrap();

    let helper = spawn_real_helper(mock_sshd_addr);
    let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap()).parse().unwrap();
    let cert_sha256_hex = helper.handshake.cert_sha256().to_string();
    let profile_key = isekai_trust::normalize_host_port(alias).unwrap();
    register_correct_profile(&profiles_dir_under(&home), &profile_key, helper_addr, &cert_sha256_hex, &helper.handshake.session_secret);

    let runtime_dir = tmp.path().join("runtime");
    std::fs::create_dir_all(&runtime_dir).unwrap();

    let mut tab = TokioCommand::new(isekai_ssh_bin_path())
        .arg("--isekai-pipe-path")
        .arg(isekai_pipe_bin_path())
        .arg(alias)
        .env("HOME", &home)
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&home))
        .env("ISEKAI_PIPE_LOG_FILE", home.join("isekai-ssh-verbose.log"))
        .env("ISEKAI_PIPE_RUNTIME_DIR", &runtime_dir)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn isekai-ssh");

    wait_for_ready(&mut tab, Duration::from_secs(30), "cert-auth tab").await;
    assert_eq!(connection_count.load(Ordering::SeqCst), 1, "the mock sshd (which rejects plain publickey auth) must have accepted exactly one certificate-authenticated connection");

    let _ = tab.start_kill();
    let _ = helper.child.kill();
}
