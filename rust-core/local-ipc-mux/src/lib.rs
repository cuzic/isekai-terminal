//! Exclusive single-owner local IPC channel + generic framing/relay.
//!
//! The pattern this crate exists for: of several sibling processes running
//! on the same machine that all want to reach "the one shared resource for
//! this session", exactly one becomes the *owner* (it claimed the channel
//! name first) and the rest connect to it as *clients*. This crate has no
//! opinion on what bytes flow over an established connection or what the
//! shared resource actually is — see `isekai-ssh`'s own multiplexer (built
//! on top of this crate) for the SSH-specific frame protocol
//! (stdin/resize/signal/stdout/exit-code) that motivated it.
//!
//! **Windows only for now**: `WindowsNamedPipeChannel` is the only real
//! [`ExclusiveChannel`] implementation. A Unix implementation (e.g. bind-
//! exclusive semantics over a `UnixListener`) is deliberately out of scope —
//! real `ssh(1)`'s own `ControlMaster`/`ControlPersist` already gives Unix
//! this exact capability for free, so there's no pressing need for a native
//! implementation there yet. The trait boundary is designed so one can be
//! added later without disturbing any existing caller.
//!
//! **Skeleton status**: this file currently defines the trait contract, the
//! error types, and [`InMemoryChannel`] (a same-process test double useful
//! for exercising a caller's logic without real IPC). The real
//! [`WindowsNamedPipeChannel`] implementation and the framing/relay layer
//! are tracked as `fancy-humming-pnueli.md` M4 follow-up work.

use std::io;

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

/// Why [`ExclusiveChannel::try_claim`] failed.
#[derive(Debug, Error)]
pub enum ClaimError {
    /// Another process already owns this channel name — the caller should
    /// fall back to [`ExclusiveChannel::connect`] instead.
    #[error("channel {name:?} is already claimed by another owner")]
    AlreadyClaimed { name: String },
    /// Some other OS-level failure prevented claiming the channel (not a
    /// "someone else owns it" situation — e.g. a permissions problem
    /// unrelated to ownership).
    #[error("failed to claim channel {name:?}: {source}")]
    Io {
        name: String,
        #[source]
        source: io::Error,
    },
}

/// Why [`ExclusiveChannel::connect`] failed.
#[derive(Debug, Error)]
pub enum ConnectError {
    /// No owner currently holds this channel name.
    #[error("no owner found for channel {name:?}")]
    NotFound { name: String },
    /// An owner exists, but the connection attempt itself failed (e.g. a
    /// race where the owner exited between the caller learning it existed
    /// and this connect attempt landing).
    #[error("failed to connect to channel {name:?}: {source}")]
    Io {
        name: String,
        #[source]
        source: io::Error,
    },
}

/// A single named exclusive-ownership IPC channel. Implementations claim a
/// platform-specific resource identified by `name` (a Windows named pipe
/// path, in the only real implementation so far) such that at most one
/// process can be the *owner* at a time; every other process trying to
/// claim the same name gets [`ClaimError::AlreadyClaimed`] and should
/// [`ExclusiveChannel::connect`] instead.
///
/// The owner side is a long-lived *acceptor*: after claiming the name, call
/// [`ExclusiveChannel::accept`] in a loop to receive each new client
/// connection in turn (an owner can serve many clients over its lifetime,
/// not just one) — this mirrors how a Windows named pipe server works
/// (claim the name once via `first_pipe_instance`, then create further pipe
/// instances to accept subsequent client connections on the same name).
#[async_trait]
pub trait ExclusiveChannel: Sized + Send {
    /// One established, bidirectional connection — either the owner's view
    /// of one accepted client, or a client's view of its connection to the
    /// owner. Callers layer their own framing/protocol on top of this.
    type Connection: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    /// Attempts to become the owner of `name`. Returns
    /// [`ClaimError::AlreadyClaimed`] (not a generic I/O error) when another
    /// process already owns it, so callers can reliably fall back to
    /// [`Self::connect`] without misclassifying a real failure as "someone
    /// else owns it".
    async fn try_claim(name: &str) -> Result<Self, ClaimError>;

    /// Owner-side only: waits for and returns the next client connection.
    /// Callers typically `tokio::spawn` a task per accepted connection and
    /// loop calling this again immediately. Ends (returns `Err`) only on a
    /// genuine failure of the underlying channel, never merely because no
    /// client has connected yet (this call is expected to await).
    async fn accept(&mut self) -> io::Result<Self::Connection>;

    /// Connects to `name` as a client. Returns [`ConnectError::NotFound`]
    /// specifically when no owner exists yet, so callers can distinguish
    /// "nobody to connect to" (a signal to become the owner instead) from a
    /// transient connection failure worth retrying.
    async fn connect(name: &str) -> Result<Self::Connection, ConnectError>;
}

// Compiled on all platforms so its pure error-classification/retry logic can
// be unit-tested here on Linux; only actually *used* by the Windows named-pipe
// implementation.
#[cfg_attr(not(windows), allow(dead_code))]
mod pipe_classify;

#[cfg(windows)]
mod windows_named_pipe;
#[cfg(windows)]
pub use windows_named_pipe::{PipeConnection, WindowsNamedPipeChannel};

pub mod in_memory;
pub use in_memory::InMemoryChannel;

pub mod framing;
