//! `isekai-pipe serve` の E2E テスト(旧 isekai-helper crate から移設、
//! `archive/ISEKAI_PIPE_MIGRATION.md` P5「isekai-pipe serveの独立実装化」)。
//!
//! コンパイル済みの isekai-pipe バイナリを `serve` サブコマンドでサブプロセスとして
//! 起動し、ローカル TCP エコーサーバーを --target にして、実際に QUIC 経由で
//! HELLO/ACK ハンドシェイクと双方向リレーが機能することを確認する。
//! 契約の詳細は /HELPER_PROTOCOL.md を参照。エンジン本体は
//! `isekai-pipe/src/engine/`(旧 `isekai-helper/src/lib.rs`)。

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use hmac::{Hmac, Mac};
use isekai_protocol::attach::{
    attach_hello_proof_transcript, decode_attach_response, encode_attach_activate, encode_attach_hello, AttachActivate,
    AttachHello, AttachProof, AttachResponse, AttemptId, ConnectionGeneration, FRAME_ATTACH_READY,
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
const FRAME_REJECT_AUTH: u8 = 0xFF;
const FRAME_REJECT_BUSY_OTHER_SESSION: u8 = 0xF2;
const CONTROL_HELLO: u8 = 0x10;
const CONTROL_ACK: u8 = 0x11;
const RESUME: u8 = 0x03;
const RESUME_ACK: u8 = 0x13;
const REJECT_UNKNOWN_SESSION: u8 = 0xF9;
const REJECT_OFFSET_GONE: u8 = 0xF8;

/// One client-side attach identity: a fresh `SessionId`, the INITIAL
/// generation, and a fresh `AttemptId` — everything the ATTACH v2 handshake
/// needs the client to pick before connecting (`#18-4`).
fn fresh_attach_ids() -> (SessionId, ConnectionGeneration, AttemptId) {
    let mut sid = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut sid);
    let mut aid = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut aid);
    (SessionId::from_bytes(sid), ConnectionGeneration::INITIAL, AttemptId::from_bytes(aid))
}

/// Computes the ATTACH_HELLO proof exactly like the client:
/// `HMAC-SHA256(session_secret, exporter || attach_hello_proof_transcript(..))`.
fn compute_attach_proof(
    conn: &quinn::Connection,
    secret: &[u8],
    session_id: &SessionId,
    generation: ConnectionGeneration,
    attempt_id: &AttemptId,
    requested_resume_grace_secs: u32,
) -> AttachProof {
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"").unwrap();
    let transcript = attach_hello_proof_transcript(session_id, generation, attempt_id, requested_resume_grace_secs);
    let mut mac = HmacSha256::new_from_slice(secret).unwrap();
    mac.update(&exporter);
    mac.update(&transcript);
    let bytes: [u8; 32] = mac.finalize().into_bytes().into();
    AttachProof::new(bytes)
}

/// Builds an ATTACH_HELLO frame requesting no particular resume-grace
/// preference (`0`) — none of these tests exercise the negotiated value
/// itself, they only need a wire-correct frame.
fn attach_hello_frame(
    session_id: SessionId,
    generation: ConnectionGeneration,
    attempt_id: AttemptId,
    proof: AttachProof,
) -> Vec<u8> {
    encode_attach_hello(&AttachHello {
        session_id,
        generation,
        attempt_id,
        requested_resume_grace_secs: 0,
        proof,
    })
}

/// Reads a full `AttachResponse` off the wire using the same two-step read the
/// real client uses (`isekai-transport::relay::read_attach_response`): the type
/// byte first, then — only for `FRAME_ATTACH_READY` / `FRAME_REJECT_STALE_GENERATION`
/// — the remaining bytes; every other reject byte is a bare single byte.
async fn read_attach_response(recv: &mut quinn::RecvStream) -> AttachResponse {
    let mut type_byte = [0u8; 1];
    recv.read_exact(&mut type_byte).await.unwrap();
    let mut full = vec![type_byte[0]];
    let extra_len = match type_byte[0] {
        FRAME_ATTACH_READY => {
            isekai_protocol::attach::ATTACH_READY_FRAME_LEN - 1
        }
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

/// Drives the full happy-path attach on `conn`'s data stream: sends
/// ATTACH_HELLO, expects AttachReadyV2, sends the matching AttachActivate, and
/// returns the open stream halves ready for raw relay. Panics if the server
/// rejects.
async fn attach_and_activate(
    conn: &quinn::Connection,
    session_secret: &[u8],
    session_id: SessionId,
    generation: ConnectionGeneration,
    attempt_id: AttemptId,
) -> (quinn::SendStream, quinn::RecvStream) {
    let proof = compute_attach_proof(conn, session_secret, &session_id, generation, &attempt_id, 0);
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&attach_hello_frame(session_id, generation, attempt_id, proof)).await.unwrap();
    let attach_token = match read_attach_response(&mut recv).await {
        AttachResponse::Ready { attach_token, .. } => attach_token,
        other => panic!("expected AttachReadyV2, got {other:?}"),
    };
    let activate = AttachActivate { session_id, generation, attempt_id, attach_token };
    send.write_all(&encode_attach_activate(&activate)).await.unwrap();
    (send, recv)
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

    fn stun_observed_addr(&self) -> Option<&str> {
        self.candidates
            .iter()
            .find(|candidate| candidate.kind == "server-reflexive")
            .and_then(|candidate| candidate.endpoint.as_deref())
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
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
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

fn spawn_helper(target: SocketAddr, extra_args: &[&str]) -> HelperProcess {
    let mut cmd = Command::new(helper_bin_path());
    cmd.arg("serve")
        .arg("--target")
        .arg(target.to_string())
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--log-level")
        .arg("debug")
        .args(extra_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn isekai-helper");
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("failed to read handshake line from isekai-helper stdout");
    let handshake: Handshake =
        serde_json::from_str(line.trim()).expect("failed to parse handshake JSON");

    // 残りの stdout/stderr は捨てるが、プロセスの stderr がブロックしないよう drain しておく。
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
    // stdout の残りも drain
    std::mem::forget(reader);

    HelperProcess { child, handshake }
}

async fn spawn_echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
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

fn make_client_endpoint(cert_sha256_hex: &str) -> Endpoint {
    let mut client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
            expected_sha256_hex: cert_sha256_hex.to_string(),
        }))
        .with_no_client_auth();
    client_crypto.alpn_protocols = vec![ALPN.to_vec()];
    let client_config =
        ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_crypto).unwrap()));
    let mut endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(client_config);
    endpoint
}

fn compute_proof(conn: &quinn::Connection, secret: &[u8]) -> [u8; 32] {
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"")
        .unwrap();
    let mut mac = HmacSha256::new_from_slice(secret).unwrap();
    mac.update(&exporter);
    mac.finalize().into_bytes().into()
}

#[tokio::test]
async fn hello_ack_and_relay_roundtrip() {
    let echo_addr = spawn_echo_server().await;
    let helper = spawn_helper(echo_addr, &[]);
    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&helper.handshake.session_secret)
        .unwrap();

    let endpoint = make_client_endpoint(helper.handshake.cert_sha256());
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap())
        .parse()
        .unwrap();
    let conn = endpoint
        .connect(server_addr, "isekai-pipe.local")
        .unwrap()
        .await
        .expect("QUIC handshake failed");

    let (sid, generation, aid) = fresh_attach_ids();
    let (mut send, mut recv) = attach_and_activate(&conn, &session_secret, sid, generation, aid).await;

    let payload = b"hello-isekai-helper-e2e-test";
    send.write_all(payload).await.unwrap();
    let mut buf = vec![0u8; payload.len()];
    tokio::time::timeout(Duration::from_secs(5), recv.read_exact(&mut buf))
        .await
        .expect("timed out waiting for echo")
        .unwrap();
    assert_eq!(&buf[..], payload);

    send.finish().unwrap();
}

#[tokio::test]
async fn wrong_proof_is_rejected_before_connection_closes() {
    let echo_addr = spawn_echo_server().await;
    let helper = spawn_helper(echo_addr, &[]);

    let endpoint = make_client_endpoint(helper.handshake.cert_sha256());
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap())
        .parse()
        .unwrap();
    let conn = endpoint
        .connect(server_addr, "isekai-pipe.local")
        .unwrap()
        .await
        .expect("QUIC handshake failed");

    // A well-formed ATTACH_HELLO whose proof was computed with the wrong
    // secret — so the server reaches the proof check and rejects with
    // REJECT_AUTH (0xFF, unchanged from v1), not a decode/Unsupported failure.
    let (sid, generation, aid) = fresh_attach_ids();
    let bogus_secret = [0xAAu8; 32];
    let proof = compute_attach_proof(&conn, &bogus_secret, &sid, generation, &aid, 0);
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&attach_hello_frame(sid, generation, aid, proof)).await.unwrap();

    let mut resp = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(5), recv.read_exact(&mut resp))
        .await
        .expect("timed out waiting for REJECT_AUTH")
        .expect("connection closed before REJECT_AUTH byte was delivered (regression!)");
    assert_eq!(resp[0], FRAME_REJECT_AUTH);
}

/// ATTACH v2 (`#18`): a single `isekai-pipe serve` instance only ever serves
/// one logical session at a time. Once conn1 has reached `Established` (full
/// ATTACH_HELLO → AttachReadyV2 → AttachActivate), a second, independent client
/// (its own fresh `session_id`) attempting to attach is rejected with
/// `BusyOtherSession` (0xF2). This is the faithful modernization of the old v1
/// "duplicate connection" test, whose `REJECT_DUPLICATE` (0xFE) byte no longer
/// exists — a genuinely different client can't co-opt this server instance.
#[tokio::test]
async fn duplicate_connection_is_rejected() {
    let echo_addr = spawn_echo_server().await;
    let helper = spawn_helper(echo_addr, &[]);
    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&helper.handshake.session_secret)
        .unwrap();
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap())
        .parse()
        .unwrap();

    // 1本目: ATTACH_HELLO → AttachReadyV2 → AttachActivate まで進めて `Established`
    // にする(AttachActivate を送らないと slot は `PendingActivation` のままで、
    // 2本目の衝突が別の reject 理由になってしまう)。
    let endpoint1 = make_client_endpoint(helper.handshake.cert_sha256());
    let conn1 = endpoint1
        .connect(server_addr, "isekai-pipe.local")
        .unwrap()
        .await
        .unwrap();
    let (sid1, gen1, aid1) = fresh_attach_ids();
    let (_send1, _recv1) = attach_and_activate(&conn1, &session_secret, sid1, gen1, aid1).await;

    // 2本目: proof 自体は正しいが、別の(独立した)session_id で attach しようと
    // するので、この serve インスタンスが既に別 session を抱えていることを理由に
    // `BusyOtherSession` で拒否される。
    let endpoint2 = make_client_endpoint(helper.handshake.cert_sha256());
    let conn2 = endpoint2
        .connect(server_addr, "isekai-pipe.local")
        .unwrap()
        .await
        .unwrap();
    let (sid2, gen2, aid2) = fresh_attach_ids();
    let proof2 = compute_attach_proof(&conn2, &session_secret, &sid2, gen2, &aid2, 0);
    let (mut send2, mut recv2) = conn2.open_bi().await.unwrap();
    send2.write_all(&attach_hello_frame(sid2, gen2, aid2, proof2)).await.unwrap();
    let mut resp2 = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(5), recv2.read_exact(&mut resp2))
        .await
        .expect("timed out waiting for REJECT_BUSY_OTHER_SESSION")
        .expect("connection closed before REJECT_BUSY_OTHER_SESSION byte was delivered");
    assert_eq!(resp2[0], FRAME_REJECT_BUSY_OTHER_SESSION);
}

/// Phase 8-3: QUIC connection が失われた後、`RESUME` で reattach すると
/// 未確認だった S→C データが再送され、その後も同じ TCP 接続で中継が
/// 継続することを確認する。
#[tokio::test]
async fn resume_after_connection_loss_replays_and_continues() {
    let echo_addr = spawn_echo_server().await;
    let helper = spawn_helper(echo_addr, &[]);
    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&helper.handshake.session_secret)
        .unwrap();
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap())
        .parse()
        .unwrap();

    // 1本目の connection: HELLO/ACK + control stream で session_id を取得し、
    // データを1往復させる（このバイト列を後で「未確認のまま失われた」ことにする）。
    let endpoint1 = make_client_endpoint(helper.handshake.cert_sha256());
    let conn1 = endpoint1
        .connect(server_addr, "isekai-pipe.local")
        .unwrap()
        .await
        .unwrap();
    let (sid1, gen1, aid1) = fresh_attach_ids();
    let (mut send1, recv1) = attach_and_activate(&conn1, &session_secret, sid1, gen1, aid1).await;

    // CONTROL_HELLO の proof はプレーンな exporter HMAC(transcript 無し)のままで、
    // ATTACH v2 でも変更されていない。
    let proof1 = compute_proof(&conn1, &session_secret);
    let (mut csend1, mut crecv1) = conn1.open_bi().await.unwrap();
    let mut chello1 = vec![CONTROL_HELLO];
    chello1.extend_from_slice(&proof1);
    csend1.write_all(&chello1).await.unwrap();
    let mut cack1 = [0u8; 17];
    tokio::time::timeout(Duration::from_secs(5), crecv1.read_exact(&mut cack1))
        .await
        .expect("timed out waiting for CONTROL_ACK")
        .unwrap();
    assert_eq!(cack1[0], CONTROL_ACK);
    let session_id = cack1[1..17].to_vec();

    let payload = b"before-disconnect";
    send1.write_all(payload).await.unwrap();
    // わざと echo を読み切らない（= client_delivered_offset は 0 のまま）。
    // helper 側の output_buffer にはこの echo バイト列が残っているはず。
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 1本目の connection を明示的に閉じ、ネットワーク断を模す。
    conn1.close(0u32.into(), b"simulated network loss");
    drop(send1);
    drop(recv1);
    drop(csend1);
    drop(crecv1);
    drop(endpoint1);

    // helper 側が data stream の切断を検知して session を park するまで少し待つ。
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 2本目の connection: 新しい QUIC connection から RESUME で reattach する。
    let endpoint2 = make_client_endpoint(helper.handshake.cert_sha256());
    let conn2 = endpoint2
        .connect(server_addr, "isekai-pipe.local")
        .unwrap()
        .await
        .expect("second QUIC handshake failed");

    let mut exporter2 = [0u8; 32];
    conn2
        .export_keying_material(&mut exporter2, EXPORTER_LABEL, b"")
        .unwrap();
    let mut mac = HmacSha256::new_from_slice(&session_secret).unwrap();
    mac.update(&exporter2);
    mac.update(&session_id);
    let resume_proof = mac.finalize().into_bytes();

    let (mut send2, mut recv2) = conn2.open_bi().await.unwrap();
    let mut resume_frame = vec![RESUME];
    resume_frame.extend_from_slice(&session_id);
    resume_frame.extend_from_slice(&resume_proof);
    resume_frame.extend_from_slice(&(payload.len() as u64).to_be_bytes()); // client_sent_offset
    resume_frame.extend_from_slice(&0u64.to_be_bytes()); // client_delivered_offset（何も受け取れていない）
    send2.write_all(&resume_frame).await.unwrap();

    let mut resume_ack = [0u8; 17];
    tokio::time::timeout(Duration::from_secs(5), recv2.read_exact(&mut resume_ack))
        .await
        .expect("timed out waiting for RESUME_ACK")
        .expect("connection closed before RESUME_ACK was delivered");
    assert_eq!(resume_ack[0], RESUME_ACK, "expected RESUME_ACK");
    let helper_committed_offset = u64::from_be_bytes(resume_ack[1..9].try_into().unwrap());
    let helper_sent_offset = u64::from_be_bytes(resume_ack[9..17].try_into().unwrap());
    assert_eq!(helper_committed_offset, payload.len() as u64, "C->S は全部 committed 済みのはず");
    assert_eq!(helper_sent_offset, payload.len() as u64, "echo された分だけ S->C も進んでいるはず");

    // 未確認だった echo バイト列がそのまま再送されてくるはず。
    let mut replayed = vec![0u8; payload.len()];
    tokio::time::timeout(Duration::from_secs(5), recv2.read_exact(&mut replayed))
        .await
        .expect("timed out waiting for replayed bytes")
        .unwrap();
    assert_eq!(&replayed[..], payload, "reattach 後に未確認の echo データが再送されるはず");

    // reattach 後も同じ TCP 接続で中継が継続することを確認する。
    let more = b"after-resume";
    send2.write_all(more).await.unwrap();
    let mut more_echo = vec![0u8; more.len()];
    tokio::time::timeout(Duration::from_secs(5), recv2.read_exact(&mut more_echo))
        .await
        .expect("timed out waiting for post-resume echo")
        .unwrap();
    assert_eq!(&more_echo[..], more, "resume 後も中継が継続するはず");

    send2.finish().unwrap();
}

/// Phase 8-4: 存在しない（あるいは既に sweep 済みの）session_id で `RESUME` を
/// 送ると `REJECT_UNKNOWN_SESSION` が返ることを確認する。session_id 自体は
/// でたらめだが proof はその connection の exporter から正しく計算するので、
/// 認証エラー（`FRAME_REJECT_AUTH`）ではなく確実に「session不明」の分岐を通す。
#[tokio::test]
async fn resume_with_unknown_session_id_is_rejected() {
    let echo_addr = spawn_echo_server().await;
    let helper = spawn_helper(echo_addr, &[]);
    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&helper.handshake.session_secret)
        .unwrap();
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap())
        .parse()
        .unwrap();

    let endpoint = make_client_endpoint(helper.handshake.cert_sha256());
    let conn = endpoint
        .connect(server_addr, "isekai-pipe.local")
        .unwrap()
        .await
        .expect("QUIC handshake failed");

    // 一度も HELLO していない、存在しない session_id ででたらめに reattach を試みる。
    let bogus_session_id = [0x42u8; 16];
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"")
        .unwrap();
    let mut mac = HmacSha256::new_from_slice(&session_secret).unwrap();
    mac.update(&exporter);
    mac.update(&bogus_session_id);
    let resume_proof = mac.finalize().into_bytes();

    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let mut resume_frame = vec![RESUME];
    resume_frame.extend_from_slice(&bogus_session_id);
    resume_frame.extend_from_slice(&resume_proof);
    resume_frame.extend_from_slice(&0u64.to_be_bytes());
    resume_frame.extend_from_slice(&0u64.to_be_bytes());
    send.write_all(&resume_frame).await.unwrap();

    let mut resp = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(5), recv.read_exact(&mut resp))
        .await
        .expect("timed out waiting for REJECT_UNKNOWN_SESSION")
        .expect("connection closed before REJECT_UNKNOWN_SESSION byte was delivered");
    assert_eq!(resp[0], REJECT_UNKNOWN_SESSION);
}

/// Phase 8-4: `RESUME` の `client_delivered_offset` が helper の output buffer に
/// 存在する範囲を超えている（＝存在しないはずの未来のバイト列を要求している）
/// 場合に `REJECT_OFFSET_GONE` が返ることを確認する。バッファ溢れを実際に
/// 起こすには数MBのデータが要るため、代わりに `end_offset` を超える不正な
/// offset を送ることで `replay_from` の同じ `None` 分岐を軽量に検証する。
#[tokio::test]
async fn resume_with_offset_beyond_buffer_is_rejected() {
    let echo_addr = spawn_echo_server().await;
    let helper = spawn_helper(echo_addr, &[]);
    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&helper.handshake.session_secret)
        .unwrap();
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap())
        .parse()
        .unwrap();

    // 1本目: HELLO/ACK + control stream で本物の session_id を取得する。
    let endpoint1 = make_client_endpoint(helper.handshake.cert_sha256());
    let conn1 = endpoint1
        .connect(server_addr, "isekai-pipe.local")
        .unwrap()
        .await
        .unwrap();
    let (sid1, gen1, aid1) = fresh_attach_ids();
    let (mut send1, mut recv1) = attach_and_activate(&conn1, &session_secret, sid1, gen1, aid1).await;

    // CONTROL_HELLO の proof はプレーンな exporter HMAC(transcript 無し)のままで、
    // ATTACH v2 でも変更されていない。
    let proof1 = compute_proof(&conn1, &session_secret);
    let (mut csend1, mut crecv1) = conn1.open_bi().await.unwrap();
    let mut chello1 = vec![CONTROL_HELLO];
    chello1.extend_from_slice(&proof1);
    csend1.write_all(&chello1).await.unwrap();
    let mut cack1 = [0u8; 17];
    tokio::time::timeout(Duration::from_secs(5), crecv1.read_exact(&mut cack1))
        .await
        .expect("timed out waiting for CONTROL_ACK")
        .unwrap();
    assert_eq!(cack1[0], CONTROL_ACK);
    let session_id = cack1[1..17].to_vec();

    let payload = b"short";
    send1.write_all(payload).await.unwrap();
    let mut echoed = vec![0u8; payload.len()];
    tokio::time::timeout(Duration::from_secs(5), recv1.read_exact(&mut echoed))
        .await
        .expect("timed out waiting for echo")
        .unwrap();
    assert_eq!(&echoed[..], payload);

    conn1.close(0u32.into(), b"simulated network loss");
    drop(send1);
    drop(recv1);
    drop(csend1);
    drop(crecv1);
    drop(endpoint1);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 2本目: 実際に送出済みのバイト数（= end_offset）をはるかに超える
    // client_delivered_offset を主張して reattach を試みる。
    let endpoint2 = make_client_endpoint(helper.handshake.cert_sha256());
    let conn2 = endpoint2
        .connect(server_addr, "isekai-pipe.local")
        .unwrap()
        .await
        .expect("second QUIC handshake failed");

    let mut exporter2 = [0u8; 32];
    conn2
        .export_keying_material(&mut exporter2, EXPORTER_LABEL, b"")
        .unwrap();
    let mut mac = HmacSha256::new_from_slice(&session_secret).unwrap();
    mac.update(&exporter2);
    mac.update(&session_id);
    let resume_proof = mac.finalize().into_bytes();

    let (mut send2, mut recv2) = conn2.open_bi().await.unwrap();
    let mut resume_frame = vec![RESUME];
    resume_frame.extend_from_slice(&session_id);
    resume_frame.extend_from_slice(&resume_proof);
    resume_frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    resume_frame.extend_from_slice(&1_000_000u64.to_be_bytes()); // 存在しない未来の offset
    send2.write_all(&resume_frame).await.unwrap();

    let mut resp = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(5), recv2.read_exact(&mut resp))
        .await
        .expect("timed out waiting for REJECT_OFFSET_GONE")
        .expect("connection closed before REJECT_OFFSET_GONE byte was delivered");
    assert_eq!(resp[0], REJECT_OFFSET_GONE);
}

/// Phase 8-4: 長時間圏外（`--resume-window` を超えて park されたまま）になった
/// session は `sweep_expired_parked` により自動的に破棄され、その後の
/// `RESUME` は `REJECT_UNKNOWN_SESSION` になることを確認する。
/// 定期掃除タスクは 5 秒間隔で走るので、`--resume-window` を短く設定して
/// 現実的な待ち時間でテストする（Phase 8-4b の実機検証で `--idle-timeout` と
/// `--resume-window` を分離したため、park の寿命はこちらで制御する）。
#[tokio::test]
async fn resume_after_park_expiry_is_rejected_as_unknown_session() {
    let echo_addr = spawn_echo_server().await;
    let helper = spawn_helper(echo_addr, &["--resume-window", "2"]);
    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&helper.handshake.session_secret)
        .unwrap();
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.direct_by_bootstrap_host_port().unwrap())
        .parse()
        .unwrap();

    let endpoint1 = make_client_endpoint(helper.handshake.cert_sha256());
    let conn1 = endpoint1
        .connect(server_addr, "isekai-pipe.local")
        .unwrap()
        .await
        .unwrap();
    let (sid1, gen1, aid1) = fresh_attach_ids();
    let (send1, recv1) = attach_and_activate(&conn1, &session_secret, sid1, gen1, aid1).await;

    // CONTROL_HELLO の proof はプレーンな exporter HMAC(transcript 無し)のままで、
    // ATTACH v2 でも変更されていない。
    let proof1 = compute_proof(&conn1, &session_secret);
    let (mut csend1, mut crecv1) = conn1.open_bi().await.unwrap();
    let mut chello1 = vec![CONTROL_HELLO];
    chello1.extend_from_slice(&proof1);
    csend1.write_all(&chello1).await.unwrap();
    let mut cack1 = [0u8; 17];
    tokio::time::timeout(Duration::from_secs(5), crecv1.read_exact(&mut cack1))
        .await
        .expect("timed out waiting for CONTROL_ACK")
        .unwrap();
    assert_eq!(cack1[0], CONTROL_ACK);
    let session_id = cack1[1..17].to_vec();

    // データ交換なしですぐに切断し、park させる。
    conn1.close(0u32.into(), b"simulated long outage");
    drop(send1);
    drop(recv1);
    drop(csend1);
    drop(crecv1);
    drop(endpoint1);

    // sweep 間隔(5秒) + resume-window(2秒) を十分に超えるまで待つ。
    tokio::time::sleep(Duration::from_secs(9)).await;

    let endpoint2 = make_client_endpoint(helper.handshake.cert_sha256());
    let conn2 = endpoint2
        .connect(server_addr, "isekai-pipe.local")
        .unwrap()
        .await
        .expect("second QUIC handshake failed");

    let mut exporter2 = [0u8; 32];
    conn2
        .export_keying_material(&mut exporter2, EXPORTER_LABEL, b"")
        .unwrap();
    let mut mac = HmacSha256::new_from_slice(&session_secret).unwrap();
    mac.update(&exporter2);
    mac.update(&session_id);
    let resume_proof = mac.finalize().into_bytes();

    let (mut send2, mut recv2) = conn2.open_bi().await.unwrap();
    let mut resume_frame = vec![RESUME];
    resume_frame.extend_from_slice(&session_id);
    resume_frame.extend_from_slice(&resume_proof);
    resume_frame.extend_from_slice(&0u64.to_be_bytes());
    resume_frame.extend_from_slice(&0u64.to_be_bytes());
    send2.write_all(&resume_frame).await.unwrap();

    let mut resp = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(5), recv2.read_exact(&mut resp))
        .await
        .expect("timed out waiting for REJECT_UNKNOWN_SESSION")
        .expect("connection closed before REJECT_UNKNOWN_SESSION byte was delivered");
    assert_eq!(
        resp[0], REJECT_UNKNOWN_SESSION,
        "park 期限切れで sweep された session は unknown 扱いになるはず"
    );
}

// ── STUN(--stun-server)ハンドシェイク拡張 ─────────────────────────────

/// 最小のモックSTUNサーバー(RFC 5389 Binding Request/Response)。
/// `isekai-stun`クレート自身のテストと同じ手法: 受け取ったBinding Requestの
/// 送信元アドレスをそのままXOR-MAPPED-ADDRESSとして返すだけ。
/// Runs on a dedicated OS thread with a *blocking* `std::net::UdpSocket`
/// (same technique this file already uses for stderr-draining, see
/// `spawn_helper`) rather than as a task on the test's own tokio runtime.
/// `spawn_helper()` below blocks the calling thread on a synchronous
/// `read_line()` with no `.await` point, so under the default (single-
/// threaded) `#[tokio::test]` runtime a `tokio::spawn`-based mock server
/// would never actually get polled while `spawn_helper()` is running —
/// exactly when isekai-helper's own bounded-retry STUN query needs a
/// response. A plain OS thread has no such dependency on the test's own
/// executor.
fn spawn_mock_stun_server() -> SocketAddr {
    let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        loop {
            let Ok((n, from)) = server.recv_from(&mut buf) else { break };
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
            resp.extend_from_slice(&12u16.to_be_bytes()); // message length: 4 (attr header) + 8 (attr value)
            resp.extend_from_slice(&magic_cookie.to_be_bytes());
            resp.extend_from_slice(transaction_id);
            resp.extend_from_slice(&0x0020u16.to_be_bytes()); // XOR-MAPPED-ADDRESS
            resp.extend_from_slice(&8u16.to_be_bytes());
            resp.push(0);
            resp.push(0x01); // family: IPv4
            resp.extend_from_slice(&xport.to_be_bytes());
            resp.extend_from_slice(&xaddr.to_be_bytes());

            let _ = server.send_to(&resp, from);
        }
    });
    addr
}

#[tokio::test]
async fn stun_server_flag_populates_observed_address_in_handshake() {
    let echo_addr = spawn_echo_server().await;
    let stun_server = spawn_mock_stun_server();

    let helper = spawn_helper(echo_addr, &["--stun-server", &stun_server.to_string()]);

    let observed: SocketAddr = helper
        .handshake
        .stun_observed_addr()
        .expect("stun_observed_addr should be populated when --stun-server is given")
        .parse()
        .expect("stun_observed_addr should be a valid socket address");

    // ループバック経由なのでNATによるアドレス変換は起きない。STUNサーバーから見えた
    // ポートは、実際にQUICが待ち受けているポート(handshake.direct_by_bootstrap_host_port())と一致する
    // はず——これは「STUN問い合わせとQUIC待受が本当に同じソケットを共有している」
    // ことの直接的な証拠になる。
    assert_eq!(observed.ip(), std::net::Ipv4Addr::LOCALHOST);
    assert_eq!(observed.port(), helper.handshake.direct_by_bootstrap_host_port().unwrap());
}

#[tokio::test]
async fn without_stun_server_flag_handshake_has_no_observed_address() {
    let echo_addr = spawn_echo_server().await;
    let helper = spawn_helper(echo_addr, &[]);
    assert!(helper.handshake.stun_observed_addr().is_none());
}

#[tokio::test]
async fn punch_peer_flag_does_not_prevent_normal_startup_or_relay() {
    let echo_addr = spawn_echo_server().await;
    let stun_server = spawn_mock_stun_server();
    // 実在しないダミーの相手アドレス宛にprobeを送るだけなので応答は来ないが、
    // fire-and-forgetであり、起動・通常のHELLO/ACK/リレー自体は妨げないはず。
    let dummy_peer = "127.0.0.1:1".to_string();

    let helper = spawn_helper(
        echo_addr,
        &["--stun-server", &stun_server.to_string(), "--punch-peer", &dummy_peer],
    );
    assert!(helper.handshake.stun_observed_addr().is_some());

    let client_endpoint = make_client_endpoint(helper.handshake.cert_sha256());
    let conn = client_endpoint
        .connect(
            SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), helper.handshake.direct_by_bootstrap_host_port().unwrap()),
            "isekai-pipe.local",
        )
        .unwrap()
        .await
        .expect("QUIC handshake should still succeed with --punch-peer set");

    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&helper.handshake.session_secret)
        .unwrap();
    // Only checks that startup + the ATTACH handshake reaches AttachReadyV2
    // (mirrors the original scope of "just prove --punch-peer doesn't break
    // the ACK"); no AttachActivate / full relay needed here.
    let (sid, generation, aid) = fresh_attach_ids();
    let proof = compute_attach_proof(&conn, &session_secret, &sid, generation, &aid, 0);
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&attach_hello_frame(sid, generation, aid, proof)).await.unwrap();

    let response = tokio::time::timeout(Duration::from_secs(5), read_attach_response(&mut recv))
        .await
        .expect("timed out waiting for AttachReadyV2");
    assert!(matches!(response, AttachResponse::Ready { .. }), "expected AttachReadyV2, got {response:?}");
}

/// `#20a-4`: when launched with `--bootstrap-request-file` (the real
/// `isekai-bootstrap::openssh` call shape, `#20a-2`), `isekai-pipe serve`
/// must wrap its handshake in a `BootstrapReportV2` envelope echoing back
/// the request's `session_id`/`bootstrap_attempt_id`, rather than emitting
/// the bare `HandshakeJson` line every other test in this file expects
/// (`spawn_helper` deliberately never passes this flag).
#[tokio::test]
async fn bootstrap_request_file_wraps_handshake_in_a_bootstrap_report_v2() {
    let echo_addr = spawn_echo_server().await;

    let tmp = tempfile::tempdir().unwrap();
    let request_path = tmp.path().join("bootstrap-request.json");
    let request = isekai_protocol::BootstrapRequestV2 {
        v: isekai_protocol::BOOTSTRAP_PROTOCOL_V2,
        session_id: SessionId::from_bytes([0x55; 16]).to_hex(),
        bootstrap_attempt_id: "66".repeat(16),
        client_candidates: vec![],
    };
    std::fs::write(&request_path, serde_json::to_vec(&request).unwrap()).unwrap();

    let mut cmd = Command::new(helper_bin_path());
    cmd.arg("serve")
        .arg("--target")
        .arg(echo_addr.to_string())
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--log-level")
        .arg("debug")
        .arg("--bootstrap-request-file")
        .arg(request_path.to_str().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("failed to spawn isekai-pipe");

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("failed to read bootstrap report line from isekai-pipe stdout");

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

    let _ = child.kill();
    let _ = child.wait();

    let report: isekai_protocol::BootstrapReportV2 =
        serde_json::from_str(line.trim()).expect("stdout line should be a valid BootstrapReportV2");
    assert_eq!(report.v, isekai_protocol::BOOTSTRAP_PROTOCOL_V2);
    assert_eq!(report.session_id, request.session_id);
    assert_eq!(report.bootstrap_attempt_id, request.bootstrap_attempt_id);
    assert_eq!(report.handshake.v, 1);
    assert!(report.handshake.direct_by_bootstrap_host_port().is_some());
}

/// `#20a-5`: the full `#20a` stack in one test — a `BootstrapRequestV2`
/// carrying a real `client_candidates` entry actually gets punched (not just
/// parsed), and the resulting `BootstrapReportV2`'s wrapped handshake is
/// genuinely usable for a real ATTACH v2 QUIC connection, not just
/// well-formed JSON. Ties `#20a-3` (candidate punch) and `#20a-4` (report
/// wrap) together against the real compiled `isekai-pipe` binary, the same
/// way `punch_peer_flag_does_not_prevent_normal_startup_or_relay` already
/// does for the pre-`#20a` `--punch-peer` flag.
#[tokio::test]
async fn bootstrap_request_file_candidates_are_punched_and_the_report_yields_a_working_connection() {
    let echo_addr = spawn_echo_server().await;
    let stun_server = spawn_mock_stun_server();

    // Stands in for the "peer" `isekai-terminal` would be punching toward —
    // a plain UDP socket that just needs to observe at least one probe
    // datagram, proving `client_candidate_punch_targets` actually reached
    // the punch loop rather than only being parsed and discarded.
    let peer_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer_socket.local_addr().unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let request_path = tmp.path().join("bootstrap-request.json");
    let request = isekai_protocol::BootstrapRequestV2 {
        v: isekai_protocol::BOOTSTRAP_PROTOCOL_V2,
        session_id: SessionId::from_bytes([0x77; 16]).to_hex(),
        bootstrap_attempt_id: "88".repeat(16),
        client_candidates: vec![isekai_protocol::BootstrapCandidateV2 {
            route: "stun-p2p".to_string(),
            endpoint: peer_addr.to_string(),
            valid_for_ms: 30_000,
        }],
    };
    std::fs::write(&request_path, serde_json::to_vec(&request).unwrap()).unwrap();

    let mut cmd = Command::new(helper_bin_path());
    cmd.arg("serve")
        .arg("--target")
        .arg(echo_addr.to_string())
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--log-level")
        .arg("debug")
        .arg("--stun-server")
        .arg(stun_server.to_string())
        .arg("--bootstrap-request-file")
        .arg(request_path.to_str().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("failed to spawn isekai-pipe");

    let mut punch_buf = [0u8; 64];
    let (n, _) = tokio::time::timeout(Duration::from_secs(5), peer_socket.recv_from(&mut punch_buf))
        .await
        .expect("timed out waiting for a hole-punch probe from the bootstrap candidate")
        .expect("recv_from failed");
    assert_eq!(&punch_buf[..n], b"isekai-punch");

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("failed to read bootstrap report line from isekai-pipe stdout");
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

    let report: isekai_protocol::BootstrapReportV2 =
        serde_json::from_str(line.trim()).expect("stdout line should be a valid BootstrapReportV2");
    assert_eq!(report.session_id, request.session_id);
    assert_eq!(report.bootstrap_attempt_id, request.bootstrap_attempt_id);
    assert!(report.handshake.stun_observed_addr().is_some());

    let client_endpoint = make_client_endpoint(report.handshake.cert_sha256());
    let conn = client_endpoint
        .connect(
            SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), report.handshake.direct_by_bootstrap_host_port().unwrap()),
            "isekai-pipe.local",
        )
        .unwrap()
        .await
        .expect("QUIC handshake should succeed using the wrapped BootstrapReportV2's handshake");

    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&report.handshake.session_secret)
        .unwrap();
    let (sid, generation, aid) = fresh_attach_ids();
    let proof = compute_attach_proof(&conn, &session_secret, &sid, generation, &aid, 0);
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&attach_hello_frame(sid, generation, aid, proof)).await.unwrap();

    let response = tokio::time::timeout(Duration::from_secs(5), read_attach_response(&mut recv))
        .await
        .expect("timed out waiting for AttachReadyV2");
    assert!(matches!(response, AttachResponse::Ready { .. }), "expected AttachReadyV2, got {response:?}");

    let _ = child.kill();
    let _ = child.wait();
}
