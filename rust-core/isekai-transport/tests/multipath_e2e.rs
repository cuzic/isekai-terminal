//! End-to-end test for `connect_multipath` against a real local `noq`
//! server standing in for isekai-pipe's own QUIC server — proves the
//! primary path (the initial QUIC handshake) and a secondary path (opened
//! afterward via `open_path`, `local_ip: None`) both actually validate over
//! real loopback UDP sockets, and that `PathHealthTracker` observes both as
//! `Validated`. This is the "path0/path1" pattern from
//! `isekai-terminal-core`'s `multipath_transport.rs` (Phase 9-2/9-3),
//! generalized — see `isekai-transport::multipath`'s module docs.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use isekai_protocol::hello::ALPN;
use isekai_transport::{connect_multipath, BindSpec, PathLabel, PathState, RemoteSpec, SecondaryPath, PRIMARY_PATH_LABEL};
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

/// Bound to the IPv4 wildcard address (not just `127.0.0.1`, unlike
/// `relay_e2e.rs`'s `mock_helper_server`) so it also receives datagrams
/// addressed to `127.0.0.2` — the secondary path's source/destination in
/// this test (loopback's whole `127.0.0.0/8` range is routed to `lo`
/// without needing an explicit second address assignment, the same
/// assumption `quicsock-noq`'s own tests make).
fn mock_server(cert_der: CertificateDer<'static>, key_der: PrivatePkcs8KeyDer<'static>) -> noq::Endpoint {
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())
        .unwrap();
    tls_config.alpn_protocols = vec![ALPN.to_vec()];
    let quic_crypto = noq::crypto::rustls::QuicServerConfig::try_from(tls_config).unwrap();
    let mut config = noq::ServerConfig::with_crypto(Arc::new(quic_crypto));
    // Both sides must advertise noq's multipath extension before
    // `open_path` will do anything but fail with "multipath extension not
    // negotiated" — see `client_config_for`'s `multipath` parameter docs.
    let mut transport = noq::TransportConfig::default();
    transport.max_concurrent_multipath_paths(8);
    config.transport_config(Arc::new(transport));
    noq::Endpoint::server(config, SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)).unwrap()
}

#[tokio::test]
async fn primary_and_secondary_paths_both_validate_over_loopback() {
    let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Debug).try_init();
    let (cert_der, key_der, cert_sha256_hex) = generate_cert();
    let endpoint = mock_server(cert_der, key_der);
    let port = endpoint.local_addr().unwrap().port();
    let primary_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let secondary_addr: SocketAddr = format!("127.0.0.2:{port}").parse().unwrap();

    // Keeps the connection (and thus its paths) alive long enough for both
    // this test's `connect_multipath` call and its secondary-path polling
    // loop below to finish — dropping `conn` tears the whole thing down.
    let server_task = tokio::spawn(async move {
        let incoming = endpoint.accept().await.unwrap();
        let conn = incoming.await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        conn.close(noq::VarInt::from_u32(0), b"");
    });

    let primary = RemoteSpec { addr: primary_addr, server_name: SNI.to_string(), cert_sha256_hex };
    let secondary = SecondaryPath { label: PathLabel::Borrowed("secondary"), addr: secondary_addr };
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);

    let mp = tokio::time::timeout(
        Duration::from_secs(10),
        connect_multipath(BindSpec::any_ipv4(), primary, vec![secondary], event_tx),
    )
    .await
    .expect("connect_multipath should not hang")
    .expect("connect_multipath should establish the primary path");

    let primary_label = PathLabel::Borrowed(PRIMARY_PATH_LABEL);
    assert_eq!(mp.tracker.get(&primary_label), PathState::Validated, "primary path should validate immediately");

    // Secondary path establishment happens in a background task
    // (`open_path_with_retry`), so poll for it instead of asserting
    // immediately.
    let secondary_label = PathLabel::Borrowed("secondary");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        if mp.tracker.get(&secondary_label) == PathState::Validated {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "secondary path did not validate in time");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    mp.conn.close(noq::VarInt::from_u32(0), b"");
    server_task.await.unwrap();
}
