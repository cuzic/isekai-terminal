//! QUIC-style variable-length integer (RFC 9000 §16), used both by the
//! generic HTTP capsule framing (`type || length || payload`) and by
//! `seera-networks/axum-masque-rs`'s custom capsules/datagram context_id
//! prefix. Decoding matches `axum-masque-rs`'s `decode_var_int` byte-for-byte
//! (verified against its source and its own test vectors); encoding here
//! always picks the RFC-correct (minimal) length rather than replicating a
//! quirk in that crate's encoder that overshoots for the `2^22..2^30` range
//! (irrelevant for us: context ids/capsule types/lengths we send are always
//! tiny), since decode only depends on the 2-bit length prefix, not on the
//! sender having picked the minimal length.

/// Decodes a single QUIC varint from the front of `data`.
///
/// Returns the decoded value and the remaining, unconsumed slice. Returns
/// `None` if `data` is empty or shorter than the length the first byte's
/// 2-bit prefix declares (i.e. the varint is incomplete/truncated).
pub fn decode_var_int(data: &[u8]) -> Option<(u64, &[u8])> {
    if data.is_empty() {
        return None;
    }
    let mut v: u64 = data[0].into();
    let prefix = v >> 6;
    let length = 1usize << prefix;

    if data.len() < length {
        return None;
    }
    v &= 0x3f;
    for b in data.iter().take(length).skip(1) {
        v = (v << 8) + u64::from(*b);
    }
    Some((v, &data[length..]))
}

/// Encodes `v` as a QUIC varint using the minimal length that can hold it.
///
/// # Panics
/// Panics if `v` exceeds the varint range (`2^62 - 1`); none of our values
/// (capsule types, context ids, lengths) can realistically reach that.
pub fn encode_var_int(v: u64) -> Vec<u8> {
    const MAX_VARINT: u64 = (1 << 62) - 1;
    assert!(v <= MAX_VARINT, "value {v} exceeds QUIC varint range");

    let (length, prefix): (usize, u8) = if v < (1 << 6) {
        (1, 0b00)
    } else if v < (1 << 14) {
        (2, 0b01)
    } else if v < (1 << 30) {
        (4, 0b10)
    } else {
        (8, 0b11)
    };

    let mut buf = vec![0u8; length];
    let mut rem = v;
    for i in (0..length).rev() {
        buf[i] = (rem & 0xff) as u8;
        rem >>= 8;
    }
    buf[0] |= prefix << 6;
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_boundary_values() {
        for &v in &[
            0u64,
            1,
            63,           // largest 1-byte value
            64,           // smallest 2-byte value
            16383,        // largest 2-byte value
            16384,        // smallest 4-byte value
            1073741823,   // largest 4-byte value
            1073741824,   // smallest 8-byte value
            (1u64 << 62) - 1, // largest possible varint
        ] {
            let encoded = encode_var_int(v);
            let (decoded, rest) = decode_var_int(&encoded).expect("must decode");
            assert_eq!(decoded, v, "round-trip mismatch for {v}");
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn encode_picks_minimal_length() {
        assert_eq!(encode_var_int(0).len(), 1);
        assert_eq!(encode_var_int(63).len(), 1);
        assert_eq!(encode_var_int(64).len(), 2);
        assert_eq!(encode_var_int(16383).len(), 2);
        assert_eq!(encode_var_int(16384).len(), 4);
        assert_eq!(encode_var_int(1073741823).len(), 4);
        assert_eq!(encode_var_int(1073741824).len(), 8);
    }

    // Test vectors transcribed directly from axum-masque-rs's own decode_var_int
    // tests, to confirm our decoder matches its behavior byte-for-byte.
    #[test]
    fn decode_matches_axum_masque_rs_reference_vectors() {
        assert!(decode_var_int(&[]).is_none());
        assert!(decode_var_int(&[0b01_000000]).is_none()); // 2-byte prefix, only 1 byte given
        assert!(decode_var_int(&[0b10_000000, 0, 0]).is_none()); // 4-byte prefix, only 3 bytes given

        let mut encoded = encode_var_int(42);
        encoded.extend_from_slice(&[7, 8, 9]);
        let (decoded, rest) = decode_var_int(&encoded).expect("must decode");
        assert_eq!(decoded, 42);
        assert_eq!(rest, [7, 8, 9]);
    }

    #[test]
    fn decode_empty_after_consuming_exact_length() {
        let encoded = encode_var_int(300);
        let (decoded, rest) = decode_var_int(&encoded).expect("must decode");
        assert_eq!(decoded, 300);
        assert!(rest.is_empty());
    }
}
