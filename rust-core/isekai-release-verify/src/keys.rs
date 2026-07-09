//! [`TrustedReleaseKeys`]: the set of release-signing public keys a
//! verifier accepts, keyed by [`crate::ReleaseManifest::key_id`].
//!
//! This crate deliberately has no opinion on *where* trusted keys come from
//! (an embedded constant, a file shipped alongside the binary, a CLI flag)
//! — key provisioning and rotation policy is `ISEKAI_PIPE_DESIGN.md` §8
//! Epic D's own still-open sub-task; this type is just the in-memory
//! registry a verifier consults.

use std::collections::BTreeMap;

use ed25519_dalek::VerifyingKey;

use crate::manifest::hex;

#[derive(Debug, Default, Clone)]
pub struct TrustedReleaseKeys {
    keys: BTreeMap<String, VerifyingKey>,
}

impl TrustedReleaseKeys {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key_id: impl Into<String>, key: VerifyingKey) {
        self.keys.insert(key_id.into(), key);
    }

    /// Registers a key given as a lowercase-hex-encoded 32-byte ed25519
    /// public key (the form a CLI flag or config file would carry it in).
    pub fn insert_hex(&mut self, key_id: impl Into<String>, hex_pubkey: &str) -> Result<(), KeyLoadError> {
        let bytes = hex::decode(hex_pubkey).map_err(|_| KeyLoadError::NotHex)?;
        let array: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| KeyLoadError::WrongLength(v.len()))?;
        let key = VerifyingKey::from_bytes(&array).map_err(|_| KeyLoadError::InvalidKey)?;
        self.insert(key_id, key);
        Ok(())
    }

    pub fn get(&self, key_id: &str) -> Option<&VerifyingKey> {
        self.keys.get(key_id)
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyLoadError {
    #[error("public key is not valid hex")]
    NotHex,
    #[error("ed25519 public key must be 32 bytes, got {0}")]
    WrongLength(usize),
    #[error("bytes do not form a valid ed25519 public key")]
    InvalidKey,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[test]
    fn inserted_hex_key_roundtrips_to_the_same_verifying_key() {
        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let verifying_key = signing_key.verifying_key();
        let hex_pubkey = hex::encode(verifying_key.to_bytes());

        let mut keys = TrustedReleaseKeys::new();
        keys.insert_hex("k1", &hex_pubkey).unwrap();

        assert_eq!(keys.get("k1"), Some(&verifying_key));
        assert_eq!(keys.get("unknown"), None);
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn rejects_wrong_length_hex() {
        let mut keys = TrustedReleaseKeys::new();
        assert_eq!(keys.insert_hex("k1", "abcd").unwrap_err(), KeyLoadError::WrongLength(2));
    }

    #[test]
    fn rejects_non_hex() {
        let mut keys = TrustedReleaseKeys::new();
        assert_eq!(keys.insert_hex("k1", &"zz".repeat(32)).unwrap_err(), KeyLoadError::NotHex);
    }

    #[test]
    fn empty_registry_reports_empty() {
        assert!(TrustedReleaseKeys::new().is_empty());
    }
}
