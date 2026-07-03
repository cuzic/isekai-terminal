//! isekai-helper の E2E テスト。
//!
//! コンパイル済みの isekai-helper バイナリをサブプロセスとして起動し、
//! ローカル TCP エコーサーバーを --target にして、実際に QUIC 経由で
//! HELLO/ACK ハンドシェイクと双方向リレーが機能することを確認する。
//! 契約の詳細は /HELPER_PROTOCOL.md を参照。

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use hmac::{Hmac, Mac};
use quinn::crypto::rustls::QuicClientConfig;
use quinn::{ClientConfig, Endpoint};
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type HmacSha256 = Hmac<Sha256>;
const EXPORTER_LABEL: &[u8] = b"isekai-helper-auth-v1";
const ALPN: &[u8] = b"isekai-helper/1";
const FRAME_HELLO: u8 = 0x01;
const FRAME_ACK: u8 = 0x02;
const FRAME_REJECT_AUTH: u8 = 0xFF;
const FRAME_REJECT_DUPLICATE: u8 = 0xFE;
const CONTROL_HELLO: u8 = 0x10;
const CONTROL_ACK: u8 = 0x11;
const RESUME: u8 = 0x03;
const RESUME_ACK: u8 = 0x13;
const REJECT_UNKNOWN_SESSION: u8 = 0xF9;
const REJECT_OFFSET_GONE: u8 = 0xF8;

#[derive(Debug, Deserialize)]
struct Handshake {
    #[allow(dead_code)]
    v: u32,
    listen_port: u16,
    cert_sha256: String,
    session_secret: String,
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
    // `cargo test` はテストバイナリと同じ target/{debug,release} に isekai-helper も置く
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // テストバイナリ自身を除く
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("isekai-helper");
    path
}

fn spawn_helper(target: SocketAddr, extra_args: &[&str]) -> HelperProcess {
    let mut cmd = Command::new(helper_bin_path());
    cmd.arg("--target")
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

    let endpoint = make_client_endpoint(&helper.handshake.cert_sha256);
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port)
        .parse()
        .unwrap();
    let conn = endpoint
        .connect(server_addr, "isekai-helper.local")
        .unwrap()
        .await
        .expect("QUIC handshake failed");

    let proof = compute_proof(&conn, &session_secret);
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let mut hello = vec![FRAME_HELLO];
    hello.extend_from_slice(&proof);
    send.write_all(&hello).await.unwrap();

    let mut resp = [0u8; 1];
    recv.read_exact(&mut resp).await.unwrap();
    assert_eq!(resp[0], FRAME_ACK, "expected ACK");

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

    let endpoint = make_client_endpoint(&helper.handshake.cert_sha256);
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port)
        .parse()
        .unwrap();
    let conn = endpoint
        .connect(server_addr, "isekai-helper.local")
        .unwrap()
        .await
        .expect("QUIC handshake failed");

    let bogus_secret = [0xAAu8; 32];
    let proof = compute_proof(&conn, &bogus_secret);
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let mut hello = vec![FRAME_HELLO];
    hello.extend_from_slice(&proof);
    send.write_all(&hello).await.unwrap();

    let mut resp = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(5), recv.read_exact(&mut resp))
        .await
        .expect("timed out waiting for REJECT_AUTH")
        .expect("connection closed before REJECT_AUTH byte was delivered (regression!)");
    assert_eq!(resp[0], FRAME_REJECT_AUTH);
}

#[tokio::test]
async fn duplicate_connection_is_rejected() {
    let echo_addr = spawn_echo_server().await;
    let helper = spawn_helper(echo_addr, &[]);
    let session_secret = base64::engine::general_purpose::STANDARD
        .decode(&helper.handshake.session_secret)
        .unwrap();
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port)
        .parse()
        .unwrap();

    // 1本目: ACK まで進めて能動的にリレー状態にする
    let endpoint1 = make_client_endpoint(&helper.handshake.cert_sha256);
    let conn1 = endpoint1
        .connect(server_addr, "isekai-helper.local")
        .unwrap()
        .await
        .unwrap();
    let proof1 = compute_proof(&conn1, &session_secret);
    let (mut send1, mut recv1) = conn1.open_bi().await.unwrap();
    let mut hello1 = vec![FRAME_HELLO];
    hello1.extend_from_slice(&proof1);
    send1.write_all(&hello1).await.unwrap();
    let mut resp1 = [0u8; 1];
    recv1.read_exact(&mut resp1).await.unwrap();
    assert_eq!(resp1[0], FRAME_ACK);

    // 2本目: 同じ session_secret で proof は正しいが、1本目がまだアクティブなので拒否される
    let endpoint2 = make_client_endpoint(&helper.handshake.cert_sha256);
    let conn2 = endpoint2
        .connect(server_addr, "isekai-helper.local")
        .unwrap()
        .await
        .unwrap();
    let proof2 = compute_proof(&conn2, &session_secret);
    let (mut send2, mut recv2) = conn2.open_bi().await.unwrap();
    let mut hello2 = vec![FRAME_HELLO];
    hello2.extend_from_slice(&proof2);
    send2.write_all(&hello2).await.unwrap();
    let mut resp2 = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(5), recv2.read_exact(&mut resp2))
        .await
        .expect("timed out waiting for REJECT_DUPLICATE")
        .expect("connection closed before REJECT_DUPLICATE byte was delivered");
    assert_eq!(resp2[0], FRAME_REJECT_DUPLICATE);
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
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port)
        .parse()
        .unwrap();

    // 1本目の connection: HELLO/ACK + control stream で session_id を取得し、
    // データを1往復させる（このバイト列を後で「未確認のまま失われた」ことにする）。
    let endpoint1 = make_client_endpoint(&helper.handshake.cert_sha256);
    let conn1 = endpoint1
        .connect(server_addr, "isekai-helper.local")
        .unwrap()
        .await
        .unwrap();
    let proof1 = compute_proof(&conn1, &session_secret);
    let (mut send1, mut recv1) = conn1.open_bi().await.unwrap();
    let mut hello1 = vec![FRAME_HELLO];
    hello1.extend_from_slice(&proof1);
    send1.write_all(&hello1).await.unwrap();
    let mut ack1 = [0u8; 1];
    recv1.read_exact(&mut ack1).await.unwrap();
    assert_eq!(ack1[0], FRAME_ACK);

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
    let endpoint2 = make_client_endpoint(&helper.handshake.cert_sha256);
    let conn2 = endpoint2
        .connect(server_addr, "isekai-helper.local")
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
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port)
        .parse()
        .unwrap();

    let endpoint = make_client_endpoint(&helper.handshake.cert_sha256);
    let conn = endpoint
        .connect(server_addr, "isekai-helper.local")
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
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port)
        .parse()
        .unwrap();

    // 1本目: HELLO/ACK + control stream で本物の session_id を取得する。
    let endpoint1 = make_client_endpoint(&helper.handshake.cert_sha256);
    let conn1 = endpoint1
        .connect(server_addr, "isekai-helper.local")
        .unwrap()
        .await
        .unwrap();
    let proof1 = compute_proof(&conn1, &session_secret);
    let (mut send1, mut recv1) = conn1.open_bi().await.unwrap();
    let mut hello1 = vec![FRAME_HELLO];
    hello1.extend_from_slice(&proof1);
    send1.write_all(&hello1).await.unwrap();
    let mut ack1 = [0u8; 1];
    recv1.read_exact(&mut ack1).await.unwrap();
    assert_eq!(ack1[0], FRAME_ACK);

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
    let endpoint2 = make_client_endpoint(&helper.handshake.cert_sha256);
    let conn2 = endpoint2
        .connect(server_addr, "isekai-helper.local")
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
    let server_addr: SocketAddr = format!("127.0.0.1:{}", helper.handshake.listen_port)
        .parse()
        .unwrap();

    let endpoint1 = make_client_endpoint(&helper.handshake.cert_sha256);
    let conn1 = endpoint1
        .connect(server_addr, "isekai-helper.local")
        .unwrap()
        .await
        .unwrap();
    let proof1 = compute_proof(&conn1, &session_secret);
    let (mut send1, mut recv1) = conn1.open_bi().await.unwrap();
    let mut hello1 = vec![FRAME_HELLO];
    hello1.extend_from_slice(&proof1);
    send1.write_all(&hello1).await.unwrap();
    let mut ack1 = [0u8; 1];
    recv1.read_exact(&mut ack1).await.unwrap();
    assert_eq!(ack1[0], FRAME_ACK);

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

    let endpoint2 = make_client_endpoint(&helper.handshake.cert_sha256);
    let conn2 = endpoint2
        .connect(server_addr, "isekai-helper.local")
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
