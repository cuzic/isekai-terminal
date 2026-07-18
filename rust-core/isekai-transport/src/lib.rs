//! QUIC connection establishment + HELLO/proof/ACK handshake (both relay and
//! STUN+SSH-rendezvous P2P), extracted from `isekai-terminal-core`'s
//! `isekai_pipe_quic_transport.rs` / `isekai_link_relay_transport.rs` /
//! `isekai_stun_p2p_transport.rs` so `isekai-ssh` (a plain CLI binary) can
//! reuse the same logic without depending on `isekai-terminal-core`, UniFFI, or any
//! Android-specific type (`archive/ISEKAI_SSH_DESIGN.md` "実装方針", phases S-0d-1/
//! S-0d-2).
//!
//! Scope covered so far:
//! - Connection establishment/backend selection itself (`AnyMuxFactory` /
//!   `AnyMuxEndpoint` / `AnyMuxConnection` / `AnyByteStream`, cert pinning,
//!   ALPN/exporter-label/transport tuning) now lives in the sibling
//!   `quicmux` crate — a generic, backend-agnostic (`noq`/`qmux`) crate this
//!   one depends on, so connection-establishment logic in this crate never
//!   touches a concrete socket type. `system.rs`/`qmux_relay.rs` are just
//!   this crate's own product-policy layer on top (which ALPN/timeouts to
//!   use) plus a thin `quicmux::AnyMuxFactory` constructor.
//! - `connect_via_relay` (`relay.rs`, S-0d-1): HELLO/proof/ACK against a
//!   relay-assigned isekai-helper endpoint, reusing `isekai_protocol::hello`
//!   for the wire format.
//! - `connect_stun_p2p` (`stun_p2p.rs`, S-0d-2): STUN self-observation +
//!   hole-punch probes + the same HELLO/proof/ACK handshake, reused via
//!   `relay::connect_and_handshake`, against a peer reached directly
//!   (no relay).
//! - `BackoffPolicy` (`backoff.rs`, S-0d-2): pure reconnect backoff/jitter
//!   calculation.
//! - `resume` (`resume.rs`, S-4c): control stream establishment
//!   (`CONTROL_HELLO`/`CONTROL_ACK`), `RESUME`/`RESUME_ACK` reconnection
//!   (`reconnect_and_resume`), and the `APP_ACK` background exchange
//!   (`spawn_app_ack_tasks`), ported from `isekai_pipe_quic_transport.rs`'s
//!   `open_control_stream`/`spawn_app_ack_tasks` and
//!   `isekai_link_relay_transport.rs`'s `reattach_fn`. `BackoffPolicy` is
//!   wired into an actual reconnect loop by `isekai-ssh`, not here — this
//!   crate only knows how to perform one resume attempt; deciding how many
//!   times and how long to keep retrying is `isekai-ssh`'s job (it also owns
//!   the C2H replay buffer/backpressure, `archive/ISEKAI_SSH_DESIGN.md`'s task
//!   split).
//! - `path_health` (isekai-terminal-core/isekai-transport crate共有化,
//!   multipath移植): path RTT/loss/black-hole classification and the
//!   ping-driven health monitor loop, ported and generalized from
//!   `isekai-terminal-core`'s `multipath_transport.rs` (`PathBroker`/
//!   `spawn_health_monitor`, real-hardware verified). See that module's docs
//!   for exactly which parts of the Android implementation this does and
//!   does not port (noq issue #738 means same-connection physical-interface
//!   multipath is a confirmed dead end, not ported).
//! - `multipath` (isekai-terminal-core/isekai-transport crate共有化):
//!   `connect_multipath` holds a primary QUIC path plus any number of
//!   secondary remote-address paths open simultaneously (`open_path`,
//!   `local_ip: None`) — the proven-working half of
//!   `multipath_transport.rs`'s Phase 9 work (path0/path1).
//! - `physical_interface` (isekai-terminal-core/isekai-transport crate共有化):
//!   binds a UDP socket to a specific physical network interface (via the
//!   vendored `quicsock` crate) for
//!   [`quicmux::AnyMuxRebinder::rebind_socket`] — the CLI/PC side of
//!   the proven-working reactive physical-interface failover
//!   (`rebind_abstract()`, Phase 9-4b). Android's own rebind path does not
//!   go through this module — see its docs.
//! - `dual_path` (UAV C2 OSS企画向け下準備): two independently-dialed,
//!   simultaneously-held [`AnyMuxConnection`]s to the same peer — one for
//!   reliable (stream-based) traffic, one for unreliable (datagram-based)
//!   traffic — each optionally bound to its own physical interface, so an
//!   app can spread traffic classes across separate physical links and pick
//!   explicitly which connection to use. Reuses `warm_standby.rs`'s
//!   "two independent QUIC connections" dial pattern (factored out to
//!   `physical_interface::connect_via_interface`) for a different purpose:
//!   both connections stay actively in use, not primary/standby failover.
//!   See that module's docs for why this doesn't fork `noq` or attempt
//!   per-frame path selection within one connection.
//!
//! Explicitly **out of scope** for this phase (left for later phases per
//! `archive/ISEKAI_SSH_DESIGN.md`'s フェーズ分割案):
//! - `--via` bootstrap/distribution, and the SSH-bootstrap exchange of STUN
//!   observed addresses between peers (a separate crate, `isekai-bootstrap`,
//!   S-0e/S-6).
//! - Resuming a `--mode stun` (STUN+SSH rendezvous P2P) connection — `resume`
//!   is currently only wired up for the relay path (`RelayTarget`); STUN
//!   mode's own known limitation (no recovery from NAT mapping loss) makes
//!   this lower priority, and is left as a follow-up (see
//!   `archive/ISEKAI_SSH_DESIGN.md`'s "isekai-sshでのNAT越え方式の既定").
//!
//! This crate must never depend on UniFFI, Android-specific types, or
//! `isekai-terminal-core` itself — see `quicmux`'s own module docs for the
//! (stricter) boundary that crate holds to (no dependency on this crate,
//! `isekai-protocol`, or anything isekai-specific at all).

pub mod attempt;
pub mod backoff;
pub mod candidate;
pub mod candidate_pool;
pub mod candidate_provider;
pub mod dual_path;
pub mod error;
pub mod generation_coordinator;
pub mod multipath;
pub mod path_health;
mod path_health_fsm;
pub mod physical_interface;
pub mod proof;
#[cfg(feature = "qmux-relay")]
pub mod qmux_relay;
pub mod race;
pub mod relay;
pub mod resume;
pub mod stun_p2p;
pub mod system;
pub mod telemetry;
pub mod warm_standby;

pub use attempt::AttemptFailure;
pub use backoff::BackoffPolicy;
pub use candidate::{
    validate_endpoint_identity, Candidate, CandidateClass, CandidateConversionError, CandidateDraft,
    CandidateDraftBatch, CandidateGeneration, CandidateId, CandidateKey, CandidateOrigin, CandidateOriginKind,
    CandidatePriority, CandidateRoute, CandidateSnapshot, CandidateValidity, CertificatePinError,
    CertificatePinSha256, NormalizedServerName, ServerNameError, TransportIntent, TransportRoute,
    LEGACY_INTENT_PROVIDER_ID,
};
pub use candidate_pool::{CandidatePool, Clock, StaleGeneration, SystemClock};
pub use dual_path::{connect_dual_path, connect_dual_path_best_effort, DualPathConnections, DualPathEndpoint};
pub use generation_coordinator::{
    AdvanceGenerationError, GenerationCoordinator, RoundContext, DEFAULT_MAX_GENERATION_ADVANCES,
};
pub use race::{race_direct_and_relay, DirectRelayRaceTargets, RaceConnectError, RaceOutcome, RaceWinner, DEFAULT_RELAY_DELAY};
pub use candidate_provider::{
    CandidateProvider, CandidateProviderError, ConfigRelayProvider, ConfigStunProvider, GatherContext,
    LegacyIntentProvider,
};
pub use error::{StaleTrustSignal, TransportError};
pub use multipath::{connect_multipath, connect_multipath_with_socket, MultipathConnection, SecondaryPath, PRIMARY_PATH_LABEL};
pub use physical_interface::{bind_physical_interface, InterfaceIndex};
pub use path_health::{
    classify_path_health, has_zero_response, notify_if_no_viable_path, spawn_health_monitor, PathHealthEvent,
    PathHealthTracker, PathLabel, PathState,
};
pub use proof::compute_proof;
#[cfg(feature = "qmux-relay")]
pub use qmux_relay::{qmux_relay_factory, QMUX_ALPN};
pub use relay::{connect_via_relay, connect_via_relay_with_connection, RelayTarget};
pub use resume::{
    connect_via_relay_resumable, connect_via_relay_resumable_with_fallback, open_control_stream,
    reconnect_and_resume, spawn_app_ack_tasks, AppAckCounters, AppAckTasks, ControlStream, ResumableRelaySession,
    ResumeAckOutcome, SequentialConnectError, SequentialFailure, SequentialRelayCandidate,
};
pub use stun_p2p::{
    connect_stun_p2p, connect_stun_p2p_on_socket, connect_stun_p2p_with_fallback, SequentialStunCandidate,
    SequentialStunConnectError, StunP2pConnection, StunP2pTarget,
};
pub use system::system_quic_factory;
pub use telemetry::{CandidateAttempt, CandidateIdentity, CandidateOutcome};
pub use warm_standby::{PromotedConnection, WarmStandby, WarmStandbyError, PROBE_TIMEOUT};

// Re-exported so downstream crates (e.g. `isekai-ssh`/`isekai-pipe`) that
// only depend on `isekai-transport` don't also need a direct `quicmux`
// dependency just to name the connection-establishment types this crate's
// own public functions take/return (`AnyMuxFactory`, `BindSpec`/
// `RemoteSpec`, ...). `traits.rs`/`types.rs` used to define isekai-
// transport's own equivalents of these; they now live in `quicmux` and are
// re-exported here under their new names instead.
pub use quicmux::{
    AnyByteStream, AnyByteStreamReadHalf, AnyByteStreamWriteHalf, AnyMuxConnection, AnyMuxEndpoint, AnyMuxFactory,
    AnyMuxRebinder, BindSpec, MuxError, RemoteSpec,
};

// Re-exported so downstream crates (e.g. `isekai-ssh`) that only depend on
// `isekai-transport` don't also need a direct `isekai-protocol` dependency
// just to name `SessionId`/the C2H/H2C offset types in their own resume
// bookkeeping (`archive/ISEKAI_SSH_DESIGN.md` Phase S-4c task split).
pub use isekai_protocol::attach::ConnectionGeneration;
pub use isekai_protocol::offset::{C2hHelperCommittedOffset, C2hSentOffset, H2cClientDeliveredOffset, H2cSentOffset};
pub use isekai_protocol::resume::ResumeRejectReason;
pub use isekai_protocol::session_id::SessionId;
