//! ATTACH v2-specific wrapper around `quicmux::warm_standby::WarmStandby` —
//! keeps a second, independent connection pre-established and periodically
//! probed, then resumes onto it (via `quicmux::request_resume`) when the
//! primary connection dies. The generic dial/probe/single-flight-promotion
//! mechanics moved to `quicmux::warm_standby` (2026-07-24) because they
//! don't reference anything isekai-specific; this module supplies exactly
//! the isekai-specific part `quicmux::warm_standby::WarmStandby::promote`'s
//! `on_promote` closure needs: the ATTACH v2 resume proof/offsets.
//!
//! Built specifically for PC (Windows/macOS) Wi-Fi + USB/Bluetooth
//! tethering warm-standby (motivating design discussion recorded as this
//! session's `pc-tethering-warm-standby-design` memory). See
//! `quicmux::warm_standby`'s own module docs for why this sidesteps `noq`'s
//! native multipath (issue #738) instead of using it.

use isekai_protocol::offset::{C2hHelperCommittedOffset, C2hSentOffset, H2cClientDeliveredOffset, H2cSentOffset};
use isekai_protocol::session_id::SessionId;
use quicmux::warm_standby::{WarmStandbyError as MuxWarmStandbyError, PROBE_TIMEOUT as MUX_PROBE_TIMEOUT};
use quicmux::{AnyByteStream, AnyMuxConnection, AnyMuxFactory, RemoteSpec};

use crate::physical_interface::InterfaceIndex;

use crate::error::TransportError;
use crate::proof::compute_proof;
use crate::relay::RelayTarget;
use crate::resume::map_reject_reason;

/// Re-exported so existing callers of this module don't need to reach into
/// `quicmux` directly for the same constant.
pub const PROBE_TIMEOUT: std::time::Duration = MUX_PROBE_TIMEOUT;

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
    /// Another [`WarmStandby::promote`] call is already in flight.
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

impl From<MuxWarmStandbyError<TransportError>> for WarmStandbyError {
    fn from(e: MuxWarmStandbyError<TransportError>) -> Self {
        match e {
            MuxWarmStandbyError::AlreadyPromoting => WarmStandbyError::AlreadyPromoting,
            MuxWarmStandbyError::NoStandby => WarmStandbyError::NoStandby,
            MuxWarmStandbyError::Promote(err) => WarmStandbyError::Transport(err),
        }
    }
}

/// Holds a pre-established, periodically-probed standby connection to the
/// same `target` a primary connection is already using, and promotes it to
/// a resumed data stream on demand via ATTACH v2's `quicmux::resume`. See
/// this module's docs and `quicmux::warm_standby`'s for the full design.
pub struct WarmStandby {
    inner: quicmux::warm_standby::WarmStandby,
    target: RelayTarget,
    session_id: SessionId,
}

impl WarmStandby {
    /// Builds a `WarmStandby` with no standby connection yet — call
    /// [`WarmStandby::ensure_warm`] at least once (and then periodically)
    /// before relying on [`WarmStandby::promote`] to succeed. Every standby
    /// connection is dialed with OS-default routing — for a warm-standby
    /// path that must specifically prove a *particular* physical interface
    /// (e.g. a USB/Bluetooth tethering adapter) is viable, use
    /// [`WarmStandby::new_bound_to_interface`] instead.
    pub fn new(factory: AnyMuxFactory, target: RelayTarget, session_id: SessionId) -> Self {
        let remote = remote_spec(&target);
        let inner = quicmux::warm_standby::WarmStandby::new(factory, remote, target.local_bind_port_range);
        Self { inner, target, session_id }
    }

    /// Same as [`WarmStandby::new`], but every standby connection
    /// [`WarmStandby::ensure_warm`] establishes is bound to `interface`
    /// specifically — probing "any" interface doesn't prove the specific
    /// tethering path is actually viable, which is the whole point of
    /// keeping it warm.
    ///
    /// `noq`-only: `qmux` has no bound-UDP-socket concept to restrict this
    /// way — using this constructor with a `qmux`-backed `factory` means
    /// every [`WarmStandby::ensure_warm`] call fails with
    /// `quicmux::MuxError::Unsupported`.
    pub fn new_bound_to_interface(factory: AnyMuxFactory, target: RelayTarget, session_id: SessionId, interface: InterfaceIndex) -> Self {
        let remote = remote_spec(&target);
        let inner = quicmux::warm_standby::WarmStandby::new_bound_to_interface(factory, remote, target.local_bind_port_range, interface);
        Self { inner, target, session_id }
    }

    /// Whether a standby connection is currently held — see
    /// `quicmux::warm_standby::WarmStandby::is_warm`'s docs.
    pub async fn is_warm(&self) -> bool {
        self.inner.is_warm().await
    }

    /// (Re-)establishes the standby connection if it isn't already warm and
    /// responsive — see `quicmux::warm_standby::WarmStandby::ensure_warm`'s
    /// docs.
    pub async fn ensure_warm(&self) -> Result<(), TransportError> {
        self.inner.ensure_warm().await.map_err(TransportError::Mux)
    }

    /// Promotes the standby connection: issues a `quicmux::resume` RESUME
    /// request directly on it (no fresh dial) and returns the resumed data
    /// stream.
    pub async fn promote(
        &self,
        client_sent_offset: C2hSentOffset,
        client_delivered_offset: H2cClientDeliveredOffset,
    ) -> Result<PromotedConnection, WarmStandbyError> {
        let target = &self.target;
        let session_id = self.session_id;
        self.inner
            .promote(move |conn| async move {
                // Same proof scheme `resume::reconnect_and_resume` uses — see
                // that function's identical comment for why `session_id` is
                // mixed in.
                let resume_proof = compute_proof(&conn, &target.session_secret, session_id.as_bytes()).await?;

                let outcome = quicmux::request_resume(
                    &conn,
                    session_id.as_bytes(),
                    resume_proof.as_bytes(),
                    client_sent_offset.get(),
                    client_delivered_offset.get(),
                )
                .await
                .map_err(|e| match e {
                    quicmux::ResumeRequestError::Mux(mux_err) => TransportError::Mux(mux_err),
                    quicmux::ResumeRequestError::Rejected(reason) => TransportError::ResumeRejected(map_reject_reason(reason)),
                })?;

                log::info!("warm_standby: promoted standby, session_id={session_id}, helper_committed_offset={}", outcome.committed_offset);
                Ok::<_, TransportError>(PromotedConnection {
                    connection: conn,
                    data_stream: outcome.stream,
                    helper_committed_offset: C2hHelperCommittedOffset::new(outcome.committed_offset),
                    helper_sent_offset: H2cSentOffset::new(outcome.sent_offset),
                })
            })
            .await
            .map_err(WarmStandbyError::from)
    }
}

fn remote_spec(target: &RelayTarget) -> RemoteSpec {
    RemoteSpec { addr: target.helper_addr, server_name: target.server_name.clone(), cert_sha256_hex: target.cert_sha256_hex.clone() }
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
            max_idle_timeout: std::time::Duration::from_secs(15),
            keep_alive_interval: std::time::Duration::from_secs(5),
            max_concurrent_bidi_streams: 4,
            max_concurrent_uni_streams: 0,
            multipath: false,
            datagram_send_buffer_size: None,
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

    // Windows-only: confirmed on a real `test-windows` CI run that binding to
    // a bogus interface index doesn't fail eagerly there — `ensure_warm`
    // still fails overall, just later and as a QUIC idle-timeout
    // `TransportError::Mux(TransportLost { .. })` once the handshake can't
    // actually route, not as an immediate `MuxError::Bind`.
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
        assert!(matches!(err, TransportError::Mux(quicmux::MuxError::Bind { .. })), "expected a Bind error, got {err:?}");
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
}
