//! End-to-end tests for `connect_via_relay_resumable_with_fallback`
//! (`ISEKAI_PIPE_DESIGN.md` task #12: relay-endpoint fallback). Real `noq`
//! QUIC servers stand in for two relay-assigned candidates of the same
//! isekai-helper, mirroring `relay_e2e.rs`'s/`resume_e2e.rs`'s technique
//! (this crate's convention: one self-contained e2e file per scenario,
//! duplicating the mock-server helpers rather than sharing them across
//! files).
//!
//! The scenarios here specifically exercise the safety properties tasks #12
//! and #25-2 exist to protect (ChatGPT second-opinion reviews, 2026-07-08):
//! a pre-attach failure on candidate 1 must fall back to candidate 2 within
//! the same generation; an *ambiguous* failure on candidate 1 must now
//! (`#25-2`, on top of `#18`'s fencing) safely fall back to candidate 2 too,
//! but only after advancing to a new generation — trying candidate 2 with
//! the *same* generation after an ambiguous failure would still risk a
//! double-attach against the same underlying helper. A definitively
//! terminal rejection (e.g. auth failure) still stops the whole attempt
//! immediately; retrying with any generation cannot help there.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use isekai_protocol::attach::{
    attach_hello_proof_transcript, decode_attach_activate, decode_attach_hello, encode_attach_response, AttachProof,
    AttachRejectReason, AttachResponse, AttachToken, ConnectionGeneration, ATTACH_ACTIVATE_FRAME_LEN,
    ATTACH_HELLO_FRAME_LEN,
};
use isekai_protocol::hello::{ALPN, EXPORTER_LABEL};
use isekai_transport::{
    connect_via_relay_resumable_with_fallback, RelayTarget, SequentialConnectError, SequentialRelayCandidate,
    system_quic_factory,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const SNI: &str = "isekai-pipe.local";
const SESSION_SECRET: &[u8] = b"relay-fallback-test-session-secret";

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

    // The winning candidate confirms with AttachActivate before any relay.
    let mut activate_bytes = [0u8; ATTACH_ACTIVATE_FRAME_LEN];
    recv.read_exact(&mut activate_bytes).await.unwrap();
    decode_attach_activate(&activate_bytes).unwrap();

    // CONTROL_HELLO/CONTROL_ACK, matching `resume.rs::open_control_stream`'s
    // wire contract exactly (`archive/HELPER_PROTOCOL.md` §7.3). The session_id
    // now comes from the client's ATTACH_HELLO, so echo it back verbatim like
    // the real server does (#18-4).
    let (mut csend, mut crecv) = conn.accept_bi().await.unwrap();
    let mut chello = [0u8; 33];
    crecv.read_exact(&mut chello).await.unwrap();
    assert_eq!(chello[0], 0x10, "expected CONTROL_HELLO");
    let mut cack = vec![0x11u8];
    cack.extend_from_slice(hello.session_id.as_bytes());
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
    let mut hello_bytes = [0u8; ATTACH_HELLO_FRAME_LEN];
    recv.read_exact(&mut hello_bytes).await.unwrap();
    // Deliberately no response, no AttachReadyV2 — just drop everything.
    conn.close(0u32.into(), b"simulated ack loss");
}

/// A server that unconditionally rejects `ATTACH_HELLO` with
/// `STALE_GENERATION`, reporting `current_generation` — standing in for a
/// candidate whose helper has already moved past the generation this client
/// is using (`#25-4`).
async fn run_reject_stale_generation_helper(endpoint: noq::Endpoint, current_generation: u64) {
    let incoming = endpoint.accept().await.unwrap();
    let conn = incoming.await.unwrap();
    let (mut send, mut recv) = conn.accept_bi().await.unwrap();
    let mut hello_bytes = [0u8; ATTACH_HELLO_FRAME_LEN];
    recv.read_exact(&mut hello_bytes).await.unwrap();
    let _ = decode_attach_hello(&hello_bytes).unwrap();
    let reject = AttachResponse::Reject(AttachRejectReason::StaleGeneration {
        current_generation: ConnectionGeneration::new(current_generation),
    });
    send.write_all(&encode_attach_response(&reject)).await.unwrap();
    let _ = send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
}

/// A server that rejects every HELLO with `RejectAuth` (session secret
/// mismatch), matching a `Terminal` classification.
async fn run_reject_auth_helper(endpoint: noq::Endpoint) {
    let incoming = endpoint.accept().await.unwrap();
    let conn = incoming.await.unwrap();
    let (mut send, mut recv) = conn.accept_bi().await.unwrap();
    let mut hello_bytes = [0u8; ATTACH_HELLO_FRAME_LEN];
    recv.read_exact(&mut hello_bytes).await.unwrap();
    let _ = decode_attach_hello(&hello_bytes).unwrap();
    let reject = AttachResponse::Reject(AttachRejectReason::Auth);
    send.write_all(&encode_attach_response(&reject)).await.unwrap();
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

    let factory = system_quic_factory();
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

/// `#25-2`: an ambiguous failure on candidate 1 (ACK/AttachReadyV2 lost
/// after HELLO) no longer stops the whole attempt — the
/// `GenerationCoordinator` advances to a new generation and candidate 2 is
/// tried under that new generation. `run_full_mock_helper` accepts whatever
/// generation it's handed (it just echoes back what the client sent), so
/// this proves the fallback reaches and succeeds against candidate 2 without
/// asserting on the specific generation value itself — the safety property
/// (never retrying the *same* generation against a possibly-committed
/// candidate 1) is `AttachArbiter`'s own job and already covered by `#18-7`'s
/// fencing e2e tests; this test only proves the client-side continuation
/// actually happens end to end.
#[tokio::test]
async fn first_candidate_ambiguous_failure_safely_falls_back_to_second_with_a_new_generation() {
    let (cert1_der, key1_der, cert1_hex) = generate_cert();
    let endpoint1 = mock_server(cert1_der, key1_der);
    let addr1 = endpoint1.local_addr().unwrap();
    let server1_task = tokio::spawn(run_silent_after_hello_helper(endpoint1));

    let (cert2_der, key2_der, cert2_hex) = generate_cert();
    let endpoint2 = mock_server(cert2_der, key2_der);
    let addr2 = endpoint2.local_addr().unwrap();
    let server2_task = tokio::spawn(run_full_mock_helper(endpoint2, None));

    let candidates = vec![candidate("relay-1", addr1, cert1_hex), candidate("relay-2", addr2, cert2_hex)];

    let factory = system_quic_factory();
    let (session, winning_target) = tokio::time::timeout(
        Duration::from_secs(10),
        connect_via_relay_resumable_with_fallback(&factory, &candidates, 0),
    )
    .await
    .expect("should not hang")
    .expect("an ambiguous failure on candidate 1 should safely fall back to candidate 2 with a new generation");
    assert_eq!(winning_target.helper_addr, addr2, "the winning target must be candidate 2's, not candidate 1's");

    drop(session.connection);
    server1_task.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), server2_task).await;
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

    let factory = system_quic_factory();
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

    let factory = system_quic_factory();
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

/// A server that goes silent right after reading `ATTACH_HELLO` on its
/// *first* connection (simulating "the AttachReadyV2/AttachActivate exchange
/// was lost", exactly like `run_silent_after_hello_helper`) but — unlike
/// that helper — actually did commit the attach server-side. It proves this
/// by accepting a *second*, fresh connection and completing a real `RESUME`
/// for the same `session_id`, standing in for `#25-3`'s premise: an
/// ambiguous candidate's attach may have genuinely succeeded even though the
/// client never found out.
async fn run_silent_then_resumable_helper(endpoint: noq::Endpoint) {
    let incoming = endpoint.accept().await.unwrap();
    let conn = incoming.await.unwrap();
    let (_send, mut recv) = conn.accept_bi().await.unwrap();
    let mut hello_bytes = [0u8; ATTACH_HELLO_FRAME_LEN];
    recv.read_exact(&mut hello_bytes).await.unwrap();
    let hello = decode_attach_hello(&hello_bytes).unwrap();
    conn.close(0u32.into(), b"simulated ack loss");

    let incoming2 = endpoint.accept().await.unwrap();
    let conn2 = incoming2.await.unwrap();
    let (mut send2, mut recv2) = conn2.accept_bi().await.unwrap();
    // quicmux::resume's RESUME body (quicmux-server-resume Stage B):
    // frame_type(1) + token_len(2)+token + auth_blob_len(2)+auth_blob +
    // client_sent_offset(8) + client_delivered_offset(8) — see
    // `isekai-pipe/src/engine/mod.rs::handle_resume_stream`'s identical
    // decode for the real interop target.
    let mut frame_type = [0u8; 1];
    recv2.read_exact(&mut frame_type).await.unwrap();
    assert_eq!(frame_type[0], 0x01, "expected quicmux::resume::FRAME_RESUME");
    let mut token_len = [0u8; 2];
    recv2.read_exact(&mut token_len).await.unwrap();
    let mut token = vec![0u8; u16::from_be_bytes(token_len) as usize];
    recv2.read_exact(&mut token).await.unwrap();
    let mut auth_len = [0u8; 2];
    recv2.read_exact(&mut auth_len).await.unwrap();
    let mut auth_blob = vec![0u8; u16::from_be_bytes(auth_len) as usize];
    recv2.read_exact(&mut auth_blob).await.unwrap();
    let mut sent_offset_bytes = [0u8; 8];
    recv2.read_exact(&mut sent_offset_bytes).await.unwrap();
    let mut delivered_offset_bytes = [0u8; 8];
    recv2.read_exact(&mut delivered_offset_bytes).await.unwrap();

    assert_eq!(token.as_slice(), hello.session_id.as_bytes(), "resume must target the same session_id the ambiguous attach used");

    let mut exporter = [0u8; 32];
    conn2.export_keying_material(&mut exporter, EXPORTER_LABEL, b"").unwrap();
    let mut mac = HmacSha256::new_from_slice(SESSION_SECRET).unwrap();
    mac.update(&exporter);
    mac.update(&token);
    let expected: [u8; 32] = mac.finalize().into_bytes().into();
    assert_eq!(auth_blob.as_slice(), &expected[..], "test setup bug: resume proof mismatch");

    let mut ack = Vec::with_capacity(1 + 8 + 8);
    ack.push(0x02u8); // quicmux::resume::FRAME_RESUME_ACK
    ack.extend_from_slice(&0u64.to_be_bytes()); // helper_committed_offset
    ack.extend_from_slice(&0u64.to_be_bytes()); // helper_sent_offset
    send2.write_all(&ack).await.unwrap();

    let (mut csend2, mut crecv2) = conn2.accept_bi().await.unwrap();
    let mut chello = [0u8; 33];
    crecv2.read_exact(&mut chello).await.unwrap();
    assert_eq!(chello[0], 0x10, "expected CONTROL_HELLO");
    let mut cack = vec![0x11u8];
    cack.extend_from_slice(&token);
    csend2.write_all(&cack).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// A server that unconditionally rejects `ATTACH_HELLO` with
/// `ATTACH_ALREADY_ESTABLISHED` — standing in for a candidate that routes to
/// the same underlying helper as one that already reached `Established` via
/// a different candidate.
async fn run_reject_already_established_helper(endpoint: noq::Endpoint) {
    let incoming = endpoint.accept().await.unwrap();
    let conn = incoming.await.unwrap();
    let (mut send, mut recv) = conn.accept_bi().await.unwrap();
    let mut hello_bytes = [0u8; ATTACH_HELLO_FRAME_LEN];
    recv.read_exact(&mut hello_bytes).await.unwrap();
    let _ = decode_attach_hello(&hello_bytes).unwrap();
    let reject = AttachResponse::Reject(AttachRejectReason::AttachAlreadyEstablished);
    send.write_all(&encode_attach_response(&reject)).await.unwrap();
    let _ = send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
}

/// `#25-3`: candidate 1 goes ambiguous (ACK lost after HELLO); candidate 2
/// is told `ATTACH_ALREADY_ESTABLISHED`, implying candidate 1's attach
/// actually succeeded. The fallback connector should converge on resuming
/// candidate 1's session rather than surfacing an error.
#[tokio::test]
async fn ambiguous_then_already_established_converges_on_resuming_the_ambiguous_candidate() {
    let (cert1_der, key1_der, cert1_hex) = generate_cert();
    let endpoint1 = mock_server(cert1_der, key1_der);
    let addr1 = endpoint1.local_addr().unwrap();
    let server1_task = tokio::spawn(run_silent_then_resumable_helper(endpoint1));

    let (cert2_der, key2_der, cert2_hex) = generate_cert();
    let endpoint2 = mock_server(cert2_der, key2_der);
    let addr2 = endpoint2.local_addr().unwrap();
    let server2_task = tokio::spawn(run_reject_already_established_helper(endpoint2));

    let candidates = vec![candidate("relay-1", addr1, cert1_hex), candidate("relay-2", addr2, cert2_hex)];

    let factory = system_quic_factory();
    let (session, winning_target) = tokio::time::timeout(
        Duration::from_secs(10),
        connect_via_relay_resumable_with_fallback(&factory, &candidates, 0),
    )
    .await
    .expect("should not hang")
    .expect("ATTACH_ALREADY_ESTABLISHED after an ambiguous candidate should converge on resuming it");
    assert_eq!(
        winning_target.helper_addr, addr1,
        "the resumed session must be against candidate 1 (the one that went ambiguous), not candidate 2"
    );

    drop(session.connection);
    let _ = tokio::time::timeout(Duration::from_secs(5), server1_task).await;
    server2_task.abort();
}

/// The `MustResume` convergence path (`finish_via_resume`) has no
/// `ATTACH_HELLO` exchange to learn the server's actually-granted resume
/// grace period from — it used to hardcode `effective_resume_grace_secs: 0`,
/// which callers like `isekai-pipe connect`'s `run_resume_loop` treat as a
/// literal zero-second resume window rather than "unknown" (codex review,
/// quicmux-server-resume). Confirms the fix: a caller that requested a real,
/// non-zero grace period sees that same value echoed back even after
/// converging through this path, not `0`.
#[tokio::test]
async fn ambiguous_convergence_reports_the_originally_requested_grace_period_not_zero() {
    let (cert1_der, key1_der, cert1_hex) = generate_cert();
    let endpoint1 = mock_server(cert1_der, key1_der);
    let addr1 = endpoint1.local_addr().unwrap();
    let server1_task = tokio::spawn(run_silent_then_resumable_helper(endpoint1));

    let (cert2_der, key2_der, cert2_hex) = generate_cert();
    let endpoint2 = mock_server(cert2_der, key2_der);
    let addr2 = endpoint2.local_addr().unwrap();
    let server2_task = tokio::spawn(run_reject_already_established_helper(endpoint2));

    let candidates = vec![candidate("relay-1", addr1, cert1_hex), candidate("relay-2", addr2, cert2_hex)];

    let factory = system_quic_factory();
    let (session, _winning_target) = tokio::time::timeout(
        Duration::from_secs(10),
        connect_via_relay_resumable_with_fallback(&factory, &candidates, 180),
    )
    .await
    .expect("should not hang")
    .expect("ATTACH_ALREADY_ESTABLISHED after an ambiguous candidate should converge on resuming it");
    assert_eq!(
        session.effective_resume_grace_secs, 180,
        "should fall back to the originally requested grace period, not report 0 \
         (which downstream callers would treat as an immediate-give-up deadline)"
    );

    drop(session.connection);
    let _ = tokio::time::timeout(Duration::from_secs(5), server1_task).await;
    server2_task.abort();
}

/// `#25-4`: an `ambiguous` failure on the first *two* candidates should
/// each independently advance the generation and rotate forward — proving
/// the round runner correctly handles more than one consecutive advance,
/// not just the single-hop case the other tests exercise.
#[tokio::test]
async fn ambiguous_failures_on_two_consecutive_candidates_both_rotate_forward() {
    let (cert1_der, key1_der, cert1_hex) = generate_cert();
    let endpoint1 = mock_server(cert1_der, key1_der);
    let addr1 = endpoint1.local_addr().unwrap();
    let server1_task = tokio::spawn(run_silent_after_hello_helper(endpoint1));

    let (cert2_der, key2_der, cert2_hex) = generate_cert();
    let endpoint2 = mock_server(cert2_der, key2_der);
    let addr2 = endpoint2.local_addr().unwrap();
    let server2_task = tokio::spawn(run_silent_after_hello_helper(endpoint2));

    let (cert3_der, key3_der, cert3_hex) = generate_cert();
    let endpoint3 = mock_server(cert3_der, key3_der);
    let addr3 = endpoint3.local_addr().unwrap();
    let server3_task = tokio::spawn(run_full_mock_helper(endpoint3, None));

    let candidates = vec![
        candidate("relay-1", addr1, cert1_hex),
        candidate("relay-2", addr2, cert2_hex),
        candidate("relay-3", addr3, cert3_hex),
    ];

    let factory = system_quic_factory();
    let (session, winning_target) = tokio::time::timeout(
        Duration::from_secs(10),
        connect_via_relay_resumable_with_fallback(&factory, &candidates, 0),
    )
    .await
    .expect("should not hang")
    .expect("two consecutive ambiguous failures should each advance the generation and reach candidate 3");
    assert_eq!(winning_target.helper_addr, addr3);

    drop(session.connection);
    server1_task.abort();
    server2_task.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), server3_task).await;
}

/// `#25-4`: `STALE_GENERATION` (a candidate reporting the server is already
/// ahead of this client's generation) is recovered from the same way
/// `AmbiguousAfterAttach` is — advance past the reported floor and rotate to
/// the next candidate — rather than stopping the whole attempt.
#[tokio::test]
async fn stale_generation_is_recovered_by_advancing_past_the_reported_floor() {
    let (cert1_der, key1_der, cert1_hex) = generate_cert();
    let endpoint1 = mock_server(cert1_der, key1_der);
    let addr1 = endpoint1.local_addr().unwrap();
    let server1_task = tokio::spawn(run_reject_stale_generation_helper(endpoint1, 10));

    let (cert2_der, key2_der, cert2_hex) = generate_cert();
    let endpoint2 = mock_server(cert2_der, key2_der);
    let addr2 = endpoint2.local_addr().unwrap();
    let server2_task = tokio::spawn(run_full_mock_helper(endpoint2, None));

    let candidates = vec![candidate("relay-1", addr1, cert1_hex), candidate("relay-2", addr2, cert2_hex)];

    let factory = system_quic_factory();
    let (session, winning_target) = tokio::time::timeout(
        Duration::from_secs(10),
        connect_via_relay_resumable_with_fallback(&factory, &candidates, 0),
    )
    .await
    .expect("should not hang")
    .expect("STALE_GENERATION should be recovered by advancing past the reported floor, not stopping early");
    assert_eq!(winning_target.helper_addr, addr2);

    drop(session.connection);
    let _ = tokio::time::timeout(Duration::from_secs(5), server1_task).await;
    server2_task.abort();
}

/// `#25-4`: if ambiguity keeps recurring past
/// `GenerationCoordinator`'s retry budget (`DEFAULT_MAX_GENERATION_ADVANCES`
/// = 2), the whole attempt gives up deterministically
/// (`SequentialConnectError::GaveUpAfterGenerationRetries`) instead of
/// advancing generations forever.
#[tokio::test]
async fn generation_retry_budget_exhaustion_gives_up_deterministically() {
    let (cert1_der, key1_der, cert1_hex) = generate_cert();
    let endpoint1 = mock_server(cert1_der, key1_der);
    let addr1 = endpoint1.local_addr().unwrap();
    let server1_task = tokio::spawn(run_silent_after_hello_helper(endpoint1));

    let (cert2_der, key2_der, cert2_hex) = generate_cert();
    let endpoint2 = mock_server(cert2_der, key2_der);
    let addr2 = endpoint2.local_addr().unwrap();
    let server2_task = tokio::spawn(run_silent_after_hello_helper(endpoint2));

    let (cert3_der, key3_der, cert3_hex) = generate_cert();
    let endpoint3 = mock_server(cert3_der, key3_der);
    let addr3 = endpoint3.local_addr().unwrap();
    let server3_task = tokio::spawn(run_silent_after_hello_helper(endpoint3));

    let candidates = vec![
        candidate("relay-1", addr1, cert1_hex),
        candidate("relay-2", addr2, cert2_hex),
        candidate("relay-3", addr3, cert3_hex),
    ];

    let factory = system_quic_factory();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        connect_via_relay_resumable_with_fallback(&factory, &candidates, 0),
    )
    .await
    .expect("should not hang");

    match result {
        Err(SequentialConnectError::GaveUpAfterGenerationRetries { .. }) => {}
        Ok(_) => panic!("expected GaveUpAfterGenerationRetries, but connecting succeeded"),
        Err(other) => panic!("expected GaveUpAfterGenerationRetries, got: {other}"),
    }

    server1_task.abort();
    server2_task.abort();
    server3_task.abort();
}
