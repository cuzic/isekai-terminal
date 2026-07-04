//! `BootstrapBackend`: the abstraction over "how do we reach a not-yet
//! (or no-longer) reachable `isekai-helper` and get it running", used for the
//! `--via` fallback path (`ISEKAI_SSH_DESIGN.md` "CLIコマンド構成" /
//! "`--via` の実装方式").

use async_trait::async_trait;

use crate::error::BootstrapError;
use crate::types::{BootstrapReport, HostSpec, JumpSpec, RelayLaunchSpec};

/// Installs (if needed) and launches `isekai-helper` on `target`, optionally
/// routed through a `via` jump host, and returns the handshake JSON it
/// printed on success.
///
/// Implementations must never let anything but the `isekai-helper` binary's
/// own stdout(1-line-handshake-JSON)/stderr reach their caller — see
/// `OpenSshBackend`'s module docs for the concrete contract this phase
/// (S-0e-1) implements.
#[async_trait]
pub trait BootstrapBackend: Send + Sync {
    async fn install_and_start(
        &self,
        target: &HostSpec,
        via: Option<&JumpSpec>,
        helper_binary: &[u8],
        relay: &RelayLaunchSpec,
    ) -> Result<BootstrapReport, BootstrapError>;
}

// `RusshBackend` (an implementation of this trait built on the existing
// `connect_via_jump_or_direct`/`ensure_helper_running` logic in
// `rust-core/src/transport.rs` and `rust-core/src/helper_bootstrap.rs`) is
// intentionally *not* implemented in this phase (`ISEKAI_SSH_DESIGN.md`
// フェーズ分割案 S-0e-1 explicitly scopes this crate to `OpenSshBackend`
// only). It remains future work for Android (where spawning a `ssh(1)`
// subprocess isn't an option) and for an explicit CLI `--backend russh` test
// option. `BootstrapBackend` above is already general enough for that
// implementation to slot in later without changes to this trait.
