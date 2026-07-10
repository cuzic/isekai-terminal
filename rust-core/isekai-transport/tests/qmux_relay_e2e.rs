//! End-to-end test for `connect_via_relay` against a `QmuxQuicEndpointFactory`
//! (`#qmux-leg1`) — the QMux (draft-ietf-quic-qmux) counterpart of
//! `relay_e2e.rs`, which does the identical thing over `SystemQuicEndpointFactory`
//! (real QUIC-over-UDP). This is not a type-checking-only mock: the mock
//! "isekai-helper" here TLS-accepts a real TCP connection, establishes a real
//! `qmux::Session`, and exchanges the real ATTACH v2 wire bytes
//! (`isekai_protocol::attach`: ATTACH_HELLO / AttachReadyV2 / AttachActivate)
//! end-to-end — proving `relay.rs::connect_and_handshake`'s logic is
//! genuinely transport-agnostic, not just that `QmuxQuicEndpointFactory`
//! compiles. Does not depend on the real deployed relay (whose actual QMux
//! ingress ALPN — see `qmux_relay.rs`'s `QMUX_ALPN` doc comment — is
//! unverified from this repo alone).

#![cfg(feature = "qmux-relay")]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use isekai_protocol::attach::{
    attach_hello_proof_transcript, decode_attach_activate, decode_attach_hello, encode_attach_response, AttachProof,
    AttachRejectReason, AttachResponse, AttachToken, ATTACH_ACTIVATE_FRAME_LEN, ATTACH_HELLO_FRAME_LEN,
};
use isekai_protocol::hello::EXPORTER_LABEL;
use isekai_transport::{connect_via_relay, QmuxQuicEndpointFactory, RelayTarget, TransportError, QMUX_ALPN};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use web_transport_trait::{RecvStream as _, SendStream as _, Session as _};

type HmacSha256 = Hmac<Sha256>;

const SNI: &str = "isekai-pipe.local";

fn generate_cert() -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>, String) {
    let cert = rcgen::generate_simple_self_signed(vec![SNI.to_string()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut hasher = Sha256::new();
    hasher.update(cert_der.as_ref());
    let sha256_hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    (cert_der, key_der, sha256_hex)
}

fn server_tls_config(cert_der: CertificateDer<'static>, key_der: PrivatePkcs8KeyDer<'static>) -> rustls::ServerConfig {
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())
        .unwrap();
    config.alpn_protocols = vec![QMUX_ALPN.to_vec()];
    config
}

/// `ByteStream::read`'s guarantee ("at most `buf.len()`, possibly fewer") is
/// weaker than `AttachHello`/`AttachActivate` decoding needs — mirrors
/// `relay.rs`'s own private `read_exact` helper, duplicated here per this
/// project's e2e test self-containment convention (`tests/*_e2e.rs`
/// deliberately don't share a `tests/common/` module).
async fn read_exact(recv: &mut qmux::RecvStream, buf: &mut [u8]) -> Result<(), qmux::Error> {
    let mut filled = 0;
    while filled < buf.len() {
        match recv.read(&mut buf[filled..]).await? {
            Some(n) if n > 0 => filled += n,
            _ => return Err(qmux::Error::Closed),
        }
    }
    Ok(())
}

/// Accepts exactly one TCP connection, TLS-accepts it, establishes a
/// `qmux::Session`, then runs the same ATTACH v2 responder logic
/// `relay_e2e.rs`'s `run_mock_helper` runs over real QUIC — see that
/// function's doc comment for the full protocol description, unchanged
/// here.
async fn run_mock_helper(
    listener: TcpListener,
    tls_config: rustls::ServerConfig,
    session_secret: Vec<u8>,
    client_done: tokio::sync::oneshot::Receiver<()>,
) {
    let (tcp, _peer) = listener.accept().await.unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));
    let mut tls_stream = acceptor.accept(tcp).await.unwrap();

    // Captured now, symmetric to the client side — see `qmux_relay.rs`'s
    // module docs for why this must happen before `qmux::Session::accept`
    // takes ownership of `tls_stream`.
    let mut exporter = [0u8; 32];
    tls_stream.get_mut().1.export_keying_material(&mut exporter, EXPORTER_LABEL, None).unwrap();

    let config = qmux::Config::new(qmux::Version::QMux01);
    let transport = qmux::transport::Stream::new(tls_stream, config.version, config.max_record_size);
    let session = qmux::Session::accept(transport, config).await.unwrap();

    let (mut send, mut recv) = session.accept_bi().await.unwrap();

    let mut hello_bytes = [0u8; ATTACH_HELLO_FRAME_LEN];
    read_exact(&mut recv, &mut hello_bytes).await.unwrap();
    let hello = decode_attach_hello(&hello_bytes).unwrap();

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
    read_exact(&mut recv, &mut activate_bytes).await.unwrap();
    decode_attach_activate(&activate_bytes).unwrap();

    let mut buf = [0u8; 64];
    if let Ok(Some(n)) = recv.read(&mut buf).await {
        send.write_all(&buf[..n]).await.unwrap();
    }
    send.finish().ok();

    client_done.await.ok();
}

#[tokio::test]
async fn connect_via_relay_completes_hello_ack_over_a_real_qmux_connection() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let listener = TcpListener::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).await.unwrap();
    let helper_addr = listener.local_addr().unwrap();
    let tls_config = server_tls_config(cert_der, key_der);
    let session_secret = b"integration-test-session-secret".to_vec();
    let (client_done_tx, client_done_rx) = tokio::sync::oneshot::channel();

    let server_task = tokio::spawn(run_mock_helper(listener, tls_config, session_secret.clone(), client_done_rx));

    let target = RelayTarget { helper_addr, server_name: SNI.to_string(), cert_sha256_hex, session_secret };
    let factory = QmuxQuicEndpointFactory;
    let mut stream = tokio::time::timeout(Duration::from_secs(10), connect_via_relay(&factory, &target))
        .await
        .expect("connect_via_relay should not hang")
        .expect("connect_via_relay should complete HELLO/ACK over a real qmux connection");

    stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"ping", "helper should echo back what it received over the established stream");

    client_done_tx.send(()).ok();
    server_task.await.unwrap();
}

#[tokio::test]
async fn connect_via_relay_fails_the_handshake_when_the_cert_pin_does_not_match() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (cert_der, key_der, _real_sha256_hex) = generate_cert();
    let listener = TcpListener::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).await.unwrap();
    let helper_addr = listener.local_addr().unwrap();
    let tls_config = server_tls_config(cert_der, key_der);

    let server_task = tokio::spawn(async move {
        if let Ok((tcp, _peer)) = listener.accept().await {
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));
            let _ = acceptor.accept(tcp).await;
        }
    });

    let wrong_fingerprint = "0".repeat(64);
    let target = RelayTarget {
        helper_addr,
        server_name: SNI.to_string(),
        cert_sha256_hex: wrong_fingerprint,
        session_secret: b"unused".to_vec(),
    };
    let factory = QmuxQuicEndpointFactory;
    match tokio::time::timeout(Duration::from_secs(10), connect_via_relay(&factory, &target)).await {
        Ok(Ok(_stream)) => panic!("a mismatched cert pin must fail the TLS handshake, but it succeeded"),
        Ok(Err(err)) => match &err {
            TransportError::CertPinMismatch { expected, got } => {
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
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let listener = TcpListener::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).await.unwrap();
    let helper_addr = listener.local_addr().unwrap();
    let tls_config = server_tls_config(cert_der, key_der);
    let (client_done_tx, client_done_rx) = tokio::sync::oneshot::channel();

    let server_task =
        tokio::spawn(run_mock_helper(listener, tls_config, b"server-side-secret".to_vec(), client_done_rx));

    let target = RelayTarget {
        helper_addr,
        server_name: SNI.to_string(),
        cert_sha256_hex,
        session_secret: b"client-side-secret-does-not-match".to_vec(),
    };
    let factory = QmuxQuicEndpointFactory;
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

