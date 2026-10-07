//! `isekai_transport::WarmStandby` against the **real** `isekai-pipe serve`
//! binary (not a mock): regression test for the standby being silently
//! closed by the server's first-frame `HELLO_TIMEOUT` (5s) — before
//! `isekai_protocol::standby`'s `STANDBY_HOLD` existed, a standby held longer
//! than that could never be promoted, and `warm_standby.rs`'s own mock
//! server (which didn't enforce the deadline) hid it.
//!
//! Per this repo's e2e convention every helper is duplicated here rather
//! than shared with `serve_e2e.rs` (see that file's module docs).

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use hmac::{Hmac, Mac};
use isekai_protocol::attach::{
    attach_hello_proof_transcript, decode_attach_response, encode_attach_activate, encode_attach_hello, AttachActivate,
    AttachHello, AttachProof, AttachResponse, AttemptId, ConnectionGeneration, ATTACH_READY_FRAME_LEN, FRAME_ATTACH_READY,
};
use isekai_protocol::session_id::SessionId;
use isekai_transport::{C2hSentOffset, H2cClientDeliveredOffset, RelayTarget, WarmStandby};
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
const CONTROL_HELLO: u8 = 0x10;
const CONTROL_ACK: u8 = 0x11;
/// `isekai-pipe serve`'s `HELLO_TIMEOUT` (engine/mod.rs).
const SERVER_HELLO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
struct Handshake {
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
    port: Option<u16>,
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

fn spawn_helper(target: SocketAddr) -> HelperProcess {
    let mut child = Command::new(env!("CARGO_BIN_EXE_isekai-pipe"))
        .arg("serve")
        .arg("--target")
        .arg(target.to_string())
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--log-level")
        .arg("debug")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn isekai-pipe serve");
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).expect("failed to read handshake line");
    let handshake: Handshake = serde_json::from_str(line.trim()).expect("failed to parse handshake JSON");
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut r = BufReader::new(stderr);
            let mut buf = String::new();
            while r.read_line(&mut buf).unwrap_or(0) != 0 {
                buf.clear();
            }
        });
    }
    std::mem::forget(reader);
    HelperProcess { child, handshake }
}

async fn spawn_echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
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
            });
        }
    });
    addr
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
        let got: String = Sha256::digest(end_entity.as_ref()).iter().map(|b| format!("{b:02x}")).collect();
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

fn make_client_endpoint(cert_sha256_hex: &str) -> Endpoint {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier { expected_sha256_hex: cert_sha256_hex.to_string() }))
        .with_no_client_auth();
    client_crypto.alpn_protocols = vec![ALPN.to_vec()];
    let client_config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_crypto).unwrap()));
    let mut endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(client_config);
    endpoint
}

fn exporter_hmac(conn: &quinn::Connection, secret: &[u8], extra: &[u8]) -> [u8; 32] {
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"").unwrap();
    let mut mac = HmacSha256::new_from_slice(secret).unwrap();
    mac.update(&exporter);
    mac.update(extra);
    mac.finalize().into_bytes().into()
}

async fn attach_and_activate(
    conn: &quinn::Connection,
    session_secret: &[u8],
    session_id: SessionId,
) -> (quinn::SendStream, quinn::RecvStream) {
    let generation = ConnectionGeneration::INITIAL;
    let mut aid = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut aid);
    let attempt_id = AttemptId::from_bytes(aid);
    let transcript = attach_hello_proof_transcript(&session_id, generation, &attempt_id, 0);
    let proof = AttachProof::new(exporter_hmac(conn, session_secret, &transcript));
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&encode_attach_hello(&AttachHello { session_id, generation, attempt_id, requested_resume_grace_secs: 0, proof }))
        .await
        .unwrap();
    let mut type_byte = [0u8; 1];
    recv.read_exact(&mut type_byte).await.unwrap();
    assert_eq!(type_byte[0], FRAME_ATTACH_READY, "attach must succeed");
    let mut full = vec![type_byte[0]; ATTACH_READY_FRAME_LEN];
    recv.read_exact(&mut full[1..]).await.unwrap();
    let AttachResponse::Ready { attach_token, .. } = decode_attach_response(&full).unwrap() else {
        panic!("expected AttachReadyV2");
    };
    send.write_all(&encode_attach_activate(&AttachActivate { session_id, generation, attempt_id, attach_token })).await.unwrap();
    (send, recv)
}

#[tokio::test]
async fn warm_standby_held_past_the_servers_hello_timeout_is_still_promotable() {
    let echo_addr = spawn_echo_server().await;
    let helper = spawn_helper(echo_addr);
    let session_secret = base64::engine::general_purpose::STANDARD.decode(&helper.handshake.session_secret).unwrap();
    let port = helper
        .handshake
        .candidates
        .iter()
        .find(|c| c.kind == "direct-by-bootstrap-host")
        .and_then(|c| c.port)
        .expect("direct candidate port");
    let server_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let cert = helper.handshake.peer.server_identity.cert_sha256.clone();

    // Primary connection: ATTACH + control stream, then leave echo bytes
    // unread so there's something to replay after promotion.
    let endpoint1 = make_client_endpoint(&cert);
    let conn1 = endpoint1.connect(server_addr, "isekai-pipe.local").unwrap().await.unwrap();
    let mut sid = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut sid);
    let session_id = SessionId::from_bytes(sid);
    let (mut send1, recv1) = attach_and_activate(&conn1, &session_secret, session_id).await;
    let (mut csend1, mut crecv1) = conn1.open_bi().await.unwrap();
    let mut chello = vec![CONTROL_HELLO];
    chello.extend_from_slice(&exporter_hmac(&conn1, &session_secret, &[]));
    csend1.write_all(&chello).await.unwrap();
    let mut cack = [0u8; 17];
    tokio::time::timeout(Duration::from_secs(5), crecv1.read_exact(&mut cack)).await.unwrap().unwrap();
    assert_eq!(cack[0], CONTROL_ACK);

    let payload = b"before-failover";
    send1.write_all(payload).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let standby = WarmStandby::new(
        isekai_transport::system_quic_factory(),
        RelayTarget {
            helper_addr: server_addr,
            server_name: "isekai-pipe.local".to_string(),
            cert_sha256_hex: cert.clone(),
            session_secret: session_secret.clone(),
            local_bind_port_range: None,
        },
        session_id,
    );
    standby.ensure_warm().await.expect("ensure_warm against the real server");
    assert!(standby.is_warm().await);

    // Outlive the server's first-frame deadline: a standby without
    // STANDBY_HOLD would have been closed by now.
    tokio::time::sleep(SERVER_HELLO_TIMEOUT + Duration::from_secs(2)).await;
    standby.ensure_warm().await.expect("the held standby must still answer its probe");
    assert!(standby.is_warm().await);

    // Primary dies.
    conn1.close(0u32.into(), b"simulated primary loss");
    drop((send1, recv1, csend1, crecv1, endpoint1));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let promoted = standby
        .promote(C2hSentOffset::new(payload.len() as u64), H2cClientDeliveredOffset::new(0))
        .await
        .expect("promotion of a standby older than HELLO_TIMEOUT must succeed");
    assert_eq!(promoted.helper_committed_offset.get(), payload.len() as u64);
    let mut stream = promoted.data_stream;
    let mut replayed = vec![0u8; payload.len()];
    let mut filled = 0;
    while filled < replayed.len() {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut replayed[filled..])).await.unwrap().unwrap();
        assert!(n > 0, "stream ended before the replay arrived");
        filled += n;
    }
    assert_eq!(&replayed[..], payload);

    let more = b"after-promotion";
    stream.write_all(more).await.unwrap();
    let mut echoed = vec![0u8; more.len()];
    let mut filled = 0;
    while filled < echoed.len() {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut echoed[filled..])).await.unwrap().unwrap();
        assert!(n > 0, "stream ended before the echo arrived");
        filled += n;
    }
    assert_eq!(&echoed[..], more);
}
