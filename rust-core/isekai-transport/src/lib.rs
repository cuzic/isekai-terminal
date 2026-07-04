//! QUIC connection establishment + relay-only HELLO/proof/ACK handshake,
//! extracted from `tssh-core`'s `helper_quic_transport.rs` /
//! `isekai_link_relay_transport.rs` so `isekai-ssh` (a plain CLI binary) can
//! reuse the same logic without depending on `tssh-core`, UniFFI, or any
//! Android-specific type (`ISEKAI_SSH_DESIGN.md` "実装方針", phase S-0d-1).
//!
//! Scope of this phase (S-0d-1, **relay connection only**):
//! - The `QuicEndpointFactory` / `QuicEndpoint` / `QuicConnection` /
//!   `ByteStream` traits (`traits.rs`), so relay-connection logic never
//!   touches a concrete socket type.
//! - `SystemQuicEndpointFactory` (`system.rs`): the CLI's concrete
//!   implementation, built directly on `noq` + `rustls` + a plain
//!   `tokio::net::UdpSocket`.
//! - `connect_via_relay` (`relay.rs`): HELLO/proof/ACK against a
//!   relay-assigned isekai-helper endpoint, reusing `isekai_protocol::hello`
//!   for the wire format.
//!
//! Explicitly **out of scope** for this phase (left for later phases per
//! `ISEKAI_SSH_DESIGN.md`'s フェーズ分割案):
//! - STUN/P2P connection establishment and reconnect/backoff policy (S-0d-2).
//! - `RESUME`/`RESUME_ACK`, the control stream, and isekai-helper's session
//!   table (S-4a onward).
//! - `--via` bootstrap/distribution (a separate crate, `isekai-bootstrap`,
//!   S-0e).
//!
//! This crate must never depend on UniFFI, Android-specific types, or
//! `tssh-core` itself — see `traits.rs`'s module docs for why the trait
//! boundary exists in the first place.

pub mod error;
pub mod proof;
pub mod relay;
pub mod system;
pub mod traits;
pub mod types;

pub use error::TransportError;
pub use proof::compute_proof;
pub use relay::{connect_via_relay, RelayTarget};
pub use system::SystemQuicEndpointFactory;
pub use traits::{ByteStream, QuicConnection, QuicEndpoint, QuicEndpointFactory};
pub use types::{BindSpec, RemoteSpec};
