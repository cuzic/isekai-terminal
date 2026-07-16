//! Linux backend: a raw `AF_NETLINK`/`NETLINK_ROUTE` socket subscribed to
//! the link/address/route multicast groups (`RTMGRP_LINK` |
//! `RTMGRP_IPV4_IFADDR` | `RTMGRP_IPV6_IFADDR` | `RTMGRP_IPV4_ROUTE` |
//! `RTMGRP_IPV6_ROUTE`), so an interface up/down, address change, or
//! default-route change all wake this monitor — the same breadth as
//! Windows' `NotifyIpInterfaceChange(AF_UNSPEC, ...)` and macOS's "any
//! route" `SCNetworkReachability` (crate module docs). This backend never
//! parses the netlink message payload (`NetworkChangeEvent` is deliberately
//! content-free) — any successful read on this socket already means "the
//! kernel reported a change in one of the subscribed groups", which is all
//! a caller needs. No root/`CAP_NET_ADMIN` is required to *read* these
//! multicast groups (same as e.g. `ip monitor`), only to *modify* routing
//! state.
//!
//! No async-friendly netlink API exists without extra dependencies
//! (`tokio::net::UdpSocket` doesn't support `AF_NETLINK`), so — same shape
//! as `windows.rs`/`macos.rs` — this owns a dedicated background thread
//! doing blocking reads, forwarding each one as a `NetworkChangeEvent` over
//! a channel. The socket's `SO_RCVTIMEO` is set to `STOP_POLL_INTERVAL`
//! purely so the thread can periodically notice `Drop`'s stop flag and
//! exit instead of blocking forever — timeouts (and any other transient
//! `recv` error) are silently retried, never treated as fatal.
//!
//! Defines its own minimal `sockaddr_nl` shape (`SockaddrNl` below) rather
//! than using `libc::sockaddr_nl` directly: that struct's padding field is
//! private in recent `libc` versions, so it isn't constructible from
//! outside the crate via a struct literal. The netlink `sockaddr_nl` ABI
//! (`family`/`pad`/`pid`/`groups`, 12 bytes) is a stable kernel UAPI shape
//! that hasn't changed since netlink's introduction, so hand-rolling it
//! here is safe and avoids depending on `libc`'s internal representation.
//!
//! Verified directly in this development environment (unlike
//! `windows.rs`/`macos.rs`, which could only be verified to type-check on a
//! cross-compile target): socket creation, binding to the multicast
//! groups, and clean background-thread shutdown all run for real here
//! (`tests::` below). Not verified: an actual netlink notification
//! arriving in response to a real interface/address/route change, since
//! triggering one deterministically would need root and a disposable
//! network interface this sandboxed environment doesn't have.

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::{NetworkChangeEvent, NetworkChangeMonitor};

/// `<linux/netlink.h>`'s `NETLINK_ROUTE` — an ABI-stable kernel UAPI
/// constant (always `0`), hardcoded here rather than pulled from `libc`
/// for the same reason `SockaddrNl` below is hand-rolled: keep this
/// backend's netlink surface self-contained instead of depending on
/// exactly which of `libc`'s internal netlink representations a given
/// version exposes.
const NETLINK_ROUTE: libc::c_int = 0;

/// How often the background thread's blocking `recv` times out to check
/// whether `Drop` has asked it to stop. Also the upper bound on how long
/// `Drop` (and thus dropping this monitor) can block joining that thread.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Minimal, hand-rolled `struct sockaddr_nl` (see module docs for why not
/// `libc::sockaddr_nl`). Field layout must match the kernel's exactly:
/// `sa_family_t nl_family; unsigned short nl_pad; __u32 nl_pid; __u32 nl_groups;`.
#[repr(C)]
struct SockaddrNl {
    nl_family: libc::sa_family_t,
    nl_pad: u16,
    nl_pid: u32,
    nl_groups: u32,
}

pub struct LinuxNetworkChangeMonitor {
    receiver: mpsc::UnboundedReceiver<NetworkChangeEvent>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl LinuxNetworkChangeMonitor {
    pub fn new() -> Result<Self, String> {
        // SAFETY: a plain `socket(2)` call with well-known constant
        // arguments; the returned fd is checked for `< 0` (error) before
        // any further use.
        let fd: RawFd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, NETLINK_ROUTE) };
        if fd < 0 {
            return Err(format!("socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE) failed: {}", std::io::Error::last_os_error()));
        }

        let groups = (libc::RTMGRP_LINK
            | libc::RTMGRP_IPV4_IFADDR
            | libc::RTMGRP_IPV6_IFADDR
            | libc::RTMGRP_IPV4_ROUTE
            | libc::RTMGRP_IPV6_ROUTE) as u32;
        let addr =
            SockaddrNl { nl_family: libc::AF_NETLINK as libc::sa_family_t, nl_pad: 0, nl_pid: 0, nl_groups: groups };
        // SAFETY: `fd` was just created above and isn't used anywhere
        // else yet; `addr` is a fully-initialized `SockaddrNl`, and
        // `size_of::<SockaddrNl>()` describes that exact same local type,
        // not `libc::sockaddr_nl` — both sides of this call agree.
        let bind_result = unsafe {
            libc::bind(fd, &addr as *const SockaddrNl as *const libc::sockaddr, std::mem::size_of::<SockaddrNl>() as libc::socklen_t)
        };
        if bind_result < 0 {
            let e = std::io::Error::last_os_error();
            // SAFETY: `fd` is still owned solely by this function at this
            // point (never handed to the background thread on this
            // error path).
            unsafe { libc::close(fd) };
            return Err(format!("bind(AF_NETLINK) failed: {e}"));
        }

        let timeout = libc::timeval { tv_sec: 0, tv_usec: STOP_POLL_INTERVAL.as_micros() as libc::suseconds_t };
        // SAFETY: `fd` is a valid, just-bound socket; `timeout` is a
        // fully-initialized `timeval`. A failure here is non-fatal (the
        // background thread would just block longer between stop-flag
        // checks), so it's intentionally not propagated as an error.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &timeout as *const libc::timeval as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let spawn_result = std::thread::Builder::new().name("isekai-netmon-linux".to_string()).spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                if worker_stop.load(Ordering::Relaxed) {
                    break;
                }
                // SAFETY: `fd` is valid for this thread's entire loop
                // (owned solely by this closure from here on); `buf` is
                // large enough to drain a netlink datagram — its contents
                // are never read, this crate only cares that *a* message
                // arrived.
                let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
                if n > 0 {
                    // An unbounded send only fails once the receiver
                    // (this monitor) has been dropped, at which point
                    // `Drop` has already set `stop` — this loop exits on
                    // its own next iteration either way.
                    let _ = tx.send(NetworkChangeEvent);
                }
                // `n <= 0` covers both the expected `SO_RCVTIMEO` timeout
                // (EAGAIN/EWOULDBLOCK, fired every `STOP_POLL_INTERVAL` —
                // just loop back to the stop check) and any real `recv`
                // error: this background thread has no way to surface an
                // error to the caller after `new()` already returned
                // `Ok`, so both cases just retry.
            }
            // SAFETY: this thread has been the sole owner of `fd` since
            // `new()` handed it off, and this is its last use.
            unsafe { libc::close(fd) };
        });
        let worker = match spawn_result {
            Ok(handle) => handle,
            Err(e) => {
                // The closure (and the `fd` copy moved into it) never ran
                // and never reached its own `close`, so `fd` is still
                // this function's responsibility.
                // SAFETY: same as the `bind` failure path above — `fd`
                // has not been handed to any other owner.
                unsafe { libc::close(fd) };
                return Err(format!("failed to spawn the netlink-monitor thread: {e}"));
            }
        };

        Ok(Self { receiver: rx, stop, worker: Some(worker) })
    }
}

#[async_trait]
impl NetworkChangeMonitor for LinuxNetworkChangeMonitor {
    async fn next_change(&mut self) -> Option<NetworkChangeEvent> {
        self.receiver.recv().await
    }
}

impl Drop for LinuxNetworkChangeMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_succeeds_and_drop_completes_promptly_without_a_real_network_event() {
        let started = std::time::Instant::now();
        let monitor = LinuxNetworkChangeMonitor::new().expect(
            "binding an AF_NETLINK/NETLINK_ROUTE socket to read-only multicast groups \
             must not require root (same as `ip monitor`)",
        );
        drop(monitor);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "Drop must join the background thread within roughly one \
             STOP_POLL_INTERVAL, not hang indefinitely: took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn next_change_does_not_resolve_spuriously_within_a_short_window() {
        let mut monitor = LinuxNetworkChangeMonitor::new().expect("socket setup must succeed in this sandbox");
        tokio::select! {
            _ = monitor.next_change() => panic!(
                "no real network change happened during this test; next_change() must not resolve on its own"
            ),
            _ = tokio::time::sleep(Duration::from_millis(300)) => {}
        }
    }
}
