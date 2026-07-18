//! Resolves which private key file to use for pubkey authentication on the
//! native path, and loads it into a `russh_stream_session::Credential`.
//!
//! The candidate *ordering* (`identity_file_candidates`) now lives in the
//! shared `isekai_fs_guard` crate and is re-exported below, so this crate's
//! connect path and `isekai-bootstrap`'s `RusshBackend` can't drift on the
//! `ssh(1)` default probe order again. What stays here is the *loading* half
//! (`readable_credentials`) — reading each candidate off disk into a
//! `russh_stream_session::Credential`, which the shared crate deliberately
//! doesn't depend on.
//!
//! **Deliberately out of scope** (matches the plan's M1 non-compat list):
//! passphrase-protected keys, `IdentitiesOnly`, `CertificateFile`. A key
//! that needs a passphrase to decrypt simply fails to parse — the same
//! observable failure `russh_stream_session::SessionError::InvalidPrivateKey`
//! already produces for any other malformed key.

use std::path::PathBuf;

use anyhow::{Context, Result};
use russh_stream_session::Credential;

/// The `IdentityFile` candidate ordering — the pure path-selection half of
/// `ssh(1)`'s `IdentityFile` handling, shared with `isekai-bootstrap`'s
/// `RusshBackend` (see the crate's own docs for why the default probe order
/// lives there). `connect.rs` calls it as `private_key::identity_file_candidates`
/// through this re-export.
pub(crate) use isekai_fs_guard::identity_file_candidates;

/// Reads *every* candidate in `candidates` that exists on disk, in order,
/// returning each as a [`Credential::PublicKey`]. The caller
/// (`connect_and_authenticate` → `russh_stream_session::authenticate_session`)
/// is what actually parses and validates the key material, so a candidate
/// that exists but isn't a valid/decryptable OpenSSH private key is still
/// returned here and only fails later, at its own authentication attempt
/// (surfaced as `SessionError::InvalidPrivateKey`, not from this function).
///
/// Returns all readable candidates rather than just the first (Codex review
/// finding): `ssh(1)` offers every configured `IdentityFile` to the server
/// in turn, so a first identity that exists but is *rejected* by the server
/// (unauthorized) or fails to *parse* (e.g. passphrase-protected) must not
/// block the remaining configured identities — nor the SSH-agent fallback.
/// The previous "first existing file wins" behavior silently dropped every
/// identity after the first one present on disk.
///
/// Reads each candidate directly rather than `exists()`-checking first
/// (Codex review finding): a separate `exists()` call before `read()` is
/// both an extra filesystem round-trip for every candidate that *is*
/// present, and a TOCTOU gap (the file could vanish between the check and
/// the read) — treating a `NotFound` read error as "skip this candidate"
/// gets the same skip-if-missing behavior from a single syscall per
/// candidate instead of up to two. A non-`NotFound` read error on a file
/// that *does* exist (e.g. a permissions problem) is surfaced rather than
/// silently skipped.
pub(crate) fn readable_credentials(candidates: &[PathBuf]) -> Result<Vec<Credential>> {
    let mut credentials = Vec::new();
    for path in candidates {
        match std::fs::read(path) {
            Ok(private_key_pem) => credentials.push(Credential::PublicKey { private_key_pem }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("failed to read identity file {}", path.display())),
        }
    }
    Ok(credentials)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_credentials_skips_missing_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let present = dir.path().join("id_ed25519");
        std::fs::write(&present, b"fake key bytes\n").unwrap();

        let credentials = readable_credentials(&[missing, present.clone()]).unwrap();
        assert_eq!(credentials.len(), 1, "the one missing candidate must be skipped, the present one kept");
        match &credentials[0] {
            Credential::PublicKey { private_key_pem } => {
                assert_eq!(*private_key_pem, std::fs::read(&present).unwrap());
            }
            _ => panic!("expected Credential::PublicKey"),
        }
    }

    #[test]
    fn readable_credentials_returns_empty_when_nothing_exists() {
        let dir = tempfile::tempdir().unwrap();
        let candidates = vec![dir.path().join("a"), dir.path().join("b")];
        let credentials = readable_credentials(&candidates).unwrap();
        assert!(credentials.is_empty(), "no existing candidate means no credentials to offer");
    }

    #[test]
    fn readable_credentials_returns_all_present_candidates_in_order() {
        // Regression for the "only the first IdentityFile is ever tried" bug:
        // every existing candidate must be returned, in the configured order,
        // so `connect_and_authenticate` can offer each in turn (a rejected or
        // unparseable first key no longer blocks the rest).
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("id_ed25519");
        let second = dir.path().join("id_rsa");
        std::fs::write(&first, b"ed25519 bytes\n").unwrap();
        std::fs::write(&second, b"rsa bytes\n").unwrap();

        let credentials = readable_credentials(&[first.clone(), second.clone()]).unwrap();
        assert_eq!(credentials.len(), 2, "both present candidates must be returned");
        let pems: Vec<&Vec<u8>> = credentials
            .iter()
            .map(|c| match c {
                Credential::PublicKey { private_key_pem } => private_key_pem,
                _ => panic!("expected Credential::PublicKey"),
            })
            .collect();
        assert_eq!(*pems[0], std::fs::read(&first).unwrap(), "first candidate must come first");
        assert_eq!(*pems[1], std::fs::read(&second).unwrap(), "second candidate must come second");
    }
}
