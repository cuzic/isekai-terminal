//! Warm-standby failover: keep a second, independent [`AnyMuxConnection`]
//! pre-established and periodically probed, then promote it via
//! `quicmux::resume` when the primary connection dies — the client-side
//! counterpart to `quicmux-server-resume` Stage B's generic resume
//! primitive, built specifically for PC (Windows/macOS) Wi-Fi + USB/
//! Bluetooth tethering warm-standby (motivating design discussion recorded
//! as this session's `pc-tethering-warm-standby-design` memory).
//!
//! # Why not `noq`'s native multipath
//!
//! `noq::Connection::open_path(local_ip=Some(..))` — holding two physical
//! interfaces simultaneously *within one connection* — is a confirmed dead
//! end ([noq issue #738](https://github.com/n0-computer/noq/issues/738)):
//! `PATH_RESPONSE` frames for such paths never reach noq's internal
//! dispatch, so the path is always abandoned. This module sidesteps that
//! entirely by bundling two *independent* connections at the application
//! layer instead (mirroring how `finish_via_resume` in `resume.rs` already
//! resumes onto a fresh connection) — a design that also works with the
//! `qmux` backend, which has no path/multipath concept of its own at all.
//!
//! # Scope
//!
//! - **Standby health** ([`WarmStandby::ensure_warm`]): a lightweight,
//!   backend-agnostic probe (open a stream, verify it opens within a
//!   timeout, close it) — *not* `path_health.rs`'s `noq::Path`-based ping/
//!   stats mechanism, which only applies to multiple paths *within one* noq
//!   multipath connection and has no equivalent for `qmux` or for two
//!   genuinely separate connections. Call `ensure_warm` periodically (the
//!   `pc-tethering-warm-standby-design` memory's agreed tiering: ~15-30s
//!   while the primary looks healthy, ~1-3s once it looks like it's
//!   degrading) to both keep NAT mappings alive and catch a dead standby
//!   before it's actually needed.
//! - **Primary failure detection is *not* this module's job.** The caller
//!   already drives the primary's own data stream (read/write loop) and is
//!   the first to observe a transport-level error there — this module only
//!   owns what happens *after* that decision: promoting the standby.
//! - **Promotion reuses the already-connected standby connection**, not a
//!   fresh dial — unlike `resume::reconnect_and_resume` (which always dials
//!   a brand-new connection), [`WarmStandby::promote`] issues the resume
//!   request directly on the connection [`WarmStandby::ensure_warm`] already
//!   established and kept alive. That's the entire point of "warm": the
//!   QUIC/QMux handshake latency is paid ahead of time, not at the moment of
//!   failover.
//! - **Single-flight promotion**: [`WarmStandby::promote`] takes the
//!   standby connection out and marks a promotion in flight; a concurrent
//!   second call (e.g. a caller's independent read task and write task both
//!   noticing the primary died at nearly the same time) gets
//!   [`WarmStandbyError::AlreadyPromoting`] immediately rather than racing
//!   its own resume attempt — the caller should treat that as "someone else
//!   is already handling this," not retry. This is a client-side efficiency/
//!   clarity guard, not the sole correctness backstop: the server's own
//!   `ResumeAcceptor::try_resume` contract (quicmux-server-resume Stage B)
//!   already makes a second *concurrent* resume attempt for the same
//!   session fail closed (`UnknownToken`) even if this guard somehow didn't
//!   exist, since a session can only be "claimed" once server-side.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use log::{info, warn};
use tokio::sync::Mutex;

use isekai_protocol::offset::{C2hHelperCommittedOffset, C2hSentOffset, H2cClientDeliveredOffset, H2cSentOffset};
use isekai_protocol::session_id::SessionId;
use quicmux::{AnyByteStream, AnyMuxConnection, AnyMuxFactory, MuxError, RemoteSpec};

use crate::physical_interface::InterfaceIndex;

use crate::error::TransportError;
use crate::proof::compute_proof;
use crate::relay::RelayTarget;
use crate::resume::map_reject_reason;

/// How long [`WarmStandby::ensure_warm`]'s probe (open a stream, close it)
/// may take before the standby is judged dead and re-established. Short —
/// this is a liveness check on an already-established connection, not a
/// fresh handshake; a healthy connection answers `open_bi()` near-instantly.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// A successful [`WarmStandby::promote`]: the resumed connection and data
/// stream (ready for raw pass-through, exactly like a fresh `HELLO`/`ACK`'d
/// or `reconnect_and_resume`d connection), plus the offsets the acceptor
/// reported so the caller knows what it may safely discard from its own C2H
/// replay buffer.
pub struct PromotedConnection {
    pub connection: AnyMuxConnection,
    pub data_stream: AnyByteStream,
    pub helper_committed_offset: C2hHelperCommittedOffset,
    pub helper_sent_offset: H2cSentOffset,
}

impl std::fmt::Debug for PromotedConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromotedConnection")
            .field("helper_committed_offset", &self.helper_committed_offset)
            .field("helper_sent_offset", &self.helper_sent_offset)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WarmStandbyError {
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
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// Holds a pre-established, periodically-probed standby [`AnyMuxConnection`]
/// to the same `target` a primary connection is already using, and promotes
/// it to a resumed data stream on demand. See this module's docs for the
/// full design.
pub struct WarmStandby {
    factory: AnyMuxFactory,
    target: RelayTarget,
    session_id: SessionId,
    /// When set, every standby connection is bound to this physical
    /// interface specifically instead of OS-default routing — see
    /// [`WarmStandby::new_bound_to_interface`]'s docs.
    interface: Option<InterfaceIndex>,
    standby: Mutex<Option<AnyMuxConnection>>,
    /// `promote`'s single-flight guard — see this module's docs on why this
    /// is a client-side efficiency/clarity measure, not the sole
    /// correctness backstop against a double-promotion.
    promoting: AtomicBool,
}

impl WarmStandby {
    /// Builds a `WarmStandby` with no standby connection yet — call
    /// [`WarmStandby::ensure_warm`] at least once (and then periodically)
    /// before relying on [`WarmStandby::promote`] to succeed. Every standby
    /// connection is dialed with OS-default routing (whichever interface the
    /// OS picks) — for a warm-standby path that must specifically prove a
    /// *particular* physical interface (e.g. a USB/Bluetooth tethering
    /// adapter) is viable, use [`WarmStandby::new_bound_to_interface`]
    /// instead.
    pub fn new(factory: AnyMuxFactory, target: RelayTarget, session_id: SessionId) -> Self {
        Self { factory, target, session_id, interface: None, standby: Mutex::new(None), promoting: AtomicBool::new(false) }
    }

    /// Same as [`WarmStandby::new`], but every standby connection
    /// [`WarmStandby::ensure_warm`] establishes is bound to `interface`
    /// specifically (via [`crate::physical_interface::bind_physical_interface`])
    /// instead of OS-default routing — probing "any" interface doesn't prove
    /// the specific tethering path is actually viable, which is the whole
    /// point of keeping it warm.
    ///
    /// `noq`-only: `qmux` has no bound-UDP-socket concept to restrict this
    /// way — [`AnyMuxFactory::wrap_bound_socket`] structurally cannot
    /// succeed for it (see that method's docs). Using this constructor with
    /// a `qmux`-backed `factory` means every [`WarmStandby::ensure_warm`]
    /// call fails with [`quicmux::MuxError::Unsupported`] — this crate's
    /// existing "fail loud, don't silently ignore the request" stance on
    /// backend/capability mismatches (matches
    /// [`quicmux::AnyMuxEndpoint::rebinder`] returning `None` rather than a
    /// no-op for the same reason).
    pub fn new_bound_to_interface(
        factory: AnyMuxFactory,
        target: RelayTarget,
        session_id: SessionId,
        interface: InterfaceIndex,
    ) -> Self {
        Self { factory, target, session_id, interface: Some(interface), standby: Mutex::new(None), promoting: AtomicBool::new(false) }
    }

    /// Whether a standby connection is currently held (does **not** re-probe
    /// it — a cheap, non-blocking check; the standby could still fail
    /// between this call and the next [`WarmStandby::promote`], exactly as
    /// with any liveness check). Useful for a caller's own UI/telemetry
    /// ("tethering standby ready") without needing to know this module's
    /// internal probe timing.
    pub async fn is_warm(&self) -> bool {
        self.standby.lock().await.is_some()
    }

    /// (Re-)establishes the standby connection if it isn't already warm and
    /// responsive. Call this periodically — see this module's docs for the
    /// agreed keepalive tiering — rather than only once at startup, both to
    /// keep NAT mappings alive on a metered tethering path and to catch a
    /// standby that died before [`WarmStandby::promote`] actually needs it
    /// (discovering that *during* a failover, with the primary already
    /// dead, would leave the caller with no viable path at all).
    pub async fn ensure_warm(&self) -> Result<(), TransportError> {
        let mut guard = self.standby.lock().await;
        if let Some(conn) = guard.as_ref() {
            match probe(conn).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    warn!("warm_standby: standby probe failed ({e}), re-establishing");
                    *guard = None;
                }
            }
        }
        let conn = self.dial().await?;
        info!("warm_standby: standby connection established to {}", self.target.helper_addr);
        *guard = Some(conn);
        Ok(())
    }

    /// Binds (either OS-default or, if [`WarmStandby::new_bound_to_interface`]
    /// was used, restricted to `self.interface`) a fresh local socket and
    /// dials `self.target`. Split out of [`WarmStandby::ensure_warm`] purely
    /// so that method's "probe, and only dial if the probe failed" flow
    /// isn't tangled up with the two different ways of obtaining an
    /// endpoint.
    async fn dial(&self) -> Result<AnyMuxConnection, TransportError> {
        let endpoint = match self.interface {
            None => self.factory.create_endpoint(quicmux::BindSpec::any_ipv4()).await.map_err(TransportError::Mux)?,
            Some(interface) => {
                let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
                let std_socket = crate::physical_interface::bind_physical_interface(interface, local_addr)
                    .map_err(|source| TransportError::Mux(MuxError::Bind { addr: local_addr, source }))?;
                std_socket.set_nonblocking(true).map_err(|e| TransportError::Mux(MuxError::SocketSetup(e.to_string())))?;
                let tokio_socket =
                    tokio::net::UdpSocket::from_std(std_socket).map_err(|e| TransportError::Mux(MuxError::SocketSetup(e.to_string())))?;
                self.factory.wrap_bound_socket(tokio_socket).await.map_err(TransportError::Mux)?
            }
        };
        endpoint
            .connect(RemoteSpec {
                addr: self.target.helper_addr,
                server_name: self.target.server_name.clone(),
                cert_sha256_hex: self.target.cert_sha256_hex.clone(),
            })
            .await
            .map_err(TransportError::Mux)
    }

    /// Promotes the standby connection: issues a `quicmux::resume` RESUME
    /// request directly on it (no fresh dial — see this module's docs on
    /// why that's the whole point of "warm") and returns the resumed data
    /// stream. Takes the standby connection out unconditionally once a
    /// promotion attempt starts (whether it succeeds or fails) — a failed
    /// promotion does not leave a half-used connection behind for a later
    /// `ensure_warm` to accidentally reuse; the caller should treat any
    /// [`WarmStandbyError`] other than [`WarmStandbyError::AlreadyPromoting`]
    /// as "no standby left, `ensure_warm` will build a new one."
    pub async fn promote(
        &self,
        client_sent_offset: C2hSentOffset,
        client_delivered_offset: H2cClientDeliveredOffset,
    ) -> Result<PromotedConnection, WarmStandbyError> {
        if self.promoting.swap(true, Ordering::SeqCst) {
            return Err(WarmStandbyError::AlreadyPromoting);
        }
        let result = self.promote_inner(client_sent_offset, client_delivered_offset).await;
        self.promoting.store(false, Ordering::SeqCst);
        result
    }

    async fn promote_inner(
        &self,
        client_sent_offset: C2hSentOffset,
        client_delivered_offset: H2cClientDeliveredOffset,
    ) -> Result<PromotedConnection, WarmStandbyError> {
        let conn = self.standby.lock().await.take().ok_or(WarmStandbyError::NoStandby)?;

        // Same proof scheme `resume::reconnect_and_resume` uses — see that
        // function's identical comment for why `session_id` is mixed in.
        let resume_proof = compute_proof(&conn, &self.target.session_secret, self.session_id.as_bytes()).await?;

        let outcome = quicmux::request_resume(
            &conn,
            self.session_id.as_bytes(),
            resume_proof.as_bytes(),
            client_sent_offset.get(),
            client_delivered_offset.get(),
        )
        .await
        .map_err(|e| match e {
            quicmux::ResumeRequestError::Mux(mux_err) => TransportError::Mux(mux_err),
            quicmux::ResumeRequestError::Rejected(reason) => TransportError::ResumeRejected(map_reject_reason(reason)),
        })?;

        info!(
            "warm_standby: promoted standby, session_id={}, helper_committed_offset={}",
            self.session_id, outcome.committed_offset
        );
        Ok(PromotedConnection {
            connection: conn,
            data_stream: outcome.stream,
            helper_committed_offset: C2hHelperCommittedOffset::new(outcome.committed_offset),
            helper_sent_offset: H2cSentOffset::new(outcome.sent_offset),
        })
    }
}

/// Opens a stream and immediately shuts it down, bounded by
/// [`PROBE_TIMEOUT`] — the backend-agnostic liveness check this module uses
/// in place of `path_health.rs`'s `noq`-specific ping/stats mechanism (see
/// this module's top docs for why that doesn't apply here). Deliberately
/// does not write/read any bytes: opening a bidirectional stream at all
/// already requires the connection to still be alive at the transport
/// level, and every backend this crate supports keeps its own idle-timeout/
/// keepalive machinery running underneath (`MuxClientConfig`) — this probe
/// only needs to catch "the connection is already gone" faster than that
/// background machinery would notice on its own.
async fn probe(conn: &AnyMuxConnection) -> Result<(), MuxError> {
    let mut stream = tokio::time::timeout(PROBE_TIMEOUT, conn.open_bi())
        .await
        .map_err(|_| MuxError::TransportLost { reason: "standby probe timed out".to_string(), retryable: true })??;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::system_quic_factory;
    use quicmux::{AnyMuxListener, MuxServerConfig};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn test_server_config() -> (MuxServerConfig, String) {
        let cert = rcgen::generate_simple_self_signed(vec!["isekai-pipe.local".to_string()]).unwrap();
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().clone());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
        let cert_sha256_hex = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(cert_der.as_ref());
            hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let config = MuxServerConfig {
            alpn: isekai_protocol::hello::ALPN.to_vec(),
            exporter_label: isekai_protocol::hello::EXPORTER_LABEL.to_vec(),
            max_idle_timeout: Duration::from_secs(15),
            keep_alive_interval: Duration::from_secs(5),
            max_concurrent_bidi_streams: 4,
            max_concurrent_uni_streams: 0,
            multipath: false,
            cert_chain: vec![cert_der],
            private_key: key_der,
        };
        (config, cert_sha256_hex)
    }

    async fn spawn_resume_capable_listener() -> (SocketAddr, String, [u8; 32], SessionId) {
        let (server_config, cert_sha256_hex) = test_server_config();
        let listener = AnyMuxListener::bind_noq(server_config, quicmux::BindSpec::any_ipv4()).await.unwrap();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listener.local_addr().unwrap().port());

        let session_secret: [u8; 32] = rand::random();
        let session_id = SessionId::from_bytes(rand::random());

        tokio::spawn(async move {
            loop {
                let Some(incoming) = listener.accept().await else { break };
                let Ok(conn) = incoming.accept().await else { continue };
                let session_secret = session_secret;
                tokio::spawn(async move {
                    loop {
                        let Ok(stream) = conn.accept_bi().await else { break };
                        let (mut recv, mut send) = stream.split();
                        let mut frame_type = [0u8; 1];
                        if recv.read(&mut frame_type).await.unwrap_or(0) == 0 {
                            break;
                        }
                        if frame_type[0] != quicmux::FRAME_RESUME {
                            // The probe: just opens and shuts down a stream
                            // with no bytes at all — nothing to respond to.
                            continue;
                        }
                        let exporter = conn.export_keying_material(isekai_protocol::hello::EXPORTER_LABEL, b"").await.unwrap();
                        let request = quicmux::decode_resume_request(&mut recv, exporter).await.unwrap();
                        let expected = {
                            use hmac::{Hmac, Mac};
                            let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&session_secret).unwrap();
                            mac.update(&exporter);
                            mac.update(&request.token);
                            mac.finalize().into_bytes()
                        };
                        if request.auth_blob != expected.as_slice() {
                            quicmux::respond_resume_rejected(&mut send, quicmux::ResumeRejectReason::Auth).await;
                            continue;
                        }
                        quicmux::respond_resume_accepted(&mut send, 100, 200, b"promoted-replay").await.unwrap();
                    }
                });
            }
        });

        (addr, cert_sha256_hex, session_secret, session_id)
    }

    #[tokio::test]
    async fn ensure_warm_then_promote_succeeds_without_a_fresh_dial() {
        let (addr, cert_sha256_hex, session_secret, session_id) = spawn_resume_capable_listener().await;
        let target = RelayTarget {
            helper_addr: addr,
            server_name: "isekai-pipe.local".to_string(),
            cert_sha256_hex,
            session_secret: session_secret.to_vec(),
            local_bind_port_range: None,
        };
        let factory = system_quic_factory();

        let standby = WarmStandby::new(factory, target, session_id);
        assert!(!standby.is_warm().await);
        standby.ensure_warm().await.expect("ensure_warm should succeed");
        assert!(standby.is_warm().await);

        let promoted = standby
            .promote(C2hSentOffset::new(300), H2cClientDeliveredOffset::new(190))
            .await
            .expect("promote should succeed against the already-warm standby");
        assert_eq!(promoted.helper_committed_offset.get(), 100);
        assert_eq!(promoted.helper_sent_offset.get(), 200);

        let mut stream = promoted.data_stream;
        let mut buf = [0u8; 32];
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"promoted-replay");

        // The standby was consumed by promotion — nothing left to promote again.
        assert!(!standby.is_warm().await);
    }

    #[tokio::test]
    async fn ensure_warm_binds_to_the_requested_interface() {
        let loopback = quicsock::discovery::list_interfaces()
            .into_iter()
            .find(|(_, iface)| iface.is_loopback())
            .map(|(index, _)| index)
            .expect("this machine should have a loopback interface");

        let (addr, cert_sha256_hex, session_secret, session_id) = spawn_resume_capable_listener().await;
        let target = RelayTarget {
            helper_addr: addr,
            server_name: "isekai-pipe.local".to_string(),
            cert_sha256_hex,
            session_secret: session_secret.to_vec(),
            local_bind_port_range: None,
        };

        let standby = WarmStandby::new_bound_to_interface(system_quic_factory(), target, session_id, loopback);
        standby.ensure_warm().await.expect("ensure_warm should succeed when bound to the loopback interface");
        assert!(standby.is_warm().await);

        let promoted = standby.promote(C2hSentOffset::new(0), H2cClientDeliveredOffset::new(0)).await.expect("promote should succeed");
        assert_eq!(promoted.helper_committed_offset.get(), 100);
    }

    #[tokio::test]
    async fn ensure_warm_fails_on_a_bogus_interface_rather_than_silently_falling_back() {
        let target = RelayTarget {
            helper_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
            server_name: "isekai-pipe.local".to_string(),
            cert_sha256_hex: "0".repeat(64),
            session_secret: vec![0u8; 32],
            local_bind_port_range: None,
        };
        let standby =
            WarmStandby::new_bound_to_interface(system_quic_factory(), target, SessionId::from_bytes([0u8; 16]), InterfaceIndex(u32::MAX));
        let err = standby.ensure_warm().await.unwrap_err();
        assert!(matches!(err, TransportError::Mux(MuxError::Bind { .. })), "expected a Bind error, got {err:?}");
    }

    #[tokio::test]
    async fn promote_without_ensure_warm_fails_with_no_standby() {
        let target = RelayTarget {
            helper_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
            server_name: "isekai-pipe.local".to_string(),
            cert_sha256_hex: "0".repeat(64),
            session_secret: vec![0u8; 32],
            local_bind_port_range: None,
        };
        let standby = WarmStandby::new(system_quic_factory(), target, SessionId::from_bytes([0u8; 16]));
        let err = standby.promote(C2hSentOffset::new(0), H2cClientDeliveredOffset::new(0)).await.unwrap_err();
        assert!(matches!(err, WarmStandbyError::NoStandby));
    }

    #[tokio::test]
    async fn a_second_concurrent_promote_is_rejected_single_flight() {
        let (addr, cert_sha256_hex, session_secret, session_id) = spawn_resume_capable_listener().await;
        let target = RelayTarget {
            helper_addr: addr,
            server_name: "isekai-pipe.local".to_string(),
            cert_sha256_hex,
            session_secret: session_secret.to_vec(),
            local_bind_port_range: None,
        };
        let standby = std::sync::Arc::new(WarmStandby::new(system_quic_factory(), target, session_id));
        standby.ensure_warm().await.unwrap();

        // Flip the guard directly to simulate "a promotion is already in
        // flight" without racing two real promotions against each other
        // (which would be inherently timing-dependent to assert on) — the
        // guard itself is what this test exists to prove, not the timing.
        standby.promoting.store(true, Ordering::SeqCst);
        let err = standby.promote(C2hSentOffset::new(0), H2cClientDeliveredOffset::new(0)).await.unwrap_err();
        assert!(matches!(err, WarmStandbyError::AlreadyPromoting));
    }
}
