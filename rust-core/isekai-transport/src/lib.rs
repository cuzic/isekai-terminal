//! QUIC connection establishment + HELLO/proof/ACK handshake (both relay and
//! STUN+SSH-rendezvous P2P), extracted from `tssh-core`'s
//! `helper_quic_transport.rs` / `isekai_link_relay_transport.rs` /
//! `isekai_stun_p2p_transport.rs` so `isekai-ssh` (a plain CLI binary) can
//! reuse the same logic without depending on `tssh-core`, UniFFI, or any
//! Android-specific type (`ISEKAI_SSH_DESIGN.md` "実装方針", phases S-0d-1/
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
//!   calculation. Not wired into an actual reconnect loop yet — that lands
//!   with resume support (S-4 onward).
//!
//! Explicitly **out of scope** for this phase (left for later phases per
//! `ISEKAI_SSH_DESIGN.md`'s フェーズ分割案):
//! - `RESUME`/`RESUME_ACK`, the control stream, and isekai-helper's session
//!   table (S-4a onward).
//! - `--via` bootstrap/distribution, and the SSH-bootstrap exchange of STUN
//!   observed addresses between peers (a separate crate, `isekai-bootstrap`,
//!   S-0e/S-6).
//!
//! This crate must never depend on UniFFI, Android-specific types, or
//! `tssh-core` itself — see `traits.rs`'s module docs for why the trait
//! boundary exists in the first place.

pub mod backoff;
pub mod error;
pub mod proof;
pub mod relay;
pub mod stun_p2p;
pub mod system;
pub mod traits;
pub mod types;

pub use backoff::BackoffPolicy;
pub use error::TransportError;
pub use proof::compute_proof;
pub use relay::{connect_via_relay, RelayTarget};
pub use stun_p2p::{connect_stun_p2p, StunP2pConnection, StunP2pTarget};
pub use system::SystemQuicEndpointFactory;
pub use traits::{
    ByteStream, ByteStreamReadHalf, ByteStreamWriteHalf, QuicConnection, QuicEndpoint, QuicEndpointFactory,
};
pub use types::{BindSpec, RemoteSpec};
