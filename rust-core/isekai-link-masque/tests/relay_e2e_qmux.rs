//! End-to-end test for `relay_client.rs::connect_relay_agent_via_qmux_with_tls_config`
//! against a local mock relay built directly on `h3-qmux` — the QMux
//! (draft-ietf-quic-qmux) counterpart of `relay_e2e.rs`'s noq-based mock
//! relay. Structural mirror of that file: same CONNECT-UDP-bind wire
//! contract (path, headers, COMPRESSION_ASSIGN/ACK capsule bytes,
//! `datagram_codec.rs` payload framing), only the transport carrying the
//! HTTP/3 connection to the relay differs (QMux-over-TLS-over-TCP here,
//! real QUIC-over-UDP there). Proves `run_connect_udp_bind_agent`'s shared
//! capsule/compression/datagram-pump logic is genuinely transport-agnostic,
//! without depending on the real deployed relay (whose actual QMux ingress
//! ALPN/version — see `connect_relay_agent_via_qmux`'s doc comment on
//! `H3_QMUX_ALPN` in `relay_client.rs` — is unverified from this repo alone).
//!
//! The downstream leg (isekai-helper's own QUIC server using the tunnel as
//! its abstract socket, and isekai-terminal's ordinary QUIC client dialing
//! the relay-assigned public address) is unchanged from `relay_e2e.rs` —
//! real `noq`, real UDP — since only the uplink (isekai-helper→relay) leg is
//! what `connect_relay_agent_via_qmux` replaces.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use h3_datagram::datagram_handler::HandleDatagramsExt;
use isekai_link_masque::capsule::{Capsule, CapsuleReader};
use isekai_link_masque::datagram_codec::{decode_datagram_payload, encode_datagram_payload};
use isekai_link_masque::{connect_relay_agent_via_qmux_with_tls_config, uplink_transport_config};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;

/// Must match `relay_client.rs`'s private `H3_QMUX_ALPN` constant.
const RELAY_QMUX_ALPN: &str = "h3qx-01";
const RELAY_ALPN: &[u8] = b"h3";

fn generate_cert(sni: &str) -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>) {
    let cert = rcgen::generate_simple_self_signed(vec![sni.to_string()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    (cert_der, key_der)
}

fn relay_server_tls_config(cert_der: CertificateDer<'static>, key_der: PrivatePkcs8KeyDer<'static>) -> rustls::ServerConfig {
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())
        .unwrap();
    config.alpn_protocols = vec![RELAY_QMUX_ALPN.as_bytes().to_vec()];
    config
}

/// A `rustls::ClientConfig` that trusts exactly `cert_der` — standing in for
/// `connect_relay_agent_via_qmux`'s real `webpki-roots` verification, which a
/// self-signed test cert obviously can't satisfy.
fn trusting_client_tls_config(cert_der: &CertificateDer<'static>) -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der.clone()).unwrap();
    let mut config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    config.alpn_protocols = vec![RELAY_QMUX_ALPN.as_bytes().to_vec()];
    config
}

fn downstream_server_config(cert_der: CertificateDer<'static>, key_der: PrivatePkcs8KeyDer<'static>) -> noq::ServerConfig {
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

/// Runs the mock relay's server-side handling of exactly one
/// CONNECT-UDP-bind tunnel over QMux: TLS-accepts the TCP connection,
/// establishes the `qmux::Session`, then does the same request
/// validation/capsule/datagram-pump dance as `relay_e2e.rs`'s
/// `run_mock_relay` (see that function's doc comment for the wire-contract
/// details, unchanged here).
async fn run_mock_relay_over_qmux(
    listener: TcpListener,
    tls_config: rustls::ServerConfig,
    public_socket: Arc<tokio::net::UdpSocket>,
) {
    let (tcp, _peer) = listener.accept().await.unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));
    let tls_stream = acceptor.accept(tcp).await.unwrap();

    let config = qmux::Config::negotiated(qmux::Version::QMux01, Some(RELAY_QMUX_ALPN.to_string()));
    let transport = qmux::transport::Stream::new(tls_stream, config.version, config.max_record_size);
    let session = qmux::Session::accept(transport, config).await.unwrap();

    let mut h3_conn = h3::server::Connection::new(h3_qmux::Connection::new(session, true)).await.unwrap();

    let resolver = h3_conn.accept().await.unwrap().expect("expected one CONNECT-UDP-bind request");
    let (req, mut stream) = resolver.resolve_request().await.unwrap();

    assert_eq!(req.method(), &http::Method::CONNECT);
    assert_eq!(
        req.extensions().get::<h3::ext::Protocol>(),
        Some(&h3::ext::Protocol::CONNECT_UDP),
        "connect_relay_agent_via_qmux must set the CONNECT_UDP extended-CONNECT protocol extension"
    );
    assert_eq!(req.headers().get("connect-udp-bind").unwrap(), "?1");
    assert_eq!(req.headers().get("capsule-protocol").unwrap(), "?1");
    assert_eq!(req.headers().get("authorization").unwrap(), "Bearer test-jwt-token");
    assert_eq!(
        req.uri().path(),
        "/.well-known/masque/udp/*/*",
        "connect_relay_agent_via_qmux must use the relay's custom wildcard CONNECT-UDP path"
    );

    let resp = http::Response::builder()
        .status(http::StatusCode::OK)
        .header("connect-udp-bind", "?1")
        .header("capsule-protocol", "?1")
        .header("proxy-public-address", public_socket.local_addr().unwrap().to_string())
        .body(())
        .unwrap();
    stream.send_response(resp).await.unwrap();

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
async fn full_tunnel_round_trips_real_quic_traffic_through_a_qmux_relay() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (relay_cert, relay_key) = generate_cert("relay.test");
    let listener = TcpListener::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).await.unwrap();
    let relay_addr = listener.local_addr().unwrap();
    let relay_tls_config = relay_server_tls_config(relay_cert.clone(), relay_key);

    let public_socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());

    let relay_task = tokio::spawn(run_mock_relay_over_qmux(listener, relay_tls_config, public_socket));

    let client_tls_config = trusting_client_tls_config(&relay_cert);
    let (relay_udp_socket, proxy_public_address) = connect_relay_agent_via_qmux_with_tls_config(
        relay_addr,
        "relay.test",
        "test-jwt-token",
        client_tls_config,
    )
    .await
    .expect("connect_relay_agent_via_qmux_with_tls_config should negotiate the tunnel");

    // `isekai-helper`'s own noq server endpoint, using the (QMux-tunneled)
    // relay socket as its abstract socket instead of a real bound UDP socket
    // — identical to `relay_e2e.rs` from here on, since only the uplink leg
    // differs.
    let (downstream_cert, downstream_key) = generate_cert("isekai-pipe.local");
    let downstream_cert_der = downstream_cert.clone();
    let helper_endpoint = noq::Endpoint::new_with_abstract_socket(
        noq::EndpointConfig::default(),
        Some(downstream_server_config(downstream_cert, downstream_key)),
        Box::new(relay_udp_socket),
        Arc::new(noq::TokioRuntime),
    )
    .unwrap();

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
    // directly to isekai-helper — no MASQUE/HTTP/3/capsule/QMux awareness at
    // all (this leg is real QUIC-over-UDP regardless of how the uplink leg
    // reached the relay).
    let mut roots = rustls::RootCertStore::empty();
    roots.add(downstream_cert_der).unwrap();
    let mut terminal_tls = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    terminal_tls.alpn_protocols = vec![RELAY_ALPN.to_vec()];
    let terminal_quic_crypto = noq::crypto::rustls::QuicClientConfig::try_from(terminal_tls).unwrap();
    let terminal_client_config = noq::ClientConfig::new(Arc::new(terminal_quic_crypto));

    let terminal_endpoint = noq::Endpoint::client(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap();
    terminal_endpoint.set_default_client_config(terminal_client_config);

    let conn = tokio::time::timeout(
        Duration::from_secs(10),
        terminal_endpoint.connect(proxy_public_address, "isekai-pipe.local").unwrap(),
    )
    .await
    .expect("QUIC handshake through the qmux relay tunnel timed out")
    .expect("QUIC handshake through the qmux relay tunnel failed");

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
