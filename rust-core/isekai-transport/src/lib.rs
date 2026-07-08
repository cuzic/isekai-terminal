//! QUIC connection establishment + HELLO/proof/ACK handshake (both relay and
//! STUN+SSH-rendezvous P2P), extracted from `isekai-terminal-core`'s
//! `isekai_pipe_quic_transport.rs` / `isekai_link_relay_transport.rs` /
//! `isekai_stun_p2p_transport.rs` so `isekai-ssh` (a plain CLI binary) can
//! reuse the same logic without depending on `isekai-terminal-core`, UniFFI, or any
//! Android-specific type (`archive/ISEKAI_SSH_DESIGN.md` "実装方針", phases S-0d-1/
//! S-0d-2).
//!
//! Scope covered so far:
//! - The `QuicEndpointFactory` / `QuicEndpoint` / `QuicConnection` /
//!   `ByteStream` traits (`traits.rs`), so connection-establishment logic
//!   never touches a concrete socket type.
//! - `SystemQuicEndpointFactory` (`system.rs`): the CLI's concrete
//!   implementation, built directly on `noq` + `rustls` + a plain
//!   `tokio::net::UdpSocket`.
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
//! `isekai-terminal-core` itself — see `traits.rs`'s module docs for why the trait
//! boundary exists in the first place.

pub mod attempt;
pub mod backoff;
pub mod candidate_pool;
pub mod candidate_provider;
pub mod error;
pub mod proof;
pub mod relay;
pub mod resume;
pub mod stun_p2p;
pub mod system;
pub mod telemetry;
pub mod traits;
pub mod types;

pub use attempt::AttemptFailure;
pub use backoff::BackoffPolicy;
pub use candidate_pool::{CandidatePool, Clock, StaleGeneration, SystemClock};
pub use candidate_provider::{
    CandidateProvider, CandidateProviderError, ConfigRelayProvider, GatherContext, LegacyIntentProvider,
};
pub use error::TransportError;
pub use proof::compute_proof;
pub use relay::{connect_via_relay, RelayTarget};
pub use resume::{
    connect_via_relay_resumable, open_control_stream, reconnect_and_resume, spawn_app_ack_tasks, AppAckCounters,
    AppAckTasks, ControlStream, ResumableRelaySession, ResumeAckOutcome,
};
pub use stun_p2p::{connect_stun_p2p, StunP2pConnection, StunP2pTarget};
pub use system::SystemQuicEndpointFactory;
pub use telemetry::{CandidateAttempt, CandidateIdentity, CandidateOutcome};
pub use traits::{
    ByteStream, ByteStreamReadHalf, ByteStreamWriteHalf, QuicConnection, QuicEndpoint, QuicEndpointFactory,
};
pub use types::{BindSpec, RemoteSpec};

// Re-exported so downstream crates (e.g. `isekai-ssh`) that only depend on
// `isekai-transport` don't also need a direct `isekai-protocol` dependency
// just to name `SessionId`/the C2H/H2C offset types in their own resume
// bookkeeping (`archive/ISEKAI_SSH_DESIGN.md` Phase S-4c task split).
pub use isekai_protocol::offset::{C2hHelperCommittedOffset, C2hSentOffset, H2cClientDeliveredOffset, H2cSentOffset};
pub use isekai_protocol::resume::ResumeRejectReason;
pub use isekai_protocol::session_id::SessionId;
