//! Client for `seera-networks/axum-masque-rs`'s MASQUE relay
//! (`bound-udp-server`), built on `h3-noq` instead of the relay's own
//! `msquic`-based stack — see `~/isekai-terminal/rust-core/h3-noq` and this
//! crate's README (once written) for why. Implements the specific
//! non-standard capsule protocol and datagram framing that relay uses (not
//! generic RFC 9298 MASQUE), verified directly against
//! `seera-networks/axum-masque-rs` source.
//!
//! Deliberately does not implement the relay's WebSocket ICE-signaling
//! session (`/sessions/{id}/ws`, `SEERA_MAPPED_ADDR` capsule) — see
//! `capsule.rs` for why that's safe to omit. Hole punching instead reuses
//! the CONNECT-UDP-bind tunnel itself as the signaling channel; see
//! `punch_signal.rs`.

mod addr_codec;
pub mod capsule;
pub mod datagram_codec;
pub mod relay_client;
mod varint;

pub use capsule::{Capsule, CapsuleDecodeError, CapsuleReader};
pub use datagram_codec::{decode_datagram_payload, encode_datagram_payload, peek_context_id};
pub use relay_client::{
    connect_relay_agent, connect_relay_agent_via_qmux, connect_relay_agent_via_qmux_with_tls_config,
    connect_relay_agent_with_client_config, uplink_transport_config, RelayClientError, RelayUdpSocket,
};
