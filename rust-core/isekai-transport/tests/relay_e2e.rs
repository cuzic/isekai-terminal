//! End-to-end test for `connect_via_relay` against a real local QUIC server
//! standing in for isekai-helper's own noq server (the peer side of
//! `isekai_pipe_quic_transport.rs::establish_quic_connection_with_socket`). This
//! is not a type-checking-only mock: `system_quic_factory` binds an
//! actual UDP socket, performs a real QUIC handshake pinned to the server's
//! self-signed certificate fingerprint, opens a real bidirectional QUIC
//! stream, and exchanges the real ATTACH v2 wire bytes
//! (`isekai_protocol::attach`: ATTACH_HELLO / AttachReadyV2 / AttachActivate)
//! end-to-end.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use isekai_protocol::attach::{
    attach_hello_proof_transcript, decode_attach_activate, decode_attach_hello, encode_attach_response, AttachProof,
    AttachRejectReason, AttachResponse, AttachToken, ATTACH_ACTIVATE_FRAME_LEN, ATTACH_HELLO_FRAME_LEN,
};
use isekai_protocol::hello::{ALPN, EXPORTER_LABEL};
use isekai_transport::{connect_via_relay, system_quic_factory, MuxError, RelayTarget, TransportError};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const SNI: &str = "isekai-pipe.local";

/// Generates a self-signed certificate (standing in for isekai-helper's own
/// ephemeral cert, `archive/HELPER_PROTOCOL.md` §2) and returns it alongside the
/// lowercase-hex SHA-256 fingerprint a real client would receive out-of-band
/// over the bootstrap SSH channel.
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

/// A real `noq` server endpoint, configured exactly like isekai-helper's own
/// QUIC server (`archive/HELPER_PROTOCOL.md` §4 ALPN, self-signed cert).
fn mock_helper_server(cert_der: CertificateDer<'static>, key_der: PrivatePkcs8KeyDer<'static>) -> noq::Endpoint {
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())
        .unwrap();
    tls_config.alpn_protocols = vec![ALPN.to_vec()];
    let quic_crypto = noq::crypto::rustls::QuicServerConfig::try_from(tls_config).unwrap();
    let config = noq::ServerConfig::with_crypto(Arc::new(quic_crypto));
    noq::Endpoint::server(config, SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap()
}

/// Accepts exactly one connection and one bidirectional stream, reads the
/// ATTACH_HELLO frame, verifies the proof the same way isekai-helper would
/// (`isekai_protocol::attach`: `HMAC-SHA256(session_secret, exporter ||
/// attach_hello_proof_transcript(..))`), and replies AttachReadyV2 /
/// REJECT_AUTH accordingly. On AttachReadyV2 it then reads the client's
/// AttachActivate before echoing back one more message, to prove the returned
/// stream is a real, working, bidirectional pass-through afterward — not just
/// a handshake stub.
///
/// `client_done` must fire only after the client side has finished reading
/// everything it needs from this connection. Dropping `conn`/`endpoint`
/// (which happens as soon as this function returns) races the client
/// actually draining its receive buffer otherwise — the same hand-off
/// hazard `isekai-link-masque/tests/relay_e2e.rs` documents and works around
/// the same way.
async fn run_mock_helper(
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

    // The client confirms the attach with AttachActivate on the same stream
    // before it becomes a raw pass-through.
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
async fn connect_via_relay_completes_hello_ack_over_a_real_quic_connection() {
    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let endpoint = mock_helper_server(cert_der, key_der);
    let helper_addr = endpoint.local_addr().unwrap();
    let session_secret = b"integration-test-session-secret".to_vec();
    let (client_done_tx, client_done_rx) = tokio::sync::oneshot::channel();

    let server_task = tokio::spawn(run_mock_helper(endpoint, session_secret.clone(), client_done_rx));

    let target = RelayTarget {
        helper_addr,
        server_name: SNI.to_string(),
        cert_sha256_hex,
        session_secret,
    };
    let factory = system_quic_factory();
    let mut stream = tokio::time::timeout(Duration::from_secs(10), connect_via_relay(&factory, &target))
        .await
        .expect("connect_via_relay should not hang")
        .expect("connect_via_relay should complete HELLO/ACK over a real QUIC connection");

    // Prove the returned stream is a live, working, bidirectional
    // pass-through — not just something that satisfied the handshake.
    stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"ping", "helper should echo back what it received over the established stream");

    client_done_tx.send(()).ok();
    server_task.await.unwrap();
}

#[tokio::test]
async fn connect_via_relay_fails_the_handshake_when_the_cert_pin_does_not_match() {
    let (cert_der, key_der, _real_sha256_hex) = generate_cert();
    let endpoint = mock_helper_server(cert_der, key_der);
    let helper_addr = endpoint.local_addr().unwrap();

    let server_task = tokio::spawn(async move {
        // The client is expected to abort the TLS handshake because of the
        // mismatched pin below; this task exists only so the endpoint's
        // accept loop actually gets driven instead of leaving the incoming
        // attempt queued and undriven.
        if let Some(incoming) = endpoint.accept().await {
            let _ = incoming.await;
        }
    });

    let wrong_fingerprint = "0".repeat(64);
    let target = RelayTarget {
        helper_addr,
        server_name: SNI.to_string(),
        cert_sha256_hex: wrong_fingerprint,
        session_secret: b"unused".to_vec(),
    };
    let factory = system_quic_factory();
    // `Box<dyn ByteStream>` (the success type) isn't `Debug`, so this can't
    // use `.expect_err()`/`.unwrap_err()` (both require `T: Debug`) — match
    // explicitly instead.
    match tokio::time::timeout(Duration::from_secs(10), connect_via_relay(&factory, &target)).await {
        Ok(Ok(_stream)) => panic!("a mismatched cert pin must fail the QUIC handshake, but it succeeded"),
        // `ISEKAI_PIPE_DESIGN.md` §8 Epic N: cert pin mismatches are now
        // classified precisely (`TransportError::CertPinMismatch`, recovered
        // out-of-band from `PinnedCertVerifier`'s shared slot) rather than
        // falling into the generic `Handshake(String)` bucket every other
        // QUIC handshake failure still uses — this is exactly the signal
        // `is_stale_trust_signal()` needs to distinguish "cached trust
        // material went stale" from "peer unreachable".
        Ok(Err(err)) => match &err {
            TransportError::Mux(MuxError::CertPinMismatch { expected, got }) => {
                assert_eq!(expected, "0".repeat(64).as_str());
                assert_ne!(got, expected);
                assert!(err.is_stale_trust_signal(), "got: {err:?}");
            }
            other => panic!("expected TransportError::CertPinMismatch, got: {other:?}"),
        },
        Err(_) => panic!("connect_via_relay should fail fast rather than hang"),
    }

    server_task.abort();
}

#[tokio::test]
async fn connect_via_relay_surfaces_reject_auth_for_a_wrong_session_secret() {
    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let endpoint = mock_helper_server(cert_der, key_der);
    let helper_addr = endpoint.local_addr().unwrap();
    let (client_done_tx, client_done_rx) = tokio::sync::oneshot::channel();

    let server_task =
        tokio::spawn(run_mock_helper(endpoint, b"server-side-secret".to_vec(), client_done_rx));

    let target = RelayTarget {
        helper_addr,
        server_name: SNI.to_string(),
        cert_sha256_hex,
        session_secret: b"client-side-secret-does-not-match".to_vec(),
    };
    let factory = system_quic_factory();
    match tokio::time::timeout(Duration::from_secs(10), connect_via_relay(&factory, &target)).await {
        Ok(Ok(_stream)) => panic!("a mismatched session_secret must be rejected, but it succeeded"),
        Ok(Err(err)) => {
            assert!(matches!(err, TransportError::Rejected(AttachRejectReason::Auth)), "got: {err:?}")
        }
        Err(_) => panic!("connect_via_relay should not hang"),
    }

    client_done_tx.send(()).ok();
    server_task.await.unwrap();
}
