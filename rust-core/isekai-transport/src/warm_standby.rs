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
//! - **Standby hold** (`isekai_protocol::standby`): the real `isekai-pipe
//!   serve` closes any connection whose first stream doesn't deliver a frame
//!   type within its `HELLO_TIMEOUT` (5s). A standby therefore opens its
//!   first stream with an authenticated `STANDBY_HOLD` frame, which the
//!   server exempts from that deadline; promotion sends the RESUME on a
//!   *second* stream. (Before this existed the server silently closed every
//!   standby ~5s after dialing it, so promotion almost never worked and each
//!   `ensure_warm` tick re-dialed — the unit-test mock server here didn't
//!   enforce the deadline, which hid it. The mock now does.) An older server
//!   answers `STANDBY_HOLD` with `FRAME_REJECT_UNSUPPORTED`; the standby is
//!   then disabled for that session rather than re-dialed every tick.
//! - **Standby health** ([`WarmStandby::ensure_warm`]): a lightweight,
//!   backend-agnostic probe (a `STANDBY_PING`/`STANDBY_PONG` round trip on
//!   the hold stream, within a timeout) — *not* `path_health.rs`'s `noq::Path`-based ping/
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
//!   clarity guard, not the sole correctness backstop: the server
//!   (`isekai-pipe serve`'s `handle_resume_stream`) already makes a second
//!   *concurrent* resume attempt for the same session fail closed
//!   (`UnknownToken`) even if this guard somehow didn't exist, because it
//!   claims the session by `parked_tcp.take()` under the session lock — so
//!   only one attempt can ever find a parked connection to resume onto.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use log::{info, warn};
use tokio::sync::Mutex;

use isekai_protocol::offset::{C2hSentOffset, H2cClientDeliveredOffset};
use isekai_protocol::session_id::SessionId;
use isekai_protocol::standby::{
    encode_standby_hold, FRAME_STANDBY_PING, FRAME_STANDBY_PONG, FRAME_STANDBY_READY, STANDBY_HOLD_PROOF_DOMAIN,
};
use quicmux::{AnyByteStream, AnyMuxConnection, AnyMuxFactory, MuxError};

use crate::physical_interface::InterfaceIndex;

use crate::error::TransportError;
use crate::proof::compute_proof;
use crate::relay::RelayTarget;
use crate::resume::{resume_on_connection, ResumeAckOutcome, TRANSPORT_STEP_TIMEOUT};

/// How long [`WarmStandby::ensure_warm`]'s probe (a `STANDBY_PING`/
/// `STANDBY_PONG` round trip on the hold stream) may take before the standby
/// is judged dead and re-established. Short — this is a liveness check on an
/// already-established connection, not a fresh handshake.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// A successful [`WarmStandby::promote`]: the resumed connection and data
/// stream (ready for raw pass-through, exactly like a fresh `HELLO`/`ACK`'d
/// or `reconnect_and_resume`d connection), plus the offsets the server
/// reported so the caller knows what it may safely discard from its own C2H
/// replay buffer.
///
/// Literally [`ResumeAckOutcome`]: this used to be a separate struct with
/// the same four fields, differing only in lacking
/// [`ResumeAckOutcome::network_rebinder`]. Since both are produced by the
/// one shared [`resume_on_connection`] and describe the same thing ("a
/// connection that has just completed a RESUME"), they are now one type —
/// promotion simply always reports `network_rebinder: None`, because it
/// resumes onto a connection whose endpoint it holds no rebinder for. The
/// name is kept as an alias so `promote`'s signature still reads as what it
/// means at the call site.
pub type PromotedConnection = ResumeAckOutcome;

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

/// One established standby: the connection plus its hold stream (the
/// connection's first stream, carrying `STANDBY_HOLD` and then the liveness
/// pings — see `isekai_protocol::standby`). Shared via `Arc` so a probe can
/// run without holding [`WarmStandby`]'s slot lock (a concurrent
/// [`WarmStandby::promote`] must never wait behind a probe).
struct HeldStandby {
    conn: AnyMuxConnection,
    hold: Mutex<AnyByteStream>,
}

/// Why establishing the hold failed — kept separate from [`TransportError`]
/// only so [`WarmStandby::ensure_warm`] can tell "this server predates
/// `STANDBY_HOLD`" (stop dialing standbys against it entirely) apart from
/// every ordinary transient failure (retry on the next tick).
enum HoldError {
    ServerUnsupported,
    Transport(TransportError),
}

impl From<TransportError> for HoldError {
    fn from(e: TransportError) -> Self {
        Self::Transport(e)
    }
}

impl From<MuxError> for HoldError {
    fn from(e: MuxError) -> Self {
        Self::Transport(TransportError::Mux(e))
    }
}

/// Resets `promoting` when dropped, so a cancelled [`WarmStandby::promote`]
/// future (e.g. the caller's `select!`/timeout dropping it mid-resume)
/// can't leave every later promotion permanently rejected with
/// [`WarmStandbyError::AlreadyPromoting`].
struct PromotingGuard<'a>(&'a AtomicBool);

impl Drop for PromotingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
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
    standby: Mutex<Option<Arc<HeldStandby>>>,
    /// `promote`'s single-flight guard — see this module's docs on why this
    /// is a client-side efficiency/clarity measure, not the sole
    /// correctness backstop against a double-promotion.
    promoting: AtomicBool,
    /// Set once the server answered `STANDBY_HOLD` with
    /// `FRAME_REJECT_UNSUPPORTED` (an `isekai-pipe serve` that predates it):
    /// such a server closes any standby after its `HELLO_TIMEOUT`, so
    /// dialing one every tick would only burn (possibly metered) traffic.
    server_unsupported: AtomicBool,
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
        Self::build(factory, target, session_id, None)
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
        Self::build(factory, target, session_id, Some(interface))
    }

    fn build(factory: AnyMuxFactory, target: RelayTarget, session_id: SessionId, interface: Option<InterfaceIndex>) -> Self {
        Self {
            factory,
            target,
            session_id,
            interface,
            standby: Mutex::new(None),
            promoting: AtomicBool::new(false),
            server_unsupported: AtomicBool::new(false),
        }
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

    /// Unconditionally discards the current standby connection, if any,
    /// without probing it — the caller's own `ensure_warm` on the next tick
    /// then dials a completely fresh one. For use when the caller has
    /// independent evidence (e.g. a wall-clock jump across a host suspend/
    /// resume — see `resume_loop.rs`'s warm-standby task) that the existing
    /// standby, even if it would still pass [`WarmStandby::ensure_warm`]'s
    /// own probe, is more likely a zombie than a genuinely healthy
    /// connection (ADR_SLEEP_RESUME_MUX_OWNER_DEATH.md D-3): a standby dialed
    /// *before* a suspend has zero value and negative value if promoted
    /// (it can win the server's park race against a healthy resume attempt —
    /// see that ADR's RC-3/RC-4). `probe`'s own liveness check cannot catch
    /// this case — a zombie connection is, by definition, one that still
    /// looks alive to both sides.
    pub async fn invalidate(&self) {
        *self.standby.lock().await = None;
    }

    /// (Re-)establishes the standby connection if it isn't already warm and
    /// responsive. Call this periodically — see this module's docs for the
    /// agreed keepalive tiering — rather than only once at startup, both to
    /// keep NAT mappings alive on a metered tethering path and to catch a
    /// standby that died before [`WarmStandby::promote`] actually needs it
    /// (discovering that *during* a failover, with the primary already
    /// dead, would leave the caller with no viable path at all).
    ///
    /// Never holds the standby slot's lock across the probe or the dial, so
    /// a concurrent [`WarmStandby::promote`] is never delayed by either; the
    /// dial (including the `STANDBY_HOLD` handshake) is bounded by
    /// [`TRANSPORT_STEP_TIMEOUT`].
    ///
    /// Returns `Ok(())` without dialing when the server has already told us
    /// it can't hold a standby (`isekai-pipe serve` older than
    /// `isekai_protocol::standby`) — [`WarmStandby::is_warm`] stays `false`
    /// and [`WarmStandby::promote`] reports [`WarmStandbyError::NoStandby`],
    /// so the caller falls back to an ordinary resume as before.
    pub async fn ensure_warm(&self) -> Result<(), TransportError> {
        if self.server_unsupported.load(Ordering::SeqCst) {
            return Ok(());
        }
        let current = self.standby.lock().await.clone();
        if let Some(held) = current {
            match probe(&held).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    warn!("warm_standby: standby probe failed ({e}), re-establishing");
                    let mut slot = self.standby.lock().await;
                    // Only clear the slot if it still holds the standby we
                    // just probed — a promote (or another ensure_warm) may
                    // have replaced/consumed it meanwhile.
                    if slot.as_ref().is_some_and(|s| Arc::ptr_eq(s, &held)) {
                        *slot = None;
                    }
                }
            }
        }

        let held = match tokio::time::timeout(TRANSPORT_STEP_TIMEOUT, self.dial_and_hold()).await {
            Err(_) => return Err(TransportError::TimedOut { stage: "warm_standby::ensure_warm: dial" }),
            Ok(Err(HoldError::ServerUnsupported)) => {
                info!(
                    "warm_standby: {} does not support STANDBY_HOLD (older isekai-pipe serve); warm standby disabled for this session",
                    self.target.helper_addr
                );
                self.server_unsupported.store(true, Ordering::SeqCst);
                return Ok(());
            }
            Ok(Err(HoldError::Transport(e))) => return Err(e),
            Ok(Ok(held)) => held,
        };
        info!("warm_standby: standby connection established to {}", self.target.helper_addr);
        let mut slot = self.standby.lock().await;
        if slot.is_none() {
            *slot = Some(Arc::new(held));
        }
        Ok(())
    }

    /// Binds (either OS-default or, if [`WarmStandby::new_bound_to_interface`]
    /// was used, restricted to `self.interface`) a fresh local socket and
    /// dials `self.target`, via [`crate::physical_interface::connect_via_interface`].
    /// Also applies `self.target.local_bind_port_range`, the same
    /// narrowed-outbound-port-range knob `relay.rs`/`race.rs`/`resume.rs`
    /// already honor for their own dials — previously silently ignored here.
    async fn dial(&self) -> Result<AnyMuxConnection, TransportError> {
        // `connect_via_interface` takes the port range separately (not a
        // full `BindSpec`, unlike this crate's other dial sites) since it
        // still needs to compute its own bind address matching `remote`'s
        // IPv4/IPv6 family (`unspecified_addr_for`) — so only the
        // `RemoteSpec` half of `RelayTarget`'s two dial-preamble helpers
        // applies here; `self.target.local_bind_port_range` is passed
        // through as-is, unchanged from before.
        crate::physical_interface::connect_via_interface(
            &self.factory,
            self.interface,
            self.target.remote_spec(),
            self.target.local_bind_port_range,
        )
        .await
    }

    /// Dials a fresh connection and turns it into a held standby: sends
    /// `STANDBY_HOLD` (+ proof) as the connection's first frame so the
    /// server exempts it from its first-frame `HELLO_TIMEOUT`, and waits for
    /// `STANDBY_READY`. Without this the real `isekai-pipe serve` closes a
    /// bare standby connection ~5s after it's dialed (its guard against
    /// "handshake then sit on it" peers), so promotion only ever worked
    /// within those first 5s.
    async fn dial_and_hold(&self) -> Result<HeldStandby, HoldError> {
        let conn = self.dial().await?;
        let proof = compute_proof(&conn, &self.target.session_secret, STANDBY_HOLD_PROOF_DOMAIN).await?;
        let mut hold = conn.open_bi().await?;
        hold.write_all(&encode_standby_hold(proof.as_bytes())).await?;
        let mut resp = [0u8; 1];
        if hold.read(&mut resp).await? == 0 {
            return Err(HoldError::Transport(TransportError::UnexpectedEof));
        }
        match resp[0] {
            FRAME_STANDBY_READY => Ok(HeldStandby { conn, hold: Mutex::new(hold) }),
            isekai_protocol::hello::FRAME_REJECT_UNSUPPORTED => Err(HoldError::ServerUnsupported),
            isekai_protocol::hello::FRAME_REJECT_AUTH => {
                Err(HoldError::Transport(TransportError::Rejected(isekai_protocol::attach::AttachRejectReason::Auth)))
            }
            other => Err(HoldError::Transport(TransportError::Protocol(isekai_protocol::ProtocolError::UnknownFrameType(other)))),
        }
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
        let _guard = PromotingGuard(&self.promoting);
        self.promote_inner(client_sent_offset, client_delivered_offset).await
    }

    async fn promote_inner(
        &self,
        client_sent_offset: C2hSentOffset,
        client_delivered_offset: H2cClientDeliveredOffset,
    ) -> Result<PromotedConnection, WarmStandbyError> {
        let held = self.standby.lock().await.take().ok_or(WarmStandbyError::NoStandby)?;

        // Identical to what `resume::reconnect_and_resume` does once *it* has
        // a connection in hand — same proof scheme (see `resume_on_connection`
        // for why `session_id` is mixed in), same error mapping, same offset
        // bookkeeping. The only difference is that this connection was kept
        // warm rather than freshly dialed, so there is no endpoint rebinder
        // to hand back. The RESUME goes out on a *second* stream (the server
        // hands exactly that stream to its ordinary RESUME handling);
        // `held` (and with it the hold stream) stays alive until the resume
        // completes, so the server never sees the hold end before the
        // promotion stream arrives.
        let promoted = resume_on_connection(
            held.conn.clone(),
            &self.target.session_secret,
            self.session_id,
            client_sent_offset,
            client_delivered_offset,
            None,
            "warm_standby::promote: request_resume",
        )
        .await?;
        drop(held);

        info!(
            "warm_standby: promoted standby, session_id={}, helper_committed_offset={}",
            self.session_id, promoted.helper_committed_offset
        );
        Ok(promoted)
    }
}

/// One `STANDBY_PING`/`STANDBY_PONG` round trip on the hold stream, bounded
/// by [`PROBE_TIMEOUT`] — the backend-agnostic liveness check this module
/// uses in place of `path_health.rs`'s `noq`-specific ping/stats mechanism
/// (see this module's top docs for why that doesn't apply here). Unlike the
/// previous "open a stream and shut it down" probe, this is answered by the
/// server application itself, so it fails as soon as the server has given
/// up on the connection — and it never burns a stream (the server accepts
/// exactly one promotion stream per standby).
async fn probe(held: &HeldStandby) -> Result<(), MuxError> {
    tokio::time::timeout(PROBE_TIMEOUT, async {
        let mut hold = held.hold.lock().await;
        hold.write_all(&[FRAME_STANDBY_PING]).await?;
        let mut resp = [0u8; 1];
        match hold.read(&mut resp).await? {
            0 => Err(MuxError::TransportLost { reason: "standby hold stream closed by server".to_string(), retryable: true }),
            _ if resp[0] == FRAME_STANDBY_PONG => Ok(()),
            _ => Err(MuxError::StreamIo(format!("unexpected standby probe response {:#x}", resp[0]))),
        }
    })
    .await
    .map_err(|_| MuxError::TransportLost { reason: "standby probe timed out".to_string(), retryable: true })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::system_quic_factory;
    use isekai_protocol::standby::{decode_standby_hold, FRAME_STANDBY_HOLD, STANDBY_HOLD_FRAME_LEN};
    use quicmux::{AnyByteStreamReadHalf, AnyMuxListener, MuxServerConfig};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::atomic::AtomicUsize;

    /// The mock's stand-in for `isekai-pipe serve`'s `HELLO_TIMEOUT`:
    /// deliberately much shorter than the real 5s so the "standby survives
    /// past the first-frame deadline" tests stay fast, but enforced exactly
    /// the way the real server enforces it (a connection whose first stream
    /// doesn't deliver its frame type in time is dropped/closed).
    const MOCK_FIRST_FRAME_DEADLINE: Duration = Duration::from_millis(400);

    fn test_server_config() -> (MuxServerConfig, String) {
        let (mut config, cert_sha256_hex) = quicmux::test_support::self_signed_server_config("isekai-pipe.local");
        config.alpn = isekai_protocol::hello::ALPN.to_vec();
        config.exporter_label = isekai_protocol::hello::EXPORTER_LABEL.to_vec();
        config.max_concurrent_bidi_streams = 3;
        (config, cert_sha256_hex)
    }

    fn hmac(session_secret: &[u8; 32], exporter: &[u8], extra: &[u8]) -> [u8; 32] {
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(session_secret).unwrap();
        mac.update(exporter);
        mac.update(extra);
        mac.finalize().into_bytes().into()
    }

    async fn read_exact(recv: &mut AnyByteStreamReadHalf, buf: &mut [u8]) -> bool {
        let mut filled = 0;
        while filled < buf.len() {
            match recv.read(&mut buf[filled..]).await {
                Ok(0) | Err(_) => return false,
                Ok(n) => filled += n,
            }
        }
        true
    }

    /// Answers a RESUME whose type byte has already been read off `recv`.
    async fn answer_resume(
        conn: &AnyMuxConnection,
        mut recv: AnyByteStreamReadHalf,
        mut send: quicmux::AnyByteStreamWriteHalf,
        session_secret: &[u8; 32],
    ) {
        let exporter = conn.export_keying_material(isekai_protocol::hello::EXPORTER_LABEL, b"").await.unwrap();
        let request = quicmux::decode_resume_request(&mut recv, exporter).await.unwrap();
        if request.auth_blob != hmac(session_secret, &exporter, &request.token) {
            quicmux::respond_resume_rejected(&mut send, quicmux::ResumeRejectReason::Auth).await;
            return;
        }
        quicmux::respond_resume_accepted(&mut send, 100, 200, b"promoted-replay").await.unwrap();
        // Keep the resumed stream (and so the connection) alive for the test.
        let mut sink = [0u8; 64];
        while matches!(recv.read(&mut sink).await, Ok(n) if n > 0) {}
    }

    /// One connection, handled the way the real `isekai-pipe serve`
    /// `handle_connection` + `standby_hold::hold_until_resume_stream` do:
    /// the first stream's frame type must arrive within
    /// [`MOCK_FIRST_FRAME_DEADLINE`] or the connection is dropped (closed);
    /// `FRAME_STANDBY_HOLD` (if `supports_hold`) is exempt from that and
    /// then serves pings until a second stream brings the RESUME; anything
    /// else is rejected with `FRAME_REJECT_UNSUPPORTED` and the connection
    /// dropped.
    async fn serve_like_real_server(conn: AnyMuxConnection, session_secret: [u8; 32], supports_hold: bool) {
        let first = tokio::time::timeout(MOCK_FIRST_FRAME_DEADLINE, async {
            let stream = conn.accept_bi().await.ok()?;
            let (mut recv, send) = stream.split();
            let mut type_byte = [0u8; 1];
            read_exact(&mut recv, &mut type_byte).await.then_some((recv, send, type_byte[0]))
        })
        .await;
        let Ok(Some((mut recv, mut send, frame_type))) = first else { return };

        match frame_type {
            quicmux::FRAME_RESUME => answer_resume(&conn, recv, send, &session_secret).await,
            FRAME_STANDBY_HOLD if supports_hold => {
                let mut frame = [0u8; STANDBY_HOLD_FRAME_LEN];
                frame[0] = FRAME_STANDBY_HOLD;
                if !read_exact(&mut recv, &mut frame[1..]).await {
                    return;
                }
                let proof = decode_standby_hold(&frame).unwrap();
                let exporter = conn.export_keying_material(isekai_protocol::hello::EXPORTER_LABEL, b"").await.unwrap();
                if proof != hmac(&session_secret, &exporter, STANDBY_HOLD_PROOF_DOMAIN) {
                    let _ = send.write_all(&[isekai_protocol::hello::FRAME_REJECT_AUTH]).await;
                    return;
                }
                send.write_all(&[FRAME_STANDBY_READY]).await.unwrap();
                loop {
                    let mut byte = [0u8; 1];
                    tokio::select! {
                        biased;
                        stream = conn.accept_bi() => {
                            let Ok(stream) = stream else { return };
                            let (mut rrecv, rsend) = stream.split();
                            let mut t = [0u8; 1];
                            if !read_exact(&mut rrecv, &mut t).await || t[0] != quicmux::FRAME_RESUME {
                                return;
                            }
                            answer_resume(&conn, rrecv, rsend, &session_secret).await;
                            return;
                        }
                        read = async { recv.read(&mut byte).await.map(|n| (n, byte[0])) } => {
                            match read {
                                Ok((n, b)) if n > 0 && b == FRAME_STANDBY_PING => {
                                    send.write_all(&[FRAME_STANDBY_PONG]).await.unwrap();
                                }
                                _ => return,
                            }
                        }
                    }
                }
            }
            _ => {
                let _ = send.write_all(&[isekai_protocol::hello::FRAME_REJECT_UNSUPPORTED]).await;
                let _ = send.shutdown().await;
                let _ = tokio::time::timeout(Duration::from_secs(2), send.wait_for_close()).await;
            }
        }
    }

    struct MockServer {
        addr: SocketAddr,
        cert_sha256_hex: String,
        session_secret: [u8; 32],
        session_id: SessionId,
        connections: Arc<AtomicUsize>,
    }

    impl MockServer {
        fn target(&self) -> RelayTarget {
            RelayTarget {
                helper_addr: self.addr,
                server_name: "isekai-pipe.local".to_string(),
                cert_sha256_hex: self.cert_sha256_hex.clone(),
                session_secret: self.session_secret.to_vec(),
                local_bind_port_range: None,
            }
        }
    }

    async fn spawn_mock_server(supports_hold: bool) -> MockServer {
        let (server_config, cert_sha256_hex) = test_server_config();
        let listener = AnyMuxListener::bind_noq(server_config, quicmux::BindSpec::any_ipv4()).await.unwrap();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listener.local_addr().unwrap().port());

        let session_secret: [u8; 32] = rand::random();
        let session_id = SessionId::from_bytes(rand::random());
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = connections.clone();

        tokio::spawn(async move {
            loop {
                let Some(incoming) = listener.accept().await else { break };
                let Ok(conn) = incoming.accept().await else { continue };
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(serve_like_real_server(conn, session_secret, supports_hold));
            }
        });

        MockServer { addr, cert_sha256_hex, session_secret, session_id, connections }
    }

    /// Regression test for the standby being silently closed by the server's
    /// first-frame deadline: the standby must still probe healthy and be
    /// promotable well after that deadline, without ever being re-dialed.
    #[tokio::test]
    async fn standby_survives_the_servers_first_frame_deadline_and_promotes() {
        let server = spawn_mock_server(true).await;
        let standby = WarmStandby::new(system_quic_factory(), server.target(), server.session_id);
        standby.ensure_warm().await.expect("ensure_warm should succeed");

        tokio::time::sleep(MOCK_FIRST_FRAME_DEADLINE * 3).await;
        standby.ensure_warm().await.expect("the held standby should still pass its probe");
        assert_eq!(server.connections.load(Ordering::SeqCst), 1, "a healthy held standby must not be re-dialed");

        let promoted = standby
            .promote(C2hSentOffset::new(300), H2cClientDeliveredOffset::new(190))
            .await
            .expect("promote should succeed on a standby older than the first-frame deadline");
        assert_eq!(promoted.helper_committed_offset.get(), 100);
        let mut stream = promoted.data_stream;
        let mut buf = [0u8; 32];
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"promoted-replay");
    }

    /// An `isekai-pipe serve` that predates `STANDBY_HOLD` rejects it; the
    /// standby must then be disabled (no re-dial every tick on a possibly
    /// metered path) rather than treated as an error forever.
    #[tokio::test]
    async fn an_older_server_without_standby_hold_disables_the_standby_instead_of_redialing() {
        let server = spawn_mock_server(false).await;
        let standby = WarmStandby::new(system_quic_factory(), server.target(), server.session_id);
        standby.ensure_warm().await.expect("an unsupported server is not an ensure_warm error");
        assert!(!standby.is_warm().await);
        standby.ensure_warm().await.unwrap();
        standby.ensure_warm().await.unwrap();
        assert_eq!(server.connections.load(Ordering::SeqCst), 1, "must stop dialing once the server said it can't hold");
        assert!(matches!(
            standby.promote(C2hSentOffset::new(0), H2cClientDeliveredOffset::new(0)).await,
            Err(WarmStandbyError::NoStandby)
        ));
    }

    /// A promote future dropped mid-flight must not leave the single-flight
    /// guard stuck (every later promote would otherwise fail with
    /// `AlreadyPromoting` forever).
    #[tokio::test]
    async fn a_cancelled_promote_does_not_leave_the_single_flight_guard_set() {
        let server = spawn_mock_server(true).await;
        let standby = WarmStandby::new(system_quic_factory(), server.target(), server.session_id);
        standby.ensure_warm().await.unwrap();
        {
            // The first poll takes the standby and parks on the RESUME round
            // trip; the zero timeout then drops the future mid-flight.
            let fut = standby.promote(C2hSentOffset::new(0), H2cClientDeliveredOffset::new(0));
            tokio::pin!(fut);
            // Poll once, then drop.
            let _ = futures_poll_once(fut.as_mut()).await;
        }
        assert!(!standby.promoting.load(Ordering::SeqCst));
    }

    async fn futures_poll_once<F: std::future::Future + Unpin>(fut: F) -> Option<F::Output> {
        tokio::time::timeout(Duration::from_millis(0), fut).await.ok()
    }

    #[tokio::test]
    async fn ensure_warm_then_promote_succeeds_without_a_fresh_dial() {
        let server = spawn_mock_server(true).await;
        let standby = WarmStandby::new(system_quic_factory(), server.target(), server.session_id);
        assert!(!standby.is_warm().await);
        standby.ensure_warm().await.expect("ensure_warm should succeed");
        assert!(standby.is_warm().await);

        let promoted = standby
            .promote(C2hSentOffset::new(300), H2cClientDeliveredOffset::new(190))
            .await
            .expect("promote should succeed against the already-warm standby");
        assert_eq!(promoted.helper_committed_offset.get(), 100);
        assert_eq!(promoted.helper_sent_offset.get(), 200);
        assert_eq!(server.connections.load(Ordering::SeqCst), 1, "promotion must reuse the standby, not dial");

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

        let server = spawn_mock_server(true).await;
        let standby = WarmStandby::new_bound_to_interface(system_quic_factory(), server.target(), server.session_id, loopback);
        standby.ensure_warm().await.expect("ensure_warm should succeed when bound to the loopback interface");
        assert!(standby.is_warm().await);

        let promoted = standby.promote(C2hSentOffset::new(0), H2cClientDeliveredOffset::new(0)).await.expect("promote should succeed");
        assert_eq!(promoted.helper_committed_offset.get(), 100);
    }

    // Windows-only: confirmed on a real `test-windows` CI run that binding to
    // a bogus interface index doesn't fail eagerly there (see
    // `physical_interface::tests::bogus_interface_index_fails_rather_than_panicking`'s
    // comment) — `ensure_warm` still fails overall, just later and as a QUIC
    // idle-timeout `TransportError::Mux(TransportLost { .. })` once the
    // handshake can't actually route, not as an immediate `MuxError::Bind`.
    #[tokio::test]
    #[cfg(not(windows))]
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
        let server = spawn_mock_server(true).await;
        let standby = std::sync::Arc::new(WarmStandby::new(system_quic_factory(), server.target(), server.session_id));
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
