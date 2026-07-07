//! Core library skeleton for the future `isekai-pipe` data plane.
//!
//! This crate will own candidate collection, path selection, QUIC/session
//! management, service dispatch, and resume buffers as the migration proceeds.
//! It currently exposes only the responsibility boundary so downstream code can
//! depend on the crate without pulling in the existing `isekai-helper` logic.

pub use isekai_pipe_protocol::{LogicalHost, ServiceName};

/// High-level role of an `isekai-pipe` process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeRole {
    /// Local side: stdio/TCP listen to logical session.
    Connect,
    /// Remote side: logical session to service target.
    Serve,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_distinct() {
        assert_ne!(PipeRole::Connect, PipeRole::Serve);
    }
}
