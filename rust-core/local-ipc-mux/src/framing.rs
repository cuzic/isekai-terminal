//! Generic length-prefixed message framing over an [`crate::ExclusiveChannel`]
//! connection, and the owner-side relay loop that dispatches accepted
//! connections to per-client handlers.
//!
//! **Not yet implemented** — tracked as `fancy-humming-pnueli.md` M4 work.
//! This module exists as a placeholder so the crate's module structure is
//! stable for downstream callers (`isekai-ssh`'s SSH-specific multiplexer)
//! to depend on while the real implementation lands. Expected shape once
//! implemented: a `u32`-length-prefixed frame reader/writer over any
//! `AsyncRead + AsyncWrite` (matching `isekai-protocol`'s own
//! `HandshakeJson`/`CtlMessage` conventions — a size cap, so a malformed or
//! hostile peer can't force an unbounded allocation), plus a small
//! `serve(channel, handler)` loop that accepts connections in a loop and
//! spawns a task per connection running the caller-supplied handler.
