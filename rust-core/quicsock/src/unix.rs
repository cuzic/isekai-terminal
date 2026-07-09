//! Unix backend: [`socket2::Socket::bind_device_by_index_v4`]/`_v6`, which
//! maps to `SO_BINDTOIFINDEX`/`IP_BOUND_IF` on the platforms this module is
//! compiled for (see `socket2`'s own platform gate on that method, which
//! this module's `cfg` in `lib.rs` mirrors). Verified with `cargo check`
//! (native) on Linux and `cargo check --target aarch64-apple-darwin`
//! (type-check only, no Apple SDK available in this crate's development
//! environment) on macOS.

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
