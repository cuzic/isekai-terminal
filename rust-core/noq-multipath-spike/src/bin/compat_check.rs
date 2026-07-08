//! Phase 9-0: protocol-compatibility spike (see /home/cuzic/.claude/plans/typed-dancing-codd.md).
//!
//! Proves, before touching any production crate, that:
//!  1. HELPER_PROTOCOL.md's HELLO/ACK/proof contract (cert pinning via a custom
//!     `ServerCertVerifier`, `export_keying_material`-based HMAC proof, 0x01/0x02/0xFF
//!     frame bytes) is reproducible on top of `noq` instead of `quinn` -- both
//!     client and server side.
//!  2. A plain, unmodified `quinn` client (the exact shape used by
//!     `isekai_pipe_quic_transport.rs::establish_quic_connection`) can still complete
//!     a QUIC handshake and the HELLO/ACK exchange against a `noq` server. This
//!     is the backward-compatibility premise the Phase 9 plan depends on: if
//!     this holds, isekai-helper can move to a single noq-based listener without
//!     touching the existing quinn-based Phase 7/8 client code at all.
//!  3. `noq` multipath itself still works end-to-end (path0 + path1 + failover)
//!     wrapped in this exact protocol, on loopback (127.0.0.1 / [::1] dual-stack,
//!     no real Tailscale/direct addresses or Android fds needed).
//!
//! Isolated in `noq-multipath-spike` on purpose -- `noq` is not a dependency of
//! `isekai-terminal-core`/`isekai-helper` yet (see `jni_bridge.rs` doc comment).

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use hmac::{Hmac, Mac};
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const EXPORTER_LABEL: &[u8] = b"isekai-pipe-auth-v1";
const ALPN: &[u8] = b"isekai-pipe/1";
const FRAME_HELLO: u8 = 0x01;
const FRAME_ACK: u8 = 0x02;
const FRAME_REJECT_AUTH: u8 = 0xFF;

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ── noq server: exact HELPER_PROTOCOL.md HELLO/ACK contract ────────────────

async fn run_noq_server(
    cert_der: rustls::pki_types::CertificateDer<'static>,
    key_der: rustls::pki_types::PrivateKeyDer<'static>,
    session_secret: [u8; 32],
) -> Result<u16> {
    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;
    server_crypto.alpn_protocols = vec![ALPN.to_vec()];
    server_crypto.max_early_data_size = 0; // 0-RTT disabled, per HELPER_PROTOCOL.md

    let quic_crypto = noq::crypto::rustls::QuicServerConfig::try_from(server_crypto)
        .map_err(|e| anyhow!("QuicServerConfig::try_from failed: {e}"))?;

    let mut transport = noq::TransportConfig::default();
    transport.max_concurrent_bidi_streams(noq::VarInt::from_u32(2));
    transport.max_concurrent_uni_streams(noq::VarInt::from_u32(0));
    transport.max_concurrent_multipath_paths(8);

    let mut server_config = noq::ServerConfig::with_crypto(Arc::new(quic_crypto));
    server_config.transport_config(Arc::new(transport));

    // [::] with bindv6only=0 accepts both IPv4 and IPv6 loopback traffic on the
    // same socket -- same dual-stack trick as noq-spike-server / dual_fd_loopback_test.
    let bind_addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0);
    let endpoint = noq::Endpoint::server(server_config, bind_addr)?;
    let port = endpoint.local_addr()?.port();

    tokio::spawn(async move {
        loop {
            let Some(incoming) = endpoint.accept().await else { break };
            let session_secret = session_secret;
            tokio::spawn(async move {
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(e) => {
                        println!("[server] handshake failed: {e}");
                        return;
                    }
                };
                println!("[server] connection established");
                if let Err(e) = serve_connection(conn, session_secret).await {
                    println!("[server] connection ended: {e:#}");
                }
            });
        }
    });

    Ok(port)
}

async fn serve_connection(conn: noq::Connection, session_secret: [u8; 32]) -> Result<()> {
    loop {
        let (mut send, mut recv) = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => return Ok(()), // connection closed
        };
        let session_secret = session_secret;
        let conn = conn.clone();
        tokio::spawn(async move {
            let mut hello = [0u8; 33];
            if recv.read_exact(&mut hello).await.is_err() {
                return;
            }
            if hello[0] != FRAME_HELLO {
                let _ = send.write_all(&[0xFD]).await;
                return;
            }
            let mut exporter = [0u8; 32];
            if conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"").is_err() {
                let _ = send.write_all(&[FRAME_REJECT_AUTH]).await;
                return;
            }
            let mut mac = HmacSha256::new_from_slice(&session_secret).unwrap();
            mac.update(&exporter);
            let expected = mac.finalize().into_bytes();

            if hello[1..33] != expected[..] {
                println!("[server] REJECT_AUTH (proof mismatch)");
                let _ = send.write_all(&[FRAME_REJECT_AUTH]).await;
                return;
            }
            println!("[server] HELLO verified, sending ACK");
            if send.write_all(&[FRAME_ACK]).await.is_err() {
                return;
            }
            // Echo anything further sent on this stream (simplified relay
            // stand-in -- the real isekai-helper relays to a TCP target
            // instead, irrelevant to what this spike is checking).
            if let Ok(data) = recv.read_to_end(4096).await {
                let mut reply = b"echo:".to_vec();
                reply.extend_from_slice(&data);
                let _ = send.write_all(&reply).await;
                let _ = send.finish();
            }
        });
    }
}

// ── shared: compute proof the same way the real client does ────────────────

fn compute_proof_noq(conn: &noq::Connection, session_secret: &[u8; 32]) -> Result<[u8; 32]> {
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"")
        .map_err(|e| anyhow!("export_keying_material failed: {e:?}"))?;
    let mut mac = HmacSha256::new_from_slice(session_secret).unwrap();
    mac.update(&exporter);
    Ok(mac.finalize().into_bytes().into())
}

fn compute_proof_quinn(conn: &quinn::Connection, session_secret: &[u8; 32]) -> Result<[u8; 32]> {
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"")
        .map_err(|e| anyhow!("export_keying_material failed: {e:?}"))?;
    let mut mac = HmacSha256::new_from_slice(session_secret).unwrap();
    mac.update(&exporter);
    Ok(mac.finalize().into_bytes().into())
}

// ── cert pinning verifier, replicated verbatim from isekai_pipe_quic_transport.rs ─

#[derive(Debug)]
struct PinnedCertVerifier {
    expected_sha256_hex: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
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
            Err(rustls::Error::General(format!("cert pin mismatch: expected {} got {}", self.expected_sha256_hex, got)))
        }
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

fn build_pinned_rustls_client_config(cert_sha256_hex: &str) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| anyhow!("TLS config failed"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
            expected_sha256_hex: cert_sha256_hex.to_string(),
            provider,
        }))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    Ok(crypto)
}

// ── check 1: noq client, HELLO/ACK + multipath (path0 + path1 + failover) ──

async fn check_noq_multipath_client(
    server_port: u16,
    cert_sha256_hex: &str,
    session_secret: [u8; 32],
) -> Result<()> {
    let crypto = build_pinned_rustls_client_config(cert_sha256_hex)?;
    let quic_crypto = noq::crypto::rustls::QuicClientConfig::try_from(crypto)
        .map_err(|e| anyhow!("QuicClientConfig::try_from failed: {e}"))?;
    let mut client_config = noq::ClientConfig::new(Arc::new(quic_crypto));
    let mut transport = noq::TransportConfig::default();
    transport.max_concurrent_bidi_streams(noq::VarInt::from_u32(2));
    transport.max_concurrent_uni_streams(noq::VarInt::from_u32(0));
    transport.max_concurrent_multipath_paths(8);
    client_config.transport_config(Arc::new(transport));

    let endpoint = noq::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    let path0_addr: SocketAddr = format!("127.0.0.1:{server_port}").parse()?;
    let conn = endpoint.connect(path0_addr, "isekai-pipe.local")?.await.context("path0 handshake")?;
    println!("[noq-client] path0 established");

    hello_ack_roundtrip_noq(&conn, &session_secret, "via path0").await?;
    println!("[noq-client] PASS: HELLO/ACK over noq path0");

    // Same server (bound dual-stack on [::]), reached via a second IPv4
    // loopback alias -- stands in for "path1 = a different remote address"
    // (e.g. direct address vs Tailscale address), which is what Phase 9-2
    // actually needs. Avoids client-side v4/v6 socket-family mixing entirely.
    let path1_addr: SocketAddr = format!("127.0.0.2:{server_port}").parse()?;
    let path1 = tokio::time::timeout(
        Duration::from_secs(8),
        conn.open_path(noq::FourTuple::from_remote(path1_addr), noq::PathStatus::Available),
    )
    .await
    .context("open_path timed out")??;
    println!("[noq-client] path1 established: id={:?}", path1.id());

    hello_ack_roundtrip_noq(&conn, &session_secret, "via path1").await?;
    println!("[noq-client] PASS: HELLO/ACK over noq path1 (both paths alive simultaneously)");

    if let Some(path0) = conn.path(noq::PathId::ZERO) {
        let _ = path0.close();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    hello_ack_roundtrip_noq(&conn, &session_secret, "via path1 only").await?;
    println!("[noq-client] PASS: failover -- path1 alone still serves HELLO/ACK after path0 closed");

    conn.close(0u32.into(), b"done");
    Ok(())
}

async fn hello_ack_roundtrip_noq(conn: &noq::Connection, session_secret: &[u8; 32], payload: &str) -> Result<()> {
    let proof = compute_proof_noq(conn, session_secret)?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let mut hello = Vec::with_capacity(33 + payload.len());
    hello.push(FRAME_HELLO);
    hello.extend_from_slice(&proof);
    send.write_all(&hello).await?;

    let mut resp = [0u8; 1];
    recv.read_exact(&mut resp).await?;
    if resp[0] != FRAME_ACK {
        bail!("expected ACK, got {:#x}", resp[0]);
    }
    send.write_all(payload.as_bytes()).await?;
    send.finish()?;
    let reply = recv.read_to_end(4096).await?;
    println!("  echo: {:?}", String::from_utf8_lossy(&reply));
    Ok(())
}

// ── check 2: plain quinn client against the noq server (backward compat) ───

async fn check_quinn_backward_compat(server_port: u16, cert_sha256_hex: &str, session_secret: [u8; 32]) -> Result<()> {
    let crypto = build_pinned_rustls_client_config(cert_sha256_hex)?;
    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .map_err(|_| anyhow!("quinn QuicClientConfig conversion failed"))?;

    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(1));
    transport.max_concurrent_uni_streams(quinn::VarInt::from_u32(0));

    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_crypto));
    client_config.transport_config(Arc::new(transport));

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    let addr: SocketAddr = format!("127.0.0.1:{server_port}").parse()?;
    let conn = endpoint.connect(addr, "isekai-pipe.local")?.await.context("quinn handshake against noq server")?;
    println!("[quinn-client] QUIC handshake OK against noq server, rtt={:?}", conn.rtt());

    let proof = compute_proof_quinn(&conn, &session_secret)?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let mut hello = Vec::with_capacity(33);
    hello.push(FRAME_HELLO);
    hello.extend_from_slice(&proof);
    send.write_all(&hello).await?;

    let mut resp = [0u8; 1];
    recv.read_exact(&mut resp).await?;
    if resp[0] != FRAME_ACK {
        bail!("expected ACK, got {:#x}", resp[0]);
    }
    send.write_all(b"via plain quinn client").await?;
    send.finish()?;
    let reply = recv.read_to_end(4096).await?;
    println!("[quinn-client] echo: {:?}", String::from_utf8_lossy(&reply));
    println!("[quinn-client] PASS: unmodified quinn client completes HELLO/ACK against a noq server");

    conn.close(0u32.into(), b"done");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cert = rcgen::generate_simple_self_signed(vec!["isekai-pipe.local".to_string()])?;
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().clone());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der())
        .map_err(|e| anyhow!("key conversion failed: {e}"))?;
    let cert_sha256_hex = {
        let mut hasher = Sha256::new();
        hasher.update(cert_der.as_ref());
        hex_lower(&hasher.finalize())
    };

    let mut session_secret = [0u8; 32];
    {
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut session_secret);
    }

    let server_port = run_noq_server(cert_der.clone(), key_der, session_secret).await?;
    println!("=== Phase 9-0 compat check: noq server on udp/{server_port}, cert_sha256={cert_sha256_hex} ===\n");

    println!("--- check 1: noq client (HELLO/ACK contract + multipath + failover) ---");
    check_noq_multipath_client(server_port, &cert_sha256_hex, session_secret).await?;

    println!("\n--- check 2: plain quinn client against the same noq server (backward compat) ---");
    check_quinn_backward_compat(server_port, &cert_sha256_hex, session_secret).await?;

    println!("\n=== ALL CHECKS PASSED ===");
    Ok(())
}
