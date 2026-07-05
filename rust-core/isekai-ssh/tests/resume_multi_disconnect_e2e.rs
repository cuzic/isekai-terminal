//! Integration end-to-end test for `isekai-ssh connect` (`--mode relay`, the
//! default) covering a **single process lifecycle that experiences two
//! separate disconnect events** (`ISEKAI_SSH_DESIGN.md` "resume を
//! ProxyCommand の背後に隠す"): first a recoverable outage (resumes and keeps
//! relaying), then a second, permanent outage that outlives the resume
//! window (gives up cleanly). `resume_reconnect_e2e.rs` and
//! `resume_window_exceeded_e2e.rs` each cover exactly one of these scenarios
//! in isolation, spawning a fresh `isekai-ssh connect` process per test; this
//! file exists to prove the two code paths compose correctly within *one*
//! long-lived process — i.e. that a successful resume properly resets
//! `run_relay_resumable`'s internal state (`disconnected_since`/`attempt`,
//! `connect.rs`) so a *second*, later disconnect is handled exactly like a
//! first one would be, not short-circuited or double-counted against the
//! first outage's already-elapsed window.
//!
//! This is also a stronger correctness check than the sibling tests along a
//! second axis (`ISEKAI_SSH_DESIGN.md`'s C2H/H2C commit/delivered-offset
//! design): instead of single short "hello\n"-style pings, this test
//! round-trips several distinguishable multi-kilobyte chunks (each built from
//! a distinct deterministic byte pattern, `make_chunk`) through the relay
//! before/after the recoverable outage, and asserts **byte-for-byte, in
//! order** on both each individual round trip and, after the process exits,
//! on isekai-ssh's *entire* accumulated stdout — proving no chunk is lost,
//! duplicated, reordered, or corrupted across a resume, not merely that *some*
//! response eventually arrives.
//!
//! Nothing here is a type-checking-only mock: this spawns the actual compiled
//! `isekai-ssh` binary (`--dev-insecure-*`, the same bypass
//! `resume_reconnect_e2e.rs`/`resume_window_exceeded_e2e.rs` use since the
//! trust store isn't wired into this test) and the actual compiled
//! `isekai-helper` binary, relaying to a real TCP echo server, with a real UDP
//! blackhole proxy toggled to genuinely drop packets in both directions (see
//! `resume_reconnect_e2e.rs`'s module docs for why a real blackhole is used
//! instead of `SIGSTOP`/`SIGCONT`).
//!
//! Requires the `dev-insecure` feature: `cargo test -p isekai-ssh --features
//! dev-insecure --test resume_multi_disconnect_e2e`.
//!
//! This test is inherently slow — it combines both sibling tests' waits (a
//! recoverable outage plus a full give-up sequence) in one run — expect it to
//! take on the order of several minutes.

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
// `resume_window_exceeded_e2e.rs` — see `stdout_cleanliness.rs`'s module docs
// for why this crate's convention is one self-contained test file per
// scenario rather than a shared `tests/common/` module).
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
    /// Every stderr line isekai-helper has printed so far
    /// (`resume_window_exceeded_e2e.rs`'s rationale: proves the parked
    /// session was actually discarded on isekai-helper's own side too, not
    /// just that isekai-ssh gave up on its own end).
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
/// SSH/`sshd` involved, matching the sibling tests' rationale (this test is
/// about isekai-ssh's resume/give-up behavior, not SSH itself). Never
/// frozen/signaled by this test; it runs continuously and detects the
/// simulated outages entirely on its own via its normal `--idle-timeout`.
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
/// (usually `sshd`), matching the sibling tests' rationale.
async fn spawn_tcp_echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
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
/// here). While `enabled` is `true` (the default), every datagram is
/// forwarded transparently in both directions. While `false`, every datagram
/// arriving on either side is silently dropped, genuinely simulating a lost
/// network path rather than a frozen peer.
struct BlackholeProxy {
    enabled: Arc<AtomicBool>,
}

impl BlackholeProxy {
    fn block(&self) {
        self.enabled.store(false, Ordering::SeqCst);
    }

    fn unblock(&self) {
        self.enabled.store(true, Ordering::SeqCst);
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

/// Background reader for a child's stdout (`resume_window_exceeded_e2e.rs`'s
/// `StdoutReader`, duplicated here) — makes reads-with-a-deadline possible
/// despite `std::process::ChildStdout::read` blocking indefinitely, and keeps
/// every byte ever received so the final assertion can check the *entire*
/// accumulated stdout, not just what was consumed along the way.
struct StdoutReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    leftover: Vec<u8>,
    all_received: Arc<Mutex<Vec<u8>>>,
}

impl StdoutReader {
    fn spawn(mut stdout: std::process::ChildStdout) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let all_received = Arc::new(Mutex::new(Vec::new()));
        let all_received_writer = all_received.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
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

/// Builds a distinguishable, deterministic pseudo-random payload for chunk
/// `id` of length `len` bytes (a trivial xorshift-style LCG keyed by `id`).
/// Different `id`s always produce different byte streams, so a bug that
/// duplicated, dropped, reordered, or corrupted a chunk across a resume shows
/// up as a concrete byte mismatch (or a total-length mismatch) rather than
/// being masked by every chunk looking alike, the way a fixed short string
/// like `b"hello\n"` repeated many times could.
fn make_chunk(id: u32, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut state = id.wrapping_mul(2_654_435_761).wrapping_add(0x9e37_79b9);
    for _ in 0..len {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        v.push((state >> 16) as u8);
    }
    v
}

/// Writes `payload` to `stdin` and reads exactly `payload.len()` bytes back
/// via `reader` within `timeout`, asserting an exact byte-for-byte match —
/// proves the byte relay is genuinely alive and uncorrupted end-to-end
/// (isekai-ssh -> isekai-helper -> TCP echo -> isekai-helper -> isekai-ssh) at
/// the point it's called, not just that the process is running.
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
async fn connect_recovers_from_one_outage_then_cleanly_gives_up_on_a_second() {
    // Long enough to comfortably survive the first (recoverable) outage's
    // full blackout duration below (45s) regardless of exactly when within
    // that window either side's QUIC idle-timeout fires and
    // `disconnected_since` starts ticking — the worst case gap between
    // detection and the blackhole lifting is bounded by the blackout
    // duration itself, not by the (unknown, up to ~40s) detection delay. Also
    // short enough that the *second*, permanent outage's give-up path is
    // still reached in a bounded, test-friendly amount of time. Equal on
    // both isekai-ssh and isekai-helper, matching `ConnectArgs::resume_window`'s
    // docs (mismatched windows would make isekai-helper's own
    // `sweep_expired_parked` race isekai-ssh's give-up and turn the second
    // phase's clean-give-up assertions into flaky `REJECT_UNKNOWN_SESSION`
    // failures instead).
    const RESUME_WINDOW_SECS: u64 = 75;
    const FIRST_OUTAGE_SECS: u64 = 45;
    const CHUNK_SIZE: usize = 8 * 1024;

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

    // Every chunk ever successfully echoed, in order — compared byte-for-byte
    // against isekai-ssh's *entire* accumulated stdout once the process has
    // exited, at the very end of this test.
    let mut expected_stdout: Vec<u8> = Vec::new();
    let mut next_chunk_id: u32 = 0;
    let mut send_and_verify_chunk = |stdin: &mut std::process::ChildStdin,
                                      reader: &mut StdoutReader,
                                      expected_stdout: &mut Vec<u8>,
                                      timeout: Duration|
     -> bool {
        let chunk = make_chunk(next_chunk_id, CHUNK_SIZE);
        next_chunk_id += 1;
        let ok = echo_round_trip(stdin, reader, &chunk, timeout);
        if ok {
            expected_stdout.extend_from_slice(&chunk);
        }
        ok
    };

    // === Phase 0: prove the relay works end-to-end before touching anything.
    for i in 0..3 {
        assert!(
            send_and_verify_chunk(&mut stdin, &mut stdout_reader, &mut expected_stdout, Duration::from_secs(15)),
            "pre-outage chunk {i} round trip failed"
        );
    }

    // === Phase 1: a recoverable outage (mirrors `resume_reconnect_e2e.rs`).
    // Genuinely drop every datagram in both directions for a while, then let
    // them through again. Both sides' independent idle-timeouts should fire
    // within their documented range, after which isekai-ssh's resume loop
    // (`connect.rs::run_relay_resumable`) starts retrying `RESUME` against
    // isekai-helper — which, once unblocked, should still have the session
    // parked (`RESUME_WINDOW_SECS` comfortably covers `FIRST_OUTAGE_SECS`).
    proxy.block();
    tokio::time::sleep(Duration::from_secs(FIRST_OUTAGE_SECS)).await;
    proxy.unblock();

    // Wait for the relay to come back to life over the *same* stdin/stdout
    // pipes (`ssh`'s ProxyCommand contract: isekai-ssh must never have closed
    // them) — proving `RESUME`/`RESUME_ACK` succeeded and the byte relay
    // continued, with the first post-resume chunk verified byte-for-byte
    // against its expected deterministic content (not just "something came
    // back").
    assert!(
        send_and_verify_chunk(&mut stdin, &mut stdout_reader, &mut expected_stdout, Duration::from_secs(60)),
        "relay did not resume within the expected window after the first (recoverable) outage ended"
    );

    // A few more chunks well after resume, to prove the relay is genuinely
    // back to steady-state operation (not just transiently replaying a
    // buffered response) and that `run_relay_resumable`'s post-resume state
    // reset (`disconnected_since = None; attempt = 0;`) didn't leave anything
    // behind that would corrupt subsequent traffic.
    for i in 0..3 {
        assert!(
            send_and_verify_chunk(&mut stdin, &mut stdout_reader, &mut expected_stdout, Duration::from_secs(15)),
            "post-resume steady-state chunk {i} round trip failed"
        );
    }

    // Sanity check on isekai-ssh's own stderr: it must actually have logged a
    // successful resume before phase 2 begins — otherwise the "recovers from
    // one outage" half of this test's name would be true only by accident
    // (e.g. the outage never actually tripped either side's idle-timeout).
    {
        let stderr_text = isekai_ssh_stderr.lock().unwrap().clone();
        assert!(
            stderr_text.contains("resume succeeded"),
            "expected isekai-ssh to have logged a successful resume after the first outage, got stderr:\n{stderr_text}"
        );
    }

    // === Phase 2: a second, *permanent* outage (mirrors
    // `resume_window_exceeded_e2e.rs`). This is the crux of what this test
    // adds over the two sibling tests run in isolation: proving the give-up
    // path still works correctly on a process that has *already* resumed
    // once before — i.e. that the first outage's bookkeeping doesn't leak
    // into (or shorten/lengthen) the second outage's own resume-window
    // countdown.
    proxy.block();

    // Deliberately do **not** close our end of the child's stdin here — keep
    // it open, exactly like a real `ssh` ProxyCommand parent that has no
    // reason to think the session is over yet
    // (`resume_window_exceeded_e2e.rs`'s rationale: this is what actually
    // exercises the `std::process::exit`-based fix for the orphaned
    // blocking-stdin-read thread, `main.rs`).
    let _stdin = stdin;

    // Generous bound: idle-timeout detection alone can take up to ~40s worst
    // case (`HELPER_PROTOCOL.md` §7.5), plus `RESUME_WINDOW_SECS` (75s) of
    // retrying, plus at least one full resume attempt that can run past the
    // deadline before the loop notices — this assertion is about "does it
    // ever finish" (no hang), not about being tight to the second.
    let status = wait_with_timeout(&mut child, Duration::from_secs(240))
        .await
        .expect(
            "isekai-ssh connect did not exit within 240s of the second, permanent disconnect — it appears to be \
             hanging",
        );

    // Exit must be clean (`run_relay_resumable` returns `Ok(())`, which
    // `main.rs` maps to `ExitCode::SUCCESS`), not a crash/panic.
    assert!(status.success(), "isekai-ssh connect should exit successfully (Ok(())) when it gives up, got {status:?}");

    // stdout purity + byte-exactness across the *entire* run: the only bytes
    // ever written to stdout are the seven successfully-echoed chunks from
    // phases 0-1, in order, with nothing extra, missing, or corrupted —
    // proving neither outage (recoverable or permanent) caused any loss,
    // duplication, or corruption anywhere in the C2H/H2C pipeline, and that
    // the give-up path itself never wrote anything to stdout.
    let total_stdout = stdout_reader.total_bytes_received();
    assert_eq!(
        total_stdout.len(),
        expected_stdout.len(),
        "stdout length mismatch: expected {} bytes across all successfully-echoed chunks, got {}",
        expected_stdout.len(),
        total_stdout.len()
    );
    assert_eq!(
        total_stdout, expected_stdout,
        "stdout must contain exactly the concatenation of every chunk echoed before the permanent outage, \
         byte-for-byte and in order, and nothing else"
    );

    // The give-up path's stderr message (`connect.rs::run_relay_resumable`)
    // must actually have been printed for the *second* outage.
    let stderr_text = isekai_ssh_stderr.lock().unwrap().clone();
    assert!(
        stderr_text.contains("giving up") && stderr_text.contains("resume window"),
        "expected an actionable give-up message on isekai-ssh's stderr, got:\n{stderr_text}"
    );

    // (Best-effort, matching `resume_window_exceeded_e2e.rs`'s acceptance
    // criterion): isekai-helper's own `sweep_expired_parked` should also have
    // actually discarded the parked session by now — proving the give-up
    // isn't just isekai-ssh's own client-side illusion.
    let helper_stderr_text = helper.stderr_log.lock().unwrap().clone();
    assert!(
        helper_stderr_text.contains("expired while parked, discarded"),
        "expected isekai-helper's own sweep_expired_parked to have discarded the parked session by now, got \
         isekai-helper stderr:\n{helper_stderr_text}"
    );

    let _ = child.kill();
    let _ = child.wait();
}
