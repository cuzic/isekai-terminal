//! Self-contained UDP-over-quicmux-datagram relay (`#32`/`#33`), built for
//! the UAV C2 OSS side-project exploration described in
//! `aquila-oss-proposal.html` and designed in task #35's design doc. **Not
//! wired into `isekai-pipe`'s real ATTACH v2 SSH session engine
//! (`crate::engine`)** — deliberately isolated (per project decision,
//! 2026-07-17): this module never imports `isekai_protocol::attach`, never
//! touches a live `isekai-ssh`/`isekai-pipe serve` session, and is exercised
//! only by its own tests plus task #37's e2e tests. Building a real UAV
//! product on top of this — binding channels to an ATTACHed session's
//! generation/lease, as task #35 §4 designed — is deferred until that
//! side-project is actually greenlit; wiring this into the live engine
//! before then would be exactly the kind of speculative-generality-in-
//! production-code this project's own `CLAUDE.md` warns against.
//!
//! # What this relays
//!
//! One quicmux connection can carry any number of independent UDP-relay
//! *channels*, each identified by a [`frame::ChannelId`] and each bridging
//! one *connected* local `UdpSocket` (i.e. bound via
//! `tokio::net::UdpSocket::connect`, so `.send()`/`.recv()` — not
//! `.send_to()`/`.recv_from()` — talk to exactly one fixed local peer) to
//! the datagram plane of the connection:
//!
//! - [`run_udp_pump`] reads from the local socket and forwards each
//!   datagram onto the connection, tagged with the channel id
//!   ([`frame::encode_datagram`]).
//! - [`run_datagram_pump`] reads every datagram the connection receives
//!   (regardless of channel), demuxes by channel id via a shared
//!   [`ChannelTable`], and forwards each to its registered local socket.
//!   A datagram whose channel id has no registered socket is silently
//!   dropped — task #35 §4.2's cross-channel-injection rationale, adapted
//!   for this session-less protocol (any peer that can write to this
//!   connection's datagram plane already passed the underlying QUIC/TLS
//!   handshake; there is no further per-channel credential to check).
//! - [`run_control_stream_reader`] drives the receiving side of the
//!   [`frame::ControlFrame`] protocol on a dedicated reliable stream:
//!   `ChannelOpen` binds/connects a fresh local `UdpSocket`, registers it in
//!   the [`ChannelTable`], and spawns its own [`run_udp_pump`] task so
//!   replies relay back onto the connection too; `ChannelClose` unregisters
//!   it.
//! - [`open_channel`]/[`close_channel`] are the sending side, for a caller
//!   that already has its own local socket bound and just needs the peer to
//!   register a matching channel for it.
//!
//! Nothing in `isekai-pipe`'s real `main()`/CLI wires into this module yet —
//! it exists to be exercised by its own tests and task #37's e2e tests until
//! the UAV side-project (if ever greenlit) needs a real entry point.
#![allow(dead_code)]

pub mod frame;

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use quicmux::{AnyByteStream, AnyMuxConnection, MuxError};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;

pub use frame::ChannelId;

/// One registered channel: the local socket [`run_datagram_pump`] relays
/// incoming datagrams to, plus a handle to abort the [`run_udp_pump`] task
/// relaying the other direction — held so [`ChannelTable::unregister`] and a
/// duplicate [`ChannelTable::register`] for the same id can actually stop
/// that task instead of leaking it (Codexレビュー、2026-07-17: `ChannelClose`/
/// re-`ChannelOpen` previously left the old `run_udp_pump` running forever
/// against a socket the table no longer tracked, still relaying whatever the
/// old local peer sent under the reused channel id).
struct ChannelEntry {
    socket: Arc<UdpSocket>,
    pump_abort: tokio::task::AbortHandle,
}

/// Shared table of channel id → local UDP socket (+ its pump task's abort
/// handle). `Clone`ed freely (cheap — an `Arc` around the actual map) so
/// [`run_datagram_pump`] (reads it on every received datagram) and
/// [`run_control_stream_reader`] (mutates it on every
/// `ChannelOpen`/`ChannelClose`) can each hold their own handle onto the
/// same underlying table.
#[derive(Default, Clone)]
pub struct ChannelTable {
    inner: Arc<RwLock<HashMap<ChannelId, ChannelEntry>>>,
}

impl ChannelTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `socket` for `id`. If `id` was already registered, the
    /// previous entry's `run_udp_pump` task is aborted first — a caller that
    /// re-registers the same id (e.g. a duplicate `ChannelOpen`) must not end
    /// up with two pumps relaying under the same tag.
    pub async fn register(&self, id: ChannelId, socket: Arc<UdpSocket>, pump_abort: tokio::task::AbortHandle) {
        let previous = self.inner.write().await.insert(id, ChannelEntry { socket, pump_abort });
        if let Some(previous) = previous {
            previous.pump_abort.abort();
        }
    }

    /// Unregisters `id`, aborting its `run_udp_pump` task.
    pub async fn unregister(&self, id: ChannelId) {
        if let Some(entry) = self.inner.write().await.remove(&id) {
            entry.pump_abort.abort();
        }
    }

    async fn get(&self, id: ChannelId) -> Option<Arc<UdpSocket>> {
        self.inner.read().await.get(&id).map(|entry| entry.socket.clone())
    }

    #[cfg(test)]
    async fn len(&self) -> usize {
        self.inner.read().await.len()
    }
}

/// Relays every datagram `conn` receives to its registered local UDP socket
/// (per `channels`), forever, until `conn.recv_datagram()` itself errors
/// (connection lost — the returned [`MuxError`] is that error, for the
/// caller to log/react to). Meant to run as its own `tokio::spawn`ed task —
/// one per connection, shared across every channel that connection carries
/// (channels are demuxed by tag, not by separate receive loops).
pub async fn run_datagram_pump(conn: AnyMuxConnection, channels: ChannelTable) -> MuxError {
    loop {
        let datagram = match conn.recv_datagram().await {
            Ok(d) => d,
            Err(e) => return e,
        };
        let Some((channel_id, payload)) = frame::decode_datagram(&datagram) else {
            log::warn!("datagram_relay: dropping malformed datagram (shorter than a channel id)");
            continue;
        };
        let Some(socket) = channels.get(channel_id).await else {
            log::debug!("datagram_relay: dropping datagram for unregistered channel {channel_id:?}");
            continue;
        };
        if let Err(e) = socket.send(&payload).await {
            log::warn!("datagram_relay: local UDP send failed for channel {channel_id:?}: {e}");
        }
    }
}

/// Relays UDP datagrams read from `socket` onto `conn`'s datagram plane,
/// tagged with `channel_id`, until `socket.recv()` errors (the local peer is
/// gone) or `conn.send_datagram()` reports the *connection* itself is dead
/// (see [`is_connection_lost`]) — a per-datagram issue (too large, peer
/// doesn't support datagrams) is logged and the loop continues, since
/// nothing about the local socket or the connection changed. Meant to run as
/// its own `tokio::spawn`ed task — one per channel (unlike
/// [`run_datagram_pump`], which is shared across all channels on one
/// connection). Expected to be aborted via [`ChannelTable::unregister`]/a
/// duplicate [`ChannelTable::register`] rather than exit on its own in the
/// common case (a still-live connection with no more local traffic just
/// idles in `socket.recv()`).
pub async fn run_udp_pump(conn: AnyMuxConnection, channel_id: ChannelId, socket: Arc<UdpSocket>) {
    let mut buf = [0u8; 65535];
    loop {
        let n = match socket.recv(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                log::warn!("datagram_relay: local UDP recv failed for channel {channel_id:?}: {e}");
                return;
            }
        };
        let framed = frame::encode_datagram(channel_id, &buf[..n]);
        if let Err(e) = conn.send_datagram(framed) {
            log::warn!("datagram_relay: send_datagram failed for channel {channel_id:?}: {e:?}");
            if is_connection_lost(&e) {
                return;
            }
        }
    }
}

/// Whether a [`MuxError`] from `send_datagram` means the whole connection is
/// gone (so a pump task looping on it should stop, not keep retrying/logging
/// forever) — as opposed to a per-call issue (payload too large, datagrams
/// unsupported) that leaves the connection itself still usable.
fn is_connection_lost(e: &MuxError) -> bool {
    matches!(e, MuxError::PeerClosed { .. } | MuxError::TransportLost { .. } | MuxError::LocallyClosed)
}

#[derive(Debug)]
pub enum ReadControlFrameError {
    Mux(MuxError),
    /// The stream ended mid-frame (after at least one byte of a new frame
    /// was already read) — distinct from a clean end-of-stream between
    /// frames, which [`read_control_frame`] reports as `Ok(None)` instead.
    UnexpectedEof,
    FrameTooLarge { len: usize },
    Decode(frame::FrameDecodeError),
}

impl std::fmt::Display for ReadControlFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mux(e) => write!(f, "control stream I/O failed: {e}"),
            Self::UnexpectedEof => write!(f, "control stream closed mid-frame"),
            Self::FrameTooLarge { len } => {
                write!(f, "control frame body too large: {len} bytes (max {})", frame::MAX_CONTROL_FRAME_LEN)
            }
            Self::Decode(e) => write!(f, "control frame decode failed: {e}"),
        }
    }
}

impl std::error::Error for ReadControlFrameError {}

async fn read_exact(stream: &mut AnyByteStream, buf: &mut [u8]) -> Result<(), ReadControlFrameError> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = stream.read(&mut buf[filled..]).await.map_err(ReadControlFrameError::Mux)?;
        if n == 0 {
            return Err(ReadControlFrameError::UnexpectedEof);
        }
        filled += n;
    }
    Ok(())
}

/// Reads and decodes one [`frame::ControlFrame`] off `stream` — see
/// `frame`'s module docs for the `[u16 body_len][body]` wire shape. `Ok(None)`
/// means the stream ended cleanly at a frame boundary (the peer is done
/// sending control frames); any other end-of-stream is
/// [`ReadControlFrameError::UnexpectedEof`].
pub async fn read_control_frame(stream: &mut AnyByteStream) -> Result<Option<frame::ControlFrame>, ReadControlFrameError> {
    let mut len_buf = [0u8; 2];
    let n = stream.read(&mut len_buf[..1]).await.map_err(ReadControlFrameError::Mux)?;
    if n == 0 {
        return Ok(None);
    }
    read_exact(stream, &mut len_buf[1..]).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    if len > frame::MAX_CONTROL_FRAME_LEN {
        return Err(ReadControlFrameError::FrameTooLarge { len });
    }
    let mut body = vec![0u8; len];
    read_exact(stream, &mut body).await?;
    let parsed = frame::decode_control_frame_body(&body).map_err(ReadControlFrameError::Decode)?;
    Ok(Some(parsed))
}

/// Sends a [`frame::ControlFrame::ChannelOpen`] for `id`/`target` on
/// `control` — the sending side of the control protocol
/// [`run_control_stream_reader`] drives on the receiving side.
pub async fn open_channel(control: &mut AnyByteStream, id: ChannelId, target: std::net::SocketAddr) -> Result<(), MuxError> {
    let framed = frame::encode_control_frame(&frame::ControlFrame::ChannelOpen { id, target });
    control.write_all(&framed).await
}

/// Sends a [`frame::ControlFrame::ChannelClose`] for `id` on `control`.
pub async fn close_channel(control: &mut AnyByteStream, id: ChannelId) -> Result<(), MuxError> {
    let framed = frame::encode_control_frame(&frame::ControlFrame::ChannelClose { id });
    control.write_all(&framed).await
}

/// Drives the receiving side of the control protocol: reads
/// [`frame::ControlFrame`]s off `control` until it closes (or errors), and
/// for each `ChannelOpen { id, target }` binds a fresh local `UdpSocket`
/// connected to `target`, registers it in `channels`, and spawns its own
/// [`run_udp_pump`] task so replies from `target` relay back onto `conn`
/// (without this, [`run_datagram_pump`] alone only relays *into* the newly
/// bound socket, never back out of it — a bug this module's own e2e test,
/// `full_round_trip_relays_udp_through_the_datagram_plane`, caught). Each
/// `ChannelClose { id }` unregisters it, which now also aborts that spawned
/// `run_udp_pump` task (Codexレビュー、2026-07-17: it does *not* exit on its
/// own just because the table entry is gone — see [`ChannelTable::register`]/
/// [`ChannelTable::unregister`]'s docs). Meant to run as its own
/// `tokio::spawn`ed task, one per connection.
///
/// **Security note (not yet a concern while this module stays unwired from
/// any real ATTACH v2 session, Codexレビュー2026-07-17)**: `target` is an
/// arbitrary `SocketAddr` the *peer* supplies over the control stream, with
/// no allowlist — a real deployment must not let an authenticated-but-
/// unprivileged peer direct this process to send UDP traffic to an
/// arbitrary local/LAN address (SSRF-shaped risk: loopback services,
/// RFC1918/link-local hosts, metadata endpoints, reflection/scanning
/// footholds). Before wiring this into a real session, add a target
/// allowlist/policy bound to what that session is actually authorized to
/// reach.
///
/// `bind_addr` is the local address new sockets bind to (typically
/// `0.0.0.0`/`::` for an OS-assigned port) — callers that need a specific
/// local interface pass that instead, mirroring
/// `isekai_transport::physical_interface`'s own bind-address parameter
/// convention.
pub async fn run_control_stream_reader(mut control: AnyByteStream, conn: AnyMuxConnection, channels: ChannelTable, bind_addr: std::net::IpAddr) {
    loop {
        match read_control_frame(&mut control).await {
            Ok(Some(frame::ControlFrame::ChannelOpen { id, target })) => {
                if let Err(e) = open_local_channel_socket(&conn, &channels, id, target, bind_addr).await {
                    log::warn!("datagram_relay: failed to open local UDP socket for channel {id:?} -> {target}: {e}");
                }
            }
            Ok(Some(frame::ControlFrame::ChannelClose { id })) => {
                channels.unregister(id).await;
            }
            Ok(None) => return,
            Err(e) => {
                log::warn!("datagram_relay: control stream read failed: {e}");
                return;
            }
        }
    }
}

async fn open_local_channel_socket(
    conn: &AnyMuxConnection,
    channels: &ChannelTable,
    id: ChannelId,
    target: std::net::SocketAddr,
    bind_addr: std::net::IpAddr,
) -> io::Result<()> {
    let socket = Arc::new(UdpSocket::bind((bind_addr, 0)).await?);
    socket.connect(target).await?;
    let pump_handle = tokio::spawn(run_udp_pump(conn.clone(), id, socket.clone()));
    channels.register(id, socket, pump_handle.abort_handle()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    // --- ChannelTable ---

    /// An `AbortHandle` for a test that only cares about `ChannelTable`'s own
    /// bookkeeping, not about a real `run_udp_pump` task.
    fn dummy_abort_handle() -> tokio::task::AbortHandle {
        tokio::spawn(std::future::pending::<()>()).abort_handle()
    }

    #[tokio::test]
    async fn channel_table_register_then_get_returns_the_socket() {
        let table = ChannelTable::new();
        let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
        table.register(ChannelId(1), socket.clone(), dummy_abort_handle()).await;
        assert!(table.get(ChannelId(1)).await.is_some());
        assert_eq!(table.len().await, 1);
    }

    #[tokio::test]
    async fn channel_table_unregister_removes_the_socket() {
        let table = ChannelTable::new();
        let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
        table.register(ChannelId(1), socket, dummy_abort_handle()).await;
        table.unregister(ChannelId(1)).await;
        assert!(table.get(ChannelId(1)).await.is_none());
        assert_eq!(table.len().await, 0);
    }

    /// Regression test for the leak Codex's review caught (2026-07-17):
    /// `unregister` used to only remove the table entry, leaving the
    /// `run_udp_pump` task spawned for it running forever.
    #[tokio::test]
    async fn channel_table_unregister_aborts_the_registered_pump_task() {
        let table = ChannelTable::new();
        let socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
        let handle = tokio::spawn(std::future::pending::<()>());
        table.register(ChannelId(1), socket, handle.abort_handle()).await;

        table.unregister(ChannelId(1)).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(handle.is_finished(), "unregister should abort the pump task, not leak it");
    }

    /// Regression test for the second half of the same review finding: a
    /// duplicate `register` for an id already in use (a second `ChannelOpen`
    /// for the same channel) used to leave the *previous* pump task running
    /// too, so both the old and new local peer's traffic could cross-talk
    /// under the same channel id.
    #[tokio::test]
    async fn channel_table_register_over_an_existing_id_aborts_the_previous_pump_task() {
        let table = ChannelTable::new();
        let socket_a = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
        let handle_a = tokio::spawn(std::future::pending::<()>());
        table.register(ChannelId(1), socket_a, handle_a.abort_handle()).await;

        let socket_b = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
        let handle_b = tokio::spawn(std::future::pending::<()>());
        table.register(ChannelId(1), socket_b, handle_b.abort_handle()).await;

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(handle_a.is_finished(), "re-registering the same id should abort the old pump task");
        assert!(!handle_b.is_finished(), "the new pump task must not be aborted");
    }

    #[tokio::test]
    async fn channel_table_get_on_unregistered_id_is_none() {
        let table = ChannelTable::new();
        assert!(table.get(ChannelId(99)).await.is_none());
    }

    // --- control frame stream round-trip (over a real quicmux connection) ---

    fn test_client_config() -> quicmux::MuxClientConfig {
        quicmux::MuxClientConfig {
            alpn: b"datagram-relay-test/1".to_vec(),
            exporter_label: b"datagram-relay-test-exporter".to_vec(),
            max_idle_timeout: std::time::Duration::from_secs(15),
            keep_alive_interval: std::time::Duration::from_secs(5),
            max_concurrent_bidi_streams: 4,
            max_concurrent_uni_streams: 0,
            multipath: false,
            datagram_send_buffer_size: Some(64 * 1024),
        }
    }

    fn test_server_config() -> (quicmux::MuxServerConfig, String) {
        let cert = rcgen::generate_simple_self_signed(vec!["datagram-relay-test.local".to_string()]).unwrap();
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().clone());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
        let cert_sha256_hex = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(cert_der.as_ref());
            hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let config = quicmux::MuxServerConfig {
            alpn: test_client_config().alpn,
            exporter_label: test_client_config().exporter_label,
            max_idle_timeout: std::time::Duration::from_secs(15),
            keep_alive_interval: std::time::Duration::from_secs(5),
            max_concurrent_bidi_streams: 4,
            max_concurrent_uni_streams: 0,
            multipath: false,
            datagram_send_buffer_size: Some(64 * 1024),
            cert_chain: vec![cert_der],
            private_key: key_der,
        };
        (config, cert_sha256_hex)
    }

    /// Connects a fresh noq-backed client/server pair for this module's own
    /// tests — mirrors `quicmux::noq_backend`'s own test helpers, kept local
    /// here (rather than shared) per this repo's `isekai-ssh-e2e-test-self-
    /// containment-convention`.
    async fn connect_pair() -> (AnyMuxConnection, AnyMuxConnection) {
        let (server_config, cert_sha256_hex) = test_server_config();
        let listener = quicmux::AnyMuxListener::bind_noq(server_config, quicmux::BindSpec::any_ipv4()).await.unwrap();
        let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listener.local_addr().unwrap().port());

        let accept_task = tokio::spawn(async move {
            let incoming = listener.accept().await.expect("listener closed before accepting");
            incoming.accept().await.expect("server-side handshake failed")
        });

        let factory = quicmux::AnyMuxFactory::noq(test_client_config());
        let endpoint = factory.create_endpoint(quicmux::BindSpec::any_ipv4()).await.unwrap();
        let client_conn = endpoint
            .connect(quicmux::RemoteSpec { addr: server_addr, server_name: "datagram-relay-test.local".to_string(), cert_sha256_hex })
            .await
            .expect("client-side handshake failed");

        let server_conn = accept_task.await.unwrap();
        (client_conn, server_conn)
    }

    #[tokio::test]
    async fn channel_open_over_control_stream_registers_a_local_socket_on_the_receiving_side() {
        let (client_conn, server_conn) = connect_pair().await;

        // The "far end" binds a local UDP socket to act as the relay target
        // (standing in for e.g. a drone's real MAVLink UDP listener).
        let far_end_target = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let target_addr = far_end_target.local_addr().unwrap();

        let server_channels = ChannelTable::new();
        let server_channels_for_reader = server_channels.clone();
        // `accept_bi()` must run concurrently with (not before) the client's
        // `open_bi()` below — both sides block until the other rendezvous,
        // so awaiting `accept_bi()` inline here (instead of inside this
        // spawned task) would deadlock the test on this line forever, before
        // `client_conn.open_bi()` ever runs.
        let server_conn_for_reader = server_conn.clone();
        let reader_task = tokio::spawn(async move {
            let server_control = server_conn_for_reader.accept_bi().await.unwrap();
            run_control_stream_reader(server_control, server_conn_for_reader, server_channels_for_reader, IpAddr::V4(Ipv4Addr::LOCALHOST)).await
        });

        let mut client_control = client_conn.open_bi().await.unwrap();
        open_channel(&mut client_control, ChannelId(1), target_addr).await.unwrap();

        // `run_control_stream_reader` runs forever until the stream closes —
        // poll `server_channels` (the same table it registers into) instead
        // of waiting on `reader_task` itself.
        for _ in 0..50 {
            if server_channels.get(ChannelId(1)).await.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(server_channels.get(ChannelId(1)).await.is_some(), "ChannelOpen should have registered a local socket");

        close_channel(&mut client_control, ChannelId(1)).await.unwrap();
        for _ in 0..50 {
            if server_channels.get(ChannelId(1)).await.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(server_channels.get(ChannelId(1)).await.is_none(), "ChannelClose should have unregistered the local socket");

        drop(client_control);
        reader_task.abort();
    }

    /// Full round-trip: client opens a channel bound to a local "far end" UDP
    /// target on the server side, then relays a UDP datagram from its own
    /// local socket, through the quicmux datagram plane, to that target — and
    /// the target's reply relays all the way back.
    #[tokio::test]
    async fn full_round_trip_relays_udp_through_the_datagram_plane() {
        let (client_conn, server_conn) = connect_pair().await;

        // Stands in for the real UDP service being reached (e.g. a drone's
        // MAVLink listener) — echoes whatever it receives.
        let far_end = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let far_end_addr = far_end.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                let Ok((n, peer)) = far_end.recv_from(&mut buf).await else { return };
                let _ = far_end.send_to(&buf[..n], peer).await;
            }
        });

        // Server side: registers the channel on ChannelOpen, then pumps
        // datagrams for it. `accept_bi()` must run concurrently with (not
        // before) the client's `open_bi()` below — see the identical
        // deadlock comment in
        // `channel_open_over_control_stream_registers_a_local_socket_on_the_receiving_side`.
        let server_channels = ChannelTable::new();
        let server_conn_for_reader = server_conn.clone();
        let server_channels_for_reader = server_channels.clone();
        tokio::spawn(async move {
            let server_control = server_conn_for_reader.accept_bi().await.unwrap();
            run_control_stream_reader(server_control, server_conn_for_reader, server_channels_for_reader, IpAddr::V4(Ipv4Addr::LOCALHOST)).await
        });
        let server_conn_for_pump = server_conn.clone();
        tokio::spawn(run_datagram_pump(server_conn_for_pump, server_channels.clone()));

        // Client side: a local socket standing in for e.g. a local MAVLink
        // application, bridged to the channel. Connected to its real peer
        // (`mavlink_app`, below) once that peer's ephemeral port is known.
        let local_app_socket = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
        let client_channels = ChannelTable::new();

        let mut client_control = client_conn.open_bi().await.unwrap();
        open_channel(&mut client_control, ChannelId(7), far_end_addr).await.unwrap();

        // Wait for the server to actually register the channel before
        // sending — otherwise the first datagram races the ChannelOpen and
        // would be (correctly, per this module's drop-unregistered policy)
        // dropped.
        for _ in 0..50 {
            if server_channels.get(ChannelId(7)).await.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(server_channels.get(ChannelId(7)).await.is_some());

        let udp_pump_handle = tokio::spawn(run_udp_pump(client_conn.clone(), ChannelId(7), local_app_socket.clone()));
        client_channels.register(ChannelId(7), local_app_socket.clone(), udp_pump_handle.abort_handle()).await;
        let client_conn_for_pump = client_conn.clone();
        tokio::spawn(run_datagram_pump(client_conn_for_pump, client_channels.clone()));

        // A second, independent local socket plays "the local MAVLink app"
        // talking to `local_app_socket` as its peer.
        let mavlink_app = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        mavlink_app.connect(local_app_socket.local_addr().unwrap()).await.unwrap();
        local_app_socket.connect(mavlink_app.local_addr().unwrap()).await.unwrap();

        mavlink_app.send(b"hello mavlink over quic datagram").await.unwrap();

        let mut buf = [0u8; 1024];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), mavlink_app.recv(&mut buf))
            .await
            .expect("timed out waiting for the echoed datagram to relay all the way back")
            .unwrap();
        assert_eq!(&buf[..n], b"hello mavlink over quic datagram");
    }
}
