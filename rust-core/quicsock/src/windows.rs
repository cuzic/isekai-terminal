//! Windows backend: `IP_UNICAST_IF`/`IPV6_UNICAST_IF` via a raw `setsockopt`
//! call — `socket2` does not wrap these on Windows (only the Unix-family
//! `bind_device`/`bind_device_by_index_v4`/`_v6` exist there), so this
//! module hand-rolls the same functionality via the `windows` crate.
//!
//! **Not verified against a real Windows machine** — this development
//! environment is Linux-only. Verified so far: `cargo check --target
//! x86_64-pc-windows-gnu` type-checks and compiles cleanly against the real
//! `windows` 0.58 crate's generated `WinSock` bindings (mingw-w64
//! toolchain, no real Windows runtime available to actually execute the
//! result or observe real interface behavior). If you can test this on
//! real hardware, please report findings upstream.
//!
//! A well-known asymmetry in the underlying Win32 API (not specific to this
//! crate — see the `IP_UNICAST_IF`/`IPV6_UNICAST_IF` docs on
//! learn.microsoft.com) is that `IP_UNICAST_IF` expects the interface index
//! in **network byte order**, while `IPV6_UNICAST_IF` expects it in **host
//! byte order**. Getting this backwards silently binds to the wrong
//! interface (or an interface that happens to share the byte-swapped index)
//! rather than failing loudly, so it's worth flagging prominently here
//! rather than leaving it as an easy-to-miss one-line comment.

use std::io;
use std::os::windows::io::AsRawSocket;

use socket2::Socket;
use windows::Win32::Networking::WinSock::{
    setsockopt, WSAGetLastError, IPPROTO_IP, IPPROTO_IPV6, IP_UNICAST_IF, IPV6_UNICAST_IF, SOCKET,
};

pub(crate) fn bind_to_interface(socket: &Socket, index: u32, is_v4: bool) -> io::Result<()> {
    let raw = SOCKET(socket.as_raw_socket() as usize);
    let (level, optname, bytes): (i32, i32, [u8; 4]) = if is_v4 {
        (IPPROTO_IP.0, IP_UNICAST_IF, index.to_be_bytes())
    } else {
        (IPPROTO_IPV6.0, IPV6_UNICAST_IF, index.to_ne_bytes())
    };

    // SAFETY: `raw` wraps a `SOCKET` owned by `socket`, which outlives this
    // call (`socket` is borrowed for the whole function body); `bytes` is a
    // 4-byte buffer matching the `DWORD` these two option names expect.
    let result = unsafe { setsockopt(raw, level, optname, Some(&bytes)) };
    if result == 0 {
        Ok(())
    } else {
        // SAFETY: only queried immediately after `setsockopt` itself
        // reported failure via its return value, per `WSAGetLastError`'s
        // documented contract.
        let err = unsafe { WSAGetLastError() };
        Err(io::Error::from_raw_os_error(err.0))
    }
}
