//! Simultaneous multi-remote-address QUIC paths — `isekai-terminal-core`'s
//! path0/path1 pattern (`multipath_transport.rs`, Phase 9-2/9-3), generalized
//! and ported here. One `noq::Connection` holds a primary path plus any
//! number of secondary paths open at once, each to a different remote
//! address for the *same* peer (e.g. a Tailscale address and a
//! directly-reachable address for the same isekai-pipe helper) — all via
//! `noq::Connection::open_path` with `local_ip: None` (OS-default-routed),
//! which is the half of Android's Phase 9 multipath work that is actually
//! proven working on real hardware.
//!
//! See [`crate::path_health`]'s module docs for what this deliberately does
//! **not** port (same-connection physical-interface multipath via
//! `open_path(local_ip=Some(..))` — a confirmed dead end, noq issue #738).
//! Reactive physical-interface failover belongs to
//! [`crate::traits::QuicEndpointRebinder`] instead, driven by the same
//! [`crate::path_health`] classification this module also uses — that
//! wiring is a separate piece of work, not part of this module.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use log::{info, warn};
use tokio_stream::StreamExt;

use crate::error::TransportError;
use crate::path_health::{self, PathHealthEvent, PathHealthTracker, PathLabel};
use crate::system::client_config_for;
use crate::types::{BindSpec, RemoteSpec};

/// Label [`connect_multipath`] registers the initial connection's path
/// under (`noq::PathId::ZERO`, established as part of the QUIC handshake
/// itself rather than via a later `open_path` call).
pub const PRIMARY_PATH_LABEL: &str = "primary";

const OPEN_PATH_MAX_ATTEMPTS: u32 = 3;
const OPEN_PATH_INITIAL_BACKOFF: Duration = Duration::from_secs(2);
const OPEN_PATH_TIMEOUT: Duration = Duration::from_secs(8);

/// One additional remote address to hold open simultaneously alongside the
/// primary path, once the primary connection is established.
#[derive(Debug, Clone)]
pub struct SecondaryPath {
    pub label: PathLabel,
    pub addr: SocketAddr,
}

/// The result of [`connect_multipath`] — the live connection, its path
/// health tracker (already seeded with the primary path and watching for
/// secondary paths as they establish), and the endpoint the connection was
/// made through (kept alive here since `noq::Connection` does not itself
/// keep its `noq::Endpoint` alive — the same reason
/// `multipath_transport.rs::establish_multipath_connection` returns it).
pub struct MultipathConnection {
    pub conn: noq::Connection,
    pub tracker: PathHealthTracker,
    pub endpoint: noq::Endpoint,
}

/// Binds a plain UDP socket at `bind` and delegates to
/// [`connect_multipath_with_socket`] — the common case (`isekai-pipe`/
/// `isekai-ssh`, no need to control the underlying socket type). See
/// [`connect_multipath_with_socket`]'s docs for everything else.
pub async fn connect_multipath(
    bind: BindSpec,
    primary: RemoteSpec,
    secondaries: Vec<SecondaryPath>,
    event_tx: tokio::sync::mpsc::Sender<PathHealthEvent>,
) -> Result<MultipathConnection, TransportError> {
    use noq::Runtime as _;

    let socket = tokio::net::UdpSocket::bind(bind.local_addr)
        .await
        .map_err(|source| TransportError::Bind { addr: bind.local_addr, source })?;
    let std_socket =
        socket.into_std().map_err(|source| TransportError::Bind { addr: bind.local_addr, source })?;
    let async_socket = noq::TokioRuntime
        .wrap_udp_socket(std_socket)
        .map_err(|e| TransportError::EndpointSetup(e.to_string()))?;

    connect_multipath_with_socket(async_socket, primary, secondaries, event_tx).await
}

/// Connects to `primary` over `socket`, then opens every address in
/// `secondaries` as an additional simultaneous path on the same connection
/// (each via [`open_path_with_retry`], serialized one at a time —
/// `multipath_transport.rs` found opening multiple paths concurrently makes
/// every path but the first reliably fail validation on real hardware, see
/// that module's `establish_multipath_connection` comment). All paths use
/// `local_ip: None` (OS-default routing) — see this module's docs for why
/// that is the load-bearing constraint that keeps this working, unlike
/// Android's abandoned physical-interface variant.
///
/// Takes an already-constructed `Box<dyn noq::AsyncUdpSocket>` rather than
/// binding one itself (unlike [`connect_multipath`]) so a caller that needs
/// a non-default socket — e.g. `isekai-terminal-core`'s fault-injection-
/// wrapped `MultiUdpSocket`, used for both its own physical-path fan-out
/// (out of scope here, see this module's docs) and its real-device fault-
/// injection test harness — can supply it. `noq::AsyncUdpSocket` is a plain
/// `noq` trait, not an Android-specific type, so accepting it here doesn't
/// cross this crate's "no Android/UniFFI types" boundary (mirrors
/// `traits::QuicEndpointFactory::wrap_bound_socket`'s reason for existing
/// alongside `create_endpoint`).
///
/// [`PathHealthEvent`]s (currently just [`PathHealthEvent::NoViablePath`])
/// are sent on `event_tx` as paths degrade/recover/get abandoned — the
/// caller decides what queue depth and backpressure policy fit its use case,
/// so this function takes an already-constructed sender rather than owning
/// the channel itself.
pub async fn connect_multipath_with_socket(
    socket: Box<dyn noq::AsyncUdpSocket>,
    primary: RemoteSpec,
    secondaries: Vec<SecondaryPath>,
    event_tx: tokio::sync::mpsc::Sender<PathHealthEvent>,
) -> Result<MultipathConnection, TransportError> {
    let (client_config, _mismatch) = client_config_for(&primary.cert_sha256_hex, true)?;

    let endpoint = noq::Endpoint::new_with_abstract_socket(
        noq::EndpointConfig::default(), None, socket, Arc::new(noq::TokioRuntime),
    )
    .map_err(|e| TransportError::EndpointSetup(e.to_string()))?;
    // Paths opened later via `open_path` ride on the already-authenticated
    // connection (no fresh TLS handshake per path), so this is the only
    // place a client_config is needed — mirrors
    // `establish_multipath_connection`'s `set_default_client_config`.
    endpoint.set_default_client_config(client_config);

    info!("isekai-transport::multipath: connecting primary -> {}", primary.addr);
    let conn = endpoint
        .connect(primary.addr, &primary.server_name)
        .map_err(|e| TransportError::ConnectSetup(e.to_string()))?
        .await
        .map_err(|e| TransportError::Handshake(e.to_string()))?;
    info!("isekai-transport::multipath: primary path established");

    let tracker = PathHealthTracker::new();
    let primary_label: PathLabel = PRIMARY_PATH_LABEL.into();
    tracker.register_path(noq::PathId::ZERO, primary_label.clone());
    tracker.set(primary_label.clone(), path_health::PathState::Validated);
    path_health::spawn_health_monitor(conn.clone(), noq::PathId::ZERO, primary_label, tracker.clone(), event_tx.clone());

    spawn_path_event_listener(conn.clone(), tracker.clone(), event_tx.clone());

    if !secondaries.is_empty() {
        let conn2 = conn.clone();
        let tracker2 = tracker.clone();
        let event_tx2 = event_tx.clone();
        tokio::spawn(async move {
            // Serialized on purpose — see this function's doc comment.
            for secondary in secondaries {
                open_path_with_retry(&conn2, secondary.addr, secondary.label, &tracker2, &event_tx2).await;
            }
        });
    }

    Ok(MultipathConnection { conn, tracker, endpoint })
}

/// Watches `conn.path_events()` and keeps `tracker` in sync with paths this
/// module didn't itself open the health monitor for yet (`Established`), or
/// that the peer/network tore down out from under us (`Abandoned`/
/// `Discarded`) — mirrors `establish_multipath_connection`'s inline
/// `RUNTIME.spawn` block.
fn spawn_path_event_listener(
    conn: noq::Connection, tracker: PathHealthTracker, event_tx: tokio::sync::mpsc::Sender<PathHealthEvent>,
) {
    tokio::spawn(async move {
        let mut events = conn.path_events();
        while let Some(ev) = events.next().await {
            match ev {
                Ok(noq::PathEvent::Established { id, .. }) => {
                    if let Some(label) = tracker.label_for(id) {
                        tracker.set(label.clone(), path_health::PathState::Validated);
                        path_health::spawn_health_monitor(conn.clone(), id, label, tracker.clone(), event_tx.clone());
                    }
                }
                Ok(noq::PathEvent::Abandoned { id, reason, .. }) => {
                    info!("isekai-transport::multipath: path {id:?} abandoned: {reason:?}");
                    if let Some(label) = tracker.label_for(id) {
                        tracker.set(label, path_health::PathState::Failed);
                        path_health::notify_if_no_viable_path(&tracker, &event_tx);
                    }
                }
                Ok(noq::PathEvent::Discarded { id, .. }) => {
                    if let Some(label) = tracker.label_for(id) {
                        tracker.set(label, path_health::PathState::Failed);
                        path_health::notify_if_no_viable_path(&tracker, &event_tx);
                    }
                }
                Ok(_) => {}
                Err(_) => break, // connection closed
            }
        }
    });
}

/// Retries opening `target_addr` as a new simultaneous path (`local_ip:
/// None`) up to [`OPEN_PATH_MAX_ATTEMPTS`] times with exponential backoff —
/// ported from `multipath_transport.rs::open_path_with_retry`, minus its
/// `local_ip: Option<IpAddr>` parameter (always `None` here; see this
/// module's docs on why physical-interface-bound paths aren't ported).
async fn open_path_with_retry(
    conn: &noq::Connection, target_addr: SocketAddr, label: PathLabel, tracker: &PathHealthTracker,
    event_tx: &tokio::sync::mpsc::Sender<PathHealthEvent>,
) {
    let four_tuple = noq::FourTuple::from_remote(target_addr);
    let mut backoff = OPEN_PATH_INITIAL_BACKOFF;
    for attempt in 1..=OPEN_PATH_MAX_ATTEMPTS {
        info!("isekai-transport::multipath: opening path {label:?} -> {target_addr} (attempt {attempt}/{OPEN_PATH_MAX_ATTEMPTS})");
        let result = tokio::time::timeout(OPEN_PATH_TIMEOUT, conn.open_path(four_tuple, noq::PathStatus::Available)).await;
        match result {
            Ok(Ok(path)) => {
                info!("isekai-transport::multipath: path {label:?} established: id={:?}", path.id());
                tracker.register_path(path.id(), label.clone());
                tracker.set(label.clone(), path_health::PathState::Validated);
                path_health::spawn_health_monitor(conn.clone(), path.id(), label, tracker.clone(), event_tx.clone());
                return;
            }
            Ok(Err(e)) => warn!("isekai-transport::multipath: path {label:?} open_path failed (attempt {attempt}): {e}"),
            Err(_) => {
                warn!("isekai-transport::multipath: path {label:?} open_path timed out after {OPEN_PATH_TIMEOUT:?} (attempt {attempt})")
            }
        }
        tracker.set(label.clone(), path_health::PathState::Failed);
        if attempt < OPEN_PATH_MAX_ATTEMPTS {
            tokio::time::sleep(backoff).await;
            backoff *= 2;
        }
    }
    warn!("isekai-transport::multipath: giving up on path {label:?} after {OPEN_PATH_MAX_ATTEMPTS} attempts");
    path_health::notify_if_no_viable_path(tracker, event_tx);
}
