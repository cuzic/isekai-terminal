//! End-to-end smoke test: drives a real HTTP/3 GET request/response over a
//! real `qmux::Session` (itself running over a real TCP socket + rustls TLS
//! handshake) through h3-qmux's `quic::Connection` impl. Mirrors
//! `h3-noq/tests/smoke.rs` exactly, swapping noq's UDP `Endpoint` for a plain
//! `TcpListener`/`TcpStream` pair wrapped in `tokio_rustls`. This is not a
//! type-check — it proves h3 (from hyperium/h3 PR #340) actually
//! interoperates with `qmux` at the wire/runtime level, not just that the
//! trait impls compile.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use bytes::{Buf, Bytes};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName};
use tokio::net::{TcpListener, TcpStream};

static ALPN: &[u8] = b"h3-qmux-smoke-test";

fn server_tls_config(cert_der: CertificateDer<'static>, key_der: PrivatePkcs8KeyDer<'static>) -> rustls::ServerConfig {
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())
        .unwrap();
    config.alpn_protocols = vec![ALPN.to_vec()];
    config
}

fn client_tls_config(cert_der: &[u8]) -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(cert_der.to_vec())).unwrap();
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![ALPN.to_vec()];
    config
}

#[tokio::test]
async fn h3_over_qmux_get_request_round_trips() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());

    let listener = TcpListener::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).await.unwrap();
    let server_addr = listener.local_addr().unwrap();

    let expected_body = b"hello from h3-qmux server";

    let (client_done_tx, client_done_rx) = tokio::sync::oneshot::channel::<()>();

    let server_tls_config = server_tls_config(cert_der.clone(), key_der);
    let server_task = tokio::spawn(async move {
        let (tcp, _peer) = listener.accept().await.unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_tls_config));
        let tls_stream = acceptor.accept(tcp).await.unwrap();

        let config = qmux::Config::new(qmux::Version::QMux01);
        let transport = qmux::transport::Stream::new(tls_stream, config.version, config.max_record_size);
        let session = qmux::Session::accept(transport, config).await.unwrap();

        let mut h3_conn = h3::server::Connection::new(h3_qmux::Connection::new(session, true)).await.unwrap();

        let Some(resolver) = h3_conn.accept().await.unwrap() else {
            panic!("expected one request");
        };
        let (req, mut stream) = resolver.resolve_request().await.unwrap();
        assert_eq!(req.uri().path(), "/hello");

        let resp = http::Response::builder().status(http::StatusCode::OK).body(()).unwrap();
        stream.send_response(resp).await.unwrap();
        stream.send_data(Bytes::from_static(expected_body)).await.unwrap();
        stream.finish().await.unwrap();

        client_done_rx.await.ok();
    });

    let client_tls_config = client_tls_config(&cert_der);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_tls_config));
    let tcp = TcpStream::connect(server_addr).await.unwrap();
    let server_name = ServerName::try_from("localhost").unwrap();
    let tls_stream = connector.connect(server_name, tcp).await.unwrap();

    let config = qmux::Config::new(qmux::Version::QMux01);
    let transport = qmux::transport::Stream::new(tls_stream, config.version, config.max_record_size);
    let session = qmux::Session::connect(transport, config).await.unwrap();

    let qmux_conn = h3_qmux::Connection::new(session, false);
    let (mut driver, mut send_request) = h3::client::new(qmux_conn).await.unwrap();

    let drive = tokio::spawn(async move { std::future::poll_fn(|cx| driver.poll_close(cx)).await });

    let req = http::Request::builder().uri("https://localhost/hello").body(()).unwrap();
    let mut stream = send_request.send_request(req).await.unwrap();
    stream.finish().await.unwrap();

    let resp = stream.recv_response().await.unwrap();
    assert_eq!(resp.status(), http::StatusCode::OK);

    let mut received = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await.unwrap() {
        let mut buf = vec![0u8; chunk.remaining()];
        chunk.copy_to_slice(&mut buf);
        received.extend_from_slice(&buf);
    }
    assert_eq!(received, expected_body);

    client_done_tx.send(()).ok();
    drop(send_request);

    server_task.await.unwrap();
    drive.abort();
}

/// MASQUE (RFC 9298, `CONNECT-UDP`) is built on HTTP/3 Datagrams (RFC 9297).
/// `isekai-link-masque`'s relay tunnel needs this to carry forwarded UDP
/// payloads, so this is the actual prerequisite the whole h3-qmux exercise is
/// for — not just generic HTTP/3 request/response. Proves a datagram
/// associated with a request stream round-trips both directions
/// (client→server and server→client) over a real `qmux::Session`, exercising
/// h3-qmux's `datagram` feature (`DatagramConnectionExt` impl).
#[tokio::test]
async fn h3_datagram_over_qmux_round_trips() {
    use h3_datagram::datagram_handler::HandleDatagramsExt;

    let _ = rustls::crypto::ring::default_provider().install_default();

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());

    let listener = TcpListener::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).await.unwrap();
    let server_addr = listener.local_addr().unwrap();

    let client_to_server = Bytes::from_static(b"client->server datagram");
    let server_to_client = Bytes::from_static(b"server->client datagram");

    let (client_done_tx, client_done_rx) = tokio::sync::oneshot::channel::<()>();

    let server_tls_config = server_tls_config(cert_der.clone(), key_der);
    let expected_from_client = client_to_server.clone();
    let reply_from_server = server_to_client.clone();
    let server_task = tokio::spawn(async move {
        let (tcp, _peer) = listener.accept().await.unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_tls_config));
        let tls_stream = acceptor.accept(tcp).await.unwrap();

        let config = qmux::Config::new(qmux::Version::QMux01);
        let transport = qmux::transport::Stream::new(tls_stream, config.version, config.max_record_size);
        let session = qmux::Session::accept(transport, config).await.unwrap();

        let mut h3_conn = h3::server::Connection::new(h3_qmux::Connection::new(session, true)).await.unwrap();

        let Some(resolver) = h3_conn.accept().await.unwrap() else {
            panic!("expected one request");
        };
        let (req, mut stream) = resolver.resolve_request().await.unwrap();
        assert_eq!(req.uri().path(), "/hello");
        let stream_id = stream.id();

        let mut datagram_reader = h3_conn.get_datagram_reader();
        let mut datagram_sender = h3_conn.get_datagram_sender(stream_id);

        let received = datagram_reader.read_datagram().await.unwrap();
        assert_eq!(received.stream_id(), stream_id);
        assert_eq!(received.payload(), &expected_from_client);

        datagram_sender.send_datagram(reply_from_server).unwrap();

        let resp = http::Response::builder().status(http::StatusCode::OK).body(()).unwrap();
        stream.send_response(resp).await.unwrap();
        stream.finish().await.unwrap();

        client_done_rx.await.ok();
    });

    let client_tls_config = client_tls_config(&cert_der);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_tls_config));
    let tcp = TcpStream::connect(server_addr).await.unwrap();
    let server_name = ServerName::try_from("localhost").unwrap();
    let tls_stream = connector.connect(server_name, tcp).await.unwrap();

    let config = qmux::Config::new(qmux::Version::QMux01);
    let transport = qmux::transport::Stream::new(tls_stream, config.version, config.max_record_size);
    let session = qmux::Session::connect(transport, config).await.unwrap();

    let qmux_conn = h3_qmux::Connection::new(session, false);
    let (mut driver, mut send_request) = h3::client::new(qmux_conn).await.unwrap();

    let req = http::Request::builder().uri("https://localhost/hello").body(()).unwrap();
    let mut stream = send_request.send_request(req).await.unwrap();
    stream.finish().await.unwrap();
    let stream_id = stream.id();

    // Must be obtained before `driver` is moved into the spawned poll_close
    // task below: `HandleDatagramsExt` is implemented on `h3::client::Connection`
    // itself (the driver), not on `SendRequest`/`RequestStream`.
    let mut datagram_reader = driver.get_datagram_reader();
    let mut datagram_sender = driver.get_datagram_sender(stream_id);

    let drive = tokio::spawn(async move { std::future::poll_fn(|cx| driver.poll_close(cx)).await });

    datagram_sender.send_datagram(client_to_server).unwrap();

    let received = datagram_reader.read_datagram().await.unwrap();
    assert_eq!(received.stream_id(), stream_id);
    assert_eq!(received.payload(), &server_to_client);

    let resp = stream.recv_response().await.unwrap();
    assert_eq!(resp.status(), http::StatusCode::OK);

    client_done_tx.send(()).ok();
    drop(send_request);

    server_task.await.unwrap();
    drive.abort();
}
