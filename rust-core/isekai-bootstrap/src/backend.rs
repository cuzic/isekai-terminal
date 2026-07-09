//! `BootstrapBackend`: the abstraction over "how do we reach a not-yet
//! (or no-longer) reachable `isekai-helper` and get it running", used for the
//! `--via` fallback path (`archive/ISEKAI_SSH_DESIGN.md` "CLIコマンド構成" /
//! "`--via` の実装方式").

use std::net::SocketAddr;

use async_trait::async_trait;

use crate::error::BootstrapError;
use crate::types::{BootstrapReport, HostSpec, JumpSpec, LaunchSpec};

/// Installs (if needed) and launches `isekai-helper` on `target`, optionally
/// routed through a chain of `via` jump hosts (`ISEKAI_PIPE_DESIGN.md` §8
/// Epic K — an empty slice means no jump host, a 0-hop direct connection;
/// multiple entries chain through each hop in order using `ssh(1)`'s own
/// multi-hop `-J host1,host2,...` support rather than nesting a separate
/// `ssh` invocation per hop, per Epic K's executor requirement), and returns
/// the handshake JSON it printed on success.
///
/// `remote_binary_path` overrides the remote install path (full path to the
/// uploaded binary, not just a directory) sourced from `#@isekai remote-path`
/// (`ISEKAI_PIPE_DESIGN.md`); `None` falls back to
/// `isekai_protocol::bootstrap::{ISEKAI_PIPE_INSTALL_DIR, ISEKAI_PIPE_BIN_NAME}`.
///
/// `stun_servers` (`#20b`): STUN servers the caller has configured (e.g.
/// `isekai-ssh`'s `#@isekai stun` directive / `isekai-ssh init --stun-server`)
/// — the implementation queries each for this side's own observed address
/// and includes the results as `BootstrapRequestV2.client_candidates` sent
/// to the remote side, and passes the first one through to the launched
/// `isekai-helper` so it reports its own `server-reflexive` candidate back
/// too. An empty slice disables STUN candidate exchange entirely (today's
/// pre-`#20b` behavior).
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
        via: &[JumpSpec],
        helper_binary: &[u8],
        launch: &LaunchSpec,
        remote_binary_path: Option<&str>,
        stun_servers: &[SocketAddr],
    ) -> Result<BootstrapReport, BootstrapError>;
}

// `RusshBackend` (an implementation of this trait built on the existing
// `connect_via_jump_or_direct`/`ensure_helper_running` logic in
// `rust-core/src/transport.rs` and `rust-core/src/helper_bootstrap.rs`) is
// intentionally *not* implemented in this phase (`archive/ISEKAI_SSH_DESIGN.md`
// フェーズ分割案 S-0e-1 explicitly scopes this crate to `OpenSshBackend`
// only). It remains future work for Android (where spawning a `ssh(1)`
// subprocess isn't an option) and for an explicit CLI `--backend russh` test
// option. `BootstrapBackend` above is already general enough for that
// implementation to slot in later without changes to this trait.
