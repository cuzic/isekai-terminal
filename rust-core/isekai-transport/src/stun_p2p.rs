//! STUN+SSH rendezvous P2P QUIC connection establishment
//! (`ISEKAI_SSH_DESIGN.md` phase S-0d-2), extracted from `isekai-terminal-core`'s
//! `isekai_stun_p2p_transport.rs`.
//!
//! Scope of this module (mirrors `try_connect_isekai_stun_p2p` /
//! `connect_stun_p2p_stream`, **minus** the parts out of scope for this
//! phase):
//! - Bind a fresh UDP socket, query a STUN server for this socket's own
//!   observed address on it (`isekai_stun::query_stun`).
//! - Send hole-punch probes to the peer's already-known observed address
//!   (simultaneous open).
//! - Reuse that *same* socket as a QUIC endpoint
//!   (`system::quic_endpoint_from_std_socket`) and perform the
//!   HELLO/proof/ACK handshake against the peer
//!   (`relay::connect_and_handshake`, shared with `connect_via_relay`).
//!
//! Explicitly **out of scope** here (`ISEKAI_SSH_DESIGN.md`'s task
//! description for this phase):
//! - The SSH-bootstrap step that actually exchanges `our_observed_addr`/
//!   `peer_addr` out-of-band between the two sides
//!   (`bootstrap_via_ssh_with_punch` on the Android side). Callers of
//!   `connect_stun_p2p` must already know `target.peer_addr` by whatever
//!   means (a future `isekai-bootstrap`/`isekai-ssh` wiring, S-6) — this
//!   crate does not know how to reach a bootstrap channel.
//! - `resume_client::ClientResumeState`/`reattach_fn`/the control stream —
//!   resume support lands in S-4a onward.

use std::net::SocketAddr;
use std::time::Duration;

use log::info;

use crate::error::TransportError;
use crate::relay::connect_and_handshake;
use crate::system::quic_endpoint_from_std_socket;
use crate::traits::ByteStream;
use crate::types::{BindSpec, RemoteSpec};

/// Number of hole-punch probe datagrams sent to the peer's observed address
/// before attempting the QUIC handshake. Matches
/// `isekai_stun_p2p_transport.rs::PUNCH_PROBE_COUNT`.
const PUNCH_PROBE_COUNT: u32 = 5;
/// Interval between hole-punch probes. Matches
/// `isekai_stun_p2p_transport.rs::PUNCH_PROBE_INTERVAL`.
const PUNCH_PROBE_INTERVAL: Duration = Duration::from_millis(150);
/// Payload of each hole-punch probe datagram. The content is never parsed by
/// either side — it exists purely to prime a NAT mapping / trigger
/// simultaneous open — so any fixed byte string works
/// (`isekai_stun_p2p_transport.rs` uses the same literal).
const PUNCH_PROBE_PAYLOAD: &[u8] = b"isekai-punch";

/// Everything `connect_stun_p2p` needs to know about the remote isekai-helper
/// instance reached directly (peer-to-peer, no relay). Mirrors the subset of
/// `isekai_stun_p2p_transport.rs::connect_stun_p2p_stream`'s inputs this
/// crate is responsible for.
#[derive(Debug, Clone)]
pub struct StunP2pTarget {
    /// The peer's (isekai-helper's) own STUN-observed address
    /// (`HelperHandshake::stun_observed_addr` on the Android side), obtained
    /// out-of-band by the caller. Exchanging this value is explicitly out of
    /// scope for this crate (`ISEKAI_SSH_DESIGN.md` S-6: a future
    /// `isekai-bootstrap`/`isekai-ssh` concern).
    pub peer_addr: SocketAddr,
    /// TLS SNI / QUIC server name (`RemoteSpec::server_name`'s docs: ignored
    /// by isekai-helper, but required by rustls's API).
    pub server_name: String,
    /// `HandshakeJson::cert_sha256` (already validated by
    /// `isekai_protocol::handshake::decode_handshake_json`).
    pub cert_sha256_hex: String,
    /// Already base64-decoded `HandshakeJson::session_secret`.
    pub session_secret: Vec<u8>,
}

/// Result of a successful `connect_stun_p2p` call: the HELLO/ACK'd byte
/// stream, plus this side's own STUN-observed address — in case the caller
/// still needs to hand it to a signaling/bootstrap channel. Producing that
/// value is this crate's job; wiring it anywhere is not
/// (`ISEKAI_SSH_DESIGN.md` S-6).
pub struct StunP2pConnection {
    pub our_observed_addr: SocketAddr,
    pub stream: Box<dyn ByteStream>,
}

/// Binds a fresh UDP socket, queries `stun_server` for this socket's own
/// observed address, sends hole-punch probes to `target.peer_addr`
/// (simultaneous open — the peer is assumed to be probing this side's
/// observed address at roughly the same time, by whatever out-of-band
/// exchange got `target.peer_addr` to this caller in the first place), then
/// reuses the *same* socket as a QUIC endpoint to perform the HELLO/proof/ACK
/// handshake against `target.peer_addr`.
///
/// Mirrors `isekai_stun_p2p_transport.rs::try_connect_isekai_stun_p2p` +
/// `connect_stun_p2p_stream`'s connection-establishment portion; the
/// SSH-bootstrap step that exchanges observed addresses out-of-band is the
/// caller's responsibility here, not this function's (see module docs).
pub async fn connect_stun_p2p(
    stun_server: SocketAddr,
    target: &StunP2pTarget,
) -> Result<StunP2pConnection, TransportError> {
    let bind_addr = BindSpec::any_ipv4().local_addr;
    let socket = tokio::net::UdpSocket::bind(bind_addr)
        .await
        .map_err(|source| TransportError::Bind { addr: bind_addr, source })?;

    let our_observed_addr = isekai_stun::query_stun(&socket, stun_server).await?;
    info!("isekai-transport: our STUN-observed address is {our_observed_addr} (via {stun_server})");

    // Simultaneous open: fire a handful of probes at the peer's observed
    // address before attempting the QUIC handshake so both sides' NAT
    // mappings are primed at roughly the same time
    // (`isekai_stun_p2p_transport.rs`'s comment on why this needs to happen
    // on the *same* socket that will become the QUIC endpoint).
    for _ in 0..PUNCH_PROBE_COUNT {
        let _ = socket.send_to(PUNCH_PROBE_PAYLOAD, target.peer_addr).await;
        tokio::time::sleep(PUNCH_PROBE_INTERVAL).await;
    }

    let std_socket = socket
        .into_std()
        .map_err(|e| TransportError::SocketSetup(e.to_string()))?;
    let endpoint = quic_endpoint_from_std_socket(std_socket)?;

    let remote = RemoteSpec {
        addr: target.peer_addr,
        server_name: target.server_name.clone(),
        cert_sha256_hex: target.cert_sha256_hex.clone(),
    };
    let (_conn, stream, _proof) = connect_and_handshake(endpoint.as_ref(), remote, &target.session_secret).await?;

    Ok(StunP2pConnection { our_observed_addr, stream })
}
