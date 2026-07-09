//! [`verify_artifact`]: the actual bootstrap-time check
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic D) — given a [`SignedManifest`], the
//! artifact bytes it describes, a set of [`TrustedReleaseKeys`], and what
//! platform/architecture the caller expects to deploy to, decides whether
//! the artifact may be trusted.

use ed25519_dalek::{Signature, Verifier};

use crate::keys::TrustedReleaseKeys;
use crate::manifest::{canonical_manifest_bytes, hex, SignedManifest};

/// What the caller is about to deploy to — compared against the manifest's
/// own `platform`/`architecture` fields so a signed-and-valid manifest for
/// the *wrong* target is still rejected (a correctly signed aarch64
/// artifact must not be accepted when the caller asked for x86_64).
#[derive(Debug, Clone, Copy)]
pub struct ExpectedTarget<'a> {
    pub platform: &'a str,
    pub architecture: &'a str,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    #[error("signature is not valid hex")]
    SignatureNotHex,
    #[error("ed25519 signature must be 64 bytes, got {0}")]
    SignatureWrongLength(usize),
    #[error("no trusted key registered for key_id {0:?}")]
    UnknownKeyId(String),
    #[error("signature verification failed")]
    SignatureInvalid,
    #[error("platform mismatch: manifest says {manifest:?}, expected {expected:?}")]
    PlatformMismatch { manifest: String, expected: String },
    #[error("architecture mismatch: manifest says {manifest:?}, expected {expected:?}")]
    ArchitectureMismatch { manifest: String, expected: String },
    #[error("artifact size mismatch: manifest says {expected}, got {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("artifact sha256 mismatch: manifest says {expected}, computed {actual}")]
    DigestMismatch { expected: String, actual: String },
}

/// Verifies `artifact_bytes` against `signed`, in an order that never acts
/// on unauthenticated manifest data: the signature is checked first (using
/// only `signed.manifest.key_id` to select which trusted key to check
/// against — routing on unauthenticated data, then trusting what it routed
/// to, would defeat the signature check), and only once that passes are
/// the manifest's own fields (platform/architecture/size/digest) trusted
/// enough to compare against.
pub fn verify_artifact(
    signed: &SignedManifest,
    artifact_bytes: &[u8],
    trusted_keys: &TrustedReleaseKeys,
    expected: ExpectedTarget<'_>,
) -> Result<(), VerifyError> {
    let key = trusted_keys.get(&signed.manifest.key_id).ok_or_else(|| VerifyError::UnknownKeyId(signed.manifest.key_id.clone()))?;

    let sig_bytes = hex::decode(&signed.signature).map_err(|_| VerifyError::SignatureNotHex)?;
    let sig_array: [u8; 64] = sig_bytes.try_into().map_err(|v: Vec<u8>| VerifyError::SignatureWrongLength(v.len()))?;
    let signature = Signature::from_bytes(&sig_array);

    let canonical = canonical_manifest_bytes(&signed.manifest);
    key.verify(&canonical, &signature).map_err(|_| VerifyError::SignatureInvalid)?;

    if signed.manifest.platform != expected.platform {
        return Err(VerifyError::PlatformMismatch { manifest: signed.manifest.platform.clone(), expected: expected.platform.to_string() });
    }
    if signed.manifest.architecture != expected.architecture {
        return Err(VerifyError::ArchitectureMismatch {
            manifest: signed.manifest.architecture.clone(),
            expected: expected.architecture.to_string(),
        });
    }

    let actual_size = artifact_bytes.len() as u64;
    if actual_size != signed.manifest.size {
        return Err(VerifyError::SizeMismatch { expected: signed.manifest.size, actual: actual_size });
    }

    let actual_sha256 = hex_sha256(artifact_bytes);
    if actual_sha256 != signed.manifest.sha256 {
        return Err(VerifyError::DigestMismatch { expected: signed.manifest.sha256.clone(), actual: actual_sha256 });
    }

    Ok(())
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{sign_manifest, ReleaseManifest};
    use ed25519_dalek::SigningKey;

    fn manifest_for(bytes: &[u8]) -> ReleaseManifest {
        ReleaseManifest {
            version: "0.5.0".to_string(),
            platform: "linux".to_string(),
            architecture: "x86_64".to_string(),
            artifact_filename: "isekai-pipe-x86_64-unknown-linux-musl".to_string(),
            size: bytes.len() as u64,
            sha256: hex_sha256(bytes),
            protocol_compat: "isekai-pipe/1".to_string(),
            release_channel: "stable".to_string(),
            key_id: "test-key".to_string(),
        }
    }

    fn target() -> ExpectedTarget<'static> {
        ExpectedTarget { platform: "linux", architecture: "x86_64" }
    }

    #[test]
    fn accepts_a_correctly_signed_matching_artifact() {
        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let artifact = b"pretend-isekai-pipe-binary-bytes".to_vec();
        let signed = sign_manifest(manifest_for(&artifact), &signing_key);

        let mut keys = TrustedReleaseKeys::new();
        keys.insert("test-key", signing_key.verifying_key());

        assert_eq!(verify_artifact(&signed, &artifact, &keys, target()), Ok(()));
    }

    #[test]
    fn rejects_a_tampered_artifact_even_with_a_valid_signature() {
        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let artifact = b"pretend-isekai-pipe-binary-bytes".to_vec();
        let signed = sign_manifest(manifest_for(&artifact), &signing_key);

        let mut keys = TrustedReleaseKeys::new();
        keys.insert("test-key", signing_key.verifying_key());

        let tampered = b"pretend-isekai-pipe-binary-BYTES".to_vec();
        assert!(matches!(verify_artifact(&signed, &tampered, &keys, target()), Err(VerifyError::DigestMismatch { .. })));
    }

    #[test]
    fn rejects_a_manifest_signed_by_an_untrusted_key() {
        let attacker_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let artifact = b"pretend-isekai-pipe-binary-bytes".to_vec();
        let signed = sign_manifest(manifest_for(&artifact), &attacker_key);

        // The verifier's trust store never learned the attacker's key at all.
        let keys = TrustedReleaseKeys::new();
        assert_eq!(verify_artifact(&signed, &artifact, &keys, target()), Err(VerifyError::UnknownKeyId("test-key".to_string())));
    }

    #[test]
    fn rejects_a_manifest_whose_key_id_resolves_to_a_different_key() {
        let real_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let other_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let artifact = b"pretend-isekai-pipe-binary-bytes".to_vec();
        let signed = sign_manifest(manifest_for(&artifact), &real_key);

        // Same key_id string, but the registry maps it to a *different*
        // key than the one that actually signed — e.g. a stale/rotated
        // registration. Must fail closed, not silently accept.
        let mut keys = TrustedReleaseKeys::new();
        keys.insert("test-key", other_key.verifying_key());

        assert_eq!(verify_artifact(&signed, &artifact, &keys, target()), Err(VerifyError::SignatureInvalid));
    }

    #[test]
    fn rejects_a_manifest_for_the_wrong_platform() {
        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let artifact = b"pretend-isekai-pipe-binary-bytes".to_vec();
        let mut manifest = manifest_for(&artifact);
        manifest.platform = "macos".to_string();
        let signed = sign_manifest(manifest, &signing_key);

        let mut keys = TrustedReleaseKeys::new();
        keys.insert("test-key", signing_key.verifying_key());

        assert_eq!(
            verify_artifact(&signed, &artifact, &keys, target()),
            Err(VerifyError::PlatformMismatch { manifest: "macos".to_string(), expected: "linux".to_string() })
        );
    }

    #[test]
    fn rejects_a_manifest_for_the_wrong_architecture() {
        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let artifact = b"pretend-isekai-pipe-binary-bytes".to_vec();
        let mut manifest = manifest_for(&artifact);
        manifest.architecture = "aarch64".to_string();
        let signed = sign_manifest(manifest, &signing_key);

        let mut keys = TrustedReleaseKeys::new();
        keys.insert("test-key", signing_key.verifying_key());

        assert_eq!(
            verify_artifact(&signed, &artifact, &keys, target()),
            Err(VerifyError::ArchitectureMismatch { manifest: "aarch64".to_string(), expected: "x86_64".to_string() })
        );
    }

    #[test]
    fn rejects_a_manifest_whose_declared_size_does_not_match() {
        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let artifact = b"pretend-isekai-pipe-binary-bytes".to_vec();
        let mut manifest = manifest_for(&artifact);
        manifest.size += 1;
        let signed = sign_manifest(manifest, &signing_key);

        let mut keys = TrustedReleaseKeys::new();
        keys.insert("test-key", signing_key.verifying_key());

        assert_eq!(
            verify_artifact(&signed, &artifact, &keys, target()),
            Err(VerifyError::SizeMismatch { expected: artifact.len() as u64 + 1, actual: artifact.len() as u64 })
        );
    }

    #[test]
    fn rejects_a_corrupted_signature_string() {
        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let artifact = b"pretend-isekai-pipe-binary-bytes".to_vec();
        let mut signed = sign_manifest(manifest_for(&artifact), &signing_key);
        signed.signature = "not-hex-at-all".to_string();

        let mut keys = TrustedReleaseKeys::new();
        keys.insert("test-key", signing_key.verifying_key());

        assert_eq!(verify_artifact(&signed, &artifact, &keys, target()), Err(VerifyError::SignatureNotHex));
    }
}
