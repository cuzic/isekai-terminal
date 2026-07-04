//! End-to-end test for `isekai-ssh connect --resume-window` (Phase S-4d,
//! `ISEKAI_SSH_DESIGN.md` "resume を ProxyCommand の背後に隠す"): once a
//! disconnect outlives the configured resume window, `connect` must give up
//! *cleanly* — explicitly close stdin/stdout, print an actionable stderr
//! message, and let the process exit successfully (`Ok(())`) — rather than
//! hang forever retrying, or leak anything onto stdout.
//!
//! This is the "resume window exceeded" counterpart to
//! `resume_reconnect_e2e.rs`'s "resume window not exceeded" happy path: same
//! real `isekai-ssh`/`isekai-helper` binaries and the same UDP blackhole
//! proxy technique (see that file's module docs for why a real blackhole is
//! used instead of `SIGSTOP`/`SIGCONT`), but here the blackhole is **never
//! lifted** — both `isekai-ssh --resume-window` and `isekai-helper
//! --resume-window` are configured short (this test's whole reason to exist:
//! `RESUME_WINDOW` used to be a hardcoded 120s constant, far too slow for a
//! test) so the give-up path is actually reached in a bounded amount of time.
//!
//! Nothing here is a type-checking-only mock: this spawns the actual compiled
//! `isekai-ssh` binary (`--dev-insecure-*`, the same bypass
//! `resume_reconnect_e2e.rs`/`connect_e2e.rs` use since the trust store isn't
//! wired into this test) and the actual compiled `isekai-helper` binary,
//! relaying to a real TCP echo server.
//!
//! Requires the `dev-insecure` feature: `cargo test -p isekai-ssh --features
//! dev-insecure --test resume_window_exceeded_e2e`.
//!
//! This test is inherently slow, for the same reason
//! `resume_reconnect_e2e.rs` is: both sides' QUIC idle-timeout detection
//! (`isekai_transport::system::CLIENT_MAX_IDLE_TIMEOUT` / isekai-helper's own
//! `--idle-timeout`, 15s each, `HELPER_PROTOCOL.md` §7.5 notes up to ~40s
//! worst case for detection alone) has to elapse *before* isekai-ssh's own
//! `--resume-window` clock even starts ticking (`connect.rs::run_relay_resumable`
//! only starts `disconnected_since` once `run_data_pump` actually reports an
//! error) — so even a `--resume-window` of a few seconds cannot make this
//! test finish in a few seconds. Expect it to take on the order of a minute.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use isekai_protocol::handshake::{decode_handshake_json, HandshakeJson};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

// ---------------------------------------------------------------------
// Shared plumbing (deliberately duplicated from `resume_reconnect_e2e.rs` /
// `connect_e2e.rs` — see `stdout_cleanliness.rs`'s module docs for why this
// crate's convention is one self-contained test file per scenario rather
// than a shared `tests/common/` module).
// ---------------------------------------------------------------------

fn isekai_ssh_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-ssh"))
}

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
        let mut cmd = Command::new(env!("CARGO"));
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

struct HelperProcess {
    child: Child,
    handshake: HandshakeJson,
    /// Every stderr line isekai-helper has printed so far, so this test can
    /// check for `sweep_expired_parked`'s "expired while parked, discarded"
    /// log line (`isekai-helper/src/resume.rs`) after the wait — proof the
    /// parked session was actually discarded, not just that isekai-ssh gave
    /// up on its own end (acceptance criterion 3's "可能なら…helper側の
    /// ログ…からも確認する").
    stderr_log: Arc<Mutex<String>>,
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawns the real compiled `isekai-helper` binary with a short
/// `--resume-window`, targeting a real TCP listener (`target_addr`) — no
/// SSH/`sshd` involved, matching `resume_reconnect_e2e.rs`'s rationale (this
/// test is about isekai-ssh's give-up behavior, not SSH itself).
fn spawn_helper(target_addr: SocketAddr, resume_window_secs: u64) -> HelperProcess {
    let mut cmd = Command::new(isekai_helper_bin_path());
    cmd.arg("--target")
        .arg(target_addr.to_string())
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--resume-window")
        .arg(resume_window_secs.to_string())
        .arg("--log-level")
        .arg("debug")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn isekai-helper");
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read handshake line from isekai-helper stdout");
    let handshake = decode_handshake_json(line.trim().as_bytes()).expect("failed to parse/validate handshake JSON");

    let stderr_log = Arc::new(Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let stderr_log = stderr_log.clone();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stderr);
            let mut buf = String::new();
            loop {
                buf.clear();
                if r.read_line(&mut buf).unwrap_or(0) == 0 {
                    break;
                }
                eprint!("[isekai-helper] {buf}");
                stderr_log.lock().unwrap().push_str(&buf);
            }
        });
    }
    std::mem::forget(reader);

    HelperProcess { child, handshake, stderr_log }
}

fn dev_insecure_args(proxy_addr: SocketAddr, handshake: &HandshakeJson) -> Vec<String> {
    vec![
        "--dev-insecure-target".to_string(),
        proxy_addr.to_string(),
        "--dev-insecure-cert-sha256".to_string(),
        handshake.cert_sha256.clone(),
        "--dev-insecure-session-secret".to_string(),
        handshake.session_secret.clone(),
    ]
}

/// A trivial TCP echo server standing in for isekai-helper's `--target`
/// (usually `sshd`), matching `resume_reconnect_e2e.rs`'s rationale.
async fn spawn_tcp_echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if sock.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

/// Handle to a UDP passthrough proxy sitting between isekai-ssh and the real
/// isekai-helper (`resume_reconnect_e2e.rs`'s `BlackholeProxy`, duplicated
/// here — this test only ever calls `block()`, never `unblock()`, but keeps
/// the same shape for clarity/consistency with the sibling test).
struct BlackholeProxy {
    enabled: Arc<AtomicBool>,
}

impl BlackholeProxy {
    fn block(&self) {
        self.enabled.store(false, Ordering::SeqCst);
    }
}

async fn spawn_blackhole_proxy(upstream: SocketAddr) -> (SocketAddr, BlackholeProxy) {
    let client_side = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = client_side.local_addr().unwrap();
    let upstream_side = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    let enabled = Arc::new(AtomicBool::new(true));
    let last_client_addr: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));

    let client_side = Arc::new(client_side);
    let upstream_side = Arc::new(upstream_side);

    // client -> upstream (isekai-ssh -> isekai-helper)
    {
        let client_side = client_side.clone();
        let upstream_side = upstream_side.clone();
        let enabled = enabled.clone();
        let last_client_addr = last_client_addr.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 65535];
            loop {
                let Ok((n, from)) = client_side.recv_from(&mut buf).await else { break };
                *last_client_addr.lock().unwrap() = Some(from);
                if enabled.load(Ordering::SeqCst) {
                    let _ = upstream_side.send_to(&buf[..n], upstream).await;
                }
                // else: blackholed — the datagram is simply dropped.
            }
        });
    }

    // upstream -> client (isekai-helper -> isekai-ssh)
    {
        let client_side = client_side.clone();
        let upstream_side = upstream_side.clone();
        let enabled = enabled.clone();
        let last_client_addr = last_client_addr.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 65535];
            loop {
                let Ok((n, _from)) = upstream_side.recv_from(&mut buf).await else { break };
                if enabled.load(Ordering::SeqCst) {
                    let target = *last_client_addr.lock().unwrap();
                    if let Some(target) = target {
                        let _ = client_side.send_to(&buf[..n], target).await;
                    }
                }
            }
        });
    }

    (proxy_addr, BlackholeProxy { enabled })
}

/// Background reader for a child's stdout (`resume_reconnect_e2e.rs`'s
/// `StdoutReader`, duplicated here) — makes reads-with-a-deadline possible
/// despite `std::process::ChildStdout::read` blocking indefinitely.
struct StdoutReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    leftover: Vec<u8>,
    /// Every byte ever received, kept around (unlike `resume_reconnect_e2e.rs`'s
    /// version) so this test can assert on the *total* stdout output at the
    /// end, not just what was consumed by `read_exact_timeout` along the way.
    all_received: Arc<Mutex<Vec<u8>>>,
}

impl StdoutReader {
    fn spawn(mut stdout: std::process::ChildStdout) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let all_received = Arc::new(Mutex::new(Vec::new()));
        let all_received_writer = all_received.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        all_received_writer.lock().unwrap().extend_from_slice(&buf[..n]);
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self { rx, leftover: Vec::new(), all_received }
    }

    /// Reads exactly `len` bytes within `timeout`, or `None` on
    /// timeout/EOF/a dead reader thread.
    fn read_exact_timeout(&mut self, len: usize, timeout: Duration) -> Option<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        while self.leftover.len() < len {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            match self.rx.recv_timeout(remaining) {
                Ok(chunk) => self.leftover.extend_from_slice(&chunk),
                Err(_) => return None,
            }
        }
        Some(self.leftover.drain(..len).collect())
    }

    fn total_bytes_received(&self) -> Vec<u8> {
        self.all_received.lock().unwrap().clone()
    }
}

fn echo_round_trip(stdin: &mut impl Write, reader: &mut StdoutReader, payload: &[u8], timeout: Duration) -> bool {
    if stdin.write_all(payload).is_err() || stdin.flush().is_err() {
        return false;
    }
    match reader.read_exact_timeout(payload.len(), timeout) {
        Some(received) => received == payload,
        None => false,
    }
}

/// Polls `child.try_wait()` until it exits or `timeout` elapses. Async (not
/// `std::thread::sleep`-based) so the tokio-driven blackhole proxy tasks
/// above keep running concurrently on this test's multi-thread runtime.
async fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_gives_up_cleanly_once_resume_window_is_exceeded() {
    // Short and equal on both sides (`ConnectArgs::resume_window`'s docs:
    // isekai-ssh's own window should not outlive isekai-helper's, or every
    // attempt made after isekai-helper's own sweep would just fail with
    // REJECT_UNKNOWN_SESSION instead of isekai-ssh's own clean give-up
    // message) — this is the whole point of making `RESUME_WINDOW`
    // configurable instead of the old hardcoded 120s constant.
    const RESUME_WINDOW_SECS: u64 = 5;

    let target_addr = spawn_tcp_echo_server().await;
    let helper = spawn_helper(target_addr, RESUME_WINDOW_SECS);
    let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port).parse().unwrap();
    let (proxy_addr, proxy) = spawn_blackhole_proxy(helper_addr).await;

    let mut child = Command::new(isekai_ssh_bin_path())
        .arg("connect")
        .arg("dummy-host")
        .args(dev_insecure_args(proxy_addr, &helper.handshake))
        .arg("--resume-window")
        .arg(RESUME_WINDOW_SECS.to_string())
        .env("RUST_LOG", "isekai_ssh=debug,isekai_transport=debug")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh connect");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout_reader = StdoutReader::spawn(child.stdout.take().unwrap());

    // Collect isekai-ssh's own stderr (instead of just printing it, like
    // `resume_reconnect_e2e.rs` does) so this test can assert the give-up
    // message (`connect.rs::run_relay_resumable`'s `eprintln!`) actually
    // appears.
    let isekai_ssh_stderr = Arc::new(Mutex::new(String::new()));
    {
        let stderr = child.stderr.take().unwrap();
        let isekai_ssh_stderr = isekai_ssh_stderr.clone();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stderr);
            let mut buf = String::new();
            loop {
                buf.clear();
                if r.read_line(&mut buf).unwrap_or(0) == 0 {
                    break;
                }
                eprint!("[isekai-ssh] {buf}");
                isekai_ssh_stderr.lock().unwrap().push_str(&buf);
            }
        });
    }

    // 1. Prove the relay works end-to-end before touching anything, exactly
    // like `resume_reconnect_e2e.rs` — this is also the only stdout traffic
    // this test ever expects to see (see step 4 below).
    let initial_payload = b"hello-before-permanent-disconnect\n";
    assert!(
        echo_round_trip(&mut stdin, &mut stdout_reader, initial_payload, Duration::from_secs(15)),
        "initial echo round trip (before any disconnect) failed"
    );

    // 2. Blackhole the path and never lift it. Both sides' independent QUIC
    // idle-timeouts fire on their own schedule (`CLIENT_MAX_IDLE_TIMEOUT`/
    // `--idle-timeout`, both 15s by default; this test does not shorten
    // those — only the two `--resume-window`s, S-4d's actual scope), after
    // which isekai-ssh's own `RESUME_WINDOW_SECS`-long give-up clock starts.
    proxy.block();

    // 2b. Deliberately do **not** close our end of the child's stdin here —
    // keep it open, exactly like a real `ssh` ProxyCommand parent that has no
    // reason to think the session is over yet (this is the realistic
    // production scenario `ISEKAI_SSH_DESIGN.md`'s "制約: sshの生存確認との
    // レース" describes: with `ServerAliveInterval 0` or a `ServerAlive`
    // grace period longer than isekai-ssh's own `--resume-window`, `ssh`
    // itself never closes its write end before isekai-ssh gives up).
    //
    // This is exactly the condition that used to hang the process
    // indefinitely: `pump_c2h`'s `tokio::io::stdin()` read dispatches to a
    // background OS thread that Tokio's own docs say is "not currently
    // cancelled" on runtime shutdown, and `run_relay_resumable`'s
    // `drop(stdin)` in its give-up path only drops *our* handle to it, not
    // that already-blocked thread. `main.rs` now calls `std::process::exit`
    // on every path instead of returning normally from `#[tokio::main]`
    // specifically so this orphaned thread can never block process exit
    // (see that file's comment on this exact scenario) — leaving this
    // child's stdin open through to the end of the test is what actually
    // exercises that fix, rather than sidestepping the scenario entirely.
    let _stdin = stdin;

    // 3. Wait for isekai-ssh to give up entirely on its own — no unblocking,
    // unlike `resume_reconnect_e2e.rs`. Generous bound: idle-timeout
    // detection alone can take up to ~40s worst case
    // (`HELPER_PROTOCOL.md` §7.5), plus at least one full resume attempt
    // (itself a fresh QUIC connect against the still-blackholed address)
    // that can run past `RESUME_WINDOW_SECS` before the loop notices —
    // this assertion is about "does it ever finish" (no hang), not about
    // being tight to the second.
    let status = wait_with_timeout(&mut child, Duration::from_secs(150))
        .await
        .expect("isekai-ssh connect did not exit within 150s of a permanent disconnect — it appears to be hanging");

    // 4. Exit must be clean (`ISEKAI_SSH_DESIGN.md`'s minimal give-up policy:
    // `run_relay_resumable` returns `Ok(())`, which `main.rs` maps to
    // `ExitCode::SUCCESS`), not a crash/panic.
    assert!(status.success(), "isekai-ssh connect should exit successfully (Ok(())) when it gives up, got {status:?}");

    // 5. stdout purity: the *only* bytes ever written to stdout for this
    // whole test are the initial echoed payload from step 1 — nothing from
    // isekai-helper arrives after the blackhole starts (it's blackholed),
    // and the give-up path must never write anything to stdout itself
    // (`connect.rs`'s module docs: stdout purity is load-bearing). Checking
    // the *total* accumulated stdout (not just what step 1 already
    // consumed) additionally proves the explicit `stdout.shutdown()` in the
    // give-up path didn't somehow flush/leak anything extra.
    let total_stdout = stdout_reader.total_bytes_received();
    assert_eq!(
        total_stdout, initial_payload,
        "stdout must contain exactly the initial echoed payload and nothing else, got {:?}",
        String::from_utf8_lossy(&total_stdout)
    );

    // 6. The give-up path's stderr message (`connect.rs::run_relay_resumable`)
    // must actually have been printed, and be actionable.
    let stderr_text = isekai_ssh_stderr.lock().unwrap().clone();
    assert!(
        stderr_text.contains("giving up") && stderr_text.contains("resume window"),
        "expected an actionable give-up message on isekai-ssh's stderr, got:\n{stderr_text}"
    );

    // 7. (Best-effort, acceptance criterion 3's "可能なら"): isekai-helper's
    // own `sweep_expired_parked` (`isekai-helper/src/resume.rs`) should also
    // have actually discarded the parked session by now — proving the give
    // up isn't just isekai-ssh's own client-side illusion, isekai-helper's
    // side of the resumable session is genuinely gone too. isekai-helper's
    // sweep loop polls every 5s (`isekai-helper/src/main.rs`), and 150s of
    // wall-clock time has already elapsed above, so this should be reliably
    // observable by now.
    let helper_stderr_text = helper.stderr_log.lock().unwrap().clone();
    assert!(
        helper_stderr_text.contains("expired while parked, discarded"),
        "expected isekai-helper's own sweep_expired_parked to have discarded the parked session by now, \
         got isekai-helper stderr:\n{helper_stderr_text}"
    );

    let _ = child.kill();
    let _ = child.wait();
}
