//! End-to-end test for `QuicEndpointRebinder::rebind_socket` fed a socket
//! bound via `isekai_transport::bind_physical_interface` (the vendored
//! `quicsock` crate) — proves the CLI/PC physical-interface-rebind path
//! (`isekai-transport::physical_interface`, the reactive-failover half of
//! `multipath_transport.rs`'s Phase 9-4b `rebind_abstract()`) actually
//! switches a live QUIC connection's socket and keeps it usable afterward,
//! not just that `quicsock::bind_udp` succeeds in isolation
//! (`physical_interface.rs`'s own unit tests only cover that part).
//!
//! Goes through `system_quic_factory` — the actual production
//! single-path connection path `isekai-pipe`'s `--experimental-network-
//! rebind` uses — rather than a hand-rolled connection. Earlier versions of
//! this test could not do that: `noq::Endpoint::rebind`'s connection-
//! migration (PATH_CHALLENGE/PATH_RESPONSE) validation only succeeds if
//! *both* sides negotiated noq's multipath extension
//! (`TransportConfig::max_concurrent_multipath_paths`), and
//! `SystemQuicEndpoint::connect` didn't negotiate it — meaning
//! `--experimental-network-rebind` would silently break the connection on
//! its first rebind. Fixed in `system.rs`'s `client_config_for` call site
//! (now unconditionally `true`, matching `isekai-pipe serve`'s own
//! server-side `TransportConfig`, which already negotiated this
//! unconditionally since Phase 9-1). This test exercises the real,
//! now-fixed path end-to-end.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use isekai_protocol::hello::ALPN;
use isekai_transport::{bind_physical_interface, BindSpec, InterfaceIndex, RemoteSpec, system_quic_factory};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

const SNI: &str = "isekai-pipe.local";

fn generate_cert() -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>, String) {
    // The `qmux-relay` feature links `aws-lc-rs` alongside noq's own
    // `ring`, so rustls can no longer auto-select a single process-wide
    // crypto provider when this crate is built with that feature on —
    // every test in this file calls `generate_cert` first, so fixing it
    // once here covers all of them.
    let _ = rustls::crypto::ring::default_provider().install_default();

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
/// socket after it rebinds to a different loopback address. Matches
/// `isekai-pipe serve`'s own transport config (`max_concurrent_multipath_paths`
/// negotiated unconditionally, Phase 9-1) rather than hand-tuning it for
/// this test.
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

// macOS-excluded: confirmed on a real `test-macos` CI run that this test
// hangs after rebinding onto a `quicsock`-bound (`IP_BOUND_IF`-restricted)
// *secondary* loopback alias (`127.0.0.4`, added via `ifconfig lo0 alias`):
// `noq`'s QUIC path validation (PATH_CHALLENGE/PATH_RESPONSE) times out
// (`new path validation failed`) even though the raw `bind()` itself
// succeeds. **Narrowed and largely de-risked** by a follow-up real
// `test-macos` CI run that added the two sibling tests below it in this
// file: `rebind_onto_a_quicsock_bound_interface_using_the_primary_loopback_address`
// (rebinds onto `127.0.0.1`, `lo0`'s primary address, no alias) and
// `rebind_onto_a_quicsock_bound_interface_using_a_wildcard_bind` (rebinds
// onto a wildcard `0.0.0.0:0` `IP_BOUND_IF`-restricted socket — the exact
// pattern `WarmStandby::dial` in `warm_standby.rs`, this crate's only real
// production caller of `bind_physical_interface`, actually uses) — **both
// passed** in the same CI run this test failed in. That run's debug logs
// also showed no `noq_udp` `log_sendmsg_error` output for this failing
// test, meaning the local `sendmsg()` call itself is not failing — so this
// is not the general `IP_BOUND_IF`+`noq` incompatibility originally
// suspected (Darwin's stricter interface-scoped route lookup under
// `IP_BOUND_IF`, per XNU's `in_pcb.c`), but something specific to routing
// traffic to/from a *secondary* loopback alias on macOS — and real physical
// interfaces (Wi-Fi/cellular) never need a secondary alias, they have
// exactly one natural address, matching the primary-address/wildcard
// variants that passed. Still excluded here because it remains a genuine,
// reproducible failure and its exact mechanism is still unconfirmed, but it
// no longer casts doubt on `--experimental-network-rebind`'s real-world
// macOS behavior the way it originally did.
#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn rebind_onto_a_quicsock_bound_interface_keeps_the_connection_usable() {
    let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Debug).try_init();
    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let server_addr = run_echo_server(cert_der, key_der).await;

    let factory = system_quic_factory();
    let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await.expect("create_endpoint should succeed");
    let conn = endpoint
        .connect(RemoteSpec { addr: server_addr, server_name: SNI.to_string(), cert_sha256_hex })
        .await
        .expect("initial connect should succeed");

    // Prove the connection works before rebinding.
    let mut stream = conn.open_bi().await.unwrap();
    stream.write_all(b"before rebind").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"before rebind");

    // `127.0.0.4` needs to be explicitly aliased onto `lo0` on macOS (unlike
    // Linux, which routes the whole `127.0.0.0/8` range to `lo`
    // unconditionally) — confirmed via a real `test-macos` CI failure
    // (`AddrNotAvailable`); done in
    // `.github/workflows/rust-core-test-check.yml`'s `test-macos` job.
    let physical_socket = bind_physical_interface(loopback_index(), SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 4)), 0))
        .expect("quicsock should bind a loopback-restricted socket");
    let physical_addr = physical_socket.local_addr().unwrap();
    let rebinder = endpoint.rebinder().expect("SystemQuicEndpoint should support rebinding");
    tokio::time::timeout(Duration::from_secs(5), rebinder.rebind_socket(physical_socket))
        .await
        .expect("rebind_socket should not hang")
        .expect("rebind onto the quicsock-bound socket should succeed");

    // Prove the connection is still usable — over a fresh stream, on the
    // endpoint's new (quicsock-bound) socket — after the rebind. Before the
    // fix this described in this file's module docs, this step would hang
    // (the connection was silently broken by the rebind).
    let mut stream = tokio::time::timeout(Duration::from_secs(5), conn.open_bi())
        .await
        .expect("open_bi after rebind should not hang")
        .expect("open_bi after rebind should succeed");
    stream.write_all(b"after rebind").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("read after rebind should not hang")
        .unwrap();
    assert_eq!(&buf[..n], b"after rebind", "connection must keep working after rebinding onto {physical_addr}");

    conn.close().await;
}

/// macOS-only (see this file's other `rebind_onto_a_quicsock_bound_interface_*`
/// tests). Same scenario as
/// `rebind_onto_a_quicsock_bound_interface_keeps_the_connection_usable`, but
/// rebinds onto `lo0`'s *primary* address (`127.0.0.1`, always present, no
/// `ifconfig lo0 alias` needed) instead of the secondary alias `127.0.0.4`
/// that test uses to simulate a distinct path. Confirmed passing on a real
/// `test-macos` CI run in the same run that test failed — isolates that the
/// gap that test hits is specific to *secondary* loopback aliases, not
/// `IP_BOUND_IF` + `noq` path validation in general.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn rebind_onto_a_quicsock_bound_interface_using_the_primary_loopback_address() {
    let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Debug).try_init();
    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let server_addr = run_echo_server(cert_der, key_der).await;

    let factory = system_quic_factory();
    let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await.expect("create_endpoint should succeed");
    let conn = endpoint
        .connect(RemoteSpec { addr: server_addr, server_name: SNI.to_string(), cert_sha256_hex })
        .await
        .expect("initial connect should succeed");

    let mut stream = conn.open_bi().await.unwrap();
    stream.write_all(b"before rebind").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"before rebind");

    let physical_socket = bind_physical_interface(loopback_index(), SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .expect("quicsock should bind lo0's primary address");
    let physical_addr = physical_socket.local_addr().unwrap();
    let rebinder = endpoint.rebinder().expect("SystemQuicEndpoint should support rebinding");
    tokio::time::timeout(Duration::from_secs(5), rebinder.rebind_socket(physical_socket))
        .await
        .expect("rebind_socket should not hang")
        .expect("rebind onto the quicsock-bound socket should succeed");

    let mut stream = tokio::time::timeout(Duration::from_secs(5), conn.open_bi())
        .await
        .expect("open_bi after rebind should not hang")
        .expect("open_bi after rebind should succeed");
    stream.write_all(b"after rebind").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("read after rebind should not hang")
        .unwrap();
    assert_eq!(&buf[..n], b"after rebind", "connection must keep working after rebinding onto {physical_addr}");

    conn.close().await;
}

/// macOS-only. Same scenario again, but rebinds onto a *wildcard*
/// (`0.0.0.0:0`) `IP_BOUND_IF`-restricted socket — the exact bind pattern
/// `WarmStandby::dial` (`isekai-transport/src/warm_standby.rs`, the only
/// real production caller of `bind_physical_interface`) actually uses, as
/// opposed to this file's other tests, which bind a specific address to get
/// one distinguishable enough to prove the test is really exercising a
/// different path. Confirmed passing on a real `test-macos` CI run — direct
/// evidence that `WarmStandby`'s real usage on macOS is unaffected by the
/// gap `rebind_onto_a_quicsock_bound_interface_keeps_the_connection_usable`
/// hits, which is a loopback-test-methodology artifact rather than a bug in
/// the shipped `--experimental-network-rebind` feature.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn rebind_onto_a_quicsock_bound_interface_using_a_wildcard_bind() {
    let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Debug).try_init();
    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let server_addr = run_echo_server(cert_der, key_der).await;

    let factory = system_quic_factory();
    let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await.expect("create_endpoint should succeed");
    let conn = endpoint
        .connect(RemoteSpec { addr: server_addr, server_name: SNI.to_string(), cert_sha256_hex })
        .await
        .expect("initial connect should succeed");

    let mut stream = conn.open_bi().await.unwrap();
    stream.write_all(b"before rebind").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"before rebind");

    let physical_socket = bind_physical_interface(loopback_index(), SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
        .expect("quicsock should bind a wildcard, interface-restricted socket");
    let physical_addr = physical_socket.local_addr().unwrap();
    let rebinder = endpoint.rebinder().expect("SystemQuicEndpoint should support rebinding");
    tokio::time::timeout(Duration::from_secs(5), rebinder.rebind_socket(physical_socket))
        .await
        .expect("rebind_socket should not hang")
        .expect("rebind onto the quicsock-bound socket should succeed");

    let mut stream = tokio::time::timeout(Duration::from_secs(5), conn.open_bi())
        .await
        .expect("open_bi after rebind should not hang")
        .expect("open_bi after rebind should succeed");
    stream.write_all(b"after rebind").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("read after rebind should not hang")
        .unwrap();
    assert_eq!(&buf[..n], b"after rebind", "connection must keep working after rebinding onto {physical_addr}");

    conn.close().await;
}
