//! End-to-end tests for `connect_via_relay_resumable_with_fallback`
//! (`ISEKAI_PIPE_DESIGN.md` task #12: relay-endpoint fallback). Real `noq`
//! QUIC servers stand in for two relay-assigned candidates of the same
//! isekai-helper, mirroring `relay_e2e.rs`'s/`resume_e2e.rs`'s technique
//! (this crate's convention: one self-contained e2e file per scenario,
//! duplicating the mock-server helpers rather than sharing them across
//! files).
//!
//! The scenarios here specifically exercise the safety property task #12
//! exists to protect (ChatGPT second-opinion review, 2026-07-08): a
//! pre-attach failure on candidate 1 must fall back to candidate 2, but an
//! *ambiguous* (or definitively terminal) failure on candidate 1 must stop
//! immediately — trying candidate 2 after an ambiguous failure would risk a
//! double-attach against the same underlying helper, unsafe before `#18`'s
//! fencing exists.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use isekai_protocol::hello::{
    decode_hello, encode_ack_response, AckResponse, Proof, ALPN, EXPORTER_LABEL, HELLO_FRAME_LEN,
};
use isekai_transport::{
    connect_via_relay_resumable_with_fallback, RelayTarget, SequentialConnectError, SequentialRelayCandidate,
    SystemQuicEndpointFactory,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const SNI: &str = "isekai-pipe.local";
const SESSION_SECRET: &[u8] = b"relay-fallback-test-session-secret";

fn generate_cert() -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>, String) {
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

fn candidate(id: &str, helper_addr: SocketAddr, cert_sha256_hex: String) -> SequentialRelayCandidate {
    SequentialRelayCandidate {
        target: RelayTarget {
            helper_addr,
            server_name: SNI.to_string(),
            cert_sha256_hex,
            session_secret: SESSION_SECRET.to_vec(),
        },
        candidate_id: id.to_string(),
    }
}

/// A real server that completes the whole handshake including opening a
/// control stream — needed because a *winning* candidate must get all the
/// way to `connect_via_relay_resumable_with_fallback`'s
/// `open_control_stream` step, not just the data-stream `HELLO`/`ACK`.
async fn run_full_mock_helper(endpoint: noq::Endpoint, contacted: Option<Arc<AtomicBool>>) {
    let incoming = endpoint.accept().await.unwrap();
    let conn = incoming.await.unwrap();
    if let Some(flag) = &contacted {
        flag.store(true, Ordering::SeqCst);
    }

    let (mut send, mut recv) = conn.accept_bi().await.unwrap();
    let mut hello = [0u8; HELLO_FRAME_LEN];
    recv.read_exact(&mut hello).await.unwrap();
    let hello = decode_hello(&hello).unwrap();

    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"").unwrap();
    let mut mac = HmacSha256::new_from_slice(SESSION_SECRET).unwrap();
    mac.update(&exporter);
    let expected = Proof::new(mac.finalize().into_bytes().into());
    assert!(hello.proof.ct_eq(&expected), "test setup bug: session secret mismatch");

    send.write_all(&encode_ack_response(AckResponse::Ack { effective_resume_grace_secs: 0 })).await.unwrap();

    // CONTROL_HELLO/CONTROL_ACK, matching `resume.rs::open_control_stream`'s
    // wire contract exactly (`archive/HELPER_PROTOCOL.md` §7.3).
    let (mut csend, mut crecv) = conn.accept_bi().await.unwrap();
    let mut chello = [0u8; 33];
    crecv.read_exact(&mut chello).await.unwrap();
    assert_eq!(chello[0], 0x10, "expected CONTROL_HELLO");
    let mut cack = vec![0x11u8];
    cack.extend_from_slice(&[0u8; 16]); // session_id: any 16 bytes, unchecked by this test
    csend.write_all(&cack).await.unwrap();

    // Keep the connection alive briefly so the client can finish reading.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// A server that accepts the connection and reads a full `HELLO`, then
/// closes without ever responding — simulating "the ACK was lost", the
/// central ambiguous-failure scenario `#12` must fail closed on.
async fn run_silent_after_hello_helper(endpoint: noq::Endpoint) {
    let incoming = endpoint.accept().await.unwrap();
    let conn = incoming.await.unwrap();
    let (_send, mut recv) = conn.accept_bi().await.unwrap();
    let mut hello = [0u8; HELLO_FRAME_LEN];
    recv.read_exact(&mut hello).await.unwrap();
    // Deliberately no response, no ACK — just drop everything.
    conn.close(0u32.into(), b"simulated ack loss");
}

/// A server that rejects every HELLO with `RejectAuth` (session secret
/// mismatch), matching a `Terminal` classification.
async fn run_reject_auth_helper(endpoint: noq::Endpoint) {
    let incoming = endpoint.accept().await.unwrap();
    let conn = incoming.await.unwrap();
    let (mut send, mut recv) = conn.accept_bi().await.unwrap();
    let mut hello = [0u8; HELLO_FRAME_LEN];
    recv.read_exact(&mut hello).await.unwrap();
    let _ = decode_hello(&hello).unwrap();
    send.write_all(&encode_ack_response(AckResponse::RejectAuth)).await.unwrap();
    let _ = send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
}

#[tokio::test]
async fn first_candidate_pre_attach_failure_falls_back_to_second() {
    // Candidate 1: a real server, but the client is given the *wrong*
    // cert pin for it — the QUIC/TLS handshake itself fails locally
    // (`ConnectAttemptStage::QuicConnect`), before any HELLO is ever sent,
    // so this must be classified `RetryablePreAttach`.
    let (cert1_der, key1_der, _real_cert1_hex) = generate_cert();
    let endpoint1 = mock_server(cert1_der, key1_der);
    let addr1 = endpoint1.local_addr().unwrap();
    let server1_task = tokio::spawn(async move {
        if let Some(incoming) = endpoint1.accept().await {
            let _ = incoming.await;
        }
    });

    // Candidate 2: succeeds fully.
    let (cert2_der, key2_der, cert2_hex) = generate_cert();
    let endpoint2 = mock_server(cert2_der, key2_der);
    let addr2 = endpoint2.local_addr().unwrap();
    let server2_task = tokio::spawn(run_full_mock_helper(endpoint2, None));

    let candidates =
        vec![candidate("relay-1", addr1, "0".repeat(64) /* wrong pin */), candidate("relay-2", addr2, cert2_hex)];

    let factory = SystemQuicEndpointFactory;
    let (session, winning_target) = tokio::time::timeout(
        Duration::from_secs(10),
        connect_via_relay_resumable_with_fallback(&factory, &candidates, 0),
    )
    .await
    .expect("should not hang")
    .expect("should fall back to candidate 2 and succeed");
    assert_eq!(winning_target.helper_addr, addr2, "the winning target must be candidate 2's, not candidate 1's");

    drop(session.connection);
    server1_task.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), server2_task).await;
}

#[tokio::test]
async fn first_candidate_ambiguous_failure_stops_early_without_trying_second() {
    let (cert1_der, key1_der, cert1_hex) = generate_cert();
    let endpoint1 = mock_server(cert1_der, key1_der);
    let addr1 = endpoint1.local_addr().unwrap();
    let server1_task = tokio::spawn(run_silent_after_hello_helper(endpoint1));

    // Candidate 2 must never be contacted — proven via this flag.
    let (cert2_der, key2_der, cert2_hex) = generate_cert();
    let endpoint2 = mock_server(cert2_der, key2_der);
    let addr2 = endpoint2.local_addr().unwrap();
    let contacted = Arc::new(AtomicBool::new(false));
    let server2_task = tokio::spawn(run_full_mock_helper(endpoint2, Some(contacted.clone())));

    let candidates = vec![candidate("relay-1", addr1, cert1_hex), candidate("relay-2", addr2, cert2_hex)];

    let factory = SystemQuicEndpointFactory;
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        connect_via_relay_resumable_with_fallback(&factory, &candidates, 0),
    )
    .await
    .expect("should not hang");

    match result {
        Err(SequentialConnectError::StoppedEarly { candidate_id, .. }) => {
            assert_eq!(candidate_id, "relay-1");
        }
        Ok(_) => panic!("expected StoppedEarly on the ambiguous first candidate, but connecting succeeded"),
        Err(other) => panic!("expected StoppedEarly on the ambiguous first candidate, got: {other}"),
    }
    assert!(!contacted.load(Ordering::SeqCst), "candidate 2 must never be contacted after an ambiguous failure");

    server1_task.abort();
    server2_task.abort();
}

#[tokio::test]
async fn first_candidate_terminal_rejection_stops_early_without_trying_second() {
    let (cert1_der, key1_der, cert1_hex) = generate_cert();
    let endpoint1 = mock_server(cert1_der, key1_der);
    let addr1 = endpoint1.local_addr().unwrap();
    let server1_task = tokio::spawn(run_reject_auth_helper(endpoint1));

    let (cert2_der, key2_der, cert2_hex) = generate_cert();
    let endpoint2 = mock_server(cert2_der, key2_der);
    let addr2 = endpoint2.local_addr().unwrap();
    let contacted = Arc::new(AtomicBool::new(false));
    let server2_task = tokio::spawn(run_full_mock_helper(endpoint2, Some(contacted.clone())));

    let candidates = vec![candidate("relay-1", addr1, cert1_hex), candidate("relay-2", addr2, cert2_hex)];

    let factory = SystemQuicEndpointFactory;
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        connect_via_relay_resumable_with_fallback(&factory, &candidates, 0),
    )
    .await
    .expect("should not hang");

    match result {
        Err(SequentialConnectError::StoppedEarly { candidate_id, .. }) => {
            assert_eq!(candidate_id, "relay-1");
        }
        Ok(_) => panic!("expected StoppedEarly on the terminal (RejectAuth) first candidate, but connecting succeeded"),
        Err(other) => panic!("expected StoppedEarly on the terminal (RejectAuth) first candidate, got: {other}"),
    }
    assert!(!contacted.load(Ordering::SeqCst), "candidate 2 must never be contacted after a terminal rejection");

    let _ = tokio::time::timeout(Duration::from_secs(5), server1_task).await;
    server2_task.abort();
}

#[tokio::test]
async fn all_candidates_pre_attach_failing_returns_all_candidates_failed() {
    let (cert1_der, key1_der, _) = generate_cert();
    let endpoint1 = mock_server(cert1_der, key1_der);
    let addr1 = endpoint1.local_addr().unwrap();
    let server1_task = tokio::spawn(async move {
        if let Some(incoming) = endpoint1.accept().await {
            let _ = incoming.await;
        }
    });

    let (cert2_der, key2_der, _) = generate_cert();
    let endpoint2 = mock_server(cert2_der, key2_der);
    let addr2 = endpoint2.local_addr().unwrap();
    let server2_task = tokio::spawn(async move {
        if let Some(incoming) = endpoint2.accept().await {
            let _ = incoming.await;
        }
    });

    let candidates = vec![
        candidate("relay-1", addr1, "0".repeat(64)),
        candidate("relay-2", addr2, "1".repeat(64)),
    ];

    let factory = SystemQuicEndpointFactory;
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        connect_via_relay_resumable_with_fallback(&factory, &candidates, 0),
    )
    .await
    .expect("should not hang");

    match result {
        Err(SequentialConnectError::AllCandidatesFailed { failures }) => {
            assert_eq!(failures.len(), 2);
            assert_eq!(failures[0].candidate_id, "relay-1");
            assert_eq!(failures[1].candidate_id, "relay-2");
        }
        Ok(_) => panic!("expected AllCandidatesFailed, but connecting succeeded"),
        Err(other) => panic!("expected AllCandidatesFailed, got: {other}"),
    }

    server1_task.abort();
    server2_task.abort();
}
