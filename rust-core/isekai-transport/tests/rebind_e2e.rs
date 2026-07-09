//! End-to-end test for `noq::Endpoint::rebind` (what
//! `QuicEndpointRebinder::rebind_socket` calls, `system.rs`) fed a socket
//! bound via `isekai_transport::bind_physical_interface` (the vendored
//! `quicsock` crate) — proves the CLI/PC physical-interface-rebind path
//! (`isekai-transport::physical_interface`, the reactive-failover half of
//! `multipath_transport.rs`'s Phase 9-4b `rebind_abstract()`) actually
//! switches a live QUIC connection's socket and keeps it usable afterward,
//! not just that `quicsock::bind_udp` succeeds in isolation
//! (`physical_interface.rs`'s own unit tests only cover that part).
//!
//! This deliberately does **not** go through `SystemQuicEndpointFactory` (the
//! production single-path connection path) — building the connection this
//! test found a real, separate, pre-existing issue: `noq::Endpoint::rebind`
//! relies on QUIC connection-migration path validation, which in `noq`
//! 1.0.1 only succeeds if *both* sides negotiated the multipath extension
//! (`TransportConfig::max_concurrent_multipath_paths`) at connect time —
//! `SystemQuicEndpointFactory`'s connections don't (`client_config_for`'s
//! `multipath: bool` param, `false` there on purpose, matching what
//! `isekai-pipe serve` negotiates today). That means `isekai-pipe`'s
//! `--experimental-network-rebind` likely does not actually work as shipped
//! — tracked separately, not fixed here (production fix needs
//! `isekai-pipe serve`'s server-side transport config changed too, out of
//! scope for the isekai-transport crate-sharing work this test belongs to).
//! This test builds its own multipath-negotiated connection directly so it
//! can still prove the piece actually in scope: quicsock-bound sockets are
//! valid, working `noq` rebind targets.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use isekai_protocol::hello::ALPN;
use isekai_transport::{bind_physical_interface, system::client_config_for, InterfaceIndex};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

const SNI: &str = "isekai-pipe.local";

fn generate_cert() -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>, String) {
    let cert = rcgen::generate_simple_self_signed(vec![SNI.to_string()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut hasher = Sha256::new();
    hasher.update(cert_der.as_ref());
    let sha256_hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    (cert_der, key_der, sha256_hex)
}

fn loopback_index() -> InterfaceIndex {
    isekai_transport::physical_interface::quicsock::discovery::list_interfaces()
        .into_iter()
        .find(|(_, iface)| iface.is_loopback())
        .map(|(index, _)| index)
        .expect("this machine should have a loopback interface")
}

/// Bound to the wildcard address so it keeps receiving from the client's
/// socket after it rebinds to a different loopback address.
async fn run_echo_server(cert_der: CertificateDer<'static>, key_der: PrivatePkcs8KeyDer<'static>) -> SocketAddr {
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())
        .unwrap();
    tls_config.alpn_protocols = vec![ALPN.to_vec()];
    let quic_crypto = noq::crypto::rustls::QuicServerConfig::try_from(tls_config).unwrap();
    let mut config = noq::ServerConfig::with_crypto(Arc::new(quic_crypto));
    let mut transport = noq::TransportConfig::default();
    transport.max_concurrent_bidi_streams(noq::VarInt::from_u32(2));
    // Required for the client's post-rebind path-migration validation to
    // succeed — see this file's module docs.
    transport.max_concurrent_multipath_paths(8);
    config.transport_config(Arc::new(transport));
    let endpoint = noq::Endpoint::server(config, SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)).unwrap();
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), endpoint.local_addr().unwrap().port());

    tokio::spawn(async move {
        let incoming = endpoint.accept().await.unwrap();
        let conn = incoming.await.unwrap();
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let mut buf = [0u8; 64];
            if let Ok(Some(n)) = recv.read(&mut buf).await {
                let _ = send.write_all(&buf[..n]).await;
            }
            let _ = send.finish();
        }
    });

    addr
}

#[tokio::test]
async fn rebind_onto_a_quicsock_bound_interface_keeps_the_connection_usable() {
    let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Debug).try_init();
    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let server_addr = run_echo_server(cert_der, key_der).await;

    // Built directly (not via `SystemQuicEndpointFactory`) with multipath
    // negotiated — see this file's module docs for why.
    let client_config = client_config_for(&cert_sha256_hex, true).unwrap();
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    let endpoint =
        noq::Endpoint::new(noq::EndpointConfig::default(), None, socket, Arc::new(noq::TokioRuntime)).unwrap();
    let conn = endpoint.connect_with(client_config, server_addr, SNI).unwrap().await.expect("initial connect should succeed");

    // Prove the connection works before rebinding.
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(b"before rebind").await.unwrap();
    send.finish().unwrap();
    let mut buf = [0u8; 64];
    let n = recv.read(&mut buf).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"before rebind");

    let physical_socket = bind_physical_interface(loopback_index(), SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 4)), 0))
        .expect("quicsock should bind a loopback-restricted socket");
    let physical_addr = physical_socket.local_addr().unwrap();
    // What `system::SystemQuicEndpointRebinder::rebind_socket` calls under
    // the hood — see that type's docs.
    endpoint.rebind(physical_socket).expect("rebind onto the quicsock-bound socket should succeed");

    // Prove the connection is still usable — over a fresh stream, on the
    // endpoint's new (quicsock-bound) socket — after the rebind.
    let (mut send, mut recv) = tokio::time::timeout(Duration::from_secs(5), conn.open_bi())
        .await
        .expect("open_bi after rebind should not hang")
        .expect("open_bi after rebind should succeed");
    send.write_all(b"after rebind").await.unwrap();
    send.finish().unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), recv.read(&mut buf))
        .await
        .expect("read after rebind should not hang")
        .unwrap()
        .unwrap();
    assert_eq!(&buf[..n], b"after rebind", "connection must keep working after rebinding onto {physical_addr}");

    conn.close(noq::VarInt::from_u32(0), b"");
}
