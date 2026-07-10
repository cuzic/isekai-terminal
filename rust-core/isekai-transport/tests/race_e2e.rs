//! End-to-end tests for `race_direct_and_relay` (`#19`: direct(STUN
//! P2P)/relay minimal two-path race) against real local mock STUN/QUIC
//! servers, mirroring `stun_p2p_e2e.rs`'s and `relay_e2e.rs`'s techniques
//! (this crate's convention: one self-contained e2e file per scenario,
//! duplicating mock-server helpers rather than sharing them).
//!
//! Scenarios: direct wins when it completes within the stagger window
//! (relay never even starts); relay wins when direct keeps failing (STUN
//! unreachable). Proving the "same underlying helper, one AttachArbiter,
//! never double-attaches" safety property itself is `#18-7`'s job
//! (`isekai-pipe/tests/fencing_e2e.rs`, against the real server) — these
//! tests are scoped to the client-side race *mechanism* (staggered start,
//! correct winner selection, the loser's future actually getting dropped).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use isekai_protocol::attach::{
    attach_hello_proof_transcript, decode_attach_activate, decode_attach_hello, encode_attach_response, AttachProof,
    AttachResponse, AttachToken, ATTACH_ACTIVATE_FRAME_LEN, ATTACH_HELLO_FRAME_LEN,
};
use isekai_protocol::hello::{ALPN, EXPORTER_LABEL};
use isekai_transport::{
    race_direct_and_relay, DirectRelayRaceTargets, RaceWinner, RelayTarget, StunP2pTarget, SystemQuicEndpointFactory,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const SNI: &str = "isekai-pipe.local";
const SESSION_SECRET: &[u8] = b"race-e2e-test-session-secret";

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

fn generate_cert() -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>, String) {
    // The `qmux-relay` feature links `aws-lc-rs` alongside noq's own
    // `ring`, so rustls can no longer auto-select a single process-wide
    // crypto provider when this crate is built with that feature on —
    // every test in this file calls `generate_cert` first, so fixing it
    // once here covers all of them.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cert = rcgen::generate_simple_self_signed(vec![SNI.to_string()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut hasher = Sha256::new();
    hasher.update(cert_der.as_ref());
    let sha256_hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    (cert_der, key_der, sha256_hex)
}

fn mock_server(cert_der: CertificateDer<'static>, key_der: PrivatePkcs8KeyDer<'static>) -> noq::Endpoint {
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())
        .unwrap();
    tls_config.alpn_protocols = vec![ALPN.to_vec()];
    let quic_crypto = noq::crypto::rustls::QuicServerConfig::try_from(tls_config).unwrap();
    let config = noq::ServerConfig::with_crypto(Arc::new(quic_crypto));
    noq::Endpoint::server(config, SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap()
}

/// Accepts one connection, verifies the ATTACH_HELLO proof, replies
/// AttachReadyV2, reads AttachActivate, then echoes one message — used for
/// both the "peer" (direct/STUN) and "helper" (relay) mock servers, since
/// both speak the exact same ATTACH v2 wire protocol once a QUIC connection
/// exists (mirrors `relay_e2e.rs::run_mock_helper`/`stun_p2p_e2e.rs::run_mock_peer`).
async fn run_full_mock(endpoint: noq::Endpoint, contacted: Option<Arc<AtomicBool>>) {
    let incoming = endpoint.accept().await.unwrap();
    let conn = incoming.await.unwrap();
    if let Some(flag) = &contacted {
        flag.store(true, Ordering::SeqCst);
    }
    let (mut send, mut recv) = conn.accept_bi().await.unwrap();

    let mut hello_bytes = [0u8; ATTACH_HELLO_FRAME_LEN];
    recv.read_exact(&mut hello_bytes).await.unwrap();
    let hello = decode_attach_hello(&hello_bytes).unwrap();

    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"").unwrap();
    let transcript = attach_hello_proof_transcript(
        &hello.session_id,
        hello.generation,
        &hello.attempt_id,
        hello.requested_resume_grace_secs,
    );
    let mut mac = HmacSha256::new_from_slice(SESSION_SECRET).unwrap();
    mac.update(&exporter);
    mac.update(&transcript);
    let expected = AttachProof::new(mac.finalize().into_bytes().into());
    assert!(hello.proof.ct_eq(&expected), "test setup bug: session secret mismatch");

    let ready = AttachResponse::Ready {
        session_id: hello.session_id,
        generation: hello.generation,
        attempt_id: hello.attempt_id,
        negotiated_resume_grace_secs: 0,
        attach_token: AttachToken::new(rand::random()),
    };
    send.write_all(&encode_attach_response(&ready)).await.unwrap();

    let mut activate_bytes = [0u8; ATTACH_ACTIVATE_FRAME_LEN];
    recv.read_exact(&mut activate_bytes).await.unwrap();
    decode_attach_activate(&activate_bytes).unwrap();

    let mut buf = [0u8; 64];
    if let Ok(Some(n)) = recv.read(&mut buf).await {
        send.write_all(&buf[..n]).await.unwrap();
    }
    send.finish().ok();
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn direct_wins_when_it_completes_within_the_stagger_window() {
    let stun_server = spawn_mock_stun_server().await;

    let (direct_cert_der, direct_key_der, direct_cert_hex) = generate_cert();
    let direct_endpoint = mock_server(direct_cert_der, direct_key_der);
    let direct_addr = direct_endpoint.local_addr().unwrap();
    let direct_task = tokio::spawn(run_full_mock(direct_endpoint, None));

    let (relay_cert_der, relay_key_der, relay_cert_hex) = generate_cert();
    let relay_endpoint = mock_server(relay_cert_der, relay_key_der);
    let relay_addr = relay_endpoint.local_addr().unwrap();
    let relay_contacted = Arc::new(AtomicBool::new(false));
    let relay_task = tokio::spawn(run_full_mock(relay_endpoint, Some(relay_contacted.clone())));

    let targets = DirectRelayRaceTargets {
        stun_server,
        direct: StunP2pTarget {
            peer_addr: direct_addr,
            server_name: SNI.to_string(),
            cert_sha256_hex: direct_cert_hex,
            session_secret: SESSION_SECRET.to_vec(),
        },
        relay: RelayTarget {
            helper_addr: relay_addr,
            server_name: SNI.to_string(),
            cert_sha256_hex: relay_cert_hex,
            session_secret: SESSION_SECRET.to_vec(),
        },
    };

    let factory = SystemQuicEndpointFactory;
    // Generous stagger relative to how long the direct path actually takes
    // on loopback (STUN query + 5x150ms hole-punch probes + QUIC handshake,
    // well under a second) — this proves direct winning *within* the
    // stagger window, not merely winning the eventual race.
    let mut outcome = tokio::time::timeout(
        Duration::from_secs(15),
        race_direct_and_relay(&factory, &targets, Duration::from_secs(3)),
    )
    .await
    .expect("should not hang")
    .expect("direct should win");
    assert_eq!(outcome.winner, RaceWinner::Direct);
    assert!(!relay_contacted.load(Ordering::SeqCst), "relay must never be contacted if direct wins within the stagger window");

    // The mock server's `recv.read(..)` blocks waiting for this echo payload
    // before it finishes and its own task completes — without this, awaiting
    // `direct_task` below would hang until the QUIC connection's idle
    // timeout, not a quick, deterministic completion.
    outcome.stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), outcome.stream.read(&mut buf))
        .await
        .expect("timed out waiting for echo")
        .unwrap();
    assert_eq!(&buf[..n], b"ping");

    tokio::time::timeout(Duration::from_secs(5), direct_task).await.expect("direct mock server task should finish promptly").unwrap();
    relay_task.abort();
}

#[tokio::test]
async fn relay_wins_when_direct_keeps_failing() {
    // Nothing listens here: bind-then-drop so the STUN query direct depends
    // on fails (mirrors `stun_p2p_e2e.rs`'s "unreachable STUN server" test).
    let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dead_stun_server = probe.local_addr().unwrap();
    drop(probe);

    let (relay_cert_der, relay_key_der, relay_cert_hex) = generate_cert();
    let relay_endpoint = mock_server(relay_cert_der, relay_key_der);
    let relay_addr = relay_endpoint.local_addr().unwrap();
    let relay_task = tokio::spawn(run_full_mock(relay_endpoint, None));

    let targets = DirectRelayRaceTargets {
        stun_server: dead_stun_server,
        direct: StunP2pTarget {
            peer_addr: "127.0.0.1:1".parse().unwrap(), // never actually reached
            server_name: SNI.to_string(),
            cert_sha256_hex: "0".repeat(64),
            session_secret: SESSION_SECRET.to_vec(),
        },
        relay: RelayTarget {
            helper_addr: relay_addr,
            server_name: SNI.to_string(),
            cert_sha256_hex: relay_cert_hex,
            session_secret: SESSION_SECRET.to_vec(),
        },
    };

    let factory = SystemQuicEndpointFactory;
    let mut outcome = tokio::time::timeout(
        Duration::from_secs(15),
        race_direct_and_relay(&factory, &targets, Duration::from_millis(300)),
    )
    .await
    .expect("should not hang")
    .expect("relay should win once direct's STUN query keeps failing");
    assert_eq!(outcome.winner, RaceWinner::Relay);

    // Same reason as the other test: let the mock server's echo step
    // complete so it finishes and awaiting its task doesn't hang.
    outcome.stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), outcome.stream.read(&mut buf))
        .await
        .expect("timed out waiting for echo")
        .unwrap();
    assert_eq!(&buf[..n], b"ping");

    tokio::time::timeout(Duration::from_secs(5), relay_task).await.expect("relay mock server task should finish promptly").unwrap();
}
