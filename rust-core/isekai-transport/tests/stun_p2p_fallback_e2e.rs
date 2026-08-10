//! End-to-end tests for `connect_stun_p2p_with_fallback` (`#11`): trying
//! several STUN-server candidates against the *same* peer in order, falling
//! back only when it's safe to (`AttemptFailure::may_retry_pre_fencing`).
//! `generate_cert`/`mock_peer_server`/`run_mock_peer`/`spawn_mock_stun_server`
//! come from `tests/common/mod.rs`, shared with `stun_p2p_e2e.rs`.

use std::net::SocketAddr;
use std::time::Duration;

use isekai_transport::{connect_stun_p2p_with_fallback, SequentialStunCandidate, SequentialStunConnectError, StunP2pTarget, system_quic_factory};

mod common;
use common::{generate_cert, mock_noq_server as mock_peer_server, run_mock_attach_helper as run_mock_peer, spawn_mock_stun_server, SNI};

/// Binds then immediately drops a UDP socket, so its port is very unlikely to
/// have anything else answer on it — standing in for an unreachable/dead
/// STUN server (mirrors `stun_p2p_e2e.rs::connect_stun_p2p_fails_fast_when_the_stun_server_is_unreachable`).
async fn dead_stun_server() -> SocketAddr {
    let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    addr
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
        tokio::time::timeout(Duration::from_secs(10), connect_stun_p2p_with_fallback(&system_quic_factory(), &target, &candidates))
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

    match tokio::time::timeout(Duration::from_secs(10), connect_stun_p2p_with_fallback(&system_quic_factory(), &target, &candidates)).await {
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

    match tokio::time::timeout(Duration::from_secs(10), connect_stun_p2p_with_fallback(&system_quic_factory(), &target, &candidates)).await {
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
    match connect_stun_p2p_with_fallback(&system_quic_factory(), &target, &[]).await {
        Err(SequentialStunConnectError::NoCandidates) => {}
        Ok(_) => panic!("an empty candidate list must not succeed"),
        Err(other) => panic!("expected NoCandidates, got: {other}"),
    }
}
