//! `quicmux`: a backend-agnostic abstraction over two QUIC-ish multiplexed-
//! stream transports —
//! [`noq`](https://docs.rs/noq) (quinn's multipath fork, UDP-based) and
//! [`qmux`](https://github.com/moq-dev/web-transport) (draft-ietf-quic-qmux,
//! QUIC's stream API polyfilled over TLS-over-TCP) — behind one small set of
//! enum types: [`AnyMuxFactory`] → [`AnyMuxEndpoint`] → [`AnyMuxConnection`]
//! → [`AnyByteStream`], plus [`AnyMuxRebinder`].
//!
//! Extracted out of `isekai-transport`, which used to hand-roll backend
//! selection itself via a `dyn QuicEndpointFactory`/`dyn QuicConnection`/
//! `dyn ByteStream` trait-object hierarchy. This crate exists to let that
//! selection logic (and, more broadly, any future consumer facing the same
//! "noq vs. qmux" choice — e.g. `isekai-terminal-core` eventually) depend on
//! one generic, potentially publishable crate instead.
//!
//! # Design notes
//!
//! - **Enum-based, not trait-object-based.** Every layer here is a plain
//!   enum with a variant per compiled-in backend, not an object-safe trait.
//!   With exactly two backends chosen once at startup (not an open set a
//!   downstream crate might add more of), an enum is simpler than a trait
//!   hierarchy and avoids `Box<dyn Trait>` indirection on every call.
//! - **`noq` and `qmux` are both optional cargo features** (`noq`/`qmux`,
//!   see this crate's `Cargo.toml`). A consumer that only cares about one
//!   backend — or only wants [`race_with_stagger`], which needs neither —
//!   doesn't have to compile or link the other.
//! - **No dependency on `isekai-transport`, `isekai-protocol`, or any
//!   isekai-specific type.** ALPN, the exporter label, and QUIC transport
//!   tuning (idle timeout, keepalive, stream limits) are supplied by the
//!   caller via [`MuxClientConfig`] — this crate has no built-in default and
//!   never references anything isekai-specific by name. Errors are reported
//!   through this crate's own [`MuxError`], not any caller's error type.
//! - **[`AnyByteStream`] keeps a combined read/write/shutdown/split() shape**
//!   — not split into separate send/recv types with `finish`/`reset`/`stop`.
//!   No current caller's wire protocol uses stream reset, so that
//!   finer-grained API would be speculative generality; see that type's docs.

mod cert;
mod config;
mod error;
#[cfg(feature = "noq")]
pub mod noq_backend;
#[cfg(feature = "qmux")]
pub mod qmux_backend;
mod mux;
mod race;
mod resume;
mod types;

pub use cert::{CertMismatchSlot, PinnedCertVerifier};
pub use config::{MuxClientConfig, MuxServerConfig};
pub use error::MuxError;
pub use mux::{
    AnyByteStream, AnyByteStreamReadHalf, AnyByteStreamWriteHalf, AnyMuxConnection, AnyMuxEndpoint, AnyMuxFactory, AnyMuxIncoming,
    AnyMuxListener, AnyMuxRebinder,
};
pub use race::{race_with_stagger, Winner};
pub use resume::{
    accept_resume, decode_resume_request, request_resume, respond_resume_accepted, respond_resume_rejected, ReplayBuffer, ResumeAcceptor,
    ResumeAckOutcome, ResumeDecision, ResumeRejectReason, ResumeRequest, ResumeRequestError, FRAME_RESUME, FRAME_RESUME_ACK,
    FRAME_RESUME_REJECT,
};
pub use types::{BindSpec, RemoteSpec};

#[cfg(feature = "noq")]
pub use noq_backend::{noq_client_config, noq_server_config};
