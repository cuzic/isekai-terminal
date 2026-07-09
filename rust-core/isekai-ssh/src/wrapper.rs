//! Minimal OpenSSH frontend for the `chatgpt.md` migration path.
//!
//! `init`/`login`/`logout` remain as the interactive trust-store
//! subcommands. A non-subcommand invocation, such as `isekai-ssh
//! production`, is treated as an OpenSSH invocation with an injected
//! `ProxyCommand` that delegates the byte stream to `isekai-pipe connect`.

use std::collections::HashSet;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use isekai_auth::TokenProvider;
use isekai_bootstrap::{BootstrapBackend, HostSpec, JumpSpec, LaunchSpec, OpenSshBackend, RelayLaunchSpec};
use isekai_bootstrap_plan::{classify_bootstrap_error, BootstrapFailure};
use isekai_pipe_core::{
    default_profiles_dir, default_runtime_dir, load_persistent_profile, write_connection_intent,
    write_persistent_profile, BootstrapProvenance, ConnectionIntent, IntentTransport, PersistentProfile,
    ServiceSpec, DEFAULT_CANDIDATE_RACE_DELAY_MS, DEFAULT_RELAY_DELAY_MS,
};
use isekai_trust::{HelperTrust, UpdatePolicy};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

const LEGACY_SUBCOMMANDS: &[&str] = &["init", "login", "logout"];

/// Matches `isekai-ssh init`'s own default (`cli::InitArgs::idle_lifetime`):
/// the auto-bootstrapped helper is expected to keep running across many
/// separate `isekai-ssh <destination>` invocations, possibly hours/days
/// apart, unlike `isekai-terminal-core`'s (Android's) per-session bootstrap.
const DEFAULT_IDLE_LIFETIME_SECS: u64 = 2_592_000;

#[derive(Debug, PartialEq, Eq)]
struct WrapperPlan {
    openssh_path: PathBuf,
    pipe_path: PathBuf,
    destination: String,
    destination_index: usize,
    ssh_args: Vec<String>,
    isekai: WrapperIsekaiOptions,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallMode {
    User,
    System,
}

#[derive(Debug, Clone)]
struct WrapperResolution {
    openssh: OpenSshEffectiveConfig,
    isekai: IsekaiConfig,
}

pub fn should_run_wrapper(args: &[String]) -> bool {
    let Some(first) = args.first().map(String::as_str) else {
        return false;
    };
    !matches!(first, "-h" | "--help" | "help" | "-V" | "--version")
        && !LEGACY_SUBCOMMANDS.contains(&first)
}

pub async fn run(args: Vec<String>) -> Result<u8> {
    let plan = parse_wrapper(args)?;
    if plan.isekai.direct {
        return run_openssh_direct(&plan).await;
    }
    let resolution = resolve_wrapper(&plan).await?;
    if !resolution.isekai.enabled {
        return run_openssh_direct(&plan).await;
    }
    if plan.isekai.explain || plan.isekai.dry_run {
        eprintln!(
            "isekai-ssh: resolved OpenSSH config: {:?}",
            resolution.openssh
        );
        eprintln!(
            "isekai-ssh: resolved isekai config: {:?}",
            resolution.isekai
        );
        if plan.isekai.dry_run {
            return Ok(0);
        }
    }
    let intent = match build_connection_intent(&resolution) {
        Ok(intent) => intent,
        Err(err) if should_bootstrap(&plan, &resolution) => {
            if let Err(bootstrap_err) = bootstrap_and_register(&plan, &resolution).await {
                print_bootstrap_failure_guidance(&bootstrap_err);
                return Err(bootstrap_err.context(format!("{err}\nisekai-ssh: auto-bootstrap failed")));
            }
            build_connection_intent(&resolution)
                .context("isekai-ssh: still not trusted after auto-bootstrap")?
        }
        Err(err) => return Err(err),
    };
    let runtime_dir = default_runtime_dir()?;
    write_connection_intent(&runtime_dir, &intent)?;
    let proxy_command = proxy_command(&plan.pipe_path, &resolution.isekai.profile);

    let mut command = Command::new(&plan.openssh_path);
    command
        .env("ISEKAI_INTENT_ID", &intent.intent_id)
        .env("ISEKAI_PIPE_RUNTIME_DIR", &runtime_dir)
        .arg("-o")
        .arg(format!("ProxyCommand={proxy_command}"))
        .args(&plan.ssh_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = command.status().await.map_err(|e| {
        anyhow!(
            "isekai-ssh: failed to execute OpenSSH at {}: {e}",
            plan.openssh_path.display()
        )
    })?;
    Ok(status.code().unwrap_or(1) as u8)
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

fn build_connection_intent(resolution: &WrapperResolution) -> Result<ConnectionIntent> {
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
fn print_bootstrap_failure_guidance(err: &anyhow::Error) {
    let Some(failure) = err.downcast_ref::<BootstrapFailure>() else {
        return;
    };
    if failure.should_redirect_to_login() {
        eprintln!("isekai-ssh: {failure} — run `isekai-ssh login` and try again.");
    } else if failure.should_redirect_to_init() {
        eprintln!("isekai-ssh: {failure} — run `isekai-ssh init` to set up trust/credentials for this host.");
    } else if failure.may_retry() {
        eprintln!("isekai-ssh: {failure} — this looks transient; retrying may help.");
    }
}

/// Deploys `isekai-helper` to the highest-priority bootstrap candidate and,
/// after an explicit `[y/N]` confirmation, registers it in the trust store
/// `build_connection_intent` reads from. Mirrors `init.rs`'s
/// deploy-then-confirm-then-register flow, but triggered automatically by
/// `run()` on a trust-store miss instead of via the standalone `init`
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
/// `isekai_bootstrap_plan::BootstrapPlan::validate_jump_chain` cycle/hop-count
/// checks `init.rs` uses, then passed to `OpenSshBackend::install_and_start`
/// as a single `ssh(1)` `-J host1,host2,...` invocation, not nested `ssh`
/// executions per hop.
async fn bootstrap_and_register(plan: &WrapperPlan, resolution: &WrapperResolution) -> Result<()> {
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
    let target = HostSpec::new(host).with_port(port);

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
    isekai_bootstrap_plan::BootstrapPlan::validate_jump_chain(&target, &via)
        .with_context(|| format!("invalid --via chain {:?}", candidate.via))?;

    let backend = OpenSshBackend::new();
    let helper_binary_was_explicit = plan.isekai.helper_binary.is_some();
    let helper_binary = crate::helper_download::resolve_helper_binary(
        plan.isekai.helper_binary.as_deref(),
        &backend,
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
    let helper_sha256 = hex_sha256(&helper_binary);

    // `#@isekai stun` directives are collected as plain strings without
    // socket-address validation (`resolve_isekai_config`'s `append_args`
    // just accumulates whatever text follows the directive) — a malformed
    // entry is skipped with a warning here rather than failing the whole
    // auto-bootstrap over one bad directive (`#20b`).
    let stun_servers: Vec<SocketAddr> = resolution
        .isekai
        .stun_servers
        .iter()
        .filter_map(|entry| match entry.parse::<SocketAddr>() {
            Ok(addr) => Some(addr),
            Err(e) => {
                eprintln!("isekai-ssh: ignoring malformed #@isekai stun entry {entry:?}: {e}");
                None
            }
        })
        .collect();

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
                idle_lifetime_secs: DEFAULT_IDLE_LIFETIME_SECS,
            })
        }
        None => LaunchSpec::Direct { idle_lifetime_secs: DEFAULT_IDLE_LIFETIME_SECS },
    };

    eprintln!("isekai-ssh: {:?} is not trusted yet; deploying isekai-helper to {}...", resolution.isekai.profile, candidate.target);
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

    eprintln!();
    eprintln!("Host:            {}", candidate.target);
    if let Some(relay_target) = &resolution.isekai.bootstrap_relay {
        eprintln!("Relay:           {}", relay_target.relay_addr);
    }
    eprintln!("Helper identity: {identity}");
    eprintln!("Binary sha256:   {helper_sha256}");
    eprintln!();
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

    let profiles_dir =
        default_profiles_dir().context("could not determine the profiles directory (is $HOME set?)")?;
    let key = isekai_trust::normalize_host_port(&resolution.isekai.profile)
        .with_context(|| format!("invalid profile {:?}", resolution.isekai.profile))?;
    let now = now_rfc3339();
    let trust = HelperTrust {
        identity_pubkey: identity.clone(),
        trusted_helper_sha256: helper_sha256,
        trusted_helper_version: "unknown".to_string(),
        update_policy: UpdatePolicy::ExactDigestOnly,
        release_channel: None,
        last_via: (!candidate.via.is_empty()).then(|| candidate.via.join(",")),
        trusted_at: now.clone(),
        last_seen_at: now,
        cached_relay_addr,
        cached_cert_sha256: identity,
        cached_session_secret: handshake.session_secret.clone(),
        cached_stun_observed_addr: handshake.stun_observed_addr().map(str::to_string),
    };
    let profile = PersistentProfile::migrate_legacy_helper_trust(&key, &trust);
    let path = write_persistent_profile(&profiles_dir, &profile)
        .map_err(|e| {
            anyhow::Error::new(e)
                .context(BootstrapFailure::PersistenceFailed(format!("failed to write profile to {}", profiles_dir.display())))
        })?;
    eprintln!("Registered {key:?} in {}", path.display());
    Ok(())
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Current UTC time formatted as RFC 3339, matching `init.rs`'s own
/// `now_rfc3339`/`format_rfc3339_utc` (duplicated rather than shared across
/// two ~60-line modules for a single timestamp helper).
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_rfc3339_utc(secs)
}

fn format_rfc3339_utc(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let secs_of_day = unix_secs % 86_400;
    let (hour, minute, second) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);

    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn primary_service(config: &IsekaiConfig) -> &ServiceSpec {
    config
        .services
        .iter()
        .find(|service| service.name().as_str() == "ssh")
        .or_else(|| config.services.first())
        .expect("IsekaiConfig always has at least one service")
}

fn should_bootstrap(plan: &WrapperPlan, resolution: &WrapperResolution) -> bool {
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

fn parse_wrapper(args: Vec<String>) -> Result<WrapperPlan> {
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

    Ok(WrapperPlan {
        openssh_path,
        pipe_path,
        destination,
        destination_index,
        ssh_args,
        isekai,
    })
}

async fn resolve_wrapper(plan: &WrapperPlan) -> Result<WrapperResolution> {
    let openssh = resolve_openssh_effective_config(plan).await?;
    let isekai = resolve_isekai_config(plan, &openssh)?;
    Ok(WrapperResolution { openssh, isekai })
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

fn resolve_isekai_config(
    plan: &WrapperPlan,
    openssh: &OpenSshEffectiveConfig,
) -> Result<IsekaiConfig> {
    let directives = load_isekai_directives(plan)?;
    let default_target = format!(
        "{}:{}",
        openssh
            .hostname
            .as_deref()
            .unwrap_or(plan.destination.as_str()),
        openssh.port.unwrap_or(22)
    );
    let mut builder = IsekaiConfigBuilder {
        enabled: None,
        bootstrap_policy: None,
        profile: None,
        remote_path: None,
        services: Vec::new(),
        bootstrap_candidates: Vec::new(),
        link_endpoints: Vec::new(),
        rendezvous: Vec::new(),
        stun_servers: Vec::new(),
        relay_endpoints: Vec::new(),
        resume_grace_secs: None,
        candidate_race_delay_ms: None,
        relay_delay_ms: None,
        install_mode: None,
        bootstrap_relay: None,
    };
    for directive in directives {
        apply_isekai_directive(&mut builder, directive)?;
    }
    if builder.bootstrap_candidates.is_empty() {
        builder.bootstrap_candidates.push(BootstrapCandidate {
            target: default_target,
            via: openssh
                .proxy_jump
                .as_deref()
                .map(parse_jump_chain)
                .unwrap_or_default(),
            priority: 100,
        });
    }
    if builder.services.is_empty() {
        builder
            .services
            .push(ServiceSpec::ssh_target("127.0.0.1:22").expect("default service is valid"));
    }
    // `install-mode=system` needs sudo handling, ownership/permissions,
    // overwrite-of-an-existing-binary semantics, signature/hash verification,
    // and update/rollback — none of which exist yet. Rather than silently
    // wiring it through as if it were equivalent to `user` (or silently
    // ignoring it), fail closed here at config-resolution time so a typo'd or
    // aspirational `#@isekai install-mode system` never gets treated as
    // meaning something it doesn't (`ISEKAI_PIPE_DESIGN.md`).
    if builder.install_mode == Some(InstallMode::System) {
        return Err(anyhow!(
            "isekai-ssh: '#@isekai install-mode system' is not supported yet (no sudo/ownership/\
             signature-verification/rollback design exists) — remove the directive or use \
             'install-mode user'"
        ));
    }
    Ok(IsekaiConfig {
        enabled: builder.enabled.unwrap_or(true),
        bootstrap_policy: builder.bootstrap_policy.unwrap_or(BootstrapPolicy::Auto),
        profile: builder.profile.unwrap_or_else(|| plan.destination.clone()),
        remote_path: builder.remote_path,
        services: builder.services,
        bootstrap_candidates: builder.bootstrap_candidates,
        link_endpoints: builder.link_endpoints,
        rendezvous: builder.rendezvous,
        stun_servers: builder.stun_servers,
        relay_endpoints: builder.relay_endpoints,
        resume_grace_secs: builder.resume_grace_secs.unwrap_or(120),
        candidate_race_delay_ms: builder
            .candidate_race_delay_ms
            .unwrap_or(DEFAULT_CANDIDATE_RACE_DELAY_MS),
        relay_delay_ms: builder.relay_delay_ms.unwrap_or(DEFAULT_RELAY_DELAY_MS),
        install_mode: builder.install_mode.unwrap_or(InstallMode::User),
        bootstrap_relay: builder.bootstrap_relay,
    })
}

#[derive(Debug)]
struct IsekaiConfigBuilder {
    enabled: Option<bool>,
    bootstrap_policy: Option<BootstrapPolicy>,
    profile: Option<String>,
    remote_path: Option<String>,
    services: Vec<ServiceSpec>,
    bootstrap_candidates: Vec<BootstrapCandidate>,
    link_endpoints: Vec<String>,
    rendezvous: Vec<String>,
    stun_servers: Vec<String>,
    relay_endpoints: Vec<String>,
    bootstrap_relay: Option<BootstrapRelayTarget>,
    resume_grace_secs: Option<u64>,
    candidate_race_delay_ms: Option<u64>,
    relay_delay_ms: Option<u64>,
    install_mode: Option<InstallMode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IsekaiDirective {
    name: String,
    args: Vec<String>,
}

fn apply_isekai_directive(
    builder: &mut IsekaiConfigBuilder,
    directive: IsekaiDirective,
) -> Result<()> {
    match directive.name.as_str() {
        "enabled" => set_once(
            &mut builder.enabled,
            parse_yes_no(one_arg(&directive)?)?,
            "enabled",
        ),
        "bootstrap-policy" => set_once(
            &mut builder.bootstrap_policy,
            match one_arg(&directive)? {
                "auto" => BootstrapPolicy::Auto,
                "always" => BootstrapPolicy::Always,
                "never" => BootstrapPolicy::Never,
                other => {
                    return Err(anyhow!(
                        "isekai-ssh: invalid #@isekai bootstrap-policy {other:?}"
                    ))
                }
            },
            "bootstrap-policy",
        ),
        "profile" => set_once(
            &mut builder.profile,
            one_arg(&directive)?.to_string(),
            "profile",
        ),
        "remote-path" => set_once(
            &mut builder.remote_path,
            one_arg(&directive)?.to_string(),
            "remote-path",
        ),
        "service" => {
            for arg in &directive.args {
                builder.services.push(
                    ServiceSpec::parse(arg).map_err(|e| {
                        anyhow!("isekai-ssh: invalid #@isekai service {arg:?}: {e}")
                    })?,
                );
            }
            Ok(())
        }
        "bootstrap-candidate" => {
            builder
                .bootstrap_candidates
                .push(parse_bootstrap_candidate(&directive.args)?);
            Ok(())
        }
        "link" => append_args(&mut builder.link_endpoints, &directive),
        "rendezvous" => append_args(&mut builder.rendezvous, &directive),
        "stun" => append_args(&mut builder.stun_servers, &directive),
        "relay" => append_args(&mut builder.relay_endpoints, &directive),
        "resume-grace" => set_once(
            &mut builder.resume_grace_secs,
            parse_duration_ms(one_arg(&directive)?, "resume-grace")?.div_ceil(1000),
            "resume-grace",
        ),
        "candidate-race-delay" => set_once(
            &mut builder.candidate_race_delay_ms,
            parse_duration_ms(one_arg(&directive)?, "candidate-race-delay")?,
            "candidate-race-delay",
        ),
        "relay-delay" => set_once(
            &mut builder.relay_delay_ms,
            parse_duration_ms(one_arg(&directive)?, "relay-delay")?,
            "relay-delay",
        ),
        "bootstrap-relay" => set_once(
            &mut builder.bootstrap_relay,
            parse_bootstrap_relay(&directive.args)?,
            "bootstrap-relay",
        ),
        "install-mode" => set_once(
            &mut builder.install_mode,
            match one_arg(&directive)? {
                "user" => InstallMode::User,
                "system" => InstallMode::System,
                other => {
                    return Err(anyhow!(
                        "isekai-ssh: invalid #@isekai install-mode {other:?}"
                    ))
                }
            },
            "install-mode",
        ),
        other => Err(anyhow!("isekai-ssh: unknown #@isekai directive {other:?}")),
    }
}

fn append_args(target: &mut Vec<String>, directive: &IsekaiDirective) -> Result<()> {
    if directive.args.is_empty() {
        return Err(anyhow!(
            "isekai-ssh: #@isekai {} expects at least one argument",
            directive.name
        ));
    }
    target.extend(directive.args.iter().cloned());
    Ok(())
}

fn set_once<T>(slot: &mut Option<T>, value: T, name: &str) -> Result<()> {
    if slot.is_none() {
        *slot = Some(value);
    }
    let _ = name;
    Ok(())
}

fn one_arg(directive: &IsekaiDirective) -> Result<&str> {
    match directive.args.as_slice() {
        [single] => Ok(single),
        _ => Err(anyhow!(
            "isekai-ssh: #@isekai {} expects exactly one argument",
            directive.name
        )),
    }
}

fn parse_yes_no(value: &str) -> Result<bool> {
    match value {
        "yes" | "true" | "on" | "1" => Ok(true),
        "no" | "false" | "off" | "0" => Ok(false),
        _ => Err(anyhow!("isekai-ssh: expected yes/no, got {value:?}")),
    }
}

fn parse_duration_ms(value: &str, field: &str) -> Result<u64> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1000)
    } else {
        (value, 1000)
    };
    let amount: u64 = number
        .parse()
        .map_err(|_| anyhow!("isekai-ssh: invalid #@isekai {field} duration {value:?}"))?;
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("isekai-ssh: #@isekai {field} duration is too large"))
}

fn parse_bootstrap_candidate(args: &[String]) -> Result<BootstrapCandidate> {
    let mut target = None;
    let mut via = Vec::new();
    let mut priority = 100;
    for arg in args {
        let Some((key, value)) = arg.split_once('=') else {
            return Err(anyhow!(
                "isekai-ssh: bootstrap-candidate argument must be key=value: {arg:?}"
            ));
        };
        match key {
            "target" => target = Some(value.to_string()),
            "via" => via = parse_jump_chain(value),
            "priority" => {
                priority = value.parse().map_err(|_| {
                    anyhow!("isekai-ssh: invalid bootstrap-candidate priority {value:?}")
                })?;
            }
            _ => {
                return Err(anyhow!(
                    "isekai-ssh: unknown bootstrap-candidate key {key:?}"
                ))
            }
        }
    }
    Ok(BootstrapCandidate {
        target: target
            .ok_or_else(|| anyhow!("isekai-ssh: bootstrap-candidate requires target=..."))?,
        via,
        priority,
    })
}

fn parse_bootstrap_relay(args: &[String]) -> Result<BootstrapRelayTarget> {
    let mut relay_addr = None;
    let mut relay_sni = None;
    for arg in args {
        let Some((key, value)) = arg.split_once('=') else {
            return Err(anyhow!("isekai-ssh: bootstrap-relay argument must be key=value: {arg:?}"));
        };
        match key {
            "addr" => {
                relay_addr = Some(
                    value.parse::<SocketAddr>().map_err(|e| anyhow!("isekai-ssh: invalid bootstrap-relay addr {value:?}: {e}"))?,
                )
            }
            "sni" => {
                if value.is_empty() {
                    return Err(anyhow!("isekai-ssh: bootstrap-relay sni must not be empty"));
                }
                relay_sni = Some(value.to_string())
            }
            _ => return Err(anyhow!("isekai-ssh: unknown bootstrap-relay key {key:?}")),
        }
    }
    Ok(BootstrapRelayTarget {
        relay_addr: relay_addr.ok_or_else(|| anyhow!("isekai-ssh: bootstrap-relay requires addr=..."))?,
        relay_sni: relay_sni.ok_or_else(|| anyhow!("isekai-ssh: bootstrap-relay requires sni=..."))?,
    })
}

fn parse_jump_chain(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|hop| !hop.is_empty())
        .map(str::to_string)
        .collect()
}

fn load_isekai_directives(plan: &WrapperPlan) -> Result<Vec<IsekaiDirective>> {
    let roots = config_roots(plan);
    let mut visited = HashSet::new();
    let mut directives = Vec::new();
    for root in roots {
        if root.exists() {
            load_isekai_directives_from_file(
                &root,
                &plan.destination,
                &mut visited,
                &mut directives,
            )?;
        }
    }
    Ok(directives)
}

fn config_roots(plan: &WrapperPlan) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut i = 0;
    while i < plan.ssh_args.len() {
        match plan.ssh_args[i].as_str() {
            "-F" if i + 1 < plan.ssh_args.len() => {
                roots.push(expand_path(&plan.ssh_args[i + 1], None));
                i += 2;
            }
            "-F" => break,
            _ => i += ssh_option_width(plan.ssh_args[i].as_str()),
        }
    }
    if roots.is_empty() {
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join(".ssh").join("config"));
        }
    }
    roots
}

fn load_isekai_directives_from_file(
    path: &Path,
    destination: &str,
    visited: &mut HashSet<PathBuf>,
    directives: &mut Vec<IsekaiDirective>,
) -> Result<()> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return Ok(());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("isekai-ssh: failed to read ssh config {}", path.display()))?;
    let base_dir = path.parent();
    let mut active = true;
    let mut in_match = false;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#@isekai") {
            if in_match {
                return Err(anyhow!(
                    "ISEKAI_CONFIG_UNSUPPORTED_MATCH: #@isekai inside Match block"
                ));
            }
            if active {
                directives.push(parse_isekai_directive(rest.trim())?);
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let (keyword, rest) = split_keyword(line);
        match keyword.to_ascii_lowercase().as_str() {
            "include" => {
                for pattern in split_words(rest) {
                    for include in expand_include_pattern(&pattern, base_dir)? {
                        load_isekai_directives_from_file(
                            &include,
                            destination,
                            visited,
                            directives,
                        )?;
                    }
                }
            }
            "host" => {
                in_match = false;
                active = host_patterns_match(rest, destination);
            }
            "match" => {
                in_match = true;
                active = false;
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_isekai_directive(rest: &str) -> Result<IsekaiDirective> {
    let mut words = split_words(rest);
    let name = words
        .next()
        .ok_or_else(|| anyhow!("isekai-ssh: empty #@isekai directive"))?;
    Ok(IsekaiDirective {
        name,
        args: words.collect(),
    })
}

fn split_keyword(line: &str) -> (&str, &str) {
    match line.find(char::is_whitespace) {
        Some(index) => (&line[..index], line[index..].trim()),
        None => (line, ""),
    }
}

fn split_words(input: &str) -> impl Iterator<Item = String> + '_ {
    input.split_whitespace().map(str::to_string)
}

fn expand_include_pattern(pattern: &str, base_dir: Option<&Path>) -> Result<Vec<PathBuf>> {
    let expanded = expand_path(pattern, base_dir);
    let pattern = expanded.to_string_lossy().into_owned();
    let mut paths = Vec::new();
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        for entry in
            glob::glob(&pattern).with_context(|| format!("invalid Include pattern {pattern:?}"))?
        {
            paths.push(entry?);
        }
        paths.sort();
    } else {
        paths.push(PathBuf::from(pattern));
    }
    Ok(paths)
}

fn expand_path(input: &str, base_dir: Option<&Path>) -> PathBuf {
    let expanded = if input == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(input))
    } else if let Some(rest) = input.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| PathBuf::from(input))
    } else {
        PathBuf::from(input)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.unwrap_or_else(|| Path::new(".")).join(expanded)
    }
}

fn host_patterns_match(patterns: &str, destination: &str) -> bool {
    let mut matched = false;
    for pattern in patterns.split_whitespace() {
        if let Some(negative) = pattern.strip_prefix('!') {
            if wildcard_match(negative, destination) {
                return false;
            }
        } else if wildcard_match(pattern, destination) {
            matched = true;
        }
    }
    matched
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    wildcard_match_bytes(pattern.as_bytes(), value.as_bytes())
}

fn wildcard_match_bytes(pattern: &[u8], value: &[u8]) -> bool {
    match (pattern.split_first(), value.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some((&b'*', rest)), _) => {
            wildcard_match_bytes(rest, value)
                || value
                    .split_first()
                    .map(|(_, value_rest)| wildcard_match_bytes(pattern, value_rest))
                    .unwrap_or(false)
        }
        (Some((&b'?', rest)), Some((_, value_rest))) => wildcard_match_bytes(rest, value_rest),
        (Some((&p, rest)), Some((&v, value_rest))) if p == v => {
            wildcard_match_bytes(rest, value_rest)
        }
        _ => false,
    }
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

fn proxy_command(pipe_path: &Path, profile: &str) -> String {
    format!(
        "{} connect --profile {} --service ssh --stdio",
        shell_quote(&pipe_path.display().to_string()),
        shell_quote(profile)
    )
}

fn shell_quote(value: &str) -> String {
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
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return (i + 1 < args.len()).then_some(i + 1);
        }
        if !arg.starts_with('-') || arg == "-" {
            return Some(i);
        }
        i += ssh_option_width(arg);
    }
    None
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

    #[tokio::test]
    async fn bootstrap_and_register_classifies_missing_helper_binary() {
        let plan = WrapperPlan {
            openssh_path: PathBuf::from("/usr/bin/ssh"),
            pipe_path: PathBuf::from("/usr/bin/isekai-pipe"),
            destination: "production".to_string(),
            destination_index: 0,
            ssh_args: Vec::new(),
            isekai: WrapperIsekaiOptions::default(),
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
                bootstrap_candidates: vec![BootstrapCandidate { target: "127.0.0.1:1".to_string(), via: Vec::new(), priority: 0 }],
                link_endpoints: Vec::new(),
                rendezvous: Vec::new(),
                stun_servers: Vec::new(),
                relay_endpoints: Vec::new(),
                resume_grace_secs: 180,
                candidate_race_delay_ms: 150,
                relay_delay_ms: 750,
                install_mode: InstallMode::User,
                bootstrap_relay: None,
            },
        };

        // `plan.isekai.helper_binary` is `None` (the default): no explicit
        // path is given, `detect_remote_arch` fails against the unreachable
        // target above, and `resolve_helper_binary` surfaces that failure —
        // classified the same as the old "no --isekai-helper-binary given"
        // hard error used to be, since the practical guidance is identical
        // either way ("no local isekai-pipe binary to upload").
        let err = bootstrap_and_register(&plan, &resolution).await.unwrap_err();
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
            destination_index: 0,
            ssh_args: Vec::new(),
            isekai: WrapperIsekaiOptions::default(),
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
                bootstrap_candidates: vec![BootstrapCandidate { target: "production:22".to_string(), via, priority: 0 }],
                link_endpoints: Vec::new(),
                rendezvous: Vec::new(),
                stun_servers: Vec::new(),
                relay_endpoints: Vec::new(),
                resume_grace_secs: 180,
                candidate_race_delay_ms: 150,
                relay_delay_ms: 750,
                install_mode: InstallMode::User,
                bootstrap_relay: None,
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
        let err = bootstrap_and_register(&plan, &resolution).await.unwrap_err();
        let failure = err.downcast_ref::<BootstrapFailure>().expect("classified as a BootstrapFailure");
        assert!(matches!(failure, BootstrapFailure::RemoteBinaryMissing), "{failure:?}");
        assert!(format!("{err:#}").contains("nonexistent/isekai-helper"), "{err:#}");

        // A looping chain (repeats the destination, same host *and* port —
        // cycle detection is port-sensitive, matching `plan.rs`'s own
        // `distinct_ports_on_the_same_host_are_not_a_cycle`) is still
        // rejected, now via `isekai_bootstrap_plan::BootstrapPlan::validate_jump_chain`
        // rather than the old single-hop-only guard.
        let looping = resolution_with_via(vec!["bastion-a".to_string(), "production:22".to_string()]);
        let err = bootstrap_and_register(&plan, &looping).await.unwrap_err();
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
            proxy_command(Path::new("/opt/isekai pipe"), "prod'host"),
            "'/opt/isekai pipe' connect --profile 'prod'\\''host' --service ssh --stdio"
        );
    }

    #[test]
    fn builds_connection_intent_from_persistent_profile() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home =
            std::env::temp_dir().join(format!("isekai-ssh-wrapper-intent-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);

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

        if let Some(old_home) = old_home {
            std::env::set_var("HOME", old_home);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = std::fs::remove_dir_all(home);
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
        let home = std::env::temp_dir().join(format!("isekai-ssh-wrapper-stun-intent-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);

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

        if let Some(old_home) = old_home {
            std::env::set_var("HOME", old_home);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = std::fs::remove_dir_all(home);
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
            }]
        );
        assert_eq!(resolved.link_endpoints, vec!["https://link.example.com"]);
        assert_eq!(resolved.rendezvous, vec!["https://rendezvous.example.com"]);
        assert_eq!(resolved.stun_servers, vec!["stun1.example.com:3478"]);
        assert_eq!(resolved.relay_endpoints, vec!["masque://relay.example.com"]);
        assert_eq!(
            resolved.bootstrap_relay,
            Some(BootstrapRelayTarget { relay_addr: "203.0.113.10:443".parse().unwrap(), relay_sni: "relay.example.com".to_string() })
        );
        assert_eq!(resolved.resume_grace_secs, 180);
        assert_eq!(resolved.candidate_race_delay_ms, 250);
        assert_eq!(resolved.relay_delay_ms, 900);
        assert_eq!(resolved.install_mode, InstallMode::User);
    }

    #[test]
    fn parse_bootstrap_relay_accepts_addr_and_sni() {
        let target = parse_bootstrap_relay(&s(&["addr=203.0.113.10:443", "sni=relay.example.com"])).unwrap();
        assert_eq!(target, BootstrapRelayTarget { relay_addr: "203.0.113.10:443".parse().unwrap(), relay_sni: "relay.example.com".to_string() });
    }

    #[test]
    fn parse_bootstrap_relay_rejects_missing_addr() {
        assert!(parse_bootstrap_relay(&s(&["sni=relay.example.com"])).is_err());
    }

    #[test]
    fn parse_bootstrap_relay_rejects_missing_sni() {
        assert!(parse_bootstrap_relay(&s(&["addr=203.0.113.10:443"])).is_err());
    }

    #[test]
    fn parse_bootstrap_relay_rejects_invalid_addr() {
        assert!(parse_bootstrap_relay(&s(&["addr=not-an-addr", "sni=relay.example.com"])).is_err());
    }

    #[test]
    fn parse_bootstrap_relay_rejects_empty_sni() {
        assert!(parse_bootstrap_relay(&s(&["addr=203.0.113.10:443", "sni="])).is_err());
    }

    #[test]
    fn parse_bootstrap_relay_rejects_unknown_key() {
        assert!(parse_bootstrap_relay(&s(&["addr=203.0.113.10:443", "sni=relay.example.com", "jwt=abc"])).is_err());
    }

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
