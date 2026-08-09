//! Pure protocol value types for the future `isekai-pipe` data plane.
//!
//! This crate is intentionally small while the migration starts. It is the
//! landing zone for values that must be shared by `isekai-pipe connect`,
//! `isekai-pipe serve`, and the `isekai-ssh` wrapper without depending on I/O,
//! async runtimes, Android/iOS bindings, or OpenSSH-specific code.

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
    fn service_name_round_trips() {
        let service = ServiceName::new("ssh");
        assert_eq!(service.as_str(), "ssh");
    }
}
