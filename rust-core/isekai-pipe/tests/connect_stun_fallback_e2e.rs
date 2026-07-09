//! End-to-end test for `isekai-pipe connect --mode stun` with *multiple*
//! `--stun-server` flags (`#11`): proves the whole fallback pipeline
//! (`ConfigStunProvider` → `CandidatePool` → `connect_stun_p2p_with_fallback`)
//! using two real compiled `isekai-pipe` processes — a real `serve` instance
//! as the peer, and a real `connect` instance dialing it — exactly like a
//! real deployment, rather than mocking either side.
//!
//! The first configured STUN server is unreachable; the connector must fall
//! back to the second (a real mock STUN responder) and still complete a
//! genuine SSH-stdio byte round-trip through `serve`'s `--target` (a real TCP
//! echo server).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

use isekai_pipe_core::PersistentProfile;
use isekai_trust::schema::{HelperTrust, UpdatePolicy};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command as TokioCommand;

fn isekai_pipe_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-pipe"))
}

fn profiles_dir_under(home: &std::path::Path) -> PathBuf {
    home.join(".local").join("state").join("isekai").join("profiles")
}

fn register_trust(home: &std::path::Path, host: &str, entry: HelperTrust) {
    let key = isekai_trust::normalize_host_port(host).unwrap();
    let profile = PersistentProfile::migrate_legacy_helper_trust(&key, &entry);
    isekai_pipe_core::write_persistent_profile(&profiles_dir_under(home), &profile)
        .expect("failed to write persistent profile fixture");
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

/// Spawns a real `isekai-pipe serve` process (no `--stun-server` needed on
/// this side — the peer's `direct-by-bootstrap-host` candidate is reachable
/// directly on loopback) targeting `echo_addr`, and returns its handshake
/// once the one-line JSON has been read from stdout.
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

    // Drain stderr on a background task so `serve` never blocks on a full pipe.
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

/// A minimal mock STUN server (RFC 5389 Binding Request/Response), same
/// shape as every other mock STUN helper in this workspace
/// (`isekai-pipe/tests/serve_e2e.rs`, `isekai-transport/tests/stun_p2p_e2e.rs`).
fn spawn_mock_stun_server() -> SocketAddr {
    let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        loop {
            let Ok((n, from)) = server.recv_from(&mut buf) else { break };
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

            let _ = server.send_to(&resp, from);
        }
    });
    addr
}

async fn dead_stun_server() -> SocketAddr {
    let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_falls_back_past_an_unreachable_stun_server_and_completes_a_real_byte_roundtrip() {
    let echo_addr = spawn_echo_server().await;
    let (mut peer_child, handshake) = spawn_real_peer(echo_addr).await;
    let peer_port = handshake.direct_by_bootstrap_host_port().expect("serve should report a direct-by-bootstrap-host port");
    let peer_addr: SocketAddr = format!("127.0.0.1:{peer_port}").parse().unwrap();

    let dead_stun = dead_stun_server().await;
    let real_stun = spawn_mock_stun_server();

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    register_trust(
        &home,
        "stun-fallback-host",
        HelperTrust {
            identity_pubkey: "pk-test".to_string(),
            trusted_helper_sha256: "a".repeat(64),
            trusted_helper_version: "0.0.0-test".to_string(),
            update_policy: UpdatePolicy::ExactDigestOnly,
            release_channel: None,
            last_via: None,
            trusted_at: "2026-07-08T00:00:00Z".to_string(),
            last_seen_at: "2026-07-08T00:00:00Z".to_string(),
            cached_relay_addr: peer_addr.to_string(),
            cached_cert_sha256: handshake.peer.server_identity.cert_sha256.clone(),
            cached_session_secret: handshake.session_secret.clone(),
            cached_stun_observed_addr: None,
        },
    );

    let mut connect_cmd = TokioCommand::new(isekai_pipe_bin_path());
    connect_cmd
        .args([
            "connect",
            "--profile",
            "stun-fallback-host",
            "--service",
            "ssh",
            "--stdio",
            "--mode",
            "stun",
            "--stun-server",
            &dead_stun.to_string(),
            "--stun-server",
            &real_stun.to_string(),
        ])
        .env("HOME", &home)
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
            }
        });
    }

    stdin.write_all(b"ping-through-stun-fallback").await.unwrap();

    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(15), stdout.read(&mut buf))
        .await
        .expect("connect should echo back the byte round-trip within the timeout")
        .expect("reading from connect's stdout failed");
    assert_eq!(&buf[..n], b"ping-through-stun-fallback");

    let _ = connect_child.start_kill();
    let _ = connect_child.wait().await;
    let _ = peer_child.start_kill();
    let _ = peer_child.wait().await;
}
