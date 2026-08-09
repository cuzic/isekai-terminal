# quicsock

Cross-platform UDP/TCP/raw IP sockets bound to a specific network interface
— for QUIC implementations (or anything else) that need to pin a path to a
particular physical or logical interface (e.g. a phone's USB/Bluetooth
tethering adapter, kept warm as a standby path alongside a primary Wi-Fi one)
instead of whatever the OS's default route happens to pick.

`quicsock` does not implement QUIC (or anything else), and does not
depend on any protocol implementation. It produces a [`socket2::Socket`],
which any consumer that accepts an externally-created socket —
[`quinn`], [`noq`], [`s2n-quic`], [`quiche`] (via [`tokio-quiche`]), or just
`std`/`tokio` TCP/UDP directly — can convert into its own type from there.

```rust,no_run
use std::net::SocketAddr;

let interface = quicsock::InterfaceIndex(12); // e.g. from `quicsock::discovery`
let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();

// UDP (e.g. for QUIC):
let udp = quicsock::bind_udp(interface, local_addr)?;

// Hand `udp` off to whatever protocol implementation you're using.
# Ok::<(), std::io::Error>(())
```

With the `discovery` feature enabled, listing interfaces (via [`netdev`])
is one call away:

```rust,no_run
for (index, iface) in quicsock::discovery::list_interfaces() {
    println!("{index:?}: {} ({:?})", iface.name, iface.if_type);
}
```

## Android

On Android specifically, this crate's `InterfaceIndex`-based interface
restriction may not actually route traffic through the requested interface —
Android's routing is UID/fwmark-based policy routing, and a downstream
project's real-hardware testing (per-interface `/proc/net/dev` counters)
found that binding a socket's source address alone had no effect on which
physical radio traffic left through. If you have an `android.net.Network`
from the Android framework (e.g. from `ConnectivityManager`), bind the fd
directly via `android.net.Network.bindSocket()` from the Kotlin/JNI side
instead of through this crate — that's what this crate's own downstream
Android app does. `bind_udp` remains available on Android for native-only
programs with no `Network` handle to work with in the first place.

## Platform coverage

| Platform | Mechanism |
|---|---|
| Linux, Android | `socket2`'s `bind_device_by_index_v4`/`_v6` (`SO_BINDTOIFINDEX`/`IP_BOUND_IF`) |
| macOS, iOS, tvOS, watchOS, visionOS | `socket2`'s `bind_device_by_index_v4`/`_v6` (`IP_BOUND_IF`/`IPV6_BOUND_IF`) |
| Windows | `IP_UNICAST_IF`/`IPV6_UNICAST_IF` via a hand-rolled `setsockopt` call — `socket2` does not wrap these on Windows |

Windows and macOS/iOS/etc. support has been verified by cross-compiling and
type-checking against the real `windows`/`socket2` crates, but **not
executed on real hardware** (this crate is developed on Linux). If you can
test it on real Windows or Apple hardware, please open an issue with what
you found — see each platform module's doc comments (`src/windows.rs`,
`src/unix.rs`) for exactly what was and wasn't checked.

## Why not just use `socket2` directly?

You can, on every platform except Windows — `socket2::Socket` already has
`bind_device_by_index_v4`/`_v6` for the Unix family. `quicsock` exists
because `socket2` has no equivalent on Windows (`IP_UNICAST_IF` isn't
wrapped there), so a caller who wants this to work across Windows, macOS,
and Linux needs to hand-roll the Windows half themselves. `quicsock` is that
missing half, plus a single API shared across every platform.

## Why not build this into a specific QUIC implementation?

Interface binding is a socket-layer concern that's identical no matter what
sits on top of it — QUIC, TCP, or anything else — and several QUIC implementations already
accept an externally-created socket ([`quinn`], the fork it's based on
[`noq`], [`s2n-quic`]) or don't own sockets at all
([`quiche`]/[`tokio-quiche`], `ngtcp2`, `lsquic`, all sans-IO). Implementations
that *do* own their own datapath (notably `msquic`) instead take a bind
address/interface hint rather than a socket object — `quicsock` can still
resolve that hint (via [`discovery`](crate::discovery)), just not hand those
implementations a ready-made socket the way it can for the others.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

[`quinn`]: https://github.com/quinn-rs/quinn
[`noq`]: https://github.com/n0-computer/noq
[`s2n-quic`]: https://github.com/aws/s2n-quic
[`quiche`]: https://github.com/cloudflare/quiche
[`tokio-quiche`]: https://crates.io/crates/tokio-quiche
[`netdev`]: https://crates.io/crates/netdev
