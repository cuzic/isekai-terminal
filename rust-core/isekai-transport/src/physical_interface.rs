//! Re-exports `quicmux::iface_dial` — binding a UDP socket to a specific
//! physical network interface, and dialing through it, moved to `quicmux`
//! (2026-07-24) because it has no dependency on `isekai-transport`/
//! `isekai-protocol` — it only needs `quicmux` itself and the vendored
//! `quicsock` crate. This module is kept as a thin re-export so existing
//! callers (`isekai-pipe`/`isekai-ssh`, this crate's own `dual_path.rs`/
//! `warm_standby.rs`) don't need to change their `crate::physical_interface::*`
//! paths. See `quicmux::iface_dial`'s own module docs for the full design,
//! including the `bind_physical_interface`/`connect_via_interface`
//! implementation and their tests.

pub use quicmux::iface_dial::{bind_physical_interface, connect_via_interface, quicsock, InterfaceIndex};
