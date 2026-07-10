//! Optional convenience layer on top of the [`netdev`] crate (enabled by the
//! `discovery` feature). [`bind_udp`](crate::bind_udp) only needs a raw
//! [`InterfaceIndex`](crate::InterfaceIndex), so nothing in `quicsock`'s
//! core API forces this dependency on callers who already have their own
//! way of finding an interface index.

use crate::InterfaceIndex;

/// Lists every network interface the OS currently reports, alongside the
/// [`InterfaceIndex`] each one can be passed to [`bind_udp`](crate::bind_udp).
///
/// This is a thin re-export of [`netdev::get_interfaces`] paired with each
/// interface's index — see [`netdev::Interface`] for everything else it
/// reports (addresses, `friendly_name`, `if_type`, ...).
pub fn list_interfaces() -> Vec<(InterfaceIndex, netdev::Interface)> {
    netdev::get_interfaces()
        .into_iter()
        .map(|iface| (InterfaceIndex(iface.index), iface))
        .collect()
}

/// The OS's notion of the "default" interface (the one its default route
/// currently points at), if it has one right now.
pub fn default_interface() -> Result<(InterfaceIndex, netdev::Interface), String> {
    netdev::get_default_interface().map(|iface| (InterfaceIndex(iface.index), iface))
}
