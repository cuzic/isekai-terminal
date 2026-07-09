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
}

#[derive(Args)]
pub struct InitArgs {
    /// Host to deploy/register, e.g. `myhost`, `myhost:2222`, or
    /// `user@myhost` — same spec accepted by the wrapper. Normalized via
    /// `isekai_trust::normalize_host_port` before being used as the trust
    /// store key.
    pub host: String,

    /// Jump host used to reach `<host>` for this one-time deployment
    /// (`ssh -J`-style `[user@]host[:port]`). Recorded as `last_via` in the
    /// trust store entry, purely informational for the wrapper's own
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
    /// disk just to compile — exactly the trap `isekai_pipe_quic_transport.rs`'s
    /// unconditional `include_bytes!` fell into for `isekai-terminal-core`. Keeping
    /// this an explicit, required CLI argument keeps `isekai-ssh` buildable
    /// in any environment; tests pass the actual binary built alongside
    /// this crate (`CARGO_BIN_EXE_isekai-helper`/the sibling `target/`
    /// directory).
    #[arg(long, value_name = "PATH")]
    pub helper_binary: PathBuf,

    /// The isekai-link relay `isekai-helper --relay` should tunnel through
    /// (`archive/HELPER_PROTOCOL.md`, `archive/ISEKAI_SSH_DESIGN.md` "接続シーケンス").
    #[arg(long, value_name = "ADDR:PORT")]
    pub relay_addr: SocketAddr,

    /// TLS SNI / HTTP authority for `--relay-addr`.
    #[arg(long, value_name = "NAME")]
    pub relay_sni: String,

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

    /// Signed release manifest (`isekai_release_verify::SignedManifest` JSON)
    /// covering `--helper-binary`. When given, `init` verifies the binary's
    /// signature/size/digest/platform/architecture against it
    /// (`isekai-release-verify`, `ISEKAI_PIPE_DESIGN.md` §8 Epic D) *before*
    /// deploying, and refuses to proceed on any mismatch. Optional and
    /// off by default — this only adds a check on top of the existing
    /// SHA-256-pinning trust model, it never replaces it.
    #[arg(long, value_name = "PATH")]
    pub helper_manifest: Option<PathBuf>,

    /// One release-signing public key to trust for `--helper-manifest`
    /// verification, as `<key_id>=<hex-ed25519-pubkey>`. Repeatable — a
    /// manifest is accepted if its own `key_id` field matches one of these.
    /// Required (at least one) when `--helper-manifest` is given; ignored
    /// otherwise. There is deliberately no embedded default key yet: no
    /// release-signing key exists until real GitHub Releases are published
    /// (`ISEKAI_PIPE_DESIGN.md` §8 Epic D).
    #[arg(long = "trusted-release-key", value_name = "KEY_ID=HEXPUBKEY")]
    pub trusted_release_keys: Vec<String>,

    /// The platform `--helper-manifest` must declare (e.g. `linux`) —
    /// required when `--helper-manifest` is given, so a validly signed
    /// manifest for the wrong platform is still rejected rather than
    /// silently deployed.
    #[arg(long, value_name = "PLATFORM")]
    pub expect_platform: Option<String>,

    /// The architecture `--helper-manifest` must declare (e.g. `x86_64`) —
    /// required when `--helper-manifest` is given, same rationale as
    /// `--expect-platform`.
    #[arg(long, value_name = "ARCH")]
    pub expect_architecture: Option<String>,
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
