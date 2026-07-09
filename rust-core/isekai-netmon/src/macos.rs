//! macOS backend: `SCNetworkReachability` (SystemConfiguration.framework),
//! via the `system-configuration` crate (the same wrapper Mullvad's VPN
//! client uses for exactly this "detect a network change to trigger a
//! reconnect" purpose). Watches general reachability of `0.0.0.0:0` — the
//! same "any route" address Apple's own `Reachability` sample code and this
//! crate's own upstream tests use to mean "is there *some* usable network
//! path right now", not reachability of any specific host — so it fires on
//! a Wi-Fi disconnect/reconnect (and any other network-path change), which
//! is exactly the "network changed" signal this crate needs (module docs).
//!
//! `SCNetworkReachability` callbacks are only ever delivered by pumping a
//! `CFRunLoop`, so this backend owns a dedicated background thread whose
//! entire job is running that run loop; `Drop` asks it to stop
//! (`CFRunLoop::stop`, safe to call cross-thread by design) and joins it.
//!
//! **Not verified against a real macOS machine** — this development
//! environment is Linux-only. Verified so far: `cargo check --target
//! aarch64-apple-darwin` type-checks cleanly against the `system-configuration`/
//! `core-foundation` crates' real bindings; actual linking (which needs the
//! macOS SDK) and execution could not be attempted here.

use std::net::SocketAddr;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use async_trait::async_trait;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use system_configuration::network_reachability::SCNetworkReachability;
use tokio::sync::mpsc;

use crate::{NetworkChangeEvent, NetworkChangeMonitor};

/// How long `new()` waits for the background run-loop thread to finish
/// registering before giving up and falling back to
/// [`crate::NoopNetworkChangeMonitor`] (`system_monitor`'s caller). Purely a
/// startup-failure detector — once registration succeeds, the thread runs
/// for this monitor's entire lifetime.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

pub struct MacosNetworkChangeMonitor {
    receiver: mpsc::UnboundedReceiver<NetworkChangeEvent>,
    run_loop: CFRunLoop,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl MacosNetworkChangeMonitor {
    pub fn new() -> Result<Self, String> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (setup_tx, setup_rx) = std_mpsc::channel();

        let worker = std::thread::Builder::new()
            .name("isekai-netmon-macos".to_string())
            .spawn(move || {
                let any_route: SocketAddr = "0.0.0.0:0".parse().expect("valid socket address literal");
                let mut reachability = SCNetworkReachability::from(any_route);

                if let Err(e) = reachability.set_callback(move |_flags| {
                    // An unbounded send only fails once the receiver (this
                    // monitor) has been dropped, at which point `Drop` has
                    // already asked this run loop to stop — nothing to do
                    // from inside the callback either way.
                    let _ = event_tx.send(NetworkChangeEvent);
                }) {
                    let _ = setup_tx.send(Err(format!("SCNetworkReachabilitySetCallback failed: {e}")));
                    return;
                }

                let run_loop = CFRunLoop::get_current();
                // SAFETY: `kCFRunLoopCommonModes` is the Apple-provided
                // constant the `system-configuration` crate's own docs
                // recommend for exactly this call when unsure which mode to
                // use (see `network_reachability.rs`'s `schedule_with_runloop`
                // doc comment).
                let scheduled = unsafe { reachability.schedule_with_runloop(&run_loop, kCFRunLoopCommonModes) };
                if let Err(e) = scheduled {
                    let _ = setup_tx.send(Err(format!("SCNetworkReachabilityScheduleWithRunLoop failed: {e}")));
                    return;
                }

                if setup_tx.send(Ok(run_loop)).is_err() {
                    // `new()` already gave up waiting (timed out) — nothing
                    // left to notify, just exit instead of running a run
                    // loop nobody can ever stop cleanly via `Drop`.
                    return;
                }

                // Blocks this thread, delivering `reachability`'s callback
                // on every network-path change, until `Drop` calls
                // `run_loop.stop()` from another thread.
                CFRunLoop::run_current();
            })
            .map_err(|e| format!("failed to spawn the network-reachability thread: {e}"))?;

        let run_loop = setup_rx
            .recv_timeout(STARTUP_TIMEOUT)
            .map_err(|e| format!("timed out waiting for the network-reachability thread to start: {e}"))??;

        Ok(Self { receiver: event_rx, run_loop, worker: Some(worker) })
    }
}

#[async_trait]
impl NetworkChangeMonitor for MacosNetworkChangeMonitor {
    async fn next_change(&mut self) -> Option<NetworkChangeEvent> {
        self.receiver.recv().await
    }
}

impl Drop for MacosNetworkChangeMonitor {
    fn drop(&mut self) {
        self.run_loop.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
