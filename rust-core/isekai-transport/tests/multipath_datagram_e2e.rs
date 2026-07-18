//! Answers the open question left in task #35's design doc (§9-D): does
//! `AnyMuxConnection`'s QUIC datagram API (`#38`) actually work over a
//! connection established through `isekai-transport::multipath`'s
//! primary+secondary path pattern? `isekai-transport::multipath` is
//! deliberately "noq-concrete" (drives `noq::Connection`/`open_path`
//! directly, bypassing `quicmux::AnyMuxFactory` — see that module's own
//! docs), so this test does not go through the production
//! `connect_multipath`/`connect_multipath_with_socket` entry points (those
//! hardcode `system::isekai_mux_config(true)`, which explicitly disables
//! datagrams — `isekai-ssh`/`isekai-pipe`'s SSH tunnel doesn't use them
//! today). Instead it reproduces just enough of that module's connect +
//! `open_path` sequence with its own datagram-enabled `MuxClientConfig`,
//! per this repo's `isekai-ssh-e2e-test-self-containment-convention` (each
//! `tests/*_e2e.rs` file duplicates its own setup rather than sharing it).
//!
//! Wraps the resulting `noq::Connection` via the new
//! `quicmux::AnyMuxConnection::from_noq_connection` constructor (added
//! alongside this test) so `isekai-pipe`'s `datagram_relay` module (or any
//! other `AnyMuxConnection`-based caller) could drive its datagram plane the
//! same way over a multipath connection as over an ordinary single-path one.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use isekai_protocol::hello::ALPN;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

const SNI: &str = "isekai-pipe.local";

fn generate_cert() -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>, String) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert = rcgen::generate_simple_self_signed(vec![SNI.to_string()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut hasher = Sha256::new();
    hasher.update(cert_der.as_ref());
    let sha256_hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    (cert_der, key_der, sha256_hex)
}

/// Bound to the IPv4 wildcard address so it also receives the secondary
/// path's traffic on `127.0.0.2` — see `multipath_e2e.rs::mock_server`'s
/// identical comment (same macOS `lo0`-aliasing caveat applies here).
/// Datagrams enabled explicitly (rather than relying on noq's own default)
/// to keep this test's intent legible.
fn mock_server(cert_der: CertificateDer<'static>, key_der: PrivatePkcs8KeyDer<'static>) -> noq::Endpoint {
    let mut tls_config = rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(vec![cert_der], key_der.into()).unwrap();
    tls_config.alpn_protocols = vec![ALPN.to_vec()];
    let quic_crypto = noq::crypto::rustls::QuicServerConfig::try_from(tls_config).unwrap();
    let mut config = noq::ServerConfig::with_crypto(Arc::new(quic_crypto));
    let mut transport = noq::TransportConfig::default();
    transport.max_concurrent_multipath_paths(8);
    transport.datagram_receive_buffer_size(Some(64 * 1024));
    transport.datagram_send_buffer_size(64 * 1024);
    config.transport_config(Arc::new(transport));
    noq::Endpoint::server(config, SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)).unwrap()
}

fn datagram_enabled_client_config() -> quicmux::MuxClientConfig {
    quicmux::MuxClientConfig {
        alpn: ALPN.to_vec(),
        exporter_label: b"multipath-datagram-test-exporter".to_vec(),
        max_idle_timeout: Duration::from_secs(15),
        keep_alive_interval: Duration::from_secs(5),
        max_concurrent_bidi_streams: 2,
        max_concurrent_uni_streams: 0,
        multipath: true,
        datagram_send_buffer_size: Some(64 * 1024),
    }
}

#[tokio::test]
async fn datagram_send_and_recv_work_over_a_connection_with_a_secondary_path_open() {
    let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Debug).try_init();
    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let endpoint = mock_server(cert_der, key_der);
    let port = endpoint.local_addr().unwrap().port();
    let primary_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let secondary_addr: SocketAddr = format!("127.0.0.2:{port}").parse().unwrap();

    let server_task = tokio::spawn(async move {
        let incoming = endpoint.accept().await.unwrap();
        let conn = incoming.await.unwrap();
        // Echo exactly one datagram back to prove the client's send actually
        // reached the server (over whichever path noq scheduled it on),
        // then keep the connection (and both its paths) alive long enough
        // for the client's own assertions to finish.
        if let Ok(datagram) = tokio::time::timeout(Duration::from_secs(5), conn.read_datagram()).await {
            let datagram = datagram.expect("server should receive the client's datagram");
            let _ = conn.send_datagram(datagram);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        conn.close(noq::VarInt::from_u32(0), b"");
    });

    let client_config = datagram_enabled_client_config();
    let (client_config_built, _mismatch) = quicmux::noq_client_config(&cert_sha256_hex, &client_config).expect("client config build failed");

    let socket = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap();
    let std_socket = socket.into_std().unwrap();
    use noq::Runtime as _;
    let async_socket = noq::TokioRuntime.wrap_udp_socket(std_socket).unwrap();
    let client_endpoint = noq::Endpoint::new_with_abstract_socket(noq::EndpointConfig::default(), None, async_socket, Arc::new(noq::TokioRuntime)).unwrap();

    let connecting = client_endpoint.connect_with(client_config_built, primary_addr, SNI).expect("connect setup failed");
    let conn = tokio::time::timeout(Duration::from_secs(10), connecting).await.expect("connect should not hang").expect("handshake failed");

    // Open the secondary path exactly the way `isekai-transport::multipath`
    // does internally (`local_ip: None`, `PathStatus::Available`) — see this
    // file's module docs for why this test reproduces that instead of
    // calling `connect_multipath` directly. Retries with backoff, mirroring
    // `multipath::open_path_with_retry`: right after the handshake the peer
    // may not have issued enough spare connection IDs yet for a new path,
    // which surfaces as `PathError::RemoteCidsExhausted` (a transient
    // condition — hit on the very first attempt here too). Unlike
    // `open_path_with_retry` (which retries any error, matching production's
    // "keep trying, log and give up" policy), this test only retries that
    // specific transient variant and panics immediately on anything else, so
    // a real regression fails loudly instead of silently retrying into a
    // timeout.
    let four_tuple = noq::FourTuple::from_remote(secondary_addr);
    let mut backoff = Duration::from_millis(200);
    let secondary_path = loop {
        match tokio::time::timeout(Duration::from_secs(8), conn.open_path(four_tuple, noq::PathStatus::Available)).await {
            Ok(Ok(path)) => break path,
            Ok(Err(noq::PathError::RemoteCidsExhausted)) if backoff < Duration::from_secs(2) => {
                log::warn!("open_path: RemoteCidsExhausted, retrying after {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
            Ok(Err(e)) => panic!("secondary path should open: {e}"),
            Err(_) => panic!("open_path should not hang"),
        }
    };
    log::info!("secondary path established: id={:?}", secondary_path.id());

    let any_conn = quicmux::AnyMuxConnection::from_noq_connection(conn);

    assert!(any_conn.max_datagram_size().is_some(), "datagrams should be enabled on both sides of a multipath connection");

    any_conn.send_datagram(bytes::Bytes::from_static(b"hello multipath datagram")).expect("send_datagram failed");
    let echoed = tokio::time::timeout(Duration::from_secs(5), any_conn.recv_datagram())
        .await
        .expect("timed out waiting for the echoed datagram")
        .expect("recv_datagram failed");
    assert_eq!(&echoed[..], b"hello multipath datagram");

    server_task.await.unwrap();
}
