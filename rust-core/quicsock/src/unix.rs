//! Unix backend: [`socket2::Socket::bind_device_by_index_v4`]/`_v6`, which
//! maps to `SO_BINDTOIFINDEX`/`IP_BOUND_IF` on the platforms this module is
//! compiled for (see `socket2`'s own platform gate on that method, which
//! this module's `cfg` in `lib.rs` mirrors). Verified with `cargo check`
//! (native) on Linux and `cargo check --target aarch64-apple-darwin`
//! (type-check only, no Apple SDK available in this crate's development
//! environment) on macOS.
//!
//! **Real-hardware update (macOS)**: a real `test-macos` CI run confirmed
//! `bind_to_interface` itself works correctly there (the plain bind
//! succeeds and `physical_interface.rs`'s own loopback-bind unit test
//! passes) — but layering `noq`'s QUIC connection migration
//! (PATH_CHALLENGE/PATH_RESPONSE) on top of an `IP_BOUND_IF`-restricted
//! loopback socket does not currently work on macOS: path validation times
//! out (see `isekai-transport/tests/rebind_e2e.rs`'s
//! `rebind_onto_a_quicsock_bound_interface_keeps_the_connection_usable`,
//! excluded there on macOS with a fuller writeup). Root cause unconfirmed —
//! suspected to be Darwin's stricter (vs. Linux `SO_BINDTOIFINDEX`)
//! interface-scoped route lookup under `IP_BOUND_IF` per XNU's
//! `in_pcb.c`, conflicting with how a loopback alias's route is scoped —
//! but this is not yet verified against a *real* (non-loopback) physical
//! interface, so whether this backend works for its actual intended use
//! (Wi-Fi/cellular rebind on a real Mac) remains genuinely unknown.

use std::io;
use std::num::NonZeroU32;

use socket2::Socket;

pub(crate) fn bind_to_interface(socket: &Socket, index: u32, is_v4: bool) -> io::Result<()> {
    // `index` is guaranteed non-zero here — `bind_udp` rejects `0` before
    // calling this function (see `InterfaceIndex`'s docs for why `0` can't
    // just be treated as "invalid" by `socket2` itself: it's the sentinel
    // that means "remove the interface restriction").
    let index = NonZeroU32::new(index);
    if is_v4 {
        socket.bind_device_by_index_v4(index)
    } else {
        socket.bind_device_by_index_v6(index)
    }
}
