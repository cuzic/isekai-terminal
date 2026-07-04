//! `isekai-ssh` CLI surface (`ISEKAI_SSH_DESIGN.md` "CLIコマンド構成").
//!
//! `connect`, `init`, `login`, and `logout` are implemented; `trust` is still
//! out of scope. As of S-2, `connect` resolves its
//! target from the trust store (`isekai-trust`,
//! `~/.config/isekai-ssh/known_helpers.toml`) by default; the
//! `--dev-insecure-*` flags below remain only as a debug/test-only bypass
//! of that lookup (originally added to unblock S-1's end-to-end test before
//! the trust store existed). They are compiled in *only* when both
//! `debug_assertions` and the (non-default) `dev-insecure` Cargo feature are
//! active — see `main.rs`'s `compile_error!` guard for why a release build
//! can never even have this feature turned on, and this module's `cfg` gate
//! for why a plain (non-`dev-insecure`) debug build's `--help` also never
//! shows them.
//!
//! `init` (S-3) is the interactive counterpart that populates the trust
//! store `connect` reads from: it deploys/starts `isekai-helper` on a target
//! host (via `isekai-bootstrap::OpenSshBackend`) and, on confirmation, writes
//! a `HelperTrust` entry. See `init.rs`'s module docs for the full flow.
//!
//! `login`/`logout` (S-5) manage the relay JWT `isekai-helper --relay` needs
//! (`init`'s `--relay-jwt` flag, still passed explicitly — wiring `init`'s
//! `--relay-jwt` to default to the logged-in token is future work, not this
//! phase's scope): `login` runs a Device Authorization Grant (RFC 8628)
//! against caller-supplied OAuth endpoints and saves the resulting token via
//! `isekai-auth`; `logout` deletes the saved token file. See `login.rs`'s
//! module docs for the full flow.

use std::net::SocketAddr;
use std::path::PathBuf;

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

    /// Deploy/start isekai-helper on `<host>` (optionally via a jump host)
    /// and, after an explicit `[y/N]` confirmation, register it in the
    /// trust store `connect` reads from. Interactive by design — see
    /// `init.rs`'s module docs.
    Init(InitArgs),

    /// Run a Device Authorization Grant (RFC 8628) against the given OAuth
    /// endpoints and save the resulting relay JWT (+ refresh token, if any)
    /// to `~/.config/isekai-ssh/token.json`. Interactive by design (prints
    /// the verification URL/code to stdout and waits) — see `login.rs`'s
    /// module docs.
    Login(LoginArgs),

    /// Delete the saved token file (`~/.config/isekai-ssh/token.json`), if
    /// any. Not an error if already logged out.
    Logout,
}

#[derive(Args)]
pub struct ConnectArgs {
    /// Host alias, as registered via `isekai-ssh init` (trust store lookup
    /// key, normalized via `isekai_trust::normalize_host_port`; `init`
    /// itself is not implemented yet, S-3 — until then, hosts must be
    /// registered by writing `~/.config/isekai-ssh/known_helpers.toml`
    /// directly, e.g. via `isekai-trust::save_trust_store`).
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

/// DEV/TEST ONLY. Bypasses the trust store lookup (S-2) by letting the
/// caller specify isekai-helper's relay-assigned endpoint and session
/// credentials directly. See this module's docs and `main.rs`'s
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

#[derive(Args)]
pub struct InitArgs {
    /// Host to deploy/register, e.g. `myhost`, `myhost:2222`, or
    /// `user@myhost` — same spec accepted by `connect`. Normalized via
    /// `isekai_trust::normalize_host_port` before being used as the trust
    /// store key.
    pub host: String,

    /// Jump host used to reach `<host>` for this one-time deployment
    /// (`ssh -J`-style `[user@]host[:port]`). Recorded as `last_via` in the
    /// trust store entry, purely informational for `connect`'s own
    /// re-deployment fallback (not implemented yet).
    #[arg(long, value_name = "JUMPHOST")]
    pub via: Option<String>,

    /// Path to the isekai-helper binary to upload and start on `<host>`.
    ///
    /// There is deliberately no default here: a real release of isekai-ssh
    /// is expected to eventually embed a musl-static isekai-helper binary
    /// (S-7, see `rust-core/scripts/build-isekai-helper-musl.sh`) so this
    /// flag becomes optional, but doing that today would force every
    /// `cargo build -p isekai-ssh` to require a pre-built musl artifact on
    /// disk just to compile — exactly the trap `helper_quic_transport.rs`'s
    /// unconditional `include_bytes!` fell into for `tssh-core`. Keeping
    /// this an explicit, required CLI argument keeps `isekai-ssh` buildable
    /// in any environment; tests pass the actual binary built alongside
    /// this crate (`CARGO_BIN_EXE_isekai-helper`/the sibling `target/`
    /// directory).
    #[arg(long, value_name = "PATH")]
    pub helper_binary: PathBuf,

    /// The isekai-link relay `isekai-helper --relay` should tunnel through
    /// (`HELPER_PROTOCOL.md`, `ISEKAI_SSH_DESIGN.md` "接続シーケンス").
    #[arg(long, value_name = "ADDR:PORT")]
    pub relay_addr: SocketAddr,

    /// TLS SNI / HTTP authority for `--relay-addr`.
    #[arg(long, value_name = "NAME")]
    pub relay_sni: String,

    /// Bearer token authenticating isekai-helper to the relay. Obtaining
    /// this automatically (Device Authorization Flow, `isekai-ssh login`)
    /// is out of scope for this phase (S-5) — pass it directly for now.
    #[arg(long, value_name = "TOKEN")]
    pub relay_jwt: String,

    /// Free-form version string recorded as `trusted_helper_version` in the
    /// trust store entry (no automated version detection exists yet — this
    /// is display/bookkeeping only, matched against nothing).
    #[arg(long, value_name = "VERSION", default_value = "unknown")]
    pub helper_version: String,

    /// Recorded as `release_channel` in the trust store entry. Unused by
    /// any policy decision today (`UpdatePolicy::ExactDigestOnly` is the
    /// only variant, `ISEKAI_SSH_DESIGN.md` "trust store のファイル形式").
    #[arg(long, value_name = "NAME")]
    pub release_channel: Option<String>,
}

#[derive(Args)]
pub struct LoginArgs {
    /// RFC 8628 §3.1 device authorization endpoint URL. Not hardcoded: the
    /// real Auth0 tenant URL isn't fixed yet
    /// (`ISEKAI_SSH_DESIGN.md` "引き続き未決の項目").
    #[arg(long, value_name = "URL")]
    pub device_auth_endpoint: String,

    /// RFC 8628 §3.4 token endpoint URL, polled during `login` and reused
    /// later by `FileTokenProvider::get_relay_jwt`'s auto-refresh
    /// (`isekai_auth::refresh`).
    #[arg(long, value_name = "URL")]
    pub token_endpoint: String,

    /// OAuth client id for this (public, device-flow) client.
    #[arg(long, value_name = "ID")]
    pub client_id: String,
}
