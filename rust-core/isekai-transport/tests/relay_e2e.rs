//! End-to-end test for `connect_via_relay` against a real local QUIC server
//! standing in for isekai-helper's own noq server (the peer side of
//! `helper_quic_transport.rs::establish_quic_connection_with_socket`). This
//! is not a type-checking-only mock: `SystemQuicEndpointFactory` binds an
//! actual UDP socket, performs a real QUIC handshake pinned to the server's
//! self-signed certificate fingerprint, opens a real bidirectional QUIC
//! stream, and exchanges the real HELLO/ACK wire bytes
//! (`isekai_protocol::hello`) end-to-end.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use isekai_protocol::hello::{
    decode_hello, encode_ack_response, AckResponse, Proof, ALPN, EXPORTER_LABEL, HELLO_FRAME_LEN,
};
use isekai_transport::{connect_via_relay, RelayTarget, SystemQuicEndpointFactory, TransportError};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const SNI: &str = "isekai-helper.local";

/// Generates a self-signed certificate (standing in for isekai-helper's own
/// ephemeral cert, `HELPER_PROTOCOL.md` §2) and returns it alongside the
/// lowercase-hex SHA-256 fingerprint a real client would receive out-of-band
/// over the bootstrap SSH channel.
fn generate_cert() -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>, String) {
    let cert = rcgen::generate_simple_self_signed(vec![SNI.to_string()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut hasher = Sha256::new();
    hasher.update(cert_der.as_ref());
    let sha256_hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    (cert_der, key_der, sha256_hex)
}

/// A real `noq` server endpoint, configured exactly like isekai-helper's own
/// QUIC server (`HELPER_PROTOCOL.md` §4 ALPN, self-signed cert).
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
/// HELLO frame, verifies the proof the same way isekai-helper would
/// (`HELPER_PROTOCOL.md` §4: `HMAC-SHA256(session_secret, exporter)`), and
/// replies ACK/REJECT_AUTH accordingly. On ACK, echoes back one more message
/// to prove the returned stream is a real, working, bidirectional
/// pass-through afterward — not just a handshake stub.
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

    let mut hello = [0u8; HELLO_FRAME_LEN];
    recv.read_exact(&mut hello).await.unwrap();
    let proof = decode_hello(&hello).unwrap();

    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"").unwrap();
    let mut mac = HmacSha256::new_from_slice(&session_secret).unwrap();
    mac.update(&exporter);
    let expected_bytes: [u8; 32] = mac.finalize().into_bytes().into();
    let expected = Proof::new(expected_bytes);

    let ack = if proof.ct_eq(&expected) { AckResponse::Ack } else { AckResponse::RejectAuth };
    send.write_all(&[encode_ack_response(ack)]).await.unwrap();

    if ack == AckResponse::Ack {
        let mut buf = [0u8; 64];
        if let Ok(Some(n)) = recv.read(&mut buf).await {
            send.write_all(&buf[..n]).await.unwrap();
        }
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
    let factory = SystemQuicEndpointFactory;
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
    let factory = SystemQuicEndpointFactory;
    // `Box<dyn ByteStream>` (the success type) isn't `Debug`, so this can't
    // use `.expect_err()`/`.unwrap_err()` (both require `T: Debug`) — match
    // explicitly instead.
    match tokio::time::timeout(Duration::from_secs(10), connect_via_relay(&factory, &target)).await {
        Ok(Ok(_stream)) => panic!("a mismatched cert pin must fail the QUIC handshake, but it succeeded"),
        Ok(Err(err)) => assert!(matches!(err, TransportError::Handshake(_)), "got: {err:?}"),
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
    let factory = SystemQuicEndpointFactory;
    match tokio::time::timeout(Duration::from_secs(10), connect_via_relay(&factory, &target)).await {
        Ok(Ok(_stream)) => panic!("a mismatched session_secret must be rejected, but it succeeded"),
        Ok(Err(err)) => {
            assert!(matches!(err, TransportError::Rejected(AckResponse::RejectAuth)), "got: {err:?}")
        }
        Err(_) => panic!("connect_via_relay should not hang"),
    }

    client_done_tx.send(()).ok();
    server_task.await.unwrap();
}
