//! Platform-independent "did anything that could affect our connection
//! actually change?" filtering, shared by the Linux and Windows backends.
//!
//! Both OS notification APIs this crate listens to are far broader than what
//! matters to an established connection: Linux's netlink route groups fire on
//! every docker/veth creation, VPN route push, and IPv6 address-lifetime
//! refresh (every router advertisement), and Windows'
//! `NotifyIpInterfaceChange(AF_UNSPEC)` fires on every interface parameter
//! change of every adapter. Each of those used to become an
//! `InterfaceChange`, and `isekai-pipe connect`'s reconnect loop treats that
//! as "the connection is stale, RESUME now" — i.e. a spurious reconnect.
//!
//! So the backends no longer forward notifications directly. A notification
//! only *pokes* [`run_debounced_worker`], which waits until the notifications
//! have been quiet for [`DEBOUNCE`] (capped by [`MAX_DEBOUNCE`] so a steady
//! stream can't starve it), takes a fresh [`NetworkSnapshot`] of the state
//! that can actually affect reachability, and emits one event only if
//! [`is_relevant_change`] says it differs from the previous snapshot. If a
//! snapshot can't be taken, the worker errs on the side of emitting (the
//! pre-filter behavior), since a missed real change costs a full idle
//! timeout while a spurious one only costs a fast RESUME.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long notifications must be quiet before a snapshot is taken — a
/// single real change (Wi-Fi reassociation, DHCP renewal) arrives as a burst
/// of several netlink/IP Helper notifications within a few milliseconds.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(300);

/// Upper bound on how long a continuous stream of notifications can delay
/// the snapshot.
pub(crate) const MAX_DEBOUNCE: Duration = Duration::from_secs(2);

/// How long one [`run_debounced_worker`] wait may block — also how quickly
/// the worker notices its stop flag.
pub(crate) const TICK: Duration = Duration::from_millis(100);

/// The reachability-relevant slice of the host's network state. Interface
/// identity is the OS's interface index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NetworkSnapshot {
    /// Non-loopback, non-link-local unicast addresses, per interface.
    pub addresses: BTreeSet<(u32, IpAddr)>,
    /// Default routes in the main routing table: (outgoing interface,
    /// gateway if any).
    pub default_routes: BTreeSet<(u32, Option<IpAddr>)>,
    /// Non-loopback interfaces that are operationally up/connected (Linux
    /// `IFF_RUNNING`, Windows `Connected`).
    pub running: BTreeSet<u32>,
}

impl NetworkSnapshot {
    fn default_route_interfaces(&self) -> BTreeSet<u32> {
        self.default_routes.iter().map(|(ifindex, _)| *ifindex).collect()
    }
}

/// Whether an address should be tracked at all: loopback and link-local
/// addresses never carry traffic to a remote server.
pub(crate) fn is_trackable_address(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
        IpAddr::V6(v6) => {
            !v6.is_loopback() && !v6.is_unspecified() && (v6.segments()[0] & 0xffc0) != 0xfe80
        }
    }
}

/// The filter itself. Only interfaces that carry a default route — before
/// *or* after — matter: that's the path traffic to an arbitrary remote
/// server takes. A change is relevant if
/// - the set of default routes changed (Wi-Fi ↔ tethering switch, VPN full
///   tunnel up/down, route lost), or
/// - an address appeared/disappeared on a default-route interface (DHCP gave
///   a new address, IPv6 renumbering) — a lifetime-only refresh of an
///   existing address is not a change in the set, so it's ignored, or
/// - a default-route interface lost/regained its carrier (Wi-Fi
///   reassociation keeps the IPv4 route but the path did break).
///
/// Interfaces without a default route (docker0/veth, split-tunnel VPN,
/// virtual adapters) never trigger a reconnect.
pub(crate) fn is_relevant_change(old: &NetworkSnapshot, new: &NetworkSnapshot) -> bool {
    if old.default_routes != new.default_routes {
        return true;
    }
    let ifaces: BTreeSet<u32> = old.default_route_interfaces().union(&new.default_route_interfaces()).copied().collect();
    let addrs = |s: &NetworkSnapshot| -> BTreeSet<(u32, IpAddr)> {
        s.addresses.iter().filter(|(ifindex, _)| ifaces.contains(ifindex)).copied().collect()
    };
    if addrs(old) != addrs(new) {
        return true;
    }
    let running = |s: &NetworkSnapshot| -> BTreeSet<u32> { s.running.intersection(&ifaces).copied().collect() };
    running(old) != running(new)
}

/// One blocking wait's outcome, for [`run_debounced_worker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tick {
    /// The OS reported (some) change.
    Poke,
    /// Nothing arrived within [`TICK`].
    Idle,
    /// The notification source is gone; stop.
    Closed,
}

/// The shared worker loop (see module docs). `wait_tick` must block for at
/// most about [`TICK`]; `emit` returns `false` once nobody is listening any
/// more.
pub(crate) fn run_debounced_worker(
    stop: &AtomicBool,
    mut wait_tick: impl FnMut() -> Tick,
    mut snapshot: impl FnMut() -> Result<NetworkSnapshot, String>,
    mut emit: impl FnMut() -> bool,
) {
    let mut last = match snapshot() {
        Ok(s) => Some(s),
        Err(e) => {
            log::debug!("isekai-netmon: initial network snapshot failed ({e}); the first change will be reported unfiltered");
            None
        }
    };
    let mut pending_since: Option<Instant> = None;
    let mut last_poke = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match wait_tick() {
            Tick::Poke => {
                let now = Instant::now();
                pending_since.get_or_insert(now);
                last_poke = now;
            }
            Tick::Idle => {}
            Tick::Closed => return,
        }
        let Some(since) = pending_since else { continue };
        let now = Instant::now();
        if now.duration_since(last_poke) < DEBOUNCE && now.duration_since(since) < MAX_DEBOUNCE {
            continue;
        }
        pending_since = None;
        let relevant = match snapshot() {
            Ok(new) => {
                let relevant = last.as_ref().map_or(true, |old| is_relevant_change(old, &new));
                if !relevant {
                    log::debug!("isekai-netmon: network notification without a reachability-relevant change, ignoring");
                }
                last = Some(new);
                relevant
            }
            Err(e) => {
                log::debug!("isekai-netmon: network snapshot failed ({e}); reporting the change unfiltered");
                last = None;
                true
            }
        };
        if relevant && !emit() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn wifi() -> NetworkSnapshot {
        let mut s = NetworkSnapshot::default();
        s.addresses.insert((3, ip("192.168.1.20")));
        s.addresses.insert((3, ip("2001:db8::20")));
        s.default_routes.insert((3, Some(ip("192.168.1.1"))));
        s.running.insert(3);
        s
    }

    #[test]
    fn unrelated_interfaces_do_not_count() {
        let old = wifi();
        let mut new = wifi();
        // docker0 comes up with an address, a veth appears and starts running.
        new.addresses.insert((7, ip("172.17.0.1")));
        new.running.insert(7);
        new.running.insert(8);
        assert!(!is_relevant_change(&old, &new));
    }

    #[test]
    fn identical_snapshot_is_not_a_change() {
        // e.g. an RA only refreshing the lifetime of an existing address.
        assert!(!is_relevant_change(&wifi(), &wifi()));
    }

    #[test]
    fn default_route_change_counts() {
        let old = wifi();
        let mut new = wifi();
        new.default_routes.clear();
        new.default_routes.insert((5, None)); // switched to a tethering adapter
        assert!(is_relevant_change(&old, &new));
    }

    #[test]
    fn address_change_on_default_route_interface_counts() {
        let old = wifi();
        let mut new = wifi();
        new.addresses.remove(&(3, ip("192.168.1.20")));
        new.addresses.insert((3, ip("192.168.1.21")));
        assert!(is_relevant_change(&old, &new));
    }

    #[test]
    fn carrier_loss_on_default_route_interface_counts() {
        let old = wifi();
        let mut new = wifi();
        new.running.remove(&3);
        assert!(is_relevant_change(&old, &new));
    }

    #[test]
    fn link_local_and_loopback_are_not_trackable() {
        assert!(!is_trackable_address(&ip("127.0.0.1")));
        assert!(!is_trackable_address(&ip("169.254.3.4")));
        assert!(!is_trackable_address(&ip("::1")));
        assert!(!is_trackable_address(&ip("fe80::1")));
        assert!(is_trackable_address(&ip("192.168.1.20")));
        assert!(is_trackable_address(&ip("2001:db8::1")));
    }

    /// Drives [`run_debounced_worker`] with a scripted sequence of ticks
    /// (each "tick" is instantaneous here, time is advanced with real sleeps
    /// kept short) and scripted snapshots, returning how many events were
    /// emitted.
    fn drive(ticks: Vec<(Tick, Duration)>, snapshots: Vec<Result<NetworkSnapshot, String>>) -> usize {
        let stop = AtomicBool::new(false);
        let ticks = RefCell::new(VecDeque::from(ticks));
        let snapshots = RefCell::new(VecDeque::from(snapshots));
        let emitted = Cell::new(0usize);
        run_debounced_worker(
            &stop,
            || match ticks.borrow_mut().pop_front() {
                Some((tick, sleep)) => {
                    std::thread::sleep(sleep);
                    tick
                }
                None => Tick::Closed,
            },
            || snapshots.borrow_mut().pop_front().unwrap_or_else(|| Ok(wifi())),
            || {
                emitted.set(emitted.get() + 1);
                true
            },
        );
        emitted.get()
    }

    #[test]
    fn a_burst_of_pokes_is_coalesced_and_an_irrelevant_one_is_dropped() {
        let quiet = DEBOUNCE + Duration::from_millis(50);
        let n = drive(
            vec![(Tick::Poke, Duration::ZERO), (Tick::Poke, Duration::ZERO), (Tick::Poke, Duration::ZERO), (Tick::Idle, quiet)],
            vec![Ok(wifi()), Ok(wifi())],
        );
        assert_eq!(n, 0, "no relevant change → no event");
    }

    #[test]
    fn a_burst_with_a_relevant_change_emits_exactly_once() {
        let quiet = DEBOUNCE + Duration::from_millis(50);
        let mut moved = wifi();
        moved.default_routes.clear();
        let n = drive(
            vec![(Tick::Poke, Duration::ZERO), (Tick::Poke, Duration::ZERO), (Tick::Idle, quiet), (Tick::Idle, quiet)],
            vec![Ok(wifi()), Ok(moved)],
        );
        assert_eq!(n, 1);
    }

    #[test]
    fn a_failed_snapshot_errs_on_the_side_of_emitting() {
        let quiet = DEBOUNCE + Duration::from_millis(50);
        let n = drive(vec![(Tick::Poke, Duration::ZERO), (Tick::Idle, quiet)], vec![Ok(wifi()), Err("boom".to_string())]);
        assert_eq!(n, 1);
    }

    #[test]
    fn nothing_is_emitted_without_a_poke() {
        let n = drive(vec![(Tick::Idle, Duration::ZERO), (Tick::Idle, Duration::ZERO)], vec![Ok(wifi())]);
        assert_eq!(n, 0);
    }
}
