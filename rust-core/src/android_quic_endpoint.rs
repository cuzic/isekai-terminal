//! `isekai_transport::traits::{QuicEndpointFactory, QuicEndpoint, QuicConnection,
//! ByteStream}` implementations backed by `FaultyUdpSocket` (Android's fault-injection
//! wrapper) instead of a plain `tokio::net::UdpSocket`
//! (`isekai_transport::system::SystemQuicEndpointFactory`'s CLI equivalent).
//!
//! This is the adapter layer that will let Android's transport files
//! (`isekai_pipe_quic_transport.rs`/`isekai_link_relay_transport.rs`/
//! `isekai_stun_p2p_transport.rs`) eventually call into `isekai-transport`'s
//! connection-establishment/resume logic instead of duplicating it
//! (isekai-terminal-core/isekai-transport crate共有化 Phase 1c/1d). This file
//! alone does not change any existing call site — it is a standalone,
//! independently-testable trait implementation (Phase 1b).
//!
//! Fault injection is transparent unless explicitly armed via
//! `debug_fault::shared_injector()` (debug builds only, `FaultyUdpSocket`'s
//! own docs) — normal-path behavior is byte-for-byte identical to
//! `isekai_transport::system::SystemQuicEndpointFactory`.

use std::sync::Arc;

use async_trait::async_trait;
use isekai_transport::error::TransportError;
use isekai_transport::system::client_config_for;
use isekai_transport::traits::{
    ByteStream, ByteStreamReadHalf, ByteStreamWriteHalf, QuicConnection, QuicEndpoint, QuicEndpointFactory,
};
use isekai_transport::types::{BindSpec, RemoteSpec};

use crate::debug_fault;
use crate::faulty_udp_socket;

/// Stateless — every `create_endpoint` call binds a fresh `FaultyUdpSocket`.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct AndroidQuicEndpointFactory;

#[async_trait]
impl QuicEndpointFactory for AndroidQuicEndpointFactory {
    async fn create_endpoint(&self, bind: BindSpec) -> Result<Box<dyn QuicEndpoint>, TransportError> {
        let socket = faulty_udp_socket::bind_faulty_udp_socket(bind.local_addr, debug_fault::shared_injector())
            .map_err(|source| TransportError::Bind { addr: bind.local_addr, source })?;

        let endpoint = noq::Endpoint::new_with_abstract_socket(
            noq::EndpointConfig::default(),
            None,
            Box::new(socket),
            Arc::new(noq::TokioRuntime),
        )
        .map_err(|e| TransportError::EndpointSetup(e.to_string()))?;

        Ok(Box::new(AndroidQuicEndpoint { endpoint }))
    }
}

struct AndroidQuicEndpoint {
    endpoint: noq::Endpoint,
}

#[async_trait]
impl QuicEndpoint for AndroidQuicEndpoint {
    async fn connect(&self, remote: RemoteSpec) -> Result<Box<dyn QuicConnection>, TransportError> {
        let client_config = client_config_for(&remote.cert_sha256_hex)?;
        log::info!("android_quic_endpoint: connecting to {}", remote.addr);
        let conn = self
            .endpoint
            .connect_with(client_config, remote.addr, &remote.server_name)
            .map_err(|e| TransportError::ConnectSetup(e.to_string()))?
            .await
            .map_err(|e| TransportError::Handshake(e.to_string()))?;
        log::info!("android_quic_endpoint: QUIC handshake ok rtt={:?}", conn.rtt(noq::PathId::ZERO));
        Ok(Box::new(AndroidQuicConnection { conn }))
    }

    // rebinder(): 既定のNoneのまま。Androidのネットワーク変化対応は
    // `multipath_transport.rs`独自のPathBroker機構が別途担っており、この
    // 汎用QuicEndpointRebinder経由での配線はPhase 1のスコープ外。
}

struct AndroidQuicConnection {
    conn: noq::Connection,
}

#[async_trait]
impl QuicConnection for AndroidQuicConnection {
    async fn open_bi(&self) -> Result<Box<dyn ByteStream>, TransportError> {
        let (send, recv) = self.conn.open_bi().await.map_err(|e| TransportError::OpenStream(e.to_string()))?;
        Ok(Box::new(AndroidByteStream { send, recv }))
    }

    async fn close(&self) {
        self.conn.close(noq::VarInt::from_u32(0), b"");
    }

    async fn export_keying_material(&self, label: &[u8], context: &[u8]) -> Result<[u8; 32], TransportError> {
        let mut out = [0u8; 32];
        self.conn
            .export_keying_material(&mut out, label, context)
            .map_err(|e| TransportError::ExportKeyingMaterial(format!("{e:?}")))?;
        Ok(out)
    }
}

struct AndroidByteStream {
    send: noq::SendStream,
    recv: noq::RecvStream,
}

#[async_trait]
impl ByteStream for AndroidByteStream {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self.recv.read(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))? {
            Some(n) => Ok(n),
            None => Ok(0), // stream finished cleanly (EOF)
        }
    }

    async fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError> {
        self.send.write_all(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))
    }

    async fn shutdown(&mut self) -> Result<(), TransportError> {
        self.send.finish().map_err(|e| TransportError::StreamIo(e.to_string()))
    }

    fn split(self: Box<Self>) -> (Box<dyn ByteStreamReadHalf>, Box<dyn ByteStreamWriteHalf>) {
        let AndroidByteStream { send, recv } = *self;
        (Box::new(AndroidByteStreamReadHalf { recv }), Box::new(AndroidByteStreamWriteHalf { send }))
    }
}

/// `pub(crate)` (not module-private): `resume_client.rs`'s `ReattachableStream`
/// works over `isekai_transport::traits::ByteStreamReadHalf`/`WriteHalf`
/// rather than raw `noq` types (isekai-terminal-core/isekai-transport crate
/// 共有化 Phase 1d) — this lets the 3 existing transport files construct one
/// of these directly from a `noq::SendStream`/`RecvStream` pair obtained via
/// their own (not-yet-migrated) connection-establishment code, without
/// forcing that migration to land in the same change as the
/// `ReattachableStream` rewrite itself.
pub(crate) struct AndroidByteStreamReadHalf {
    recv: noq::RecvStream,
}

impl AndroidByteStreamReadHalf {
    pub(crate) fn new(recv: noq::RecvStream) -> Self {
        Self { recv }
    }
}

#[async_trait]
impl ByteStreamReadHalf for AndroidByteStreamReadHalf {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self.recv.read(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))? {
            Some(n) => Ok(n),
            None => Ok(0),
        }
    }
}

pub(crate) struct AndroidByteStreamWriteHalf {
    send: noq::SendStream,
}

impl AndroidByteStreamWriteHalf {
    pub(crate) fn new(send: noq::SendStream) -> Self {
        Self { send }
    }
}

#[async_trait]
impl ByteStreamWriteHalf for AndroidByteStreamWriteHalf {
    async fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError> {
        self.send.write_all(buf).await.map_err(|e| TransportError::StreamIo(e.to_string()))
    }

    async fn shutdown(&mut self) -> Result<(), TransportError> {
        self.send.finish().map_err(|e| TransportError::StreamIo(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use sha2::{Digest, Sha256};

    use super::*;

    /// 最小構成のechoサーバー(ATTACH v2等のアプリプロトコルは一切実装しない)。
    /// `AndroidQuicEndpointFactory`/`AndroidQuicEndpoint`/`AndroidQuicConnection`/
    /// `AndroidByteStream`のtrait実装そのものだけを検証する対象。
    async fn start_echo_server() -> (SocketAddr, String) {
        let cert = rcgen::generate_simple_self_signed(vec!["isekai-pipe.local".to_string()]).unwrap();
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().clone());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
        let cert_sha256_hex = {
            let mut hasher = Sha256::new();
            hasher.update(cert_der.as_ref());
            hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
        };

        let mut server_crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .unwrap();
        server_crypto.alpn_protocols = vec![isekai_protocol::hello::ALPN.to_vec()];
        server_crypto.max_early_data_size = 0;
        let quic_crypto = noq::crypto::rustls::QuicServerConfig::try_from(server_crypto).unwrap();
        let server_config = noq::ServerConfig::with_crypto(Arc::new(quic_crypto));

        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        let endpoint = noq::Endpoint::server(server_config, bind_addr).unwrap();
        // `local_addr()` reports the wildcard bind address (`0.0.0.0`); dial
        // loopback explicitly instead (mirrors `multipath_transport.rs`'s
        // test-server helper).
        let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), endpoint.local_addr().unwrap().port());

        tokio::spawn(async move {
            let Some(incoming) = endpoint.accept().await else { return };
            let Ok(conn) = incoming.await else { return };
            // `conn`を明示的にcloseするまでループし続けることで、echo書き込み直後に
            // `conn`がdropされてQUIC connectionごと閉じてしまう競合(クライアント側の
            // readがまだ完了していない状態)を避ける — クライアント側は読み取り後に
            // 明示的に`conn.close()`する(このテスト末尾)。
            while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                let mut buf = [0u8; 64];
                if let Ok(Some(n)) = recv.read(&mut buf).await {
                    let _ = send.write_all(&buf[..n]).await;
                }
                let _ = send.finish();
            }
        });

        (local_addr, cert_sha256_hex)
    }

    #[tokio::test]
    async fn connect_open_bi_write_and_read_roundtrip() {
        let (server_addr, cert_sha256_hex) = start_echo_server().await;

        let factory = AndroidQuicEndpointFactory;
        let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await.expect("create_endpoint failed");
        let conn = endpoint
            .connect(RemoteSpec {
                addr: server_addr,
                server_name: "isekai-pipe.local".to_string(),
                cert_sha256_hex,
            })
            .await
            .expect("connect failed");

        let mut stream = conn.open_bi().await.expect("open_bi failed");
        stream.write_all(b"hello android quic endpoint").await.expect("write failed");
        stream.shutdown().await.expect("shutdown failed");

        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.expect("read failed");
        assert_eq!(&buf[..n], b"hello android quic endpoint");

        let keying = conn.export_keying_material(b"test-label", b"").await.expect("export_keying_material failed");
        assert_eq!(keying.len(), 32);

        conn.close().await;
    }

    #[tokio::test]
    async fn connect_fails_on_cert_pin_mismatch() {
        let (server_addr, _correct_cert_sha256_hex) = start_echo_server().await;

        let factory = AndroidQuicEndpointFactory;
        let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await.expect("create_endpoint failed");
        let result = endpoint
            .connect(RemoteSpec {
                addr: server_addr,
                server_name: "isekai-pipe.local".to_string(),
                cert_sha256_hex: "0".repeat(64),
            })
            .await;

        assert!(result.is_err(), "connect should fail when the cert pin does not match");
    }
}
