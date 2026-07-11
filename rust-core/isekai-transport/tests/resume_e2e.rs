//! End-to-end test for `isekai-transport`'s resume support (`resume.rs`,
//! `archive/ISEKAI_SSH_DESIGN.md` Phase S-4c) against a real local QUIC server
//! speaking isekai-helper's actual Phase 8 wire protocol
//! (`archive/HELPER_PROTOCOL.md` §7), modeled directly on
//! `rust-core/isekai-helper/src/main.rs`'s `handle_connection`/
//! `handle_stream`/`handle_resume_stream`/`accept_control_stream`.
//!
//! This is not a type-checking-only mock: `system_quic_factory` binds
//! real UDP sockets, performs real QUIC handshakes pinned to the mock
//! server's self-signed certificate fingerprint, and exchanges the real
//! `HELLO`/`ACK`, `CONTROL_HELLO`/`CONTROL_ACK`, and `RESUME`/`RESUME_ACK`
//! wire bytes end-to-end. The key scenario this test exists to prove
//! (`archive/ISEKAI_SSH_DESIGN.md`'s acceptance criteria for S-4c) is: establish a
//! resumable session, **actually sever the QUIC connection** (the client
//! closes its own connection, standing in for "a client-side socket dies"),
//! dial a **brand-new** QUIC connection, `RESUME` on it, and confirm the byte
//! relay continues correctly from where it left off.
//!
//! `isekai-ssh/tests/resume_reconnect_e2e.rs` covers the same scenario one
//! layer up, against the actual compiled `isekai-helper` binary and a real
//! `isekai-ssh connect` reconnect loop — this test instead exercises
//! `isekai-transport`'s public API (`connect_via_relay_resumable`,
//! `reconnect_and_resume`, `open_control_stream`) directly and precisely,
//! without depending on `isekai-ssh`'s own giving-up/backoff policy.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hmac::{Hmac, Mac};
use isekai_protocol::attach::{
    attach_hello_proof_transcript, decode_attach_activate, decode_attach_hello, encode_attach_response, AttachProof,
    AttachRejectReason, AttachResponse, AttachToken, ATTACH_ACTIVATE_FRAME_LEN, ATTACH_HELLO_FRAME_LEN,
    FRAME_ATTACH_HELLO,
};
use isekai_protocol::hello::{Proof, ALPN, EXPORTER_LABEL};
use isekai_protocol::resume::{decode_resume, encode_resume_ack, ResumeAckFrame};
use isekai_protocol::session_id::{SessionId, SESSION_ID_LEN};
use isekai_transport::{
    connect_via_relay_resumable, reconnect_and_resume, C2hHelperCommittedOffset, C2hSentOffset,
    CandidateIdentity, H2cClientDeliveredOffset, H2cSentOffset, RelayTarget, system_quic_factory,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const TEST_IDENTITY: CandidateIdentity<'static> =
    CandidateIdentity { kind: "relay", source: "test", provider: "test", id: "test" };

const SNI: &str = "isekai-pipe.local";
const CONTROL_HELLO: u8 = 0x10;
const CONTROL_ACK: u8 = 0x11;

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

/// One resumable session's state, kept across QUIC connections
/// (`isekai-helper/src/resume.rs::Session`, minus the parked-TCP-connection
/// half this test doesn't need since it echoes in-process instead of
/// relaying to a real `--target`).
struct MockSession {
    /// C2H bytes received so far (`helper_committed_offset`).
    committed_offset: u64,
    /// All H2C bytes ever sent, from absolute offset 0 (this test never
    /// evicts, unlike the real `OutputBuffer` — the test only ever sends a
    /// few bytes, so an unbounded `Vec` is fine and keeps this mock small).
    output: Vec<u8>,
}

type SessionTable = Arc<Mutex<HashMap<[u8; SESSION_ID_LEN], MockSession>>>;

fn compute_expected_proof(conn: &noq::Connection, session_secret: &[u8], extra: &[u8]) -> Proof {
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"").unwrap();
    let mut mac = HmacSha256::new_from_slice(session_secret).unwrap();
    mac.update(&exporter);
    if !extra.is_empty() {
        mac.update(extra);
    }
    Proof::new(mac.finalize().into_bytes().into())
}

/// Accepts connections forever (until the endpoint is dropped/closed),
/// handling each one as either a fresh `HELLO` or a `RESUME`
/// (`isekai-helper/src/main.rs::handle_connection`'s dispatch).
async fn run_mock_helper(endpoint: noq::Endpoint, session_secret: Vec<u8>, sessions: SessionTable) {
    loop {
        let Some(incoming) = endpoint.accept().await else { break };
        let Ok(conn) = incoming.await else { continue };
        tokio::spawn(handle_connection(conn, session_secret.clone(), sessions.clone()));
    }
}

/// Writes a one-byte rejection and waits for the peer to actually receive it
/// before returning (`isekai-helper/src/main.rs::reject`'s exact rationale:
/// dropping `send`/`conn` immediately after `write_all` can close the QUIC
/// connection before the single byte is actually delivered, so the client
/// never sees the rejection and instead just gets a naked stream/connection
/// error).
async fn reject(send: &mut noq::SendStream, code: u8) {
    if send.write_all(&[code]).await.is_ok() {
        let _ = send.finish();
        let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
    }
}

async fn handle_connection(conn: noq::Connection, session_secret: Vec<u8>, sessions: SessionTable) {
    let (mut send, mut recv) = conn.accept_bi().await.unwrap();
    let mut type_byte = [0u8; 1];
    recv.read_exact(&mut type_byte).await.unwrap();

    match type_byte[0] {
        FRAME_ATTACH_HELLO => {
            let mut rest = [0u8; ATTACH_HELLO_FRAME_LEN - 1];
            recv.read_exact(&mut rest).await.unwrap();
            let mut hello_bytes = [0u8; ATTACH_HELLO_FRAME_LEN];
            hello_bytes[0] = FRAME_ATTACH_HELLO;
            hello_bytes[1..].copy_from_slice(&rest);
            let hello = decode_attach_hello(&hello_bytes).unwrap();

            let transcript = attach_hello_proof_transcript(
                &hello.session_id,
                hello.generation,
                &hello.attempt_id,
                hello.requested_resume_grace_secs,
            );
            let expected = compute_expected_proof(&conn, &session_secret, &transcript);
            if !hello.proof.ct_eq(&AttachProof::new(*expected.as_bytes())) {
                let reject = AttachResponse::Reject(AttachRejectReason::Auth);
                send.write_all(&encode_attach_response(&reject)).await.ok();
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

            // The client confirms with AttachActivate before any relay begins.
            let mut activate_bytes = [0u8; ATTACH_ACTIVATE_FRAME_LEN];
            recv.read_exact(&mut activate_bytes).await.unwrap();
            decode_attach_activate(&activate_bytes).unwrap();

            let session = Arc::new(Mutex::new(MockSession { committed_offset: 0, output: Vec::new() }));

            // Control stream: accepted concurrently with the echo loop below,
            // matching isekai-helper's own "don't delay the data-stream
            // relay waiting for the control stream" design
            // (`main.rs::relay_with_resume`'s comment).
            let control_conn = conn.clone();
            let control_secret = session_secret.clone();
            let control_sessions = sessions.clone();
            let control_session = session.clone();
            let session_id_slot: Arc<Mutex<Option<[u8; SESSION_ID_LEN]>>> = Arc::new(Mutex::new(None));
            let session_id_slot_for_task = session_id_slot.clone();
            tokio::spawn(async move {
                let Ok(Ok((mut csend, mut crecv))) =
                    tokio::time::timeout(Duration::from_secs(5), control_conn.accept_bi()).await
                else {
                    return;
                };
                let mut chello = [0u8; 33];
                if crecv.read_exact(&mut chello).await.is_err() || chello[0] != CONTROL_HELLO {
                    return;
                }
                let expected = compute_expected_proof(&control_conn, &control_secret, b"");
                if !expected.ct_eq(&Proof::new(chello[1..33].try_into().unwrap())) {
                    return;
                }
                let session_id: [u8; SESSION_ID_LEN] = rand::random();
                // NB: read both fields out of `control_session` into locals
                // *before* touching `control_sessions` — nesting two
                // `.lock()` calls inside one struct-literal statement would
                // deadlock, since temporaries in a struct expression aren't
                // dropped until the end of the whole statement (so the first
                // guard would still be held when the second `.lock()` runs).
                let (initial_committed, initial_output) = {
                    let s = control_session.lock().unwrap();
                    (s.committed_offset, s.output.clone())
                };
                control_sessions.lock().unwrap().insert(
                    session_id,
                    MockSession { committed_offset: initial_committed, output: initial_output },
                );
                *session_id_slot_for_task.lock().unwrap() = Some(session_id);
                let mut ack = Vec::with_capacity(17);
                ack.push(CONTROL_ACK);
                ack.extend_from_slice(&session_id);
                csend.write_all(&ack).await.ok();

                // Drain (and ignore) any APP_ACKs the client sends; exit on error.
                loop {
                    let mut frame = [0u8; 9];
                    if crecv.read_exact(&mut frame).await.is_err() {
                        break;
                    }
                }
            });

            echo_loop(&mut send, &mut recv, &session).await;

            // Persist the final state back into the shared table under
            // whatever session_id got assigned (if any) so a future RESUME
            // sees the bytes exchanged during this connection.
            let maybe_id = *session_id_slot.lock().unwrap();
            if let Some(id) = maybe_id {
                let final_state = session.lock().unwrap();
                if let Some(entry) = sessions.lock().unwrap().get_mut(&id) {
                    entry.committed_offset = final_state.committed_offset;
                    entry.output.clone_from(&final_state.output);
                }
            }
        }
        isekai_protocol::resume::FRAME_RESUME => {
            let mut rest = vec![0u8; isekai_protocol::resume::RESUME_FRAME_LEN - 1];
            recv.read_exact(&mut rest).await.unwrap();
            let mut frame_bytes = vec![isekai_protocol::resume::FRAME_RESUME];
            frame_bytes.extend_from_slice(&rest);
            let resume_frame = decode_resume(&frame_bytes).unwrap();

            let expected =
                compute_expected_proof(&conn, &session_secret, resume_frame.session_id.as_bytes());
            if !expected.ct_eq(&Proof::new(*resume_frame.resume_proof.as_bytes())) {
                reject(&mut send, 0xFFu8).await;
                return;
            }

            let id = *resume_frame.session_id.as_bytes();
            let Some((committed, output)) = sessions.lock().unwrap().get(&id).map(|s| (s.committed_offset, s.output.clone())) else {
                reject(&mut send, 0xF9u8).await; // REJECT_UNKNOWN_SESSION
                return;
            };

            let ack = ResumeAckFrame {
                helper_committed_offset: C2hHelperCommittedOffset::new(committed),
                helper_sent_offset: H2cSentOffset::new(output.len() as u64),
            };
            send.write_all(&encode_resume_ack(&ack)).await.unwrap();

            let replay_from = resume_frame.client_delivered_offset.get() as usize;
            if replay_from < output.len() {
                send.write_all(&output[replay_from..]).await.unwrap();
            }

            let session = Arc::new(Mutex::new(MockSession { committed_offset: committed, output }));
            echo_loop(&mut send, &mut recv, &session).await;

            let final_state = session.lock().unwrap();
            if let Some(entry) = sessions.lock().unwrap().get_mut(&id) {
                entry.committed_offset = final_state.committed_offset;
                entry.output.clone_from(&final_state.output);
            }
        }
        other => {
            send.write_all(&[isekai_protocol::hello::FRAME_REJECT_UNSUPPORTED]).await.ok();
            panic!("mock helper received unexpected frame type {other:#x}");
        }
    }
}

/// Reads C2H bytes and immediately echoes them back as H2C, tracking offsets
/// exactly like the real helper's `relay_buffered` (minus an actual
/// `--target` TCP connection — this test only needs to prove offsets and
/// bytes round-trip correctly across a resume, not that isekai-helper's own
/// TCP relay works, which `isekai-helper`'s own test suite already covers).
async fn echo_loop(send: &mut noq::SendStream, recv: &mut noq::RecvStream, session: &Arc<Mutex<MockSession>>) {
    let mut buf = [0u8; 4096];
    loop {
        let n = match recv.read(&mut buf).await {
            Ok(Some(n)) => n,
            _ => break,
        };
        {
            let mut s = session.lock().unwrap();
            s.committed_offset += n as u64;
            s.output.extend_from_slice(&buf[..n]);
        }
        if send.write_all(&buf[..n]).await.is_err() {
            break;
        }
    }
}

#[tokio::test]
async fn resume_survives_a_client_initiated_disconnect_and_relay_continues() {
    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let endpoint = mock_helper_server(cert_der, key_der);
    let helper_addr = endpoint.local_addr().unwrap();
    let session_secret = b"resume-e2e-session-secret-value".to_vec();
    let sessions: SessionTable = Arc::new(Mutex::new(HashMap::new()));

    tokio::spawn(run_mock_helper(endpoint, session_secret.clone(), sessions.clone()));

    let target = RelayTarget {
        helper_addr,
        server_name: SNI.to_string(),
        cert_sha256_hex,
        session_secret,
    };
    let factory = system_quic_factory();

    // 1. Establish the first (resumable) connection.
    let session = tokio::time::timeout(
        Duration::from_secs(10),
        connect_via_relay_resumable(&factory, &target, 180, TEST_IDENTITY),
    )
    .await
    .expect("connect_via_relay_resumable should not hang")
    .expect("connect_via_relay_resumable should complete HELLO/ACK + CONTROL_HELLO/ACK");
    assert_eq!(
        session.effective_resume_grace_secs, 180,
        "mock server should echo back the requested grace verbatim (it has no lower max configured)"
    );

    let mut data_stream = session.data_stream;

    // 2. Exchange some bytes over the data stream before disconnecting, to
    // prove offsets advance correctly.
    data_stream.write_all(b"hello1").await.unwrap();
    let mut buf = [0u8; 64];
    let n = data_stream.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hello1", "mock helper should echo back what it received");

    let client_sent_offset = C2hSentOffset::new(6);
    let client_delivered_offset = H2cClientDeliveredOffset::new(6);

    // 3. Simulate a client-side socket/connection death: close our own QUIC
    // connection out from under the still-open data stream
    // (`archive/ISEKAI_SSH_DESIGN.md`'s acceptance criterion: "クライアント側の
    // ソケットを閉じる"). Subsequent reads/writes on `data_stream` must fail.
    session.connection.close().await;
    drop(data_stream);
    // Also drop the control stream — its background reader/writer would
    // otherwise keep the (now half-dead) connection referenced and log
    // spurious errors; not required for correctness, just hygiene.
    drop(session.control_stream);

    // 4. Reconnect on a **brand-new** QUIC connection and RESUME.
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        reconnect_and_resume(&factory, &target, session.session_id, client_sent_offset, client_delivered_offset),
    )
    .await
    .expect("reconnect_and_resume should not hang")
    .expect("reconnect_and_resume should succeed against the still-running mock helper");

    assert_eq!(
        outcome.helper_committed_offset.get(),
        6,
        "RESUME_ACK's helper_committed_offset should reflect the 6 bytes already committed pre-disconnect"
    );
    assert_eq!(outcome.helper_sent_offset.get(), 6, "helper_sent_offset should reflect the 6 bytes already sent pre-disconnect");

    // 5. Prove the relay actually continues after resume: send more bytes
    // over the *new* stream and confirm they still get echoed back.
    let mut resumed_stream = outcome.data_stream;
    resumed_stream.write_all(b"hello2-after-resume").await.unwrap();
    let mut buf2 = [0u8; 64];
    let n2 = resumed_stream.read(&mut buf2).await.unwrap();
    assert_eq!(
        &buf2[..n2],
        b"hello2-after-resume",
        "relay must continue working over the resumed connection"
    );
}

#[tokio::test]
async fn reconnect_and_resume_fails_for_an_unknown_session_id() {
    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let endpoint = mock_helper_server(cert_der, key_der);
    let helper_addr = endpoint.local_addr().unwrap();
    let session_secret = b"resume-e2e-session-secret-value-2".to_vec();
    let sessions: SessionTable = Arc::new(Mutex::new(HashMap::new()));

    tokio::spawn(run_mock_helper(endpoint, session_secret.clone(), sessions));

    let target = RelayTarget { helper_addr, server_name: SNI.to_string(), cert_sha256_hex, session_secret };
    let factory = system_quic_factory();

    let bogus_session_id = SessionId::from_bytes([0xABu8; SESSION_ID_LEN]);
    // `ResumeAckOutcome` (the `Ok` payload) isn't `Debug` (it carries a
    // `Box<dyn ByteStream>`), so this can't use `.unwrap_err()` — match
    // explicitly instead, same convention as `relay_e2e.rs`'s cert-pin-mismatch
    // test.
    match tokio::time::timeout(
        Duration::from_secs(10),
        reconnect_and_resume(
            &factory,
            &target,
            bogus_session_id,
            C2hSentOffset::new(0),
            H2cClientDeliveredOffset::new(0),
        ),
    )
    .await
    {
        Ok(Err(isekai_transport::TransportError::ResumeRejected(reason))) => {
            assert_eq!(reason, isekai_transport::ResumeRejectReason::UnknownSession);
        }
        Ok(Err(other_err)) => panic!("expected ResumeRejected(UnknownSession), got a different error: {other_err}"),
        Ok(Ok(_)) => panic!("expected ResumeRejected(UnknownSession) for a bogus session_id, but resume succeeded"),
        Err(_) => panic!("reconnect_and_resume should not hang"),
    }
}
