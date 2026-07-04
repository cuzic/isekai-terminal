//! End-to-end test proving `isekai-ssh connect` (`--mode relay`, the default)
//! actually resumes across a real disconnect (`ISEKAI_SSH_DESIGN.md` Phase
//! S-4c's acceptance criterion #1: "実際に動くe2eテスト...QUIC接続を確立した
//! 後に意図的に切断し...isekai-ssh側が再接続を試み、RESUME/RESUME_ACKを経て
//! 中継が再開されることを確認する").
//!
//! Nothing here is a type-checking-only mock: this spawns the actual compiled
//! `isekai-ssh` binary (`--dev-insecure-*`, same bypass `connect_e2e.rs` uses
//! since the trust store isn't wired into this test) as a real OS subprocess,
//! and the actual compiled `isekai-helper` binary, relaying to a real TCP
//! echo server. The "disconnect" is real too: `isekai-ssh` is pointed at a
//! small UDP "blackhole proxy" (`spawn_blackhole_proxy`, this test's own
//! plumbing — no fault-injection hook is added to production code) sitting
//! in front of the real isekai-helper, and the test toggles it to drop every
//! datagram in both directions for a while, then let them through again —
//! genuinely losing packets, the same thing a real network partition does,
//! rather than freezing a process.
//!
//! **Why not just `SIGSTOP`/`SIGCONT` the isekai-helper process** (an earlier
//! version of this test did that): a frozen process's kernel socket receive
//! buffer keeps *queuing* incoming UDP datagrams (they aren't dropped, just
//! unprocessed) — so on `SIGCONT`, isekai-helper suddenly processes a burst
//! of stale-but-still-queued packets (old keepalives, old connection
//! attempts) all at once. Because QUIC's idle-timeout is based on "time since
//! the last packet was *processed*", not when it was sent, replaying that
//! backlog can repeatedly look like fresh activity and unpredictably delay
//! isekai-helper's own idle-timeout detection for the old connection well
//! past `--idle-timeout`, which in practice made the old session take far
//! longer than expected to get parked as resumable — a `SIGSTOP`-specific
//! artifact, not something a real network outage (where packets are actually
//! lost, not queued-and-replayed) would produce. A real UDP blackhole avoids
//! this: dropped packets are just gone, so both sides' idle-timeouts fire
//! close to their configured values, matching real-world behavior.
//!
//! Skips itself when `isekai-helper`'s binary can't be located/built, mirroring
//! `connect_e2e.rs`'s convention (it doesn't need `ssh(1)` itself, since this
//! test drives `isekai-ssh connect` directly rather than through a real
//! `ssh` process — the resume behavior under test lives entirely inside
//! `isekai-ssh`, with no OpenSSH involvement).
//!
//! Requires the `dev-insecure` feature: `cargo test -p isekai-ssh --features
//! dev-insecure --test resume_reconnect_e2e`.
//!
//! This test is inherently slow (the blackhole window is held longer than
//! isekai-ssh's own idle-timeout detection window on both sides) — expect it
//! to take on the order of a minute.

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
// Shared plumbing (small, deliberately duplicated subset of
// `connect_e2e.rs`'s helpers of the same name/shape — integration test
// binaries in `tests/*.rs` each compile as independent crates and can't
// `use` each other's items, and pulling this into a `tests/common/` module
// shared via `#[path]` wasn't worth the churn for this little code).
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
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawns the real compiled `isekai-helper` binary, targeting a real TCP
/// listener (`target_addr`) — no SSH/`sshd` involved, since this test is
/// about isekai-ssh's byte-stream resume, not SSH itself (already covered by
/// `connect_e2e.rs`). Never frozen/signaled by this test (unlike an earlier
/// version — see module docs); it runs continuously and detects the
/// simulated outage entirely on its own via its normal `--idle-timeout`.
fn spawn_helper(target_addr: SocketAddr) -> HelperProcess {
    let mut cmd = Command::new(isekai_helper_bin_path());
    cmd.arg("--target")
        .arg(target_addr.to_string())
        .arg("--bind")
        .arg("127.0.0.1:0")
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

    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut r = BufReader::new(stderr);
            let mut buf = String::new();
            loop {
                buf.clear();
                if r.read_line(&mut buf).unwrap_or(0) == 0 {
                    break;
                }
                eprint!("[isekai-helper] {buf}");
            }
        });
    }
    std::mem::forget(reader);

    HelperProcess { child, handshake }
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
/// (usually `sshd`) — isekai-ssh's resume logic operates purely on opaque
/// bytes and has no idea (or need to know) that real SSH traffic isn't
/// flowing, so a plain echo is sufficient and far simpler than standing up a
/// real `russh::server` (already exercised for the non-resume path by
/// `connect_e2e.rs`).
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
/// isekai-helper. While `enabled` is `true` (the default), every datagram is
/// forwarded transparently in both directions — isekai-ssh and isekai-helper
/// behave exactly as if talking directly. While `false`, every datagram
/// arriving on either side is silently dropped, genuinely simulating a lost
/// network path rather than a frozen peer (see module docs for why that
/// distinction matters here).
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

/// Binds a fresh local UDP socket that proxies to `upstream` (isekai-helper's
/// real QUIC listen address). Returns the address isekai-ssh should be told
/// to connect to instead, plus the `BlackholeProxy` handle to toggle
/// forwarding. Since there is exactly one client (isekai-ssh) for the
/// lifetime of this test, tracking "the most recently seen client address"
/// is sufficient to know where to relay isekai-helper's replies back to —
/// this doesn't need to be a general-purpose NAT-style proxy.
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

/// Background reader for a child's stdout that makes reads-with-a-deadline
/// possible: `std::process::ChildStdout::read` blocks indefinitely with no
/// timeout support, so a dedicated thread continuously reads into a channel,
/// and callers pull from that channel with `recv_timeout` instead of reading
/// the pipe directly.
struct StdoutReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    leftover: Vec<u8>,
}

impl StdoutReader {
    fn spawn(mut stdout: std::process::ChildStdout) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self { rx, leftover: Vec::new() }
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
}

/// Writes `payload` to `stdin` and reads exactly `payload.len()` bytes back
/// via `reader` within `timeout` — proves the byte relay is actually alive
/// end-to-end (isekai-ssh -> isekai-helper -> TCP echo -> isekai-helper ->
/// isekai-ssh) at the point it's called, not just that the process is
/// running.
fn echo_round_trip(stdin: &mut impl Write, reader: &mut StdoutReader, payload: &[u8], timeout: Duration) -> bool {
    if stdin.write_all(payload).is_err() || stdin.flush().is_err() {
        return false;
    }
    match reader.read_exact_timeout(payload.len(), timeout) {
        Some(received) => received == payload,
        None => false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_resumes_after_a_simulated_network_outage() {
    let target_addr = spawn_tcp_echo_server().await;
    let helper = spawn_helper(target_addr);
    let helper_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port).parse().unwrap();
    let (proxy_addr, proxy) = spawn_blackhole_proxy(helper_addr).await;

    let mut child = Command::new(isekai_ssh_bin_path())
        .arg("connect")
        .arg("dummy-host")
        .args(dev_insecure_args(proxy_addr, &helper.handshake))
        .env("RUST_LOG", "isekai_ssh=debug,isekai_transport=debug")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn isekai-ssh connect");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout_reader = StdoutReader::spawn(child.stdout.take().unwrap());

    // Drain stderr in the background so isekai-ssh never blocks on a full
    // pipe, and so we can eyeball its resume-related log lines for
    // debugging a failure.
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        let mut r = BufReader::new(stderr);
        let mut buf = String::new();
        loop {
            buf.clear();
            if r.read_line(&mut buf).unwrap_or(0) == 0 {
                break;
            }
            eprint!("[isekai-ssh] {buf}");
        }
    });

    // 1. Prove the relay works end-to-end (through the proxy, forwarding
    // normally) before touching anything.
    assert!(
        echo_round_trip(&mut stdin, &mut stdout_reader, b"hello-before-disconnect\n", Duration::from_secs(15)),
        "initial echo round trip (before any disconnect) failed"
    );

    // 2. Blackhole the path. isekai-ssh keeps running; every packet it sends
    // (including QUIC keepalives) is now silently dropped, and every packet
    // isekai-helper sends back is dropped too — genuinely indistinguishable
    // from a real network partition from either side's point of view. Both
    // sides' independent idle-timeouts (`CLIENT_MAX_IDLE_TIMEOUT`/
    // `--idle-timeout`, both 15s by default) should fire within their normal
    // documented range (`HELPER_PROTOCOL.md` §7.5 notes up to ~40s worst
    // case for the client side), with no `SIGSTOP`-style backlog artifact to
    // delay isekai-helper parking the session afterward.
    proxy.block();

    // Comfortably past the documented worst-case detection time on both
    // sides, well under `--resume-window`'s 120s default.
    tokio::time::sleep(Duration::from_secs(45)).await;

    // 3. Let packets flow again. isekai-helper's session should already be
    // parked (it never stopped running, so its own idle-timeout fired
    // on schedule during step 2) — the next `RESUME` isekai-ssh sends should
    // succeed without the repeated "not resumable (no parked TCP
    // connection)" races the `SIGSTOP`-based approach hit.
    proxy.unblock();

    // 4. Wait for the relay to come back to life over the *same* stdin/stdout
    // pipes (`ssh`'s ProxyCommand contract: isekai-ssh must never have closed
    // them) — proving `RESUME`/`RESUME_ACK` succeeded and the byte relay
    // continued. Deliberately a **single** write followed by a single
    // long-timeout read, not a write-then-short-read retry loop: while
    // isekai-ssh is still mid reconnect, `pump_c2h` isn't running at all
    // (`run_relay_resumable` is in its resume loop, not `run_data_pump`), so
    // anything written to stdin during that window just queues up unread in
    // the OS pipe. A retry loop that writes a *new, different* payload on
    // every attempt would let several such payloads queue up back-to-back
    // before isekai-ssh resumes pumping, and then read back a misaligned
    // prefix of that backlog instead of the specific payload just written.
    // `echo_round_trip`'s single `read_exact_timeout` call already loops
    // internally accumulating bytes over however long it takes, up to
    // `timeout`, which is exactly the right shape here.
    assert!(
        echo_round_trip(&mut stdin, &mut stdout_reader, b"hello-after-resume\n", Duration::from_secs(45)),
        "relay did not resume within the expected window after the simulated outage ended"
    );

    // 5. One more round trip, well after resume, to prove the relay isn't
    // just transiently working (e.g. replaying a buffered response) but is
    // genuinely back to normal operation.
    assert!(
        echo_round_trip(&mut stdin, &mut stdout_reader, b"hello-steady-state-after-resume\n", Duration::from_secs(15)),
        "post-resume steady-state echo round trip failed"
    );

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
}
