//! `ControlMaster`-equivalent multiplexer for the Windows-native path: when
//! several tabs each run `isekai-ssh <host>` to the *same* fully-resolved
//! destination, exactly one process (the *owner*) holds the single
//! authenticated `russh` connection and every other process (a *client*)
//! reaches its own private remote shell through the owner over a
//! `local-ipc-mux` named-pipe channel, instead of each independently
//! re-authenticating a fresh SSH connection.
//!
//! This commit lands the two I/O-free foundations: [`protocol`] (the
//! SSH-specific frame codec — size cap, version field, auth token) and
//! [`naming`] (how a resolved connection config maps to a pipe name). The
//! owner/client relay and the role-selecting dispatch land next.

pub(crate) mod naming;
pub(crate) mod protocol;
