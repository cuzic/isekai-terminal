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

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use isekai_protocol::attach::{
    attach_hello_proof_transcript, decode_attach_activate, decode_attach_hello, encode_attach_response, AttachProof,
    AttachRejectReason, AttachResponse, AttachToken, ATTACH_ACTIVATE_FRAME_LEN, ATTACH_HELLO_FRAME_LEN,
};
use isekai_protocol::hello::{ALPN, EXPORTER_LABEL};
use isekai_transport::{connect_stun_p2p, CandidateIdentity, StunP2pTarget, system_quic_factory, TransportError};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const TEST_IDENTITY: CandidateIdentity<'static> =
    CandidateIdentity { kind: "stun-p2p", source: "test", provider: "test", id: "test" };

const SNI: &str = "isekai-pipe.local";

/// A minimal mock STUN server (RFC 5389 Binding Request/Response): replies to
/// every Binding Request with a Binding Success Response whose
/// XOR-MAPPED-ADDRESS is the request's observed source address. Byte-for-byte
/// the same shape as `isekai-stun`'s own test helper and
/// `isekai_stun_p2p_transport.rs`'s `spawn_mock_stun_server`.
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
            resp.extend_from_slice(&0x0101u16.to_be_bytes()); // Binding Success Response
            resp.extend_from_slice(&12u16.to_be_bytes()); // 4(attr header) + 8(attr value)
            resp.extend_from_slice(&magic_cookie.to_be_bytes());
            resp.extend_from_slice(transaction_id);
            resp.extend_from_slice(&0x0020u16.to_be_bytes()); // XOR-MAPPED-ADDRESS
            resp.extend_from_slice(&8u16.to_be_bytes());
            resp.push(0);
            resp.push(0x01); // family: IPv4
            resp.extend_from_slice(&xport.to_be_bytes());
            resp.extend_from_slice(&xaddr.to_be_bytes());

            let _ = server.send_to(&resp, from).await;
        }
    });
    addr
}

/// Generates a self-signed certificate standing in for isekai-helper's own
/// ephemeral cert, returning it alongside the lowercase-hex SHA-256
/// fingerprint a real client receives out-of-band.
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

/// A real `noq` server endpoint standing in for the peer's isekai-helper QUIC
/// server, bound on loopback so its address can be handed to
/// `connect_stun_p2p` as `StunP2pTarget::peer_addr` — exactly the observed
/// address a real STUN-based rendezvous would have produced for the peer.
/// Also receives (and ignores) this test's raw hole-punch probe datagrams on
/// the very same socket before any QUIC packet arrives, exactly like a real
/// isekai-helper peer would (`isekai-helper/src/main.rs`'s
/// `--stun-server`/`--punch-peer` handling binds one socket for both).
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

/// Accepts exactly one connection and one bidirectional stream, verifies the
/// ATTACH_HELLO proof the same way isekai-helper would, and replies
/// AttachReadyV2 / REJECT_AUTH accordingly. On AttachReadyV2 it reads the
/// client's AttachActivate, then echoes back one more message to prove the
/// returned stream is a live, working, bidirectional pass-through — not just a
/// handshake stub (mirrors `relay_e2e.rs::run_mock_helper`).
async fn run_mock_peer(
    endpoint: noq::Endpoint,
    session_secret: Vec<u8>,
    client_done: tokio::sync::oneshot::Receiver<()>,
) {
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
        connect_stun_p2p(&system_quic_factory(), stun_server, &target, TEST_IDENTITY),
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
        connect_stun_p2p(&system_quic_factory(), stun_server, &target, TEST_IDENTITY),
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
        connect_stun_p2p(&system_quic_factory(), dead_stun_server, &target, TEST_IDENTITY),
    )
    .await
    {
        Ok(Ok(_conn)) => panic!("an unreachable STUN server must fail the connection, but it succeeded"),
        Ok(Err(err)) => assert!(matches!(err, TransportError::Stun(_)), "got: {err:?}"),
        Err(_) => panic!("connect_stun_p2p should fail fast rather than hang forever"),
    }
}
