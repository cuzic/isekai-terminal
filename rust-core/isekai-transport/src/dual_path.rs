//! Two independently-dialed [`AnyMuxConnection`]s to the same logical peer —
//! one designated for reliable (ordered, stream-based) traffic and one for
//! unreliable (datagram-based) traffic — each optionally bound to its own
//! physical network interface, so the app can spread its two traffic
//! classes across two separate physical links (e.g. Wi-Fi for command/
//! control, cellular for best-effort telemetry/video) and pick explicitly
//! which one to use for a given piece of data
//! (`uav-oss-transport-work-and-reliable-unreliable-path-plan` session memory).
//!
//! # Why not one connection with per-frame path selection
//!
//! `noq::Connection::send_datagram`/`open_bi` have no per-frame or
//! per-stream "use this path" parameter — which physical path a given frame
//! rides on inside a single multipath connection is decided entirely by
//! `noq`'s own internal scheduler. Steering individual traffic classes onto
//! specific physical interfaces would mean forking `noq`'s packet scheduler/
//! congestion control, which this project has already decided against once
//! for the same reason `noq` issue #738 (same-connection simultaneous
//! physical-interface paths) was abandoned — see [`crate::path_health`] and
//! [`crate::warm_standby`]'s module docs. This module sidesteps the whole
//! question by using two genuinely separate connections instead, exactly
//! like `warm_standby.rs` already does for its own (different) purpose.
//!
//! # Relationship to `warm_standby`/`multipath`/`path_health`
//!
//! `warm_standby.rs` established the "dial an [`AnyMuxConnection`], optionally
//! restricted to one physical interface via
//! [`crate::physical_interface::bind_physical_interface`]" pattern for its
//! own primary/standby *failover* use case (this project's
//! `pc-tethering-warm-standby-design` memory's "two independent QUIC
//! connections" design). This module reuses the exact same dial mechanics
//! (factored out to [`crate::physical_interface::connect_via_interface`])
//! for a different purpose: not a hot spare that only takes over when a
//! primary dies, but two connections **both actively carrying live traffic
//! at the same time**, differentiated by delivery semantics rather than by
//! primary/standby role. Unlike `multipath.rs`/`path_health.rs` (multiple
//! *paths inside one `noq::Connection`*, `noq`-concrete and unaware of
//! `qmux`), this module works through `quicmux::AnyMuxConnection` throughout
//! and is backend-agnostic — the `reliable` and `unreliable` connections
//! could even use different backends if some future caller needed that.
//!
//! # Scope
//!
//! - Establishing both connections ([`connect_dual_path`], or
//!   [`connect_dual_path_best_effort`] for partial-success semantics) and
//!   offering convenience methods for the canonical pairing (streams on
//!   `reliable`, datagrams on `unreliable`) is all this module does.
//! - **Liveness monitoring and reconnection are the caller's job.** Unlike
//!   `warm_standby.rs` (which owns its own probe-and-promote lifecycle),
//!   this module has no periodic health check and no automatic redial — the
//!   caller already drives its own read/write loop on each connection and is
//!   the first to notice a `MuxError`; recovering (calling
//!   [`crate::physical_interface::connect_via_interface`] again for just the
//!   affected side) is left entirely to it, exactly as `warm_standby.rs`'s
//!   own docs draw the same line for primary-failure detection.
//! - Nothing stops a caller from also opening streams on the `unreliable`
//!   connection or sending datagrams on the `reliable` one — both fields of
//!   [`DualPathConnections`] are plain public [`AnyMuxConnection`]s. The
//!   "reliable"/"unreliable" naming is a policy convention this module
//!   encodes via its convenience methods, not a hard capability restriction.
//!   Those convenience methods are deliberately part of this module's first
//!   cut (not added speculatively after the fact) because "the app picks
//!   explicitly which one to use" was the actual ask this module exists to
//!   satisfy — not just "hold two connections."

use quicmux::{AnyByteStream, AnyMuxConnection, AnyMuxFactory, RemoteSpec};

use crate::error::TransportError;
use crate::physical_interface::{connect_via_interface, InterfaceIndex};

/// Everything needed to dial one side of a [`DualPathConnections`] pair:
/// which factory (and therefore which `quicmux::MuxClientConfig` — ALPN,
/// datagram support, stream limits, etc.) to use, which remote address to
/// connect to, which physical interface (if any) to bind to first, and
/// (optionally) which local outbound UDP port range to restrict the bind
/// to. Kept as its own type (rather than four loose parameters) so the two
/// sides of [`connect_dual_path`] read symmetrically at the call site.
pub struct DualPathEndpoint {
    pub factory: AnyMuxFactory,
    pub remote: RemoteSpec,
    /// `None` dials with OS-default routing; `Some` restricts the dial to
    /// that specific physical interface via
    /// [`crate::physical_interface::bind_physical_interface`] — see that
    /// function's docs and `WarmStandby::new_bound_to_interface`'s identical
    /// `noq`-only caveat (binding a `qmux`-backed factory this way always
    /// fails with `MuxError::Unsupported`).
    pub interface: Option<InterfaceIndex>,
    /// Narrows the local outbound UDP port to this inclusive range instead
    /// of an OS-assigned ephemeral port, the same knob this crate's other
    /// dial paths (`relay.rs`/`race.rs`/`resume.rs`) already expose via
    /// `quicmux::BindSpec::with_port_range` — e.g. so a caller behind a
    /// restrictive local firewall/NAT can permit a narrow range explicitly.
    /// `None` keeps OS-default ephemeral-port behavior.
    pub port_range: Option<(u16, u16)>,
}

/// Two independently-dialed, simultaneously-held connections to the same
/// peer — see this module's docs for the full design. Both fields are
/// ordinary [`AnyMuxConnection`]s; nothing but convention (and this struct's
/// own convenience methods) ties "reliable" to streams and "unreliable" to
/// datagrams.
pub struct DualPathConnections {
    pub reliable: AnyMuxConnection,
    pub unreliable: AnyMuxConnection,
}

impl std::fmt::Debug for DualPathConnections {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DualPathConnections").finish_non_exhaustive()
    }
}

/// Dials [`DualPathConnections::reliable`] and [`DualPathConnections::unreliable`]
/// concurrently (not sequentially — with two genuinely independent physical
/// interfaces there is no reason to pay one dial's latency before starting
/// the other). All-or-nothing: if either dial fails, the other is dropped
/// and this returns that failure — neither connection is any use without
/// the other to a caller that specifically wants *both* traffic classes
/// available. Dropping the losing side here relies on `quicmux`/`noq`'s
/// cooperative-shutdown model (the same one `race.rs::race_with_stagger`
/// already documents and relies on for its own loser-is-dropped policy):
/// its underlying task(s) are woken and exit on their *next* poll rather
/// than being aborted synchronously, so a dropped dial's resources clear
/// promptly rather than instantly.
///
/// A caller whose traffic classes should each survive independently even
/// when the other's dial fails — plausible for this module's own motivating
/// UAV use case (keep the command/reliable link even if telemetry/unreliable
/// can't dial out) — should call [`connect_dual_path_best_effort`] instead.
pub async fn connect_dual_path(reliable: DualPathEndpoint, unreliable: DualPathEndpoint) -> Result<DualPathConnections, TransportError> {
    let (reliable_conn, unreliable_conn) = tokio::try_join!(
        connect_via_interface(&reliable.factory, reliable.interface, reliable.remote, reliable.port_range),
        connect_via_interface(&unreliable.factory, unreliable.interface, unreliable.remote, unreliable.port_range),
    )?;
    Ok(DualPathConnections { reliable: reliable_conn, unreliable: unreliable_conn })
}

/// Best-effort counterpart to [`connect_dual_path`]: dials both sides
/// concurrently the exact same way, but never cancels one side because the
/// other failed — each side's `Result` is returned independently, so a
/// caller that can operate correctly with only one of the two connections
/// established (e.g. keep the reliable/command link up even though the
/// unreliable/telemetry dial failed) gets that instead of an all-or-nothing
/// failure. This is real, tested infrastructure for that policy rather than
/// callers having to hand-roll the concurrent dial themselves via
/// [`crate::physical_interface::connect_via_interface`].
pub async fn connect_dual_path_best_effort(
    reliable: DualPathEndpoint,
    unreliable: DualPathEndpoint,
) -> (Result<AnyMuxConnection, TransportError>, Result<AnyMuxConnection, TransportError>) {
    tokio::join!(
        async {
            connect_via_interface(&reliable.factory, reliable.interface, reliable.remote, reliable.port_range).await.map_err(TransportError::from)
        },
        async {
            connect_via_interface(&unreliable.factory, unreliable.interface, unreliable.remote, unreliable.port_range).await.map_err(TransportError::from)
        },
    )
}

impl DualPathConnections {
    /// Opens a new bidirectional stream on the `reliable` connection — the
    /// canonical way to send ordered, reliably-delivered data through this
    /// pair.
    pub async fn open_reliable(&self) -> Result<AnyByteStream, TransportError> {
        self.reliable.open_bi().await.map_err(TransportError::Mux)
    }

    /// Accepts a peer-initiated bidirectional stream on the `reliable`
    /// connection.
    pub async fn accept_reliable(&self) -> Result<AnyByteStream, TransportError> {
        self.reliable.accept_bi().await.map_err(TransportError::Mux)
    }

    /// Sends one unreliable, unordered datagram on the `unreliable`
    /// connection — see `AnyMuxConnection::send_datagram`'s docs for the
    /// non-blocking drop policy this inherits.
    pub fn send_unreliable(&self, data: bytes::Bytes) -> Result<(), TransportError> {
        self.unreliable.send_datagram(data).map_err(TransportError::Mux)
    }

    /// Backpressure-aware version of [`Self::send_unreliable`] — see
    /// `AnyMuxConnection::send_datagram_wait`'s docs.
    pub async fn send_unreliable_wait(&self, data: bytes::Bytes) -> Result<(), TransportError> {
        self.unreliable.send_datagram_wait(data).await.map_err(TransportError::Mux)
    }

    /// Receives the next datagram on the `unreliable` connection.
    pub async fn recv_unreliable(&self) -> Result<bytes::Bytes, TransportError> {
        self.unreliable.recv_datagram().await.map_err(TransportError::Mux)
    }

    /// The largest single datagram payload [`Self::send_unreliable`] can
    /// currently send — see `AnyMuxConnection::max_datagram_size`'s docs on
    /// why this must be checked before every send rather than cached.
    pub fn max_unreliable_datagram_size(&self) -> Option<usize> {
        self.unreliable.max_datagram_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quicmux::{AnyMuxListener, MuxClientConfig, MuxError, MuxServerConfig};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Duration;

    const SNI: &str = "isekai-pipe.local";

    fn build_server_config(datagram_send_buffer_size: Option<usize>) -> (MuxServerConfig, String) {
        let cert = rcgen::generate_simple_self_signed(vec![SNI.to_string()]).unwrap();
        let cert_der = cert.cert.der().clone();
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
            datagram_send_buffer_size,
            cert_chain: vec![cert_der],
            private_key: key_der,
        };
        (config, cert_sha256_hex)
    }

    fn build_client_config(datagram_send_buffer_size: Option<usize>) -> MuxClientConfig {
        MuxClientConfig {
            alpn: isekai_protocol::hello::ALPN.to_vec(),
            exporter_label: isekai_protocol::hello::EXPORTER_LABEL.to_vec(),
            max_idle_timeout: Duration::from_secs(15),
            keep_alive_interval: Duration::from_secs(5),
            max_concurrent_bidi_streams: 4,
            max_concurrent_uni_streams: 0,
            multipath: false,
            datagram_send_buffer_size,
        }
    }

    /// The `reliable` side's server: accepts one connection, accepts one
    /// bidirectional stream, and echoes back exactly what it read.
    async fn spawn_reliable_echo_listener() -> (SocketAddr, String) {
        let (server_config, cert_sha256_hex) = build_server_config(None);
        let listener = AnyMuxListener::bind_noq(server_config, quicmux::BindSpec::any_ipv4()).await.unwrap();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            let Some(incoming) = listener.accept().await else { return };
            let Ok(conn) = incoming.accept().await else { return };
            let Ok(mut stream) = conn.accept_bi().await else { return };
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let _ = stream.write_all(&buf[..n]).await;
            let _ = stream.shutdown().await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        (addr, cert_sha256_hex)
    }

    /// The `unreliable` side's server: accepts one connection and echoes
    /// back exactly one datagram.
    async fn spawn_unreliable_echo_listener() -> (SocketAddr, String) {
        let (server_config, cert_sha256_hex) = build_server_config(Some(64 * 1024));
        let listener = AnyMuxListener::bind_noq(server_config, quicmux::BindSpec::any_ipv4()).await.unwrap();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            let Some(incoming) = listener.accept().await else { return };
            let Ok(conn) = incoming.accept().await else { return };
            if let Ok(Ok(datagram)) = tokio::time::timeout(Duration::from_secs(5), conn.recv_datagram()).await {
                let _ = conn.send_datagram(datagram);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        (addr, cert_sha256_hex)
    }

    #[tokio::test]
    async fn reliable_and_unreliable_connections_carry_their_own_traffic_independently() {
        let (reliable_addr, reliable_cert) = spawn_reliable_echo_listener().await;
        let (unreliable_addr, unreliable_cert) = spawn_unreliable_echo_listener().await;

        let dual = connect_dual_path(
            DualPathEndpoint {
                factory: AnyMuxFactory::noq(build_client_config(None)),
                remote: RemoteSpec { addr: reliable_addr, server_name: SNI.to_string(), cert_sha256_hex: reliable_cert },
                interface: None,
                port_range: None,
            },
            DualPathEndpoint {
                factory: AnyMuxFactory::noq(build_client_config(Some(64 * 1024))),
                remote: RemoteSpec { addr: unreliable_addr, server_name: SNI.to_string(), cert_sha256_hex: unreliable_cert },
                interface: None,
                port_range: None,
            },
        )
        .await
        .expect("connect_dual_path should succeed");

        let mut stream = dual.open_reliable().await.expect("open_reliable should succeed");
        stream.write_all(b"hello reliable").await.unwrap();
        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello reliable");

        assert!(dual.max_unreliable_datagram_size().is_some(), "datagrams should be enabled on the unreliable connection");
        dual.send_unreliable(bytes::Bytes::from_static(b"hello unreliable")).expect("send_unreliable should succeed");
        let echoed = tokio::time::timeout(Duration::from_secs(5), dual.recv_unreliable())
            .await
            .expect("timed out waiting for the echoed datagram")
            .expect("recv_unreliable failed");
        assert_eq!(&echoed[..], b"hello unreliable");
    }

    #[tokio::test]
    async fn connect_dual_path_fails_fast_when_one_side_is_misconfigured() {
        let (reliable_addr, _real_cert) = spawn_reliable_echo_listener().await;
        let (unreliable_addr, unreliable_cert) = spawn_unreliable_echo_listener().await;

        let err = connect_dual_path(
            DualPathEndpoint {
                factory: AnyMuxFactory::noq(build_client_config(None)),
                // Wrong fingerprint on purpose: cert verification should
                // fail fast rather than this hanging on the other side.
                remote: RemoteSpec { addr: reliable_addr, server_name: SNI.to_string(), cert_sha256_hex: "0".repeat(64) },
                interface: None,
                port_range: None,
            },
            DualPathEndpoint {
                factory: AnyMuxFactory::noq(build_client_config(Some(64 * 1024))),
                remote: RemoteSpec { addr: unreliable_addr, server_name: SNI.to_string(), cert_sha256_hex: unreliable_cert },
                interface: None,
                port_range: None,
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, TransportError::Mux(MuxError::CertPinMismatch { .. })), "expected a cert pin mismatch, got {err:?}");
    }

    /// Unlike the cert-mismatch test above (where both sides need a network
    /// round-trip before either resolves), this specifically exercises
    /// `connect_dual_path`'s "drop the still-in-flight side" cancellation
    /// path: the reliable side fails synchronously (a bogus physical
    /// interface fails inside `bind_physical_interface` itself, before the
    /// dial ever touches the network — see
    /// `physical_interface::tests::bogus_interface_index_fails_rather_than_panicking`
    /// for the same technique), so the unreliable side's real QUIC handshake
    /// against a live listener is guaranteed to still be in progress when
    /// `try_join!` observes the first error and drops it.
    ///
    /// Windows-only: confirmed on a real `test-windows` CI run that this
    /// relies on a Unix-only assumption — see
    /// `physical_interface::tests::bogus_interface_index_fails_rather_than_panicking`'s
    /// own identical `#[cfg(not(windows))]` caveat (Windows doesn't validate
    /// a bogus interface index at bind time, so the "reliable" side here
    /// doesn't fail fast on Windows either; it instead fails much later via
    /// a QUIC-level timeout that this test's own 5-second bound is too short
    /// for).
    #[tokio::test]
    #[cfg(not(windows))]
    async fn connect_dual_path_cancels_the_still_pending_side_when_the_other_fails_immediately() {
        let (unreliable_addr, unreliable_cert) = spawn_unreliable_echo_listener().await;

        let err = tokio::time::timeout(
            Duration::from_secs(5),
            connect_dual_path(
                DualPathEndpoint {
                    factory: AnyMuxFactory::noq(build_client_config(None)),
                    remote: RemoteSpec { addr: "127.0.0.1:1".parse().unwrap(), server_name: SNI.to_string(), cert_sha256_hex: "0".repeat(64) },
                    interface: Some(quicsock::InterfaceIndex(u32::MAX)),
                    port_range: None,
                },
                DualPathEndpoint {
                    factory: AnyMuxFactory::noq(build_client_config(Some(64 * 1024))),
                    remote: RemoteSpec { addr: unreliable_addr, server_name: SNI.to_string(), cert_sha256_hex: unreliable_cert },
                    interface: None,
                    port_range: None,
                },
            ),
        )
        .await
        .expect("connect_dual_path should not hang even though the unreliable side was still mid-handshake")
        .unwrap_err();

        assert!(matches!(err, TransportError::Mux(MuxError::Bind { .. })), "expected a bind error from the bogus interface, got {err:?}");
    }

    /// Windows-only: relies on the same bogus-interface-fails-fast
    /// assumption as `connect_dual_path_cancels_the_still_pending_side_when_the_other_fails_immediately`
    /// above, which doesn't hold on Windows — see that test's doc comment.
    #[tokio::test]
    #[cfg(not(windows))]
    async fn connect_dual_path_best_effort_returns_each_sides_result_independently() {
        let (unreliable_addr, unreliable_cert) = spawn_unreliable_echo_listener().await;

        let (reliable_result, unreliable_result) = connect_dual_path_best_effort(
            DualPathEndpoint {
                factory: AnyMuxFactory::noq(build_client_config(None)),
                remote: RemoteSpec { addr: "127.0.0.1:1".parse().unwrap(), server_name: SNI.to_string(), cert_sha256_hex: "0".repeat(64) },
                interface: Some(quicsock::InterfaceIndex(u32::MAX)),
                port_range: None,
            },
            DualPathEndpoint {
                factory: AnyMuxFactory::noq(build_client_config(Some(64 * 1024))),
                remote: RemoteSpec { addr: unreliable_addr, server_name: SNI.to_string(), cert_sha256_hex: unreliable_cert },
                interface: None,
                port_range: None,
            },
        )
        .await;

        assert!(matches!(reliable_result.map(|_| ()).unwrap_err(), TransportError::Mux(MuxError::Bind { .. })));
        assert!(unreliable_result.is_ok(), "the unreliable side's own dial should still succeed independently of the reliable side failing");
    }

    #[tokio::test]
    async fn both_sides_can_be_bound_to_a_specific_physical_interface() {
        let loopback = quicsock::discovery::list_interfaces()
            .into_iter()
            .find(|(_, iface)| iface.is_loopback())
            .map(|(index, _)| index)
            .expect("this machine should have a loopback interface");

        let (reliable_addr, reliable_cert) = spawn_reliable_echo_listener().await;
        let (unreliable_addr, unreliable_cert) = spawn_unreliable_echo_listener().await;

        let dual = connect_dual_path(
            DualPathEndpoint {
                factory: AnyMuxFactory::noq(build_client_config(None)),
                remote: RemoteSpec { addr: reliable_addr, server_name: SNI.to_string(), cert_sha256_hex: reliable_cert },
                interface: Some(loopback),
                port_range: None,
            },
            DualPathEndpoint {
                factory: AnyMuxFactory::noq(build_client_config(Some(64 * 1024))),
                remote: RemoteSpec { addr: unreliable_addr, server_name: SNI.to_string(), cert_sha256_hex: unreliable_cert },
                interface: Some(loopback),
                port_range: None,
            },
        )
        .await
        .expect("connect_dual_path bound to the loopback interface should succeed");

        assert!(dual.reliable.remote_addr().is_some());
        assert!(dual.unreliable.remote_addr().is_some());
    }

    /// Like [`spawn_reliable_echo_listener`]/[`spawn_unreliable_echo_listener`],
    /// but returns `None` (instead of panicking) specifically when the bind
    /// fails with `MuxError::EndpointSetup` — the exact error a
    /// `test-windows` CI run once hit for an IPv6-bound `noq::Endpoint::server()`
    /// (`"...os error 10022..."`, `WSAEINVAL`), which a real Windows machine
    /// running the same call does *not* reproduce (see
    /// [`connect_dual_path_dials_an_ipv6_remote`]'s doc comment). Any *other*
    /// error still panics, so a genuine regression isn't silently swallowed
    /// as if it were the known CI-runner quirk.
    async fn try_spawn_ipv6_listener() -> Option<(SocketAddr, String)> {
        let (server_config, cert_sha256_hex) = build_server_config(None);
        let listener =
            match AnyMuxListener::bind_noq(server_config, quicmux::BindSpec { local_addr: "[::1]:0".parse().unwrap(), port_range: None }).await
            {
                Ok(listener) => listener,
                Err(MuxError::EndpointSetup(msg)) => {
                    eprintln!("try_spawn_ipv6_listener: noq::Endpoint::server setup rejected the IPv6 bind ({msg}), skipping");
                    return None;
                }
                Err(e) => panic!("unexpected IPv6 listener bind failure: {e}"),
            };
        let addr: SocketAddr = format!("[::1]:{}", listener.local_addr().unwrap().port()).parse().unwrap();
        tokio::spawn(async move {
            if let Some(incoming) = listener.accept().await {
                let _ = incoming.accept().await;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        Some((addr, cert_sha256_hex))
    }

    /// Regression test: an IPv6-only remote (this module's own motivating
    /// case — a cellular interface via 464XLAT/NAT64) must be dialable, not
    /// silently impossible because the dial always bound an IPv4 socket.
    ///
    /// A `test-windows` CI run twice failed here with
    /// `MuxError::EndpointSetup("...os error 10022...")` (`WSAEINVAL`), on
    /// both the listener's `noq::Endpoint::server()` and (separately) the
    /// client's `noq::Endpoint::new_with_abstract_socket()` — but a real
    /// Windows machine running the exact same operations (confirmed
    /// directly by a user on their own machine) does not reproduce either
    /// failure. This looks like a quirk specific to that CI runner's own
    /// IPv6 stack rather than a genuine Windows/`noq` limitation, so this
    /// test tries both real operations and skips gracefully only on that
    /// exact known error (see [`try_spawn_ipv6_listener`]) rather than
    /// assuming IPv6 is broken on Windows in general.
    #[tokio::test]
    async fn connect_dual_path_dials_an_ipv6_remote() {
        let Some((addr, cert_sha256_hex)) = try_spawn_ipv6_listener().await else {
            return;
        };
        let (unreliable_addr, unreliable_cert) = spawn_unreliable_echo_listener().await;

        let result = connect_dual_path(
            DualPathEndpoint {
                factory: AnyMuxFactory::noq(build_client_config(None)),
                remote: RemoteSpec { addr, server_name: SNI.to_string(), cert_sha256_hex },
                interface: None,
                port_range: None,
            },
            DualPathEndpoint {
                factory: AnyMuxFactory::noq(build_client_config(Some(64 * 1024))),
                remote: RemoteSpec { addr: unreliable_addr, server_name: SNI.to_string(), cert_sha256_hex: unreliable_cert },
                interface: None,
                port_range: None,
            },
        )
        .await;

        match result {
            Ok(dual) => assert!(dual.reliable.remote_addr().is_some()),
            Err(TransportError::Mux(MuxError::EndpointSetup(msg))) => {
                eprintln!("connect_dual_path_dials_an_ipv6_remote: client-side noq::Endpoint setup rejected the IPv6 bind ({msg}), skipping");
            }
            Err(e) => panic!("dialing an IPv6 remote failed unexpectedly: {e}"),
        }
    }

    #[tokio::test]
    async fn connect_dual_path_honors_a_port_range() {
        let (reliable_addr, reliable_cert) = spawn_reliable_echo_listener().await;
        let (unreliable_addr, unreliable_cert) = spawn_unreliable_echo_listener().await;

        let dual = connect_dual_path(
            DualPathEndpoint {
                factory: AnyMuxFactory::noq(build_client_config(None)),
                remote: RemoteSpec { addr: reliable_addr, server_name: SNI.to_string(), cert_sha256_hex: reliable_cert },
                interface: None,
                port_range: Some((40800, 40810)),
            },
            DualPathEndpoint {
                factory: AnyMuxFactory::noq(build_client_config(Some(64 * 1024))),
                remote: RemoteSpec { addr: unreliable_addr, server_name: SNI.to_string(), cert_sha256_hex: unreliable_cert },
                interface: None,
                port_range: Some((40900, 40910)),
            },
        )
        .await
        .expect("connect_dual_path with a port_range should succeed");

        assert!(dual.reliable.remote_addr().is_some());
        assert!(dual.unreliable.remote_addr().is_some());
    }
}
