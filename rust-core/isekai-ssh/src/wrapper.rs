//! Minimal OpenSSH frontend for the `chatgpt.md` migration path.
//!
//! `init`/`login`/`logout` remain as the interactive trust-store
//! subcommands. A non-subcommand invocation, such as `isekai-ssh
//! production`, is treated as an OpenSSH invocation with an injected
//! `ProxyCommand` that delegates the byte stream to `isekai-pipe connect`.

use std::io::IsTerminal as _;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use isekai_auth::TokenProvider;
use isekai_bootstrap::{HostSpec, JumpSpec, LaunchSpec, RelayLaunchSpec, RelayTransportKind};
use isekai_bootstrap_plan::{classify_bootstrap_error, BootstrapFailure};
use isekai_pipe_core::{
    claim_connect_outcome, default_log_file, default_profiles_dir, default_runtime_dir, load_persistent_profile,
    write_connection_intent, write_persistent_profile, BootstrapProvenance, ConnectionIntent, IntentTransport,
    PersistentProfile, ServiceSpec,
};
#[cfg(test)]
use isekai_pipe_core::{DEFAULT_CANDIDATE_RACE_DELAY_MS, DEFAULT_RELAY_DELAY_MS};
use isekai_trust::{HelperTrust, UpdatePolicy};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::log_file::{log_line, log_line_verbose};
use crate::reconnect_backoff;

mod config;
mod directive;
use config::resolve_isekai_config;

/// Reserved words that `should_run_wrapper` never treats as an SSH
/// destination — the interactive trust-store subcommands (`init`/`login`/
/// `logout`, all pre-existing), `doctor` (manual diagnostic,
/// `ISEKAI_PIPE_DESIGN.md` §8 Epic N), and `build-profile` (Epic P's local
/// build-profile config management, `build_profile_cli.rs`). Not purely
/// "legacy" any more, but kept as one flat list since `should_run_wrapper`'s
/// only job is "is the first arg one of these known subcommand names or an
/// SSH destination".
const RESERVED_SUBCOMMANDS: &[&str] = &["init", "login", "logout", "doctor", "build-profile"];

/// Matches `isekai-ssh init`'s own default (`cli::InitArgs::idle_lifetime`):
/// the auto-bootstrapped helper is expected to keep running across many
/// separate `isekai-ssh <destination>` invocations, possibly hours/days
/// apart, unlike `isekai-terminal-core`'s (Android's) per-session bootstrap.
const DEFAULT_IDLE_LIFETIME_SECS: u64 = 2_592_000;

/// Bounds `resolve_stun_servers`'s DNS fallback per entry. `getaddrinfo` is
/// already self-bounding (glibc's own `resolv.conf` timeout × attempts ×
/// nameserver count), but that budget can still run to tens of seconds for
/// one bad hostname — matching the rest of this crate's practice of wrapping
/// bootstrap I/O in an explicit `tokio::time::timeout` (e.g.
/// `russh_backend.rs`'s command execution) rather than trusting an
/// OS-level bound alone. STUN entries are an opt-in, best-effort NAT
/// traversal hint, not something worth blocking a connection attempt over.
const STUN_DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the caller requested a PTY for the remote session, mirroring
/// `ssh(1)`'s `-t`/`-T`/`-tt` flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestTty {
    /// No explicit `-t`/`-T` given: allocate a PTY for an interactive
    /// (no-remote-command) session. **Diverges from real `ssh(1)`, which
    /// skips the PTY for a one-shot remote command under `Auto`** — this
    /// codebase's `decide_session_kind`/`wants_pty` (`native/connect.rs`)
    /// deliberately still allocates one (`SessionKind::Shell { command:
    /// Some(_), .. }`, execing the command over that PTY) so a command
    /// needing a real terminal (an editor, a pager, `sudo` prompting for a
    /// password) keeps working without the caller having to remember `-t`.
    /// The tradeoff — CRLF newline translation and merged stdout/stderr on
    /// the PTY, same as real `ssh -t host cmd` — matches this project's
    /// history of choosing convenience for interactive terminal use over
    /// strict `ssh(1)` parity elsewhere. Pass `-T` to opt out.
    Auto,
    /// `-t`: force PTY allocation even for a remote command.
    Yes,
    /// `-T`: never allocate a PTY.
    No,
    /// `-tt`: force PTY allocation even when local stdin is not a tty
    /// (ssh(1) uses this for batch-mode PTY-forcing).
    Force,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WrapperPlan {
    openssh_path: PathBuf,
    pipe_path: PathBuf,
    /// The raw `ssh(1)`-style destination token (e.g. `cuzic@vpsmart`).
    destination: String,
    /// The host part of the destination, with `user@` stripped (e.g. `vpsmart`).
    /// Used for config resolution (`Host` block matching) and host key
    /// verification, which don't understand `user@host` syntax.
    destination_host: String,
    /// The user part of the destination, if present (e.g. `Some("cuzic")` from
    /// `cuzic@vpsmart`). Takes precedence over `ssh_config`'s `User` and the
    /// local username — matching `ssh(1)`'s own precedence.
    destination_user: Option<String>,
    destination_index: usize,
    ssh_args: Vec<String>,
    isekai: WrapperIsekaiOptions,
    pub(crate) request_tty: RequestTty,
}

impl WrapperPlan {
    /// Resolved `isekai-pipe` binary path (`default_pipe_path`/
    /// `--isekai-pipe-path`) — `doctor.rs`'s only direct read of
    /// `WrapperPlan`, to shell out to `isekai-pipe probe` with the same
    /// binary the wrapper itself would use for `ProxyCommand`.
    pub(crate) fn pipe_path(&self) -> &Path {
        &self.pipe_path
    }

    /// The `ssh(1)`-style destination token (e.g. `production`, not the
    /// `ssh -G`/`openssh-config`-resolved `HostName`) — `native/connect.rs`'s
    /// only other direct read of `WrapperPlan` besides `pipe_path()`.
    pub(crate) fn destination(&self) -> &str {
        &self.destination
    }

    /// The host part of the destination with `user@` stripped. Used for
    /// config resolution and host key verification.
    pub(crate) fn destination_host(&self) -> &str {
        &self.destination_host
    }

    /// The user part of the `user@host` destination, if any.
    pub(crate) fn destination_user(&self) -> Option<&str> {
        self.destination_user.as_deref()
    }

    /// `--isekai-log-file <PATH>` (`log_file.rs`), if given. The native
    /// connect path (`native/connect.rs::run`) reads this to call
    /// `crate::log_file::init` itself, mirroring the `crate::log_file::init`
    /// call `wrapper::run` (the Unix path) already makes — without this the
    /// flag was silently ignored on Windows. Mirrors the existing
    /// `destination()`/`pipe_path()` getters.
    pub(crate) fn log_file(&self) -> Option<&Path> {
        self.isekai.log_file.as_deref()
    }

    /// Number of parsed `ssh(1)`-style args (options + destination + any
    /// trailing remote command). The native ctl-socket path
    /// (`native/mux/ctl_forward.rs`) feeds this to
    /// [`crate::ctl_forward::should_attempt_ctl_forward`] to skip the forward
    /// for a one-shot remote command, exactly as the Unix path does.
    pub(crate) fn ssh_args_len(&self) -> usize {
        self.ssh_args.len()
    }

    /// Index of the destination token within the parsed ssh args — anything
    /// *after* it is a remote command. Paired with [`Self::ssh_args_len`] for
    /// the native ctl-socket interactive-session check.
    pub(crate) fn destination_index(&self) -> usize {
        self.destination_index
    }

    /// The remote command (everything after the destination in the ssh args),
    /// if any. `None` means an interactive login shell.
    pub(crate) fn remote_command(&self) -> Option<&[String]> {
        let tail = &self.ssh_args[self.destination_index + 1..];
        if tail.is_empty() { None } else { Some(tail) }
    }
}

/// `--isekai-tty` (bare) vs `--isekai-tty=<name>` — see `tty_attach.rs`'s
/// `resolve_exec_command` for how each resolves to an actual
/// `isekai-pipe tty attach <name>` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TtySelection {
    /// Derives `<name>` — see `tty_attach.rs::TAB_SESSION_ENV_CANDIDATES`'s
    /// doc comment for the actual priority-ordered rule (an explicit
    /// `$ISEKAI_TTY_SESSION` opt-in, then whichever supported
    /// terminal/multiplexer env var is set, one daemon per *tab/pane*,
    /// falling back to `isekai-<profile>`, one daemon per *host*, when none
    /// of them are).
    Auto,
    Named(String),
}

/// `#@isekai tty auto|off|<name>` — the config-directive counterpart to
/// `--isekai-tty[=<name>]`. A separate type from [`TtySelection`] rather
/// than reusing it because a directive needs a third state a CLI flag
/// doesn't: `Off`, so a narrower `Host` block can opt back out of a broader
/// `Host *` directive (`ssh_config(5)` first-match-wins semantics) — e.g.
/// `Host *` turns tty-attach on everywhere, one particular `Host` still
/// wants a plain login shell. See [`resolved_tty_selection`] for how this
/// combines with the CLI flag.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TtyDirective {
    Auto,
    Named(String),
    Off,
}

/// `--isekai-tty[=<name>]` (CLI, [`WrapperIsekaiOptions::tty`]) combined
/// with `#@isekai tty auto|off|<name>` (config directive,
/// [`IsekaiConfig::tty`]) — an explicit CLI flag always wins, in *both*
/// directions, over whatever the directive says (including `Off`), so a
/// config-enabled default can still be overridden per invocation. Not the
/// same shape as [`should_bootstrap`], despite the superficial CLI/config
/// resemblance: there, config's `bootstrap-policy never` is a *veto* over an
/// explicit `--isekai-bootstrap` — there is no equivalent veto here. Every
/// consumer (`apply_ctl_socket_forward`, `native::connect::run_authenticated_session`,
/// `native::mux::mod::run_as_client_over`) only honors the result when
/// [`WrapperPlan::remote_command`] is `None` — an explicit trailing remote
/// command always wins silently, the same opportunistic-fallback convention
/// `#@isekai ctl-socket` already follows (never blocks a connection over an
/// optional feature).
pub(crate) fn resolved_tty_selection(plan: &WrapperPlan, resolution: &WrapperResolution) -> Option<TtySelection> {
    if let Some(selection) = &plan.isekai.tty {
        return Some(selection.clone());
    }
    match resolution.isekai.tty.as_ref()? {
        TtyDirective::Auto => Some(TtySelection::Auto),
        TtyDirective::Named(name) => Some(TtySelection::Named(name.clone())),
        TtyDirective::Off => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WrapperIsekaiOptions {
    bootstrap: bool,
    no_bootstrap: bool,
    direct: bool,
    explain: bool,
    dry_run: bool,
    /// Local path to the `isekai-helper` binary to upload when auto
    /// bootstrap is triggered (`--isekai-helper-binary`). No embedded
    /// default exists yet — see `cli::InitArgs::helper_binary`'s doc comment
    /// for why this stays an explicit argument rather than a guessed
    /// default. When `None`, `bootstrap_and_register` falls through to
    /// `helper_download::resolve_helper_binary` (arch detection + GitHub
    /// Release download) before giving up.
    helper_binary: Option<PathBuf>,
    /// Mirrors `cli::InitArgs::helper_release_repo`/`helper_release_tag` —
    /// see `helper_download::ReleaseSource`.
    helper_release_repo: String,
    helper_release_tag: Option<String>,
    /// `--isekai-log-file <PATH>` (`log_file.rs`): tees this invocation's
    /// diagnostic output (this process's own status messages, plus `ssh(1)`'s
    /// — and by extension `isekai-pipe connect`'s — stderr) into a file, in
    /// addition to the terminal.
    log_file: Option<PathBuf>,
    /// `--isekai-tty[=<name>]` — see [`resolved_tty_selection`].
    tty: Option<TtySelection>,
}

impl Default for WrapperIsekaiOptions {
    fn default() -> Self {
        Self {
            bootstrap: false,
            no_bootstrap: false,
            direct: false,
            explain: false,
            dry_run: false,
            helper_binary: None,
            helper_release_repo: crate::helper_download::ReleaseSource::DEFAULT_REPO.to_string(),
            helper_release_tag: None,
            log_file: None,
            tty: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct OpenSshEffectiveConfig {
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    proxy_jump: Option<String>,
    proxy_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IsekaiConfig {
    enabled: bool,
    bootstrap_policy: BootstrapPolicy,
    profile: String,
    remote_path: Option<String>,
    services: Vec<ServiceSpec>,
    bootstrap_candidates: Vec<BootstrapCandidate>,
    link_endpoints: Vec<String>,
    rendezvous: Vec<String>,
    stun_servers: Vec<String>,
    relay_endpoints: Vec<String>,
    resume_grace_secs: u64,
    candidate_race_delay_ms: u64,
    relay_delay_ms: u64,
    install_mode: InstallMode,
    bootstrap_relay: Option<BootstrapRelayTarget>,
    /// `#@isekai remote-log-level` (`isekai-helper --log-level`). Defaults
    /// to isekai-helper's own built-in default (`info`) rather than
    /// something more verbose, so debugging a stuck connection is an
    /// explicit opt-in per host, not a standing cost on every deployment.
    remote_log_level: String,
    /// `#@isekai remote-bind-port-range` (`isekai-helper --bind-port-range`).
    /// `None` leaves the deployed helper on its own default (an
    /// OS-assigned ephemeral UDP port). Lets an operator narrow which
    /// inbound UDP range a host's firewall needs to allow. Named with an
    /// explicit `remote_`/`remote-` prefix, distinct from any future
    /// *local* port-range setting for `isekai-pipe connect`'s own outbound
    /// bind on this machine.
    remote_bind_port_range: Option<(u16, u16)>,
    /// `#@isekai local-bind-port-range` (`ConnectionIntent::local_bind_port_range`,
    /// `isekai-pipe connect`'s own outbound QUIC socket). `None` leaves this
    /// side on an OS-assigned ephemeral UDP port. The client-side
    /// counterpart of `remote_bind_port_range` — this machine's own
    /// firewall/NAT is the thing being accommodated here, not the remote
    /// host's.
    local_bind_port_range: Option<(u16, u16)>,
    /// `#@isekai ctl-socket yes` (`ISEKAI_PIPE_DESIGN.md` §8 Epic M):
    /// opt-in, default off. Requests a per-invocation `-R` UNIX domain
    /// socket forward carrying the title/clipboard control-plane, so it
    /// works even when this connection ends up sharing an underlying
    /// transport via ControlMaster/ControlPersist (see `ctl_forward.rs`).
    ctl_socket_enabled: bool,
    /// `#@isekai tab-idle-color <rrggbb>` (`ISEKAI_PIPE_DESIGN.md` §8 Epic
    /// Q): the tab background color `claude-hookd` restores once no Claude
    /// Code session on this tab needs attention. `None` (no directive) means
    /// "use the built-in default" — resolved on the remote side by
    /// `claude-hookd`, not here, since `ISEKAI_TAB_IDLE_COLOR` is only
    /// exported into the remote session's environment when this is `Some`.
    tab_idle_color: Option<(u8, u8, u8)>,
    /// `#@isekai tab-attention-color <rrggbb>`, the counterpart to
    /// `tab_idle_color` for the "a session needs your input" state.
    tab_attention_color: Option<(u8, u8, u8)>,
    /// `#@isekai tty auto|off|<name>` — see [`resolved_tty_selection`] for
    /// how this combines with `--isekai-tty[=<name>]`.
    tty: Option<TtyDirective>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootstrapPolicy {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BootstrapCandidate {
    target: String,
    via: Vec<String>,
    priority: u32,
    /// The original, un-resolved `ssh(1)` destination token (e.g. `vpsmart`,
    /// not the `ssh -G`-resolved `HostName` in `target`) for the *default*
    /// candidate only — `None` for candidates from an explicit `#@isekai
    /// bootstrap-candidate target=...` directive, which are literal
    /// addresses with no alias to speak of. `bootstrap_and_register` uses
    /// this, when present, as the destination it actually hands to the
    /// `ssh(1)` subprocess that deploys `isekai-pipe`, so that `Host
    /// <alias>` blocks in the user's own `~/.ssh/config` (`IdentityFile`,
    /// `ProxyCommand`, etc.) still match — matching them against the
    /// resolved `HostName` instead (as this used to do) silently drops
    /// every such directive, since `ssh_config(5)` `Host` patterns match
    /// against the literal destination argument, not the resolved
    /// `HostName`.
    alias: Option<String>,
}

/// `#@isekai bootstrap-relay addr=<SocketAddr> sni=<name>` (`ISEKAI_PIPE_DESIGN.md`
/// §8 Epic H): opts auto-bootstrap into deploying via relay instead of
/// `direct-by-bootstrap-host`. Deliberately a distinct directive/type from
/// `relay_endpoints` (the `#@isekai relay <url>` connect-time fallback
/// list) — `RelayLaunchSpec` needs an address+SNI pair atomically, which
/// that list's bare-string shape can't express.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BootstrapRelayTarget {
    relay_addr: SocketAddr,
    relay_sni: String,
    /// `transport=qmux` (`#qmux-leg2`, defaults to `udp`) — see
    /// `isekai_bootstrap::RelayTransportKind`'s docs for why this is a
    /// static, evidence-gated choice at bootstrap time, not a runtime
    /// fallback.
    relay_transport: RelayTransportKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallMode {
    User,
    System,
}

#[derive(Debug, Clone)]
pub(crate) struct WrapperResolution {
    openssh: OpenSshEffectiveConfig,
    isekai: IsekaiConfig,
}

impl WrapperResolution {
    /// The trust-store/`ConnectionIntent` profile name this destination
    /// resolved to — the one piece of `WrapperResolution` `doctor.rs` needs
    /// to read directly (everything else it does is via `wrapper.rs`'s own
    /// `pub(crate)` functions, which already take `&WrapperResolution`).
    pub(crate) fn profile(&self) -> &str {
        &self.isekai.profile
    }

    /// Whether `#@isekai` routing applies to this destination at all — the
    /// native connect path (`native/connect.rs`) needs this to decide
    /// between routing through `isekai-pipe connect --stdio` and a plain
    /// direct connect, the same branch `run()` makes via `run_openssh_direct`.
    pub(crate) fn isekai_enabled(&self) -> bool {
        self.isekai.enabled
    }

    /// `#@isekai ctl-socket yes` (Epic M): whether the per-tab title/clipboard
    /// control-plane forward is opted in. The native path
    /// (`native/mux/ctl_forward.rs`) reads this to decide whether to request a
    /// streamlocal forward on its `russh` handle; the Unix path reads the
    /// private field directly (same module).
    pub(crate) fn ctl_socket_enabled(&self) -> bool {
        self.isekai.ctl_socket_enabled
    }

    /// `#@isekai tab-idle-color <rrggbb>` (Epic Q), already validated and
    /// parsed at config-resolution time. Both the Unix (`ctl_forward.rs`,
    /// same module) and native (`native/connect.rs`, `native/mux/owner.rs`)
    /// paths need this to build the login-shell command's `export` clauses.
    pub(crate) fn tab_idle_color(&self) -> Option<(u8, u8, u8)> {
        self.isekai.tab_idle_color
    }

    /// `#@isekai tab-attention-color <rrggbb>` (Epic Q), `tab_idle_color`'s
    /// counterpart.
    pub(crate) fn tab_attention_color(&self) -> Option<(u8, u8, u8)> {
        self.isekai.tab_attention_color
    }

    /// `{hostname}:{port}` for this destination, using the same
    /// `HostName`/`port` fallback `resolve_isekai_config`'s own
    /// `default_target` uses (destination literal, port 22) — the native
    /// path's SSH TCP target and `HostKeyVerifier` trust-store key.
    pub(crate) fn native_host_port(&self, destination: &str) -> (String, u16) {
        let host = self.openssh.hostname.clone().unwrap_or_else(|| destination.to_string());
        let port = self.openssh.port.unwrap_or(22);
        (host, port)
    }

    /// A canonical string of the connection-relevant `#@isekai` directives,
    /// hashed into the mux channel name (`native/mux/naming.rs`) alongside the
    /// OpenSSH-resolved fields so two tabs whose isekai routing differs (a
    /// different profile, relay set, bootstrap policy, …) never share an
    /// owner.
    ///
    /// This is deliberately the `Debug` rendering of the whole resolved
    /// `IsekaiConfig`: every field is connection-relevant, and folding them
    /// through `Debug` means a *newly added* field is automatically included
    /// rather than silently forgotten (a missed field would be a
    /// wrong-sharing bug). `Debug` output is deterministic within one binary
    /// build, which is all the channel-naming hash needs; if two differing
    /// binary versions ever rendered it differently they would simply compute
    /// different names and not share (over-isolation, always safe — see
    /// `naming.rs`'s "fail-safe direction" note), with the protocol version
    /// handshake as the final backstop.
    pub(crate) fn mux_identity_material(&self) -> String {
        format!("{:?}", self.isekai)
    }
}

/// Resolves `destination` (a bare host, no other `ssh` args) into the same
/// `(WrapperPlan, WrapperResolution)` pair the ordinary connect path
/// builds, for `doctor.rs` to reuse without duplicating the
/// `~/.ssh/config`/`#@isekai` directive parser (`ISEKAI_PIPE_DESIGN.md` §8
/// Epic N).
pub(crate) async fn resolve_profile_for_destination(
    destination: &str,
    extra_isekai_args: Vec<String>,
) -> Result<(WrapperPlan, WrapperResolution)> {
    let mut args = extra_isekai_args;
    args.push(destination.to_string());
    let plan = parse_wrapper(args)?;
    let resolution = resolve_wrapper(&plan).await?;
    Ok((plan, resolution))
}

pub fn should_run_wrapper(args: &[String]) -> bool {
    let Some(first) = args.first().map(String::as_str) else {
        return false;
    };
    !matches!(first, "-h" | "--help" | "help" | "-V" | "--version")
        && !RESERVED_SUBCOMMANDS.contains(&first)
}

/// Opens `--isekai-log-file` (or, absent that, the default always-on verbose
/// log) for this invocation — shared by the Unix entrypoint ([`run`]) and
/// the Windows-native entrypoint (`native::connect::prepare_with_tofu`),
/// which used to each hand-roll an identical if/else-if pair. That
/// duplication had already caused one real regression: the native copy was
/// missing entirely for a while (a real Windows CI failure, see
/// `native::connect::prepare_with_tofu`'s own doc comment for the
/// no-visible-symptom-until-a-test-depended-on-it story) — extracting this
/// structurally prevents that class of regression from recurring, since
/// there is now only one copy to keep in sync.
pub(crate) fn init_logging(plan: &WrapperPlan) -> Result<()> {
    if let Some(log_file) = plan.log_file() {
        crate::log_file::init(log_file)
            .with_context(|| format!("isekai-ssh: failed to open --isekai-log-file at {}", log_file.display()))?;
    } else if let Ok(verbose_log_file) = default_log_file() {
        // Best-effort: verbose bootstrap/diagnostic detail is a nicety, not
        // something that should ever block the connection over a
        // permissions/read-only-filesystem error opening its own log file.
        let _ = crate::log_file::init_verbose(&verbose_log_file);
    }
    Ok(())
}

/// Resolves `resolution` into a [`ConnectionIntent`], auto-bootstrapping a
/// brand-new (never-registered) destination inline when [`should_bootstrap`]
/// allows it — `tofu` decides whether *that* bootstrap's own trust
/// confirmation is interactive ([`TofuConfirmation::AlwaysPrompt`], both
/// entrypoints' first-contact case) or silent
/// ([`TofuConfirmation::Silent`], only used by the native path's detached
/// mux-holder entrypoint, which has no console to confirm on — see
/// `native::connect::prepare_with_tofu`'s doc comment).
///
/// Shared by the Unix entrypoint ([`run`]) and the Windows-native entrypoint
/// (`native::connect::prepare_with_tofu`), which used to hand-roll nearly
/// identical copies of this match — the *only* platform-specific piece
/// (which [`isekai_bootstrap::BootstrapBackend`] actually performs the
/// deploy) is already internal to [`bootstrap_and_register`], so nothing
/// here needs to branch on platform. Unifies the two copies' previously
/// divergent "auto-bootstrap is disabled" message onto the native path's
/// more actionable wording (pointing at the exact `isekai-ssh init` command
/// to run) for both — a small, deliberate UX improvement riding along with
/// this dedup, not a functional change (this arm is reached only when the
/// user has explicitly opted out of auto-bootstrap).
pub(crate) async fn build_intent_or_bootstrap(
    plan: &WrapperPlan,
    resolution: &WrapperResolution,
    tofu: TofuConfirmation,
) -> Result<ConnectionIntent> {
    match build_connection_intent(resolution) {
        Ok(intent) => Ok(intent),
        Err(err) if should_bootstrap(plan, resolution) => {
            if let Err(bootstrap_err) = bootstrap_and_register(plan, resolution, tofu).await {
                print_bootstrap_failure_guidance(&bootstrap_err);
                return Err(bootstrap_err.context(format!("{err}\nisekai-ssh: auto-bootstrap failed")));
            }
            build_connection_intent(resolution).context("isekai-ssh: still not trusted after auto-bootstrap")
        }
        Err(err) => Err(err.context(format!(
            "isekai-ssh: {:?} is not set up yet — run `isekai-ssh init {}` first \
             (auto-bootstrap is disabled: --isekai-no-bootstrap / #@isekai bootstrap-policy never)",
            plan.destination(),
            plan.destination()
        ))),
    }
}

pub async fn run(args: Vec<String>) -> Result<u8> {
    let plan = parse_wrapper(args)?;
    init_logging(&plan)?;
    if plan.isekai.direct {
        return run_openssh_direct(&plan).await;
    }
    let resolution = resolve_wrapper(&plan).await?;
    if !resolution.isekai.enabled {
        return run_openssh_direct(&plan).await;
    }
    if plan.isekai.explain || plan.isekai.dry_run {
        // Unlike the rest of this module's bootstrap-progress chatter,
        // `--isekai-explain`/`--isekai-dry-run` is a diagnostic the user
        // explicitly opted into for this one invocation — it must stay on
        // screen by default, not get silently redirected to the verbose
        // log file alongside everything else.
        log_line!(
            "isekai-ssh: resolved OpenSSH config: {:?}",
            resolution.openssh
        );
        log_line!(
            "isekai-ssh: resolved isekai config: {:?}",
            resolution.isekai
        );
        if plan.isekai.dry_run {
            return Ok(0);
        }
    }
    let intent = build_intent_or_bootstrap(&plan, &resolution, TofuConfirmation::AlwaysPrompt).await?;
    run_ssh_with_connect_failure_recovery(&plan, &resolution, intent).await
}

/// STUN P2P's lightweight reconnect claims a fresh `AttachArbiter` session
/// slot on every attempt; capped well short of the server's
/// `--max-sessions` default (16) so a long run of lightweight retries can
/// never evict another tab's genuinely parked session (ADR round 1, S2).
/// Once exceeded, the loop falls back to one `RebootstrapAndRetry`-style
/// attempt (gated on `should_bootstrap`, unlike the lightweight path
/// itself) rather than lightweight-retrying forever.
const MAX_LIGHTWEIGHT_RETRIES: u32 = 5;

/// Runs `ssh` once against `intent`; if it fails *and* `isekai-pipe connect`
/// left behind a `ConnectOutcome` side-channel file for this exact attempt
/// (`isekai-pipe-core::claim_connect_outcome`, `ISEKAI_PIPE_DESIGN.md` §8
/// Epic N's "always-connects" principle: whatever state the cached
/// deployment is in — stale trust material, or the helper simply being
/// dead/unreachable — `isekai-ssh <destination>` must self-heal rather than
/// requiring the user to notice and run `isekai-ssh doctor --fix`/`init`
/// manually), silently refreshes the trust store (no `[y/N]` prompt —
/// confirmed product decision: this profile was already trusted once, and
/// the underlying SSH re-deploy connection is still gated by the user's own
/// `~/.ssh/known_hosts`, `OpenSshBackend`'s module docs) and retries.
///
/// Epic R PR2 turned this from a single retry into a bounded loop: a
/// `ConnectOutcomeClass::MidSessionDisconnect` signal (the SSH bridge was
/// genuinely live for a while before failing, ADR §2.2.2) now retries
/// lightweight — no re-deploy — using the same `RECONNECT_BUDGET`(24h)/
/// backoff policy `native::mux::run_with_reconnect` already established
/// for Windows mux (`reconnect_backoff`, Q1). `StaleTrust`/`Unreachable`
/// keep the original single-shot `RebootstrapAndRetry` behavior — a
/// pre-handshake failure means the cached deployment itself might be
/// stale/dead, and looping a full re-deploy indefinitely would be both
/// slow and, if the target is genuinely gone, pointless.
///
/// Only reachable *after* `build_connection_intent` already succeeded once
/// in `run()` — a brand-new (never-registered) profile's own, separately
/// prompted bootstrap path is untouched by this function entirely.
/// Live process-level reconnect status line (Epic R PR2, Task 2.13) — the
/// `run_ssh_with_connect_failure_recovery` loop's counterpart to
/// `isekai-pipe/src/resume_loop.rs`'s `format_reconnect_status`/
/// `print_reconnect_status` for its own (session-level QUIC resume)
/// reconnects. Deliberately worded differently ("isekai-ssh" + "attempt
/// {n}" rather than "isekai-pipe connect" + an elapsed/total countdown):
/// this loop restarts the whole `ssh(1)` process rather than resuming one
/// live QUIC connection, and has no fixed total-window countdown to show
/// (`reconnect_backoff::RECONNECT_BUDGET` is 24h, not a useful "X/Y"
/// display). Pure so it's unit-testable without touching stderr, mirroring
/// that module's own split.
///
/// Deliberately **not** an in-place `\r`-redrawn line the way
/// `resume_loop.rs`'s countdown is (round 2 review finding, self-corrected
/// before merge): unlike that module's tight per-second countdown loop
/// inside one continuous process, this function has no way to detect the
/// *moment* a retry actually succeeds — `run_ssh_once`'s `.wait()` only
/// returns once the whole `ssh(1)` session ends (i.e. potentially hours
/// later, when the user types `exit`), so there is no matching "reconnected"
/// event to redraw over this line with. A one-shot, newline-terminated line
/// per attempt avoids needing to clear anything before the reconnected
/// session's own terminal output resumes.
fn format_process_reconnect_status(is_tty: bool, attempt: u32) -> String {
    if is_tty {
        format!("\x1b[0;33misekai-ssh: connection lost, reconnecting... (attempt {attempt})\x1b[0m")
    } else {
        format!("isekai-ssh: connection lost, reconnecting... (attempt {attempt})")
    }
}

/// Prints [`format_process_reconnect_status`] to stderr, one newline-terminated
/// line per attempt via `log_line!` (so `--isekai-log-file` output stays free
/// of ANSI color bytes, same reasoning as `resume_loop.rs`'s own
/// `is_terminal()` gate) — called *before* the backoff wait so the user sees
/// it promptly rather than only once the wait has already elapsed.
fn print_process_reconnect_status(is_tty: bool, attempt: u32) {
    log_line!("{}", format_process_reconnect_status(is_tty, attempt));
}

async fn run_ssh_with_connect_failure_recovery(
    plan: &WrapperPlan,
    resolution: &WrapperResolution,
    intent: ConnectionIntent,
) -> Result<u8> {
    let runtime_dir = default_runtime_dir()?;
    let mut intent = intent;
    let mut attempt: u32 = 0;
    let mut lost_since: Option<tokio::time::Instant> = None;
    let mut lightweight_retries: u32 = 0;
    // Task 2.13: computed once — `--isekai-log-file` redirects stderr away
    // from the terminal for the *ssh child's* stderr (`run_ssh_once`), but
    // this process's own stderr (what `is_terminal()` here actually checks)
    // is unaffected by that; the `!log_file::is_enabled()` half of this
    // check is what keeps `\r`/ANSI bytes out of the log file specifically.
    let is_tty = std::io::stderr().is_terminal() && !crate::log_file::is_enabled();

    loop {
        let attempt_started = tokio::time::Instant::now();
        let (status, intent_id) = run_ssh_once(plan, resolution, &intent, &runtime_dir).await?;
        if status.success() {
            // Deliberately no "reconnected" message here (round 2 review
            // finding, self-corrected before merge): `run_ssh_once` only
            // returns once the whole `ssh(1)` session has *ended*, which for
            // a successful reconnect is whenever the user eventually types
            // `exit` — often much later, and unrelated to the reconnect
            // itself. There is no event at this layer that actually means
            // "the retry just succeeded".
            return Ok(0);
        }
        let exit_code = status.code().unwrap_or(1) as u8;

        let outcome = resolve_claimed_outcome(claim_connect_outcome(&runtime_dir, &intent_id));

        match decide_connect_failure_recovery(outcome.as_ref().map(|o| &o.class), should_bootstrap(plan, resolution)) {
            ConnectFailureRecoveryAction::NoRecoverableSignal => return Ok(exit_code),
            ConnectFailureRecoveryAction::AutoBootstrapDisabled => {
                let outcome = outcome.expect("AutoBootstrapDisabled only returned when a connect-failure signal was found");
                log_auto_bootstrap_disabled(&outcome.class, &resolution.isekai.profile, &outcome.detail);
                return Ok(exit_code);
            }
            ConnectFailureRecoveryAction::RebootstrapAndRetry => {
                let outcome = outcome.expect("RebootstrapAndRetry only returned when a connect-failure signal was found");
                log_rebootstrap_and_retry_decision(&outcome.class, &resolution.isekai.profile, &outcome.detail, "refreshing automatically...");
                return rebootstrap_and_retry_once(plan, resolution, &runtime_dir).await;
            }
            ConnectFailureRecoveryAction::RetryConnectLightweight => {
                // Epic R PR2 (B5): never silently re-run a one-shot remote
                // command — `isekai-ssh host -- ./deploy.sh` interrupted
                // mid-run must not be retried from scratch, since a
                // non-idempotent command could be repeated. Matches the
                // identical guard `native::mux::mod::run_with_reconnect`
                // already applies to its own `OwnerLost` reconnect.
                if plan.remote_command().is_some() {
                    log_line!(
                        "isekai-ssh: connection lost while running a remote command; not auto-retrying \
                         (rerunning it could repeat a non-idempotent action)."
                    );
                    return Ok(exit_code);
                }
                // Reset *before* checking the cap below: a `lightweight_retries`
                // that's already at the cap from an earlier, long-since-stable
                // storm must not immediately trip the cap for what is really
                // the first failure of a brand-new one (round 2 review finding
                // — see `reset_budget_if_stable`'s own docs).
                reconnect_backoff::reset_budget_if_stable(attempt_started, &mut attempt, &mut lost_since, &mut lightweight_retries);
                lightweight_retries += 1;
                if lightweight_retries > MAX_LIGHTWEIGHT_RETRIES {
                    log_line!("isekai-ssh: gave up on {MAX_LIGHTWEIGHT_RETRIES} lightweight reconnect attempts; trying a full re-deploy instead");
                    if !should_bootstrap(plan, resolution) {
                        return Ok(exit_code);
                    }
                    return rebootstrap_and_retry_once(plan, resolution, &runtime_dir).await;
                }
                // Printed *before* the backoff wait below (round 2 review
                // finding: it used to print only after already sleeping out
                // the whole delay, so the terminal stayed silent for up to
                // 10s before the user saw anything). `attempt + 1` is the
                // attempt number `reconnect_backoff_or_give_up` is about to
                // record, since it increments `attempt` itself on `Retry`.
                print_process_reconnect_status(is_tty, attempt + 1);
                match reconnect_backoff::reconnect_backoff_or_give_up(&mut attempt, &mut lost_since).await {
                    reconnect_backoff::ReconnectDecision::Retry => {
                        intent = build_connection_intent(resolution).context("isekai-ssh: could not rebuild the connection intent for a reconnect")?;
                        continue;
                    }
                    reconnect_backoff::ReconnectDecision::GiveUp => return Ok(exit_code),
                }
            }
        }
    }
}

/// The re-deploy-then-retry-once action shared by `RebootstrapAndRetry` and
/// (once its own lightweight retry budget is exhausted)
/// `RetryConnectLightweight`. Structurally at most one extra `ssh`
/// invocation happens here — no loop, no recursion — so this cannot run
/// away even if the retry's own attempt also fails (e.g. a crash-looping
/// helper, or a genuinely unreachable network): whatever this attempt
/// returns is final for *this* invocation, though a subsequent manual
/// `isekai-ssh <destination>` gets its own fresh recovery budget.
async fn rebootstrap_and_retry_once(plan: &WrapperPlan, resolution: &WrapperResolution, runtime_dir: &Path) -> Result<u8> {
    if let Err(bootstrap_err) = bootstrap_and_register(plan, resolution, TofuConfirmation::Silent).await {
        print_bootstrap_failure_guidance(&bootstrap_err);
        return Err(bootstrap_err.context("isekai-ssh: automatic re-bootstrap after a connect failure failed"));
    }
    let intent2 = build_connection_intent(resolution).context("isekai-ssh: still not trusted after automatic re-bootstrap")?;
    let (status2, _) = run_ssh_once(plan, resolution, &intent2, runtime_dir).await?;
    Ok(status2.code().unwrap_or(1) as u8)
}

/// Human-readable lead-in for the `eprintln!`s below, branching on
/// `ConnectOutcomeClass` purely for message accuracy. `StaleTrust`/
/// `Unreachable`/`Unknown` all drive the same [`ConnectFailureRecoveryAction`]
/// family (`RebootstrapAndRetry`/`AutoBootstrapDisabled`); `MidSessionDisconnect`
/// (Epic R PR2) drives `RetryConnectLightweight` instead and in practice
/// never reaches `log_auto_bootstrap_disabled` (see
/// `decide_connect_failure_recovery`), but this `match` still needs an arm
/// for it to stay exhaustive. `pub(crate)` so the Windows-native connect
/// path (`native/connect.rs`) can reuse the exact same message wording for
/// its own mirror of this recovery flow.
pub(crate) fn outcome_summary(class: &isekai_pipe_core::ConnectOutcomeClass) -> &'static str {
    match class {
        isekai_pipe_core::ConnectOutcomeClass::StaleTrust => "cached trust looks stale",
        isekai_pipe_core::ConnectOutcomeClass::Unreachable => "the cached deployment could not be reached",
        isekai_pipe_core::ConnectOutcomeClass::MidSessionDisconnect => "the connection was lost mid-session",
        // Epic R PR1 (R2-M1): deliberately not the `Unreachable` wording —
        // "the cached deployment could not be reached ... run `isekai-ssh
        // init` manually" would misdirect the user for a class this build
        // simply doesn't recognize (a newer `isekai-pipe` reporting a
        // variant this `isekai-ssh` predates).
        isekai_pipe_core::ConnectOutcomeClass::Unknown => "isekai-pipe connect reported an outcome this isekai-ssh build doesn't recognize",
    }
}

/// Logs `ConnectFailureRecoveryAction::AutoBootstrapDisabled`'s explanation
/// — identical wording on both the Unix (`run_ssh_with_connect_failure_recovery`)
/// and Windows-native (`native::connect::drive_connect_recovery`) paths, so
/// this one function is the whole of the dedup for that arm (unlike
/// `RebootstrapAndRetry`'s — see [`log_rebootstrap_and_retry_decision`] —
/// whose trailing clause differs per platform).
pub(crate) fn log_auto_bootstrap_disabled(class: &isekai_pipe_core::ConnectOutcomeClass, profile: &str, detail: &str) {
    // Epic R PR1 code review: `Unknown` must not get the "run `isekai-ssh
    // init` manually" imperative — that command re-deploys/re-trusts the
    // helper, which does nothing for a version-skew schema mismatch between
    // this `isekai-ssh` build and whatever `isekai-pipe` build wrote the
    // outcome file. Every other class genuinely can be fixed that way.
    if matches!(class, isekai_pipe_core::ConnectOutcomeClass::Unknown) {
        log_line!(
            "isekai-ssh: {} for {:?} ({}); auto-bootstrap is disabled \
             (--isekai-no-bootstrap / #@isekai bootstrap-policy never), so no automatic recovery was attempted \
             for this unrecognized outcome.",
            outcome_summary(class), profile, detail
        );
        return;
    }
    log_line!(
        "isekai-ssh: {} for {:?} ({}), but auto-bootstrap is disabled \
         (--isekai-no-bootstrap / #@isekai bootstrap-policy never) — run `isekai-ssh init` manually.",
        outcome_summary(class), profile, detail
    );
}

/// Logs `ConnectFailureRecoveryAction::RebootstrapAndRetry`'s explanation,
/// shared by the same two paths as [`log_auto_bootstrap_disabled`]. Unlike
/// that arm, the trailing clause genuinely differs per platform (Unix:
/// "refreshing automatically..."; native: a longer note that a
/// still-untrusted SSH host key needs a separate confirmation prompt, since
/// its re-deploy dials `RusshBackend` directly rather than through
/// `ssh(1)`) — `retry_note` stays a caller-supplied parameter rather than
/// unified wording, the same "shared classification, caller-owned wording"
/// split [`outcome_summary`] itself already establishes.
pub(crate) fn log_rebootstrap_and_retry_decision(class: &isekai_pipe_core::ConnectOutcomeClass, profile: &str, detail: &str, retry_note: &str) {
    log_line!("isekai-ssh: {} for {:?} ({}); {retry_note}", outcome_summary(class), profile, detail);
}

/// The three ways a failed first `ssh` attempt in
/// [`run_ssh_with_connect_failure_recovery`] can be handled, given whether
/// `isekai-pipe connect` left behind a `ConnectOutcome` side-channel signal
/// (of either class — see that type's docs) and whether auto-bootstrap is
/// currently allowed. Pure decision, no I/O — split out from the
/// surrounding async function so each branch is unit-testable without
/// spawning a real `ssh`/bootstrap process.
///
/// `pub(crate)` so the Windows-native connect path (`native/connect.rs`)
/// drives its own connect-failure recovery through the exact same decision
/// (rather than duplicating the two-condition branch), keeping the
/// "always-connects" policy single-sourced across the Unix and native paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectFailureRecoveryAction {
    /// No connect-failure signal for this attempt — return the exit code
    /// as-is (e.g. the remote shell command itself exited non-zero; that
    /// never touches `isekai-pipe connect`'s own error path at all).
    NoRecoverableSignal,
    /// A signal was found, but auto-bootstrap is disabled
    /// (`--isekai-no-bootstrap` / `#@isekai bootstrap-policy never`) —
    /// return the exit code as-is, with guidance to run `isekai-ssh init`.
    AutoBootstrapDisabled,
    /// A signal was found and auto-bootstrap is allowed — attempt a silent
    /// re-bootstrap and retry `ssh` exactly once more.
    RebootstrapAndRetry,
    /// The connect-time handshake had already succeeded before this attempt
    /// failed (`ConnectOutcomeClass::MidSessionDisconnect`, Epic R PR2) —
    /// retry by re-dialing the already-deployed helper, with no re-deploy
    /// step. Unlike `RebootstrapAndRetry`, this is attempted regardless of
    /// `should_bootstrap`: it never touches SSH-based re-deployment, so
    /// `--isekai-no-bootstrap`'s contract ("don't silently re-deploy over
    /// SSH") isn't violated by trying it (ADR round 1, Q2 — both opus
    /// reviewers agreed on this reading).
    RetryConnectLightweight,
}

/// Turns `claim_connect_outcome`'s `Result` into the `Option` the rest of
/// `run_ssh_with_connect_failure_recovery` works with, degrading a claim
/// failure to "no signal" (Epic R PR1, S4) instead of failing the whole
/// invocation. A deserialize failure here means either a genuinely corrupt
/// outcome file, or a schema shape `ConnectOutcomeClass::Unknown`'s
/// `#[serde(other)]` catch-all doesn't cover (e.g. a missing required field
/// from a much newer/older `isekai-pipe` build) — the two local binaries
/// (`isekai-pipe`, possibly overridden via `--isekai-pipe-path`, and this
/// `isekai-ssh`) are independently invoked and can legitimately be
/// different builds. `Unknown` now covers the common "new class tag" case;
/// this stays defensive for whatever `Unknown` doesn't catch.
///
/// Deliberately treats every `IntentError` variant the same way, including
/// `Io` (e.g. a permissions problem or full disk under the runtime dir) —
/// not just `Json` (an actual deserialize mismatch). This loses the
/// distinction between "a version-skew schema shape" and "a persistent
/// local filesystem fault" (code review finding), but that distinction was
/// already unavailable before this function existed: the original `?`-based
/// code turned *every* `IntentError` variant into the same hard `Err`
/// uniformly, with no finer-grained handling either. `log_line!`'s output
/// (visible on stderr, or in `--isekai-log-file`'s file) is where an `Io`
/// fault should still show up for anyone debugging it, via `IntentError`'s
/// own `Display` impl.
pub(crate) fn resolve_claimed_outcome(
    result: Result<Option<isekai_pipe_core::ConnectOutcome>, isekai_pipe_core::IntentError>,
) -> Option<isekai_pipe_core::ConnectOutcome> {
    match result {
        Ok(outcome) => outcome,
        Err(e) => {
            log_line!("isekai-ssh: could not read a connect-failure signal ({e}); treating this as no signal");
            None
        }
    }
}

/// Epic R PR2 changed this from a plain `bool` to
/// `Option<&ConnectOutcomeClass>` so `MidSessionDisconnect` can route to its
/// own `RetryConnectLightweight` action instead of `RebootstrapAndRetry`.
/// **`Unknown` must fall through to the `Some(_)` arm, not be treated like
/// `None`** — round 1 review (R1-B1) caught an earlier draft doing exactly
/// that, which would have been a regression against
/// `.claude/rules/always-connects.md`: before `ConnectOutcomeClass` existed,
/// `wrapper.rs`'s old bool-based check treated *any* outcome file
/// (`outcome.is_some()`) as a recoverable signal, regardless of which class
/// it carried — `Unknown` existing at all already means "some outcome was
/// recorded", which is the only thing this function used to need to know.
pub(crate) fn decide_connect_failure_recovery(outcome_class: Option<&isekai_pipe_core::ConnectOutcomeClass>, should_bootstrap: bool) -> ConnectFailureRecoveryAction {
    match outcome_class {
        None => ConnectFailureRecoveryAction::NoRecoverableSignal,
        Some(isekai_pipe_core::ConnectOutcomeClass::MidSessionDisconnect) => ConnectFailureRecoveryAction::RetryConnectLightweight,
        Some(_) if !should_bootstrap => ConnectFailureRecoveryAction::AutoBootstrapDisabled,
        Some(_) => ConnectFailureRecoveryAction::RebootstrapAndRetry,
    }
}

/// Writes `intent`, execs `ssh` with the `isekai-pipe connect` `ProxyCommand`
/// injected, and waits for it to exit. All three of `ssh`'s stdio streams
/// are inherited (interactive TTY passthrough) — `.status()` (not
/// `.output()`) still blocks until the whole process tree, including the
/// `ProxyCommand` grandchild, has exited, which is what makes inspecting a
/// side-channel file in `runtime_dir` immediately afterward both correct
/// and zero-cost to this stdio wiring (`run_ssh_with_connect_failure_recovery`).
/// Prepares the opportunistic `#@isekai ctl-socket` `-R` forward (if enabled
/// and the session is interactive) and appends the ssh(1) args in the right
/// order: any `-R` option *before* the destination, then (if `--isekai-tty`
/// needs one) a forced `-t`, then the destination and its args, then the
/// `export ISEKAI_CTL_SOCK=...; exec <target>` remote command *after* it —
/// composing ctl-socket and `--isekai-tty` into that one remote command
/// (`ctl_forward::build_login_shell_command`) rather than one silently
/// disabling the other. Unix-only — this is the `ssh(1)` ProxyCommand path;
/// the Windows-native path handles both on its own `russh` handle
/// (`native/mux/ctl_forward.rs`).
///
/// Returns the [`crate::ctl_forward::CtlForward`] it prepared (if any) so
/// the caller (`run_ssh_once`) can keep its background listener task alive
/// for exactly this attempt's `ssh` child and tear it down right after —
/// Epic R PR2, Task 2.10: this function itself must not drop it, since
/// dropping a plain struct does nothing to the already-detached listener
/// task `spawn_ctl_listener` started (see `CtlForward::listener_task`'s
/// docs for why that matters now that `run_ssh_once` can run many times per
/// invocation).
#[cfg(unix)]
async fn apply_ctl_socket_forward(
    command: &mut Command,
    plan: &WrapperPlan,
    resolution: &WrapperResolution,
    runtime_dir: &Path,
) -> Option<crate::ctl_forward::CtlForward> {
    let ctl_forward = if crate::ctl_forward::should_attempt_ctl_forward(
        resolution.isekai.ctl_socket_enabled,
        plan.ssh_args.len(),
        plan.destination_index,
    ) {
        match crate::ctl_forward::prepare_ctl_forward(runtime_dir) {
            Ok(mut forward) => {
                crate::ctl_forward::spawn_ctl_listener(&mut forward, resolution.profile().to_string()).await;
                Some(forward)
            }
            Err(e) => {
                // Opportunistic feature (`ISEKAI_PIPE_DESIGN.md` Epic M):
                // never fail the connection over this, just skip it.
                log_line_verbose!("isekai-ssh: ctl-socket forward unavailable, continuing without it: {e:#}");
                None
            }
        }
    } else {
        None
    };
    // `--isekai-tty` is silently ignored (not an error) when the caller
    // already gave an explicit trailing remote command — same opportunistic
    // convention as ctl-socket's own `should_attempt_ctl_forward` gate,
    // `resolved_tty_selection`'s doc comment.
    let tty_exec = if plan.remote_command().is_none() {
        crate::tty_attach::resolve_exec_command(resolved_tty_selection(plan, resolution).as_ref(), resolution.profile())
    } else {
        None
    };

    if let Some(forward) = &ctl_forward {
        // `-R` is an ssh(1) option, so it must precede the destination
        // (`plan.ssh_args`'s last element, per `should_attempt_ctl_forward`).
        command.args(crate::ctl_forward::forward_option_args(forward));
    }
    if tty_exec.is_some() && plan.request_tty == RequestTty::Auto {
        // Real `ssh(1)` only allocates a PTY for a remote command when `-t`
        // is given — `isekai-pipe tty attach` needs one to relay through
        // (`login_tty` on the daemon side). An explicit `-t`/`-tt`/`-T` the
        // caller already gave (anything but `Auto`) is left untouched.
        command.arg("-t");
    }
    command.args(&plan.ssh_args);
    if ctl_forward.is_some() || tty_exec.is_some() {
        // Anything appended after the destination is the remote command, not
        // an option, to ssh(1).
        command.arg(crate::ctl_forward::build_login_shell_command(
            ctl_forward.as_ref().map(|forward| forward.remote_path.as_str()),
            resolution.isekai.tab_idle_color,
            resolution.isekai.tab_attention_color,
            tty_exec.as_deref(),
        ));
    }
    ctl_forward
}

async fn run_ssh_once(
    plan: &WrapperPlan,
    resolution: &WrapperResolution,
    intent: &ConnectionIntent,
    runtime_dir: &Path,
) -> Result<(std::process::ExitStatus, String)> {
    write_connection_intent(runtime_dir, intent)?;
    let proxy_command = proxy_command(&plan.pipe_path, &resolution.isekai.profile, &plan.openssh_path);

    let mut command = Command::new(&plan.openssh_path);
    command
        .env("ISEKAI_INTENT_ID", &intent.intent_id)
        .env("ISEKAI_PIPE_RUNTIME_DIR", runtime_dir)
        .arg("-o")
        .arg(format!("ProxyCommand={proxy_command}"));
    if plan.isekai.log_file.is_none() {
        // Only when the user hasn't asked for `--isekai-log-file`: point the
        // `isekai-pipe connect` ProxyCommand grandchild's own `env_logger` at
        // the same default verbose log this process just opened
        // (`init_verbose` in `run()`), so its diagnostic noise never reaches
        // the live terminal by default. When `--isekai-log-file` *is* given,
        // this is deliberately left unset — `isekai-pipe connect` keeps
        // targeting `stderr`, which the `is_enabled()` branch below already
        // pipes into that one user-chosen file, preserving today's
        // "everything in one place" contract unchanged.
        if let Ok(verbose_log_file) = default_log_file() {
            command.env("ISEKAI_PIPE_LOG_FILE", verbose_log_file);
        }
    }
    // The `#@isekai ctl-socket` `-R` forward needs a real local UNIX-socket
    // listener bound before the destination arg, so it is Unix-only. The
    // Windows-native path never reaches this function — it's `russh`-based and
    // forwards streamlocal in-process (see `ctl_forward.rs`'s module docs and
    // `native/mux/ctl_forward.rs`).
    #[cfg(unix)]
    let ctl_forward = apply_ctl_socket_forward(&mut command, plan, resolution, runtime_dir).await;
    #[cfg(not(unix))]
    command.args(&plan.ssh_args);
    // stdin/stdout always stay `Stdio::inherit()`ed — piping either would
    // break `ssh(1)`'s own `isatty()`-based PTY/interactive-terminal
    // behavior (`log_file.rs`'s module docs). stderr is the one stream this
    // feature can safely redirect: it carries only diagnostic output (this
    // process's own, and `isekai-pipe connect`'s `env_logger` lines, both
    // `log_file.rs`'s actual targets), never the interactive session's own
    // content.
    command.stdin(Stdio::inherit()).stdout(Stdio::inherit());
    if crate::log_file::is_enabled() {
        command.stderr(Stdio::piped());
    } else {
        command.stderr(Stdio::inherit());
    }

    let spawn_result = command.spawn();
    // Epic R PR2, Task 2.10: tear down this attempt's ctl-socket listener
    // (if any) on every exit from this function — spawn failure, wait
    // failure, or a normal exit — not just the happy path, so a long
    // `MidSessionDisconnect` lightweight-retry loop (`run_ssh_once` can now
    // run many times per invocation) never accumulates one leaked listener
    // task + local socket file per failed/completed attempt.
    #[cfg(unix)]
    let teardown = |result| {
        if let Some(forward) = ctl_forward {
            forward.teardown();
        }
        result
    };
    #[cfg(not(unix))]
    let teardown = |result| result;

    let mut child = match spawn_result {
        Ok(child) => child,
        Err(e) => {
            return teardown(Err(anyhow!(
                "isekai-ssh: failed to execute OpenSSH at {}: {e}",
                plan.openssh_path.display()
            )));
        }
    };
    let stderr_redirect = child.stderr.take().map(|stderr| tokio::spawn(crate::log_file::redirect_child_stderr(stderr)));

    let status = match child.wait().await {
        Ok(status) => status,
        Err(e) => {
            return teardown(Err(anyhow!(
                "isekai-ssh: failed while waiting for OpenSSH at {}: {e}",
                plan.openssh_path.display()
            )));
        }
    };
    // ssh(1) exiting closes its stderr, which ends `redirect_child_stderr`'s
    // read loop on its own — this just makes sure that last batch of bytes
    // has actually landed before returning (so a caller inspecting the log
    // file right after doesn't race it).
    if let Some(handle) = stderr_redirect {
        let _ = handle.await;
    }
    teardown(Ok((status, intent.intent_id.clone())))
}

async fn run_openssh_direct(plan: &WrapperPlan) -> Result<u8> {
    let status = Command::new(&plan.openssh_path)
        .args(&plan.ssh_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .map_err(|e| {
            anyhow!(
                "isekai-ssh: failed to execute OpenSSH at {}: {e}",
                plan.openssh_path.display()
            )
        })?;
    Ok(status.code().unwrap_or(1) as u8)
}

pub(crate) fn build_connection_intent(resolution: &WrapperResolution) -> Result<ConnectionIntent> {
    let key = isekai_trust::normalize_host_port(&resolution.isekai.profile).map_err(|e| {
        anyhow!(
            "isekai-ssh: invalid profile {:?}: {e}",
            resolution.isekai.profile
        )
    })?;
    let profiles_dir = default_profiles_dir()?;
    let profile = load_persistent_profile(&profiles_dir, &key)?.ok_or_else(|| {
        anyhow!(
            "isekai-ssh: {:?} is not a trusted host yet (looked up as {:?} in {})",
            resolution.isekai.profile,
            key,
            profiles_dir.display()
        )
    })?;
    let (transport, cross_family_fallback) = select_transport(&profile, &resolution.isekai.stun_servers).ok_or_else(|| {
        anyhow!(
            "isekai-ssh: profile {:?} has no cached relay transport (candidate-source profiles are not supported by the wrapper yet)",
            key
        )
    })?;

    let mut intent = ConnectionIntent::new(
        resolution.isekai.profile.clone(),
        primary_service(&resolution.isekai).name().as_str(),
        profile.server_identity.clone(),
        transport,
        BootstrapProvenance::TrustStore { key },
    );
    intent.cross_family_fallback = cross_family_fallback;
    intent.link_endpoints = resolution.isekai.link_endpoints.clone();
    intent.rendezvous = resolution.isekai.rendezvous.clone();
    intent.stun_servers = resolution.isekai.stun_servers.clone();
    intent.relay_endpoints = resolution.isekai.relay_endpoints.clone();
    intent.candidate_race_delay_ms = resolution.isekai.candidate_race_delay_ms;
    intent.relay_delay_ms = resolution.isekai.relay_delay_ms;
    intent.resume_grace_secs = resolution.isekai.resume_grace_secs;
    intent.local_bind_port_range = resolution.isekai.local_bind_port_range;
    Ok(intent)
}

/// Chooses the primary transport for a trusted profile (`ISEKAI_PIPE_DESIGN.md`
/// §8 Epic G). Prefers `IntentTransport::StunP2p` over the
/// `direct-by-bootstrap-host`/relay-shaped `legacy_relay_transport` when all
/// of the following hold — the same evidence `#20b`'s bootstrap-time STUN
/// exchange already collects, just never previously consulted here:
///
/// - `#@isekai stun` resolved to at least one configured STUN server
///   (`configured_stun_servers` non-empty) — an operator opt-in, not an
///   always-on default.
/// - the profile has a `cached_stun_observed_addr` — bootstrap actually got
///   a `server-reflexive` candidate back from the deployed helper.
///
/// Deliberately does *not race* STUN against the direct/relay candidate —
/// `ConnectionIntent::transport` is still a single primary, not an ordered
/// concurrent candidate list, so honestly expressing "try both at once" isn't
/// possible without a bigger schema change than this crate has taken on. It
/// *does* return a sequential cross-family fallback (`ConnectionIntent::
/// cross_family_fallback`, `ISEKAI_PIPE_DESIGN.md` §8 Epic I's
/// `I-route-scheduler`, ordered-fallback half only) — when STUN evidence
/// wins as primary, the same `legacy_relay_transport` this function would
/// otherwise have returned outright becomes the fallback instead of being
/// discarded, so `isekai-pipe connect` can fall back to it if the STUN P2P
/// attempt fails entirely.
fn select_transport(profile: &PersistentProfile, configured_stun_servers: &[String]) -> Option<(IntentTransport, Option<IntentTransport>)> {
    if let (Some(peer_addr), Some(stun_server), Some(legacy)) =
        (&profile.cached_stun_observed_addr, configured_stun_servers.first(), &profile.legacy_relay_transport)
    {
        let primary = IntentTransport::StunP2p {
            stun_server: stun_server.clone(),
            peer_addr: peer_addr.clone(),
            server_name: "isekai-helper".to_string(),
            session_secret_b64: legacy.session_secret_b64.clone(),
        };
        return Some((primary, profile.to_legacy_relay_transport()));
    }
    profile.to_legacy_relay_transport().map(|relay| (relay, None))
}

/// Looks for a [`BootstrapFailure`] attached anywhere in `err`'s context
/// chain (`bootstrap_and_register`'s classifiable failure sites attach one
/// via `anyhow::Error::context`, per `ISEKAI_PIPE_DESIGN.md` §8 Epic I) and,
/// if found, prints an actionable next step to stderr — `isekai-ssh login`,
/// `isekai-ssh init`, or "this looked transient, retrying may help" —
/// instead of leaving the user to interpret a raw `ssh(1)`/QUIC error.
/// Unclassified failures (no `BootstrapFailure` anywhere in the chain, e.g.
/// a local `--via`/argument-validation error) print nothing extra here; the
/// underlying error's own `Display` (propagated by the caller) is
/// self-explanatory for those.
///
/// Deliberately calls `anyhow::Error::downcast_ref` directly on the
/// top-level `err` rather than walking `err.chain()` and downcast-ing each
/// `&dyn Error` frame: anyhow's context/downcast pairing (`Error::context`
/// docs, "Effect on downcasting") is implemented as a custom vtable lookup
/// on the top-level `anyhow::Error` that finds a `C` at *any* depth,
/// including inside a `ContextError<C, E>` a plain `dyn
/// Error::downcast_ref` on a chain frame cannot see (that only matches a
/// frame whose *concrete* type is exactly `C`, which `ContextError` never is).
pub(crate) fn print_bootstrap_failure_guidance(err: &anyhow::Error) {
    let Some(failure) = err.downcast_ref::<BootstrapFailure>() else {
        return;
    };
    if failure.should_redirect_to_login() {
        log_line!("isekai-ssh: {failure} — run `isekai-ssh login` and try again.");
    } else if failure.should_redirect_to_init() {
        log_line!("isekai-ssh: {failure} — run `isekai-ssh init` to set up trust/credentials for this host.");
    } else if failure.may_retry() {
        log_line!("isekai-ssh: {failure} — this looks transient; retrying may help.");
    }
}

/// Whether `bootstrap_and_register` shows the interactive `[y/N]` TOFU
/// prompt before registering. `AlwaysPrompt` is used for a genuinely new
/// (never-before-registered) profile — this requirement is fixed and never
/// changes. `Silent` is used only by `run_ssh_with_connect_failure_recovery`'s
/// automatic re-bootstrap of an *already-trusted* profile whose cached
/// session_secret/cert pin just went stale (confirmed product decision:
/// the redeploy SSH connection is still gated by the user's own
/// `~/.ssh/known_hosts`, so this is a refresh of ephemeral material for a
/// host already trusted once, not a new trust decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TofuConfirmation {
    AlwaysPrompt,
    Silent,
}

/// Deploys `isekai-helper` to the highest-priority bootstrap candidate and,
/// depending on `confirmation`, either after an explicit `[y/N]`
/// confirmation or silently, registers it in the trust store
/// `build_connection_intent` reads from. Mirrors `init.rs`'s
/// deploy-then-confirm-then-register flow, but triggered automatically by
/// `run()` on a trust-store miss (or by `run_ssh_with_connect_failure_recovery`
/// on a detected stale-trust signal) instead of via the standalone `init`
/// subcommand.
///
/// Launch mode is a fixed, evidence-gated choice, no racing (`ISEKAI_PIPE_DESIGN.md`
/// §8 Epic H, matching Epic G's `select_transport` precedent for the same
/// "`ConnectionIntent`/deploy step can't honestly express a multi-way race"
/// reason): `#@isekai bootstrap-relay addr=... sni=...` present → always
/// `LaunchSpec::Relay` (JWT sourced from `isekai-ssh login`'s saved token,
/// fail closed if none — no `LaunchSpec::Direct` attempt at all); absent →
/// `LaunchSpec::Direct` (`direct-by-bootstrap-host`, no relay, no STUN launch
/// mode). `candidate.via` may chain through any number of hops
/// (`ISEKAI_PIPE_DESIGN.md` §8 Epic K) — validated with the same
/// `isekai_bootstrap_plan::validate_jump_chain` cycle/hop-count
/// checks `init.rs` uses, then passed to `OpenSshBackend::install_and_start`
/// as a single `ssh(1)` `-J host1,host2,...` invocation, not nested `ssh`
/// executions per hop.
/// Resolves `#@isekai stun` directives (collected as plain strings without
/// socket-address validation — `resolve_isekai_config`'s `append_args` just
/// accumulates whatever text follows the directive, `#20b`) into
/// `SocketAddr`s to pass to `install_and_start`. A malformed entry is
/// skipped with a warning rather than failing the whole auto-bootstrap over
/// one bad directive.
///
/// Well-known public STUN servers (e.g. Google's `stun.l.google.com`) are
/// conventionally referenced by hostname, not a literal IP — `.parse::
/// <SocketAddr>()` alone rejects those as "malformed" even though they're
/// exactly the kind of entry users are most likely to configure. Falls back
/// to DNS resolution (`tokio::net::lookup_host`, which — like `SocketAddr`'s
/// own `FromStr` — requires the `host:port` form) before giving up on an
/// entry.
async fn resolve_stun_servers(entries: &[String]) -> Vec<SocketAddr> {
    let mut resolved = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Ok(addr) = entry.parse::<SocketAddr>() {
            resolved.push(addr);
            continue;
        }
        match tokio::time::timeout(STUN_DNS_LOOKUP_TIMEOUT, tokio::net::lookup_host(entry.as_str())).await {
            Ok(Ok(mut addrs)) => match addrs.next() {
                Some(addr) => resolved.push(addr),
                None => log_line_verbose!("isekai-ssh: ignoring #@isekai stun entry {entry:?}: DNS lookup returned no addresses"),
            },
            Ok(Err(e)) => log_line_verbose!("isekai-ssh: ignoring malformed #@isekai stun entry {entry:?}: {e}"),
            Err(_) => log_line_verbose!(
                "isekai-ssh: ignoring #@isekai stun entry {entry:?}: DNS lookup timed out after {STUN_DNS_LOOKUP_TIMEOUT:?}"
            ),
        }
    }
    resolved
}

pub(crate) async fn bootstrap_and_register(plan: &WrapperPlan, resolution: &WrapperResolution, confirmation: TofuConfirmation) -> Result<()> {
    let candidate = resolution
        .isekai
        .bootstrap_candidates
        .iter()
        .max_by_key(|candidate| candidate.priority)
        .ok_or_else(|| anyhow!("no bootstrap candidates were resolved"))?;

    let (host, port) = candidate
        .target
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("bootstrap candidate target {:?} is not host:port", candidate.target))?;
    let port: u16 = port
        .parse()
        .with_context(|| format!("bootstrap candidate target {:?} has an invalid port", candidate.target))?;
    // Prefer the original alias (`candidate.alias`) over the `ssh -G`-resolved
    // `host` as the destination handed to the `ssh(1)` subprocess: passing
    // the resolved `HostName` instead would no longer match the user's own
    // `Host <alias>` block in `~/.ssh/config`, silently dropping
    // `IdentityFile`/`ProxyCommand`/etc. (see `BootstrapCandidate::alias`'s
    // docs). `user` is threaded through the same way, since it's otherwise
    // resolved via `ssh -G` and then never consulted again.
    let target = match &candidate.alias {
        Some(alias) => HostSpec::new(alias.clone()),
        None => HostSpec::new(host),
    }
    .with_port(port);
    let target = match &resolution.openssh.user {
        Some(user) => target.with_user(user.clone()),
        None => target,
    };

    let via: Vec<JumpSpec> = candidate
        .via
        .iter()
        .map(|hop| {
            let (via_host, via_port, via_user) =
                isekai_trust::split_user_host_port(hop).with_context(|| format!("invalid --via hop {hop:?}"))?;
            let mut spec = JumpSpec::new(via_host);
            if let Some(port) = via_port {
                spec = spec.with_port(port);
            }
            if let Some(user) = via_user {
                spec = spec.with_user(user);
            }
            Ok(spec)
        })
        .collect::<Result<_>>()?;
    isekai_bootstrap_plan::validate_jump_chain(&target, &via)
        .with_context(|| format!("invalid --via chain {:?}", candidate.via))?;

    // `plan.openssh_path` (defaults to bare `"ssh"`, PATH-resolved; overridable
    // via `--isekai-ssh-path`) must be the same `ssh(1)` this backend's
    // `uname -m` probe/deploy dial use as the one `run_ssh_once`/`resolve_openssh_effective_config`
    // use for the final connection — `OpenSshBackend::new()`'s own default is
    // also bare `"ssh"`, so this was previously silently consistent only by
    // coincidence (whenever `--isekai-ssh-path` wasn't passed). On Windows
    // this matters concretely: a `.cmd`/`.bat` shim on `%PATH%` is *not*
    // found by `Command::new("ssh")`'s bare-name resolution (only `.exe` is
    // implicit), so an explicit path is the only way either call site can
    // ever reach it.
    let backend = crate::native::bootstrap_backend::default_bootstrap_backend(
        Some(&plan.openssh_path),
        matches!(confirmation, TofuConfirmation::Silent),
    )?;
    let helper_binary_was_explicit = plan.isekai.helper_binary.is_some();
    let helper_binary = crate::helper_download::resolve_helper_binary(
        plan.isekai.helper_binary.as_deref(),
        backend.as_ref(),
        &target,
        &via,
        &crate::helper_download::ReleaseSource { repo: plan.isekai.helper_release_repo.clone(), tag: plan.isekai.helper_release_tag.clone() },
    )
    .await
    .map_err(|e| {
        let e = if helper_binary_was_explicit {
            e
        } else {
            e.context(
                "no --isekai-helper-binary given (or `isekai-ssh init` was never run for this host) and \
                 auto-download failed; auto-bootstrap needs a local isekai-helper binary to upload",
            )
        };
        e.context(BootstrapFailure::RemoteBinaryMissing)
    })?;
    let helper_sha256 = isekai_trust::hex_sha256(&helper_binary);

    let stun_servers = resolve_stun_servers(&resolution.isekai.stun_servers).await;

    let launch = match &resolution.isekai.bootstrap_relay {
        Some(relay_target) => {
            let relay_jwt = isekai_auth::FileTokenProvider::from_default_path()
                .and_then(|provider| provider.get_relay_jwt())
                .map_err(|e| {
                    anyhow::Error::new(e)
                        .context("failed to load a relay token from `isekai-ssh login` — run `isekai-ssh login` first")
                        .context(BootstrapFailure::TokenExpired)
                })?;
            LaunchSpec::Relay(RelayLaunchSpec {
                relay_addr: relay_target.relay_addr,
                relay_sni: relay_target.relay_sni.clone(),
                relay_jwt,
                relay_transport: relay_target.relay_transport,
                idle_lifetime_secs: DEFAULT_IDLE_LIFETIME_SECS,
                remote_log_level: resolution.isekai.remote_log_level.clone(),
                // Same resolved value `intent.resume_grace_secs` uses below —
                // keeping the remote helper's own `--resume-window` in sync
                // with what this client will actually request is the fix for
                // the bug where a client-only `#@isekai resume-grace` bump
                // was silently clamped back down to the server's unrelated
                // default.
                resume_window_secs: resolution.isekai.resume_grace_secs,
            })
        }
        None => LaunchSpec::Direct {
            idle_lifetime_secs: DEFAULT_IDLE_LIFETIME_SECS,
            remote_log_level: resolution.isekai.remote_log_level.clone(),
            remote_bind_port_range: resolution.isekai.remote_bind_port_range,
            resume_window_secs: resolution.isekai.resume_grace_secs,
        },
    };

    match confirmation {
        TofuConfirmation::AlwaysPrompt => {
            log_line_verbose!("isekai-ssh: {:?} is not trusted yet; deploying isekai-helper to {}...", resolution.isekai.profile, candidate.target);
        }
        TofuConfirmation::Silent => {
            log_line_verbose!(
                "isekai-ssh: cached trust for {:?} looks stale; redeploying isekai-helper to {}...",
                resolution.isekai.profile,
                candidate.target
            );
        }
    }
    let report = backend
        .install_and_start(&target, &via, &helper_binary, &launch, resolution.isekai.remote_path.as_deref(), &stun_servers)
        .await
        .map_err(|e| {
            let failure = classify_bootstrap_error(&e);
            let err = anyhow::Error::new(e);
            match failure {
                Some(f) => err.context(f),
                None => err,
            }
        })
        .with_context(|| format!("failed to deploy/start isekai-helper on {:?}", candidate.target))?;
    let handshake = &report.handshake;
    let identity = handshake.cert_sha256().to_string();

    // Direct launch: dial the same bootstrap host at the port the helper
    // reports (`direct-by-bootstrap-host`). Relay launch: dial the relay's
    // own public address, falling back to the configured `relay_addr` if
    // the handshake didn't report one (mirrors `init.rs`'s identical
    // fallback for the same reason — a test double that never populated
    // the field; a real deployment's `--relay` tunnel always sets it).
    let cached_relay_addr = match &resolution.isekai.bootstrap_relay {
        Some(relay_target) => {
            handshake.relay_public_addr().map(str::to_string).unwrap_or_else(|| relay_target.relay_addr.to_string())
        }
        None => {
            let direct_port = handshake
                .direct_by_bootstrap_host_port()
                .ok_or_else(|| anyhow!("isekai-helper did not advertise a direct-by-bootstrap-host candidate"))?;
            format!("{host}:{direct_port}")
        }
    };

    log_line_verbose!();
    log_line_verbose!("Host:            {}", candidate.target);
    if let Some(relay_target) = &resolution.isekai.bootstrap_relay {
        log_line_verbose!("Relay:           {}", relay_target.relay_addr);
    }
    log_line_verbose!("Helper identity: {identity}");
    log_line_verbose!("Binary sha256:   {helper_sha256}");
    log_line_verbose!();

    match confirmation {
        TofuConfirmation::AlwaysPrompt => {
            eprint!(
                "Trust this isekai-helper and register it for {:?}? [y/N] ",
                resolution.isekai.profile
            );
            std::io::stderr().flush().ok();

            let mut reader = BufReader::new(tokio::io::stdin());
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .context("failed to read confirmation from stdin")?;
            if !matches!(line.trim(), "y" | "Y") {
                return Err(anyhow!("aborted — user declined the trust confirmation").context(BootstrapFailure::HostKeyRejected));
            }
        }
        TofuConfirmation::Silent => {
            log_line_verbose!(
                "isekai-ssh: refreshing trust for {:?} automatically (already trusted; cached session material \
                 just went stale) — no confirmation needed.",
                resolution.isekai.profile
            );
        }
    }

    let (key, path) = register_helper_trust(
        &resolution.isekai.profile,
        &candidate.via,
        handshake,
        helper_sha256,
        "unknown".to_string(),
        None,
        cached_relay_addr,
    )
    .map_err(|e| e.context(BootstrapFailure::PersistenceFailed(format!("failed to register {:?}", resolution.isekai.profile))))?;
    log_line_verbose!("Registered {key:?} in {}", path.display());
    Ok(())
}

/// Writes a `HelperTrust`-backed `PersistentProfile` for `profile` (the
/// register-flow tail shared by `isekai-ssh init`'s `init::run` and this
/// module's own auto-bootstrap path above) — the same 13-field `HelperTrust`
/// literal, `PersistentProfile::migrate_legacy_helper_trust` +
/// `write_persistent_profile` sequence both used to hand-roll almost
/// verbatim. Returns the normalized trust-store key and the path
/// `write_persistent_profile` wrote to; the caller decides how to report
/// both (`init::run` prints to stdout unconditionally since it's always
/// interactive, this module logs via `log_line_verbose!` since it's the
/// opportunistic auto-bootstrap path — different enough UX that unifying it
/// too would fight the whole point of each caller's own output convention).
///
/// `cached_relay_addr` is a caller-computed parameter rather than derived
/// here: `init::run` always deploys via a relay (so it's always
/// `handshake.relay_public_addr().unwrap_or(<its own --relay-addr>)`), while
/// `bootstrap_and_register` above also handles a *direct* (non-relay)
/// launch (falling back to `handshake.direct_by_bootstrap_host_port()`
/// instead) — genuinely different logic per caller, not further
/// duplication worth unifying.
///
/// Deliberately returns a plain `anyhow::Result`, not the
/// `BootstrapFailure`-classified one `bootstrap_and_register` used to wrap
/// only around the final `write_persistent_profile` step: `init::run` has no
/// use for that classification (it never auto-retries — see
/// `.claude/rules/always-connects.md`), and folding the two harmless,
/// already-validated-earlier `default_profiles_dir`/`normalize_host_port`
/// steps into the same `BootstrapFailure::PersistenceFailed` classification
/// at the call site below is behavior-neutral either way (neither
/// `should_redirect_to_login`/`should_redirect_to_init`/`may_retry` treats
/// `PersistenceFailed` differently from an unclassified error — see
/// `isekai-bootstrap-plan::BootstrapFailure`'s own doc comments).
pub(crate) fn register_helper_trust(
    profile: &str,
    candidate_via: &[String],
    handshake: &isekai_protocol::HandshakeJson,
    helper_sha256: String,
    version: String,
    channel: Option<String>,
    cached_relay_addr: String,
) -> Result<(String, std::path::PathBuf)> {
    let profiles_dir =
        default_profiles_dir().context("could not determine the profiles directory (is $HOME set?)")?;
    let key = isekai_trust::normalize_host_port(profile).with_context(|| format!("invalid profile {profile:?}"))?;
    let now = isekai_trust::now_rfc3339();
    let identity = handshake.cert_sha256().to_string();
    let trust = HelperTrust {
        identity_pubkey: identity.clone(),
        trusted_helper_sha256: helper_sha256,
        trusted_helper_version: version,
        update_policy: UpdatePolicy::ExactDigestOnly,
        release_channel: channel,
        last_via: (!candidate_via.is_empty()).then(|| candidate_via.join(",")),
        trusted_at: now.clone(),
        last_seen_at: now,
        cached_relay_addr,
        cached_cert_sha256: identity,
        cached_session_secret: handshake.session_secret.clone(),
        cached_stun_observed_addr: handshake.stun_observed_addr().map(str::to_string),
    };
    let stored = PersistentProfile::migrate_legacy_helper_trust(&key, &trust);
    let path = write_persistent_profile(&profiles_dir, &stored)
        .with_context(|| format!("failed to write profile to {}", profiles_dir.display()))?;
    Ok((key, path))
}

fn primary_service(config: &IsekaiConfig) -> &ServiceSpec {
    config
        .services
        .iter()
        .find(|service| service.name().as_str() == "ssh")
        .or_else(|| config.services.first())
        .expect("IsekaiConfig always has at least one service")
}

pub(crate) fn should_bootstrap(plan: &WrapperPlan, resolution: &WrapperResolution) -> bool {
    if plan.isekai.no_bootstrap
        || matches!(resolution.isekai.bootstrap_policy, BootstrapPolicy::Never)
    {
        return false;
    }
    plan.isekai.bootstrap
        || matches!(
            resolution.isekai.bootstrap_policy,
            BootstrapPolicy::Always | BootstrapPolicy::Auto
        )
}

pub(crate) fn parse_wrapper(args: Vec<String>) -> Result<WrapperPlan> {
    let mut openssh_path = PathBuf::from("ssh");
    let mut pipe_path = default_pipe_path();
    let mut ssh_args = Vec::new();
    let mut isekai = WrapperIsekaiOptions::default();
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--isekai-bootstrap" => isekai.bootstrap = true,
            "--isekai-no-bootstrap" => isekai.no_bootstrap = true,
            "--isekai-direct" => isekai.direct = true,
            "--isekai-explain" => isekai.explain = true,
            "--isekai-dry-run" => isekai.dry_run = true,
            "--isekai-ssh-path" => {
                openssh_path = PathBuf::from(next_value(&mut iter, "--isekai-ssh-path")?);
            }
            "--isekai-pipe-path" => {
                pipe_path = PathBuf::from(next_value(&mut iter, "--isekai-pipe-path")?);
            }
            "--isekai-helper-binary" => {
                isekai.helper_binary =
                    Some(PathBuf::from(next_value(&mut iter, "--isekai-helper-binary")?));
            }
            "--isekai-helper-release-repo" => {
                isekai.helper_release_repo = next_value(&mut iter, "--isekai-helper-release-repo")?;
            }
            "--isekai-helper-release-tag" => {
                isekai.helper_release_tag = Some(next_value(&mut iter, "--isekai-helper-release-tag")?);
            }
            "--isekai-log-file" => {
                isekai.log_file = Some(PathBuf::from(next_value(&mut iter, "--isekai-log-file")?));
            }
            "--isekai-tty" => isekai.tty = Some(TtySelection::Auto),
            _ if arg.starts_with("--isekai-tty=") => {
                let name = arg["--isekai-tty=".len()..].to_string();
                if name.is_empty() {
                    return Err(anyhow!(
                        "isekai-ssh: --isekai-tty=<name> requires a non-empty session name (use bare --isekai-tty to derive one automatically)"
                    ));
                }
                isekai.tty = Some(TtySelection::Named(name));
            }
            _ => ssh_args.push(arg),
        }
    }

    if isekai.bootstrap && isekai.no_bootstrap {
        return Err(anyhow!(
            "isekai-ssh: --isekai-bootstrap conflicts with --isekai-no-bootstrap"
        ));
    }
    let destination_index = find_destination_index(&ssh_args)
        .ok_or_else(|| anyhow!("isekai-ssh: destination is required"))?;
    let destination = ssh_args[destination_index].clone();
    let request_tty = extract_request_tty(&ssh_args, destination_index);
    let (destination_user, destination_host) = split_user_host(&destination);

    Ok(WrapperPlan {
        openssh_path,
        pipe_path,
        destination,
        destination_host,
        destination_user,
        destination_index,
        ssh_args,
        isekai,
        request_tty,
    })
}

/// Extracts the `-t`/`-T`/`-tt` flags from ssh args (before the destination)
/// and returns the caller's PTY-request intent.
pub(crate) fn extract_request_tty(ssh_args: &[String], destination_index: usize) -> RequestTty {
    let mut tty = RequestTty::Auto;
    let mut i = 0;
    while i < destination_index {
        match ssh_args[i].as_str() {
            "-t" => {
                tty = if tty == RequestTty::Yes { RequestTty::Force } else { RequestTty::Yes };
            }
            "-tt" => tty = RequestTty::Force,
            "-T" => tty = RequestTty::No,
            arg if arg.starts_with("-t") && arg.len() > 2 && arg.chars().skip(1).all(|c| c == 't') => {
                // Multiple -t in one token: -ttt, -tttt, etc.
                tty = RequestTty::Force;
            }
            _ => {}
        }
        i += 1;
    }
    tty
}

/// Splits `user@host` into `(Some(user), host)`. If no `@` is present,
/// returns `(None, host)`.
pub(crate) fn split_user_host(destination: &str) -> (Option<String>, String) {
    match destination.rsplit_once('@') {
        Some((user, host)) if !user.is_empty() && !host.is_empty() => {
            (Some(user.to_string()), host.to_string())
        }
        _ => (None, destination.to_string()),
    }
}

#[cfg(test)]
mod extract_request_tty_tests {
    use super::*;

    #[test]
    fn split_user_host_with_user() {
        assert_eq!(split_user_host("cuzic@vpsmart"), (Some("cuzic".to_string()), "vpsmart".to_string()));
    }

    #[test]
    fn split_user_host_without_user() {
        assert_eq!(split_user_host("vpsmart"), (None, "vpsmart".to_string()));
    }

    #[test]
    fn split_user_host_empty_user() {
        assert_eq!(split_user_host("@vpsmart"), (None, "@vpsmart".to_string()));
    }

    #[test]
    fn split_user_host_empty_host() {
        assert_eq!(split_user_host("cuzic@"), (None, "cuzic@".to_string()));
    }

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_is_auto() {
        assert_eq!(extract_request_tty(&s(&["host"]), 0), RequestTty::Auto);
    }

    #[test]
    fn dash_t_forces_pty() {
        assert_eq!(extract_request_tty(&s(&["-t", "host"]), 1), RequestTty::Yes);
    }

    #[test]
    fn dash_twice_is_force() {
        assert_eq!(extract_request_tty(&s(&["-t", "-t", "host"]), 2), RequestTty::Force);
    }

    #[test]
    fn dash_tt_is_force() {
        assert_eq!(extract_request_tty(&s(&["-tt", "host"]), 1), RequestTty::Force);
    }

    #[test]
    fn dash_upper_t_suppresses_pty() {
        assert_eq!(extract_request_tty(&s(&["-T", "host"]), 1), RequestTty::No);
    }

    #[test]
    fn dash_t_after_destination_is_ignored() {
        // -t after the destination is a remote command argument, not a flag
        assert_eq!(extract_request_tty(&s(&["host", "-t"]), 0), RequestTty::Auto);
    }

    #[test]
    fn dash_t_before_destination_with_other_flags() {
        assert_eq!(
            extract_request_tty(&s(&["-p", "2222", "-t", "host", "cmd"]), 3),
            RequestTty::Yes
        );
    }
}

async fn resolve_wrapper(plan: &WrapperPlan) -> Result<WrapperResolution> {
    let openssh = resolve_openssh_effective_config(plan).await?;
    let isekai = resolve_isekai_config(plan, &openssh)?;
    Ok(WrapperResolution { openssh, isekai })
}

/// The `native/` module's equivalent of [`resolve_wrapper`]: resolves
/// `~/.ssh/config` via the dependency-free `openssh-config` crate (M1)
/// instead of shelling out to `ssh -G` (`resolve_openssh_effective_config`),
/// since the native path exists specifically for machines that may not have
/// `ssh.exe` at all. `#@isekai` directive resolution (`resolve_isekai_config`)
/// is unchanged and shared verbatim with the Unix wrapper path — it only
/// ever reads `openssh.hostname`/`openssh.port` (see its own doc comment),
/// both of which `openssh_config::HostConfig` provides just as well as
/// `ssh -G` did.
///
/// Returns the full `openssh_config::HostConfig` alongside `WrapperResolution`
/// because the native path needs fields (`identity_file`, `identity_agent`,
/// `forward_agent`) that `OpenSshEffectiveConfig` deliberately doesn't carry
/// (the Unix wrapper path never needs them — real `ssh(1)` resolves those
/// itself from the same config file).
pub(crate) fn resolve_for_native(plan: &WrapperPlan) -> Result<(WrapperResolution, openssh_config::HostConfig)> {
    let explicit_f = dash_f_config_path(&plan.ssh_args);
    log_line!("isekai-ssh: dash_f={:?}, destination={:?}, dest_host={:?}, dest_user={:?}", explicit_f, plan.destination, plan.destination_host, plan.destination_user);
    let host_config = match explicit_f {
        Some(config_path) => {
            log_line!("isekai-ssh: resolving config from {}", config_path.display());
            openssh_config::resolve(&config_path, &plan.destination_host).map_err(|e| {
                anyhow!(
                    "isekai-ssh: failed to resolve {:?} from {}: {e}",
                    plan.destination,
                    config_path.display()
                )
            })
        }
        None => {
            log_line!("isekai-ssh: resolving config from ~/.ssh/config for {:?}", plan.destination_host);
            openssh_config::resolve_default(&plan.destination_host).map_err(|e| {
                anyhow!(
                    "isekai-ssh: failed to resolve {:?} from ~/.ssh/config: {e}",
                    plan.destination
                )
            })
        }
    }?;
    log_line!(
        "isekai-ssh: resolved host_config: hostname={:?}, user={:?}, port={:?}, identity_file={:?}",
        host_config.host_name,
        host_config.user,
        host_config.port,
        host_config.identity_file,
    );
    let mut host_config = host_config;
    // Command-line overrides (`-p`/`-l`/`-J`/`-o Key=Value`) beat the config
    // file, matching real `ssh(1)` precedence. The Unix path gets this for
    // free by forwarding every arg to `ssh -G`; the native path resolves the
    // config file directly, so it has to re-apply the overrides itself or it
    // would silently connect to the config-file (or default) host/port/user.
    apply_cli_overrides(&mut host_config, &plan.ssh_args)?;
    let openssh = OpenSshEffectiveConfig {
        hostname: host_config.host_name.clone(),
        user: host_config.user.clone(),
        port: host_config.port,
        proxy_jump: host_config.proxy_jump.clone(),
        proxy_command: None,
    };
    let isekai = resolve_isekai_config(plan, &openssh)?;
    Ok((WrapperResolution { openssh, isekai }, host_config))
}

/// Command-line overrides collected from `ssh_args` (the portion before the
/// destination), to be layered on top of a config-file-resolved `HostConfig`.
/// First value wins per keyword — mirroring `ssh(1)`, where the earliest
/// command-line occurrence of an option takes precedence.
#[derive(Default)]
struct CliOverrides {
    host_name: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    proxy_jump: Option<String>,
    identity_file: Vec<PathBuf>,
    forward_agent: Option<openssh_config::ForwardAgent>,
    identity_agent: Option<PathBuf>,
}

/// Applies command-line overrides (`-p`/`-l`/`-J`/`-o Key=Value`) from
/// `ssh_args` onto `host_config`, so they win over the resolved config file —
/// the ordering real `ssh(1)` uses (command-line options beat matched
/// `Host`/`Match` blocks). Only the keywords `openssh_config::HostConfig`
/// already models are handled; every other `-o Key` is ignored, exactly as
/// `openssh-config` ignores config-file keywords outside its subset.
fn apply_cli_overrides(host_config: &mut openssh_config::HostConfig, ssh_args: &[String]) -> Result<()> {
    let overrides = collect_cli_overrides(ssh_args)?;
    if let Some(host_name) = overrides.host_name {
        host_config.host_name = Some(host_name);
    }
    if let Some(user) = overrides.user {
        host_config.user = Some(user);
    }
    if let Some(port) = overrides.port {
        host_config.port = Some(port);
    }
    if let Some(proxy_jump) = overrides.proxy_jump {
        host_config.proxy_jump = Some(proxy_jump);
    }
    if let Some(forward_agent) = overrides.forward_agent {
        host_config.forward_agent = Some(forward_agent);
    }
    if let Some(identity_agent) = overrides.identity_agent {
        host_config.identity_agent = Some(identity_agent);
    }
    if !overrides.identity_file.is_empty() {
        // `IdentityFile` accumulates in `ssh(1)`, with command-line entries
        // taking priority — prepend so they're tried first.
        let mut merged = overrides.identity_file;
        merged.append(&mut host_config.identity_file);
        host_config.identity_file = merged;
    }
    Ok(())
}

/// Walks `ssh_args` up to the destination (same scope and option-width logic
/// as `dash_f_config_path`) collecting the overrides expressible through
/// `openssh_config::HostConfig`. First occurrence wins per keyword.
fn collect_cli_overrides(ssh_args: &[String]) -> Result<CliOverrides> {
    let mut overrides = CliOverrides::default();
    for (arg, value) in ssh_options_before_destination(ssh_args) {
        match (arg, value) {
            ("-p", Some(v)) => {
                let port = v
                    .parse::<u16>()
                    .with_context(|| format!("isekai-ssh: invalid -p port: {v}"))?;
                overrides.port.get_or_insert(port);
            }
            ("-l", Some(v)) => {
                overrides.user.get_or_insert_with(|| v.to_string());
            }
            ("-J", Some(v)) => {
                overrides.proxy_jump.get_or_insert_with(|| v.to_string());
            }
            ("-i", Some(v)) => {
                overrides.identity_file.push(openssh_config::expand_tilde(v));
            }
            ("-o", Some(v)) => {
                apply_dash_o_override(&mut overrides, v)?;
            }
            _ => {}
        }
    }
    Ok(overrides)
}

/// Applies one `-o Key=Value` (or `-o "Key Value"`, or `-o "Key = Value"`)
/// token to `overrides`. Keyword matching is case-insensitive, following
/// `ssh_config(5)`. Keywords outside `openssh_config::HostConfig`'s modeled
/// subset are silently ignored.
fn apply_dash_o_override(overrides: &mut CliOverrides, token: &str) -> Result<()> {
    // Split at the first delimiter (whitespace or `=`), then strip a leading
    // `=` (plus any whitespace after it) from what remains — matching
    // `openssh_config::split_keyword`'s own two-step approach, so
    // `Key=Value`, `"Key Value"`, and `"Key = Value"` (spaces *around* the
    // `=`) all parse the same way a naive single `split_once` on `[=\s]`
    // doesn't (that would leave a stray leading `=` in `val` for the
    // spaced-`=` form and fail e.g. `Port`'s `u16` parse).
    let Some(end) = token.find(|c: char| c == '=' || c.is_whitespace()) else {
        return Ok(());
    };
    let key = &token[..end];
    let mut val = token[end..].trim_start();
    if let Some(stripped) = val.strip_prefix('=') {
        val = stripped.trim_start();
    }
    let val = openssh_config::strip_quotes(val.trim());
    match key.trim().to_ascii_lowercase().as_str() {
        "hostname" => {
            overrides.host_name.get_or_insert_with(|| val.to_string());
        }
        "user" => {
            overrides.user.get_or_insert_with(|| val.to_string());
        }
        "port" => {
            let port = val
                .parse::<u16>()
                .with_context(|| format!("isekai-ssh: invalid -o Port: {val}"))?;
            overrides.port.get_or_insert(port);
        }
        "proxyjump" => {
            overrides.proxy_jump.get_or_insert_with(|| val.to_string());
        }
        "identityfile" => {
            overrides.identity_file.push(openssh_config::expand_tilde(val));
        }
        "identityagent" => {
            overrides.identity_agent.get_or_insert_with(|| openssh_config::expand_tilde(val));
        }
        "forwardagent" => {
            overrides.forward_agent.get_or_insert_with(|| match val.to_ascii_lowercase().as_str() {
                "yes" => openssh_config::ForwardAgent::Yes,
                "no" => openssh_config::ForwardAgent::No,
                _ => openssh_config::ForwardAgent::Socket(val.to_string()),
            });
        }
        _ => {}
    }
    Ok(())
}

/// Finds an explicit `-F <path>` in `ssh_args` (the portion before the
/// destination — mirroring `ssh_args_through_destination`'s scope, since a
/// `-F` appearing *after* the destination is a remote command argument, not
/// an option). Only recognizes the spaced `-F value` form (matching this
/// function's only two callers/tests) — a concatenated `-Fvalue` isn't
/// handled by `find_destination_index` either, so this is consistent with
/// the wrapper's existing `-F` support, not a new gap.
fn dash_f_config_path(ssh_args: &[String]) -> Option<PathBuf> {
    ssh_options_before_destination(ssh_args).find(|(arg, _)| *arg == "-F")?.1.map(PathBuf::from)
}

async fn resolve_openssh_effective_config(plan: &WrapperPlan) -> Result<OpenSshEffectiveConfig> {
    let mut command = Command::new(&plan.openssh_path);
    command.arg("-G");
    command.args(ssh_args_through_destination(plan));
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().await.map_err(|e| {
        anyhow!(
            "isekai-ssh: failed to execute `{} -G`: {e}",
            plan.openssh_path.display()
        )
    })?;
    if !output.status.success() {
        return Err(anyhow!(
            "isekai-ssh: `{} -G` failed: {}",
            plan.openssh_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_ssh_g_output(&String::from_utf8_lossy(&output.stdout))
}

fn ssh_args_through_destination(plan: &WrapperPlan) -> &[String] {
    &plan.ssh_args[..=plan.destination_index]
}

fn parse_ssh_g_output(output: &str) -> Result<OpenSshEffectiveConfig> {
    let mut config = OpenSshEffectiveConfig::default();
    for raw_line in output.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim();
        match key.to_ascii_lowercase().as_str() {
            "hostname" => config.hostname = Some(value.to_string()),
            "user" => config.user = Some(value.to_string()),
            "port" => {
                config.port = Some(
                    value
                        .parse()
                        .with_context(|| format!("invalid ssh -G port: {value}"))?,
                );
            }
            "proxyjump" if value != "none" => config.proxy_jump = Some(value.to_string()),
            "proxycommand" if value != "none" => config.proxy_command = Some(value.to_string()),
            _ => {}
        }
    }
    Ok(config)
}

fn next_value(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    iter.next()
        .ok_or_else(|| anyhow!("isekai-ssh: {flag} requires a value"))
}

fn default_pipe_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ISEKAI_PIPE_PATH") {
        return PathBuf::from(path);
    }

    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let mut sibling = dir.join("isekai-pipe");
            if cfg!(windows) {
                sibling.set_extension("exe");
            }
            if sibling.exists() {
                return sibling;
            }
        }
    }

    PathBuf::from("isekai-pipe")
}

/// Builds the local `ssh(1)`'s `-o ProxyCommand=...` value.
///
/// On Windows this deliberately does *not* reuse `shell_quote`'s POSIX
/// single-quote escaping unconditionally: real ssh(1) on Unix always execs
/// `ProxyCommand` via `/bin/sh -c`, so POSIX quoting is unambiguously
/// correct there, but Win32-OpenSSH does not go through a POSIX shell at
/// all — its own internal `ProxyCommand` argument splitting is a separate,
/// less-documented code path (its own GitHub issue tracker shows both
/// single- and double-quoted paths misbehaving across different Win32-OpenSSH
/// versions/builds — `CreateProcessW failed`/`posix_spawn` errors either
/// way), and this repo has no Windows CI to pin an exact version against.
/// Rather than guess which quoting convention today's Win32-OpenSSH build
/// wants, `windows_arg_needs_no_quoting` below sidesteps the question: a
/// short (8.3) filename never contains a space, so once resolved there is
/// nothing left to quote — this is the same well-established workaround
/// build tools like vcpkg use for the identical "external tool mishandles
/// quoted Windows paths" class of problem. `profile` values are similarly
/// emitted bare whenever they already match a safe charset (in practice
/// always true — see `is_safe_bare_word`'s docs), which is the common case
/// regardless of platform.
///
/// That short-path-and-emit-bare trick only holds for genuine
/// Win32-OpenSSH, though. An MSYS2- or Cygwin-hosted `ssh.exe` (this
/// includes Git for Windows' bundled ssh) *does* exec `ProxyCommand` via
/// `/bin/bash -c ...`, exactly like Unix OpenSSH — a bare short path handed
/// to that bash strips every backslash (`\U`, `\c`, ... are bash escape
/// sequences that just drop the backslash before an ordinary character),
/// silently mangling the path instead of failing loudly. `openssh_path` is
/// checked via `is_posix_shell_ssh` so that case forces the same POSIX
/// single-quoting Unix always uses, bypassing the short-path optimization
/// entirely (single quotes preserve backslashes literally in POSIX shells,
/// so no further escaping is needed).
///
/// Falls back to the original POSIX-style single-quoting when a value isn't
/// already safe to emit bare (e.g. 8.3 short names are disabled on that
/// volume, or an unusual `#@isekai profile` value) — no worse than this
/// function's previous unconditional behavior for that residual case.
fn proxy_command(pipe_path: &Path, profile: &str, openssh_path: &Path) -> String {
    let force_posix_quoting = cfg!(windows) && is_posix_shell_ssh(openssh_path);
    format!(
        "{} connect --profile {} --service ssh --stdio",
        quote_proxy_command_path(pipe_path, force_posix_quoting),
        quote_proxy_command_arg(profile),
    )
}

/// Characters that never need shell/argv escaping in *any* of the quoting
/// conventions this module has to worry about (POSIX `/bin/sh -c`, and
/// whatever Win32-OpenSSH's own `ProxyCommand` argument splitter does) — so
/// a value built entirely from this charset can always be emitted bare,
/// without picking a quoting convention at all. Deliberately excludes `'`/
/// `"`/`$`/`` ` ``/`;`/`|`/`&`/`<`/`>`/`(`/`)`/`{`/`}`/`\` (and, of course,
/// whitespace) — every character either convention treats specially.
fn is_safe_bare_word(value: &str, extra_allowed: &[char]) -> bool {
    !value.is_empty()
        && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '@' | '~') || extra_allowed.contains(&c))
}

/// `profile` values come from either the literal `ssh(1)` destination
/// argument (a bare host/alias, essentially never containing whitespace or
/// shell metacharacters) or a single whitespace-delimited `#@isekai profile`
/// directive token (`apply_isekai_directive`'s `one_arg` already rejects
/// anything with an embedded space) — so in practice this is always safe to
/// emit bare. Falls back to POSIX single-quoting for the residual case of a
/// directive value containing something outside that charset.
fn quote_proxy_command_arg(value: &str) -> String {
    if is_safe_bare_word(value, &[]) {
        value.to_string()
    } else {
        shell_quote(value)
    }
}

/// `pipe_path` almost always resolves to a path with no spaces on Unix
/// (`/usr/local/bin/isekai-pipe`-shaped), but commonly *does* have one on
/// Windows (`C:\Program Files\...`) — the one case this whole function
/// exists for. See `windows_short_path`'s docs for the avoid-quoting-entirely
/// strategy used there; every other platform/path shape falls through to
/// the original POSIX single-quoting.
///
/// `force_posix_quoting` (set by `proxy_command` when `openssh_path` is an
/// MSYS2/Cygwin-hosted `ssh.exe`, see `is_posix_shell_ssh`) skips the
/// Windows short-path branch entirely and always POSIX single-quotes —
/// that `ssh` execs `ProxyCommand` via bash, not Win32-OpenSSH's own
/// splitter, so the short-path-and-emit-bare trick is unsafe there.
fn quote_proxy_command_path(pipe_path: &Path, force_posix_quoting: bool) -> String {
    let path_str = pipe_path.display().to_string();
    if !force_posix_quoting {
        if cfg!(windows) {
            if let Some(short) = windows_short_path(&path_str) {
                if is_safe_bare_word(&short, &['\\', '/']) {
                    return short;
                }
            }
        }
        if is_safe_bare_word(&path_str, &['\\', '/']) {
            return path_str;
        }
    }
    shell_quote(&path_str)
}

/// Resolves `path` to its 8.3 short filename (`C:\PROGRA~1\...`), which by
/// construction never contains a space or any other character needing shell
/// escaping — sidesteps needing to know Win32-OpenSSH's own `ProxyCommand`
/// argument-splitting convention at all, rather than guessing it (see
/// `proxy_command`'s docs). Requires the path to already exist on disk
/// (`GetShortPathNameW` queries the real filesystem entry, which
/// `default_pipe_path`'s own `sibling.exists()` check already guarantees
/// for the common case) and that 8.3 name generation hasn't been disabled
/// for that volume (an uncommon but real opt-out, mostly seen on some
/// server SSD setups for a minor performance gain) — returns `None` in
/// either case, and `quote_proxy_command_path` falls back to quoting the
/// long path instead.
#[cfg(windows)]
fn windows_short_path(path: &str) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;

    let wide: Vec<u16> = std::ffi::OsStr::new(path).encode_wide().chain(std::iter::once(0)).collect();
    let mut buf = vec![0u16; 260];
    // SAFETY: `wide` is a valid, NUL-terminated UTF-16 string for the
    // duration of this call; `buf` is a valid, writable buffer of the given
    // length. `GetShortPathNameW` never retains either pointer past return.
    let len = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if len == 0 {
        return None; // path doesn't exist, or some other failure.
    }
    if len as usize > buf.len() {
        // Buffer was too small; `len` is the required size including the
        // NUL terminator — retry once with exactly that much room.
        buf = vec![0u16; len as usize];
        let len2 = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        if len2 == 0 || len2 as usize > buf.len() {
            return None;
        }
        buf.truncate(len2 as usize);
    } else {
        buf.truncate(len as usize);
    }
    String::from_utf16(&buf).ok()
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetShortPathNameW(lpszLongPath: *const u16, lpszShortPath: *mut u16, cchBuffer: u32) -> u32;
}

/// Best-effort resolution of a `Command::new`-style program name (as stored
/// in `WrapperPlan::openssh_path`) to the actual file it will exec — either
/// already-qualified (absolute, or has a directory component: `--isekai-ssh-path`
/// was passed explicitly) and returned as-is, or the bare default (`"ssh"`,
/// see `parse_wrapper`) searched for across `%PATH%` using `%PATHEXT%`
/// (falling back to the same default Windows uses, `.COM;.EXE;.BAT;.CMD`,
/// when unset) the same way `CreateProcess` would. Deliberately does not
/// search the current directory first — unlike a directly-typed shell
/// command, `std::process::Command` never implicitly does that either.
#[cfg(windows)]
fn resolve_windows_executable(program: &Path) -> PathBuf {
    if program.is_absolute() || program.parent().is_some_and(|p| !p.as_os_str().is_empty()) {
        return program.to_path_buf();
    }
    let extensions: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_start_matches('.').to_string())
        .collect();
    let already_has_extension = program.extension().is_some();
    let Some(path_var) = std::env::var_os("PATH") else {
        return program.to_path_buf();
    };
    for dir in std::env::split_paths(&path_var) {
        if already_has_extension {
            let candidate = dir.join(program);
            if candidate.is_file() {
                return candidate;
            }
        }
        for ext in &extensions {
            let candidate = dir.join(program).with_extension(ext);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    program.to_path_buf()
}

/// True when `ssh_path` resolves to an MSYS2- or Cygwin-hosted `ssh.exe`
/// (this includes Git for Windows' bundled ssh, which also ships the MSYS2
/// runtime) rather than genuine Win32-OpenSSH
/// (`C:\Windows\System32\OpenSSH\ssh.exe`). Such builds exec `ProxyCommand`
/// via `/bin/bash -c ...` just like Unix OpenSSH — see `proxy_command`'s
/// docs for why that matters. Detected by checking for the runtime DLL next
/// to the resolved binary rather than pattern-matching the path text, since
/// MSYS2/Git-for-Windows can be installed under any prefix (the report that
/// prompted this used a `scoop`-managed install, for example).
#[cfg(windows)]
fn is_posix_shell_ssh(ssh_path: &Path) -> bool {
    let resolved = resolve_windows_executable(ssh_path);
    let Some(dir) = resolved.parent() else {
        return false;
    };
    dir.join("msys-2.0.dll").is_file() || dir.join("cygwin1.dll").is_file()
}

/// Never actually called (`proxy_command` only calls `is_posix_shell_ssh`
/// inside `cfg!(windows) && ...`), but `cfg!(...)` is a runtime check, not
/// conditional compilation — the call site still needs something to
/// type-check against on non-Windows targets.
#[cfg(not(windows))]
fn is_posix_shell_ssh(_ssh_path: &Path) -> bool {
    false
}

/// Never actually called (`quote_proxy_command_path` only calls
/// `windows_short_path` inside `if cfg!(windows)`), but `cfg!(...)` is a
/// runtime check, not conditional compilation — the call site still needs
/// something to type-check against on non-Windows targets.
#[cfg(not(windows))]
fn windows_short_path(_path: &str) -> Option<String> {
    None
}

/// POSIX `/bin/sh` single-quote escaping. `pub(crate)` so `tty_attach.rs` can
/// reuse it to quote a `--isekai-tty=<name>` value the same way this module
/// already quotes a hostile `#@isekai` directive value or proxy-command arg
/// — one escaping implementation, not a second hand-rolled copy.
pub(crate) fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    let mut quoted = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn find_destination_index(args: &[String]) -> Option<usize> {
    let mut walk = ssh_options_before_destination(args);
    for _ in walk.by_ref() {}
    let i = walk.stopped_at();
    match args.get(i) {
        // `--` explicitly terminates option parsing (`ssh(1)`'s own
        // convention): the destination is whatever immediately follows it,
        // if anything.
        Some(arg) if arg == "--" => (i + 1 < args.len()).then_some(i + 1),
        // A non-option arg (or a bare `-`, treated as non-option — matches
        // `ssh_options_before_destination`'s own stopping condition) is the
        // destination itself.
        Some(_) => Some(i),
        // Ran out of args without ever finding `--` or a non-option — every
        // arg was consumed as an option (or an option's value).
        None => None,
    }
}

/// Walks `args`' leading `ssh(1)` options (skipping each value-taking
/// flag's value via [`ssh_option_width`]), yielding `(option, value)` for
/// every option encountered before the destination — mirrors `ssh(1)`'s own
/// argv shape: `--`, or the first arg that isn't itself an option (or is a
/// bare `-`), ends option parsing, and everything from there on is the
/// destination and (optionally) a trailing remote command, never another
/// option. `value` is `Some` only for a value-taking flag (`ssh_option_width`
/// returning `2`) that actually has a following arg; a non-value-taking flag
/// always yields `None` regardless of what follows it.
///
/// [`find_destination_index`], [`dash_f_config_path`], and
/// [`collect_cli_overrides`] each used to hand-roll this exact walk (the
/// same `arg == "--" || !arg.starts_with('-') || arg == "-"` stopping guard,
/// the same `i += ssh_option_width(arg)` stepping), differing only in what
/// they did with each option — or, for `find_destination_index`, in wanting
/// the stopping index rather than the pairs themselves, which
/// [`SshOptionsBeforeDestination::stopped_at`] exposes for exactly that
/// caller.
fn ssh_options_before_destination(args: &[String]) -> SshOptionsBeforeDestination<'_> {
    SshOptionsBeforeDestination { args, i: 0 }
}

struct SshOptionsBeforeDestination<'a> {
    args: &'a [String],
    i: usize,
}

impl<'a> Iterator for SshOptionsBeforeDestination<'a> {
    type Item = (&'a str, Option<&'a str>);

    fn next(&mut self) -> Option<Self::Item> {
        let arg = self.args.get(self.i)?.as_str();
        if arg == "--" || !arg.starts_with('-') || arg == "-" {
            return None;
        }
        let width = ssh_option_width(arg);
        let value = if width == 2 { self.args.get(self.i + 1).map(String::as_str) } else { None };
        self.i += width;
        Some((arg, value))
    }
}

impl SshOptionsBeforeDestination<'_> {
    /// The index in `args` where iteration stopped — either `--`'s index, a
    /// non-option's (the destination itself), or `args.len()` if every arg
    /// was consumed as an option/its value. Only meaningful once the
    /// iterator is exhausted; [`find_destination_index`] is the only caller
    /// that needs it (the other two consumers only care about the `(option,
    /// value)` pairs themselves).
    fn stopped_at(&self) -> usize {
        self.i
    }
}

fn ssh_option_width(arg: &str) -> usize {
    if matches!(
        arg,
        "-B" | "-b"
            | "-c"
            | "-D"
            | "-E"
            | "-e"
            | "-F"
            | "-I"
            | "-i"
            | "-J"
            | "-L"
            | "-l"
            | "-m"
            | "-O"
            | "-o"
            | "-p"
            | "-Q"
            | "-R"
            | "-S"
            | "-W"
            | "-w"
    ) {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn resolve_claimed_outcome_passes_through_ok_results() {
        assert_eq!(resolve_claimed_outcome(Ok(None)), None);
        let outcome = isekai_pipe_core::ConnectOutcome {
            schema_version: 1,
            intent_id: "abc".to_string(),
            profile: "prod".to_string(),
            class: isekai_pipe_core::ConnectOutcomeClass::Unreachable,
            detail: "idle timeout".to_string(),
        };
        assert_eq!(resolve_claimed_outcome(Ok(Some(outcome.clone()))), Some(outcome));
    }

    /// Epic R PR1 (S4): a `claim_connect_outcome` failure (a corrupt outcome
    /// file, or a schema shape too different for `ConnectOutcomeClass::Unknown`
    /// to absorb) must degrade to "no signal" rather than propagate — this is
    /// the regression this whole function exists to prevent (before it, the
    /// `?` at the call site turned this into a hard `Err` for the entire
    /// `isekai-ssh` invocation).
    #[test]
    fn resolve_claimed_outcome_degrades_a_claim_failure_to_no_signal() {
        assert_eq!(resolve_claimed_outcome(Err(isekai_pipe_core::IntentError::InvalidIntentId)), None);
    }

    #[test]
    fn decide_connect_failure_recovery_returns_no_signal_when_no_signal_was_found() {
        assert_eq!(decide_connect_failure_recovery(None, true), ConnectFailureRecoveryAction::NoRecoverableSignal);
        // Whether auto-bootstrap is allowed is irrelevant without a signal.
        assert_eq!(decide_connect_failure_recovery(None, false), ConnectFailureRecoveryAction::NoRecoverableSignal);
    }

    #[test]
    fn decide_connect_failure_recovery_returns_disabled_when_signal_found_but_bootstrap_off() {
        assert_eq!(
            decide_connect_failure_recovery(Some(&isekai_pipe_core::ConnectOutcomeClass::Unreachable), false),
            ConnectFailureRecoveryAction::AutoBootstrapDisabled
        );
        assert_eq!(
            decide_connect_failure_recovery(Some(&isekai_pipe_core::ConnectOutcomeClass::StaleTrust), false),
            ConnectFailureRecoveryAction::AutoBootstrapDisabled
        );
    }

    #[test]
    fn decide_connect_failure_recovery_retries_when_signal_found_and_bootstrap_allowed() {
        assert_eq!(
            decide_connect_failure_recovery(Some(&isekai_pipe_core::ConnectOutcomeClass::Unreachable), true),
            ConnectFailureRecoveryAction::RebootstrapAndRetry
        );
        assert_eq!(
            decide_connect_failure_recovery(Some(&isekai_pipe_core::ConnectOutcomeClass::StaleTrust), true),
            ConnectFailureRecoveryAction::RebootstrapAndRetry
        );
    }

    /// Epic R PR2: `MidSessionDisconnect` always retries lightweight,
    /// regardless of `should_bootstrap` — it never re-deploys over SSH, so
    /// `--isekai-no-bootstrap`'s "don't silently re-deploy" contract isn't
    /// violated by trying it (ADR round 1, Q2).
    #[test]
    fn decide_connect_failure_recovery_retries_lightweight_for_mid_session_disconnect_regardless_of_bootstrap_flag() {
        assert_eq!(
            decide_connect_failure_recovery(Some(&isekai_pipe_core::ConnectOutcomeClass::MidSessionDisconnect), true),
            ConnectFailureRecoveryAction::RetryConnectLightweight
        );
        assert_eq!(
            decide_connect_failure_recovery(Some(&isekai_pipe_core::ConnectOutcomeClass::MidSessionDisconnect), false),
            ConnectFailureRecoveryAction::RetryConnectLightweight
        );
    }

    /// Epic R PR2 (R1-B1): `Unknown` must be treated like `Unreachable` —
    /// attempt recovery — not like "no signal at all". An earlier draft of
    /// this fix got this backwards, which round 1 review caught as a
    /// regression against `.claude/rules/always-connects.md`.
    #[test]
    fn decide_connect_failure_recovery_treats_unknown_class_like_unreachable_not_like_no_signal() {
        assert_eq!(
            decide_connect_failure_recovery(Some(&isekai_pipe_core::ConnectOutcomeClass::Unknown), true),
            ConnectFailureRecoveryAction::RebootstrapAndRetry
        );
        assert_eq!(
            decide_connect_failure_recovery(Some(&isekai_pipe_core::ConnectOutcomeClass::Unknown), false),
            ConnectFailureRecoveryAction::AutoBootstrapDisabled
        );
    }

    // Epic R PR2, Task 2.13: live reconnect status formatting.

    #[test]
    fn format_process_reconnect_status_colors_on_a_tty_but_stays_a_single_newline_terminated_line() {
        let msg = format_process_reconnect_status(true, 3);
        assert!(msg.contains("\x1b["), "TTY display should still be colored: {msg:?}");
        assert!(!msg.contains('\r'), "must not be an in-place \\r redraw (round 2 review: there's no matching 'reconnected' event to redraw over at this layer): {msg:?}");
        assert!(msg.contains("attempt 3"), "{msg:?}");
    }

    #[test]
    fn format_process_reconnect_status_is_plain_text_off_a_tty() {
        let msg = format_process_reconnect_status(false, 3);
        assert!(!msg.contains('\r'), "non-TTY output must not contain \\r (would corrupt --isekai-log-file): {msg:?}");
        assert!(!msg.contains('\x1b'), "non-TTY output must not contain ANSI escapes: {msg:?}");
        assert!(msg.contains("attempt 3"), "{msg:?}");
    }

    #[tokio::test]
    async fn resolve_stun_servers_accepts_a_literal_socket_addr() {
        let resolved = resolve_stun_servers(&["203.0.113.9:3478".to_string()]).await;
        assert_eq!(resolved, vec!["203.0.113.9:3478".parse::<SocketAddr>().unwrap()]);
    }

    #[tokio::test]
    async fn resolve_stun_servers_resolves_a_hostname_via_dns() {
        // `localhost:PORT` resolves via the OS resolver without needing
        // external network access (loopback is always in `/etc/hosts` or
        // equivalent) — a stand-in for a real-world hostname entry like
        // `stun.l.google.com:19302`, which `.parse::<SocketAddr>()` alone
        // always rejects as "malformed" since it requires a literal IP.
        let resolved = resolve_stun_servers(&["localhost:3478".to_string()]).await;
        assert_eq!(resolved.len(), 1, "expected localhost:3478 to resolve to exactly one address");
        assert_eq!(resolved[0].port(), 3478);
        assert!(resolved[0].ip().is_loopback());
    }

    #[tokio::test]
    async fn resolve_stun_servers_skips_an_unresolvable_entry() {
        let resolved = resolve_stun_servers(&["not a valid host or addr".to_string()]).await;
        assert!(resolved.is_empty());
    }

    #[tokio::test]
    async fn bootstrap_and_register_classifies_missing_helper_binary() {
        let plan = WrapperPlan {
            openssh_path: PathBuf::from("/usr/bin/ssh"),
            pipe_path: PathBuf::from("/usr/bin/isekai-pipe"),
            destination: "production".to_string(),
            destination_host: "production".to_string(),
            destination_user: None,
            destination_index: 0,
            ssh_args: Vec::new(),
            isekai: WrapperIsekaiOptions::default(),
            request_tty: RequestTty::Auto,
        };
        let resolution = WrapperResolution {
            openssh: OpenSshEffectiveConfig::default(),
            isekai: IsekaiConfig {
                enabled: true,
                bootstrap_policy: BootstrapPolicy::Auto,
                profile: "production".to_string(),
                remote_path: None,
                services: vec![ServiceSpec::ssh_target("127.0.0.1:22").unwrap()],
                // `127.0.0.1:1` (not `production:22`): nothing listens on
                // port 1, so the `ssh(1)` subprocess `detect_remote_arch`
                // spawns for its `uname -m` probe fails instantly with
                // "connection refused" — no DNS lookup, no timeout wait,
                // keeping this a fast/deterministic test despite now doing
                // a real subprocess spawn (`plan.isekai.helper_binary` is
                // `None`, so `resolve_helper_binary` no longer short-circuits
                // before *some* I/O — see `helper_download::resolve_helper_binary`).
                bootstrap_candidates: vec![BootstrapCandidate { target: "127.0.0.1:1".to_string(), via: Vec::new(), priority: 0, alias: None }],
                link_endpoints: Vec::new(),
                rendezvous: Vec::new(),
                stun_servers: Vec::new(),
                relay_endpoints: Vec::new(),
                resume_grace_secs: 180,
                candidate_race_delay_ms: 150,
                relay_delay_ms: 750,
                install_mode: InstallMode::User,
                bootstrap_relay: None,
                ctl_socket_enabled: false,
                tab_idle_color: None,
                tab_attention_color: None,
                remote_log_level: "info".to_string(),
                remote_bind_port_range: None,
                local_bind_port_range: None,
                tty: None,
            },
        };

        // `plan.isekai.helper_binary` is `None` (the default): no explicit
        // path is given, `detect_remote_arch` fails against the unreachable
        // target above, and `resolve_helper_binary` surfaces that failure —
        // classified the same as the old "no --isekai-helper-binary given"
        // hard error used to be, since the practical guidance is identical
        // either way ("no local isekai-pipe binary to upload").
        let err = bootstrap_and_register(&plan, &resolution, TofuConfirmation::AlwaysPrompt).await.unwrap_err();
        let failure = err
            .downcast_ref::<BootstrapFailure>()
            .expect("a classified BootstrapFailure should be attached to the error chain");
        assert!(matches!(failure, BootstrapFailure::RemoteBinaryMissing));
        assert!(failure.should_redirect_to_init());
        assert!(!failure.should_redirect_to_login());
        assert!(!failure.may_retry());
    }

    #[tokio::test]
    async fn bootstrap_and_register_accepts_a_multi_hop_via_chain_and_rejects_a_looping_one() {
        let mut plan = WrapperPlan {
            openssh_path: PathBuf::from("/usr/bin/ssh"),
            pipe_path: PathBuf::from("/usr/bin/isekai-pipe"),
            destination: "production".to_string(),
            destination_host: "production".to_string(),
            destination_user: None,
            destination_index: 0,
            ssh_args: Vec::new(),
            isekai: WrapperIsekaiOptions::default(),
            request_tty: RequestTty::Auto,
        };
        // A nonexistent path is enough: chain validation runs, and fails
        // closed, before this path is ever read from disk.
        plan.isekai.helper_binary = Some(PathBuf::from("/nonexistent/isekai-helper"));

        let resolution_with_via = |via: Vec<String>| WrapperResolution {
            openssh: OpenSshEffectiveConfig::default(),
            isekai: IsekaiConfig {
                enabled: true,
                bootstrap_policy: BootstrapPolicy::Auto,
                profile: "production".to_string(),
                remote_path: None,
                services: vec![ServiceSpec::ssh_target("127.0.0.1:22").unwrap()],
                bootstrap_candidates: vec![BootstrapCandidate { target: "production:22".to_string(), via, priority: 0, alias: None }],
                link_endpoints: Vec::new(),
                rendezvous: Vec::new(),
                stun_servers: Vec::new(),
                relay_endpoints: Vec::new(),
                resume_grace_secs: 180,
                candidate_race_delay_ms: 150,
                relay_delay_ms: 750,
                install_mode: InstallMode::User,
                bootstrap_relay: None,
                ctl_socket_enabled: false,
                tab_idle_color: None,
                tab_attention_color: None,
                remote_log_level: "info".to_string(),
                remote_bind_port_range: None,
                local_bind_port_range: None,
                tty: None,
            },
        };

        // A valid 2-hop chain: passes chain validation, then fails on the
        // (expected, unrelated) missing helper binary file — proving the
        // multi-hop path is no longer rejected outright the way it used to
        // be (`ISEKAI_PIPE_DESIGN.md` §8 Epic K). Classified as
        // `RemoteBinaryMissing` (helper_download's arch-detect/auto-download
        // fallback is skipped here since `--isekai-helper-binary` was given
        // explicitly, but a failure to read *that* still means "no local
        // binary to upload").
        let resolution = resolution_with_via(vec!["bastion-a".to_string(), "bastion-b".to_string()]);
        let err = bootstrap_and_register(&plan, &resolution, TofuConfirmation::AlwaysPrompt).await.unwrap_err();
        let failure = err.downcast_ref::<BootstrapFailure>().expect("classified as a BootstrapFailure");
        assert!(matches!(failure, BootstrapFailure::RemoteBinaryMissing), "{failure:?}");
        assert!(format!("{err:#}").contains("nonexistent/isekai-helper"), "{err:#}");

        // A looping chain (repeats the destination, same host *and* port —
        // cycle detection is port-sensitive, matching `plan.rs`'s own
        // `distinct_ports_on_the_same_host_are_not_a_cycle`) is still
        // rejected, now via `isekai_bootstrap_plan::validate_jump_chain`
        // rather than the old single-hop-only guard.
        let looping = resolution_with_via(vec!["bastion-a".to_string(), "production:22".to_string()]);
        let err = bootstrap_and_register(&plan, &looping, TofuConfirmation::AlwaysPrompt).await.unwrap_err();
        assert!(format!("{err:#}").contains("more than once"), "{err:#}");
    }

    #[test]
    fn wrapper_is_only_for_non_subcommand_invocations() {
        assert!(!should_run_wrapper(&s(&[])));
        assert!(!should_run_wrapper(&s(&["init", "host"])));
        assert!(!should_run_wrapper(&s(&["login", "host"])));
        assert!(!should_run_wrapper(&s(&["logout"])));
        assert!(!should_run_wrapper(&s(&["--help"])));
        assert!(should_run_wrapper(&s(&["production"])));
    }

    #[test]
    fn parses_wrapper_options_and_preserves_ssh_args() {
        let plan = parse_wrapper(s(&[
            "--isekai-ssh-path",
            "/usr/bin/ssh",
            "--isekai-pipe-path",
            "/opt/isekai pipe",
            "-p",
            "2222",
            "user@production",
            "uptime",
        ]))
        .unwrap();

        assert_eq!(plan.openssh_path, PathBuf::from("/usr/bin/ssh"));
        assert_eq!(plan.pipe_path, PathBuf::from("/opt/isekai pipe"));
        assert_eq!(plan.destination, "user@production");
        assert_eq!(plan.destination_index, 2);
        assert_eq!(
            plan.ssh_args,
            s(&["-p", "2222", "user@production", "uptime"])
        );
    }

    #[test]
    fn helper_release_source_defaults_to_this_projects_repo_and_latest() {
        let plan = parse_wrapper(s(&["production"])).unwrap();
        assert_eq!(plan.isekai.helper_release_repo, crate::helper_download::ReleaseSource::DEFAULT_REPO);
        assert_eq!(plan.isekai.helper_release_tag, None);
    }

    #[test]
    fn parses_helper_release_flags() {
        let plan = parse_wrapper(s(&[
            "--isekai-helper-release-repo",
            "someone/fork",
            "--isekai-helper-release-tag",
            "v1.2.3",
            "production",
        ]))
        .unwrap();
        assert_eq!(plan.isekai.helper_release_repo, "someone/fork");
        assert_eq!(plan.isekai.helper_release_tag, Some("v1.2.3".to_string()));
    }

    #[test]
    fn log_file_defaults_to_none_and_parses_when_given() {
        let plan = parse_wrapper(s(&["production"])).unwrap();
        assert_eq!(plan.isekai.log_file, None);

        let plan = parse_wrapper(s(&["--isekai-log-file", "/tmp/isekai-ssh.log", "production"])).unwrap();
        assert_eq!(plan.isekai.log_file, Some(PathBuf::from("/tmp/isekai-ssh.log")));
    }

    /// A `WrapperResolution` with no `#@isekai` directives applied — every
    /// `IsekaiConfig` field at its `resolve_isekai_config` default. Lets
    /// `resolved_tty_selection` tests below spell out only the one field
    /// (`isekai.tty`) they actually care about, instead of every unrelated
    /// field the bootstrap/`ConnectionIntent` tests elsewhere in this module
    /// need to.
    fn resolution_with_no_directives(profile: &str) -> WrapperResolution {
        WrapperResolution {
            openssh: OpenSshEffectiveConfig::default(),
            isekai: IsekaiConfig {
                enabled: true,
                bootstrap_policy: BootstrapPolicy::Auto,
                profile: profile.to_string(),
                remote_path: None,
                services: vec![ServiceSpec::ssh_target("127.0.0.1:22").unwrap()],
                bootstrap_candidates: Vec::new(),
                link_endpoints: Vec::new(),
                rendezvous: Vec::new(),
                stun_servers: Vec::new(),
                relay_endpoints: Vec::new(),
                resume_grace_secs: 180,
                candidate_race_delay_ms: 150,
                relay_delay_ms: 750,
                install_mode: InstallMode::User,
                bootstrap_relay: None,
                ctl_socket_enabled: false,
                remote_log_level: "info".to_string(),
                remote_bind_port_range: None,
                local_bind_port_range: None,
                tab_idle_color: None,
                tab_attention_color: None,
                tty: None,
            },
        }
    }

    #[test]
    fn tty_selection_defaults_to_none() {
        let plan = parse_wrapper(s(&["production"])).unwrap();
        let resolution = resolution_with_no_directives("production");
        assert_eq!(resolved_tty_selection(&plan, &resolution), None);
    }

    #[test]
    fn bare_isekai_tty_selects_auto() {
        let plan = parse_wrapper(s(&["--isekai-tty", "production"])).unwrap();
        let resolution = resolution_with_no_directives("production");
        assert_eq!(resolved_tty_selection(&plan, &resolution), Some(TtySelection::Auto));
    }

    #[test]
    fn isekai_tty_with_a_name_selects_named() {
        let plan = parse_wrapper(s(&["--isekai-tty=work", "production"])).unwrap();
        let resolution = resolution_with_no_directives("production");
        assert_eq!(resolved_tty_selection(&plan, &resolution), Some(TtySelection::Named("work".to_string())));
    }

    #[test]
    fn isekai_tty_with_an_empty_name_is_a_parse_error() {
        let err = parse_wrapper(s(&["--isekai-tty=", "production"])).unwrap_err();
        assert!(err.to_string().contains("--isekai-tty=<name>"), "unexpected error message: {err}");
    }

    #[test]
    fn isekai_tty_parses_fine_alongside_an_explicit_remote_command() {
        // Not a parse-time error (`--isekai-tty` + an explicit trailing
        // remote command is a valid, if redundant, combination) — resolution
        // silently prefers the explicit remote command at each call site
        // (`resolved_tty_selection`'s doc comment), not here.
        let plan = parse_wrapper(s(&["--isekai-tty", "production", "uptime"])).unwrap();
        let resolution = resolution_with_no_directives("production");
        assert_eq!(resolved_tty_selection(&plan, &resolution), Some(TtySelection::Auto));
        assert_eq!(plan.remote_command(), Some(&["uptime".to_string()][..]));
    }

    #[test]
    fn tty_directive_auto_resolves_when_no_cli_flag_is_given() {
        let plan = parse_wrapper(s(&["production"])).unwrap();
        let mut resolution = resolution_with_no_directives("production");
        resolution.isekai.tty = Some(TtyDirective::Auto);
        assert_eq!(resolved_tty_selection(&plan, &resolution), Some(TtySelection::Auto));
    }

    #[test]
    fn tty_directive_with_a_name_resolves_when_no_cli_flag_is_given() {
        let plan = parse_wrapper(s(&["production"])).unwrap();
        let mut resolution = resolution_with_no_directives("production");
        resolution.isekai.tty = Some(TtyDirective::Named("work".to_string()));
        assert_eq!(resolved_tty_selection(&plan, &resolution), Some(TtySelection::Named("work".to_string())));
    }

    #[test]
    fn tty_directive_off_resolves_to_no_tty_attach() {
        let plan = parse_wrapper(s(&["production"])).unwrap();
        let mut resolution = resolution_with_no_directives("production");
        resolution.isekai.tty = Some(TtyDirective::Off);
        assert_eq!(resolved_tty_selection(&plan, &resolution), None);
    }

    #[test]
    fn cli_flag_overrides_a_tty_directive_in_either_direction() {
        let plan = parse_wrapper(s(&["--isekai-tty=work", "production"])).unwrap();

        // Directive says `auto` — the explicit `--isekai-tty=work` still wins.
        let mut resolution = resolution_with_no_directives("production");
        resolution.isekai.tty = Some(TtyDirective::Auto);
        assert_eq!(resolved_tty_selection(&plan, &resolution), Some(TtySelection::Named("work".to_string())));

        // Directive says `off` — the CLI flag still wins (an explicit
        // `--isekai-tty` always means "yes, attach", never silently
        // overridden by config).
        resolution.isekai.tty = Some(TtyDirective::Off);
        assert_eq!(resolved_tty_selection(&plan, &resolution), Some(TtySelection::Named("work".to_string())));
    }

    #[test]
    fn finds_destination_after_common_ssh_options() {
        assert_eq!(
            find_destination_index(&s(&[
                "-i",
                "id key",
                "-o",
                "StrictHostKeyChecking=no",
                "prod"
            ])),
            Some(4)
        );
        assert_eq!(find_destination_index(&s(&["-vvv", "--", "prod"])), Some(2));
    }

    #[test]
    fn proxy_command_quotes_path_and_profile_for_shell() {
        assert_eq!(
            proxy_command(Path::new("/opt/isekai pipe"), "prod'host", Path::new("/usr/bin/ssh")),
            "'/opt/isekai pipe' connect --profile 'prod'\\''host' --service ssh --stdio"
        );
    }

    /// A path/profile with no space or shell metacharacter is emitted bare
    /// (no quoting at all) — safe on a POSIX shell either way, and sidesteps
    /// ever needing to guess Win32-OpenSSH's own `ProxyCommand`
    /// argument-splitting convention on Windows (see `proxy_command`'s
    /// module docs for why quoting there is a real, version-dependent
    /// minefield this avoids rather than picks a side on).
    #[test]
    fn proxy_command_emits_safe_path_and_profile_bare() {
        assert_eq!(
            proxy_command(Path::new("/usr/local/bin/isekai-pipe"), "prod-host:22", Path::new("/usr/bin/ssh")),
            "/usr/local/bin/isekai-pipe connect --profile prod-host:22 --service ssh --stdio"
        );
    }

    /// Regression test for the bug an MSYS2 user hit in practice: a
    /// short-pathed Windows path emitted bare gets every backslash silently
    /// eaten by bash's escape handling (`\U`, `\c`, ... before an ordinary
    /// character just drop the backslash). `quote_proxy_command_path`'s
    /// `force_posix_quoting` flag (set by `proxy_command` via
    /// `is_posix_shell_ssh` whenever `openssh_path` is MSYS2/Cygwin-hosted)
    /// must always POSIX single-quote in that case, regardless of platform
    /// or whether the path would otherwise look "safe" to emit bare.
    #[test]
    fn quote_proxy_command_path_forces_posix_quoting_when_requested() {
        assert_eq!(
            quote_proxy_command_path(Path::new(r"C:\Users\cuzic\isekai-pipe.exe"), true),
            r"'C:\Users\cuzic\isekai-pipe.exe'"
        );
    }

    #[test]
    fn quote_proxy_command_path_emits_bare_when_posix_quoting_not_forced() {
        assert_eq!(quote_proxy_command_path(Path::new("/usr/local/bin/isekai-pipe"), false), "/usr/local/bin/isekai-pipe");
    }

    #[test]
    fn is_safe_bare_word_accepts_typical_profile_and_path_charset() {
        assert!(is_safe_bare_word("prod-host:22", &[]));
        assert!(is_safe_bare_word("user@host.example.com", &[]));
        assert!(is_safe_bare_word("/usr/local/bin/isekai-pipe", &['/']));
        assert!(is_safe_bare_word(r"C:\PROGRA~1\isekai-pipe.exe", &['\\']));
    }

    #[test]
    fn is_safe_bare_word_rejects_whitespace_and_shell_metacharacters() {
        assert!(!is_safe_bare_word("has space", &[]));
        assert!(!is_safe_bare_word("", &[]));
        assert!(!is_safe_bare_word("prod'host", &[]));
        assert!(!is_safe_bare_word("$(whoami)", &[]));
        assert!(!is_safe_bare_word("a;rm -rf /", &[]));
    }

    #[test]
    fn quote_proxy_command_arg_falls_back_to_shell_quote_when_unsafe() {
        assert_eq!(quote_proxy_command_arg("prod host"), shell_quote("prod host"));
        assert_eq!(quote_proxy_command_arg("safe-host"), "safe-host");
    }

    #[test]
    fn builds_connection_intent_from_persistent_profile() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_home, _restore) = crate::test_home::with_temp_home();

        let sample_trust = || HelperTrust {
            identity_pubkey: "pk".to_string(),
            trusted_helper_sha256: "sha".to_string(),
            trusted_helper_version: "0.1.0".to_string(),
            update_policy: UpdatePolicy::ExactDigestOnly,
            release_channel: None,
            last_via: None,
            trusted_at: "2026-07-04T00:00:00Z".to_string(),
            last_seen_at: "2026-07-04T00:00:00Z".to_string(),
            cached_relay_addr: "127.0.0.1:1234".to_string(),
            cached_cert_sha256: "ab".to_string(),
            cached_session_secret: "c2VjcmV0".to_string(),
            cached_stun_observed_addr: None,
        };
        let profiles_dir = default_profiles_dir().unwrap();
        for key in ["production:22", "distinctive:22"] {
            let profile = PersistentProfile::migrate_legacy_helper_trust(key, &sample_trust());
            write_persistent_profile(&profiles_dir, &profile).unwrap();
        }

        let resolution = WrapperResolution {
            openssh: OpenSshEffectiveConfig::default(),
            isekai: IsekaiConfig {
                enabled: true,
                bootstrap_policy: BootstrapPolicy::Auto,
                profile: "production".to_string(),
                remote_path: None,
                services: vec![ServiceSpec::ssh_target("127.0.0.1:22").unwrap()],
                bootstrap_candidates: Vec::new(),
                link_endpoints: vec!["https://link.example.com".to_string()],
                rendezvous: vec!["https://rendezvous.example.com".to_string()],
                stun_servers: vec!["stun1.example.com:3478".to_string()],
                relay_endpoints: vec!["masque://relay.example.com".to_string()],
                resume_grace_secs: 180,
                candidate_race_delay_ms: 150,
                relay_delay_ms: 750,
                install_mode: InstallMode::User,
                bootstrap_relay: None,
                ctl_socket_enabled: false,
                tab_idle_color: None,
                tab_attention_color: None,
                remote_log_level: "info".to_string(),
                remote_bind_port_range: None,
                local_bind_port_range: None,
                tty: None,
            },
        };
        let intent = build_connection_intent(&resolution).unwrap();

        assert_eq!(intent.profile, "production");
        assert_eq!(intent.service, "ssh");
        assert_eq!(intent.link_endpoints, vec!["https://link.example.com"]);
        assert_eq!(intent.rendezvous, vec!["https://rendezvous.example.com"]);
        assert_eq!(intent.stun_servers, vec!["stun1.example.com:3478"]);
        assert_eq!(intent.relay_endpoints, vec!["masque://relay.example.com"]);
        assert_eq!(intent.resume_grace_secs, 180);
        assert_eq!(intent.candidate_race_delay_ms, 150);
        assert_eq!(intent.relay_delay_ms, 750);
        assert_eq!(
            intent.transport,
            IntentTransport::Relay {
                helper_addr: "127.0.0.1:1234".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string()
            }
        );

        // Regression-prevention contract check (ChatGPT second opinion,
        // 2026-07-08): this project has twice shipped a `#@isekai` directive
        // that parsed and reached `IsekaiConfig`/`IsekaiConfigBuilder` but was
        // silently dropped before ever reaching `ConnectionIntent` or the
        // actual connection (`remote-path` and `resume-grace`, both before
        // their respective fixes landed). Reusing this test's own `home`/
        // trust-store fixture (rather than a second test function) avoids a
        // cross-thread race on the process-global `$HOME` env var that a
        // separate `#[test]` mutating it concurrently would hit.
        //
        // Every directive must be accounted for by *exactly one* of:
        //   (a) asserted below to change `build_connection_intent`'s output
        //       when the directive's value changes, or
        //   (b) verified elsewhere (named in the table) to change some other
        //       concrete downstream behavior, for a stated reason it is
        //       deliberately NOT part of `ConnectionIntent`.
        //
        // | directive              | consumed by                                                                     |
        // |------------------------|----------------------------------------------------------------------------------|
        // | `enabled`              | `run()`'s own branch — controls the wrapper, not a `ConnectionIntent` field       |
        // | `bootstrap-policy`     | `should_bootstrap()` — controls auto-bootstrap, not a `ConnectionIntent` field    |
        // | `profile`              | (a) `intent.profile`                                                              |
        // | `remote-path`          | `bootstrap_and_register` (bootstrap-time only; see `wrapper_auto_bootstrap_honors_remote_path_directive` e2e test) |
        // | `service`              | (a) `intent.service`                                                              |
        // | `bootstrap-candidate`  | `bootstrap_and_register`'s candidate selection (bootstrap-time only; no `ConnectionIntent` field exists for it) |
        // | `link`                 | (a) `intent.link_endpoints`                                                       |
        // | `rendezvous`           | (a) `intent.rendezvous`                                                           |
        // | `stun`                 | (a) `intent.stun_servers`                                                         |
        // | `relay`                | (a) `intent.relay_endpoints`                                                      |
        // | `resume-grace`         | (a) `intent.resume_grace_secs`                                                    |
        // | `candidate-race-delay` | (a) `intent.candidate_race_delay_ms`                                              |
        // | `relay-delay`          | (a) `intent.relay_delay_ms`                                                       |
        // | `install-mode`         | `resolve_isekai_config`'s fail-closed check for `system` (see `install_mode_system_is_rejected_at_config_resolution`); `user` needs no plumbing (already the only implemented behavior) |
        // | `remote-log-level`     | `bootstrap_and_register` (bootstrap-time only; `isekai-helper --log-level`, no `ConnectionIntent` field exists for it) |
        // | `remote-bind-port-range` | `bootstrap_and_register` (bootstrap-time only; `isekai-helper --bind-port-range`, no `ConnectionIntent` field exists for it) |
        // | `local-bind-port-range` | (a) `intent.local_bind_port_range`                                                |
        // | `tab-idle-color`       | `ctl_forward.rs`'s `build_login_shell_command` (opt-in env var export at connect time only; no `ConnectionIntent` field exists for it — same category as the pre-existing `ctl-socket`/`bootstrap-relay` gaps in this table) |
        // | `tab-attention-color`  | ditto (`tab-idle-color`'s counterpart)                                            |
        // | `tty`                  | `resolved_tty_selection` (opt-in remote-command substitution at connect time only; no `ConnectionIntent` field exists for it — same category as `ctl-socket`/tab colors above; see `tty_directive_*` tests in `wrapper/config.rs` and this module) |
        //
        // If a new directive is ever added to `apply_isekai_directive`
        // without a corresponding row above (and without extending whichever
        // verification mechanism applies), that omission is itself the exact
        // class of bug this check exists to catch.
        let distinctive_isekai = IsekaiConfig {
            profile: "distinctive".to_string(),
            services: vec![ServiceSpec::parse("postgres=127.0.0.1:5432").unwrap()],
            link_endpoints: vec!["https://distinctive.example.com".to_string()],
            rendezvous: vec!["https://distinctive-rendezvous.example.com".to_string()],
            stun_servers: vec!["distinctive-stun.example.com:3478".to_string()],
            relay_endpoints: vec!["masque://distinctive-relay.example.com".to_string()],
            resume_grace_secs: 999,
            candidate_race_delay_ms: 987,
            relay_delay_ms: 8765,
            local_bind_port_range: Some((40100, 40200)),
            ..resolution.isekai.clone()
        };
        let distinctive_intent = build_connection_intent(&WrapperResolution {
            openssh: OpenSshEffectiveConfig::default(),
            isekai: distinctive_isekai,
        })
        .unwrap();

        assert_ne!(intent.profile, distinctive_intent.profile, "profile directive");
        assert_ne!(intent.service, distinctive_intent.service, "service directive");
        assert_ne!(intent.link_endpoints, distinctive_intent.link_endpoints, "link directive");
        assert_ne!(intent.rendezvous, distinctive_intent.rendezvous, "rendezvous directive");
        assert_ne!(intent.stun_servers, distinctive_intent.stun_servers, "stun directive");
        assert_ne!(intent.relay_endpoints, distinctive_intent.relay_endpoints, "relay directive");
        assert_ne!(
            intent.resume_grace_secs, distinctive_intent.resume_grace_secs,
            "resume-grace directive"
        );
        assert_ne!(
            intent.candidate_race_delay_ms, distinctive_intent.candidate_race_delay_ms,
            "candidate-race-delay directive"
        );
        assert_ne!(intent.relay_delay_ms, distinctive_intent.relay_delay_ms, "relay-delay directive");
        assert_ne!(
            intent.local_bind_port_range, distinctive_intent.local_bind_port_range,
            "local-bind-port-range directive"
        );
    }

    fn sample_trust_with_stun_observed(stun_observed: Option<&str>) -> HelperTrust {
        HelperTrust {
            identity_pubkey: "pk".to_string(),
            trusted_helper_sha256: "sha".to_string(),
            trusted_helper_version: "0.1.0".to_string(),
            update_policy: UpdatePolicy::ExactDigestOnly,
            release_channel: None,
            last_via: None,
            trusted_at: "2026-07-04T00:00:00Z".to_string(),
            last_seen_at: "2026-07-04T00:00:00Z".to_string(),
            cached_relay_addr: "127.0.0.1:1234".to_string(),
            cached_cert_sha256: "ab".to_string(),
            cached_session_secret: "c2VjcmV0".to_string(),
            cached_stun_observed_addr: stun_observed.map(str::to_string),
        }
    }

    #[test]
    fn select_transport_prefers_stun_p2p_when_evidence_and_config_both_exist() {
        let trust = sample_trust_with_stun_observed(Some("198.51.100.7:45231"));
        let profile = PersistentProfile::migrate_legacy_helper_trust("stun-host:22", &trust);
        let stun_servers = vec!["stun1.example.com:3478".to_string(), "stun2.example.com:3478".to_string()];

        let (transport, fallback) = select_transport(&profile, &stun_servers).unwrap();

        assert_eq!(
            transport,
            IntentTransport::StunP2p {
                stun_server: "stun1.example.com:3478".to_string(),
                peer_addr: "198.51.100.7:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            }
        );
        // STUN P2P is primary, but the relay transport that would otherwise
        // have been chosen outright becomes the cross-family fallback
        // instead of being discarded (`ISEKAI_PIPE_DESIGN.md` §8 Epic I).
        assert_eq!(
            fallback,
            Some(IntentTransport::Relay {
                helper_addr: "127.0.0.1:1234".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            })
        );
    }

    #[test]
    fn select_transport_falls_back_to_direct_relay_shape_without_stun_evidence() {
        let trust = sample_trust_with_stun_observed(None);
        let profile = PersistentProfile::migrate_legacy_helper_trust("no-stun-host:22", &trust);
        let stun_servers = vec!["stun1.example.com:3478".to_string()];

        let (transport, fallback) = select_transport(&profile, &stun_servers).unwrap();

        assert_eq!(
            transport,
            IntentTransport::Relay {
                helper_addr: "127.0.0.1:1234".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            }
        );
        assert_eq!(fallback, None, "relay is already primary — no further family to fall back to");
    }

    #[test]
    fn select_transport_falls_back_to_direct_when_stun_not_configured_even_with_evidence() {
        // Bootstrap-time STUN candidate exchange happened at some point in
        // the past (evidence is cached), but the operator hasn't opted in
        // via `#@isekai stun` for *this* connection attempt — must not use
        // STUN unasked.
        let trust = sample_trust_with_stun_observed(Some("198.51.100.7:45231"));
        let profile = PersistentProfile::migrate_legacy_helper_trust("stun-host:22", &trust);

        let (transport, fallback) = select_transport(&profile, &[]).unwrap();

        assert_eq!(
            transport,
            IntentTransport::Relay {
                helper_addr: "127.0.0.1:1234".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            }
        );
        assert_eq!(fallback, None);
    }

    #[test]
    fn build_connection_intent_selects_stun_p2p_when_profile_has_observed_address() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_home, _restore) = crate::test_home::with_temp_home();

        let trust = sample_trust_with_stun_observed(Some("198.51.100.7:45231"));
        let profile = PersistentProfile::migrate_legacy_helper_trust("stun-host:22", &trust);
        write_persistent_profile(&default_profiles_dir().unwrap(), &profile).unwrap();

        let resolution = WrapperResolution {
            openssh: OpenSshEffectiveConfig::default(),
            isekai: IsekaiConfig {
                enabled: true,
                bootstrap_policy: BootstrapPolicy::Auto,
                profile: "stun-host".to_string(),
                remote_path: None,
                services: vec![ServiceSpec::ssh_target("127.0.0.1:22").unwrap()],
                bootstrap_candidates: Vec::new(),
                link_endpoints: Vec::new(),
                rendezvous: Vec::new(),
                stun_servers: vec!["stun1.example.com:3478".to_string()],
                relay_endpoints: Vec::new(),
                resume_grace_secs: 120,
                candidate_race_delay_ms: 150,
                relay_delay_ms: 750,
                install_mode: InstallMode::User,
                bootstrap_relay: None,
                ctl_socket_enabled: false,
                tab_idle_color: None,
                tab_attention_color: None,
                remote_log_level: "info".to_string(),
                remote_bind_port_range: None,
                local_bind_port_range: None,
                tty: None,
            },
        };

        let intent = build_connection_intent(&resolution).unwrap();
        assert_eq!(
            intent.transport,
            IntentTransport::StunP2p {
                stun_server: "stun1.example.com:3478".to_string(),
                peer_addr: "198.51.100.7:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            }
        );
        assert_eq!(
            intent.cross_family_fallback,
            Some(IntentTransport::Relay {
                helper_addr: "127.0.0.1:1234".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            }),
            "the connection intent should carry the relay transport as a cross-family fallback"
        );
    }

    #[test]
    fn parses_ssh_g_output() {
        let config = parse_ssh_g_output(
            "user deploy\nhostname 10.20.0.15\nport 2222\nproxyjump bastion,edge\nproxycommand none\n",
        )
        .unwrap();
        assert_eq!(config.user.as_deref(), Some("deploy"));
        assert_eq!(config.hostname.as_deref(), Some("10.20.0.15"));
        assert_eq!(config.port, Some(2222));
        assert_eq!(config.proxy_jump.as_deref(), Some("bastion,edge"));
        assert_eq!(config.proxy_command, None);
    }

    #[test]
    fn resolves_isekai_directives_from_matching_host_block() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("ssh_config");
        std::fs::write(
            &config,
            r#"
Host production
    #@isekai profile production-east
    #@isekai bootstrap-candidate target=10.20.0.15:22 via=bastion,edge priority=120
    #@isekai remote-path ~/.local/bin/isekai-pipe
    #@isekai service ssh=127.0.0.1:2222
    #@isekai link https://link.example.com
    #@isekai rendezvous https://rendezvous.example.com
    #@isekai stun stun1.example.com:3478
    #@isekai relay masque://relay.example.com
    #@isekai bootstrap-relay addr=203.0.113.10:443 sni=relay.example.com
    #@isekai resume-grace 180s
    #@isekai candidate-race-delay 250ms
    #@isekai relay-delay 900ms
    #@isekai install-mode user

Host *
    #@isekai service postgres=127.0.0.1:5432
"#,
        )
        .unwrap();
        let plan = parse_wrapper(s(&["-F", config.to_str().unwrap(), "production"])).unwrap();
        let openssh = OpenSshEffectiveConfig {
            hostname: Some("10.20.0.15".to_string()),
            port: Some(22),
            ..Default::default()
        };
        let resolved = resolve_isekai_config(&plan, &openssh).unwrap();
        assert_eq!(resolved.profile, "production-east");
        assert_eq!(
            resolved.remote_path.as_deref(),
            Some("~/.local/bin/isekai-pipe")
        );
        assert_eq!(resolved.services.len(), 2);
        assert_eq!(
            resolved.services[0],
            ServiceSpec::ssh_target("127.0.0.1:2222").unwrap()
        );
        assert_eq!(
            resolved.bootstrap_candidates,
            vec![BootstrapCandidate {
                target: "10.20.0.15:22".to_string(),
                via: s(&["bastion", "edge"]),
                priority: 120,
                alias: None,
            }]
        );
        assert_eq!(resolved.link_endpoints, vec!["https://link.example.com"]);
        assert_eq!(resolved.rendezvous, vec!["https://rendezvous.example.com"]);
        assert_eq!(resolved.stun_servers, vec!["stun1.example.com:3478"]);
        assert_eq!(resolved.relay_endpoints, vec!["masque://relay.example.com"]);
        assert_eq!(
            resolved.bootstrap_relay,
            Some(BootstrapRelayTarget {
                relay_addr: "203.0.113.10:443".parse().unwrap(),
                relay_sni: "relay.example.com".to_string(),
                relay_transport: RelayTransportKind::Udp,
            })
        );
        assert_eq!(resolved.resume_grace_secs, 180);
        assert_eq!(resolved.candidate_race_delay_ms, 250);
        assert_eq!(resolved.relay_delay_ms, 900);
        assert_eq!(resolved.install_mode, InstallMode::User);
    }

    /// The actual motivating scenario `TtyDirective::Off` exists for: a
    /// broad `Host *` block turns tty-attach on for every host, one specific
    /// host opts back out. `ssh_config(5)`'s first-match-wins semantics mean
    /// this only works if the specific `Host` block is listed *before*
    /// `Host *` — end to end through `resolve_isekai_config`, not just the
    /// lower-level `apply_isekai_directive`/`resolved_tty_selection` unit
    /// tests, which pin `set_once` and the CLI/config merge in isolation but
    /// don't exercise real multi-`Host`-block file parsing.
    #[test]
    fn a_specific_host_can_opt_out_of_a_broader_tty_auto_default() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("ssh_config");
        std::fs::write(
            &config,
            r#"
Host noisy-legacy-box
    #@isekai tty off

Host *
    #@isekai tty auto
"#,
        )
        .unwrap();

        let opted_out = parse_wrapper(s(&["-F", config.to_str().unwrap(), "noisy-legacy-box"])).unwrap();
        let resolved = resolve_isekai_config(&opted_out, &OpenSshEffectiveConfig::default()).unwrap();
        assert_eq!(resolved.tty, Some(TtyDirective::Off));
        assert_eq!(resolved_tty_selection(&opted_out, &WrapperResolution { openssh: OpenSshEffectiveConfig::default(), isekai: resolved }), None);

        let everyone_else = parse_wrapper(s(&["-F", config.to_str().unwrap(), "some-other-host"])).unwrap();
        let resolved = resolve_isekai_config(&everyone_else, &OpenSshEffectiveConfig::default()).unwrap();
        assert_eq!(resolved.tty, Some(TtyDirective::Auto));
        assert_eq!(
            resolved_tty_selection(&everyone_else, &WrapperResolution { openssh: OpenSshEffectiveConfig::default(), isekai: resolved }),
            Some(TtySelection::Auto)
        );
    }

    #[test]
    fn dash_f_config_path_finds_an_explicit_dash_f_before_the_destination() {
        assert_eq!(
            dash_f_config_path(&s(&["-F", "/tmp/cfg", "production"])),
            Some(PathBuf::from("/tmp/cfg"))
        );
    }

    #[test]
    fn dash_f_config_path_skips_over_other_value_taking_flags() {
        assert_eq!(
            dash_f_config_path(&s(&["-p", "2222", "-F", "/tmp/cfg", "production"])),
            Some(PathBuf::from("/tmp/cfg"))
        );
    }

    #[test]
    fn dash_f_config_path_returns_none_without_dash_f() {
        assert_eq!(dash_f_config_path(&s(&["-p", "2222", "production"])), None);
    }

    /// Codex review finding: `resolve_for_native` used to always read
    /// `~/.ssh/config`, silently ignoring an explicit `-F <path>` — the same
    /// flag `resolve_openssh_effective_config` already forwards to real
    /// `ssh -G` on the Unix path. This never touches the real `$HOME` (the
    /// `-F` branch calls `openssh_config::resolve` directly on the given
    /// path), so it's deterministic regardless of what's in the sandbox's
    /// own `~/.ssh/config`.
    #[test]
    fn resolve_for_native_honors_an_explicit_dash_f_config_path() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("prod_ssh_config");
        std::fs::write(
            &config,
            "Host production\n    HostName 10.20.0.15\n    User deploy\n    Port 2222\n",
        )
        .unwrap();
        let plan = parse_wrapper(s(&["-F", config.to_str().unwrap(), "production"])).unwrap();

        let (resolution, host_config) = resolve_for_native(&plan).unwrap();

        assert_eq!(host_config.host_name.as_deref(), Some("10.20.0.15"));
        assert_eq!(host_config.user.as_deref(), Some("deploy"));
        assert_eq!(host_config.port, Some(2222));
        assert_eq!(resolution.native_host_port("production"), ("10.20.0.15".to_string(), 2222));
    }

    /// Codex review finding: `resolve_for_native` used to ignore every
    /// command-line override except `-F` — `-p`/`-l`/`-J`/`-o` were silently
    /// dropped, so `isekai-ssh -p 2222 host` on the native path connected to
    /// the config-file (or default) port instead. Command-line options must
    /// beat the config file, matching real `ssh(1)` precedence.
    #[test]
    fn resolve_for_native_applies_command_line_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("prod_ssh_config");
        std::fs::write(
            &config,
            "Host production\n    HostName 10.20.0.15\n    User deploy\n    Port 2222\n    ProxyJump bastion\n",
        )
        .unwrap();
        let plan = parse_wrapper(s(&[
            "-F",
            config.to_str().unwrap(),
            "-p",
            "2200",
            "-l",
            "root",
            "-J",
            "gateway",
            "production",
        ]))
        .unwrap();

        let (resolution, host_config) = resolve_for_native(&plan).unwrap();

        // Config file supplies HostName; -p/-l/-J win over Port/User/ProxyJump.
        assert_eq!(host_config.host_name.as_deref(), Some("10.20.0.15"));
        assert_eq!(host_config.port, Some(2200));
        assert_eq!(host_config.user.as_deref(), Some("root"));
        assert_eq!(host_config.proxy_jump.as_deref(), Some("gateway"));
        assert_eq!(resolution.native_host_port("production"), ("10.20.0.15".to_string(), 2200));
    }

    /// `-o Key=Value` overrides the matched config-file keyword too, and
    /// command-line entries win when both name the same key (`-p` set first
    /// beats a later `-o Port=`, following `ssh(1)`'s first-wins ordering).
    #[test]
    fn resolve_for_native_applies_dash_o_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("prod_ssh_config");
        std::fs::write(
            &config,
            "Host production\n    HostName 10.20.0.15\n    Port 22\n",
        )
        .unwrap();
        let plan = parse_wrapper(s(&[
            "-F",
            config.to_str().unwrap(),
            "-p",
            "2200",
            "-o",
            "Port=3300",
            "-o",
            "User=admin",
            "-o",
            "HostName=192.0.2.7",
            "production",
        ]))
        .unwrap();

        let (resolution, host_config) = resolve_for_native(&plan).unwrap();

        // -p 2200 was seen before -o Port=3300, so it wins (first occurrence).
        assert_eq!(host_config.port, Some(2200));
        assert_eq!(host_config.user.as_deref(), Some("admin"));
        assert_eq!(host_config.host_name.as_deref(), Some("192.0.2.7"));
        assert_eq!(resolution.native_host_port("production"), ("192.0.2.7".to_string(), 2200));
    }

    /// Regression (ultrareview): `-i <keyfile>` was silently dropped on the
    /// native path (no arm in `collect_cli_overrides`'s match), even though
    /// `ssh_option_width` already knew it takes a value — the Unix path
    /// forwards `-i` to real `ssh(1)` and it worked there, making this a
    /// native-only regression for a very common flag. Tilde must also expand.
    #[test]
    fn resolve_for_native_applies_dash_i_identity_override() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("prod_ssh_config");
        std::fs::write(&config, "Host production\n    HostName 10.20.0.15\n").unwrap();
        let plan = parse_wrapper(s(&["-F", config.to_str().unwrap(), "-i", "~/.ssh/deploy_key", "production"])).unwrap();

        let (_resolution, host_config) = resolve_for_native(&plan).unwrap();

        let home = isekai_fs_guard::resolve_home_dir().unwrap();
        assert_eq!(host_config.identity_file, vec![home.join(".ssh/deploy_key")], "-i must be tilde-expanded and added as a candidate");
    }

    /// Regression (ultrareview): `apply_dash_o_override` split on the first
    /// `=` *or* whitespace, so `-o "Port = 2222"` (spaces around `=`) split
    /// into key="Port", val="= 2222" — the stray leading `=` then failed
    /// `Port`'s `u16` parse. Must match `openssh_config::split_keyword`'s
    /// two-step handling (split, then strip a leading `=`).
    #[test]
    fn resolve_for_native_dash_o_tolerates_spaces_around_equals() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("prod_ssh_config");
        std::fs::write(&config, "Host production\n    HostName 10.20.0.15\n").unwrap();
        let plan = parse_wrapper(s(&["-F", config.to_str().unwrap(), "-o", "Port = 2222", "production"])).unwrap();

        let (_resolution, host_config) = resolve_for_native(&plan).unwrap();

        assert_eq!(host_config.port, Some(2222));
    }

    /// Regression (ultrareview): `-o IdentityFile=~/...` was pushed verbatim
    /// (including a literal leading `~`), so `read_credential`'s `fs::read`
    /// on a path beginning with `~` always failed — unlike the config-file
    /// path, which does expand `~` via `openssh_config`'s own resolver.
    #[test]
    fn resolve_for_native_dash_o_identity_file_expands_tilde() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("prod_ssh_config");
        std::fs::write(&config, "Host production\n    HostName 10.20.0.15\n").unwrap();
        let plan =
            parse_wrapper(s(&["-F", config.to_str().unwrap(), "-o", "IdentityFile=~/.ssh/deploy_key", "production"])).unwrap();

        let (_resolution, host_config) = resolve_for_native(&plan).unwrap();

        let home = isekai_fs_guard::resolve_home_dir().unwrap();
        assert_eq!(host_config.identity_file, vec![home.join(".ssh/deploy_key")]);
    }

    // parse_bootstrap_relay_*: moved to `wrapper/config.rs`'s own test
    // module alongside the function itself.

    #[test]
    fn install_mode_system_is_rejected_at_config_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("ssh_config");
        std::fs::write(
            &config,
            "Host production\n    #@isekai profile production\n    #@isekai install-mode system\n",
        )
        .unwrap();
        let plan = parse_wrapper(s(&["-F", config.to_str().unwrap(), "production"])).unwrap();
        let openssh = OpenSshEffectiveConfig {
            hostname: Some("10.20.0.15".to_string()),
            port: Some(22),
            ..Default::default()
        };
        let err = resolve_isekai_config(&plan, &openssh).unwrap_err();
        assert!(
            err.to_string().contains("install-mode system"),
            "expected a fail-closed error mentioning install-mode system, got: {err}"
        );
    }

    #[test]
    fn defaults_bootstrap_candidate_from_ssh_g() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("ssh_config");
        std::fs::write(
            &config,
            "Host production\n    #@isekai profile production\n",
        )
        .unwrap();
        let plan = parse_wrapper(s(&["-F", config.to_str().unwrap(), "production"])).unwrap();
        let openssh = OpenSshEffectiveConfig {
            hostname: Some("10.20.0.15".to_string()),
            port: Some(2200),
            proxy_jump: Some("bastion".to_string()),
            ..Default::default()
        };
        let resolved = resolve_isekai_config(&plan, &openssh).unwrap();
        assert_eq!(
            resolved.bootstrap_candidates,
            vec![BootstrapCandidate {
                target: "10.20.0.15:2200".to_string(),
                via: s(&["bastion"]),
                priority: 100,
                alias: Some("production".to_string()),
            }]
        );
        assert_eq!(resolved.link_endpoints, Vec::<String>::new());
        assert_eq!(
            resolved.candidate_race_delay_ms,
            DEFAULT_CANDIDATE_RACE_DELAY_MS
        );
        assert_eq!(resolved.relay_delay_ms, DEFAULT_RELAY_DELAY_MS);
        assert_eq!(resolved.install_mode, InstallMode::User);
    }
}
