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
//! Real interface-change backends exist for Windows
//! (`NotifyIpInterfaceChange`, `windows.rs`), macOS (`SCNetworkReachability`,
//! `macos.rs`), and Linux (`AF_NETLINK`/`NETLINK_ROUTE`, `linux.rs`). Every
//! other platform gets [`NoopNetworkChangeMonitor`] for that part — but
//! [`system_monitor`] always also merges in [`clock_skew::ClockSkewWatchdog`]
//! (`clock_skew.rs`), a platform-independent monitor for a *different* kind
//! of "the old connection is stale" event: the host was suspended/asleep and
//! just resumed. See that module's docs for why a QUIC connection's own
//! idle-timeout/keepalive can't be trusted to notice this on its own, and
//! why this needed a dedicated signal rather than reusing the interface-
//! change one.
//!
//! So even on a platform with no real interface-change backend,
//! [`system_monitor`] never degrades to *pure* idle-timeout-only reconnect
//! detection the way the paragraph above might suggest in isolation — the
//! clock-skew watchdog still fires there.

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
mod linux;

mod clock_skew;

pub use clock_skew::ClockSkewWatchdog;

use async_trait::async_trait;

/// What kind of change [`NetworkChangeEvent`] is reporting — the one piece
/// of "which change happened" a caller *does* need to branch on (unlike
/// interface name/address/reachability detail, which stays out of this
/// event on purpose, see below): whether a local rebind is even worth
/// attempting before giving up on the current connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkChangeCause {
    /// The OS reported an interface-level change (Wi-Fi dropped, default
    /// route switched, address changed, etc.) — the local socket may still
    /// be able to reach the same peer after a rebind, so it's worth trying
    /// one before falling back to a full reconnect.
    InterfaceChange,
    /// The host just resumed from a suspend/sleep
    /// ([`clock_skew::ClockSkewWatchdog`]). A rebind cannot fix this: the
    /// local socket was never the problem, the *connection* (and quite
    /// possibly the remote session) went stale during the gap, so callers
    /// must skip straight to a full reconnect instead of trying — and
    /// waiting out — a rebind first.
    Wake,
}

/// One "the current connection may be stale, an early reconnect attempt is
/// worth trying now" notification. `cause` is deliberately the only detail
/// carried — no interface name, no old/new address, no reachability flags —
/// callers besides the rebind-vs-reconnect decision this exists for should
/// still treat every cause identically. If a future caller genuinely needs
/// more detail, add fields rather than inventing a parallel, richer event
/// type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkChangeEvent {
    pub cause: NetworkChangeCause,
}

/// Yields [`NetworkChangeEvent`]s whenever the OS reports a network change.
/// Implementations must be safe to poll (`next_change`) in a loop from a
/// single task — they are not required to be `Clone`/shareable across tasks,
/// matching how `isekai-pipe connect`'s single reconnect loop uses this
/// (`&mut self`, not `&self`).
///
/// **Cancel-safe**: [`MergedMonitor`] (behind [`system_monitor`]) and
/// `isekai-pipe`'s own `wait_backoff_or_network_change` both race a
/// `next_change()` call inside `tokio::select!` against something else and
/// let the loser's future drop mid-poll. An implementation that buffers an
/// event internally *before* `next_change` observes it (rather than only
/// producing one as a direct result of being polled — e.g. `mpsc::Receiver::
/// recv`, which every current implementation in this crate relies on) would
/// silently lose that event on a dropped poll. `ClockSkewWatchdog` upholds
/// this by construction: its whole state (`last_mono`/`last_wall`) lives in
/// `&mut self` between calls, and it only ever "produces" an event as the
/// direct `return` of the call currently being polled.
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

/// The interface-change half of [`system_monitor`] on every platform
/// without a real implementation (today: everything except
/// Windows/macOS/Linux) — never yields an event on its own.
/// [`system_monitor`] still merges [`clock_skew::ClockSkewWatchdog`] on top
/// of this, so a caller never loses early-reconnect signal entirely, only
/// the interface-change half of it.
pub struct NoopNetworkChangeMonitor;

#[async_trait]
impl NetworkChangeMonitor for NoopNetworkChangeMonitor {
    async fn next_change(&mut self) -> Option<NetworkChangeEvent> {
        std::future::pending().await
    }
}

/// Races two monitors, yielding whichever reports a change first and
/// re-racing both from scratch on every call — safe because
/// [`NetworkChangeMonitor::next_change`]'s only contract is "wait for the
/// next one", so starting a fresh pair of `next_change()` futures each time
/// this itself is called is no different from a caller round-robining
/// between the two directly, just without making every caller do that
/// bookkeeping. Once one side permanently stops (`None`), this keeps racing
/// the other alone rather than stopping outright — either signal remaining
/// live is still worth having.
struct MergedMonitor {
    a: Box<dyn NetworkChangeMonitor>,
    b: Box<dyn NetworkChangeMonitor>,
    a_done: bool,
    b_done: bool,
}

#[async_trait]
impl NetworkChangeMonitor for MergedMonitor {
    async fn next_change(&mut self) -> Option<NetworkChangeEvent> {
        loop {
            if self.a_done && self.b_done {
                return None;
            }
            tokio::select! {
                ev = self.a.next_change(), if !self.a_done => match ev {
                    Some(ev) => return Some(ev),
                    None => self.a_done = true,
                },
                ev = self.b.next_change(), if !self.b_done => match ev {
                    Some(ev) => return Some(ev),
                    None => self.b_done = true,
                },
            }
        }
    }
}

/// Returns the best available monitor for the current platform: a real
/// interface-change backend on Windows/macOS/Linux if it can be set up
/// ([`NoopNetworkChangeMonitor`] otherwise — including when the real
/// backend's own OS registration call fails; this half never returns an
/// error, since "no interface-change signal" is always a safe, valid
/// fallback, never a reason to fail startup), merged with
/// [`clock_skew::ClockSkewWatchdog`] — which needs no OS registration, so
/// it's always present regardless of platform (see the crate docs above for
/// why a suspend/resume needs this separate signal at all).
pub fn system_monitor() -> Box<dyn NetworkChangeMonitor> {
    Box::new(MergedMonitor {
        a: interface_change_monitor(),
        b: Box::new(ClockSkewWatchdog::new()),
        a_done: false,
        b_done: false,
    })
}

fn interface_change_monitor() -> Box<dyn NetworkChangeMonitor> {
    #[cfg(target_os = "windows")]
    {
        match windows::WindowsNetworkChangeMonitor::new() {
            Ok(monitor) => return Box::new(monitor),
            Err(e) => {
                log::warn!("isekai-netmon: failed to register for Windows network-change notifications, falling back to clock-skew-only reconnect detection: {e}");
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        match macos::MacosNetworkChangeMonitor::new() {
            Ok(monitor) => return Box::new(monitor),
            Err(e) => {
                log::warn!("isekai-netmon: failed to register for macOS network-change notifications, falling back to clock-skew-only reconnect detection: {e}");
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        match linux::LinuxNetworkChangeMonitor::new() {
            Ok(monitor) => return Box::new(monitor),
            Err(e) => {
                log::warn!("isekai-netmon: failed to register for Linux network-change notifications, falling back to clock-skew-only reconnect detection: {e}");
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

    /// Yields `event` exactly once, then reports permanently stopped —
    /// mirrors a real backend that fires once and never again within one
    /// `next_change` call's `None` = "stopped" contract.
    struct FireOnceMonitor {
        event: Option<NetworkChangeEvent>,
    }

    #[async_trait]
    impl NetworkChangeMonitor for FireOnceMonitor {
        async fn next_change(&mut self) -> Option<NetworkChangeEvent> {
            self.event.take()
        }
    }

    #[tokio::test]
    async fn merged_monitor_forwards_whichever_side_fires_first() {
        let mut merged = MergedMonitor {
            a: Box::new(FireOnceMonitor { event: Some(NetworkChangeEvent { cause: NetworkChangeCause::InterfaceChange }) }),
            b: Box::new(NoopNetworkChangeMonitor),
            a_done: false,
            b_done: false,
        };
        let ev = tokio::time::timeout(std::time::Duration::from_millis(200), merged.next_change())
            .await
            .expect("must not hang waiting on the live side")
            .expect("must yield Some, not a permanent stop");
        assert_eq!(ev.cause, NetworkChangeCause::InterfaceChange);
    }

    #[tokio::test]
    async fn merged_monitor_still_yields_from_the_live_side_after_the_other_permanently_stops() {
        // `a` reports permanently stopped on its very first call — the
        // merge must not mistake that for "both sides are done" and must
        // keep racing `b` alone until it, too, fires.
        let mut merged = MergedMonitor {
            a: Box::new(FireOnceMonitor { event: None }),
            b: Box::new(FireOnceMonitor { event: Some(NetworkChangeEvent { cause: NetworkChangeCause::Wake }) }),
            a_done: false,
            b_done: false,
        };
        let ev = tokio::time::timeout(std::time::Duration::from_millis(200), merged.next_change())
            .await
            .expect("must not hang once the still-live side has an event queued")
            .expect("must yield Some, not a permanent stop");
        assert_eq!(ev.cause, NetworkChangeCause::Wake);
    }

    #[tokio::test]
    async fn merged_monitor_reports_permanently_stopped_once_both_sides_are() {
        let mut merged = MergedMonitor { a: Box::new(FireOnceMonitor { event: None }), b: Box::new(FireOnceMonitor { event: None }), a_done: false, b_done: false };
        let result = tokio::time::timeout(std::time::Duration::from_millis(200), merged.next_change())
            .await
            .expect("must not hang once both sides are exhausted");
        assert!(result.is_none());
    }
}
