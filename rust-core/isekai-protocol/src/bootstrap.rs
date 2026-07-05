//! Constants shared by the two independent isekai-helper bootstrap
//! implementations: `rust-core/src/helper_bootstrap.rs` (Android, over a
//! `russh::client::Handle`) and `isekai-bootstrap::openssh` (the `isekai-ssh`
//! CLI, over spawned `ssh(1)` subprocesses). Both upload the same binary to
//! the same path and poll for the same handshake-file contract
//! (`HELPER_PROTOCOL.md`), so the remote-side paths/filenames must actually
//! be identical — keeping them as one shared `const` rather than two
//! hand-copied literals is what guarantees that, not just documents it.
//!
//! `isekai-terminal-core` is built as a `cdylib`/`staticlib` and can't be
//! depended on as an ordinary Rust crate, so `rust-core/src/helper_bootstrap.rs`
//! can't just import `isekai-bootstrap` directly — but both it and
//! `isekai-bootstrap` already depend on this pure `isekai-protocol` crate, so
//! this is where the shared literals live.

/// Remote install directory for the isekai-helper binary.
pub const HELPER_INSTALL_DIR: &str = "~/.local/bin";
/// Remote filename of the isekai-helper binary.
pub const HELPER_BIN_NAME: &str = "isekai-helper";
/// How many times the bootstrap remote shell polls for the handshake file to
/// become non-empty before giving up.
pub const HANDSHAKE_POLL_ATTEMPTS: u32 = 50;
/// Delay between handshake-file polls, in milliseconds.
pub const HANDSHAKE_POLL_INTERVAL_MS: u32 = 100;
