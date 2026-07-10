//! Android backend: [`ndk_sys::android_setsocknetwork`] (`<android/multinetwork.h>`,
//! NDK API level 23+) — the documented native mirror of
//! `android.net.Network.bindSocket()`.
//!
//! This is deliberately **not** what [`crate::bind_udp`]/[`crate::bind`] use
//! on Android — those go through [`socket2::Socket::bind_device_by_index_v4`]
//! (`SO_BINDTOIFINDEX`-style), which is a plain kernel-level interface
//! restriction. On Android that mechanism is unlikely to do what you want:
//! Android's routing is UID/fwmark-based policy routing, not
//! address/interface-based, and `Network.bindSocket()`
//! (`android_setsocknetwork()`'s Java equivalent) exists specifically
//! because a socket-level restriction alone doesn't influence it — verified
//! on real hardware in a downstream project, where a plain `bind()` to an
//! interface's local IP was confirmed (via per-interface `/proc/net/dev`
//! counters) to have no effect on which physical radio traffic left through,
//! while `Network.bindSocket()` did. `android_setsocknetwork()` is
//! documented as that same mechanism's native entry point, not a
//! from-scratch reimplementation, so it should carry the same guarantee —
//! but that has **not been independently verified on real Android hardware
//! by this crate**; only that it compiles and links correctly against the
//! real NDK (see "Verification status" below).
//!
//! # Getting a [`NetworkHandle`]
//!
//! There is no way to obtain one from native code alone — it comes from the
//! JVM side: `android.net.Network` (e.g. from
//! `ConnectivityManager.getActiveNetwork()` or a
//! `ConnectivityManager.NetworkCallback`) has a `getNetworkHandle(): Long`
//! method. That plain integer is all this module needs; unlike importing an
//! already-bound file descriptor (the alternative pattern — see
//! `quicsock-noq`'s `NamedPath::from_socket`), no `ParcelFileDescriptor`/
//! `detachFd()`/`fdsan` ownership dance is required, because the socket
//! itself is created entirely on the Rust side by [`bind_to_network`]/
//! [`bind_udp_to_network`]/[`bind_tcp_to_network`]/[`bind_raw_to_network`]
//! below — only the opaque handle crosses the JNI/UniFFI boundary.
//!
//! # Verification status
//!
//! Verified by cross-compiling against the real Android NDK (r27,
//! `aarch64-linux-android`, via the `ndk-sys` crate's generated bindings for
//! `android_setsocknetwork`/`net_handle_t`) in this development
//! environment. **Not executed on a real Android device or emulator** —
//! only compiled and linked. If you use this, verify the actual routing
//! effect on real hardware (e.g. per-interface `/proc/net/dev` counters,
//! the same technique that caught the plain-`bind()` mechanism silently not
//! working) before trusting it in production.

use std::io;
use std::net::SocketAddr;
use std::os::fd::AsRawFd;

use socket2::{Domain, Protocol, Socket, Type};

/// An `android.net.Network`'s opaque handle
/// (`Network.getNetworkHandle()`/`net_handle_t`) — see the module docs for
/// how to obtain one; this module never constructs one itself.
///
/// A handle of `0` is Android's `NETWORK_UNSPECIFIED` sentinel ("no specific
/// network") — like [`crate::InterfaceIndex`]'s `0`, [`bind_to_network`]
/// rejects it up front rather than silently producing an unrestricted
/// socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetworkHandle(pub u64);

/// Creates a socket of the given `ty`/`protocol`, restricted to `network`
/// via [`android_setsocknetwork`](ndk_sys::android_setsocknetwork), then
/// binds it to `local_addr`. The general primitive [`bind_udp_to_network`]/
/// [`bind_tcp_to_network`]/[`bind_raw_to_network`] are built from,
/// mirroring [`crate::bind`]'s shape. Named `..._to_network` throughout
/// this module (rather than reusing the root module's plain `bind`/
/// `bind_udp`/...) so it reads unambiguously at the call site as "bind to
/// this Android network", distinct from "bind to this OS interface index".
///
/// # Errors
///
/// Returns an error if the socket cannot be created, if `network` is `0`,
/// or if `android_setsocknetwork` itself fails (its documented contract:
/// returns `0` on success, `-1` with `errno` set on failure — surfaced here
/// via [`io::Error::last_os_error`]).
pub fn bind_to_network(network: NetworkHandle, local_addr: SocketAddr, ty: Type, protocol: Option<Protocol>) -> io::Result<Socket> {
    if network.0 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "quicsock: network handle 0 is Android's NETWORK_UNSPECIFIED, not a real network",
        ));
    }
    let domain = if local_addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
    let socket = Socket::new(domain, ty, protocol)?;
    apply_network_binding(&socket, network)?;
    socket.bind(&local_addr.into())?;
    Ok(socket)
}

/// [`bind_to_network`] with `Type::DGRAM`/`Protocol::UDP`.
pub fn bind_udp_to_network(network: NetworkHandle, local_addr: SocketAddr) -> io::Result<Socket> {
    bind_to_network(network, local_addr, Type::DGRAM, Some(Protocol::UDP))
}

/// [`bind_to_network`] with `Type::STREAM`/`Protocol::TCP`. Bound but
/// neither connected nor listening, same as [`crate::bind_tcp`].
pub fn bind_tcp_to_network(network: NetworkHandle, local_addr: SocketAddr) -> io::Result<Socket> {
    bind_to_network(network, local_addr, Type::STREAM, Some(Protocol::TCP))
}

/// [`bind_to_network`] with `Type::RAW` for the given IP `protocol`. Same
/// privilege caveat as [`crate::bind_raw`].
pub fn bind_raw_to_network(network: NetworkHandle, local_addr: SocketAddr, protocol: Protocol) -> io::Result<Socket> {
    bind_to_network(network, local_addr, Type::RAW, Some(protocol))
}

fn apply_network_binding(socket: &Socket, network: NetworkHandle) -> io::Result<()> {
    // SAFETY: `socket.as_raw_fd()` is a valid, open socket fd owned by
    // `socket`, which outlives this call; `android_setsocknetwork` does not
    // take ownership of it (it only sets an option on it, same as any other
    // `setsockopt`-shaped call), so no double-close/use-after-free risk.
    let ret = unsafe { ndk_sys::android_setsocknetwork(network.0, socket.as_raw_fd()) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_zero_is_rejected_before_touching_the_os() {
        let result = bind_udp_to_network(NetworkHandle(0), "127.0.0.1:0".parse().unwrap());
        assert!(result.is_err(), "binding to network handle 0 should fail, got {result:?}");
    }
}
