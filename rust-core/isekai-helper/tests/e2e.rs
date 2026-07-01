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
