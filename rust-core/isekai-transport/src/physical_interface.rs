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
use std::net::SocketAddr;

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

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    fn loopback_index() -> InterfaceIndex {
        quicsock::discovery::list_interfaces()
            .into_iter()
            .find(|(_, iface)| iface.is_loopback())
            .map(|(index, _)| index)
            .expect("this machine should have a loopback interface")
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
}
