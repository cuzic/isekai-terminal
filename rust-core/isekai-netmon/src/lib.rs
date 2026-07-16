//! OS-level network-change notification, so a caller (`isekai-pipe connect`'s
//! reconnect loop) can attempt an early reconnect instead of waiting for a
//! QUIC idle timeout to fire when a PC's active network interface changes
//! (e.g. Wi-Fi disconnects, or the OS switches its default route).
//!
//! This is deliberately *not* the same feature as `isekai-terminal-core`'s
//! Android-side multipath (`multipath_transport.rs`): that mechanism
//! proactively validates a *new* path in parallel with the old one before
//! switching, using `noq`'s multipath support and Android's
//! `ConnectivityManager` callbacks forwarded over UniFFI, so a network
//! switch can be closed with no visible interruption at all. This crate only
//! shortens the *reactive* resume path `isekai-transport::resume` already
//! has (`RESUME`/`RESUME_ACK`, replay buffer, backoff) — there is still a
//! visible reconnect, just a faster one than blindly waiting out the idle
//! timeout. A PC typically has exactly one active network at a time (unlike
//! a phone that might have both Wi-Fi and cellular simultaneously), so full
//! multipath racing isn't the goal here.
//!
//! Real backends exist for Windows (`NotifyIpInterfaceChange`, `windows.rs`),
//! macOS (`SCNetworkReachability`, `macos.rs`), and Linux
//! (`AF_NETLINK`/`NETLINK_ROUTE`, `linux.rs`). Every other platform gets
//! [`NoopNetworkChangeMonitor`], which never fires — callers relying on
//! [`system_monitor`] see exactly today's idle-timeout-only reconnect
//! behavior on those platforms, unchanged.

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
mod linux;

use async_trait::async_trait;

/// One "the network changed" notification. Deliberately content-free (no
/// interface name, no old/new address, no reachability flags) — callers only
/// need "something changed, an early reconnect attempt is worth trying now",
/// never a reason to branch on *which* change happened. If a future caller
/// genuinely needs that detail, add fields here rather than inventing a
/// parallel, richer event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkChangeEvent;

/// Yields [`NetworkChangeEvent`]s whenever the OS reports a network change.
/// Implementations must be safe to poll (`next_change`) in a loop from a
/// single task — they are not required to be `Clone`/shareable across tasks,
/// matching how `isekai-pipe connect`'s single reconnect loop uses this
/// (`&mut self`, not `&self`).
#[async_trait]
pub trait NetworkChangeMonitor: Send {
    /// Waits for the next network-change event. Returns `None` if the
    /// monitor has permanently stopped delivering events (e.g. the
    /// underlying OS registration failed, or its background task/thread
    /// exited) — callers should treat this as "no more early-reconnect
    /// signals available from here on, fall back to whatever detection
    /// already existed before this monitor" rather than an error to
    /// propagate: `#20b`'s existing idle-timeout-based reconnect loop still
    /// works correctly without this signal, just not as promptly.
    async fn next_change(&mut self) -> Option<NetworkChangeEvent>;
}

/// The monitor used on every platform without a real implementation (today:
/// everything except Windows/macOS, including Linux) — never yields an
/// event, so a caller using [`system_monitor`] sees exactly the reconnect
/// behavior it had before this crate existed.
pub struct NoopNetworkChangeMonitor;

#[async_trait]
impl NetworkChangeMonitor for NoopNetworkChangeMonitor {
    async fn next_change(&mut self) -> Option<NetworkChangeEvent> {
        std::future::pending().await
    }
}

/// Returns the best available monitor for the current platform: a real
/// OS-backed one on Windows/macOS/Linux if it can be set up,
/// [`NoopNetworkChangeMonitor`] otherwise (including when the real
/// backend's own OS registration call fails — this never returns an error,
/// since "no early-reconnect signal" is always a safe, valid fallback,
/// never a reason to fail startup).
pub fn system_monitor() -> Box<dyn NetworkChangeMonitor> {
    #[cfg(target_os = "windows")]
    {
        match windows::WindowsNetworkChangeMonitor::new() {
            Ok(monitor) => return Box::new(monitor),
            Err(e) => {
                log::warn!("isekai-netmon: failed to register for Windows network-change notifications, falling back to idle-timeout-only reconnect detection: {e}");
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        match macos::MacosNetworkChangeMonitor::new() {
            Ok(monitor) => return Box::new(monitor),
            Err(e) => {
                log::warn!("isekai-netmon: failed to register for macOS network-change notifications, falling back to idle-timeout-only reconnect detection: {e}");
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        match linux::LinuxNetworkChangeMonitor::new() {
            Ok(monitor) => return Box::new(monitor),
            Err(e) => {
                log::warn!("isekai-netmon: failed to register for Linux network-change notifications, falling back to idle-timeout-only reconnect detection: {e}");
            }
        }
    }
    Box::new(NoopNetworkChangeMonitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_monitor_never_resolves() {
        let mut monitor = NoopNetworkChangeMonitor;
        tokio::select! {
            _ = monitor.next_change() => panic!("NoopNetworkChangeMonitor must never yield an event"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }

    #[test]
    fn system_monitor_never_panics_to_construct() {
        // Whatever platform this test happens to run on, constructing the
        // system monitor must never panic — worst case it silently falls
        // back to the no-op one (module docs).
        let _monitor = system_monitor();
    }
}
