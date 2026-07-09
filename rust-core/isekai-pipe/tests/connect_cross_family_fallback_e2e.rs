//! End-to-end test for `ConnectionIntent::cross_family_fallback`
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic I's `I-route-scheduler`, ordered-fallback
//! half): when the primary transport is `IntentTransport::StunP2p` and it
//! fails entirely (here: an unreachable STUN server, so the failure surfaces
//! before ever reaching the peer), `isekai-pipe connect` retries once via
//! the `Relay` transport named in `cross_family_fallback` instead of giving
//! up — a *different* transport family, not the same-family
//! `--stun-server`-list fallback `connect_stun_fallback_e2e.rs` already
//! covers.
//!
//! Unlike `connect_stun_fallback_e2e.rs` (which drives `connect` via
//! `--profile`/`--mode stun` CLI flags — a path that never sets
//! `cross_family_fallback`, since only `isekai-ssh/src/wrapper.rs::
//! build_connection_intent` does), this test writes a `ConnectionIntent`
//! directly and hands it off via `ISEKAI_INTENT_ID`, mirroring how the
//! wrapper actually invokes `isekai-pipe connect` in production
//! (`wrapper.rs::run`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

use isekai_pipe_core::{write_connection_intent, BootstrapProvenance, ConnectionIntent, IntentTransport, ServerIdentity};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command as TokioCommand;

fn isekai_pipe_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-pipe"))
}

#[derive(Deserialize)]
struct Handshake {
    session_secret: String,
    peer: HandshakePeer,
    #[serde(default)]
    candidates: Vec<HandshakeCandidate>,
}

#[derive(Deserialize)]
struct HandshakePeer {
    server_identity: HandshakeServerIdentity,
}

#[derive(Deserialize)]
struct HandshakeServerIdentity {
    cert_sha256: String,
}

#[derive(Deserialize)]
struct HandshakeCandidate {
    kind: String,
    #[serde(default)]
    port: Option<u16>,
}

impl Handshake {
    fn direct_by_bootstrap_host_port(&self) -> Option<u16> {
        self.candidates.iter().find(|c| c.kind == "direct-by-bootstrap-host").and_then(|c| c.port)
    }
}

/// Duplicated from `connect_stun_fallback_e2e.rs` per this crate's
/// self-contained-test-file convention.
async fn spawn_echo_server() -> SocketAddr {
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

/// Duplicated from `connect_stun_fallback_e2e.rs`: a real `isekai-pipe
/// serve` process reachable directly on loopback, used here as the
/// cross-family *fallback*'s (relay) target.
async fn spawn_real_peer(echo_addr: SocketAddr) -> (tokio::process::Child, Handshake) {
    let mut cmd = TokioCommand::new(isekai_pipe_bin_path());
    cmd.args(["serve", "--target", &echo_addr.to_string(), "--bind", "127.0.0.1:0", "--log-level", "debug"])
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped());
    let mut child = cmd.spawn().expect("failed to spawn isekai-pipe serve");

    let stdout = child.stdout.take().unwrap();
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut line = String::new();
    tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
        .await
        .expect("failed to read handshake line from isekai-pipe serve stdout");
    let handshake: Handshake = serde_json::from_str(line.trim()).expect("failed to parse handshake JSON");

    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut r = tokio::io::BufReader::new(stderr);
            let mut buf = String::new();
            loop {
                buf.clear();
                if tokio::io::AsyncBufReadExt::read_line(&mut r, &mut buf).await.unwrap_or(0) == 0 {
                    break;
                }
            }
        });
    }

    (child, handshake)
}

/// A bound-then-dropped UDP port: nothing is listening, so `isekai_stun::
/// query_stun` fails fast (retries then gives up) instead of hanging.
async fn dead_stun_server() -> SocketAddr {
    let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_falls_back_from_stun_to_a_different_family_relay_and_completes_a_real_byte_roundtrip() {
    let echo_addr = spawn_echo_server().await;
    let (mut peer_child, handshake) = spawn_real_peer(echo_addr).await;
    let peer_port = handshake.direct_by_bootstrap_host_port().expect("serve should report a direct-by-bootstrap-host port");
    let peer_addr: SocketAddr = format!("127.0.0.1:{peer_port}").parse().unwrap();

    let dead_stun = dead_stun_server().await;

    let mut intent = ConnectionIntent::new(
        "cross-family-host",
        "ssh",
        ServerIdentity { cert_sha256_hex: handshake.peer.server_identity.cert_sha256.clone() },
        // Primary: STUN P2P against an unreachable peer/STUN server —
        // guaranteed to fail entirely (never reaches the real peer at all).
        IntentTransport::StunP2p {
            stun_server: dead_stun.to_string(),
            peer_addr: "127.0.0.1:1".to_string(),
            server_name: "isekai-helper".to_string(),
            session_secret_b64: handshake.session_secret.clone(),
        },
        BootstrapProvenance::ExplicitProfile,
    );
    // Fallback: the real relay-reachable peer this test actually spawned.
    intent.cross_family_fallback = Some(IntentTransport::Relay {
        helper_addr: peer_addr.to_string(),
        server_name: "isekai-helper".to_string(),
        session_secret_b64: handshake.session_secret.clone(),
    });

    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("runtime");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    write_connection_intent(&runtime_dir, &intent).expect("failed to write ConnectionIntent fixture");

    let mut connect_cmd = TokioCommand::new(isekai_pipe_bin_path());
    connect_cmd
        .args(["connect", "--stdio"])
        .env("ISEKAI_INTENT_ID", &intent.intent_id)
        .env("ISEKAI_PIPE_RUNTIME_DIR", &runtime_dir)
        .env_remove("RUST_LOG")
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped());
    let mut connect_child = connect_cmd.spawn().expect("failed to spawn isekai-pipe connect");

    let mut stdin = connect_child.stdin.take().unwrap();
    let mut stdout = connect_child.stdout.take().unwrap();
    if let Some(stderr) = connect_child.stderr.take() {
        tokio::spawn(async move {
            let mut r = tokio::io::BufReader::new(stderr);
            let mut buf = String::new();
            loop {
                buf.clear();
                if tokio::io::AsyncBufReadExt::read_line(&mut r, &mut buf).await.unwrap_or(0) == 0 {
                    break;
                }
                eprint!("[isekai-pipe connect stderr] {buf}");
            }
        });
    }

    stdin.write_all(b"ping-through-cross-family-fallback").await.unwrap();

    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(20), stdout.read(&mut buf))
        .await
        .expect("connect should fall back to relay and echo back the byte round-trip within the timeout")
        .expect("reading from connect's stdout failed");
    assert_eq!(&buf[..n], b"ping-through-cross-family-fallback");

    let _ = connect_child.start_kill();
    let _ = connect_child.wait().await;
    let _ = peer_child.start_kill();
    let _ = peer_child.wait().await;
}
