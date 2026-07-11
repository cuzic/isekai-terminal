//! Backend-agnostic connect-input types: where to bind locally, and which
//! remote endpoint (plus pinned certificate fingerprint) to dial. Mirrors
//! `isekai-transport`'s original `BindSpec`/`RemoteSpec`, but owned by this
//! crate — `quicmux` must never depend on `isekai-transport`/
//! `isekai-protocol` (that dependency direction is exactly backwards; see
//! this crate's top-level docs), so these had to move here rather than stay
//! borrowed from the crate that used to define them.

use std::net::{Ipv4Addr, SocketAddr};

/// Local address to bind a new mux endpoint's socket to.
///
/// A thin wrapper around `SocketAddr` — kept as its own type rather than a
/// bare `SocketAddr` parameter purely for call-site clarity (`bind: BindSpec`
/// reads better than an unlabeled `SocketAddr` at a call site that also
/// takes a `RemoteSpec`).
#[derive(Debug, Clone, Copy)]
pub struct BindSpec {
    pub local_addr: SocketAddr,
}

impl BindSpec {
    /// Bind to an OS-assigned ephemeral port on the IPv4 wildcard address —
    /// the common case for an outbound-only connection.
    pub fn any_ipv4() -> Self {
        Self { local_addr: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0) }
    }
}

/// Remote endpoint to connect to, plus the certificate-pinning fingerprint
/// that must be checked instead of a normal CA chain — every backend this
/// crate supports authenticates the peer by pinned SHA-256 fingerprint
/// rather than a CA chain (the deployed peer presents an ephemeral
/// self-signed certificate whose fingerprint was delivered out-of-band by
/// the caller).
#[derive(Debug, Clone)]
pub struct RemoteSpec {
    pub addr: SocketAddr,
    /// TLS SNI / server name. Some peers ignore this entirely and present
    /// one fixed self-signed certificate regardless of SNI, but `rustls`'s
    /// `ServerCertVerifier` API requires *some* name to pass through; the
    /// pinned-fingerprint verifier ([`crate::PinnedCertVerifier`]) ignores it
    /// and checks `cert_sha256_hex` instead.
    pub server_name: String,
    /// Lowercase hex-encoded SHA-256 fingerprint of the expected leaf
    /// certificate.
    pub cert_sha256_hex: String,
}
