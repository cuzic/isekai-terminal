//! Lowercase hex encoding, shared by `isekai-ssh` and `isekai-bootstrap`.
//!
//! Both crates used to each carry several byte-identical copies of this
//! same tiny transform under different names (`hex_sha256` in three
//! `isekai-ssh` modules and two `isekai-bootstrap` modules, plus a `to_hex`
//! and an inline hex-encoding loop elsewhere in `isekai-ssh`). Consolidated
//! here for the same dependency-direction reason as [`crate::time`]: both
//! crates already depend on `isekai-trust`, never the other way around.

/// Lowercase hex encoding of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Hex-encoded SHA-256 digest of `bytes`.
pub fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encodes_bytes_lowercase() {
        assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn hex_sha256_matches_known_vector() {
        // sha256("") — a standard test vector.
        assert_eq!(hex_sha256(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }
}
