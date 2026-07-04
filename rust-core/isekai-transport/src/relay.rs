//! Relay-only connection establishment: HELLO/proof/ACK against the
//! relay-assigned public address of a remote isekai-helper
//! (`ISEKAI_SSH_DESIGN.md` phase S-0d-1). Mirrors
//! `isekai_link_relay_transport.rs::connect_relay_stream`, minus what is out
//! of scope for this phase: no `resume_client::ReattachableStream` hand-off
//! and no control stream (`RESUME`/session table land in S-4a onward).

use std::net::SocketAddr;

use isekai_protocol::hello::{decode_ack_response, encode_hello, AckResponse};
use log::info;

use crate::error::TransportError;
use crate::proof::compute_proof;
use crate::traits::{ByteStream, QuicEndpointFactory};
use crate::types::{BindSpec, RemoteSpec};

/// Everything `connect_via_relay` needs to know about one specific
/// isekai-helper instance's relay-assigned endpoint. Mirrors the subset of
/// `isekai_link_relay_transport.rs::IsekaiLinkRelayConfig` /
/// `helper_bootstrap::HelperHandshake` this crate actually consumes — SSH
/// bootstrap and handshake-JSON parsing are the caller's responsibility
/// (`isekai_protocol::handshake`), not this crate's.
#[derive(Debug, Clone)]
pub struct RelayTarget {
    /// The relay-assigned public address of the remote isekai-helper
    /// (`HandshakeJson::relay_public_addr`), *not* the relay server itself —
    /// by the time this crate is called, the relay's MASQUE tunnel has
    /// already been set up on isekai-helper's side
    /// (`ISEKAI_SSH_DESIGN.md` "isekai-helper・isekai-sshの統合方針").
    pub helper_addr: SocketAddr,
    /// SNI presented during the QUIC handshake. isekai-helper ignores it
    /// (see `RemoteSpec::server_name`'s docs); kept configurable rather than
    /// hardcoded so a future non-isekai-helper QUIC endpoint could reuse this
    /// function.
    pub server_name: String,
    /// `HandshakeJson::cert_sha256` (already validated by
    /// `isekai_protocol::handshake::decode_handshake_json`).
    pub cert_sha256_hex: String,
    /// Already base64-decoded `HandshakeJson::session_secret`.
    pub session_secret: Vec<u8>,
}

/// Establishes a fresh QUIC connection to `target.helper_addr`, pinned to
/// `target.cert_sha256_hex`, then performs the HELLO/proof/ACK handshake
/// (`HELPER_PROTOCOL.md` §4) using `isekai_protocol::hello`. On success,
/// returns the already-open bidirectional QUIC stream: from this point on it
/// is a raw byte pass-through to isekai-helper's target TCP connection.
///
/// Deliberately does *not* open a control stream or return a resume-capable
/// handle — `ISEKAI_SSH_DESIGN.md`'s S-0d-1 scope is "HELLO/proof/ACKまでの
/// 接続確立だけでよい"; resume support lands in S-4a.
pub async fn connect_via_relay(
    factory: &dyn QuicEndpointFactory,
    target: &RelayTarget,
) -> Result<Box<dyn ByteStream>, TransportError> {
    let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await?;
    let conn = endpoint
        .connect(RemoteSpec {
            addr: target.helper_addr,
            server_name: target.server_name.clone(),
            cert_sha256_hex: target.cert_sha256_hex.clone(),
        })
        .await?;

    let proof = compute_proof(conn.as_ref(), &target.session_secret, b"").await?;

    let mut stream = conn.open_bi().await?;
    stream.write_all(&encode_hello(&proof)).await?;

    let mut resp = [0u8; 1];
    read_exact(stream.as_mut(), &mut resp).await?;
    match decode_ack_response(resp[0])? {
        AckResponse::Ack => {
            info!("isekai-transport: HELLO/ACK ok — stream ready for pass-through");
            Ok(stream)
        }
        other => Err(TransportError::Rejected(other)),
    }
}

/// `ByteStream::read` only guarantees "at most `buf.len()` bytes, possibly
/// fewer"; the 1-byte ACK response needs the usual `read_exact` loop on top.
async fn read_exact(stream: &mut dyn ByteStream, buf: &mut [u8]) -> Result<(), TransportError> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = stream.read(&mut buf[filled..]).await?;
        if n == 0 {
            return Err(TransportError::UnexpectedEof);
        }
        filled += n;
    }
    Ok(())
}
