//! Android's own `quicmux::AnyMuxFactory` constructor: the `noq` backend,
//! with every socket it binds/wraps adapted through `FaultyUdpSocket`
//! (Android's fault-injection wrapper) instead of a plain
//! `tokio::net::UdpSocket` (`isekai_transport::system::system_quic_factory`'s
//! CLI-facing equivalent).
//!
//! Before `quicmux` existed, this file implemented `isekai_transport::traits::
//! {QuicEndpointFactory, QuicEndpoint, QuicConnection, ByteStream}` directly
//! against `FaultyUdpSocket` — four traits' worth of boilerplate duplicating
//! `isekai-transport::system::SystemQuicEndpointFactory`'s structure almost
//! line-for-line, with only the socket-construction step actually differing.
//! Once connection establishment moved to `quicmux`'s enum-based
//! `AnyMuxFactory`/`AnyMuxConnection`, the only Android-specific thing left
//! to express is *which concrete `noq::AsyncUdpSocket` a fresh/wrapped socket
//! becomes* — exactly the seam `quicmux::noq_backend::NoqFactory::
//! with_socket_adapter` exists for, so this file shrinks to one constructor
//! function instead of four trait impls.
//!
//! Fault injection is transparent unless explicitly armed via
//! `debug_fault::shared_injector()` (debug builds only, `FaultyUdpSocket`'s
//! own docs) — normal-path behavior is byte-for-byte identical to
//! `isekai_transport::system::system_quic_factory`'s equivalent.

use std::sync::Arc;
use std::time::Duration;

use isekai_protocol::hello::{ALPN, EXPORTER_LABEL};
use quicmux::{AnyMuxFactory, MuxClientConfig};

use crate::debug_fault;
use crate::faulty_udp_socket::FaultyUdpSocket;

/// Builds the `AnyMuxFactory` every Android transport file
/// (`isekai_pipe_quic_transport.rs`/`isekai_link_relay_transport.rs`/
/// `isekai_stun_p2p_transport.rs`) uses to reach `isekai-pipe serve` via
/// `isekai-transport`'s relay/resume/STUN-P2P connection-establishment
/// functions.
pub(crate) fn factory() -> AnyMuxFactory {
    let config = MuxClientConfig {
        alpn: ALPN.to_vec(),
        exporter_label: EXPORTER_LABEL.to_vec(),
        max_idle_timeout: Duration::from_secs(15),
        keep_alive_interval: Duration::from_secs(5),
        max_concurrent_bidi_streams: 1,
        max_concurrent_uni_streams: 0,
        // `AnyMuxEndpoint::rebinder()` is meaningless without multipath
        // negotiated (see `quicmux::noq_client_config`'s docs on why), but
        // Android never calls `rebinder()` on an endpoint built through this
        // factory in the first place — `multipath_transport.rs` handles
        // physical-interface failover through its own mechanism, not
        // `quicmux::AnyMuxRebinder` — so there is nothing on the Android side
        // that could ever make multipath negotiation worth the (harmless,
        // but non-zero) extra transport parameter.
        multipath: false,
        // The Android app never sends QUIC datagrams today — see
        // `quicmux`'s `MuxClientConfig::datagram_send_buffer_size` docs.
        datagram_send_buffer_size: None,
    };

    AnyMuxFactory::noq_with_socket_adapter(
        config,
        Arc::new(|std_socket: std::net::UdpSocket| {
            let inner = Arc::new(tokio::net::UdpSocket::from_std(std_socket)?);
            let socket = FaultyUdpSocket::new(inner, debug_fault::shared_injector());
            Ok(Box::new(socket) as Box<dyn noq::AsyncUdpSocket>)
        }),
    )
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc as StdArc;

    use quicmux::{BindSpec, RemoteSpec};
    use sha2::{Digest, Sha256};

    use super::*;

    /// 最小構成のechoサーバー(ATTACH v2等のアプリプロトコルは一切実装しない)。
    /// この`factory()`が組み立てる`AnyMuxFactory::Noq(..)`のconnect/open_bi/
    /// read/write/export_keying_material/closeそのものだけを検証する対象。
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
        let server_config = noq::ServerConfig::with_crypto(StdArc::new(quic_crypto));

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
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (server_addr, cert_sha256_hex) = start_echo_server().await;

        let factory = factory();
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
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (server_addr, _correct_cert_sha256_hex) = start_echo_server().await;

        let factory = factory();
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
