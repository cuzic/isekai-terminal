//! Picks the platform-appropriate `isekai_bootstrap::BootstrapBackend`:
//! `OpenSshBackend` (spawns real `ssh(1)`) on Unix/macOS, `RusshBackend`
//! (native, never spawns `ssh.exe`) on Windows — the M3 half of
//! `fancy-humming-pnueli.md`'s "no `ssh.exe` dependency on Windows" goal;
//! M1 already covers the day-to-day connect path, this covers
//! `isekai-ssh init`/auto-bootstrap.
//!
//! **Cross-cutting consequence of this dispatch (Codex review finding)**:
//! `isekai-ssh init dest --via a --via b` (a 2+-hop `--via` chain) is
//! accepted by CLI parsing (`init.rs::parse_via_chain`) on every platform,
//! but on Windows it now reaches `RusshBackend`, which rejects any chain
//! longer than one hop with `BootstrapError::UnsupportedViaChain` (a clear,
//! actionable error — see `russh_backend.rs`'s own module docs for why
//! multi-hop chaining is deferred, not silently truncated). This is not a
//! regression introduced here: it's the same already-reviewed, already-
//! tested scope limitation `RusshBackend` shipped with, simply reachable
//! from this call site now that Windows routes here at all. A future
//! `russh_stream_session` extension to support genuine N-hop chains would
//! close this gap for both platforms at once.
//!
//! [`NativeBootstrapBackend`] exists purely so callers that need *both*
//! `install_and_start` (the `BootstrapBackend` trait proper) *and*
//! `detect_remote_arch` (an inherent method on each concrete backend type,
//! not part of that trait) can hold one trait object instead of branching
//! on platform at every call site — `isekai-ssh::helper_download::
//! resolve_helper_binary` and `isekai-ssh::wrapper::bootstrap_and_register`/
//! `isekai-ssh::init::run` both need this.

use std::path::Path;

use anyhow::Result;
use isekai_bootstrap::{BootstrapBackend, HostSpec, JumpSpec, OpenSshBackend, RusshBackend};

/// `BootstrapBackend` (`install_and_start`) plus `detect_remote_arch` — both
/// concrete backend types already implement the latter as an inherent
/// method with an identical signature; this trait just lets a caller reach
/// both through one dynamically-dispatched value instead of knowing which
/// concrete type it's holding.
#[async_trait::async_trait]
pub(crate) trait NativeBootstrapBackend: BootstrapBackend {
    async fn detect_remote_arch(&self, target: &HostSpec, via: &[JumpSpec]) -> Result<String>;
}

#[async_trait::async_trait]
impl NativeBootstrapBackend for OpenSshBackend {
    async fn detect_remote_arch(&self, target: &HostSpec, via: &[JumpSpec]) -> Result<String> {
        OpenSshBackend::detect_remote_arch(self, target, via).await.map_err(Into::into)
    }
}

#[async_trait::async_trait]
impl NativeBootstrapBackend for RusshBackend {
    async fn detect_remote_arch(&self, target: &HostSpec, via: &[JumpSpec]) -> Result<String> {
        RusshBackend::detect_remote_arch(self, target, via).await.map_err(Into::into)
    }
}

/// `ssh_path_override` mirrors `--isekai-ssh-path`/`isekai-ssh init
/// --ssh-path`: an explicit `ssh(1)` binary to use, meaningful only for
/// `OpenSshBackend`. On Windows it's silently ignored — `RusshBackend`
/// never spawns any `ssh(1)` binary at all, so there is nothing for this
/// override to apply to; a user who genuinely wants the old `ssh.exe`-based
/// bootstrap path back has no escape hatch here (matches `main.rs`'s own
/// `cfg(windows)` dispatch for the regular connect path: Windows never
/// shells out to `ssh.exe`, full stop).
pub(crate) fn default_bootstrap_backend(ssh_path_override: Option<&Path>) -> Result<Box<dyn NativeBootstrapBackend>> {
    #[cfg(windows)]
    {
        let _ = ssh_path_override;
        Ok(Box::new(RusshBackend::new()?))
    }
    #[cfg(not(windows))]
    {
        let backend = match ssh_path_override {
            Some(path) => OpenSshBackend::new().with_ssh_program(path.to_string_lossy().into_owned()),
            None => OpenSshBackend::new(),
        };
        Ok(Box::new(backend))
    }
}
