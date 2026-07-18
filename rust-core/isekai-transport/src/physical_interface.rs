//! Binding a UDP socket to a specific physical network interface, for
//! [`quicmux::AnyMuxRebinder::rebind_socket`] — the CLI/PC side
//! of the "rebind onto a warm-standby physical interface" mechanism
//! (`multipath_transport.rs`'s Phase 9-4b `rebind_abstract()`, real-hardware
//! verified on Android; see [`crate::path_health`]'s module docs for why
//! this reactive-rebind approach is what's proven working, unlike
//! same-connection simultaneous physical multipath).
//!
//! This is a thin wrapper over the vendored `quicsock` crate — see that
//! crate's own docs for the per-platform mechanism
//! (`SO_BINDTOIFINDEX`/`IP_BOUND_IF`/`IP_UNICAST_IF`) and its important
//! Android caveat (a plain interface-index bind does not reliably route
//! traffic through the requested interface on Android app/framework
//! environments — Android's own rebind path does not go through this
//! module at all, it imports an fd already bound via
//! `android.net.Network.bindSocket()` on the Kotlin/JNI side and hands it
//! straight to `rebind_socket`, matching `quicsock`'s own recommendation).

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use rand::Rng as _;

use crate::error::TransportError;

// Re-exported (not just `InterfaceIndex`) so callers outside this crate
// (e.g. `isekai-pipe`/`isekai-ssh`, or this crate's own integration tests)
// can enumerate interfaces via `quicsock::discovery` without adding their
// own direct dependency on `quicsock`.
pub use quicsock;
pub use quicsock::InterfaceIndex;

/// Binds a UDP socket restricted to `interface` at `local_addr`, ready to
/// pass to [`quicmux::AnyMuxRebinder::rebind_socket`]. `quicsock`
/// itself returns a [`socket2::Socket`] (implementation-agnostic — see its
/// own module docs); this converts it to the plain [`std::net::UdpSocket`]
/// `rebind_socket` expects.
pub fn bind_physical_interface(interface: InterfaceIndex, local_addr: SocketAddr) -> io::Result<std::net::UdpSocket> {
    #[allow(deprecated)] // see this module's docs — the Android caveat is handled by callers never reaching here on Android
    let socket = quicsock::bind_udp(interface, local_addr)?;
    Ok(socket.into())
}

/// The OS-default unspecified bind address matching `remote`'s own address
/// family — binding a v4-unspecified socket can never dial a v6 remote (and
/// vice versa), which matters here specifically because a cellular
/// interface is very commonly IPv6-only (464XLAT/NAT64), unlike the Wi-Fi/
/// wired interfaces this crate's other dial paths were originally written
/// against.
fn unspecified_addr_for(remote: SocketAddr) -> SocketAddr {
    match remote {
        SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    }
}

/// The local ports to try binding to, in order: `addr`'s own port when no
/// range is given, otherwise every port in `port_range` (inclusive) starting
/// from a random offset so multiple sockets bound in quick succession don't
/// all race for the low end of the range. Mirrors `quicmux::noq_backend`'s
/// own (private) `candidate_ports` helper — that one drives
/// `AnyMuxFactory::create_endpoint`'s plain-socket bind, this one drives
/// [`bind_physical_interface`]'s interface-restricted bind, so it can't
/// reuse that helper directly, but the retry policy should stay identical.
fn candidate_ports(addr: SocketAddr, port_range: Option<(u16, u16)>) -> Vec<u16> {
    match port_range {
        None => vec![addr.port()],
        Some((start, end)) => {
            let span = u32::from(end) - u32::from(start) + 1;
            let offset = rand::rngs::OsRng.gen_range(0..span);
            (0..span).map(|i| start + ((offset + i) % span) as u16).collect()
        }
    }
}

/// Binds a UDP socket restricted to `interface`, trying each port
/// [`candidate_ports`] yields for `port_range` in turn via
/// [`bind_physical_interface`] until one succeeds. Split out of
/// [`connect_via_interface`] purely so this port-selection behavior itself
/// (independent of the QUIC handshake that follows it) can be unit-tested
/// directly rather than only observable end-to-end.
fn bind_physical_interface_with_port_range(
    interface: InterfaceIndex,
    unspecified: SocketAddr,
    port_range: Option<(u16, u16)>,
) -> Result<std::net::UdpSocket, TransportError> {
    let mut last_err = None;
    for port in candidate_ports(unspecified, port_range) {
        match bind_physical_interface(interface, SocketAddr::new(unspecified.ip(), port)) {
            Ok(socket) => return Ok(socket),
            Err(e) => last_err = Some(e),
        }
    }
    Err(TransportError::Mux(quicmux::MuxError::Bind {
        addr: unspecified,
        source: last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty port range")),
    }))
}

/// Creates an endpoint via `factory` — bound to `interface` specifically if
/// given (via [`bind_physical_interface`]), or to OS-default routing
/// otherwise — and dials `remote` on it. This is the "maybe-interface-bound
/// dial" step shared by [`crate::warm_standby::WarmStandby`]'s own dial and
/// [`crate::dual_path::connect_dual_path`]: both need exactly the same
/// bind-or-wrap-then-connect sequence, just for different reasons (a warm
/// spare vs. a second actively-used connection), so it lives here once
/// rather than being duplicated per caller.
///
/// The bind address always matches `remote`'s own IPv4/IPv6 family (see
/// [`unspecified_addr_for`]), and `port_range` narrows the local port the
/// same way [`quicmux::BindSpec::with_port_range`] does for this crate's
/// other dial paths (`relay.rs`/`race.rs`/`resume.rs`) — `None` binds an
/// OS-assigned ephemeral port, as before.
pub async fn connect_via_interface(
    factory: &quicmux::AnyMuxFactory,
    interface: Option<InterfaceIndex>,
    remote: quicmux::RemoteSpec,
    port_range: Option<(u16, u16)>,
) -> Result<quicmux::AnyMuxConnection, TransportError> {
    let unspecified = unspecified_addr_for(remote.addr);
    let endpoint = match interface {
        None => {
            let bind = quicmux::BindSpec { local_addr: unspecified, port_range };
            factory.create_endpoint(bind).await.map_err(TransportError::Mux)?
        }
        Some(interface) => {
            let std_socket = bind_physical_interface_with_port_range(interface, unspecified, port_range)?;
            std_socket.set_nonblocking(true).map_err(|e| TransportError::Mux(quicmux::MuxError::SocketSetup(e.to_string())))?;
            let tokio_socket =
                tokio::net::UdpSocket::from_std(std_socket).map_err(|e| TransportError::Mux(quicmux::MuxError::SocketSetup(e.to_string())))?;
            factory.wrap_bound_socket(tokio_socket).await.map_err(TransportError::Mux)?
        }
    };
    endpoint.connect(remote).await.map_err(TransportError::Mux)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::time::Duration;

    use quicmux::{AnyMuxFactory, AnyMuxListener, MuxClientConfig, MuxServerConfig};

    use super::*;

    fn loopback_index() -> InterfaceIndex {
        quicsock::discovery::list_interfaces()
            .into_iter()
            .find(|(_, iface)| iface.is_loopback())
            .map(|(index, _)| index)
            .expect("this machine should have a loopback interface")
    }

    #[test]
    fn candidate_ports_without_a_range_is_just_the_addrs_own_port() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 5555);
        assert_eq!(candidate_ports(addr, None), vec![5555]);
    }

    #[test]
    fn candidate_ports_with_a_range_covers_every_port_exactly_once() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        let mut ports = candidate_ports(addr, Some((40500, 40504)));
        ports.sort_unstable();
        assert_eq!(ports, vec![40500, 40501, 40502, 40503, 40504]);
    }

    #[test]
    fn unspecified_addr_for_matches_the_remotes_own_family() {
        assert_eq!(
            unspecified_addr_for(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 443)).ip(),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
        assert_eq!(
            unspecified_addr_for(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 443)).ip(),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        );
    }

    #[test]
    fn binds_a_socket_on_the_loopback_interface() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let socket = bind_physical_interface(loopback_index(), addr).expect("bind should succeed on loopback");
        assert_eq!(socket.local_addr().unwrap().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    // Windows-only: confirmed on a real `test-windows` CI run that
    // `setsockopt(IP_UNICAST_IF, u32::MAX)` succeeds unconditionally there —
    // Windows doesn't validate the interface index against a real adapter at
    // bind time, only (if at all) later when actually routing packets
    // through it. This matches `quicsock::windows`'s own module docs, which
    // flagged this backend as "not verified against a real Windows machine"
    // before this was observed. Unix's `SO_BINDTOIFINDEX`/`IP_BOUND_IF`
    // reject a nonexistent index immediately, which is what this test
    // actually verifies there.
    #[test]
    #[cfg(not(windows))]
    fn bogus_interface_index_fails_rather_than_panicking() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let result = bind_physical_interface(InterfaceIndex(u32::MAX), addr);
        assert!(result.is_err(), "binding to a bogus interface index should fail, got {result:?}");
    }

    const SNI: &str = "isekai-pipe.local";

    fn build_server_config() -> (MuxServerConfig, String) {
        let cert = rcgen::generate_simple_self_signed(vec![SNI.to_string()]).unwrap();
        let cert_der = cert.cert.der().clone();
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
        let cert_sha256_hex = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(cert_der.as_ref());
            hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let config = MuxServerConfig {
            alpn: isekai_protocol::hello::ALPN.to_vec(),
            exporter_label: isekai_protocol::hello::EXPORTER_LABEL.to_vec(),
            max_idle_timeout: Duration::from_secs(15),
            keep_alive_interval: Duration::from_secs(5),
            max_concurrent_bidi_streams: 4,
            max_concurrent_uni_streams: 0,
            multipath: false,
            datagram_send_buffer_size: None,
            cert_chain: vec![cert_der],
            private_key: key_der,
        };
        (config, cert_sha256_hex)
    }

    fn build_client_config() -> MuxClientConfig {
        MuxClientConfig {
            alpn: isekai_protocol::hello::ALPN.to_vec(),
            exporter_label: isekai_protocol::hello::EXPORTER_LABEL.to_vec(),
            max_idle_timeout: Duration::from_secs(15),
            keep_alive_interval: Duration::from_secs(5),
            max_concurrent_bidi_streams: 4,
            max_concurrent_uni_streams: 0,
            multipath: false,
            datagram_send_buffer_size: None,
        }
    }

    /// Spawns a listener bound to `bind_addr` (so the caller controls the
    /// address family) and accepts exactly one connection.
    async fn spawn_listener(bind_addr: SocketAddr) -> (SocketAddr, String) {
        let (server_config, cert_sha256_hex) = build_server_config();
        let listener = AnyMuxListener::bind_noq(server_config, quicmux::BindSpec { local_addr: bind_addr, port_range: None }).await.unwrap();
        let addr = SocketAddr::new(bind_addr.ip(), listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            if let Some(incoming) = listener.accept().await {
                let _ = incoming.accept().await;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        (addr, cert_sha256_hex)
    }

    /// Regression test for the IPv4-only bind this crate used to have:
    /// `connect_via_interface` must bind an address matching the remote's
    /// own family, not always IPv4 — otherwise an IPv6-only remote (e.g. a
    /// 464XLAT/NAT64 cellular interface, this crate's own motivating case
    /// for `dual_path.rs`) could never be dialed at all.
    #[tokio::test]
    async fn connect_via_interface_dials_an_ipv6_remote() {
        let (addr, cert_sha256_hex) = spawn_listener(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)).await;
        let factory = AnyMuxFactory::noq(build_client_config());
        let remote = quicmux::RemoteSpec { addr, server_name: SNI.to_string(), cert_sha256_hex };

        let conn = connect_via_interface(&factory, None, remote, None).await.expect("dialing an IPv6 remote should succeed");
        assert!(conn.remote_addr().is_some());
    }

    /// Regression test for the `port_range` this crate's other dial paths
    /// (`relay.rs`/`race.rs`/`resume.rs`) already honor but `WarmStandby::dial`
    /// silently ignored before this fix — checks the actual bound port
    /// directly (not just that a connect eventually succeeds), the same way
    /// `quicmux::noq_backend`'s own `bind_udp_socket_sync_with_a_range_picks_a_port_inside_it`
    /// test does for the non-interface-bound path.
    #[test]
    fn bind_physical_interface_with_port_range_picks_a_port_inside_it() {
        let socket = bind_physical_interface_with_port_range(
            loopback_index(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            Some((40600, 40610)),
        )
        .expect("bind should succeed on loopback");
        let port = socket.local_addr().unwrap().port();
        assert!((40600..=40610).contains(&port), "port {port} outside requested range");
    }

    #[tokio::test]
    async fn connect_via_interface_honors_port_range_when_bound_to_an_interface() {
        let (addr, cert_sha256_hex) = spawn_listener(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await;
        let factory = AnyMuxFactory::noq(build_client_config());
        let remote = quicmux::RemoteSpec { addr, server_name: SNI.to_string(), cert_sha256_hex };

        let conn = connect_via_interface(&factory, Some(loopback_index()), remote, Some((40700, 40710)))
            .await
            .expect("dialing bound to the loopback interface with a port range should succeed");
        assert!(conn.remote_addr().is_some());
    }
}
