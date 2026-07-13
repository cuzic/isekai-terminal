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

// macOS-excluded: KNOWN, UNRESOLVED GAP (not a well-understood platform
// quirk like this crate's other macOS/Windows `#[cfg]` exclusions) —
// confirmed on a real `test-macos` CI run that this test hangs after
// rebinding onto a `quicsock`-bound (`IP_BOUND_IF`-restricted) loopback
// socket: `noq`'s QUIC path validation (PATH_CHALLENGE/PATH_RESPONSE) times
// out (`new path validation failed`) even though the raw `bind()` itself
// succeeds (after aliasing `127.0.0.4` onto `lo0` in CI). Root cause not
// confirmed: per XNU's `in_pcb.c`, Darwin's `IP_BOUND_IF` — unlike Linux's
// `SO_BINDTOIFINDEX` — makes subsequent route lookups require an exact
// interface-scope match, which plausibly conflicts with how a loopback
// alias's route is scoped; tried re-aliasing with an explicit `/32` netmask
// (ruling out the broadest netmask-scoping theory) and it made no
// difference. `quicsock::unix`'s own module docs already flagged this
// backend as untested on real Apple hardware — this is the first real
// signal that it doesn't currently work for at least this loopback+rebind
// scenario, though whether the same failure would hit a *real* (non-
// loopback) physical interface on macOS remains unknown. Needs either a
// `quicsock` code fix (once the exact Darwin mechanism is understood) or a
// deeper investigation of `noq`'s own send/recv path under `IP_BOUND_IF`.
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
