//! ATTACH v2 fencing E2E tests (`#18`, `#18-7`) against a real `isekai-pipe
//! serve` subprocess over real QUIC connections — the concurrency/timing
//! properties `AttachArbiter`/`AttachRuntime` exist to guarantee: a
//! `PendingActivation` slot that never gets activated is eventually freed;
//! `CANCEL_ATTACH` frees it immediately instead of waiting for that timeout;
//! two attempts racing the same `(session_id, generation)` produce exactly
//! one winner; a stale generation is rejected and told the current one; a
//! strictly larger generation may supersede an earlier, not-yet-`Established`
//! attempt (but never lets two target TCP connections be concurrently open
//! for one server instance); and `Established` cannot be re-attached to by a
//! plain `ATTACH_HELLO` (same session → `AttachAlreadyEstablished`, this
//! crate's `serve_e2e.rs::duplicate_connection_is_rejected` already covers
//! the different-session → `BusyOtherSession` case, not repeated here).
//!
//! Self-contained per this crate's own convention (`serve_e2e.rs` duplicates
//! the same helpers rather than sharing them via a `tests/common` module) —
//! see that file for the identical pattern.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use hmac::{Hmac, Mac};
use isekai_protocol::attach::{
    attach_hello_proof_transcript, cancel_attach_proof_transcript, decode_attach_response, encode_attach_activate,
    encode_attach_hello, encode_cancel_attach, AttachActivate, AttachHello, AttachProof, AttachRejectReason,
    AttachResponse, AttemptId, CancelAttach, ConnectionGeneration, ATTACH_READY_FRAME_LEN, FRAME_ATTACH_READY,
    FRAME_REJECT_STALE_GENERATION, STALE_GENERATION_REJECT_FRAME_LEN,
};
use isekai_protocol::session_id::SessionId;
use quinn::crypto::rustls::QuicClientConfig;
use quinn::{ClientConfig, Endpoint};
use rand::RngCore;
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type HmacSha256 = Hmac<Sha256>;
const EXPORTER_LABEL: &[u8] = b"isekai-pipe-auth-v1";
const ALPN: &[u8] = b"isekai-pipe/1";

fn fresh_session_id() -> SessionId {
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    SessionId::from_bytes(bytes)
}

fn fresh_attempt_id() -> AttemptId {
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    AttemptId::from_bytes(bytes)
}

fn compute_attach_proof(
    conn: &quinn::Connection,
    secret: &[u8],
    session_id: &SessionId,
    generation: ConnectionGeneration,
    attempt_id: &AttemptId,
) -> AttachProof {
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"").unwrap();
    let transcript = attach_hello_proof_transcript(session_id, generation, attempt_id, 0);
    let mut mac = HmacSha256::new_from_slice(secret).unwrap();
    mac.update(&exporter);
    mac.update(&transcript);
    let bytes: [u8; 32] = mac.finalize().into_bytes().into();
    AttachProof::new(bytes)
}

fn attach_hello_frame(
    session_id: SessionId,
    generation: ConnectionGeneration,
    attempt_id: AttemptId,
    proof: AttachProof,
) -> Vec<u8> {
    encode_attach_hello(&AttachHello { session_id, generation, attempt_id, requested_resume_grace_secs: 0, proof })
}

/// Reads one `AttachResponse` off the wire with a bounded timeout, using the
/// same two-step read the real client uses.
async fn read_attach_response(recv: &mut quinn::RecvStream, timeout: Duration) -> AttachResponse {
    let mut type_byte = [0u8; 1];
    tokio::time::timeout(timeout, recv.read_exact(&mut type_byte))
        .await
        .expect("timed out waiting for AttachResponse type byte")
        .unwrap();
    let mut full = vec![type_byte[0]];
    let extra_len = match type_byte[0] {
        FRAME_ATTACH_READY => ATTACH_READY_FRAME_LEN - 1,
        FRAME_REJECT_STALE_GENERATION => STALE_GENERATION_REJECT_FRAME_LEN - 1,
        _ => 0,
    };
    if extra_len > 0 {
        let mut rest = vec![0u8; extra_len];
        recv.read_exact(&mut rest).await.unwrap();
        full.extend_from_slice(&rest);
    }
    decode_attach_response(&full).unwrap()
}

/// Sends `ATTACH_HELLO` and returns the raw `AttachResponse`, without sending
/// `AttachActivate` — callers that want to deliberately leave the attempt in
/// `PendingActivation` (or drive `CANCEL_ATTACH`/a superseding attempt next)
/// use this instead of the full `attach_and_activate` helper.
async fn send_hello_and_read_response(
    conn: &quinn::Connection,
    session_secret: &[u8],
    session_id: SessionId,
    generation: ConnectionGeneration,
    attempt_id: AttemptId,
) -> (quinn::SendStream, quinn::RecvStream, AttachResponse) {
    let proof = compute_attach_proof(conn, session_secret, &session_id, generation, &attempt_id);
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&attach_hello_frame(session_id, generation, attempt_id, proof)).await.unwrap();
    let response = read_attach_response(&mut recv, Duration::from_secs(10)).await;
    (send, recv, response)
}

async fn attach_and_activate(
    conn: &quinn::Connection,
    session_secret: &[u8],
    session_id: SessionId,
    generation: ConnectionGeneration,
    attempt_id: AttemptId,
) -> (quinn::SendStream, quinn::RecvStream) {
    let (mut send, recv, response) =
        send_hello_and_read_response(conn, session_secret, session_id, generation, attempt_id).await;
    let attach_token = match response {
        AttachResponse::Ready { attach_token, .. } => attach_token,
        other => panic!("expected AttachReadyV2, got {other:?}"),
    };
    let activate = AttachActivate { session_id, generation, attempt_id, attach_token };
    send.write_all(&encode_attach_activate(&activate)).await.unwrap();
    (send, recv)
}

/// Sends `CANCEL_ATTACH` for `(session_id, generation, attempt_id)` as the
/// *first* stream of a **fresh** QUIC connection — `handle_connection`
/// accepts exactly one stream per connection and dispatches on its frame
/// type, so `CANCEL_ATTACH` (like `RESUME`) always arrives as a new
/// connection's opening frame, never as a second stream on the connection
/// that performed the original `ATTACH_HELLO` (which, per `CancelAttach`'s
/// own module docs, may well be the dead connection being cancelled).
async fn send_cancel_attach(
    helper: &HelperProcess,
    session_secret: &[u8],
    session_id: SessionId,
    generation: ConnectionGeneration,
    attempt_id: AttemptId,
) {
    let conn = connect(helper).await;
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"").unwrap();
    let transcript = cancel_attach_proof_transcript(&session_id, generation, &attempt_id);
    let mut mac = HmacSha256::new_from_slice(session_secret).unwrap();
    mac.update(&exporter);
    mac.update(&transcript);
    let proof_bytes: [u8; 32] = mac.finalize().into_bytes().into();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    let frame = CancelAttach { session_id, generation, attempt_id, proof: AttachProof::new(proof_bytes) };
    send.write_all(&encode_cancel_attach(&frame)).await.unwrap();
    send.finish().ok();
    // Best-effort, fire-and-forget on the wire (module docs) — but for this
    // test to be a meaningful proof, give the server a moment to actually
    // process it before the caller races ahead to the next attach attempt.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[derive(Debug, Deserialize)]
struct Handshake {
    #[allow(dead_code)]
    v: u32,
    session_secret: String,
    peer: HandshakePeer,
    #[serde(default)]
    candidates: Vec<HandshakeCandidate>,
}

#[derive(Debug, Deserialize)]
struct HandshakePeer {
    server_identity: HandshakeServerIdentity,
}

#[derive(Debug, Deserialize)]
struct HandshakeServerIdentity {
    cert_sha256: String,
}

#[derive(Debug, Deserialize)]
struct HandshakeCandidate {
    kind: String,
    #[serde(default)]
    #[allow(dead_code)]
    endpoint: Option<String>,
    #[serde(default)]
    port: Option<u16>,
}

impl Handshake {
    fn cert_sha256(&self) -> &str {
        &self.peer.server_identity.cert_sha256
    }

    fn direct_by_bootstrap_host_port(&self) -> Option<u16> {
        self.candidates
            .iter()
            .find(|candidate| candidate.kind == "direct-by-bootstrap-host")
            .and_then(|candidate| candidate.port)
    }
}

#[derive(Debug)]
struct PinnedCertVerifier {
    expected_sha256_hex: String,
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let mut hasher = Sha256::new();
        hasher.update(end_entity.as_ref());
        let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got == self.expected_sha256_hex {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("cert pin mismatch".into()))
        }
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

struct HelperProcess {
    child: Child,
    handshake: Handshake,
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn helper_bin_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_isekai-pipe"))
}

fn spawn_helper(target: SocketAddr) -> HelperProcess {
    let mut cmd = Command::new(helper_bin_path());
    cmd.arg("serve")
        .arg("--target")
        .arg(target.to_string())
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--log-level")
        .arg("debug")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn isekai-pipe");
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read handshake line from isekai-pipe stdout");
    let handshake: Handshake = serde_json::from_str(line.trim()).expect("failed to parse handshake JSON");

    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut r = BufReader::new(stderr);
            let mut buf = String::new();
            loop {
                buf.clear();
                if r.read_line(&mut buf).unwrap_or(0) == 0 {
                    break;
                }
            }
        });
    }
    std::mem::forget(reader);

    HelperProcess { child, handshake }
}

/// A TCP echo server that also tracks the current and maximum number of
/// concurrently-open connections it has ever seen — the direct, executable
/// form of the safety property fencing exists to guarantee:
/// `max_concurrent_active_targets <= 1` for one `isekai-pipe serve` instance,
/// even while a supersede is in flight.
struct TrackedEchoServer {
    addr: SocketAddr,
    current: Arc<AtomicUsize>,
    max_seen: Arc<AtomicUsize>,
}

async fn spawn_tracked_echo_server() -> TrackedEchoServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let current = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    {
        let current = current.clone();
        let max_seen = max_seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let current = current.clone();
                let max_seen = max_seen.clone();
                tokio::spawn(async move {
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    let mut buf = [0u8; 4096];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if sock.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    current.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
    }
    TrackedEchoServer { addr, current, max_seen }
}

fn make_client_endpoint(cert_sha256_hex: &str) -> Endpoint {
    // isekai-link-masque's qmux dependency links `aws-lc-rs` alongside quinn's
    // own `ring`, so rustls can no longer auto-select a single process-wide
    // crypto provider — every test that reaches here builds a real quinn
    // client, so fixing it once at this chokepoint covers all of them.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
            expected_sha256_hex: cert_sha256_hex.to_string(),
        }))
        .with_no_client_auth();
    client_crypto.alpn_protocols = vec![ALPN.to_vec()];
    let client_config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_crypto).unwrap()));
    let mut endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(client_config);
    endpoint
}

async fn connect(helper: &HelperProcess) -> quinn::Connection {
    let endpoint = make_client_endpoint(helper.handshake.cert_sha256());
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap())
        .parse()
        .unwrap();
    endpoint.connect(server_addr, "isekai-pipe.local").unwrap().await.expect("QUIC handshake failed")
}

/// Scenario 9 (`#18-7`): a `PendingActivation` lease that never receives
/// `AttachActivate` must eventually be torn down and the target closed, so a
/// brand-new attach round can proceed — proven here by a *different* session
/// succeeding shortly after the pending-activation timeout elapses.
#[tokio::test]
async fn pending_activation_timeout_frees_the_slot_for_a_new_session() {
    let echo = spawn_tracked_echo_server().await;
    let helper = spawn_helper(echo.addr);
    let session_secret =
        base64::engine::general_purpose::STANDARD.decode(&helper.handshake.session_secret).unwrap();

    let conn1 = connect(&helper).await;
    let (sid1, gen1, aid1) = (fresh_session_id(), ConnectionGeneration::INITIAL, fresh_attempt_id());
    let (_send1, _recv1, response1) = send_hello_and_read_response(&conn1, &session_secret, sid1, gen1, aid1).await;
    assert!(matches!(response1, AttachResponse::Ready { .. }), "got: {response1:?}");
    // Deliberately never send AttachActivate for conn1.

    // The runtime's pending-activation timeout is 5s; wait comfortably past it.
    tokio::time::sleep(Duration::from_secs(7)).await;

    let conn2 = connect(&helper).await;
    let (sid2, gen2, aid2) = (fresh_session_id(), ConnectionGeneration::INITIAL, fresh_attempt_id());
    let (send2, mut recv2) = attach_and_activate(&conn2, &session_secret, sid2, gen2, aid2).await;
    let mut send2 = send2;
    send2.write_all(b"after-timeout").await.unwrap();
    let mut buf = [0u8; 13];
    tokio::time::timeout(Duration::from_secs(5), recv2.read_exact(&mut buf))
        .await
        .expect("timed out waiting for echo after the pending-activation timeout freed the slot")
        .unwrap();
    assert_eq!(&buf, b"after-timeout");
}

/// Scenario 8 (`#18-7`): `CANCEL_ATTACH` frees a `PendingActivation` slot
/// immediately, without waiting for the multi-second pending-activation
/// timeout — proven by a fresh session succeeding well within a window that
/// would otherwise still be inside that timeout.
#[tokio::test]
async fn cancel_attach_frees_the_slot_without_waiting_for_the_timeout() {
    let echo = spawn_tracked_echo_server().await;
    let helper = spawn_helper(echo.addr);
    let session_secret =
        base64::engine::general_purpose::STANDARD.decode(&helper.handshake.session_secret).unwrap();

    let conn1 = connect(&helper).await;
    let (sid1, gen1, aid1) = (fresh_session_id(), ConnectionGeneration::INITIAL, fresh_attempt_id());
    let (_send1, _recv1, response1) = send_hello_and_read_response(&conn1, &session_secret, sid1, gen1, aid1).await;
    assert!(matches!(response1, AttachResponse::Ready { .. }), "got: {response1:?}");
    send_cancel_attach(&helper, &session_secret, sid1, gen1, aid1).await;

    // The wait above is far shorter than the 5s pending-activation timeout,
    // so success here proves CANCEL actually freed the slot rather than the
    // timeout doing it.
    let conn2 = connect(&helper).await;
    let (sid2, gen2, aid2) = (fresh_session_id(), ConnectionGeneration::INITIAL, fresh_attempt_id());
    let (_send2, _recv2, response2) = tokio::time::timeout(
        Duration::from_secs(2),
        send_hello_and_read_response(&conn2, &session_secret, sid2, gen2, aid2),
    )
    .await
    .expect("a fresh session should attach well within the pending-activation timeout after CANCEL_ATTACH");
    assert!(matches!(response2, AttachResponse::Ready { .. }), "got: {response2:?}");
}

/// Scenario 5/6 (`#18-7`): two attempts racing the same `(session_id,
/// generation)` — the first one the server accepts wins; a second,
/// different `attempt_id` for the exact same generation is rejected
/// `ALREADY_ATTACHED`, not treated as a session-wide terminal failure.
#[tokio::test]
async fn second_attempt_at_the_same_generation_gets_already_attached() {
    let echo = spawn_tracked_echo_server().await;
    let helper = spawn_helper(echo.addr);
    let session_secret =
        base64::engine::general_purpose::STANDARD.decode(&helper.handshake.session_secret).unwrap();
    let session_id = fresh_session_id();
    let generation = ConnectionGeneration::INITIAL;

    let conn1 = connect(&helper).await;
    let attempt1 = fresh_attempt_id();
    let (_send1, _recv1, response1) =
        send_hello_and_read_response(&conn1, &session_secret, session_id, generation, attempt1).await;
    assert!(matches!(response1, AttachResponse::Ready { .. }), "winner should get AttachReadyV2, got: {response1:?}");

    let conn2 = connect(&helper).await;
    let attempt2 = fresh_attempt_id();
    let (_send2, _recv2, response2) =
        send_hello_and_read_response(&conn2, &session_secret, session_id, generation, attempt2).await;
    assert!(
        matches!(response2, AttachResponse::Reject(AttachRejectReason::AlreadyAttached)),
        "loser should get ALREADY_ATTACHED, got: {response2:?}"
    );
}

/// Scenario 3 (`#18-7`): a generation behind the server's current one for
/// this session is rejected `STALE_GENERATION`, and the reject carries the
/// server's actual current generation.
#[tokio::test]
async fn stale_generation_is_rejected_with_the_current_generation_reported() {
    let echo = spawn_tracked_echo_server().await;
    let helper = spawn_helper(echo.addr);
    let session_secret =
        base64::engine::general_purpose::STANDARD.decode(&helper.handshake.session_secret).unwrap();
    let session_id = fresh_session_id();

    let conn1 = connect(&helper).await;
    let (_send1, _recv1, response1) = send_hello_and_read_response(
        &conn1,
        &session_secret,
        session_id,
        ConnectionGeneration::new(5),
        fresh_attempt_id(),
    )
    .await;
    assert!(matches!(response1, AttachResponse::Ready { .. }), "got: {response1:?}");

    let conn2 = connect(&helper).await;
    let (_send2, _recv2, response2) = send_hello_and_read_response(
        &conn2,
        &session_secret,
        session_id,
        ConnectionGeneration::new(3),
        fresh_attempt_id(),
    )
    .await;
    match response2 {
        AttachResponse::Reject(AttachRejectReason::StaleGeneration { current_generation }) => {
            assert_eq!(current_generation, ConnectionGeneration::new(5));
        }
        other => panic!("expected STALE_GENERATION reporting generation 5, got: {other:?}"),
    }
}

/// Scenario 4/7/10 (`#18-7`), the core safety property this whole task
/// exists for: a strictly larger generation may supersede an earlier
/// `PendingActivation` attempt, but the old target TCP connection is always
/// torn down before the new one is opened — this server instance's target
/// never sees more than one concurrently-open connection, even mid-supersede.
#[tokio::test]
async fn larger_generation_supersedes_pending_activation_without_ever_double_connecting_the_target() {
    let echo = spawn_tracked_echo_server().await;
    let helper = spawn_helper(echo.addr);
    let session_secret =
        base64::engine::general_purpose::STANDARD.decode(&helper.handshake.session_secret).unwrap();
    let session_id = fresh_session_id();

    let conn1 = connect(&helper).await;
    let (_send1, _recv1, response1) = send_hello_and_read_response(
        &conn1,
        &session_secret,
        session_id,
        ConnectionGeneration::new(5),
        fresh_attempt_id(),
    )
    .await;
    assert!(matches!(response1, AttachResponse::Ready { .. }), "got: {response1:?}");
    // `AttachReadyV2` is sent to the client the instant the server's target
    // `TcpStream::connect` succeeds — there is a brief window where the
    // client has already read the response but the echo server's own
    // `accept()`-driven counter hasn't incremented yet. Poll briefly rather
    // than asserting immediately.
    let mut waited = Duration::ZERO;
    while echo.current.load(Ordering::SeqCst) != 1 && waited < Duration::from_secs(2) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    }
    assert_eq!(echo.current.load(Ordering::SeqCst), 1, "generation 5's target connection should be open");

    let conn2 = connect(&helper).await;
    let attempt2 = fresh_attempt_id();
    let (mut send2, mut recv2, response2) = tokio::time::timeout(
        Duration::from_secs(10),
        send_hello_and_read_response(&conn2, &session_secret, session_id, ConnectionGeneration::new(6), attempt2),
    )
    .await
    .expect("generation 6 should eventually supersede generation 5 and get a response");
    let attach_token = match response2 {
        AttachResponse::Ready { attach_token, .. } => attach_token,
        other => panic!("expected the superseding generation 6 attempt to get AttachReadyV2, got: {other:?}"),
    };

    // The critical assertion: at no point should the target have seen two
    // concurrently-open connections — the old one must be fully closed
    // before the new one is opened (`AttachArbiter::ClosingForSupersede`).
    assert_eq!(
        echo.max_seen.load(Ordering::SeqCst),
        1,
        "the target must never see more than 1 concurrently-open connection across a supersede"
    );

    let activate =
        AttachActivate { session_id, generation: ConnectionGeneration::new(6), attempt_id: attempt2, attach_token };
    send2.write_all(&encode_attach_activate(&activate)).await.unwrap();
    send2.write_all(b"post-supersede").await.unwrap();
    let mut buf = [0u8; 14];
    tokio::time::timeout(Duration::from_secs(5), recv2.read_exact(&mut buf))
        .await
        .expect("timed out waiting for echo after the supersede completed")
        .unwrap();
    assert_eq!(&buf, b"post-supersede");
}

/// Scenario 11 (`#18-7`): once `Established`, a plain `ATTACH_HELLO` for the
/// *same* session (any generation) is rejected `ATTACH_ALREADY_ESTABLISHED`
/// — the client should use `RESUME` instead, not restart the attach round.
#[tokio::test]
async fn established_session_rejects_a_new_attach_round_as_already_established() {
    let echo = spawn_tracked_echo_server().await;
    let helper = spawn_helper(echo.addr);
    let session_secret =
        base64::engine::general_purpose::STANDARD.decode(&helper.handshake.session_secret).unwrap();
    let session_id = fresh_session_id();

    let conn1 = connect(&helper).await;
    let (_send1, _recv1) =
        attach_and_activate(&conn1, &session_secret, session_id, ConnectionGeneration::INITIAL, fresh_attempt_id())
            .await;

    let conn2 = connect(&helper).await;
    let (_send2, _recv2, response2) = send_hello_and_read_response(
        &conn2,
        &session_secret,
        session_id,
        ConnectionGeneration::new(99),
        fresh_attempt_id(),
    )
    .await;
    assert!(
        matches!(response2, AttachResponse::Reject(AttachRejectReason::AttachAlreadyEstablished)),
        "got: {response2:?}"
    );
}
