//! Backend-agnostic warm-standby: keep a second, independent
//! [`AnyMuxConnection`] pre-established and periodically probed, then hand
//! it to a caller-supplied closure when the primary connection dies — the
//! generic mechanics behind `isekai-transport`'s own `WarmStandby` (which
//! wraps this with its ATTACH v2 resume protocol) and, since neither the
//! dial-a-spare-and-probe-it shape nor the single-flight promotion guard
//! reference anything isekai-specific, usable as-is by any caller of this
//! crate that wants the same "pay the QUIC handshake latency ahead of time,
//! not at the moment of failover" property for its own reconnection scheme
//! (e.g. one that has no resume protocol at all and just re-runs its own
//! lightweight auth handshake on the pre-dialed connection).
//!
//! # Why not `noq`'s native multipath
//!
//! `noq::Connection::open_path(local_ip=Some(..))` — holding two physical
//! interfaces simultaneously *within one connection* — is a confirmed dead
//! end ([noq issue #738](https://github.com/n0-computer/noq/issues/738)):
//! `PATH_RESPONSE` frames for such paths never reach noq's internal
//! dispatch, so the path is always abandoned. This module sidesteps that
//! entirely by bundling two *independent* connections at the application
//! layer instead — a design that also works with the `qmux` backend, which
//! has no path/multipath concept of its own at all.
//!
//! # Scope
//!
//! - **Standby health** ([`WarmStandby::ensure_warm`]): a lightweight,
//!   backend-agnostic probe (open a stream, verify it opens within a
//!   timeout, close it). Call it periodically (e.g. every 15-30s while the
//!   primary looks healthy, every 1-3s once it looks like it's degrading) to
//!   both keep NAT mappings alive and catch a dead standby before it's
//!   actually needed.
//! - **Primary failure detection is *not* this module's job.** The caller
//!   already drives the primary's own data stream (read/write loop) and is
//!   the first to observe a transport-level error there — this module only
//!   owns what happens *after* that decision: promoting the standby.
//! - **What "promotion" means is entirely up to the caller.** [`WarmStandby::promote`]
//!   takes an `on_promote` closure that receives the already-connected,
//!   already-warm [`AnyMuxConnection`] and returns whatever the caller's own
//!   protocol produces from it (a resumed data stream with replay offsets,
//!   a freshly-reauthenticated connection, or anything else) — this module
//!   itself never speaks any application-level resume/auth protocol.
//! - **Single-flight promotion**: [`WarmStandby::promote`] takes the standby
//!   connection out and marks a promotion in flight; a concurrent second
//!   call gets [`WarmStandbyError::AlreadyPromoting`] immediately rather
//!   than racing its own promotion attempt.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;

use crate::error::MuxError;
use crate::iface_dial::{connect_via_interface, InterfaceIndex};
use crate::mux::{AnyMuxConnection, AnyMuxFactory};
use crate::types::RemoteSpec;

/// How long [`WarmStandby::ensure_warm`]'s probe (open a stream, close it)
/// may take before the standby is judged dead and re-established. Short —
/// this is a liveness check on an already-established connection, not a
/// fresh handshake; a healthy connection answers `open_bi()` near-instantly.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// `E` is the caller's own error type for whatever `on_promote` does with the
/// pre-connected standby (e.g. `isekai-transport`'s `TransportError`, or a
/// consumer with no resume protocol simply reusing [`MuxError`]) — kept
/// generic rather than hardcoded to [`MuxError`] so a caller with a richer
/// application-level error (e.g. one that distinguishes *why* its own
/// reauth/resume step was rejected) doesn't lose that detail funneling
/// through this module's error type.
#[derive(Debug, thiserror::Error)]
pub enum WarmStandbyError<E: std::error::Error + 'static> {
    /// Another [`WarmStandby::promote`] call is already in flight — see this
    /// module's docs on why the caller should back off rather than retry.
    #[error("a promotion is already in flight")]
    AlreadyPromoting,
    /// [`WarmStandby::promote`] was called before [`WarmStandby::ensure_warm`]
    /// ever successfully established a standby connection (or the standby
    /// died and `ensure_warm` hasn't re-established it yet) — there is
    /// nothing to promote.
    #[error("no standby connection is currently warm")]
    NoStandby,
    /// `on_promote` itself failed (e.g. the caller's reauth/resume step was
    /// rejected, or the connection died before it could run).
    #[error(transparent)]
    Promote(#[from] E),
}

/// Holds a pre-established, periodically-probed standby [`AnyMuxConnection`]
/// to `remote`, and hands it to a caller-supplied closure on demand. See
/// this module's docs for the full design.
pub struct WarmStandby {
    factory: AnyMuxFactory,
    remote: RemoteSpec,
    /// When set, every standby connection is bound to this physical
    /// interface specifically instead of OS-default routing — see
    /// [`WarmStandby::new_bound_to_interface`]'s docs.
    interface: Option<InterfaceIndex>,
    port_range: Option<(u16, u16)>,
    standby: Mutex<Option<AnyMuxConnection>>,
    /// `promote`'s single-flight guard.
    promoting: AtomicBool,
}

impl WarmStandby {
    /// Builds a `WarmStandby` with no standby connection yet — call
    /// [`WarmStandby::ensure_warm`] at least once (and then periodically)
    /// before relying on [`WarmStandby::promote`] to succeed. Every standby
    /// connection is dialed with OS-default routing (whichever interface the
    /// OS picks) — for a warm-standby path that must specifically prove a
    /// *particular* physical interface (e.g. a cellular modem) is viable,
    /// use [`WarmStandby::new_bound_to_interface`] instead.
    pub fn new(factory: AnyMuxFactory, remote: RemoteSpec, port_range: Option<(u16, u16)>) -> Self {
        Self { factory, remote, interface: None, port_range, standby: Mutex::new(None), promoting: AtomicBool::new(false) }
    }

    /// Same as [`WarmStandby::new`], but every standby connection
    /// [`WarmStandby::ensure_warm`] establishes is bound to `interface`
    /// specifically (via [`crate::iface_dial::bind_physical_interface`])
    /// instead of OS-default routing — probing "any" interface doesn't prove
    /// the specific path is actually viable, which is the whole point of
    /// keeping it warm.
    ///
    /// `noq`-only: `qmux` has no bound-UDP-socket concept to restrict this
    /// way — using this constructor with a `qmux`-backed `factory` means
    /// every [`WarmStandby::ensure_warm`] call fails with
    /// [`crate::MuxError::Unsupported`] — this crate's existing "fail loud,
    /// don't silently ignore the request" stance on backend/capability
    /// mismatches.
    pub fn new_bound_to_interface(factory: AnyMuxFactory, remote: RemoteSpec, port_range: Option<(u16, u16)>, interface: InterfaceIndex) -> Self {
        Self { factory, remote, interface: Some(interface), port_range, standby: Mutex::new(None), promoting: AtomicBool::new(false) }
    }

    /// Whether a standby connection is currently held (does **not** re-probe
    /// it — a cheap, non-blocking check; the standby could still fail
    /// between this call and the next [`WarmStandby::promote`], exactly as
    /// with any liveness check).
    pub async fn is_warm(&self) -> bool {
        self.standby.lock().await.is_some()
    }

    /// (Re-)establishes the standby connection if it isn't already warm and
    /// responsive. Call this periodically rather than only once at startup,
    /// both to keep NAT mappings alive on a metered path and to catch a
    /// standby that died before [`WarmStandby::promote`] actually needs it
    /// (discovering that *during* a failover, with the primary already
    /// dead, would leave the caller with no viable path at all).
    pub async fn ensure_warm(&self) -> Result<(), MuxError> {
        let mut guard = self.standby.lock().await;
        if let Some(conn) = guard.as_ref() {
            match probe(conn).await {
                Ok(()) => return Ok(()),
                Err(_) => *guard = None,
            }
        }
        let conn = self.dial().await?;
        *guard = Some(conn);
        Ok(())
    }

    async fn dial(&self) -> Result<AnyMuxConnection, MuxError> {
        connect_via_interface(&self.factory, self.interface, self.remote.clone(), self.port_range).await
    }

    /// Promotes the standby connection: takes it out (whether `on_promote`
    /// succeeds or fails — a failed promotion does not leave a half-used
    /// connection behind for a later `ensure_warm` to accidentally reuse)
    /// and hands it to `on_promote`, returning whatever that closure
    /// produces. The caller should treat any [`WarmStandbyError`] other than
    /// [`WarmStandbyError::AlreadyPromoting`] as "no standby left,
    /// `ensure_warm` will build a new one."
    pub async fn promote<F, Fut, P, E>(&self, on_promote: F) -> Result<P, WarmStandbyError<E>>
    where
        F: FnOnce(AnyMuxConnection) -> Fut,
        Fut: Future<Output = Result<P, E>>,
        E: std::error::Error + 'static,
    {
        if self.promoting.swap(true, Ordering::SeqCst) {
            return Err(WarmStandbyError::AlreadyPromoting);
        }
        let result = self.promote_inner(on_promote).await;
        self.promoting.store(false, Ordering::SeqCst);
        result
    }

    async fn promote_inner<F, Fut, P, E>(&self, on_promote: F) -> Result<P, WarmStandbyError<E>>
    where
        F: FnOnce(AnyMuxConnection) -> Fut,
        Fut: Future<Output = Result<P, E>>,
        E: std::error::Error + 'static,
    {
        let conn = self.standby.lock().await.take().ok_or(WarmStandbyError::NoStandby)?;
        Ok(on_promote(conn).await?)
    }
}

/// Opens a stream and immediately shuts it down, bounded by
/// [`PROBE_TIMEOUT`] — a backend-agnostic liveness check. Deliberately does
/// not write/read any bytes: opening a bidirectional stream at all already
/// requires the connection to still be alive at the transport level, and
/// every backend this crate supports keeps its own idle-timeout/keepalive
/// machinery running underneath (`MuxClientConfig`) — this probe only needs
/// to catch "the connection is already gone" faster than that background
/// machinery would notice on its own.
async fn probe(conn: &AnyMuxConnection) -> Result<(), MuxError> {
    let mut stream = tokio::time::timeout(PROBE_TIMEOUT, conn.open_bi())
        .await
        .map_err(|_| MuxError::TransportLost { reason: "standby probe timed out".to_string(), retryable: true })??;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AnyMuxListener, MuxServerConfig};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    const TEST_ALPN: &[u8] = b"quicmux-warm-standby-test";
    const EXPORTER_LABEL: &[u8] = b"quicmux-warm-standby-test-exporter";

    fn test_client_config() -> crate::MuxClientConfig {
        crate::MuxClientConfig {
            alpn: TEST_ALPN.to_vec(),
            exporter_label: EXPORTER_LABEL.to_vec(),
            max_idle_timeout: Duration::from_secs(15),
            keep_alive_interval: Duration::from_secs(5),
            max_concurrent_bidi_streams: 4,
            max_concurrent_uni_streams: 0,
            multipath: false,
            datagram_send_buffer_size: None,
        }
    }

    fn test_server_config() -> (MuxServerConfig, String) {
        let cert = rcgen::generate_simple_self_signed(vec!["quicmux-warm-standby-test.local".to_string()]).unwrap();
        let cert_der = cert.cert.der().clone();
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
        let cert_sha256_hex = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(cert_der.as_ref());
            hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let config = MuxServerConfig {
            alpn: TEST_ALPN.to_vec(),
            exporter_label: EXPORTER_LABEL.to_vec(),
            max_idle_timeout: Duration::from_secs(15),
            keep_alive_interval: Duration::from_secs(5),
            max_concurrent_bidi_streams: 4,
            max_concurrent_uni_streams: 0,
            multipath: false,
            datagram_send_buffer_size: None,
            cert_chain: vec![cert_der],
            private_key: key_der,
        };
        (config, cert_sha256_hex)
    }

    async fn spawn_listener() -> (SocketAddr, String) {
        let (server_config, cert_sha256_hex) = test_server_config();
        let listener = AnyMuxListener::bind_noq(server_config, crate::BindSpec::any_ipv4()).await.unwrap();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            loop {
                let Some(incoming) = listener.accept().await else { break };
                let Ok(conn) = incoming.accept().await else { continue };
                tokio::spawn(async move {
                    loop {
                        let Ok(stream) = conn.accept_bi().await else { break };
                        let (mut recv, _send) = stream.split();
                        let mut buf = [0u8; 1];
                        let _ = recv.read(&mut buf).await;
                    }
                });
            }
        });
        (addr, cert_sha256_hex)
    }

    #[tokio::test]
    async fn ensure_warm_then_promote_hands_the_connection_to_the_closure() {
        let (addr, cert_sha256_hex) = spawn_listener().await;
        let remote = RemoteSpec { addr, server_name: "quicmux-warm-standby-test.local".to_string(), cert_sha256_hex };
        let factory = AnyMuxFactory::noq(test_client_config());

        let standby = WarmStandby::new(factory, remote, None);
        assert!(!standby.is_warm().await);
        standby.ensure_warm().await.expect("ensure_warm should succeed");
        assert!(standby.is_warm().await);

        let promoted: &'static str =
            standby.promote(|_conn| async move { Ok::<_, MuxError>("promoted") }).await.expect("promote should succeed");
        assert_eq!(promoted, "promoted");

        // The standby was consumed by promotion — nothing left to promote again.
        assert!(!standby.is_warm().await);
    }

    #[tokio::test]
    async fn promote_without_ensure_warm_fails_with_no_standby() {
        let remote =
            RemoteSpec { addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1), server_name: "unused.local".to_string(), cert_sha256_hex: "0".repeat(64) };
        let standby = WarmStandby::new(AnyMuxFactory::noq(test_client_config()), remote, None);
        let err = standby.promote(|_conn| async move { Ok::<(), MuxError>(()) }).await.unwrap_err();
        assert!(matches!(err, WarmStandbyError::NoStandby));
    }

    #[tokio::test]
    async fn a_second_concurrent_promote_is_rejected_single_flight() {
        let (addr, cert_sha256_hex) = spawn_listener().await;
        let remote = RemoteSpec { addr, server_name: "quicmux-warm-standby-test.local".to_string(), cert_sha256_hex };
        let standby = std::sync::Arc::new(WarmStandby::new(AnyMuxFactory::noq(test_client_config()), remote, None));
        standby.ensure_warm().await.unwrap();

        // Flip the guard directly to simulate "a promotion is already in
        // flight" without racing two real promotions against each other.
        standby.promoting.store(true, Ordering::SeqCst);
        let err = standby.promote(|_conn| async move { Ok::<(), MuxError>(()) }).await.unwrap_err();
        assert!(matches!(err, WarmStandbyError::AlreadyPromoting));
    }

    #[tokio::test]
    async fn ensure_warm_binds_to_the_requested_interface() {
        let loopback = quicsock::discovery::list_interfaces()
            .into_iter()
            .find(|(_, iface)| iface.is_loopback())
            .map(|(index, _)| index)
            .expect("this machine should have a loopback interface");

        let (addr, cert_sha256_hex) = spawn_listener().await;
        let remote = RemoteSpec { addr, server_name: "quicmux-warm-standby-test.local".to_string(), cert_sha256_hex };

        let standby = WarmStandby::new_bound_to_interface(AnyMuxFactory::noq(test_client_config()), remote, None, loopback);
        standby.ensure_warm().await.expect("ensure_warm should succeed when bound to the loopback interface");
        assert!(standby.is_warm().await);
    }
}
