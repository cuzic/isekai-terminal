//! Resolves which private key file to use for pubkey authentication on the
//! native path, and loads it into a `russh_stream_session::Credential`.
//!
//! The candidate *ordering* (`identity_file_candidates`) now lives in the
//! shared `isekai_fs_guard` crate and is re-exported below, so this crate's
//! connect path and `isekai-bootstrap`'s `RusshBackend` can't drift on the
//! `ssh(1)` default probe order again. What stays here is the *loading* half
//! (`read_credential`) — reading a single candidate off disk into a
//! `russh_stream_session::Credential`, which the shared crate deliberately
//! doesn't depend on.
//!
//! **Deliberately out of scope** (matches the plan's M1 non-compat list):
//! passphrase-protected keys, `IdentitiesOnly`, `CertificateFile`. A key
//! that needs a passphrase to decrypt simply fails to parse — the same
//! observable failure `russh_stream_session::SessionError::InvalidPrivateKey`
//! already produces for any other malformed key.

use std::path::Path;

use russh_stream_session::Credential;

use crate::log_file::log_line;

/// The `IdentityFile` candidate ordering — the pure path-selection half of
/// `ssh(1)`'s `IdentityFile` handling, shared with `isekai-bootstrap`'s
/// `RusshBackend` (see the crate's own docs for why the default probe order
/// lives there). `connect.rs` calls it as `private_key::identity_file_candidates`
/// through this re-export.
pub(crate) use isekai_fs_guard::identity_file_candidates;

/// Reads one candidate identity file into a [`Credential::PublicKey`], or
/// returns `None` if it can't be read. **Never fatal, and never eager**: the
/// caller (`connect_and_authenticate`) reads candidates one at a time,
/// interleaved with the authentication attempt, and on `None` simply falls
/// through to the next candidate — and then the SSH agent. A missing file
/// (`NotFound`, the common "this default probe path doesn't exist" case) is
/// skipped silently; any other read error (a permissions problem, or the path
/// not being a regular file) is skipped with a warning, matching `ssh(1)`'s
/// own tolerance of an unreadable `IdentityFile` — it warns and moves on
/// rather than aborting.
///
/// Codex review finding: an earlier version eagerly read *all* candidates up
/// front and propagated any non-`NotFound` read error, so a permissions
/// problem on a *later* configured `IdentityFile` aborted the whole
/// authentication before the perfectly-readable *first* one was ever tried.
/// Reading lazily, one candidate at a time, and skipping *every* read error
/// removes that failure mode (and the `NotFound`-vs-other special-casing).
///
/// Reads directly rather than `exists()`-checking first: a separate `exists()`
/// call is both an extra filesystem round-trip and a TOCTOU gap — a single
/// `read()` whose error is treated as "skip" gets the same behavior in one
/// syscall. Parsing/validating the key material is the caller's job
/// (`authenticate_session`), so a file that reads fine but isn't a
/// valid/decryptable OpenSSH key is still returned here and only fails later,
/// at its own auth attempt (`SessionError::InvalidPrivateKey`).
pub(crate) fn read_credential(path: &Path) -> Option<Credential> {
    match std::fs::read(path) {
        Ok(private_key_pem) => Some(Credential::PublicKey { private_key_pem }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            log_line!("isekai-ssh: skipping unreadable identity file {}: {e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_credential_reads_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("id_ed25519");
        std::fs::write(&present, b"fake key bytes\n").unwrap();

        // `Credential` implements `Drop` (zeroizes), so match by reference
        // rather than moving `private_key_pem` out of it.
        let credential = read_credential(&present).expect("a readable file must yield Some(Credential)");
        match &credential {
            Credential::PublicKey { private_key_pem } => {
                assert_eq!(*private_key_pem, std::fs::read(&present).unwrap());
            }
            _ => panic!("expected Credential::PublicKey for a readable file"),
        }
    }

    #[test]
    fn read_credential_returns_none_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_credential(&dir.path().join("does-not-exist")).is_none());
    }

    #[test]
    fn read_credential_returns_none_for_an_unreadable_non_file() {
        // A directory reliably produces a non-`NotFound` read error regardless
        // of the test's uid (a chmod-000 file is still readable as root, which
        // CI often runs as), standing in for "exists but can't be read as a
        // key". Must be skipped, not fatal (Codex review finding).
        let dir = tempfile::tempdir().unwrap();
        let not_a_file = dir.path().join("id_is_a_dir");
        std::fs::create_dir(&not_a_file).unwrap();
        assert!(read_credential(&not_a_file).is_none(), "an unreadable candidate must be skipped, not fatal");
    }
}
