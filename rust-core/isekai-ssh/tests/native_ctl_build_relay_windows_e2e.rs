//! End-to-end coverage for the Windows-native mux path's ctl-socket forward
//! (`native/mux/ctl_forward.rs`) and its Epic P build-trigger relay
//! (`native/mux/build_relay.rs` + dedicated dispatch in `owner.rs::relay_client`/
//! `client.rs::run_inner`). `ISEKAI_PIPE_DESIGN.md` (§8 Epic P) says Phase 2
//! (Windows-native) was verified only via mock-SSH + in-memory mux unit
//! tests plus cross-compilation to `x86_64-pc-windows-gnu` — "no real
//! Windows machine, no real named pipe, no real `cmd.exe`, no real
//! `isekai-ssh.exe` build was ever run." This file closes that gap with a
//! real `isekai-ssh.exe` process.
//!
//! **Harness**: same mock-sshd-over-TCP + real subprocess pattern as
//! `mux_holder_windows_e2e.rs`, but the mock sshd here plays the *ctl-socket
//! pusher* role instead: on the client's real `streamlocal_forward` request
//! (issued by `native/mux/ctl_forward.rs::request`, which is what
//! `#@isekai ctl-socket yes` opts into), it opens a real
//! `forwarded-streamlocal` channel back — the exact same
//! `session.handle().channel_open_forwarded_streamlocal(path)` API this
//! crate's own in-process unit test double (`ctl_forward.rs`'s
//! `CtlPushHandler`) already proves works, just driven by a real subprocess
//! this time instead of an in-process `request()`/`pump_to_frames()` call.
//!
//! Only one tab is spawned in both tests here (it becomes its own spawn
//! leader and, per `mod.rs`'s dispatch, a real *separate* detached holder
//! process gets spawned and the tab attaches to it as a client — same as
//! `mux_holder_windows_e2e.rs`'s single-tab case). The ctl-socket forward is
//! requested by the holder (`owner.rs::relay_client`, called once per
//! attaching client) and relayed to the tab as `Frame::Ctl`
//! (`ctl_forward.rs::pump_to_frames` → `owner.rs::relay_loop`), which the
//! tab applies via `crate::ctl_forward::osc_sequence_for` written to *its
//! own* stderr (`client.rs::run_inner`, confirmed by reading that function)
//! — that's the observable signal Test 1 polls for. For `BuildRequest`
//! specifically, the owner also tracks a `reply_tx`
//! (`CtlRelayEvent::BuildStarted`) so the tab's own `BuildOutputChunk`/
//! `BuildFinished` bytes route back onto the *same* real forwarded channel
//! this mock server's `streamlocal_forward` handler is still holding —
//! Test 2 reads them back off that same `Channel` object.
//!
//! Requires bare `isekai-ssh <alias>` (no trailing remote command) —
//! `crate::ctl_forward::should_attempt_ctl_forward` requires
//! `ssh_args_len == destination_index + 1` — plus `#@isekai ctl-socket yes`.
#![cfg(windows)]

use std::io::BufRead as StdBufRead;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

use isekai_protocol::handshake::HandshakeJson;
use isekai_protocol::CtlMessage;
use isekai_trust::{HelperTrust, UpdatePolicy};
use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
use russh::{Channel as RusshChannel, ChannelId, CryptoVec, Pty};
use russh_keys::PublicKey;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::mpsc;

mod support;

/// Serializes this file's tests, same rationale as
/// `mux_holder_windows_e2e.rs::TEST_SERIAL_GUARD`: each test spawns a real
/// detached mux holder, and `HOLDER_STARTUP_TIMEOUT` (10s, not
/// test-overridable) is tight enough that CPU contention from a concurrently
/// running sibling test can make one give up and fall back to a direct
/// connect (a real CI failure this project already hit once).
static TEST_SERIAL_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
// Mock sshd: accepts a real `streamlocal_forward` request (issued by the
// native ctl-socket-forward path), opens a real `forwarded-streamlocal`
// channel back exactly like `ctl_forward.rs`'s own `CtlPushHandler` test
// double, pushes one `CtlMessage`, and (unlike that fire-and-forget double)
// keeps reading the channel afterward so a build's replies can be observed.
// Also accepts the plain `channel_open_session`+`pty_request`+`exec_request`
// the native path's `open_login_shell` issues right after — the mock never
// interprets that command string, it just needs to succeed so the whole
// `relay_client` call doesn't tear the already-established ctl forward back
// down.
// ---------------------------------------------------------------------

#[derive(Clone)]
struct CtlPushServer {
    accepted_client_key: PublicKey,
    push_message: CtlMessage,
    /// If set, forwards every subsequent byte read off the pushed channel
    /// here — used by the build-relay test to observe `BuildOutputChunk`/
    /// `BuildFinished` replies. `None` for the fire-and-forget ctl-relay
    /// test (`SetTabColor`), which never expects a reply.
    response_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
}

impl server::Server for CtlPushServer {
    type Handler = CtlPushHandler;
    fn new_client(&mut self, _: Option<SocketAddr>) -> CtlPushHandler {
        CtlPushHandler { accepted_client_key: self.accepted_client_key.clone(), push_message: self.push_message.clone(), response_tx: self.response_tx.clone() }
    }
}

#[derive(Clone)]
struct CtlPushHandler {
    accepted_client_key: PublicKey,
    push_message: CtlMessage,
    response_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
}

#[async_trait::async_trait]
impl server::Handler for CtlPushHandler {
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

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }

    /// The native path's `open_login_shell` execs `export
    /// ISEKAI_CTL_SOCK=...; exec "${SHELL:-/bin/sh}" -i -l` here — a Linux
    /// shell command meaningless to actually run on this (Windows) mock
    /// process, so it's accepted and never interpreted. The channel is left
    /// open indefinitely (no exit status, no close) since nothing in either
    /// test needs this "login shell" channel to ever produce output.
    async fn exec_request(&mut self, channel: ChannelId, _data: &[u8], session: &mut ServerSession) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }

    async fn streamlocal_forward(&mut self, socket_path: &str, session: &mut ServerSession) -> Result<bool, Self::Error> {
        let handle = session.handle();
        let path = socket_path.to_string();
        let msg = self.push_message.clone();
        let response_tx = self.response_tx.clone();
        tokio::spawn(async move {
            let Ok(mut channel) = handle.channel_open_forwarded_streamlocal(path.clone()).await else { return };
            let _ = channel.data(format!("{path}\n").as_bytes()).await;
            let Ok(mut line) = serde_json::to_vec(&msg) else { return };
            line.push(b'\n');
            let _ = channel.data(&line[..]).await;

            let Some(response_tx) = response_tx else {
                let _ = channel.eof().await;
                return;
            };
            loop {
                match channel.wait().await {
                    Some(russh::ChannelMsg::Data { data }) => {
                        if response_tx.send(data.to_vec()).is_err() {
                            break;
                        }
                    }
                    Some(russh::ChannelMsg::Eof) | Some(russh::ChannelMsg::Close) | None => break,
                    _ => {}
                }
            }
        });
        Ok(true)
    }
}

async fn spawn_ctl_push_ssh_server(accepted_client_key: PublicKey, push_message: CtlMessage, response_tx: Option<mpsc::UnboundedSender<Vec<u8>>>) -> (SocketAddr, String) {
    support::spawn_sshd(211, CtlPushServer { accepted_client_key, push_message, response_tx }).await
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

/// Writes `~/.config/isekai-ssh/build_profiles.toml` directly (matching
/// `build_profile.rs::save_build_profiles`'s TOML shape) rather than going
/// through the `isekai-ssh build-profile add` CLI subprocess. `host` must
/// equal the ssh_config alias the tab connects through
/// (`build_relay.rs`/`client.rs` look profiles up by `(host, name)` where
/// `host` is the tab's own resolved destination alias, confirmed by reading
/// `run_client_build`/`client.rs::run`). `result_glob`/`dest_dir` are both
/// omitted (must be both-or-neither, `build_profile.rs::upsert_profile`) so
/// `BuildFinished` never triggers `push_result_file`'s recursive `isekai-ssh`
/// invocation — out of scope here, already covered by
/// `build_result_push_e2e.rs`.
fn write_build_profile(home: &std::path::Path, host_alias: &str, profile_name: &str, command: &str) {
    let dir = home.join(".config").join("isekai-ssh");
    std::fs::create_dir_all(&dir).unwrap();
    let toml = format!(
        "[[profile]]\nhost = {host_alias:?}\nname = {profile_name:?}\ndir = {home_dir:?}\ncommand = {command:?}\n",
        home_dir = home.to_string_lossy(),
    );
    std::fs::write(dir.join("build_profiles.toml"), toml).unwrap();
}

/// Drains a piped stream in the background and discards it — used for
/// whichever of a tab's stdout/stderr this harness isn't actively reading
/// for its own assertion, so the child never blocks on a full pipe buffer.
/// Unlike `mux_holder_windows_e2e.rs`'s `wait_for_ready`, there is no
/// `"ready\n"` banner to wait for here: no real remote shell ever answers
/// (the mock's `exec_request` accepts the login-shell exec but never writes
/// anything back), so the real synchronization signal in both tests below
/// is the ctl-push side-channel itself (the tab's stderr OSC sequence, or
/// the mock server's own `response_rx`), each already guarded by its own
/// generous timeout.
fn drain_in_background<R>(stream: R)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        tokio::pin!(stream);
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
}

fn spawn_tab(home: &std::path::Path, isekai_pipe_path: &std::path::Path, runtime_dir: &std::path::Path, alias: &str) -> Child {
    TokioCommand::new(isekai_ssh_bin_path())
        .arg("--isekai-pipe-path")
        .arg(isekai_pipe_path)
        .arg(alias)
        .env("HOME", home)
        .env("ISEKAI_PIPE_PROFILES_DIR", profiles_dir_under(home))
        .env("ISEKAI_PIPE_LOG_FILE", home.join("isekai-ssh-verbose.log"))
        .env("ISEKAI_PIPE_RUNTIME_DIR", runtime_dir)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn isekai-ssh")
}

struct CtlFixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    isekai_pipe_path: PathBuf,
    runtime_dir: PathBuf,
    _helper: HelperProcess,
    alias: String,
}

impl CtlFixture {
    async fn new(alias: &str, push_message: CtlMessage, response_tx: Option<mpsc::UnboundedSender<Vec<u8>>>) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let (key_path, client_pubkey) = generate_client_keypair(tmp.path());
        let (mock_sshd_addr, mock_sshd_fingerprint) = spawn_ctl_push_ssh_server(client_pubkey, push_message, response_tx).await;

        let home = tmp.path().join("client-home");
        std::fs::create_dir_all(&home).unwrap();
        seed_ssh_host_key_trust(&home, &format!("127.0.0.1:{}", mock_sshd_addr.port()), &mock_sshd_fingerprint);

        // `#@isekai ctl-socket yes` opts into the ctl-socket forward at all
        // (`should_attempt_ctl_forward`); no trailing remote command (bare
        // `isekai-ssh <alias>`) since that predicate also requires
        // `ssh_args_len == destination_index + 1`.
        let host_block = format!(
            "Host {alias}\n\
             \x20\x20\x20\x20HostName 127.0.0.1\n\
             \x20\x20\x20\x20Port {port}\n\
             \x20\x20\x20\x20User tester\n\
             \x20\x20\x20\x20IdentityFile {key}\n\
             \x20\x20\x20\x20IdentitiesOnly yes\n\
             \x20\x20\x20\x20#@isekai ctl-socket yes\n",
            port = mock_sshd_addr.port(),
            key = key_path.display(),
        );
        let home_ssh_dir = home.join(".ssh");
        std::fs::create_dir_all(&home_ssh_dir).unwrap();
        std::fs::write(home_ssh_dir.join("config"), &host_block).unwrap();

        let helper = spawn_real_helper(mock_sshd_addr);
        let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap()).parse().unwrap();
        let cert_sha256_hex = helper.handshake.cert_sha256().to_string();
        let profile_key = isekai_trust::normalize_host_port(alias).unwrap();
        register_correct_profile(&profiles_dir_under(&home), &profile_key, helper_addr, &cert_sha256_hex, &helper.handshake.session_secret);

        let runtime_dir = tmp.path().join("runtime");
        std::fs::create_dir_all(&runtime_dir).unwrap();

        Self { _tmp: tmp, home, isekai_pipe_path: isekai_pipe_bin_path(), runtime_dir, _helper: helper, alias: alias.to_string() }
    }

    fn spawn_tab(&self) -> Child {
        spawn_tab(&self.home, &self.isekai_pipe_path, &self.runtime_dir, &self.alias)
    }
}

/// Test 1: a pushed `SetTabColor` ctl message must reach the tab (via the
/// holder's ctl-socket forward → `Frame::Ctl` mux relay) and be applied as
/// the exact OSC 4;264 sequence `crate::ctl_forward::osc_sequence_for`
/// produces, written to the tab's own stderr.
#[tokio::test(flavor = "multi_thread")]
async fn ctl_message_relays_from_the_mock_sshd_to_the_tabs_own_stderr() {
    let _serial = TEST_SERIAL_GUARD.lock().await;
    let push = CtlMessage::SetTabColor { r: 0xff, g: 0x00, b: 0xaa };
    let fixture = CtlFixture::new("native-ctl-relay-e2e", push, None).await;

    let mut tab = fixture.spawn_tab();
    drain_in_background(tab.stdout.take().expect("stdout was piped"));

    let mut stderr = tab.stderr.take().expect("stderr was piped");
    let mut seen = Vec::new();
    let mut chunk = [0u8; 4096];
    let expected = "4;264;rgb:ff/00/aa";
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let n = stderr.read(&mut chunk).await.expect("reading tab stderr should not error");
            assert!(n > 0, "tab's stderr closed before the OSC sequence ever appeared, saw {:?}", String::from_utf8_lossy(&seen));
            seen.extend_from_slice(&chunk[..n]);
            if String::from_utf8_lossy(&seen).contains(expected) {
                return;
            }
        }
    })
    .await;
    assert!(result.is_ok(), "timed out waiting for the OSC 4;264 sequence on the tab's stderr, saw so far: {:?}", String::from_utf8_lossy(&seen));

    let _ = tab.start_kill();
}

/// Test 2: the Epic P build-trigger relay's full round trip through the
/// Windows-native mux — a real `cmd.exe` child actually runs, and
/// `BuildFinished`'s exit code makes it all the way back to the mock sshd
/// over the same real forwarded-streamlocal channel.
#[tokio::test(flavor = "multi_thread")]
async fn build_request_relays_through_the_holder_and_a_real_child_process_runs() {
    let _serial = TEST_SERIAL_GUARD.lock().await;
    let alias = "native-build-relay-e2e";
    let (response_tx, mut response_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let push = CtlMessage::BuildRequest { profile: "win-build".to_string() };
    let fixture = CtlFixture::new(alias, push, Some(response_tx)).await;
    // `build_exec::spawn_shell_command` already wraps this in `cmd /C` on
    // Windows — the profile's own `command` must not double-wrap.
    write_build_profile(&fixture.home, alias, "win-build", "echo hi>out.txt");

    let mut tab = fixture.spawn_tab();
    drain_in_background(tab.stdout.take().expect("stdout was piped"));
    drain_in_background(tab.stderr.take().expect("stderr was piped"));

    // Accumulate response bytes until a decodable `BuildFinished` line
    // appears — same newline-delimited-JSON wire format used everywhere else
    // in this project's ctl protocol.
    let mut buf = Vec::new();
    let finished = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let chunk = response_rx.recv().await.expect("mock sshd's ctl channel closed before BuildFinished arrived");
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = &line[..line.len() - 1];
                if let Ok(msg) = isekai_protocol::decode_ctl_message(line) {
                    if let CtlMessage::BuildFinished { exit_code, result_paths } = msg {
                        return (exit_code, result_paths);
                    }
                }
            }
        }
    })
    .await
    .expect("timed out waiting for BuildFinished");

    assert_eq!(finished.0, 0, "the real `cmd /C echo hi>out.txt` child must exit 0");
    assert!(finished.1.is_empty(), "result_glob/dest_dir were omitted, so no result_paths are expected: {:?}", finished.1);

    let out_file = fixture.home.join("out.txt");
    assert!(out_file.exists(), "the build profile's real cmd.exe child must have actually run and written out.txt");

    let _ = tab.start_kill();
}
