//! Core library skeleton for the future `isekai-pipe` data plane.
//!
//! This crate will own candidate collection, path selection, QUIC/session
//! management, service dispatch, and resume buffers as the migration proceeds.
//! It currently exposes only the responsibility boundary so downstream code can
//! depend on the crate without pulling in the existing `isekai-helper` logic.

pub use isekai_pipe_protocol::{LogicalHost, ServiceName};

/// A remote service exposed by `isekai-pipe serve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    name: ServiceName,
    target: String,
}

impl ServiceSpec {
    pub fn new(name: ServiceName, target: impl Into<String>) -> Result<Self, ServiceSpecError> {
        let target = target.into();
        if name.as_str().is_empty() {
            return Err(ServiceSpecError::EmptyName);
        }
        if target.is_empty() {
            return Err(ServiceSpecError::EmptyTarget);
        }
        Ok(Self { name, target })
    }

    pub fn parse(input: &str) -> Result<Self, ServiceSpecError> {
        let Some((name, target)) = input.split_once('=') else {
            return Err(ServiceSpecError::MissingEquals);
        };
        Self::new(ServiceName::new(name), target)
    }

    pub fn ssh_target(target: impl Into<String>) -> Result<Self, ServiceSpecError> {
        Self::new(ServiceName::new("ssh"), target)
    }

    pub fn name(&self) -> &ServiceName {
        &self.name
    }

    pub fn target(&self) -> &str {
        &self.target
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceSpecError {
    MissingEquals,
    EmptyName,
    EmptyTarget,
}

impl std::fmt::Display for ServiceSpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingEquals => write!(f, "service must be in name=target form"),
            Self::EmptyName => write!(f, "service name must not be empty"),
            Self::EmptyTarget => write!(f, "service target must not be empty"),
        }
    }
}

impl std::error::Error for ServiceSpecError {}

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

    #[test]
    fn parses_named_service_spec() {
        let spec = ServiceSpec::parse("ssh=127.0.0.1:22").unwrap();
        assert_eq!(spec.name().as_str(), "ssh");
        assert_eq!(spec.target(), "127.0.0.1:22");
    }

    #[test]
    fn maps_legacy_target_to_ssh_service() {
        let spec = ServiceSpec::ssh_target("127.0.0.1:22").unwrap();
        assert_eq!(spec.name().as_str(), "ssh");
        assert_eq!(spec.target(), "127.0.0.1:22");
    }

    #[test]
    fn rejects_malformed_service_specs() {
        assert_eq!(
            ServiceSpec::parse("ssh").unwrap_err(),
            ServiceSpecError::MissingEquals
        );
        assert_eq!(
            ServiceSpec::parse("=127.0.0.1:22").unwrap_err(),
            ServiceSpecError::EmptyName
        );
        assert_eq!(
            ServiceSpec::parse("ssh=").unwrap_err(),
            ServiceSpecError::EmptyTarget
        );
    }
}
