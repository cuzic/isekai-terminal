//! Shared mock-server scaffolding for `isekai-transport`'s `tests/*_e2e.rs`
//! files.
//!
//! `generate_cert`, the plain-loopback ATTACH-v2-capable `noq::Endpoint`
//! constructor, `spawn_mock_stun_server`, and the ATTACH v2 handshake
//! responder used to be copied byte-for-byte (only the function name
//! differed) across several files (`relay_e2e.rs`, `resume_e2e.rs`,
//! `race_e2e.rs`, `stun_p2p_e2e.rs`, `stun_p2p_fallback_e2e.rs`,
//! `relay_fallback_e2e.rs`, `rebind_e2e.rs`, `multipath_e2e.rs`) — unlike
//! `isekai-ssh/tests/*_e2e.rs`'s deliberate per-file duplication convention
//! (see that crate's own e2e files and
//! `isekai-ssh-e2e-test-self-containment-convention` in project memory),
//! these copies had genuinely diverged from that intent and drifted into
//! plain copy-paste, so consolidating them here is the actual reduction in
//! duplication that convention is meant to avoid paying for needlessly.
//!
//! Deliberately **not** shared here: the multipath-flavored `mock_server`
//! variants (`multipath_e2e.rs`, `multipath_datagram_e2e.rs`,
//! `rebind_e2e.rs::run_echo_server`) bind to the wildcard address and
//! negotiate noq's multipath extension / datagram buffers — real,
//! per-scenario configuration differences, not copy-paste — and the
//! larger per-scenario mock responders in `resume_e2e.rs`
//! (ATTACH+RESUME dispatch over a session table), `race_e2e.rs::run_full_mock`
//! (fixed `negotiated_resume_grace_secs: 0`, optional "contacted" flag, no
//! `client_done` signaling), and `relay_fallback_e2e.rs` (CONTROL_HELLO/ACK
//! chaining, stale-generation/reject-auth/silent-then-resumable variants),
//! which each encode real behavioral differences the plain ATTACH-v2
//! responder below doesn't need to (and shouldn't) generalize over.
//!
//! Each `tests/*_e2e.rs` file that includes this module (`mod common;`) is
//! compiled as its own separate crate and only uses a subset of what's
//! exported here, so `dead_code` is silenced crate-wide instead of chasing
//! per-binary subsets.
#![allow(dead_code)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use hmac::{Hmac, Mac};
use isekai_protocol::attach::{
    attach_hello_proof_transcript, decode_attach_activate, decode_attach_hello, encode_attach_response, AttachProof,
    AttachRejectReason, AttachResponse, AttachToken, ATTACH_ACTIVATE_FRAME_LEN, ATTACH_HELLO_FRAME_LEN,
};
use isekai_protocol::hello::{ALPN, EXPORTER_LABEL};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// The SNI every e2e test in this crate pins its mock server's self-signed
/// cert to — a fixed value everywhere (a real client learns this out of
/// band from bootstrap SSH), so it lives here instead of being redeclared
/// as an identical `const SNI` in every file.
pub const SNI: &str = "isekai-pipe.local";

/// Generates a self-signed certificate (standing in for isekai-helper's own
/// ephemeral cert, `archive/HELPER_PROTOCOL.md` §2) and returns it alongside
/// the lowercase-hex SHA-256 fingerprint a real client would receive
/// out-of-band over the bootstrap SSH channel.
pub fn generate_cert() -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>, String) {
    // The `qmux-relay` feature links `aws-lc-rs` alongside noq's own
    // `ring`, so rustls can no longer auto-select a single process-wide
    // crypto provider when this crate is built with that feature on —
    // every e2e test file calls `generate_cert` first, so fixing it once
    // here covers all of them.
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
/// QUIC server (`archive/HELPER_PROTOCOL.md` §4 ALPN, self-signed cert),
/// bound to loopback with no multipath/datagram extensions negotiated.
///
/// Multipath-flavored test files need a wildcard bind plus extra
/// `TransportConfig` knobs this constructor deliberately doesn't have
/// (see this module's docs), so they keep their own local `mock_server`
/// instead of using this one.
pub fn mock_noq_server(cert_der: CertificateDer<'static>, key_der: PrivatePkcs8KeyDer<'static>) -> noq::Endpoint {
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())
        .unwrap();
    tls_config.alpn_protocols = vec![ALPN.to_vec()];
    let quic_crypto = noq::crypto::rustls::QuicServerConfig::try_from(tls_config).unwrap();
    let config = noq::ServerConfig::with_crypto(Arc::new(quic_crypto));
    noq::Endpoint::server(config, SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap()
}

/// A minimal mock STUN server (RFC 5389 Binding Request/Response): replies
/// to every Binding Request with a Binding Success Response whose
/// XOR-MAPPED-ADDRESS is the request's observed source address. Byte-for-byte
/// the same shape as `isekai-stun`'s own test helper and
/// `isekai_stun_p2p_transport.rs`'s `spawn_mock_stun_server`.
pub async fn spawn_mock_stun_server() -> SocketAddr {
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

/// Accepts exactly one connection and one bidirectional stream, reads the
/// ATTACH_HELLO frame, verifies the proof the same way isekai-helper would
/// (`isekai_protocol::attach`: `HMAC-SHA256(session_secret, exporter ||
/// attach_hello_proof_transcript(..))`), and replies AttachReadyV2 /
/// REJECT_AUTH accordingly. On AttachReadyV2 it then reads the client's
/// AttachActivate before echoing back one more message, to prove the
/// returned stream is a real, working, bidirectional pass-through afterward
/// — not just a handshake stub.
///
/// Used for both the relay ("helper") and STUN-P2P ("peer") mock servers,
/// since both speak the exact same ATTACH v2 wire protocol once a QUIC
/// connection exists.
///
/// `client_done` must fire only after the client side has finished reading
/// everything it needs from this connection. Dropping `conn`/`endpoint`
/// (which happens as soon as this function returns) races the client
/// actually draining its receive buffer otherwise — the same hand-off
/// hazard `isekai-link-masque/tests/relay_e2e.rs` documents and works around
/// the same way.
pub async fn run_mock_attach_helper(
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
