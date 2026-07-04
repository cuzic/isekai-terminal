//! Encodes/decodes the UDP-forwarding payload carried inside QUIC Datagrams
//! (RFC 9221) on a CONNECT-UDP-bind stream, as `axum-masque-rs` lays it out
//! (`masque/from_quic_to_udp.rs`/`from_udp_to_quic.rs`):
//!
//! `context_id(varint) || [ip_version(1B) + addr + port(2B BE)]? || udp_payload`
//!
//! The address prefix is present only for "uncompressed" context ids (the
//! ones registered via `Capsule::CompressionAssign { addr: None, .. }`,
//! `ip_version = 0`); once a context id is registered *with* an address via
//! `CompressionAssign`, subsequent datagrams for it omit the prefix entirely
//! and the peer looks the address up from its own registration table.
//! Because arbitrary payload bytes could coincidentally look like a valid
//! address prefix, decoding always requires the caller to already know
//! (from its own context-id registration table, exactly as
//! `axum-masque-rs`'s own `compression_info` map does) whether this
//! particular context id carries one — there is no self-describing/heuristic
//! decode.
//!
//! Note: this is the payload *after* h3-datagram has already stripped its
//! own RFC 9297 quarter-stream-id framing (`h3_datagram::datagram::Datagram`
//! handles that layer; see `relay_client.rs`).

use crate::addr_codec::{decode_addr, encode_addr};
use crate::varint::{decode_var_int, encode_var_int};
use bytes::{BufMut, Bytes, BytesMut};
use std::net::SocketAddr;

/// Encodes a datagram-forwarding payload for `context_id`.
///
/// Pass `Some(addr)` for an uncompressed context id (every datagram must
/// carry the address); pass `None` once the context id has been registered
/// with a fixed address via `CompressionAssign` and no longer needs it.
pub fn encode_datagram_payload(context_id: u64, addr: Option<SocketAddr>, payload: &[u8]) -> Bytes {
    let mut buf = BytesMut::with_capacity(1 + 19 + payload.len());
    buf.put_slice(&encode_var_int(context_id));
    if let Some(addr) = addr {
        buf.put_slice(&encode_addr(addr));
    }
    buf.put_slice(payload);
    buf.freeze()
}

/// Decodes a datagram-forwarding payload. `context_is_compressed` must
/// reflect what the caller's own context-id registration table says about
/// `context_id` (only knowable after peeking it — see `peek_context_id` for
/// extracting just the context id before deciding).
pub fn decode_datagram_payload(
    data: &[u8],
    context_is_compressed: bool,
) -> Option<(u64, Option<SocketAddr>, &[u8])> {
    let (context_id, rest) = decode_var_int(data)?;
    if context_is_compressed {
        Some((context_id, None, rest))
    } else {
        let (addr, payload_rest) = decode_addr(rest)?;
        Some((context_id, Some(addr), payload_rest))
    }
}

/// Extracts just the context id, without assuming anything about whether an
/// address prefix follows. Callers use this to look up
/// `context_is_compressed` in their own table before calling
/// `decode_datagram_payload`.
pub fn peek_context_id(data: &[u8]) -> Option<u64> {
    decode_var_int(data).map(|(context_id, _)| context_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_uncompressed_ipv4() {
        let addr: SocketAddr = "127.0.0.1:4433".parse().unwrap();
        let encoded = encode_datagram_payload(1, Some(addr), b"hello");
        assert_eq!(peek_context_id(&encoded), Some(1));
        let (context_id, decoded_addr, payload) =
            decode_datagram_payload(&encoded, false).unwrap();
        assert_eq!(context_id, 1);
        assert_eq!(decoded_addr, Some(addr));
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn round_trips_uncompressed_ipv6() {
        let addr: SocketAddr = "[2001:db8::1]:4433".parse().unwrap();
        let encoded = encode_datagram_payload(1, Some(addr), b"hello");
        let (context_id, decoded_addr, payload) =
            decode_datagram_payload(&encoded, false).unwrap();
        assert_eq!(context_id, 1);
        assert_eq!(decoded_addr, Some(addr));
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn round_trips_compressed_no_addr() {
        let encoded = encode_datagram_payload(7, None, b"world");
        assert_eq!(peek_context_id(&encoded), Some(7));
        let (context_id, decoded_addr, payload) =
            decode_datagram_payload(&encoded, true).unwrap();
        assert_eq!(context_id, 7);
        assert_eq!(decoded_addr, None);
        assert_eq!(payload, b"world");
    }

    #[test]
    fn empty_payload_round_trips() {
        let encoded = encode_datagram_payload(3, None, b"");
        let (context_id, addr, payload) = decode_datagram_payload(&encoded, true).unwrap();
        assert_eq!(context_id, 3);
        assert_eq!(addr, None);
        assert!(payload.is_empty());
    }

    #[test]
    fn decode_rejects_truncated_address_prefix() {
        // A context id followed by only 3 bytes: not enough for even an
        // ipv4 address+port (needs 1 + 4 + 2 = 7).
        let mut raw = encode_var_int(1);
        raw.extend_from_slice(&[4, 1, 2]);
        assert_eq!(decode_datagram_payload(&raw, false), None);
    }
}
