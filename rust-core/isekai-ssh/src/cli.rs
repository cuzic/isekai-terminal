//! `isekai-ssh` CLI surface (`ISEKAI_SSH_DESIGN.md` "CLIコマンド構成").
//!
//! Phase S-1 only implements `connect` — `init`/`login`/`logout`/`trust` are
//! out of scope (フェーズ分割案 S-2/S-3/S-5).
//!
//! The `--dev-insecure-*` flags on `connect` exist only to unblock this
//! phase's end-to-end test before the trust store (S-2) is wired up. They
//! are compiled in *only* when both `debug_assertions` and the
//! (non-default) `dev-insecure` Cargo feature are active — see `main.rs`'s
//! `compile_error!` guard for why a release build can never even have this
//! feature turned on, and this module's `cfg` gate for why a plain
//! (non-`dev-insecure`) debug build's `--help` also never shows them.

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "isekai-ssh",
    version,
    about = "ssh(1) ProxyCommand wrapper reusing isekai-helper's QUIC connection resilience"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Connect to a trusted host's isekai-helper and relay this process's
    /// stdin/stdout against the established QUIC stream. Meant to be
    /// invoked as `ssh`'s ProxyCommand: non-interactive, and stdout carries
    /// nothing but the raw byte stream from isekai-helper (all logging goes
    /// to stderr) — see `ISEKAI_SSH_DESIGN.md` "ユーザー体験の流れ".
    Connect(ConnectArgs),
}

#[derive(Args)]
pub struct ConnectArgs {
    /// Host alias, as registered via `isekai-ssh init` (trust store lookup
    /// key; `init`/the trust store itself are not implemented yet, S-2/S-3).
    pub host: String,

    /// Jump host used only as a fallback to re-deploy/restart isekai-helper
    /// when the relay path itself is unreachable. Not implemented yet
    /// (reserved for S-3); accepted here so `~/.ssh/config` entries written
    /// against the eventual CLI already parse.
    #[arg(long, value_name = "JUMPHOST")]
    pub via: Option<String>,

    #[cfg(all(debug_assertions, feature = "dev-insecure"))]
    #[command(flatten)]
    pub dev_insecure: DevInsecureArgs,
}

/// DEV/TEST ONLY. Bypasses the (not-yet-implemented) trust store lookup by
/// letting the caller specify isekai-helper's relay-assigned endpoint and
/// session credentials directly. See this module's docs and `main.rs`'s
/// `compile_error!` guard for why this can never ship in a release binary.
#[cfg(all(debug_assertions, feature = "dev-insecure"))]
#[derive(Args)]
pub struct DevInsecureArgs {
    /// DEV ONLY: skip the trust store and connect directly to this
    /// isekai-helper address (e.g. `127.0.0.1:45231`) instead of resolving
    /// `host`. Must be given together with the other `--dev-insecure-*`
    /// flags below.
    #[arg(long, value_name = "ADDR:PORT")]
    pub dev_insecure_target: Option<String>,

    /// DEV ONLY: the target isekai-helper's `cert_sha256` fingerprint
    /// (`HandshakeJson::cert_sha256`), lowercase hex.
    #[arg(long, value_name = "HEX64")]
    pub dev_insecure_cert_sha256: Option<String>,

    /// DEV ONLY: the target isekai-helper's `session_secret`
    /// (`HandshakeJson::session_secret`), base64-encoded.
    #[arg(long, value_name = "BASE64")]
    pub dev_insecure_session_secret: Option<String>,

    /// DEV ONLY: QUIC SNI / server name to present during the handshake.
    /// isekai-helper ignores it (see `isekai_transport::RelayTarget`'s
    /// docs), so the default is just a placeholder.
    #[arg(long, value_name = "NAME", default_value = "isekai-helper")]
    pub dev_insecure_server_name: String,
}
