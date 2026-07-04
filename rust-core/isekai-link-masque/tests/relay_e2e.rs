//! End-to-end test for `relay_client.rs` against a local mock relay server
//! built directly on `h3-noq` (already proven to interoperate with `h3` at
//! the wire/runtime level by `h3-noq/tests/smoke.rs`). This is not the real
//! `seera-networks/axum-masque-rs` `bound-udp-server` binary — building that
//! locally requires a from-source `msquic` (C++/cmake) build that this
//! environment could not complete (see PLAN.md Phase 10 notes) — but the
//! mock server here implements the exact wire contract read directly out of
//! that repository's `bound_udp/service.rs`: the same wildcard CONNECT-UDP
//! path, the same `connect-udp-bind`/`capsule-protocol`/`proxy-public-address`
//! headers, the same COMPRESSION_ASSIGN/ACK capsule bytes, and the same
//! `datagram_codec.rs` payload framing.
//!
//! The test proves the full path end-to-end: `connect_relay_agent_with_client_config`
//! negotiates the tunnel and returns a `RelayUdpSocket`; that socket is then
//! used as the abstract socket for a *real* `noq::Endpoint::server` (standing
//! in for `isekai-helper`'s own QUIC server); a *second*, completely
//! independent `noq::Endpoint::client` (standing in for `isekai-terminal`)
//! connects to the relay-assigned `proxy_public_address` and completes a real
//! QUIC handshake plus a bidirectional stream exchange — with the mock relay
//! physically forwarding raw UDP datagrams between the two in between.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use h3_datagram::datagram_handler::HandleDatagramsExt;
use isekai_link_masque::capsule::{Capsule, CapsuleReader};
use isekai_link_masque::datagram_codec::{decode_datagram_payload, encode_datagram_payload};
use isekai_link_masque::{connect_relay_agent_with_client_config, uplink_transport_config};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

const RELAY_ALPN: &[u8] = b"h3";

fn generate_cert(sni: &str) -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>) {
    let cert = rcgen::generate_simple_self_signed(vec![sni.to_string()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    (cert_der, key_der)
}

fn relay_server_config(cert_der: CertificateDer<'static>, key_der: PrivatePkcs8KeyDer<'static>) -> noq::ServerConfig {
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())
        .unwrap();
    tls_config.alpn_protocols = vec![RELAY_ALPN.to_vec()];
    let quic_crypto = noq::crypto::rustls::QuicServerConfig::try_from(tls_config).unwrap();
    let mut config = noq::ServerConfig::with_crypto(Arc::new(quic_crypto));
    config.transport_config(Arc::new(uplink_transport_config()));
    config
}

/// A `noq::ClientConfig` that trusts exactly `cert_der` — standing in for
/// `connect_relay_agent`'s real `webpki-roots` verification, which a
/// self-signed test cert obviously can't satisfy.
fn trusting_client_config(cert_der: &CertificateDer<'static>) -> noq::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der.clone()).unwrap();
    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![RELAY_ALPN.to_vec()];
    let quic_crypto = noq::crypto::rustls::QuicClientConfig::try_from(tls_config).unwrap();
    let mut config = noq::ClientConfig::new(Arc::new(quic_crypto));
    config.transport_config(Arc::new(uplink_transport_config()));
    config
}

/// Runs the mock relay's server-side handling of exactly one CONNECT-UDP-bind
/// tunnel: accepts the QUIC+HTTP/3 connection, validates the request the way
/// `bound_udp/service.rs::validate_connect_udp_request` does, responds with
/// the bind headers, performs the COMPRESSION_ASSIGN/ACK handshake, then
/// forwards raw UDP between `public_socket` and the tunnel's datagram channel
/// until the test ends.
async fn run_mock_relay(server_endpoint: noq::Endpoint, public_socket: Arc<tokio::net::UdpSocket>) {
    let incoming = server_endpoint.accept().await.unwrap();
    let conn = incoming.await.unwrap();
    let mut h3_conn = h3::server::Connection::new(h3_noq::Connection::new(conn)).await.unwrap();

    let resolver = h3_conn.accept().await.unwrap().expect("expected one CONNECT-UDP-bind request");
    let (req, mut stream) = resolver.resolve_request().await.unwrap();

    assert_eq!(req.method(), &http::Method::CONNECT);
    assert_eq!(
        req.extensions().get::<h3::ext::Protocol>(),
        Some(&h3::ext::Protocol::CONNECT_UDP),
        "relay_client.rs must set the CONNECT_UDP extended-CONNECT protocol extension"
    );
    assert_eq!(
        req.headers().get("connect-udp-bind").unwrap(),
        "?1",
        "relay_client.rs must send connect-udp-bind: ?1"
    );
    assert_eq!(
        req.headers().get("capsule-protocol").unwrap(),
        "?1",
        "relay_client.rs must send capsule-protocol: ?1"
    );
    assert_eq!(
        req.headers().get("authorization").unwrap(),
        "Bearer test-jwt-token",
        "relay_client.rs must send the JWT as an Authorization: Bearer header"
    );
    assert_eq!(
        req.uri().path(),
        "/.well-known/masque/udp/*/*",
        "relay_client.rs must use the relay's custom wildcard CONNECT-UDP path"
    );

    let resp = http::Response::builder()
        .status(http::StatusCode::OK)
        .header("connect-udp-bind", "?1")
        .header("capsule-protocol", "?1")
        .header("proxy-public-address", public_socket.local_addr().unwrap().to_string())
        .body(())
        .unwrap();
    stream.send_response(resp).await.unwrap();

    // Read the client's COMPRESSION_ASSIGN capsule and answer with COMPRESSION_ACK,
    // exactly as `bound_udp/service.rs` does for a successfully-registered context.
    let mut reader = CapsuleReader::new();
    let capsule = loop {
        if let Some(c) = reader.next_capsule().unwrap() {
            break c;
        }
        let mut chunk = stream.recv_data().await.unwrap().expect("stream closed before capsule arrived");
        use bytes::Buf;
        let mut buf = vec![0u8; chunk.remaining()];
        chunk.copy_to_slice(&mut buf);
        reader.feed(&buf);
    };
    assert_eq!(capsule, Capsule::CompressionAssign { context_id: 0, addr: None });
    let ack = Capsule::CompressionAck { context_id: 0 };
    stream.send_data(Bytes::from(ack.encode())).await.unwrap();

    let stream_id = stream.id();
    let mut datagram_reader = h3_conn.get_datagram_reader();
    let mut datagram_sender = h3_conn.get_datagram_sender(stream_id);

    // tunnel -> public_socket (models isekai-helper sending UDP toward isekai-terminal)
    let uplink = {
        let public_socket = public_socket.clone();
        tokio::spawn(async move {
            loop {
                let Ok(datagram) = datagram_reader.read_datagram().await else { break };
                if let Some((_ctx, Some(addr), data)) = decode_datagram_payload(datagram.payload(), false) {
                    let _ = public_socket.send_to(data, addr).await;
                }
            }
        })
    };

    // public_socket -> tunnel (models a peer's UDP arriving at the relay's
    // dedicated public address and being forwarded to isekai-helper)
    let downlink = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, from)) = public_socket.recv_from(&mut buf).await else { break };
            let encoded = encode_datagram_payload(0, Some(from), &buf[..n]);
            if datagram_sender.send_datagram(encoded).is_err() {
                break;
            }
        }
    });

    // Keep the request stream (and therefore the relay's forwarding
    // registration) alive for the rest of the test.
    tokio::time::sleep(Duration::from_secs(5)).await;
    uplink.abort();
    downlink.abort();
}

#[tokio::test]
async fn full_tunnel_round_trips_real_quic_traffic_through_the_relay() {
    let (relay_cert, relay_key) = generate_cert("relay.test");
    let relay_endpoint = noq::Endpoint::server(
        relay_server_config(relay_cert.clone(), relay_key),
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
    )
    .unwrap();
    let relay_addr = relay_endpoint.local_addr().unwrap();

    let public_socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());

    let relay_task = tokio::spawn(run_mock_relay(relay_endpoint, public_socket));

    let client_config = trusting_client_config(&relay_cert);
    let (relay_udp_socket, proxy_public_address) =
        connect_relay_agent_with_client_config(relay_addr, "relay.test", "test-jwt-token", client_config)
            .await
            .expect("connect_relay_agent_with_client_config should negotiate the tunnel");

    // `isekai-helper`'s own noq server endpoint, using the relay tunnel as
    // its abstract socket instead of a real bound UDP socket.
    let (downstream_cert, downstream_key) = generate_cert("isekai-helper.local");
    let downstream_cert_der = downstream_cert.clone();
    let helper_endpoint = noq::Endpoint::new_with_abstract_socket(
        noq::EndpointConfig::default(),
        Some(relay_server_config(downstream_cert, downstream_key)),
        Box::new(relay_udp_socket),
        Arc::new(noq::TokioRuntime),
    )
    .unwrap();

    // See `h3-noq/tests/smoke.rs` for why this hand-off is needed: dropping
    // `conn` (which happens as soon as this task's async block ends) as soon
    // as `finish()` returns races the peer actually draining the response
    // out of its receive buffer — `finish()` only means "no more data to
    // send", not "peer has read it". Only drop `conn` after the terminal
    // side confirms (over this channel) that it has read the response.
    let (terminal_done_tx, terminal_done_rx) = tokio::sync::oneshot::channel::<()>();
    let helper_accept_task = tokio::spawn(async move {
        let incoming = helper_endpoint.accept().await.unwrap();
        let conn = incoming.await.unwrap();
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let mut buf = [0u8; 64];
        let n = recv.read(&mut buf).await.unwrap().unwrap();
        assert_eq!(&buf[..n], b"hello from isekai-terminal");
        send.write_all(b"hello from isekai-helper").await.unwrap();
        send.finish().unwrap();
        terminal_done_rx.await.ok();
    });

    // `isekai-terminal`'s side: an entirely ordinary QUIC client, connecting
    // to the relay's assigned public address exactly as if it were talking
    // directly to isekai-helper — no MASQUE/HTTP/3/capsule awareness at all.
    let mut roots = rustls::RootCertStore::empty();
    roots.add(downstream_cert_der).unwrap();
    let mut terminal_tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    terminal_tls.alpn_protocols = vec![RELAY_ALPN.to_vec()];
    let terminal_quic_crypto = noq::crypto::rustls::QuicClientConfig::try_from(terminal_tls).unwrap();
    let terminal_client_config = noq::ClientConfig::new(Arc::new(terminal_quic_crypto));

    let terminal_endpoint = noq::Endpoint::client(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap();
    terminal_endpoint.set_default_client_config(terminal_client_config);

    let conn = tokio::time::timeout(
        Duration::from_secs(10),
        terminal_endpoint.connect(proxy_public_address, "isekai-helper.local").unwrap(),
    )
    .await
    .expect("QUIC handshake through the relay tunnel timed out")
    .expect("QUIC handshake through the relay tunnel failed");

    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(b"hello from isekai-terminal").await.unwrap();
    send.finish().unwrap();
    let mut buf = [0u8; 64];
    let n = recv.read(&mut buf).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"hello from isekai-helper");

    terminal_done_tx.send(()).ok();
    helper_accept_task.await.unwrap();
    relay_task.abort();
}
