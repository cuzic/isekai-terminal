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
    /// When set, a free port within this inclusive range is chosen instead
    /// of `local_addr`'s own port (which callers leave `0` when setting
    /// this). Lets a caller narrow which *outbound* UDP port range a local
    /// firewall/NAT needs to permit, instead of the whole OS ephemeral
    /// range — the client-side counterpart of `isekai-pipe serve
    /// --bind-port-range` on the remote side.
    pub port_range: Option<(u16, u16)>,
}

impl BindSpec {
    /// Bind to an OS-assigned ephemeral port on the IPv4 wildcard address —
    /// the common case for an outbound-only connection.
    pub fn any_ipv4() -> Self {
        Self { local_addr: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0), port_range: None }
    }

    /// Bind to an OS-assigned ephemeral port on the wildcard address of the
    /// same family as `remote` — an IPv4-bound socket can never reach an
    /// IPv6 peer (IPv6-only cellular/464XLAT networks), so every outbound
    /// dial site should use this rather than [`Self::any_ipv4`].
    pub fn unspecified_for(remote: SocketAddr) -> Self {
        let ip: std::net::IpAddr = match remote {
            SocketAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
            SocketAddr::V6(_) => std::net::Ipv6Addr::UNSPECIFIED.into(),
        };
        Self { local_addr: SocketAddr::new(ip, 0), port_range: None }
    }

    /// `local` re-targeted to `remote`'s address family when `local` is an
    /// *unspecified* (wildcard) address of the other family — a specific
    /// local address is returned unchanged (binding it is then the caller's
    /// explicit choice). Used by backends whose endpoint binds lazily at
    /// connect time, when the remote is finally known.
    pub fn adapt_to_remote(local: SocketAddr, remote: SocketAddr) -> SocketAddr {
        if local.ip().is_unspecified() && local.is_ipv4() != remote.is_ipv4() {
            let ip: std::net::IpAddr = if remote.is_ipv4() { Ipv4Addr::UNSPECIFIED.into() } else { std::net::Ipv6Addr::UNSPECIFIED.into() };
            SocketAddr::new(ip, local.port())
        } else {
            local
        }
    }

    /// Builder-style setter for [`Self::port_range`], for a call site that
    /// starts from [`Self::any_ipv4`] and layers an optional caller-supplied
    /// range on top.
    pub fn with_port_range(mut self, port_range: Option<(u16, u16)>) -> Self {
        self.port_range = port_range;
        self
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_for_matches_the_remote_family() {
        let v4: SocketAddr = "203.0.113.5:443".parse().unwrap();
        let v6: SocketAddr = "[2001:db8::1]:443".parse().unwrap();
        assert_eq!(BindSpec::unspecified_for(v4).local_addr, "0.0.0.0:0".parse().unwrap());
        assert_eq!(BindSpec::unspecified_for(v6).local_addr, "[::]:0".parse().unwrap());
    }

    #[test]
    fn adapt_to_remote_only_retargets_a_wildcard_of_the_wrong_family() {
        let v6: SocketAddr = "[2001:db8::1]:443".parse().unwrap();
        let v4: SocketAddr = "203.0.113.5:443".parse().unwrap();
        assert_eq!(BindSpec::adapt_to_remote("0.0.0.0:0".parse().unwrap(), v6), "[::]:0".parse().unwrap());
        assert_eq!(BindSpec::adapt_to_remote("[::]:7000".parse().unwrap(), v4), "0.0.0.0:7000".parse().unwrap());
        let specific: SocketAddr = "192.168.1.2:0".parse().unwrap();
        assert_eq!(BindSpec::adapt_to_remote(specific, v6), specific);
        assert_eq!(BindSpec::adapt_to_remote("0.0.0.0:0".parse().unwrap(), v4), "0.0.0.0:0".parse().unwrap());
    }
}
