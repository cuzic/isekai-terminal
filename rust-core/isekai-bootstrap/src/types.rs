//! Value types shared by `BootstrapBackend` implementations
//! (`ISEKAI_SSH_DESIGN.md` "`--via` の実装方式").

use std::net::SocketAddr;

use isekai_protocol::HandshakeJson;

/// The host to bootstrap `isekai-helper` on (the `<host>` argument to
/// `isekai-ssh init`/`connect`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSpec {
    pub host: String,
    /// `None` lets `ssh(1)` fall back to its config file / default (22).
    pub port: Option<u16>,
    /// `None` lets `ssh(1)` fall back to its config file / the invoking
    /// user's name.
    pub user: Option<String>,
}

impl HostSpec {
    pub fn new(host: impl Into<String>) -> Self {
        Self { host: host.into(), port: None, user: None }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// The positional destination argument ssh(1) expects: `[user@]host`.
    /// Port is passed separately via `-p`, matching how `ssh_config(5)`'s
    /// `Port`/`User` keywords are conventionally overridden from the CLI.
    pub(crate) fn ssh_destination(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

/// A `-J`/`ProxyJump` hop used only as the "`--via`" fallback path
/// (`ISEKAI_SSH_DESIGN.md` "`--via` フォールバックの2つの用途").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpSpec {
    pub host: String,
    pub port: Option<u16>,
    pub user: Option<String>,
}

impl JumpSpec {
    pub fn new(host: impl Into<String>) -> Self {
        Self { host: host.into(), port: None, user: None }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// The single-token form ssh(1)'s `-J` flag accepts:
    /// `[user@]host[:port]`.
    pub(crate) fn to_arg(&self) -> String {
        let dest = match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        };
        match self.port {
            Some(port) => format!("{dest}:{port}"),
            None => dest,
        }
    }
}

/// Arguments passed to `isekai-helper --relay ... --relay-sni ... --relay-jwt
/// ...` (`HelperP2pMode::Relay` in `rust-core/src/helper_bootstrap.rs`,
/// `HELPER_PROTOCOL.md`). STUN/P2P launch is out of scope for this phase
/// (`ISEKAI_SSH_DESIGN.md` フェーズ分割案 S-0e-1/S-6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayLaunchSpec {
    pub relay_addr: SocketAddr,
    pub relay_sni: String,
    pub relay_jwt: String,
}

/// What a successful `BootstrapBackend::install_and_start` call yields: the
/// handshake JSON `isekai-helper` printed once it was up and running
/// (`HELPER_PROTOCOL.md` §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapReport {
    pub handshake: HandshakeJson,
}
