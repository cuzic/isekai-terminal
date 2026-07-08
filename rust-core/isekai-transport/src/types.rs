//! Value types shared by the `QuicEndpointFactory`/`QuicEndpoint` traits
//! (`archive/ISEKAI_SSH_DESIGN.md` "実装方針" trait design).

use std::net::{Ipv4Addr, SocketAddr};

/// Local address to bind a new QUIC endpoint's UDP socket to.
///
/// A thin wrapper around `SocketAddr` today — every current caller just
/// wants an OS-assigned ephemeral port (`any_ipv4()`) — but kept as its own
/// type rather than a bare `SocketAddr` parameter so a later phase (S-0d-2:
/// STUN/P2P, reconnect/backoff) can grow it (e.g. an already-bound socket
/// reused across hole-punch probes and the eventual QUIC endpoint, mirroring
/// `isekai_stun_p2p_transport.rs` on the Android side) without changing the
/// `QuicEndpointFactory` signature.
#[derive(Debug, Clone, Copy)]
pub struct BindSpec {
    pub local_addr: SocketAddr,
}

impl BindSpec {
    /// Bind to an OS-assigned ephemeral port on the IPv4 wildcard address —
    /// the only bind spec `connect_via_relay` needs (relay connections are
    /// always outbound, `isekai_link_relay_transport.rs::connect_relay_stream`
    /// does the same with `"0.0.0.0:0"`).
    pub fn any_ipv4() -> Self {
        Self { local_addr: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0) }
    }
}

/// Remote endpoint to connect to, plus the certificate-pinning fingerprint
/// that must be checked instead of a normal CA chain (`archive/HELPER_PROTOCOL.md`
/// §2/§4: isekai-helper serves an ephemeral self-signed certificate, and its
/// SHA-256 fingerprint is delivered out-of-band over the bootstrap SSH
/// channel).
#[derive(Debug, Clone)]
pub struct RemoteSpec {
    pub addr: SocketAddr,
    /// TLS SNI / QUIC server name. isekai-helper does not check this against
    /// anything — it presents one fixed self-signed cert regardless of SNI —
    /// but rustls's `ServerCertVerifier` API requires *some* name to pass
    /// through; `PinnedCertVerifier` (`system.rs`) ignores it and checks
    /// `cert_sha256_hex` instead.
    pub server_name: String,
    /// Lowercase hex-encoded SHA-256 fingerprint of the expected leaf
    /// certificate (`isekai_protocol::handshake::HandshakeJson::cert_sha256`,
    /// already validated by that module's decoder before it reaches here).
    pub cert_sha256_hex: String,
}
