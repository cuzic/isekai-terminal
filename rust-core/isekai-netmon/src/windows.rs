//! Windows backend: `NotifyIpInterfaceChange` (iphlpapi) fires a callback on
//! any IP interface add/remove/address/connectivity change — this covers a
//! Wi-Fi disconnect/reconnect (and any other interface change; deliberately
//! broader than "Wi-Fi only", see module docs on why that's fine for a PC
//! that typically has one active network at a time).
//!
//! **Not verified against a real Windows machine** — this development
//! environment is Linux-only. Verified so far: `cargo build --target
//! x86_64-pc-windows-gnu` compiles and links cleanly against the `windows`
//! crate's generated bindings (mingw-w64 toolchain, no real Windows runtime
//! available to actually execute the result).

use std::ffi::c_void;

use async_trait::async_trait;
use tokio::sync::mpsc;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::IpHelper::{
    CancelMibChangeNotify2, NotifyIpInterfaceChange, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE,
};
use windows::Win32::Networking::WinSock::AF_UNSPEC;

use crate::{NetworkChangeEvent, NetworkChangeMonitor};

/// The callback `NotifyIpInterfaceChange` invokes on its own internal
/// (non-tokio) thread whenever any IP interface changes. `caller_context` is
/// the raw `*const Sender` this monitor registered — see `new()`'s safety
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
    let sender = &*(caller_context as *const mpsc::UnboundedSender<NetworkChangeEvent>);
    // An unbounded send only fails if the receiver (this monitor) was
    // already dropped — nothing to do about that from inside an OS
    // callback; `Drop` cancels the underlying registration anyway, so this
    // callback shouldn't fire again after that point.
    let _ = sender.send(NetworkChangeEvent);
}

pub struct WindowsNetworkChangeMonitor {
    receiver: mpsc::UnboundedReceiver<NetworkChangeEvent>,
    /// Kept alive so the raw pointer registered as `CallerContext` (see
    /// `new()`) stays valid for as long as the OS might still invoke the
    /// callback — must not be dropped before `notification_handle` is
    /// cancelled, which is why `Drop` below cancels first, then lets this
    /// field's own `Drop` run afterward (struct field drop order is
    /// declaration order).
    _sender_box: Box<mpsc::UnboundedSender<NetworkChangeEvent>>,
    notification_handle: HANDLE,
}

// SAFETY: `HANDLE` is a plain OS handle (an integer-sized value, not backed
// by thread-local state) and every access to it here goes through this
// struct's own `&mut self`/`Drop`, never shared concurrently.
unsafe impl Send for WindowsNetworkChangeMonitor {}

impl WindowsNetworkChangeMonitor {
    pub fn new() -> Result<Self, String> {
        let (tx, rx) = mpsc::unbounded_channel();
        let sender_box = Box::new(tx);
        // SAFETY: `sender_box` (and the `UnboundedSender` it owns) is kept
        // alive for this monitor's entire lifetime via `_sender_box` below,
        // and `Drop` cancels the OS registration *before* `_sender_box` can
        // be dropped (declaration order = drop order), so this pointer
        // never dangles while `NotifyIpInterfaceChange` might still
        // dereference it.
        let caller_context = sender_box.as_ref() as *const mpsc::UnboundedSender<NetworkChangeEvent> as *const c_void;

        let mut handle = HANDLE::default();
        // SAFETY: `interface_change_callback` matches `PIPINTERFACE_CHANGE_CALLBACK`'s
        // required signature exactly; `caller_context` is valid per the
        // comment above; `handle` is a fresh, uninitialized-but-valid-to-write
        // `HANDLE` the API fills in on success.
        unsafe {
            NotifyIpInterfaceChange(
                AF_UNSPEC,
                Some(interface_change_callback),
                Some(caller_context),
                false,
                &mut handle,
            )
        }
        .ok()
        .map_err(|e| format!("NotifyIpInterfaceChange failed: {e}"))?;

        Ok(Self { receiver: rx, _sender_box: sender_box, notification_handle: handle })
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
        // SAFETY: `notification_handle` was returned by a successful
        // `NotifyIpInterfaceChange` call in `new()` and has not been
        // cancelled yet (this is the only place that cancels it).
        unsafe {
            let _ = CancelMibChangeNotify2(self.notification_handle);
        }
    }
}
