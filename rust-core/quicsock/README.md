# quicsock

Cross-platform UDP/TCP/raw IP sockets bound to a specific network interface
— for QUIC implementations (or anything else) that need to pin a path to a
particular physical or logical interface (e.g. a phone's USB/Bluetooth
tethering adapter, kept warm as a standby path alongside a primary Wi-Fi one)
instead of whatever the OS's default route happens to pick.

Interface binding is a socket-layer concern independent of what protocol
rides on top of it, so this crate isn't limited to UDP/QUIC even though
that's the motivating use case: [`bind`] is the general primitive,
`bind_udp`/`bind_tcp`/`bind_raw` are thin wrappers over it.

`quicsock` does not implement QUIC (or TCP, or anything else), and does not
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

// TCP — bound but not yet connected/listening, same as a plain socket2 socket:
let tcp = quicsock::bind_tcp(interface, local_addr)?;

// Hand `udp`/`tcp` off to whatever protocol implementation you're using.
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

**If you're inside an Android app (i.e. you have an `android.net.Network`
from `ConnectivityManager`), use the `android` module, not the functions
above:**

```rust,no_run
use std::net::SocketAddr;

let network = quicsock::android::NetworkHandle(12345); // from Kotlin's Network.getNetworkHandle()
let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
let udp = quicsock::android::bind_udp_to_network(network, local_addr)?;
# Ok::<(), std::io::Error>(())
```

The plain `bind_udp`/`bind_tcp`/`bind_raw`/`bind` (`InterfaceIndex`-based)
are `#[deprecated]` on Android specifically, because on real hardware they
may not actually restrict which physical network traffic uses — see the
"Platform coverage" section and [`bind`]'s own docs for why. They remain
available (not removed, just discouraged) for native-only Android programs
with no `Network` handle to work with in the first place.

## Platform coverage

| Platform | Mechanism |
|---|---|
| Linux, Android | `socket2`'s `bind_device_by_index_v4`/`_v6` (`SO_BINDTOIFINDEX`/`IP_BOUND_IF`) |
| macOS, iOS, tvOS, watchOS, visionOS | `socket2`'s `bind_device_by_index_v4`/`_v6` (`IP_BOUND_IF`/`IPV6_BOUND_IF`) |
| Windows | `IP_UNICAST_IF`/`IPV6_UNICAST_IF` via a hand-rolled `setsockopt` call — `socket2` does not wrap these on Windows |
| Android, **recommended** | [`android` module](src/android.rs) (`bind_udp_to_network`/etc.): `android_setsocknetwork()` (NDK), the native mirror of `Network.bindSocket()` — see the module docs for why this exists *in addition to* the Linux mechanism above (Android's routing is UID/fwmark-based policy routing, so a plain kernel-level interface restriction has been observed on real hardware to not actually affect it) |

Windows and macOS/iOS/etc. support has been verified by cross-compiling and
type-checking against the real `windows`/`socket2` crates, but **not
executed on real hardware** (this crate is developed on Linux). If you can
test it on real Windows or Apple hardware, please open an issue with what
you found — see each platform module's doc comments (`src/windows.rs`,
`src/unix.rs`, `src/android.rs`) for exactly what was and wasn't checked.
The `android` module was verified the same way, by cross-compiling against
the real Android NDK (r27, `aarch64-linux-android`).

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
