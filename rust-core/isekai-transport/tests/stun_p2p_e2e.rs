//! End-to-end test for `connect_stun_p2p` against a real local mock STUN
//! server and a real local QUIC server (standing in for a peer's own noq
//! server, mirroring `relay_e2e.rs`'s `mock_helper_server`). Exercises the
//! whole sequence for real: bind a UDP socket, query STUN for this socket's
//! own observed address over real UDP, send real hole-punch probe
//! datagrams to the peer's address, then reuse that same socket for a real
//! QUIC handshake and the HELLO/proof/ACK wire exchange
//! (`isekai_protocol::hello`).
//!
//! This is loopback-only, so there is no real NAT to punch through — like
//! `isekai-terminal-core`'s own `isekai_stun_p2p_transport.rs` test suite, this proves
//! the code path executes correctly end-to-end (STUN query → probe
//! datagrams → QUIC-over-the-same-socket → HELLO/ACK), not that hole
//! punching succeeds against a real NAT (that requires two real networks,
//! `archive/ISEKAI_SSH_DESIGN.md` phase S-7).
//!
//! Mock scaffolding (`generate_cert`/`mock_peer_server`/`run_mock_peer`/
//! `spawn_mock_stun_server`) lives in `tests/common/mod.rs`, shared with
//! `relay_e2e.rs`/`stun_p2p_fallback_e2e.rs`/`race_e2e.rs`.

use std::net::Ipv4Addr;
use std::time::Duration;

use isekai_protocol::attach::AttachRejectReason;
use isekai_transport::{connect_stun_p2p, CandidateIdentity, StunP2pTarget, system_quic_factory, TransportError};

mod common;
use common::{generate_cert, mock_noq_server as mock_peer_server, run_mock_attach_helper as run_mock_peer, spawn_mock_stun_server, SNI};

const TEST_IDENTITY: CandidateIdentity<'static> =
    CandidateIdentity { kind: "stun-p2p", source: "test", provider: "test", id: "test" };

#[tokio::test]
async fn connect_stun_p2p_completes_stun_probe_and_hello_ack_over_a_real_quic_connection() {
    let stun_server = spawn_mock_stun_server().await;

    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let endpoint = mock_peer_server(cert_der, key_der);
    // This is the address a real STUN-based rendezvous would have reported
    // for the peer — on loopback it's simply the peer's bound address.
    let peer_addr = endpoint.local_addr().unwrap();
    let session_secret = b"stun-p2p-integration-test-secret".to_vec();
    let (client_done_tx, client_done_rx) = tokio::sync::oneshot::channel();

    let server_task = tokio::spawn(run_mock_peer(endpoint, session_secret.clone(), client_done_rx));

    let target = StunP2pTarget {
        peer_addr,
        server_name: SNI.to_string(),
        cert_sha256_hex,
        session_secret,
    };

    let mut connection = tokio::time::timeout(
        Duration::from_secs(10),
        connect_stun_p2p(&system_quic_factory(), stun_server, &target, 0, TEST_IDENTITY),
    )
        .await
        .expect("connect_stun_p2p should not hang")
        .expect("connect_stun_p2p should complete STUN + probes + HELLO/ACK over a real QUIC connection");

    // The mock STUN server observed our own probe socket over real UDP —
    // this is the value a real caller would go on to hand to a bootstrap/
    // signaling channel (out of scope for this crate, `archive/ISEKAI_SSH_DESIGN.md`
    // S-6), but here we can at least assert it is a real, non-zero loopback
    // address rather than a placeholder.
    assert_eq!(connection.our_observed_addr.ip(), Ipv4Addr::LOCALHOST);
    assert_ne!(connection.our_observed_addr.port(), 0);

    // Prove the returned stream is a live, working, bidirectional
    // pass-through — not just something that satisfied the handshake.
    connection.stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 64];
    let n = connection.stream.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"ping", "peer should echo back what it received over the established stream");

    client_done_tx.send(()).ok();
    server_task.await.unwrap();
}

#[tokio::test]
async fn connect_stun_p2p_surfaces_reject_auth_for_a_wrong_session_secret() {
    let stun_server = spawn_mock_stun_server().await;

    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let endpoint = mock_peer_server(cert_der, key_der);
    let peer_addr = endpoint.local_addr().unwrap();
    let (client_done_tx, client_done_rx) = tokio::sync::oneshot::channel();

    let server_task =
        tokio::spawn(run_mock_peer(endpoint, b"peer-side-secret".to_vec(), client_done_rx));

    let target = StunP2pTarget {
        peer_addr,
        server_name: SNI.to_string(),
        cert_sha256_hex,
        session_secret: b"client-side-secret-does-not-match".to_vec(),
    };

    match tokio::time::timeout(
        Duration::from_secs(10),
        connect_stun_p2p(&system_quic_factory(), stun_server, &target, 0, TEST_IDENTITY),
    )
    .await
    {
        Ok(Ok(_conn)) => panic!("a mismatched session_secret must be rejected, but it succeeded"),
        Ok(Err(err)) => {
            assert!(matches!(err, TransportError::Rejected(AttachRejectReason::Auth)), "got: {err:?}")
        }
        Err(_) => panic!("connect_stun_p2p should not hang"),
    }

    client_done_tx.send(()).ok();
    server_task.await.unwrap();
}

#[tokio::test]
async fn connect_stun_p2p_fails_fast_when_the_stun_server_is_unreachable() {
    // Nothing listens here: bind-then-drop a UDP socket so its port is very
    // unlikely to have anything else answer on it.
    let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dead_stun_server = probe.local_addr().unwrap();
    drop(probe);

    let target = StunP2pTarget {
        peer_addr: "127.0.0.1:1".parse().unwrap(), // never actually reached
        server_name: SNI.to_string(),
        cert_sha256_hex: "0".repeat(64),
        session_secret: b"unused".to_vec(),
    };

    match tokio::time::timeout(
        Duration::from_secs(10),
        connect_stun_p2p(&system_quic_factory(), dead_stun_server, &target, 0, TEST_IDENTITY),
    )
    .await
    {
        Ok(Ok(_conn)) => panic!("an unreachable STUN server must fail the connection, but it succeeded"),
        Ok(Err(err)) => assert!(matches!(err, TransportError::Stun(_)), "got: {err:?}"),
        Err(_) => panic!("connect_stun_p2p should fail fast rather than hang forever"),
    }
}
