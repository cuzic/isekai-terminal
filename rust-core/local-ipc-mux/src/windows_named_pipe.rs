//! The real Windows implementation of [`crate::ExclusiveChannel`], backed by
//! `tokio::net::windows::named_pipe`.
//!
//! **Not yet implemented** — tracked as `fancy-humming-pnueli.md` M4 work.
//! Design notes for the real implementation:
//! - `try_claim` uses `ServerOptions::new().first_pipe_instance(true).create(name)`;
//!   a `PermissionDenied` result means another process already owns `name`
//!   (map to [`crate::ClaimError::AlreadyClaimed`], not a generic I/O error).
//! - `accept` creates *further* pipe instances on the same `name` (without
//!   `first_pipe_instance`) and waits for `NamedPipeServer::connect()` on
//!   each, so the owner can serve many clients over its lifetime — not just
//!   the first one.
//! - `connect` uses `ClientOptions::new().open(name)`, retrying briefly with
//!   backoff on `ERROR_PIPE_BUSY` (a real, transient race: the owner may not
//!   have created its next accepting instance yet) before giving up.
//! - Set an explicit same-user ACL on the pipe (`windows` crate,
//!   `Win32_Security`/`Win32_Security_Authorization`, already a `cfg(windows)`
//!   dependency of this crate) rather than relying solely on named-pipe
//!   default permissions.

#![allow(dead_code)] // placeholder until the real implementation lands

/// Placeholder — does not yet implement [`crate::ExclusiveChannel`]. See
/// this module's docs for the intended design.
pub struct WindowsNamedPipeChannel;
