//! `isekai-ssh` CLI surface for the legacy, interactive trust-bootstrapping
//! subcommands (`archive/ISEKAI_SSH_DESIGN.md` "CLIコマンド構成"). The day-to-day
//! connection path is the non-subcommand wrapper mode (`isekai-ssh
//! <destination>`, `wrapper.rs`), which delegates the actual QUIC relay to
//! the separate `isekai-pipe connect` binary
//! (`archive/ISEKAI_PIPE_MIGRATION.md` P4). The standalone `connect` subcommand that
//! used to duplicate that relay logic directly inside `isekai-ssh` has been
//! removed now that the wrapper covers the same ground.
//!
//! `init` (S-3) is the interactive command that populates the trust store
//! `wrapper.rs` reads from: it deploys/starts `isekai-helper` on a target
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

use clap::{Args, Parser, Subcommand, ValueEnum};

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
    /// Deploy/start isekai-helper on `<host>` (optionally via a jump host)
    /// and, after an explicit `[y/N]` confirmation, register it in the
    /// trust store the wrapper reads from. Interactive by design — see
    /// `init.rs`'s module docs.
    Init(Box<InitArgs>),

    /// Run a Device Authorization Grant (RFC 8628) against the given OAuth
    /// endpoints and save the resulting relay JWT (+ refresh token, if any)
    /// to `~/.config/isekai-ssh/token.json`. Interactive by design (prints
    /// the verification URL/code to stdout and waits) — see `login.rs`'s
    /// module docs.
    Login(LoginArgs),

    /// Delete the saved token file (`~/.config/isekai-ssh/token.json`), if
    /// any. Not an error if already logged out.
    Logout,

    /// Manual diagnostic for `<host>`: reports whether it has ever been
    /// bootstrapped and, if so, actually dials its cached transport
    /// (`isekai-pipe probe`) to check whether it's reachable and whether
    /// the cached trust material (session_secret/cert pin) looks stale.
    /// Never part of `isekai-ssh <host>`'s own connection path — that
    /// already detects and silently recovers from staleness on its own
    /// (`wrapper.rs::run_ssh_with_stale_trust_recovery`); this is purely
    /// for a human to inspect on demand. See `doctor.rs`'s module docs.
    Doctor(DoctorArgs),
}

/// CLI-facing mirror of `isekai_bootstrap::RelayTransportKind` — kept
/// separate rather than deriving `clap::ValueEnum` directly on that type so
/// `isekai-bootstrap` (shared with `isekai-terminal-core`/Android) never
/// needs a `clap` dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RelayTransportArg {
    Udp,
    Qmux,
}

impl From<RelayTransportArg> for isekai_bootstrap::RelayTransportKind {
    fn from(value: RelayTransportArg) -> Self {
        match value {
            RelayTransportArg::Udp => isekai_bootstrap::RelayTransportKind::Udp,
            RelayTransportArg::Qmux => isekai_bootstrap::RelayTransportKind::Qmux,
        }
    }
}

#[derive(Args)]
pub struct InitArgs {
    /// Host to deploy/register, e.g. `myhost`, `myhost:2222`, or
    /// `user@myhost` — same spec accepted by the wrapper. Normalized via
    /// `isekai_trust::normalize_host_port` before being used as the trust
    /// store key.
    pub host: String,

    /// Jump host(s) used to reach `<host>` for this one-time deployment
    /// (`ssh -J`-style `[user@]host[:port]` each). Repeat `--via` to chain
    /// through multiple hops in order (`ISEKAI_PIPE_DESIGN.md` §8 Epic K) —
    /// `isekai-bootstrap::OpenSshBackend` passes the whole chain to `ssh(1)`
    /// as a single comma-joined `-J` argument, not one nested `ssh`
    /// invocation per hop. Recorded (comma-joined) as `last_via` in the
    /// trust store entry, purely informational for the wrapper's own
    /// re-deployment fallback (not implemented yet).
    #[arg(long, value_name = "JUMPHOST")]
    pub via: Vec<String>,

    /// Path to the isekai-helper binary to upload and start on `<host>`.
    ///
    /// There is deliberately no *embedded* default here: a real release of
    /// isekai-ssh is expected to eventually embed a musl-static
    /// isekai-helper binary (S-7, see `rust-core/scripts/build-isekai-helper-musl.sh`),
    /// but doing that today would force every `cargo build -p isekai-ssh` to
    /// require a pre-built musl artifact on disk just to compile — exactly
    /// the trap `isekai_pipe_quic_transport.rs`'s unconditional
    /// `include_bytes!` fell into for `isekai-terminal-core`. When omitted,
    /// `init` instead detects the remote's architecture (`uname -m`) and
    /// downloads a matching release asset (`helper_download`, `--helper-release-repo`/
    /// `--helper-release-tag`) — this only succeeds once this project
    /// actually publishes GitHub Releases (honest gap today, see
    /// `helper_download`'s module docs), so passing this flag explicitly
    /// remains the reliable path until then; tests pass the actual binary
    /// built alongside this crate (`CARGO_BIN_EXE_isekai-helper`/the sibling
    /// `target/` directory).
    #[arg(long, value_name = "PATH")]
    pub helper_binary: Option<PathBuf>,

    /// `owner/repo` to fetch a release asset from when `--helper-binary` is
    /// omitted (`helper_download::ReleaseSource`). Defaults to this
    /// project's own repository.
    #[arg(long, value_name = "OWNER/REPO", default_value = crate::helper_download::ReleaseSource::DEFAULT_REPO)]
    pub helper_release_repo: String,

    /// Pin a specific release tag to fetch a helper binary from when
    /// `--helper-binary` is omitted. Defaults to the latest release.
    #[arg(long, value_name = "TAG")]
    pub helper_release_tag: Option<String>,

    /// The isekai-link relay `isekai-helper --relay` should tunnel through
    /// (`archive/HELPER_PROTOCOL.md`, `archive/ISEKAI_SSH_DESIGN.md` "接続シーケンス").
    #[arg(long, value_name = "ADDR:PORT")]
    pub relay_addr: SocketAddr,

    /// TLS SNI / HTTP authority for `--relay-addr`.
    #[arg(long, value_name = "NAME")]
    pub relay_sni: String,

    /// Transport the deployed isekai-helper uses to reach `--relay-addr`
    /// itself (`#qmux-leg2`). `qmux` (QMux-over-TLS-over-TCP,
    /// EXPERIMENTAL — wire compatibility with the deployed relay is
    /// unverified) is for networks that block outbound UDP on the *server*
    /// side; see `isekai_bootstrap::RelayTransportKind`'s docs for why this
    /// is a static, evidence-gated choice, not a runtime fallback.
    #[arg(long, value_enum, default_value_t = RelayTransportArg::Udp)]
    pub relay_transport: RelayTransportArg,

    /// Bearer token authenticating isekai-helper to the relay. Exactly one
    /// of `--relay-jwt`/`--relay-jwt-from-login` is required.
    #[arg(long, value_name = "TOKEN", required_unless_present = "relay_jwt_from_login")]
    pub relay_jwt: Option<String>,

    /// Source the relay bearer token from `isekai-ssh login`'s saved token
    /// file (`isekai_auth::FileTokenProvider`, `~/.config/isekai-ssh/token.json`,
    /// auto-refreshed if near expiry) instead of passing it directly via
    /// `--relay-jwt`. Exactly one of the two is required
    /// (`ISEKAI_PIPE_DESIGN.md` §8 Epic F).
    #[arg(long, conflicts_with = "relay_jwt")]
    pub relay_jwt_from_login: bool,

    /// Free-form version string recorded as `trusted_helper_version` in the
    /// trust store entry (no automated version detection exists yet — this
    /// is display/bookkeeping only, matched against nothing).
    #[arg(long, value_name = "VERSION", default_value = "unknown")]
    pub helper_version: String,

    /// Recorded as `release_channel` in the trust store entry. Unused by
    /// any policy decision today (`UpdatePolicy::ExactDigestOnly` is the
    /// only variant, `archive/ISEKAI_SSH_DESIGN.md` "trust store のファイル形式").
    #[arg(long, value_name = "NAME")]
    pub release_channel: Option<String>,

    /// Passed straight through as `isekai-helper --max-idle-lifetime <SECS>`:
    /// how long the deployed helper stays running with no active connection
    /// before self-exiting. Defaults to 30 days rather than isekai-helper's
    /// own 600s default, because `init` deploys a helper once and the
    /// wrapper is expected to keep dialing that same long-running process
    /// across many separate, possibly hours/days-apart `ssh` invocations —
    /// unlike `isekai-terminal-core`'s (Android's) per-session bootstrap,
    /// which re-deploys a fresh helper on every connection attempt and so is
    /// unaffected by a short self-exit window
    /// (`archive/ISEKAI_SSH_DESIGN.md` "引き続き未決の項目").
    #[arg(long, value_name = "SECS", default_value_t = 2_592_000)]
    pub idle_lifetime: u64,

    /// STUN server(s) to query for this side's own observed address,
    /// exchanged with the remote isekai-helper over the bootstrap channel
    /// (`#20b`) — this side's candidates go out as
    /// `BootstrapRequestV2.client_candidates`, and the first one is also
    /// passed to the remote `isekai-helper` so it reports its own
    /// `server-reflexive` candidate back. Repeatable; omit entirely to
    /// disable STUN candidate exchange (today's pre-`#20b` behavior).
    #[arg(long = "stun-server", value_name = "ADDR:PORT")]
    pub stun_servers: Vec<SocketAddr>,
}

#[derive(Args)]
pub struct LoginArgs {
    /// RFC 8628 §3.1 device authorization endpoint URL. Not hardcoded: the
    /// real Auth0 tenant URL isn't fixed yet
    /// (`archive/ISEKAI_SSH_DESIGN.md` "引き続き未決の項目").
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

#[derive(Args)]
pub struct DoctorArgs {
    /// Host to diagnose, same spec `init`/the wrapper accept.
    pub host: String,

    /// If a stale-trust signal is found, immediately re-bootstrap (no
    /// `[y/N]` prompt — same silent-refresh behavior as the wrapper's own
    /// automatic recovery). Without this flag, `doctor` only reports what
    /// it found.
    #[arg(long)]
    pub fix: bool,

    /// STUN server to probe against, if the profile has cached STUN
    /// evidence — mirrors `isekai-pipe probe --stun-server`. Omit to only
    /// probe the profile's primary (relay/direct) transport.
    #[arg(long, value_name = "ADDR:PORT")]
    pub stun_server: Option<SocketAddr>,

    /// Path to the isekai-helper binary to upload during `--fix`'s
    /// re-bootstrap — mirrors the wrapper's own `--isekai-helper-binary`.
    /// When omitted, `--fix` falls through to the same arch-detection +
    /// GitHub Release auto-download `helper_download::resolve_helper_binary`
    /// already provides for the wrapper's own automatic recovery (honest
    /// gap: only actually succeeds once this project publishes releases,
    /// see `helper_download.rs`'s module docs).
    #[arg(long, value_name = "PATH")]
    pub helper_binary: Option<PathBuf>,
}
