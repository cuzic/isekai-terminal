//! Value types shared by `BootstrapBackend` implementations
//! (`archive/ISEKAI_SSH_DESIGN.md` "`--via` の実装方式").

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
/// (`archive/ISEKAI_SSH_DESIGN.md` "`--via` フォールバックの2つの用途").
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
/// ... --max-idle-lifetime ...` (`IsekaiPipeP2pMode::Relay` in
/// `rust-core/src/helper_bootstrap.rs`, `archive/HELPER_PROTOCOL.md`). STUN/P2P
/// launch is out of scope for this phase (`archive/ISEKAI_SSH_DESIGN.md` フェーズ
/// 分割案 S-0e-1/S-6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayLaunchSpec {
    pub relay_addr: SocketAddr,
    pub relay_sni: String,
    pub relay_jwt: String,
    /// `isekai-helper --max-idle-lifetime <SECS>`: how long the deployed
    /// helper stays running with no active connection before it self-exits.
    /// `isekai-helper`'s own default (600s) is tuned for `isekai-terminal-core`'s
    /// per-session bootstrap model (Android re-deploys/re-launches a fresh
    /// helper on every connection attempt, so a short self-exit window is
    /// pure cleanup). `isekai-ssh init` deploys a helper once and expects
    /// `connect` to keep dialing that *same* long-running process across
    /// many separate `ssh` invocations, potentially hours or days apart, so
    /// callers building a `RelayLaunchSpec` for that use case must pass a
    /// much larger value explicitly (`archive/ISEKAI_SSH_DESIGN.md` "引き続き未決の
    /// 項目" — resolved by making this field required rather than defaulted
    /// inside this crate, keeping the policy decision in `isekai-ssh`
    /// itself and leaving `isekai-helper`'s own default untouched).
    pub idle_lifetime_secs: u64,
}

/// What a successful `BootstrapBackend::install_and_start` call yields: the
/// handshake JSON `isekai-helper` printed once it was up and running
/// (`archive/HELPER_PROTOCOL.md` §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapReport {
    pub handshake: HandshakeJson,
}

/// How to launch the uploaded `isekai-helper` binary once it's on the
/// target host (`isekai-ssh init`'s `Relay` path vs. the wrapper's
/// auto-bootstrap `Direct` path, `archive/ISEKAI_PIPE_MIGRATION.md` P4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchSpec {
    /// `isekai-helper --relay ... --relay-sni ... --relay-jwt-file ...`.
    Relay(RelayLaunchSpec),
    /// `isekai-helper --bind 0.0.0.0:0` (no relay, no STUN): the
    /// `direct-by-bootstrap-host` candidate the client already knows how to
    /// dial (the same SSH bootstrap host, at the port this launch reports).
    /// Scoped deliberately narrow for the wrapper's auto-bootstrap: no
    /// relay JWT sourcing exists there yet, and this mode needs none.
    Direct { idle_lifetime_secs: u64 },
}
