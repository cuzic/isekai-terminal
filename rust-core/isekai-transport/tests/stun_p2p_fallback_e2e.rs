//! End-to-end tests for `connect_stun_p2p_with_fallback` (`#11`): trying
//! several STUN-server candidates against the *same* peer in order, falling
//! back only when it's safe to (`AttemptFailure::may_retry_pre_fencing`).
//! Shares its mock STUN/peer server helpers with `stun_p2p_e2e.rs` by
//! duplication rather than a shared `tests/common` module — this crate's own
//! established convention (see `relay_fallback_e2e.rs`'s equivalent split
//! from `relay_e2e.rs`).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use isekai_protocol::attach::{
    attach_hello_proof_transcript, decode_attach_activate, decode_attach_hello, encode_attach_response, AttachProof,
    AttachRejectReason, AttachResponse, AttachToken, ATTACH_ACTIVATE_FRAME_LEN, ATTACH_HELLO_FRAME_LEN,
};
use isekai_protocol::hello::{ALPN, EXPORTER_LABEL};
use isekai_transport::{connect_stun_p2p_with_fallback, SequentialStunCandidate, SequentialStunConnectError, StunP2pTarget, SystemQuicEndpointFactory};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const SNI: &str = "isekai-pipe.local";

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

/// Binds then immediately drops a UDP socket, so its port is very unlikely to
/// have anything else answer on it — standing in for an unreachable/dead
/// STUN server (mirrors `stun_p2p_e2e.rs::connect_stun_p2p_fails_fast_when_the_stun_server_is_unreachable`).
async fn dead_stun_server() -> SocketAddr {
    let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    addr
}

fn generate_cert() -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>, String) {
    let cert = rcgen::generate_simple_self_signed(vec![SNI.to_string()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut hasher = Sha256::new();
    hasher.update(cert_der.as_ref());
    let sha256_hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    (cert_der, key_der, sha256_hex)
}

fn mock_peer_server(cert_der: CertificateDer<'static>, key_der: PrivatePkcs8KeyDer<'static>) -> noq::Endpoint {
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())
        .unwrap();
    tls_config.alpn_protocols = vec![ALPN.to_vec()];
    let quic_crypto = noq::crypto::rustls::QuicServerConfig::try_from(tls_config).unwrap();
    let config = noq::ServerConfig::with_crypto(Arc::new(quic_crypto));
    noq::Endpoint::server(config, SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap()
}

async fn run_mock_peer(endpoint: noq::Endpoint, session_secret: Vec<u8>, client_done: tokio::sync::oneshot::Receiver<()>) {
    let incoming = endpoint.accept().await.unwrap();
    let conn = incoming.await.unwrap();
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
    let mut mac = HmacSha256::new_from_slice(&session_secret).unwrap();
    mac.update(&exporter);
    mac.update(&transcript);
    let expected_bytes: [u8; 32] = mac.finalize().into_bytes().into();
    let expected = AttachProof::new(expected_bytes);

    if !hello.proof.ct_eq(&expected) {
        let reject = AttachResponse::Reject(AttachRejectReason::Auth);
        send.write_all(&encode_attach_response(&reject)).await.unwrap();
        send.finish().ok();
        client_done.await.ok();
        return;
    }

    let ready = AttachResponse::Ready {
        session_id: hello.session_id,
        generation: hello.generation,
        attempt_id: hello.attempt_id,
        negotiated_resume_grace_secs: hello.requested_resume_grace_secs,
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

    client_done.await.ok();
}

#[tokio::test]
async fn first_candidate_unreachable_stun_falls_back_to_second_and_succeeds() {
    let dead = dead_stun_server().await;
    let real_stun = spawn_mock_stun_server().await;

    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let endpoint = mock_peer_server(cert_der, key_der);
    let peer_addr = endpoint.local_addr().unwrap();
    let session_secret = b"stun-fallback-integration-test-secret".to_vec();
    let (client_done_tx, client_done_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(run_mock_peer(endpoint, session_secret.clone(), client_done_rx));

    let target = StunP2pTarget { peer_addr, server_name: SNI.to_string(), cert_sha256_hex, session_secret };
    let candidates = vec![
        SequentialStunCandidate { stun_server: dead, candidate_id: "stun-0".to_string() },
        SequentialStunCandidate { stun_server: real_stun, candidate_id: "stun-1".to_string() },
    ];

    let (mut conn, winning_stun_server) =
        tokio::time::timeout(Duration::from_secs(10), connect_stun_p2p_with_fallback(&SystemQuicEndpointFactory, &target, &candidates))
            .await
            .expect("connect_stun_p2p_with_fallback should not hang")
            .expect("should fall back past the dead STUN server to the real one");
    assert_eq!(winning_stun_server, real_stun);

    conn.stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 64];
    let n = conn.stream.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"ping");

    client_done_tx.send(()).ok();
    server_task.await.unwrap();
}

#[tokio::test]
async fn a_terminal_failure_on_the_first_candidate_stops_without_trying_the_second() {
    let real_stun = spawn_mock_stun_server().await;
    let second_stun = spawn_mock_stun_server().await;

    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let endpoint = mock_peer_server(cert_der, key_der);
    let peer_addr = endpoint.local_addr().unwrap();
    let (client_done_tx, client_done_rx) = tokio::sync::oneshot::channel();
    // The peer only ever knows "peer-side-secret" — every attempt against it
    // will be rejected with REJECT_AUTH regardless of which STUN server the
    // client used, so if fallback incorrectly tried the second candidate too,
    // the peer would see (and this test could detect) a second connection.
    let server_task = tokio::spawn(run_mock_peer(endpoint, b"peer-side-secret".to_vec(), client_done_rx));

    let target = StunP2pTarget {
        peer_addr,
        server_name: SNI.to_string(),
        cert_sha256_hex,
        session_secret: b"client-side-secret-does-not-match".to_vec(),
    };
    let candidates = vec![
        SequentialStunCandidate { stun_server: real_stun, candidate_id: "stun-0".to_string() },
        SequentialStunCandidate { stun_server: second_stun, candidate_id: "stun-1".to_string() },
    ];

    match tokio::time::timeout(Duration::from_secs(10), connect_stun_p2p_with_fallback(&SystemQuicEndpointFactory, &target, &candidates)).await {
        Ok(Err(SequentialStunConnectError::StoppedEarly { candidate_id, .. })) => {
            assert_eq!(candidate_id, "stun-0");
        }
        Ok(Ok(_)) => panic!("expected StoppedEarly on the first candidate, but the fallback succeeded"),
        Ok(Err(other)) => panic!("expected StoppedEarly on the first candidate, got: {other}"),
        Err(_) => panic!("connect_stun_p2p_with_fallback should not hang"),
    }

    client_done_tx.send(()).ok();
    server_task.await.unwrap();
}

#[tokio::test]
async fn all_candidates_unreachable_reports_every_failure() {
    let dead_1 = dead_stun_server().await;
    let dead_2 = dead_stun_server().await;

    let target = StunP2pTarget {
        peer_addr: "127.0.0.1:1".parse().unwrap(),
        server_name: SNI.to_string(),
        cert_sha256_hex: "0".repeat(64),
        session_secret: b"unused".to_vec(),
    };
    let candidates = vec![
        SequentialStunCandidate { stun_server: dead_1, candidate_id: "stun-0".to_string() },
        SequentialStunCandidate { stun_server: dead_2, candidate_id: "stun-1".to_string() },
    ];

    match tokio::time::timeout(Duration::from_secs(10), connect_stun_p2p_with_fallback(&SystemQuicEndpointFactory, &target, &candidates)).await {
        Ok(Err(SequentialStunConnectError::AllCandidatesFailed { failures })) => {
            assert_eq!(failures.len(), 2);
            assert_eq!(failures[0].candidate_id, "stun-0");
            assert_eq!(failures[1].candidate_id, "stun-1");
        }
        Ok(Ok(_)) => panic!("expected AllCandidatesFailed, but the fallback succeeded"),
        Ok(Err(other)) => panic!("expected AllCandidatesFailed, got: {other}"),
        Err(_) => panic!("connect_stun_p2p_with_fallback should not hang"),
    }
}

#[tokio::test]
async fn no_candidates_is_a_caller_error() {
    let target = StunP2pTarget {
        peer_addr: "127.0.0.1:1".parse().unwrap(),
        server_name: SNI.to_string(),
        cert_sha256_hex: "0".repeat(64),
        session_secret: b"unused".to_vec(),
    };
    match connect_stun_p2p_with_fallback(&SystemQuicEndpointFactory, &target, &[]).await {
        Err(SequentialStunConnectError::NoCandidates) => {}
        Ok(_) => panic!("an empty candidate list must not succeed"),
        Err(other) => panic!("expected NoCandidates, got: {other}"),
    }
}
