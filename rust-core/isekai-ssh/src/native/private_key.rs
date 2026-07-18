//! Resolves which private key file to use for pubkey authentication on the
//! native path, and loads it into a `russh_stream_session::Credential`.
//!
//! `openssh-config::HostConfig::identity_file` only reflects explicit
//! `IdentityFile` lines in the config file — matching that crate's own
//! documented scope, it does not apply `ssh(1)`'s own built-in default
//! probe order when no `IdentityFile` is configured at all. That probing is
//! `ssh(1)` client behavior, not `ssh_config(5)` file syntax, so it lives
//! here instead.
//!
//! **Deliberately out of scope** (matches the plan's M1 non-compat list):
//! passphrase-protected keys, `IdentitiesOnly`, `CertificateFile`. A key
//! that needs a passphrase to decrypt simply fails to parse — the same
//! observable failure `russh_stream_session::SessionError::InvalidPrivateKey`
//! already produces for any other malformed key.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use russh_stream_session::Credential;

/// Default `IdentityFile` probe order tried when the config specifies none
/// explicitly, per the plan's M1 design (`id_ed25519` → `id_rsa` →
/// `id_ecdsa`) — this had drifted out of order in an earlier revision of
/// this file (Codex review finding), swapping `id_rsa`/`id_ecdsa`.
const DEFAULT_IDENTITY_FILE_NAMES: &[&str] = &["id_ed25519", "id_rsa", "id_ecdsa"];

/// Returns the `IdentityFile` candidates to try, in order: `configured` if
/// non-empty (as resolved by `openssh_config::resolve`/`resolve_default`),
/// else `ssh(1)`'s own default probe order rooted at `home/.ssh/`.
pub(crate) fn identity_file_candidates(configured: &[PathBuf], home: &Path) -> Vec<PathBuf> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    DEFAULT_IDENTITY_FILE_NAMES.iter().map(|name| home.join(".ssh").join(name)).collect()
}

/// Reads the first candidate in `candidates` that exists on disk and
/// returns it as a [`Credential::PublicKey`] — the caller
/// (`russh_stream_session::authenticate_session`) is what actually parses
/// and validates the key material, so a candidate that exists but isn't a
/// valid/decryptable OpenSSH private key still gets picked here and only
/// fails later, at the authentication attempt itself (surfaced as
/// `SessionError::InvalidPrivateKey`, not from this function).
///
/// Reads each candidate directly rather than `exists()`-checking first
/// (Codex review finding): a separate `exists()` call before `read()` is
/// both an extra filesystem round-trip for every candidate that *is*
/// present, and a TOCTOU gap (the file could vanish between the check and
/// the read) — treating a `NotFound` read error as "try the next candidate"
/// gets the same skip-if-missing behavior from a single syscall per
/// candidate instead of up to two.
pub(crate) fn load_first_existing(candidates: &[PathBuf]) -> Result<Credential> {
    for path in candidates {
        match std::fs::read(path) {
            Ok(private_key_pem) => return Ok(Credential::PublicKey { private_key_pem }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("failed to read identity file {}", path.display())),
        }
    }
    anyhow::bail!(
        "no usable identity file found (tried: {})",
        candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_candidates_are_used_verbatim_when_non_empty() {
        let configured = vec![PathBuf::from("/custom/key1"), PathBuf::from("/custom/key2")];
        let home = PathBuf::from("/home/alice");
        assert_eq!(identity_file_candidates(&configured, &home), configured);
    }

    #[test]
    fn empty_configured_falls_back_to_default_probe_order_under_home() {
        let home = PathBuf::from("/home/alice");
        let candidates = identity_file_candidates(&[], &home);
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/home/alice/.ssh/id_ed25519"),
                PathBuf::from("/home/alice/.ssh/id_rsa"),
                PathBuf::from("/home/alice/.ssh/id_ecdsa"),
            ]
        );
    }

    #[test]
    fn load_first_existing_skips_missing_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let present = dir.path().join("id_ed25519");
        std::fs::write(&present, b"fake key bytes\n").unwrap();

        let credential = load_first_existing(&[missing, present.clone()]).unwrap();
        match credential {
            Credential::PublicKey { private_key_pem } => {
                assert_eq!(private_key_pem, std::fs::read(&present).unwrap());
            }
            _ => panic!("expected Credential::PublicKey"),
        }
    }

    #[test]
    fn load_first_existing_errors_when_nothing_exists() {
        // `Credential` intentionally doesn't derive `Debug` (avoids
        // accidentally formatting a password/key into a log line), so
        // `Result::unwrap_err()` isn't available here — match instead.
        let dir = tempfile::tempdir().unwrap();
        let candidates = vec![dir.path().join("a"), dir.path().join("b")];
        match load_first_existing(&candidates) {
            Err(e) => assert!(e.to_string().contains("no usable identity file")),
            Ok(_) => panic!("expected an error when no candidate exists"),
        }
    }

    #[test]
    fn load_first_existing_prefers_earlier_candidates_over_later_ones() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("id_ed25519");
        let second = dir.path().join("id_rsa");
        std::fs::write(&first, b"ed25519 bytes\n").unwrap();
        std::fs::write(&second, b"rsa bytes\n").unwrap();

        let credential = load_first_existing(&[first.clone(), second]).unwrap();
        match credential {
            Credential::PublicKey { private_key_pem } => {
                assert_eq!(private_key_pem, std::fs::read(&first).unwrap());
            }
            _ => panic!("expected Credential::PublicKey"),
        }
    }
}
