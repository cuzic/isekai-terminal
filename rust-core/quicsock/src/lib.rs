//! `quicsock` binds a socket to a specific network interface, so a caller can
//! say "send/receive this over *this* interface" (e.g. a phone's USB/
//! Bluetooth tethering adapter, kept warm as a standby path next to the
//! primary Wi-Fi one) rather than whatever the OS's default route happens to
//! pick — for UDP (most commonly QUIC) sockets.
//!
//! This crate is deliberately implementation-agnostic for whatever sits on
//! top of the socket: it only produces a [`socket2::Socket`], which any
//! consumer that accepts an externally-created socket (`quinn`, `noq`,
//! `s2n-quic`, `quiche` via `tokio-quiche`, or just `std`/`tokio` TCP/UDP
//! directly) can convert into its own type from there. It does not depend on
//! any of them.
//!
//! # Platform coverage
//!
//! | Platform | Mechanism |
//! |---|---|
//! | Linux, Android | [`socket2::Socket::bind_device_by_index_v4`]/`_v6` (`SO_BINDTOIFINDEX`/`IP_BOUND_IF` where supported by the kernel, falling back per socket2's own platform handling) |
//! | macOS, iOS, tvOS, watchOS, visionOS | [`socket2::Socket::bind_device_by_index_v4`]/`_v6` (`IP_BOUND_IF`/`IPV6_BOUND_IF`) |
//! | Windows | `IP_UNICAST_IF`/`IPV6_UNICAST_IF` (`setsockopt`, hand-rolled — `socket2` does not wrap these on Windows) |
//!
//! Windows and macOS/iOS support in this crate has been verified by
//! cross-compiling and type-checking against the real `windows`/`socket2`
//! crates (see each platform module's doc comment for exactly what was and
//! wasn't checked) but **not executed on real hardware** — this crate was
//! developed on Linux. Please report any real-hardware findings upstream.
//!
//! On Android specifically, note that this crate's interface-index mechanism
//! is unlikely to influence routing in ordinary Android app/framework
//! environments (Android's routing is UID/fwmark-based policy routing, not
//! address/interface-based) — see [`bind_udp`]'s "Android" section. This
//! project's own Android app does not go through this crate for physical
//! interface selection at all; it binds the fd directly via
//! `android.net.Network.bindSocket()` on the Kotlin/JNI side instead.
//!
//! # Interface identification
//!
//! The core API ([`bind_udp`]) takes a raw OS interface index
//! ([`InterfaceIndex`]) rather than a name or an enumeration-crate-specific
//! type, so this crate stays usable regardless of which (if any) interface
//! enumeration crate the caller prefers. Enable the `discovery` feature for
//! an optional convenience layer on top of the `netdev` crate.

use std::io;
use std::net::SocketAddr;

use socket2::{Domain, Protocol, Socket, Type};

#[cfg(windows)]
mod windows;

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos",
))]
mod unix;

#[cfg(feature = "discovery")]
pub mod discovery;

/// An OS-assigned network interface index (what `if_nametoindex(3)` returns
/// on Unix, or the adapter's `IfIndex` on Windows).
///
/// On Android specifically, see [`bind_udp`]'s "Android" section before
/// using this.
///
/// An index of `0` is never a real interface (Unix `ifindex`es and Windows
/// `IfIndex`es both start at `1`) and is special-cased by the underlying OS
/// APIs to mean "no restriction" rather than "invalid" — so [`bind_udp`]
/// rejects it up front with a clear error instead of silently producing an
/// unrestricted socket when the caller most likely meant to name a specific
/// interface. Any other index that doesn't currently name a live interface
/// simply makes the underlying `setsockopt` call fail, which surfaces as an
/// [`io::Error`] from [`bind_udp`] the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InterfaceIndex(pub u32);

impl From<u32> for InterfaceIndex {
    fn from(index: u32) -> Self {
        InterfaceIndex(index)
    }
}

/// Creates a UDP socket restricted to sending and receiving only via
/// `interface`, then binds it to `local_addr` — the common case for QUIC and
/// other UDP-based protocols, and the only socket type/protocol this crate
/// currently exposes a convenience constructor for.
///
/// The returned [`socket2::Socket`] is not set non-blocking or connected —
/// callers should do whatever of that they need themselves, exactly as they
/// would for a socket they created directly with `socket2`.
///
/// # Errors
///
/// Returns an error if the socket cannot be created, if `local_addr`'s
/// address family doesn't match an interface the OS will accept for it (e.g.
/// binding an IPv6 address to an interface that only has IPv4 addresses),
/// or — on Unix — if `interface` does not name a currently-live interface.
///
/// **Windows caveat** (confirmed on a real `test-windows` CI run, not just
/// documentation): `IP_UNICAST_IF`/`IPV6_UNICAST_IF`'s underlying
/// `setsockopt` call does not validate the interface index against a live
/// adapter — passing an index that names no real interface still returns
/// success here. The restriction is only (if at all) enforced later, when
/// the OS actually routes packets through it. Callers on Windows that need
/// to fail fast on a bogus index should validate it against
/// [`discovery::list_interfaces`] themselves first.
///
/// # Android
///
/// This function uses the interface-index mechanism
/// (`SO_BINDTOIFINDEX`/`IP_BOUND_IF`-style). In ordinary Android app/
/// framework environments this may not route traffic through the requested
/// interface at all — Android's routing is UID/fwmark-based policy routing,
/// and a downstream project's real-hardware testing (per-interface
/// `/proc/net/dev` counters) found that binding a socket's source address
/// alone had no effect on which physical radio traffic left through. An
/// `android.net.Network`-based alternative (`Network.bindSocket()`'s native
/// mirror, `android_setsocknetwork()`) was previously implemented in this
/// crate but removed as dead code — this project's own Android app performs
/// that binding directly from the Kotlin/JNI side instead, never through
/// this crate. This function remains available on Android for native-only
/// programs with no Android framework `Network` handle to work with (there
/// is no other way to select an interface in that situation).
pub fn bind_udp(interface: InterfaceIndex, local_addr: SocketAddr) -> io::Result<Socket> {
    if interface.0 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "quicsock: interface index 0 is never a real interface (see InterfaceIndex docs)",
        ));
    }
    let domain = if local_addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    bind_to_interface(&socket, interface, local_addr.is_ipv4())?;
    socket.bind(&local_addr.into())?;
    Ok(socket)
}

fn bind_to_interface(socket: &Socket, interface: InterfaceIndex, is_v4: bool) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows::bind_to_interface(socket, interface.0, is_v4)
    }
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
    ))]
    {
        unix::bind_to_interface(socket, interface.0, is_v4)
    }
    #[cfg(not(any(
        windows,
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
    )))]
    {
        let _ = (socket, interface, is_v4);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "quicsock: binding a socket to a specific interface is not implemented on this platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_zero_is_rejected_before_touching_the_os() {
        let result = bind_udp(InterfaceIndex(0), "127.0.0.1:0".parse().unwrap());
        assert!(result.is_err(), "binding to interface index 0 should fail, got {result:?}");
    }

    // Windows-only exclusion: confirmed on a real `test-windows` CI run that
    // `setsockopt(IP_UNICAST_IF/IPV6_UNICAST_IF, ...)` succeeds unconditionally
    // there regardless of whether the index names a live interface — see
    // `bind_udp`'s doc comment's "Windows caveat" above. Unix's
    // `SO_BINDTOIFINDEX`/`IP_BOUND_IF` reject a nonexistent index immediately,
    // which is what this test actually verifies there.
    #[test]
    #[cfg(not(windows))]
    fn nonexistent_interface_index_fails_rather_than_panicking() {
        // No portable way to name a real interface here (this crate must
        // build and test on any machine, real interfaces vary), so this
        // just exercises the platform-level error path with an index
        // essentially guaranteed not to name a live interface.
        let result = bind_udp(InterfaceIndex(u32::MAX), "127.0.0.1:0".parse().unwrap());
        assert!(result.is_err(), "binding to a bogus interface index should fail, got {result:?}");
    }
}
