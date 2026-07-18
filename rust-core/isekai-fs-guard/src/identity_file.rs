//! Which private-key files to try for SSH pubkey auth, in order — the pure
//! path-selection half of `ssh(1)`'s `IdentityFile` handling, shared by every
//! native SSH path (`isekai-ssh`'s connect path and `isekai-bootstrap`'s
//! `RusshBackend`).
//!
//! Only the candidate *ordering* lives here (pure `Path` logic, no I/O, no
//! key-loading dependency). Actually reading a candidate off disk and wrapping
//! it in a `russh_stream_session::Credential` stays in each caller — this
//! crate is deliberately low-level and dependency-light, so it does not take
//! on `russh-stream-session`. The thing that actually drifted once before (the
//! probe-order constant getting reordered — see the git history of
//! `isekai-ssh::native::private_key`) is exactly the part consolidated here.

use std::path::{Path, PathBuf};

/// Default `IdentityFile` probe order tried when the config specifies none
/// explicitly (`id_ed25519` → `id_rsa` → `id_ecdsa`), matching `ssh(1)`'s own
/// default identity order.
const DEFAULT_IDENTITY_FILE_NAMES: &[&str] = &["id_ed25519", "id_rsa", "id_ecdsa"];

/// Returns the `IdentityFile` candidates to try, in order: `configured` if
/// non-empty (as resolved from `ssh_config`'s explicit `IdentityFile` lines),
/// else `ssh(1)`'s own default probe order rooted at `home/.ssh/`.
///
/// `openssh-config` only reflects explicit `IdentityFile` lines, matching its
/// own documented scope — it does not apply `ssh(1)`'s built-in default probe
/// order when none is configured. That probing is `ssh(1)` client behavior,
/// not `ssh_config(5)` file syntax, so it lives here.
pub fn identity_file_candidates(configured: &[PathBuf], home: &Path) -> Vec<PathBuf> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    DEFAULT_IDENTITY_FILE_NAMES.iter().map(|name| home.join(".ssh").join(name)).collect()
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
}
