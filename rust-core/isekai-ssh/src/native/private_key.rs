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
//! `IdentitiesOnly`. Passphrase-protected keys and `CertificateFile` *are*
//! supported — see [`resolve_certificate_file`] and
//! `native::connect::try_encrypted_identity`.

use std::path::{Path, PathBuf};

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
    read_key_bytes(path).map(|private_key_pem| Credential::PublicKey { private_key_pem })
}

/// The raw-bytes half of [`read_credential`], split out so
/// [`read_credential_with_certificate`] can build a
/// [`Credential::PublicKeyWithCertificate`] directly from the key bytes
/// without destructuring a [`Credential`] (which implements `Drop` to
/// zeroize, so its fields can't be moved out of by pattern-matching).
pub(crate) fn read_key_bytes(path: &Path) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            log_line!("isekai-ssh: skipping unreadable identity file {}: {e}", path.display());
            None
        }
    }
}

/// Resolves the `CertificateFile` paired with the identity file at
/// `candidate_index` (its position within the list `identity_file_candidates`
/// returned), for `CertificateFile` authentication: an explicit
/// `host_config.certificate_file` entry at the same index — `ssh_config(5)`'s
/// own positional `IdentityFile`/`CertificateFile` pairing, meaningful only
/// when `host_config.identity_file` was itself non-empty (`identity_file_candidates`
/// returns `host_config.identity_file` verbatim in that case, so the indices
/// line up; the default `id_ed25519`→`id_rsa`→`id_ecdsa` probe order has no
/// corresponding `CertificateFile` list to pair against) — falling back to
/// `ssh(1)`'s own default convention of a `-cert.pub`-suffixed sibling file,
/// which applies regardless of where `candidate` came from.
///
/// **Known simplification**: an explicit `CertificateFile` is only paired
/// positionally against a *configured* `IdentityFile` at the same index —
/// not against a candidate from the default probe order, and not by matching
/// certificate/key content when the counts differ. Real `ssh(1)`'s own
/// pairing rules for mismatched `IdentityFile`/`CertificateFile` counts are
/// more involved; this covers the common case (one identity, its matching
/// certificate) `ssh-keygen -s`'s own default output layout produces.
pub(crate) fn resolve_certificate_file(
    host_config: &openssh_config::HostConfig,
    candidate: &Path,
    candidate_index: usize,
) -> Option<PathBuf> {
    if !host_config.identity_file.is_empty() {
        if let Some(explicit) = host_config.certificate_file.get(candidate_index) {
            return Some(explicit.clone());
        }
    }
    let mut default_name = candidate.as_os_str().to_owned();
    default_name.push("-cert.pub");
    let default_path = PathBuf::from(default_name);
    default_path.is_file().then_some(default_path)
}

/// Like [`read_credential`], but upgrades to
/// [`Credential::PublicKeyWithCertificate`] when [`resolve_certificate_file`]
/// finds a paired certificate for `candidate` that's actually readable.
/// A configured/discovered certificate path that turns out to be unreadable
/// is skipped with a warning (same tolerance `read_credential` already has
/// for the private key itself) — falling back to plain pubkey auth with that
/// same key, rather than failing the candidate outright.
pub(crate) fn read_credential_with_certificate(
    host_config: &openssh_config::HostConfig,
    candidate: &Path,
    candidate_index: usize,
) -> Option<Credential> {
    let private_key_pem = read_key_bytes(candidate)?;
    let Some(cert_path) = resolve_certificate_file(host_config, candidate, candidate_index) else {
        return Some(Credential::PublicKey { private_key_pem });
    };
    match std::fs::read(&cert_path) {
        Ok(certificate_pem) => Some(Credential::PublicKeyWithCertificate { private_key_pem, certificate_pem }),
        Err(e) => {
            log_line!(
                "isekai-ssh: skipping unreadable certificate file {} (falling back to plain pubkey auth for {}): {e}",
                cert_path.display(),
                candidate.display()
            );
            Some(Credential::PublicKey { private_key_pem })
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
    fn resolve_certificate_file_uses_the_default_cert_pub_suffix_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let identity = dir.path().join("id_ed25519");
        let cert = dir.path().join("id_ed25519-cert.pub");
        std::fs::write(&cert, b"cert bytes").unwrap();
        let host_config = openssh_config::HostConfig::default(); // no explicit CertificateFile configured
        assert_eq!(resolve_certificate_file(&host_config, &identity, 0), Some(cert));
    }

    #[test]
    fn resolve_certificate_file_returns_none_when_no_default_cert_exists() {
        let dir = tempfile::tempdir().unwrap();
        let identity = dir.path().join("id_ed25519"); // no sibling -cert.pub written
        let host_config = openssh_config::HostConfig::default();
        assert_eq!(resolve_certificate_file(&host_config, &identity, 0), None);
    }

    #[test]
    fn resolve_certificate_file_prefers_an_explicit_configured_certificate_file() {
        let dir = tempfile::tempdir().unwrap();
        let identity = dir.path().join("id_ed25519");
        let default_cert = dir.path().join("id_ed25519-cert.pub");
        std::fs::write(&default_cert, b"default cert").unwrap(); // exists too, but must lose to the explicit one
        let explicit_cert = dir.path().join("custom-cert-name.pub");
        let host_config = openssh_config::HostConfig {
            identity_file: vec![identity.clone()],
            certificate_file: vec![explicit_cert.clone()],
            ..Default::default()
        };
        assert_eq!(resolve_certificate_file(&host_config, &identity, 0), Some(explicit_cert));
    }

    #[test]
    fn resolve_certificate_file_explicit_pairing_is_positional_and_only_applies_to_configured_identities() {
        // With no configured IdentityFile at all (candidate came from the
        // default id_ed25519/id_rsa/id_ecdsa probe order), an explicit
        // certificate_file list has nothing to pair against positionally —
        // only the default -cert.pub convention can still apply.
        let dir = tempfile::tempdir().unwrap();
        let identity = dir.path().join("id_ed25519");
        let host_config = openssh_config::HostConfig {
            identity_file: vec![], // empty: candidate came from the default probe order
            certificate_file: vec![dir.path().join("irrelevant-cert.pub")],
            ..Default::default()
        };
        assert_eq!(resolve_certificate_file(&host_config, &identity, 0), None);
    }

    #[test]
    fn read_credential_with_certificate_upgrades_to_a_certificate_credential_when_one_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let identity = dir.path().join("id_ed25519");
        std::fs::write(&identity, b"fake key bytes").unwrap();
        let cert = dir.path().join("id_ed25519-cert.pub");
        std::fs::write(&cert, b"fake cert bytes").unwrap();
        let host_config = openssh_config::HostConfig::default();

        let credential = read_credential_with_certificate(&host_config, &identity, 0).expect("must yield a credential");
        match &credential {
            Credential::PublicKeyWithCertificate { private_key_pem, certificate_pem } => {
                assert_eq!(*private_key_pem, b"fake key bytes");
                assert_eq!(*certificate_pem, b"fake cert bytes");
            }
            _ => panic!("expected PublicKeyWithCertificate when a paired certificate exists"),
        }
    }

    #[test]
    fn read_credential_with_certificate_falls_back_to_plain_pubkey_without_a_paired_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let identity = dir.path().join("id_ed25519");
        std::fs::write(&identity, b"fake key bytes").unwrap();
        let host_config = openssh_config::HostConfig::default(); // no -cert.pub written

        let credential = read_credential_with_certificate(&host_config, &identity, 0).expect("must yield a credential");
        assert!(matches!(credential, Credential::PublicKey { .. }), "no certificate found means plain pubkey");
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
