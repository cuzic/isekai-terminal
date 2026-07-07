//! Pure protocol value types for the future `isekai-pipe` data plane.
//!
//! This crate is intentionally small while the migration starts. It is the
//! landing zone for values that must be shared by `isekai-pipe connect`,
//! `isekai-pipe serve`, and the `isekai-ssh` wrapper without depending on I/O,
//! async runtimes, Android/iOS bindings, or OpenSSH-specific code.

/// User-facing logical host name, such as `production`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogicalHost(String);

impl LogicalHost {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Named service exposed by `isekai-pipe serve`, such as `ssh`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServiceName(String);

impl ServiceName {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_host_round_trips() {
        let host = LogicalHost::new("production");
        assert_eq!(host.as_str(), "production");
    }

    #[test]
    fn service_name_round_trips() {
        let service = ServiceName::new("ssh");
        assert_eq!(service.as_str(), "ssh");
    }
}
