//! Client for `bound-udp-server`'s CONNECT-UDP-bind endpoint — the "agent"
//! role: this crate's only role (see the module-level rationale in `lib.rs`
//! for why `isekai-terminal`, the "client" role of the relay's own
//! terminology, needs none of this).
//!
//! What "agent role" means concretely: `isekai-helper`, running behind a NAT
//! it cannot otherwise be reached through, opens ONE CONNECT-UDP-bind tunnel
//! to the relay over HTTP/3. The relay's response carries a
//! `proxy-public-address` header — a stable public `ip:port` that the relay
//! itself now listens on and forwards to/from this tunnel. That address is
//! handed back to `isekai-terminal` (over the existing SSH bootstrap channel,
//! the same way `isekai_stun_p2p_transport.rs` hands back a STUN-observed
//! address) and `isekai-terminal` then does a completely ordinary QUIC client
//! connect to it — `isekai-terminal` never speaks MASQUE/HTTP/3/capsules at
//! all. The relay is, from `isekai-terminal`'s point of view, indistinguishable
//! from `isekai-helper` listening directly at that address.
//!
//! Registering context id 0 as *uncompressed* (`CompressionAssign { context_id: 0,
//! addr: None }`, verified against `bound_udp/service.rs`) once, right after the
//! tunnel opens, is sufficient: every datagram in either direction then carries
//! its own `ip_version + addr + port` prefix (`datagram_codec.rs`), so the relay
//! never needs to know `isekai-terminal`'s address in advance (important — unlike
//! a VPN-style MASQUE client, this crate has no out-of-band way to tell the relay
//! who will be connecting before that peer's first UDP datagram, the QUIC
//! Initial packet, actually arrives). This costs a few extra prefix bytes per
//! datagram; that overhead was judged an acceptable, simpler alternative to
//! wiring up compressed contexts.
//!
//! `proxy-public-address` is obtained directly from the CONNECT-UDP-bind
//! response headers — there is no need to separately call `GET
//! /public_address` (verified: `bound_udp/service.rs`'s handler sets
//! `proxy-public-address` on every successful bind response, and this is the
//! exact address it also stores via `public_address_store`).

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes};
use h3_datagram::datagram_handler::HandleDatagramsExt;
use http::Method;
use noq::udp::{RecvMeta, Transmit};
use noq::{AsyncUdpSocket, UdpSender};
use tokio::sync::mpsc;

use crate::capsule::Capsule;
use crate::datagram_codec::{decode_datagram_payload, encode_datagram_payload};

/// The relay's own custom wildcard CONNECT-UDP path — not RFC 9298's
/// `{target_host}/{target_port}` form, verified against
/// `bound_udp/service.rs::is_masque_bound_udp_path`.
const BOUND_UDP_PATH: &str = "/.well-known/masque/udp/*/*";
/// The uncompressed "accept from any peer address" context id this crate
/// always registers (see module docs for why one context id is enough).
const UNCOMPRESSED_CONTEXT_ID: u64 = 0;
/// ALPN protocol id for HTTP/3 (RFC 9114 §3.1). `bound-udp-server` (via
/// `h3-util`) negotiates this the same way regardless of QUIC backend.
const H3_ALPN: &[u8] = b"h3";

#[derive(Debug, thiserror::Error)]
pub enum RelayClientError {
    #[error("failed to bind local UDP socket: {0}")]
    Bind(io::Error),
    #[error("QUIC connect to relay failed: {0}")]
    QuicConnect(String),
    #[error("HTTP/3 handshake with relay failed: {0}")]
    H3Handshake(String),
    #[error("CONNECT-UDP-bind request failed: {0}")]
    ConnectRequest(String),
    #[error("relay rejected CONNECT-UDP-bind with status {0}")]
    RejectedStatus(http::StatusCode),
    #[error("relay CONNECT-UDP-bind response missing proxy-public-address header")]
    MissingProxyPublicAddress,
    #[error("relay's proxy-public-address header is not a valid socket address: {0}")]
    InvalidProxyPublicAddress(String),
    #[error("failed to read capsule response from relay: {0}")]
    CapsuleRead(String),
    #[error("relay closed the compression context instead of acknowledging it")]
    CompressionRejected,
    #[error("relay sent an unexpected capsule while registering the compression context: {0:?}")]
    UnexpectedCapsule(Capsule),
    #[error("relay closed the CONNECT-UDP-bind stream before acknowledging the compression context")]
    StreamClosed,
}

/// `noq::TransportConfig` suitable for a CONNECT-UDP-bind tunnel connection
/// (either side): raises `initial_mtu` above the QUIC-mandated 1200-byte
/// minimum and `datagram_receive_buffer_size` well above what's needed to
/// carry a forwarded QUIC Initial packet (already ~1200 bytes on its own)
/// plus this crate's own context_id/address-prefix framing (`datagram_codec.rs`).
/// Without this, `noq`'s default `initial_mtu` (exactly 1200) makes the very
/// first forwarded datagram — before MTU discovery has had a chance to raise
/// it — too large to send at all, which is exactly the failure this crate
/// hit against a real Initial packet during development. Exposed so tests
/// (`tests/relay_e2e.rs`) can apply the same settings to their mock relay's
/// server config, since the size ceiling is enforced by whichever side is
/// *receiving* a given direction's datagrams.
pub fn uplink_transport_config() -> noq::TransportConfig {
    let mut transport = noq::TransportConfig::default();
    transport.datagram_receive_buffer_size(Some(64 * 1024));
    transport.initial_mtu(1500);
    transport
}

/// Opens a CONNECT-UDP-bind tunnel to `relay_addr` (TLS SNI `relay_sni`,
/// verified against the real CA-issued certificate — this is a production
/// relay, not `isekai-helper`'s self-signed ephemeral cert) authenticated with
/// `jwt` as an `Authorization: Bearer` token, registers the uncompressed
/// forwarding context, and returns a [`RelayUdpSocket`] usable as a
/// `noq::AsyncUdpSocket` standing in for a real bound UDP socket, together
/// with the relay-assigned public address `isekai-terminal` should connect to.
pub async fn connect_relay_agent(
    relay_addr: SocketAddr,
    relay_sni: &str,
    jwt: &str,
) -> Result<(RelayUdpSocket, SocketAddr), RelayClientError> {
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut crypto = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    crypto.alpn_protocols = vec![H3_ALPN.to_vec()];
    let quic_crypto = noq::crypto::rustls::QuicClientConfig::try_from(crypto)
        .map_err(|e| RelayClientError::QuicConnect(format!("TLS config: {e}")))?;
    let mut client_config = noq::ClientConfig::new(Arc::new(quic_crypto));
    client_config.transport_config(Arc::new(uplink_transport_config()));

    connect_relay_agent_with_client_config(relay_addr, relay_sni, jwt, client_config).await
}

/// Core of [`connect_relay_agent`], parameterized over the `noq::ClientConfig`
/// (and therefore the TLS verifier) so tests can exercise everything below
/// the certificate-trust layer against a local mock relay with a self-signed
/// cert, without weakening what production actually uses (real CA
/// verification via `webpki-roots`, since the real relay has an ACME-issued
/// certificate — see `connect_relay_agent`).
pub async fn connect_relay_agent_with_client_config(
    relay_addr: SocketAddr,
    relay_sni: &str,
    jwt: &str,
    client_config: noq::ClientConfig,
) -> Result<(RelayUdpSocket, SocketAddr), RelayClientError> {
    let std_socket =
        std::net::UdpSocket::bind("0.0.0.0:0").map_err(RelayClientError::Bind)?;
    std_socket.set_nonblocking(true).map_err(RelayClientError::Bind)?;
    let uplink_socket = Arc::new(
        tokio::net::UdpSocket::from_std(std_socket).map_err(RelayClientError::Bind)?,
    );

    let endpoint = noq::Endpoint::new_with_abstract_socket(
        noq::EndpointConfig::default(),
        None,
        Box::new(UplinkUdpSocket::new(uplink_socket)),
        Arc::new(noq::TokioRuntime),
    )
    .map_err(|e| RelayClientError::QuicConnect(format!("endpoint bind failed: {e}")))?;
    endpoint.set_default_client_config(client_config);

    let conn = endpoint
        .connect(relay_addr, relay_sni)
        .map_err(|e| RelayClientError::QuicConnect(format!("connect setup failed: {e}")))?
        .await
        .map_err(|e| RelayClientError::QuicConnect(format!("QUIC handshake failed: {e}")))?;

    let h3_conn = h3_noq::Connection::new(conn);
    let (mut driver, mut send_request) = h3::client::new(h3_conn)
        .await
        .map_err(|e| RelayClientError::H3Handshake(e.to_string()))?;

    let uri: http::Uri = format!("https://{relay_sni}{BOUND_UDP_PATH}")
        .parse()
        .expect("relay_sni is a valid authority and BOUND_UDP_PATH is a valid path");
    let req = http::Request::builder()
        .method(Method::CONNECT)
        .uri(uri)
        .header("connect-udp-bind", "?1")
        .header("capsule-protocol", "?1")
        .header("authorization", format!("Bearer {jwt}"))
        .extension(h3::ext::Protocol::CONNECT_UDP)
        .body(())
        .expect("well-formed CONNECT-UDP-bind request");

    let mut stream = send_request
        .send_request(req)
        .await
        .map_err(|e| RelayClientError::ConnectRequest(e.to_string()))?;

    let resp = stream
        .recv_response()
        .await
        .map_err(|e| RelayClientError::ConnectRequest(e.to_string()))?;
    if resp.status() != http::StatusCode::OK {
        return Err(RelayClientError::RejectedStatus(resp.status()));
    }
    let proxy_public_address: SocketAddr = resp
        .headers()
        .get("proxy-public-address")
        .ok_or(RelayClientError::MissingProxyPublicAddress)?
        .to_str()
        .map_err(|e| RelayClientError::InvalidProxyPublicAddress(e.to_string()))?
        .parse()
        .map_err(|e: std::net::AddrParseError| {
            RelayClientError::InvalidProxyPublicAddress(e.to_string())
        })?;

    // Register the single uncompressed context (see module docs) and wait
    // for the relay's COMPRESSION_ACK before treating the tunnel as usable.
    let assign = Capsule::CompressionAssign { context_id: UNCOMPRESSED_CONTEXT_ID, addr: None };
    stream
        .send_data(Bytes::from(assign.encode()))
        .await
        .map_err(|e| RelayClientError::CapsuleRead(e.to_string()))?;
    await_compression_ack(&mut stream).await?;

    let stream_id = stream.id();
    let datagram_sender = driver.get_datagram_sender(stream_id);
    let mut datagram_reader = driver.get_datagram_reader();

    // Keeps the HTTP/3 connection driven for the lifetime of the tunnel.
    // `stream`/`send_request` are moved in too so the CONNECT stream (and
    // therefore the relay's forwarding registration) stays open; none of
    // these are ever read from again after the compression handshake above,
    // but dropping them would tear the tunnel down.
    tokio::spawn(async move {
        let _stream = stream;
        let _send_request = send_request;
        std::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });

    let (recv_tx, recv_rx) = mpsc::unbounded_channel::<(SocketAddr, Bytes)>();
    tokio::spawn(async move {
        loop {
            let datagram = match datagram_reader.read_datagram().await {
                Ok(d) => d,
                Err(e) => {
                    log::info!("isekai-link-masque: relay datagram reader ended: {e}");
                    break;
                }
            };
            let payload = datagram.payload();
            match decode_datagram_payload(payload, false) {
                Some((_context_id, Some(addr), data)) => {
                    if recv_tx.send((addr, Bytes::copy_from_slice(data))).is_err() {
                        break;
                    }
                }
                Some((context_id, None, _)) => {
                    log::warn!(
                        "isekai-link-masque: received datagram for compressed context {context_id}, but only the uncompressed context {UNCOMPRESSED_CONTEXT_ID} was ever registered"
                    );
                }
                None => log::warn!("isekai-link-masque: received malformed relay datagram"),
            }
        }
    });

    let (send_tx, mut send_rx) = mpsc::unbounded_channel::<(SocketAddr, Bytes)>();
    tokio::spawn(async move {
        let mut datagram_sender = datagram_sender;
        while let Some((addr, payload)) = send_rx.recv().await {
            let encoded = encode_datagram_payload(UNCOMPRESSED_CONTEXT_ID, Some(addr), &payload);
            if let Err(e) = datagram_sender.send_datagram(encoded) {
                log::info!("isekai-link-masque: relay datagram sender ended: {e}");
                break;
            }
        }
    });

    Ok((
        RelayUdpSocket { recv_rx, send_tx, local_addr: proxy_public_address },
        proxy_public_address,
    ))
}

/// Reads capsule-framed body data off `stream` until a complete capsule is
/// available, expecting `CompressionAck` for [`UNCOMPRESSED_CONTEXT_ID`].
async fn await_compression_ack<S, B>(
    stream: &mut h3::client::RequestStream<S, B>,
) -> Result<(), RelayClientError>
where
    S: h3::quic::RecvStream,
    B: Buf,
{
    let mut reader = crate::capsule::CapsuleReader::new();
    loop {
        if let Some(capsule) = reader
            .next_capsule()
            .map_err(|e| RelayClientError::CapsuleRead(e.to_string()))?
        {
            return match capsule {
                Capsule::CompressionAck { context_id } if context_id == UNCOMPRESSED_CONTEXT_ID => {
                    Ok(())
                }
                Capsule::CompressionClose { context_id } if context_id == UNCOMPRESSED_CONTEXT_ID => {
                    Err(RelayClientError::CompressionRejected)
                }
                other => Err(RelayClientError::UnexpectedCapsule(other)),
            };
        }
        let chunk = stream
            .recv_data()
            .await
            .map_err(|e| RelayClientError::CapsuleRead(e.to_string()))?
            .ok_or(RelayClientError::StreamClosed)?;
        let mut buf = vec![0u8; chunk.remaining()];
        let mut chunk = chunk;
        chunk.copy_to_slice(&mut buf);
        reader.feed(&buf);
    }
}

/// Stands in for a real bound UDP socket on top of the relay's
/// CONNECT-UDP-bind tunnel: `poll_send`/`poll_recv` translate to/from the
/// tunnel's QUIC Datagram channel via `datagram_codec.rs`, so callers (e.g.
/// `isekai-helper`'s `noq::Endpoint::server`) can use it exactly like
/// `isekai-helper`'s own `PlainUdpSocket`.
pub struct RelayUdpSocket {
    recv_rx: mpsc::UnboundedReceiver<(SocketAddr, Bytes)>,
    send_tx: mpsc::UnboundedSender<(SocketAddr, Bytes)>,
    local_addr: SocketAddr,
}

impl std::fmt::Debug for RelayUdpSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayUdpSocket").field("local_addr", &self.local_addr).finish()
    }
}

struct RelayUdpSender {
    tx: mpsc::UnboundedSender<(SocketAddr, Bytes)>,
}

impl std::fmt::Debug for RelayUdpSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RelayUdpSender")
    }
}

impl UdpSender for RelayUdpSender {
    fn poll_send(self: Pin<&mut Self>, transmit: &Transmit<'_>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let payload = Bytes::copy_from_slice(transmit.contents);
        match self.tx.send((transmit.destination, payload)) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "relay send-pump task ended (CONNECT-UDP-bind tunnel closed)",
            ))),
        }
    }
}

impl AsyncUdpSocket for RelayUdpSocket {
    fn create_sender(&self) -> Pin<Box<dyn UdpSender>> {
        Box::pin(RelayUdpSender { tx: self.send_tx.clone() })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        match self.recv_rx.poll_recv(cx) {
            Poll::Ready(Some((addr, payload))) => {
                let n = payload.len().min(bufs[0].len());
                bufs[0][..n].copy_from_slice(&payload[..n]);
                let mut m = RecvMeta::default();
                m.addr = addr;
                m.len = n;
                m.stride = n;
                meta[0] = m;
                Poll::Ready(Ok(1))
            }
            Poll::Ready(None) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay recv-pump task ended (CONNECT-UDP-bind tunnel closed)",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

/// Plain pass-through `noq::AsyncUdpSocket` for the *uplink* connection to
/// the relay itself (isekai-helper→relay), with no fault injection — mirrors
/// `isekai-helper/src/plain_socket.rs`'s `PlainUdpSocket` exactly. Not shared
/// as a dependency across the crate boundary for a ~30-line adapter.
struct UplinkUdpSocket {
    inner: Arc<tokio::net::UdpSocket>,
}

impl UplinkUdpSocket {
    fn new(inner: Arc<tokio::net::UdpSocket>) -> Self {
        Self { inner }
    }
}

impl std::fmt::Debug for UplinkUdpSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UplinkUdpSocket").finish_non_exhaustive()
    }
}

struct UplinkUdpSender {
    inner: Arc<tokio::net::UdpSocket>,
}

impl std::fmt::Debug for UplinkUdpSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UplinkUdpSender")
    }
}

impl UdpSender for UplinkUdpSender {
    fn poll_send(self: Pin<&mut Self>, transmit: &Transmit<'_>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.inner.poll_send_to(cx, transmit.contents, transmit.destination) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncUdpSocket for UplinkUdpSocket {
    fn create_sender(&self) -> Pin<Box<dyn UdpSender>> {
        Box::pin(UplinkUdpSender { inner: self.inner.clone() })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut read_buf = tokio::io::ReadBuf::new(&mut bufs[0]);
        match self.inner.poll_recv_from(cx, &mut read_buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(addr)) => {
                let len = read_buf.filled().len();
                let mut m = RecvMeta::default();
                m.addr = addr;
                m.len = len;
                m.stride = len;
                meta[0] = m;
                Poll::Ready(Ok(1))
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}
