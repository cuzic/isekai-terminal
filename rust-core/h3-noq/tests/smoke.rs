//! End-to-end smoke test: drives a real HTTP/3 GET request/response over a
//! real noq QUIC connection through h3-noq's `quic::Connection` impl. This is
//! not a type-check — it proves h3 (from hyperium/h3 PR #340) actually
//! interoperates with noq at the wire/runtime level, not just that the trait
//! impls compile.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use bytes::{Buf, Bytes};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

static ALPN: &[u8] = b"h3-noq-smoke-test";

fn server_config(cert_der: CertificateDer<'static>, key_der: PrivatePkcs8KeyDer<'static>) -> noq::ServerConfig {
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())
        .unwrap();
    tls_config.alpn_protocols = vec![ALPN.to_vec()];
    let quic_crypto = noq::crypto::rustls::QuicServerConfig::try_from(tls_config).unwrap();
    noq::ServerConfig::with_crypto(Arc::new(quic_crypto))
}

fn client_config(cert_der: &[u8]) -> noq::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(cert_der.to_vec())).unwrap();
    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![ALPN.to_vec()];
    let quic_crypto = noq::crypto::rustls::QuicClientConfig::try_from(tls_config).unwrap();
    noq::ClientConfig::new(Arc::new(quic_crypto))
}

#[tokio::test]
async fn h3_over_noq_get_request_round_trips() {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());

    let server_endpoint = noq::Endpoint::server(
        server_config(cert_der.clone(), key_der),
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
    )
    .unwrap();
    let server_addr = server_endpoint.local_addr().unwrap();

    let expected_body = b"hello from h3-noq server";

    // `h3::server::Connection::shutdown()` only waits for the server side's
    // own bookkeeping to consider the accepted request "done" (i.e. that
    // `finish()` was called) — it does not wait for the client to have
    // actually drained the response out of its receive buffer at the
    // application layer. Racing the connection's teardown against that is
    // exactly the kind of thing this smoke test exists to exercise faithfully
    // rather than paper over, so synchronize explicitly: only drop the
    // server's `h3_conn` after the client confirms (over this channel) that
    // it has fully read the response.
    let (client_done_tx, client_done_rx) = tokio::sync::oneshot::channel::<()>();

    let server_task = tokio::spawn(async move {
        let incoming = server_endpoint.accept().await.unwrap();
        let conn = incoming.await.unwrap();

        let mut h3_conn = h3::server::Connection::new(h3_noq::Connection::new(conn))
            .await
            .unwrap();

        let Some(resolver) = h3_conn.accept().await.unwrap() else {
            panic!("expected one request");
        };
        let (req, mut stream) = resolver.resolve_request().await.unwrap();
        assert_eq!(req.uri().path(), "/hello");

        let resp = http::Response::builder()
            .status(http::StatusCode::OK)
            .body(())
            .unwrap();
        stream.send_response(resp).await.unwrap();
        stream
            .send_data(Bytes::from_static(expected_body))
            .await
            .unwrap();
        stream.finish().await.unwrap();

        client_done_rx.await.ok();
    });

    let client_endpoint =
        noq::Endpoint::client(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap();
    client_endpoint.set_default_client_config(client_config(&cert_der));

    let conn = client_endpoint
        .connect(server_addr, "localhost")
        .unwrap()
        .await
        .unwrap();

    let noq_conn = h3_noq::Connection::new(conn);
    let (mut driver, mut send_request) = h3::client::new(noq_conn).await.unwrap();

    let drive = tokio::spawn(async move {
        std::future::poll_fn(|cx| driver.poll_close(cx)).await
    });

    let req = http::Request::builder()
        .uri("https://localhost/hello")
        .body(())
        .unwrap();
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

/// MASQUE (RFC 9298, `CONNECT-UDP`) is built on HTTP/3 Datagrams (RFC 9297),
/// which need the underlying QUIC connection to support unreliable datagram
/// frames (RFC 9221) — this is the actual prerequisite this whole h3-noq
/// investigation was about, not just generic HTTP/3 request/response. Proves
/// a datagram associated with a request stream round-trips both directions
/// (client→server and server→client) over a real noq connection, exercising
/// `h3-noq`'s `datagram` feature (`DatagramConnectionExt` impl).
#[tokio::test]
async fn h3_datagram_over_noq_round_trips() {
    use h3_datagram::datagram_handler::HandleDatagramsExt;

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());

    let server_endpoint = noq::Endpoint::server(
        server_config(cert_der.clone(), key_der),
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
    )
    .unwrap();
    let server_addr = server_endpoint.local_addr().unwrap();

    let client_to_server = Bytes::from_static(b"client->server datagram");
    let server_to_client = Bytes::from_static(b"server->client datagram");

    let (client_done_tx, client_done_rx) = tokio::sync::oneshot::channel::<()>();

    let expected_from_client = client_to_server.clone();
    let reply_from_server = server_to_client.clone();
    let server_task = tokio::spawn(async move {
        let incoming = server_endpoint.accept().await.unwrap();
        let conn = incoming.await.unwrap();

        let mut h3_conn = h3::server::Connection::new(h3_noq::Connection::new(conn))
            .await
            .unwrap();

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

        let resp = http::Response::builder()
            .status(http::StatusCode::OK)
            .body(())
            .unwrap();
        stream.send_response(resp).await.unwrap();
        stream.finish().await.unwrap();

        client_done_rx.await.ok();
    });

    let client_endpoint =
        noq::Endpoint::client(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)).unwrap();
    client_endpoint.set_default_client_config(client_config(&cert_der));

    let conn = client_endpoint
        .connect(server_addr, "localhost")
        .unwrap()
        .await
        .unwrap();

    let noq_conn = h3_noq::Connection::new(conn);
    let (mut driver, mut send_request) = h3::client::new(noq_conn).await.unwrap();

    let req = http::Request::builder()
        .uri("https://localhost/hello")
        .body(())
        .unwrap();
    let mut stream = send_request.send_request(req).await.unwrap();
    stream.finish().await.unwrap();
    let stream_id = stream.id();

    // Must be obtained before `driver` is moved into the spawned poll_close
    // task below: `HandleDatagramsExt` is implemented on `h3::client::Connection`
    // itself (the driver), not on `SendRequest`/`RequestStream`.
    let mut datagram_reader = driver.get_datagram_reader();
    let mut datagram_sender = driver.get_datagram_sender(stream_id);

    let drive = tokio::spawn(async move {
        std::future::poll_fn(|cx| driver.poll_close(cx)).await
    });

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
