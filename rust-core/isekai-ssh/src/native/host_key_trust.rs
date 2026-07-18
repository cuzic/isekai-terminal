//! The native path's TOFU host-key check. The actual trust-store state
//! machine now lives once, in [`isekai_trust::FileBackedHostKeyVerifier`],
//! and is shared with `isekai-bootstrap`'s `RusshBackend` (both are native,
//! non-`ssh(1)`-subprocess SSH paths that verify against the same
//! `known_ssh_hosts.toml`). See that type's module docs for the full TOFU
//! semantics (known/matching → accept+refresh; known/mismatched → silent
//! reject; unknown → `confirm_new_host` decides).
//!
//! This module keeps a paper-thin newtype over that shared verifier for one
//! reason only: it pins the `log_context` to `"isekai-ssh"` (so this path's
//! host-key log lines stay attributable to `isekai-ssh` rather than
//! `isekai-bootstrap`) while preserving the exact 3-argument
//! `FileBackedHostKeyVerifier::new(store_path, host_port, confirm_new_host)`
//! constructor `native::connect` already calls.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use russh_stream_session::HostKeyVerifier;

/// `isekai-ssh`'s host-key verifier: [`isekai_trust::FileBackedHostKeyVerifier`]
/// with this path's `log_context` baked in.
pub(crate) struct FileBackedHostKeyVerifier(isekai_trust::FileBackedHostKeyVerifier);

impl FileBackedHostKeyVerifier {
    pub(crate) fn new(
        store_path: PathBuf,
        host_port: String,
        confirm_new_host: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    ) -> Self {
        Self(isekai_trust::FileBackedHostKeyVerifier::new(store_path, host_port, confirm_new_host, "isekai-ssh"))
    }
}

#[async_trait]
impl HostKeyVerifier for FileBackedHostKeyVerifier {
    async fn verify(&self, fingerprint: &str) -> bool {
        self.0.verify(fingerprint).await
    }
}
