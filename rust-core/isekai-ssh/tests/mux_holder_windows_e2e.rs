//! End-to-end test for the Windows-native mux/holder path
//! (`native/mux/mod.rs`'s `ControlPersist`-equivalent redesign, Phase 1a/1b):
//! spawns several *real* `isekai-ssh.exe` processes against the same
//! destination and verifies, at the real OS-process/named-pipe level, the
//! claims `native/mux/mod.rs`'s own `InMemoryChannel`-based unit tests can't
//! reach:
//!
//! - a second tab multiplexes onto the first tab's freshly-spawned detached
//!   holder instead of dialing SSH itself;
//! - closing the tab that originally spawned the holder does not kill the
//!   shared connection — a third tab opened afterward still multiplexes
//!   (this is `ControlPersist`'s actual core claim: the master's lifetime is
//!   decoupled from any one tab, not just "several tabs share a connection
//!   while all stay open");
//! - (lower priority) once every tab closes, the holder self-exits after
//!   `owner::IDLE_GRACE` and a subsequent tab spawns a fresh holder.
//!
//! This is deliberately a *different* kind of gap than the 5 existing
//! `ssh_test_shim`-based e2e files in this directory cover: those all drive
//! `wrapper::run`'s single-process auto-bootstrap path (`main.rs` dispatches
//! there on every platform this crate's other e2e files run on); the mux
//! module is reachable only via `main.rs`'s `#[cfg(windows)]` arm
//! (`native::mux::run`/`run_as_holder_entrypoint`), so a real multi-process
//! run of it can only ever execute on Windows. Per this crate's convention
//! (`stdout_cleanliness.rs`'s module docs) this file is self-contained; it
//! duplicates the mock-sshd/real-`isekai-pipe serve` harness pattern from
//! `wrapper_stale_trust_auto_recovery_e2e.rs` rather than sharing it.
//!
//! Unlike every sibling e2e file here, this one is gated `#![cfg(windows)]`
//! for the whole file rather than per-item: the scenario it exercises
//! (multiple `isekai-ssh.exe` processes racing to claim a named pipe) simply
//! doesn't exist on any other platform, so there is no shared-across-platforms
//! test body to gate individual assertions within — on Linux/macOS CI this
//! whole file compiles to an empty test binary ("0 tests, ok"), same as the
//! effect the `test-windows` job's own per-file allowlist already has today.
//!
//! **Synchronization strategy**: rather than feeding synthetic keystrokes (a
//! spawned tab's `RawModeGuard::enable()` opens the real console's `CONIN$`/
//! `CONOUT$` directly — see `native/console.rs`'s module docs — so it works
//! whether or not this test redirects the child's stdin/stdout, but there is
//! no way to inject keystrokes into a *specific* subprocess's console short of
//! owning a real terminal), each test spawns a mock sshd whose `shell_request`
//! handler immediately writes a `"ready\n"` banner on the new channel. That
//! banner is relayed all the way back through the mux protocol to the actual
//! tab process's own (piped) stdout, so polling a tab's stdout for `"ready"`
//! is a reliable "this tab now has a live, relayed remote shell channel"
//! signal — enough to prove/disprove multiplexing via the mock sshd's own
//! connection counter, without needing any stdin content at all.
#![cfg(windows)]

use std::io::BufRead as StdBufRead;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use isekai_protocol::handshake::HandshakeJson;
use isekai_trust::{HelperTrust, UpdatePolicy};
use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
use russh::{Channel as RusshChannel, CryptoVec};
use russh_keys::ssh_key::private::Ed25519Keypair;
use russh_keys::{PrivateKey, PublicKey};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener as TokioTcpListener;
use tokio::process::{Child, Command as TokioCommand};

/// Serializes this file's 3 tests (`cargo test`'s default harness runs
/// `#[test]` functions concurrently). Each test spawns several real
/// `isekai-ssh.exe`/`isekai-pipe.exe` processes and waits on
/// `HOLDER_STARTUP_TIMEOUT` (10s, not overridable — see the module docs on
/// why a real wait beats a test-only knob here too); a real `test-windows`
/// CI failure (2026-07-23, `holder_exits_after_idle_grace_once_all_tabs_close`)
/// showed a *single* tab produce 2 real SSH connections instead of 1 under
/// exactly this concurrency — the detached holder took longer than
/// `HOLDER_STARTUP_TIMEOUT` to claim the channel (three tests' worth of
/// subprocesses competing for CPU on one CI runner), so the spawning tab's
/// own client-side retry gave up and fell back to a direct connect while
/// the slow-but-still-live holder *also* eventually finished its own dial.
/// A `tokio::sync::Mutex` (not `std::sync::Mutex`, which can't be held
/// across `.await` on a multi-thread runtime) held for each test's entire
/// body removes the concurrency rather than trying to out-guess a CI
/// runner's variable load with a bigger timeout.
static TEST_SERIAL_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn isekai_ssh_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-ssh"))
}

/// Locates the sibling `isekai-pipe` binary, building it if missing —
/// duplicated from `wrapper_stale_trust_auto_recovery_e2e.rs::isekai_pipe_bin_path`
/// per this crate's self-contained-test-file convention. Used both for the
/// real `isekai-pipe serve` process standing in for "the already-deployed
/// helper" and (via `--isekai-pipe-path`) as the binary the native connect
/// path spawns internally for `isekai-pipe connect --stdio`.
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

fn ssh_keygen_available() -> bool {
    std::process::Command::new("ssh-keygen")
        .arg("-V")
        .stdin(StdStdio::null())
        .stdout(StdStdio::null())
        .stderr(StdStdio::null())
        .status()
        .map(|s| s.success() || s.code().is_some())
        .unwrap_or(false)
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

// ---------------------------------------------------------------------
// Mock sshd: only `shell_request` (each mux client opens its own SSH
// *channel*, never a new TCP connection) is needed, unlike the other e2e
// files' `exec_request`-based deploy stand-ins. Mirrors
// `native/mux/mod.rs`'s own `EchoShellServer` unit-test double, plus a
// connection counter this file's assertions are actually built on: every
// *tab* gets its own relayed channel and its own "ready\n" banner, but only
// the *holder* ever dials a new TCP connection here — so this counter
// staying at 1 across N tabs is this file's core proof of multiplexing.
// ---------------------------------------------------------------------

#[derive(Clone)]
struct FakeShellServer {
    accepted_client_key: PublicKey,
    connection_count: Arc<AtomicUsize>,
}

impl server::Server for FakeShellServer {
    type Handler = FakeShellHandler;
    fn new_client(&mut self, _: Option<SocketAddr>) -> FakeShellHandler {
        self.connection_count.fetch_add(1, Ordering::SeqCst);
        FakeShellHandler { accepted_client_key: self.accepted_client_key.clone() }
    }
}

#[derive(Clone)]
struct FakeShellHandler {
    accepted_client_key: PublicKey,
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

    async fn shell_request(&mut self, channel: russh::ChannelId, session: &mut ServerSession) -> Result<(), Self::Error> {
        session.data(channel, CryptoVec::from(b"ready\n".to_vec()))?;
        Ok(())
    }
}

/// Returns the mock sshd's address and its host key's SHA256 fingerprint (the
/// same format `isekai_trust::SshHostKeyTrust::fingerprint` stores) — needed
/// to pre-seed `known_ssh_hosts.toml` so the native path's own SSH host-key
/// TOFU prompt (distinct from the app-level bootstrap trust this test also
/// pre-seeds) never fires; see `wrapper_stale_trust_auto_recovery_e2e.rs`'s
/// `spawn_fake_ssh_server` docs for why an unattended e2e run needs this.
async fn spawn_fake_ssh_server(accepted_client_key: PublicKey, connection_count: Arc<AtomicUsize>) -> (SocketAddr, String) {
    let keypair = Ed25519Keypair::from_seed(&[91u8; 32]);
    let host_key = PrivateKey::from(keypair);
    let fingerprint = host_key.public_key().fingerprint(russh_keys::HashAlg::Sha256).to_string();
    let config = std::sync::Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
    let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut sh = FakeShellServer { accepted_client_key, connection_count };
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

// ---------------------------------------------------------------------
// Real `isekai-pipe serve` standing in for "the already-deployed helper" —
// duplicated from `wrapper_stale_trust_auto_recovery_e2e.rs::spawn_real_helper`/
// `HelperProcess`. Unlike that file's stale-trust scenario, this test points
// `--target` directly at the mock sshd from the start, so the profile
// registered below is correct from the first connect (no redeploy).
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
    cmd.arg("serve").arg("--target").arg(target_addr.to_string()).arg("--bind").arg("127.0.0.1:0").stdout(StdStdio::piped()).stderr(StdStdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn isekai-pipe serve");
    let stdout = child.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read handshake line from isekai-pipe serve stdout");
    let handshake = isekai_protocol::handshake::decode_handshake_json(line.trim().as_bytes()).expect("failed to parse/validate handshake JSON");

    // Drain stderr on a background thread so the child never blocks on a
    // full pipe buffer — same pattern as the sibling file this is duplicated
    // from.
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

// ---------------------------------------------------------------------
// Test fixture: one already-trusted, already-deployed destination shared by
// every tab a test spawns against it.
// ---------------------------------------------------------------------

struct MuxFixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    isekai_pipe_path: PathBuf,
    runtime_dir: PathBuf,
    connection_count: Arc<AtomicUsize>,
    _helper: HelperProcess,
    alias: String,
    next_tab_id: AtomicU32,
}

impl MuxFixture {
    async fn new(alias: &str) -> Self {
        assert!(ssh_keygen_available(), "ssh-keygen(1) not available in this environment (expected on windows-latest)");

        let tmp = tempfile::tempdir().unwrap();
        let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
        let connection_count = Arc::new(AtomicUsize::new(0));
        let (mock_sshd_addr, mock_sshd_fingerprint) = spawn_fake_ssh_server(client_pubkey, connection_count.clone()).await;

        let home = tmp.path().join("client-home");
        std::fs::create_dir_all(&home).unwrap();
        seed_ssh_host_key_trust(&home, &format!("127.0.0.1:{}", mock_sshd_addr.port()), &mock_sshd_fingerprint);

        let host_block = format!(
            "Host {alias}\n\
             \x20\x20\x20\x20HostName 127.0.0.1\n\
             \x20\x20\x20\x20Port {port}\n\
             \x20\x20\x20\x20User tester\n\
             \x20\x20\x20\x20IdentityFile {key}\n\
             \x20\x20\x20\x20IdentitiesOnly yes\n",
            port = mock_sshd_addr.port(),
            key = key_path.display(),
        );
        let home_ssh_dir = home.join(".ssh");
        std::fs::create_dir_all(&home_ssh_dir).unwrap();
        std::fs::write(home_ssh_dir.join("config"), &host_block).unwrap();

        // The real, already-running "deployed helper" — `--target` points
        // straight at the mock sshd, so the profile registered below is
        // correct from the very first connect attempt (unlike
        // `wrapper_stale_trust_auto_recovery_e2e.rs`, this test isn't
        // exercising the redeploy path).
        let helper = spawn_real_helper(mock_sshd_addr);
        let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap()).parse().unwrap();
        let cert_sha256_hex = helper.handshake.cert_sha256().to_string();
        let key = isekai_trust::normalize_host_port(alias).unwrap();
        register_correct_profile(&profiles_dir_under(&home), &key, helper_addr, &cert_sha256_hex, &helper.handshake.session_secret);

        let runtime_dir = tmp.path().join("runtime");
        std::fs::create_dir_all(&runtime_dir).unwrap();

        Self {
            _tmp: tmp,
            home,
            isekai_pipe_path: isekai_pipe_bin_path(),
            runtime_dir,
            connection_count,
            _helper: helper,
            alias: alias.to_string(),
            next_tab_id: AtomicU32::new(0),
        }
    }

    /// Spawns one real `isekai-ssh.exe` "tab" against this fixture's
    /// destination. Every tab shares the same `HOME`/`ISEKAI_PIPE_PROFILES_DIR`/
    /// `ISEKAI_PIPE_RUNTIME_DIR` (so they resolve to the exact same
    /// `native/mux/naming.rs::channel_name`, the whole precondition for
    /// multiplexing to even be possible) but gets its own log file (avoids
    /// concurrent tabs interleaving writes to one file) and its own detached
    /// stdin (`Stdio::null()` — no interactive input this test ever needs to
    /// send; see this file's module docs on why the `"ready\n"` banner alone
    /// is enough to synchronize on).
    fn spawn_tab(&self) -> Child {
        let tab_id = self.next_tab_id.fetch_add(1, Ordering::SeqCst);
        let log_path = self.home.join(format!("isekai-ssh-verbose-tab{tab_id}.log"));
        TokioCommand::new(isekai_ssh_bin_path())
            .arg("--isekai-pipe-path")
            .arg(&self.isekai_pipe_path)
            .arg(&self.alias)
            .env("HOME", &self.home)
            .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(&self.home))
            .env("ISEKAI_PIPE_LOG_FILE", log_path)
            .env("ISEKAI_PIPE_RUNTIME_DIR", &self.runtime_dir)
            .env_remove("RUST_LOG")
            .stdin(StdStdio::null())
            .stdout(StdStdio::piped())
            .stderr(StdStdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn isekai-ssh")
    }
}

/// Polls `child`'s stdout until the mock sshd's `"ready\n"` banner appears
/// (this tab now has a live, relayed remote shell channel — see this file's
/// module docs), or panics past `timeout`. Also drains stderr concurrently
/// (best-effort, discarded) so a chatty child never blocks on a full stderr
/// pipe while this only reads stdout.
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

/// Scenario (a): a second tab opened against an already-multiplexed
/// destination must reuse the first tab's freshly-spawned holder rather than
/// dialing SSH itself — the mock sshd must see exactly one real connection
/// across both tabs.
#[tokio::test(flavor = "multi_thread")]
async fn second_tab_reuses_the_first_tabs_holder_without_a_new_ssh_handshake() {
    let _serial = TEST_SERIAL_GUARD.lock().await;
    let fixture = MuxFixture::new("mux-e2e-second-tab-reuses-holder").await;

    // Tab 1 finds no existing holder for this destination, so it becomes the
    // spawn leader: it spawns a detached holder (a real, separate `isekai-
    // ssh.exe` process on Windows — see `native/mux/holder.rs`'s
    // `DetachedProcessSpawner`) and waits to connect to it as an ordinary
    // client, same as every other tab.
    let mut tab1 = fixture.spawn_tab();
    wait_for_ready(&mut tab1, Duration::from_secs(30), "tab1").await;
    assert_eq!(fixture.connection_count.load(Ordering::SeqCst), 1, "tab1 alone must produce exactly one real SSH connection (via the holder it spawned)");

    // Tab 2 must find that holder already claiming the channel and connect
    // straight to it — no new holder spawn, no new SSH handshake.
    let mut tab2 = fixture.spawn_tab();
    wait_for_ready(&mut tab2, Duration::from_secs(30), "tab2").await;
    assert_eq!(
        fixture.connection_count.load(Ordering::SeqCst),
        1,
        "a second tab against the same destination must multiplex onto the first tab's holder, not open a second SSH connection"
    );

    let _ = tab1.start_kill();
    let _ = tab2.start_kill();
}

/// Scenario (b), `ControlPersist`'s actual core claim: the shared connection
/// must outlive the specific tab that spawned it. Closing tab 1 (the spawn
/// leader) must not tear down the holder — a third tab opened afterward must
/// still multiplex onto it, proving the holder's lifetime is decoupled from
/// any one tab rather than merely "shared while every original tab stays
/// open".
#[tokio::test(flavor = "multi_thread")]
async fn holder_outlives_the_tab_that_spawned_it() {
    let _serial = TEST_SERIAL_GUARD.lock().await;
    let fixture = MuxFixture::new("mux-e2e-holder-outlives-spawner").await;

    let mut tab1 = fixture.spawn_tab();
    wait_for_ready(&mut tab1, Duration::from_secs(30), "tab1").await;

    let mut tab2 = fixture.spawn_tab();
    wait_for_ready(&mut tab2, Duration::from_secs(30), "tab2").await;
    assert_eq!(fixture.connection_count.load(Ordering::SeqCst), 1, "setup: tab1+tab2 must share one SSH connection before tab1 closes");

    // Close tab1 (the process that originally spawned the detached holder).
    // The holder is a separate, detached process — this must not affect it
    // or tab2's still-live relay at all.
    let _ = tab1.start_kill();
    let _ = tab1.wait().await;

    let mut tab3 = fixture.spawn_tab();
    wait_for_ready(&mut tab3, Duration::from_secs(30), "tab3").await;
    assert_eq!(
        fixture.connection_count.load(Ordering::SeqCst),
        1,
        "a third tab opened after the original spawning tab closed must still multiplex onto the (still-alive, detached) holder — this is ControlPersist's core claim"
    );

    let _ = tab2.start_kill();
    let _ = tab3.start_kill();
}

/// Scenario (c) (lower priority per the task): once every tab closes, the
/// holder must self-exit after `owner::IDLE_GRACE` (10s, not test-overridable
/// — see this crate's `.github/workflows/rust-core-test-check.yml` comment on
/// why a real wait here is preferable to adding a test-only knob to
/// production code for a one-time ~15s cost). A subsequent tab must then find
/// no holder to reuse and spawn a fresh one, producing a second real SSH
/// connection.
#[tokio::test(flavor = "multi_thread")]
async fn holder_exits_after_idle_grace_once_all_tabs_close() {
    let _serial = TEST_SERIAL_GUARD.lock().await;
    let fixture = MuxFixture::new("mux-e2e-holder-idle-exit").await;

    let mut tab1 = fixture.spawn_tab();
    wait_for_ready(&mut tab1, Duration::from_secs(30), "tab1").await;
    assert_eq!(fixture.connection_count.load(Ordering::SeqCst), 1, "tab1 alone must produce exactly one real SSH connection (via the holder it spawned)");

    let _ = tab1.start_kill();
    let _ = tab1.wait().await;

    // `owner::IDLE_GRACE` is 10s; wait comfortably past it (real wall-clock
    // sleep — this is a real detached process on the other end, not
    // something a paused tokio clock in this test process could fast-forward)
    // before concluding the holder must have exited.
    tokio::time::sleep(Duration::from_secs(20)).await;

    let mut tab4 = fixture.spawn_tab();
    wait_for_ready(&mut tab4, Duration::from_secs(30), "tab4").await;
    assert_eq!(
        fixture.connection_count.load(Ordering::SeqCst),
        2,
        "once every tab closed and IDLE_GRACE elapsed, the old holder must have exited — a new tab must spawn a fresh holder (a second real SSH connection), not find a stale one still listening"
    );

    let _ = tab4.start_kill();
}
