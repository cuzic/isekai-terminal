//! [`ReleaseManifest`]/[`SignedManifest`]: what a release-signing key signs
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic D). The manifest is signed rather than
//! the raw artifact bytes so one signature can carry platform/architecture/
//! version/channel metadata alongside the digest — a verifier checks all of
//! it together instead of trusting an out-of-band filename to imply the
//! platform.

use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};

/// One release artifact's signed metadata. Field values are free-form
/// strings deliberately (no closed enum for `platform`/`architecture`) —
/// the *signing* side is the trust boundary; a verifier compares against
/// its own caller-supplied expectation ([`crate::ExpectedTarget`]), so a
/// new platform string needs no schema change here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub version: String,
    pub platform: String,
    pub architecture: String,
    pub artifact_filename: String,
    pub size: u64,
    /// Lowercase hex-encoded SHA-256 of the artifact, 64 characters.
    pub sha256: String,
    /// Which `isekai-pipe`/`isekai-ssh` wire-protocol version(s) this
    /// artifact speaks — a free-form string (e.g. an exact version or a
    /// range) a verifier's caller interprets; this crate never parses it.
    pub protocol_compat: String,
    pub release_channel: String,
    /// Selects which entry in the verifier's [`crate::TrustedReleaseKeys`]
    /// signed this manifest. Carried on the manifest itself (not just the
    /// envelope) so it is covered by the signature — a key ID is metadata
    /// that a signer authenticates, not routing info a MITM should be free
    /// to rewrite.
    pub key_id: String,
}

/// A [`ReleaseManifest`] plus a signature over its canonical bytes
/// ([`canonical_manifest_bytes`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedManifest {
    pub manifest: ReleaseManifest,
    /// Lowercase hex-encoded ed25519 signature (64 bytes).
    pub signature: String,
}

/// The exact bytes a release-signing key signs, and a verifier re-derives
/// to check the signature against: the manifest's canonical JSON
/// serialization. Deterministic because [`ReleaseManifest`] is a
/// fixed-shape struct with no maps — `serde_json` always emits struct
/// fields in declaration order, so two independent serializations of an
/// equal manifest always produce identical bytes.
pub fn canonical_manifest_bytes(manifest: &ReleaseManifest) -> Vec<u8> {
    serde_json::to_vec(manifest).expect("ReleaseManifest always serializes")
}

/// Signs `manifest` with `signing_key`, producing the [`SignedManifest`] a
/// release process would publish. Exists mainly for tests and any future
/// signing CLI — this crate's primary consumer is the verifier, not the
/// signer (the signing key never needs to exist on a machine that only
/// verifies).
pub fn sign_manifest(manifest: ReleaseManifest, signing_key: &SigningKey) -> SignedManifest {
    let canonical = canonical_manifest_bytes(&manifest);
    let signature = signing_key.sign(&canonical);
    SignedManifest { manifest, signature: hex::encode(signature.to_bytes()) }
}

pub(crate) mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(bytes.as_ref().len() * 2);
        for byte in bytes.as_ref() {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    #[derive(Debug, PartialEq, Eq)]
    pub enum DecodeError {
        OddLength,
        NotHex,
    }

    pub fn decode(s: &str) -> Result<Vec<u8>, DecodeError> {
        if !s.len().is_multiple_of(2) {
            return Err(DecodeError::OddLength);
        }
        let mut out = Vec::with_capacity(s.len() / 2);
        let bytes = s.as_bytes();
        for chunk in bytes.chunks_exact(2) {
            let hi = nibble(chunk[0]).ok_or(DecodeError::NotHex)?;
            let lo = nibble(chunk[1]).ok_or(DecodeError::NotHex)?;
            out.push((hi << 4) | lo);
        }
        Ok(out)
    }

    fn nibble(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> ReleaseManifest {
        ReleaseManifest {
            version: "0.5.0".to_string(),
            platform: "linux".to_string(),
            architecture: "x86_64".to_string(),
            artifact_filename: "isekai-pipe-x86_64-unknown-linux-musl".to_string(),
            size: 123,
            sha256: "a".repeat(64),
            protocol_compat: "isekai-pipe/1".to_string(),
            release_channel: "stable".to_string(),
            key_id: "2026-07".to_string(),
        }
    }

    #[test]
    fn canonical_bytes_are_deterministic_across_equal_manifests() {
        let a = canonical_manifest_bytes(&sample_manifest());
        let b = canonical_manifest_bytes(&sample_manifest());
        assert_eq!(a, b);
    }

    #[test]
    fn canonical_bytes_change_when_any_field_changes() {
        let base = canonical_manifest_bytes(&sample_manifest());
        let mut changed = sample_manifest();
        changed.key_id = "2026-08".to_string();
        assert_ne!(base, canonical_manifest_bytes(&changed));
    }

    #[test]
    fn sign_manifest_produces_a_64_byte_hex_signature() {
        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let signed = sign_manifest(sample_manifest(), &signing_key);
        assert_eq!(signed.signature.len(), 128, "64 bytes hex-encoded is 128 chars");
        assert!(hex::decode(&signed.signature).is_ok());
    }

    #[test]
    fn hex_roundtrips() {
        let bytes = [0u8, 1, 2, 0xab, 0xff];
        let encoded = hex::encode(bytes);
        assert_eq!(encoded, "000102abff");
        assert_eq!(hex::decode(&encoded).unwrap(), bytes.to_vec());
    }

    #[test]
    fn hex_decode_rejects_odd_length_and_non_hex() {
        assert_eq!(hex::decode("abc").unwrap_err(), hex::DecodeError::OddLength);
        assert_eq!(hex::decode("zz").unwrap_err(), hex::DecodeError::NotHex);
    }
}
