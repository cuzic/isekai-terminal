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
//! - `HostSpec`/`JumpSpec`/`RelayLaunchSpec`/`BootstrapReport` (`types.rs`).
//! - `BootstrapError` (`error.rs`).

pub mod backend;
pub mod client_candidates;
pub mod error;
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
