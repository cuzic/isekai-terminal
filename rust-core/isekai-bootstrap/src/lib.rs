//! `isekai-ssh`'s `--via` bootstrap logic: installing and launching
//! `isekai-helper` on a remote host over SSH (`archive/ISEKAI_SSH_DESIGN.md`
//! "共有ロジックの crate 分割", phase S-0e-1).
//!
//! This crate extracts the *logic* of
//! `rust-core/src/helper_bootstrap.rs` (constants, shell command
//! construction, handshake capture/validation) behind a `BootstrapBackend`
//! trait, so `isekai-ssh` (a plain CLI binary with no `russh::client::Handle`
//! of its own) can reuse it via a plain `ssh(1)` subprocess
//! (`OpenSshBackend`) instead. `isekai-terminal-core`/Android keeps its existing
//! `russh`-based implementation; a `RusshBackend` adapter for it is future
//! work (see `backend` module docs).
//!
//! Scope of this phase (S-0e-1, **relay launch mode only** — no STUN/P2P, no
//! resume):
//! - `BootstrapBackend` (`backend.rs`): the CLI/Android-agnostic trait.
//! - `OpenSshBackend` (`openssh.rs`): the CLI-default implementation, backed
//!   by a real `ssh(1)` subprocess with strict stdout-purity enforcement.
//! - `install_script.rs` (crate-private): the remote install/launch shell
//!   script, its stdin framing, and the handshake parsing — shared by
//!   *both* backends, which differ only in how they establish an SSH
//!   session and push the bytes.
//! - `HostSpec`/`JumpSpec`/`RelayLaunchSpec`/`BootstrapReport` (`types.rs`).
//! - `BootstrapError` (`error.rs`).

pub mod backend;
pub mod client_candidates;
pub mod error;
mod install_script;
pub mod openssh;
mod reuse;
pub mod russh_backend;
pub mod types;

pub use backend::BootstrapBackend;
pub use error::BootstrapError;
pub use openssh::OpenSshBackend;
pub use reuse::launch_fingerprint;
pub use russh_backend::RusshBackend;
pub use types::{BootstrapReport, HostSpec, JumpSpec, LaunchSpec, RelayLaunchSpec, RelayTransportKind};

/// The exact `isekai-pipe serve` argv tail (as shell text — `$tmpdir`
/// references and single-quoted values unexpanded) that every bootstrap
/// backend launches the uploaded helper with for `launch`.
///
/// Not part of the bootstrap API proper: exposed only so `isekai-pipe`'s own
/// tests can feed the *real* generated argv through its real `serve` parser
/// (review 2026-09-29 PIPE-01 — `parse_serve` had silently drifted behind
/// this generator and rejected `--bind-port-range`/`--relay-transport`,
/// failing every bootstrap and silent re-deploy that used them;
/// `.claude/rules/always-connects.md`).
#[doc(hidden)]
pub fn serve_launch_args(
    launch: &LaunchSpec,
    stun_servers: &[std::net::SocketAddr],
) -> Result<String, BootstrapError> {
    install_script::serve_launch_args(launch, stun_servers).map(|(args, _jwt)| args)
}
