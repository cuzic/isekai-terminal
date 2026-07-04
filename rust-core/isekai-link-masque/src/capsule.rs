//! The custom HTTP capsule protocol `axum-masque-rs`'s CONNECT-UDP-bind
//! endpoint uses to negotiate datagram address-compression contexts. These
//! ride the CONNECT-UDP-bind request/response *stream* (ordinary HTTP/3 DATA
//! frames), not the QUIC Datagram channel that carries the actual forwarded
//! UDP payloads (see `datagram_codec.rs` for that).
//!
//! Capsule type values (`0x11`/`0x12`/`0x13`) are `axum-masque-rs`-specific,
//! not IANA-registered RFC 9298 capsule types — verified directly against
//! `bound_udp/service.rs` in that repository. `0x40` (`SEERA_MAPPED_ADDR`) is
//! deliberately not implemented here: it only has an effect when paired with
//! the relay's WebSocket ICE-signaling session (`seera-signaling-session-id`
//! header), which this crate does not use — hole punching here instead
//! reuses the already-established CONNECT-UDP-bind tunnel itself as the
//! signaling channel (see `punch_signal.rs`). Confirmed in
//! `bound_udp/service.rs` that a `0x40` capsule sent without that header is
//! silently ignored server-side (not an error, not a rejection), so omitting
//! it is safe.

use crate::addr_codec::{decode_addr, encode_addr};
use crate::varint::{decode_var_int, encode_var_int};
use std::net::SocketAddr;

const COMPRESSION_ASSIGN: u64 = 0x11;
const COMPRESSION_ACK: u64 = 0x12;
const COMPRESSION_CLOSE: u64 = 0x13;

/// A parsed capsule. `Unknown` preserves the type so callers can log it;
/// `axum-masque-rs`'s own server treats unrecognized capsule types as a
/// policy violation (see `bound_udp/service.rs`), so callers should treat
/// receiving one as noteworthy rather than silently ignoring it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capsule {
    /// Register (or re-register) `context_id` for `addr`. `addr = None`
    /// means "uncompressed": every datagram using this context_id must
    /// carry its own `ip_version + addr + port` prefix (see
    /// `datagram_codec.rs`) rather than omitting it.
    CompressionAssign {
        context_id: u64,
        addr: Option<SocketAddr>,
    },
    /// Sent by the peer in response to `CompressionAssign` once both
    /// forwarding directions have registered the context id successfully.
    CompressionAck { context_id: u64 },
    /// Sent by the peer in response to `CompressionAssign` if registration
    /// failed on either forwarding direction (in which case the peer also
    /// unregisters whichever direction *did* succeed).
    CompressionClose { context_id: u64 },
    /// A capsule type this crate doesn't implement.
    Unknown { capsule_type: u64 },
}

impl Capsule {
    pub fn encode(&self) -> Vec<u8> {
        let (capsule_type, payload) = match self {
            Capsule::CompressionAssign { context_id, addr } => {
                let mut payload = encode_var_int(*context_id);
                match addr {
                    Some(addr) => payload.extend_from_slice(&encode_addr(*addr)),
                    None => payload.push(0), // ip_version = 0 => uncompressed
                }
                (COMPRESSION_ASSIGN, payload)
            }
            Capsule::CompressionAck { context_id } => (COMPRESSION_ACK, encode_var_int(*context_id)),
            Capsule::CompressionClose { context_id } => {
                (COMPRESSION_CLOSE, encode_var_int(*context_id))
            }
            Capsule::Unknown { capsule_type } => {
                panic!("cannot encode an Unknown capsule (type {capsule_type}); it exists only to represent capsules read from a peer")
            }
        };
        let mut buf = encode_var_int(capsule_type);
        buf.extend_from_slice(&encode_var_int(payload.len() as u64));
        buf.extend_from_slice(&payload);
        buf
    }

    /// Attempts to decode a single capsule from the front of `data`.
    ///
    /// Returns:
    /// - `Ok(Some((capsule, rest)))` if a complete capsule was parsed.
    /// - `Ok(None)` if `data` doesn't yet contain a complete capsule header
    ///   or payload (caller should buffer more bytes and retry).
    /// - `Err(_)` if the capsule is malformed in a way more data can't fix
    ///   (e.g. a `CompressionAssign` payload missing its context id).
    pub fn decode(data: &[u8]) -> Result<Option<(Capsule, &[u8])>, CapsuleDecodeError> {
        let Some((capsule_type, after_type)) = decode_var_int(data) else {
            return Ok(None);
        };
        let Some((length, after_length)) = decode_var_int(after_type) else {
            return Ok(None);
        };
        let payload_len =
            usize::try_from(length).map_err(|_| CapsuleDecodeError::LengthOverflow)?;
        if after_length.len() < payload_len {
            return Ok(None); // incomplete payload; wait for more bytes
        }
        let payload = &after_length[..payload_len];
        let rest = &after_length[payload_len..];

        let capsule = match capsule_type {
            COMPRESSION_ASSIGN => {
                let (context_id, addr_payload) =
                    decode_var_int(payload).ok_or(CapsuleDecodeError::MissingContextId)?;
                let ip_version = *addr_payload
                    .first()
                    .ok_or(CapsuleDecodeError::MissingIpVersion)?;
                let addr = if ip_version == 0 {
                    None
                } else {
                    let (addr, _) =
                        decode_addr(addr_payload).ok_or(CapsuleDecodeError::MalformedAddr)?;
                    Some(addr)
                };
                Capsule::CompressionAssign { context_id, addr }
            }
            COMPRESSION_ACK => {
                let (context_id, _) =
                    decode_var_int(payload).ok_or(CapsuleDecodeError::MissingContextId)?;
                Capsule::CompressionAck { context_id }
            }
            COMPRESSION_CLOSE => {
                let (context_id, _) =
                    decode_var_int(payload).ok_or(CapsuleDecodeError::MissingContextId)?;
                Capsule::CompressionClose { context_id }
            }
            other => Capsule::Unknown { capsule_type: other },
        };
        Ok(Some((capsule, rest)))
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CapsuleDecodeError {
    #[error("capsule payload length exceeds usize range")]
    LengthOverflow,
    #[error("CompressionAssign/Ack/Close capsule missing context id")]
    MissingContextId,
    #[error("CompressionAssign capsule missing ip_version byte")]
    MissingIpVersion,
    #[error("CompressionAssign capsule address payload is malformed or truncated")]
    MalformedAddr,
}

/// Accumulates bytes arriving in arbitrary chunks (as HTTP/3 DATA frames do)
/// and yields complete capsules as they become available, mirroring
/// `axum-masque-rs`'s own `BytesMut`-accumulation loop in
/// `bound_udp/service.rs`.
#[derive(Default)]
pub struct CapsuleReader {
    buf: Vec<u8>,
}

impl CapsuleReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends newly-received bytes to the internal buffer.
    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Pops and returns the next complete capsule, if one is fully buffered.
    pub fn next_capsule(&mut self) -> Result<Option<Capsule>, CapsuleDecodeError> {
        match Capsule::decode(&self.buf)? {
            Some((capsule, rest)) => {
                let consumed = self.buf.len() - rest.len();
                self.buf.drain(..consumed);
                Ok(Some(capsule))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compression_assign_uncompressed_round_trips() {
        let capsule = Capsule::CompressionAssign {
            context_id: 1,
            addr: None,
        };
        let encoded = capsule.encode();
        let (decoded, rest) = Capsule::decode(&encoded).unwrap().unwrap();
        assert_eq!(decoded, capsule);
        assert!(rest.is_empty());
    }

    #[test]
    fn compression_assign_with_ipv4_addr_round_trips() {
        let addr: SocketAddr = "203.0.113.5:51820".parse().unwrap();
        let capsule = Capsule::CompressionAssign {
            context_id: 7,
            addr: Some(addr),
        };
        let encoded = capsule.encode();
        let (decoded, rest) = Capsule::decode(&encoded).unwrap().unwrap();
        assert_eq!(decoded, capsule);
        assert!(rest.is_empty());
    }

    #[test]
    fn compression_assign_with_ipv6_addr_round_trips() {
        let addr: SocketAddr = "[2001:db8::1]:51820".parse().unwrap();
        let capsule = Capsule::CompressionAssign {
            context_id: 42,
            addr: Some(addr),
        };
        let encoded = capsule.encode();
        let (decoded, rest) = Capsule::decode(&encoded).unwrap().unwrap();
        assert_eq!(decoded, capsule);
        assert!(rest.is_empty());
    }

    #[test]
    fn compression_ack_and_close_round_trip() {
        for capsule in [
            Capsule::CompressionAck { context_id: 5 },
            Capsule::CompressionClose { context_id: 5 },
        ] {
            let encoded = capsule.encode();
            let (decoded, rest) = Capsule::decode(&encoded).unwrap().unwrap();
            assert_eq!(decoded, capsule);
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn decode_reports_incomplete_data_as_none_not_error() {
        let full = Capsule::CompressionAck { context_id: 5 }.encode();
        for cut in 1..full.len() {
            assert_eq!(
                Capsule::decode(&full[..cut]).unwrap(),
                None,
                "truncated at {cut} bytes should be incomplete, not an error"
            );
        }
    }

    #[test]
    fn decode_unknown_capsule_type_preserves_type_and_skips_payload() {
        let mut raw = encode_var_int(0x99);
        raw.extend_from_slice(&encode_var_int(3));
        raw.extend_from_slice(b"abc");
        raw.extend_from_slice(b"trailing");
        let (decoded, rest) = Capsule::decode(&raw).unwrap().unwrap();
        assert_eq!(decoded, Capsule::Unknown { capsule_type: 0x99 });
        assert_eq!(rest, b"trailing");
    }

    #[test]
    fn decode_rejects_compression_assign_missing_context_id() {
        let mut raw = encode_var_int(COMPRESSION_ASSIGN);
        raw.extend_from_slice(&encode_var_int(0)); // zero-length payload
        assert_eq!(
            Capsule::decode(&raw),
            Err(CapsuleDecodeError::MissingContextId)
        );
    }

    #[test]
    fn reader_accumulates_capsules_split_across_multiple_feeds() {
        let capsule = Capsule::CompressionAssign {
            context_id: 1,
            addr: Some("127.0.0.1:9000".parse().unwrap()),
        };
        let encoded = capsule.encode();
        let (first_half, second_half) = encoded.split_at(encoded.len() / 2);

        let mut reader = CapsuleReader::new();
        reader.feed(first_half);
        assert_eq!(reader.next_capsule().unwrap(), None);
        reader.feed(second_half);
        assert_eq!(reader.next_capsule().unwrap(), Some(capsule));
    }

    #[test]
    fn reader_yields_multiple_capsules_fed_together() {
        let a = Capsule::CompressionAck { context_id: 1 };
        let b = Capsule::CompressionClose { context_id: 2 };
        let mut combined = a.encode();
        combined.extend_from_slice(&b.encode());

        let mut reader = CapsuleReader::new();
        reader.feed(&combined);
        assert_eq!(reader.next_capsule().unwrap(), Some(a));
        assert_eq!(reader.next_capsule().unwrap(), Some(b));
        assert_eq!(reader.next_capsule().unwrap(), None);
    }
}
