//! Windows backend: `NotifyIpInterfaceChange` (iphlpapi) fires a callback on
//! any IP interface add/remove/parameter/connectivity change of *any*
//! adapter (virtual switches, VPN adapters, WSL/Hyper-V vEthernet, ...), far
//! broader than what matters to an established connection. So — same as the
//! Linux backend — a callback only *pokes* the shared
//! [`crate::snapshot::run_debounced_worker`] (running on a dedicated thread),
//! which debounces the burst, takes a fresh [`NetworkSnapshot`] from the IP
//! Helper tables (`GetIpForwardTable2`/`GetUnicastIpAddressTable`/
//! `GetIpInterfaceTable`) and emits an `InterfaceChange` only if the
//! reachability-relevant state actually changed (see `snapshot.rs`).
//!
//! **Not verified against a real Windows machine from this (Linux)
//! development environment** — only compiled by the Windows CI job.

use std::ffi::c_void;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::IpHelper::{
    CancelMibChangeNotify2, FreeMibTable, GetIpForwardTable2, GetIpInterfaceTable, GetUnicastIpAddressTable,
    NotifyIpInterfaceChange, MIB_IPFORWARD_TABLE2, MIB_IPINTERFACE_ROW, MIB_IPINTERFACE_TABLE, MIB_NOTIFICATION_TYPE,
    MIB_UNICASTIPADDRESS_TABLE,
};
use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_INET};

use crate::snapshot::{is_trackable_address, run_debounced_worker, NetworkSnapshot, Tick, TICK};
use crate::{NetworkChangeCause, NetworkChangeEvent, NetworkChangeMonitor};

type Poke = std::sync::mpsc::Sender<()>;

/// The callback `NotifyIpInterfaceChange` invokes on its own internal
/// (non-tokio) thread whenever any IP interface changes. `caller_context` is
/// the raw `*const Poke` this monitor registered — see `new()`'s safety
/// comment for why that pointer stays valid for the callback's whole
/// lifetime.
unsafe extern "system" fn interface_change_callback(
    caller_context: *const c_void,
    _row: *const MIB_IPINTERFACE_ROW,
    _notification_type: MIB_NOTIFICATION_TYPE,
) {
    if caller_context.is_null() {
        return;
    }
    let poke = &*(caller_context as *const Poke);
    // Only fails once the worker thread is gone; nothing to do about that
    // from inside an OS callback.
    let _ = poke.send(());
}

pub struct WindowsNetworkChangeMonitor {
    receiver: mpsc::UnboundedReceiver<NetworkChangeEvent>,
    notification_handle: HANDLE,
    /// Kept alive so the raw pointer registered as `CallerContext` stays
    /// valid for as long as the OS might still invoke the callback — `Drop`
    /// cancels the registration first; this field (declared after
    /// `notification_handle`) is dropped only afterwards, which also
    /// disconnects the worker thread's receiver and ends it.
    _poke_box: Box<Poke>,
    stop: Arc<AtomicBool>,
}

// SAFETY: `HANDLE` is a plain OS handle (an integer-sized value, not backed
// by thread-local state) and every access to it here goes through this
// struct's own `&mut self`/`Drop`, never shared concurrently.
unsafe impl Send for WindowsNetworkChangeMonitor {}

impl WindowsNetworkChangeMonitor {
    pub fn new() -> Result<Self, String> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (poke_tx, poke_rx) = std::sync::mpsc::channel::<()>();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("isekai-netmon-windows".to_string())
            .spawn(move || {
                run_debounced_worker(
                    &worker_stop,
                    || match poke_rx.recv_timeout(TICK) {
                        Ok(()) => Tick::Poke,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Tick::Idle,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Tick::Closed,
                    },
                    take_snapshot,
                    || tx.send(NetworkChangeEvent { cause: NetworkChangeCause::InterfaceChange }).is_ok(),
                );
            })
            .map_err(|e| format!("failed to spawn the network-monitor thread: {e}"))?;

        let poke_box = Box::new(poke_tx);
        // SAFETY: `poke_box` is kept alive for this monitor's entire lifetime
        // via `_poke_box`, and `Drop` cancels the OS registration *before*
        // `_poke_box` can be dropped, so this pointer never dangles while
        // `NotifyIpInterfaceChange` might still dereference it.
        let caller_context = poke_box.as_ref() as *const Poke as *const c_void;

        let mut handle = HANDLE::default();
        // SAFETY: `interface_change_callback` matches `PIPINTERFACE_CHANGE_CALLBACK`'s
        // required signature exactly; `caller_context` is valid per the
        // comment above; `handle` is written by the API on success.
        unsafe { NotifyIpInterfaceChange(AF_UNSPEC, Some(interface_change_callback), Some(caller_context), false, &mut handle) }
            .ok()
            .map_err(|e| {
                stop.store(true, Ordering::Relaxed);
                format!("NotifyIpInterfaceChange failed: {e}")
            })?;

        Ok(Self { receiver: rx, notification_handle: handle, _poke_box: poke_box, stop })
    }
}

#[async_trait]
impl NetworkChangeMonitor for WindowsNetworkChangeMonitor {
    async fn next_change(&mut self) -> Option<NetworkChangeEvent> {
        self.receiver.recv().await
    }
}

impl Drop for WindowsNetworkChangeMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // SAFETY: `notification_handle` was returned by a successful
        // `NotifyIpInterfaceChange` call in `new()` and has not been
        // cancelled yet (this is the only place that cancels it).
        unsafe {
            let _ = CancelMibChangeNotify2(self.notification_handle);
        }
    }
}

/// # Safety
/// `addr` must be a valid, initialized `SOCKADDR_INET`.
unsafe fn ip_from_sockaddr_inet(addr: &SOCKADDR_INET) -> Option<IpAddr> {
    let family = addr.si_family;
    if family == AF_INET {
        let raw = addr.Ipv4.sin_addr.S_un.S_addr;
        Some(IpAddr::V4(Ipv4Addr::from(raw.to_ne_bytes())))
    } else if family == AF_INET6 {
        Some(IpAddr::V6(Ipv6Addr::from(addr.Ipv6.sin6_addr.u.Byte)))
    } else {
        None
    }
}

fn take_snapshot() -> Result<NetworkSnapshot, String> {
    let mut snapshot = NetworkSnapshot::default();
    // SAFETY: each Get*Table2 call either fails (checked) or hands back a
    // table allocated by the OS containing `NumEntries` rows, which is freed
    // with `FreeMibTable` exactly once after being read.
    unsafe {
        let mut routes: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
        let err = GetIpForwardTable2(AF_UNSPEC, &mut routes);
        if err.0 != 0 || routes.is_null() {
            return Err(format!("GetIpForwardTable2 failed: {}", err.0));
        }
        let rows = std::slice::from_raw_parts((*routes).Table.as_ptr(), (*routes).NumEntries as usize);
        for row in rows {
            if row.DestinationPrefix.PrefixLength == 0 {
                let gateway = ip_from_sockaddr_inet(&row.NextHop).filter(|ip| !ip.is_unspecified());
                snapshot.default_routes.insert((row.InterfaceIndex, gateway));
            }
        }
        FreeMibTable(routes as *const c_void);

        let mut addrs: *mut MIB_UNICASTIPADDRESS_TABLE = std::ptr::null_mut();
        let err = GetUnicastIpAddressTable(AF_UNSPEC, &mut addrs);
        if err.0 != 0 || addrs.is_null() {
            return Err(format!("GetUnicastIpAddressTable failed: {}", err.0));
        }
        let rows = std::slice::from_raw_parts((*addrs).Table.as_ptr(), (*addrs).NumEntries as usize);
        for row in rows {
            if let Some(ip) = ip_from_sockaddr_inet(&row.Address) {
                if is_trackable_address(&ip) {
                    snapshot.addresses.insert((row.InterfaceIndex, ip));
                }
            }
        }
        FreeMibTable(addrs as *const c_void);

        let mut ifaces: *mut MIB_IPINTERFACE_TABLE = std::ptr::null_mut();
        let err = GetIpInterfaceTable(AF_UNSPEC, &mut ifaces);
        if err.0 != 0 || ifaces.is_null() {
            return Err(format!("GetIpInterfaceTable failed: {}", err.0));
        }
        let rows = std::slice::from_raw_parts((*ifaces).Table.as_ptr(), (*ifaces).NumEntries as usize);
        for row in rows {
            if row.Connected.0 != 0 {
                snapshot.running.insert(row.InterfaceIndex);
            }
        }
        FreeMibTable(ifaces as *const c_void);
    }
    Ok(snapshot)
}
