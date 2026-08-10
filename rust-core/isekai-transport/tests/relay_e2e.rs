//! End-to-end test for `connect_via_relay` against a real local QUIC server
//! standing in for isekai-helper's own noq server (the peer side of
//! `isekai_pipe_quic_transport.rs::establish_quic_connection_with_socket`). This
//! is not a type-checking-only mock: `system_quic_factory` binds an
//! actual UDP socket, performs a real QUIC handshake pinned to the server's
//! self-signed certificate fingerprint, opens a real bidirectional QUIC
//! stream, and exchanges the real ATTACH v2 wire bytes
//! (`isekai_protocol::attach`: ATTACH_HELLO / AttachReadyV2 / AttachActivate)
//! end-to-end.
//!
//! Mock scaffolding (`generate_cert`/`mock_helper_server`/`run_mock_helper`)
//! lives in `tests/common/mod.rs`, shared with the other e2e files that speak
//! the same ATTACH v2 protocol over the same plain-loopback `noq::Endpoint`
//! shape.

use std::time::Duration;

use isekai_protocol::attach::AttachRejectReason;
use isekai_transport::{connect_via_relay, system_quic_factory, MuxError, RelayTarget, TransportError};

mod common;
use common::{generate_cert, mock_noq_server as mock_helper_server, run_mock_attach_helper as run_mock_helper, SNI};

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
        local_bind_port_range: None,
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
        local_bind_port_range: None,
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
        local_bind_port_range: None,
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
