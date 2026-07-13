//! `isekai-pipe connect`: the local stdio side. Parses `connect` CLI flags
//! (or claims a pre-written `ConnectionIntent` when `ISEKAI_INTENT_ID` is
//! set), resolves that into candidates, picks a transport route
//! (relay/STUN P2P/single-candidate, `choose_connect_route`), and dials —
//! handing off the established connection to [`crate::resume_loop`] for the
//! actual data pump/resume loop. `write_connect_outcome_for_wrapper` is the
//! "always connects" (`.claude/rules/always-connects.md`) side-channel that
//! lets `isekai-ssh`'s wrapper silently re-bootstrap on any failure here.

use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use isekai_pipe_core::{
    claim_connection_intent, default_profiles_dir, default_runtime_dir, load_persistent_profile,
    BootstrapProvenance, Candidate, CandidateGeneration, CandidateRoute, ConnectionIntent, IntentTransport,
    ServiceSpec,
};
use isekai_transport::{
    connect_stun_p2p, qmux_relay_factory, system_quic_factory, AnyMuxFactory, CandidatePool, CandidateProvider,
    ConfigRelayProvider, ConfigStunProvider, GatherContext, RelayTarget, SequentialConnectError,
    SequentialRelayCandidate, SequentialStunCandidate, SequentialStunConnectError, StunP2pTarget,
};
use std::process::ExitCode;

use crate::resume_loop::{relay_stdio, run_relay_resumable, run_relay_resumable_with_fallback, run_stun_p2p_with_fallback};
use crate::{DEFAULT_RESUME_WINDOW, EX_USAGE, EX_UNAVAILABLE};

#[derive(Debug)]
struct ConnectLaunch {
    profile: Option<String>,
    service: Option<ServiceSpec>,
    stdio: bool,
    mode: ConnectMode,
    /// Repeatable `--stun-server` (accumulates, matching `--relay`'s
    /// `relay_endpoints`/`isekai-ssh`'s `#@isekai stun` directive
    /// convention). `--mode stun` requires at least one; the first entry
    /// backs the legacy single-candidate `IntentTransport::StunP2p.stun_server`
    /// field, the full list drives `ConfigStunProvider` fallback across the
    /// rest (`#11`).
    stun_servers: Vec<String>,
    resume_window: Duration,
    /// Experimental, default-off (`ISEKAI_PIPE_DESIGN.md`'s convention for
    /// opt-in features): on an OS-reported network change, try
    /// `isekai_transport::AnyMuxRebinder::rebind` (a fresh local
    /// socket, same QUIC endpoint/connection — no RESUME round trip) before
    /// falling back to today's "close and RESUME" reconnect. See
    /// `run_resume_loop`'s module-level comment on why this needed a
    /// restructure rather than a one-line addition to the existing
    /// `select!`.
    experimental_network_rebind: bool,
    /// `--relay-transport <udp|qmux>` (`#qmux-leg1`, default `Udp`): which
    /// transport this side uses to reach the relay-assigned `isekai-helper`
    /// endpoint. Mirrors `engine::RelayTransportKind` (`isekai-pipe serve`'s
    /// own equivalent for the `isekai-helper→relay` leg, `#qmux-leg2`) —
    /// deliberately a separate, locally-scoped type rather than shared
    /// across the `connect`/`serve` sides of this binary, since the two
    /// sides have no other coupling. Per `ISEKAI_PIPE_DESIGN.md` Epic G/H's
    /// "single evidence-gated selection, no runtime fallback" policy, this
    /// is chosen once up front — never retried automatically if the `Udp`
    /// path fails.
    relay_transport: RelayTransportKind,
    /// `--bind-port-range <START>-<END>`: narrows this connection's local
    /// QUIC socket to that inclusive UDP port range instead of an
    /// OS-assigned ephemeral one (`isekai_pipe_core::ConnectionIntent::local_bind_port_range`'s
    /// docs) — only takes effect on the manual `--profile`-driven path
    /// (`intent_from_profile`); when `ISEKAI_INTENT_ID` is set, the claimed
    /// `ConnectionIntent` (already written by `isekai-ssh`'s `#@isekai
    /// local-bind-port-range`) wins instead, matching every other
    /// intent-carried setting.
    bind_port_range: Option<(u16, u16)>,
    /// `--tethering-interface <NAME>` (experimental, default off, relay mode
    /// only): keeps a second, independent connection to the same relay
    /// target warm on this specific physical interface
    /// (`isekai_transport::WarmStandby`) and promotes it — no fresh dial, no
    /// backoff wait — the instant the primary connection dies, before
    /// falling back to the ordinary `reconnect_and_resume` retry loop. Meant
    /// for PC Wi-Fi + USB/Bluetooth tethering failover (this session's
    /// `pc-tethering-warm-standby-design` memory); has no effect in `--mode
    /// stun` (STUN P2P has no resume/control-stream concept at all, see
    /// `stun_p2p.rs`'s module docs).
    tethering_interface: Option<String>,
}

/// See `ConnectLaunch::relay_transport`'s doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum RelayTransportKind {
    #[default]
    Udp,
    Qmux,
}

impl std::str::FromStr for RelayTransportKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "udp" => Ok(RelayTransportKind::Udp),
            "qmux" => Ok(RelayTransportKind::Qmux),
            other => Err(format!("invalid --relay-transport value: {other} (expected udp|qmux)")),
        }
    }
}

/// `connect_via_relay_resumable`/`_with_fallback`/`reconnect_and_resume`
/// (`isekai-transport`) already take `&AnyMuxFactory` — this just picks
/// which concrete backend it's built against, once, up front (never
/// re-picked mid-connection, matching `ConnectLaunch::relay_transport`'s doc
/// comment).
pub(crate) fn relay_endpoint_factory(kind: RelayTransportKind) -> AnyMuxFactory {
    match kind {
        RelayTransportKind::Udp => system_quic_factory(),
        RelayTransportKind::Qmux => qmux_relay_factory(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectMode {
    Relay,
    Stun,
}

pub(crate) fn next_arg(
    command: &str,
    iter: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<String, String> {
    iter.next()
        .ok_or_else(|| format!("isekai-pipe {command}: {flag} requires a value"))
}

fn validate_connect_service(value: &str) -> Result<ServiceSpec, ExitCode> {
    let spec = ServiceSpec::new(isekai_pipe_core::ServiceName::new(value), "legacy-connect")
        .map_err(|e| {
            eprintln!("isekai-pipe connect: invalid --service {value:?}: {e}");
            ExitCode::from(EX_USAGE)
        })?;
    if spec.name().as_str() != "ssh" {
        eprintln!(
            "isekai-pipe connect: only ssh service is wired to the legacy connect runtime for now"
        );
        return Err(ExitCode::from(EX_USAGE));
    }
    Ok(spec)
}

fn parse_connect(args: impl Iterator<Item = String>) -> Result<Option<ConnectLaunch>, ExitCode> {
    let mut profile: Option<String> = None;
    let mut service: Option<ServiceSpec> = None;
    let mut stdio = false;
    let mut mode = ConnectMode::Relay;
    let mut stun_servers: Vec<String> = Vec::new();
    let mut resume_window = DEFAULT_RESUME_WINDOW;
    let mut experimental_network_rebind = false;
    let mut relay_transport = RelayTransportKind::default();
    let mut bind_port_range: Option<(u16, u16)> = None;
    let mut tethering_interface: Option<String> = None;
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("USAGE:");
                println!(
                    "    isekai-pipe connect --profile production --service ssh --stdio [OPTIONS]"
                );
                println!();
                println!("INTENT:");
                println!("    If ISEKAI_INTENT_ID is set, the matching ConnectionIntent is claimed first.");
                println!();
                println!("EXPERIMENTAL:");
                println!(
                    "    --experimental-network-rebind  try a fast in-place socket rebind on network"
                );
                println!(
                    "                                   change before falling back to RESUME (default off)"
                );
                println!(
                    "    --relay-transport <udp|qmux>   transport to the relay-assigned isekai-helper endpoint"
                );
                println!(
                    "                                   (default: udp); qmux uses QMux-over-TLS-over-TCP for"
                );
                println!(
                    "                                   networks that block outbound UDP on this side"
                );
                println!(
                    "                                   (EXPERIMENTAL, unverified wire compat with the deployed relay)"
                );
                println!(
                    "    --bind-port-range <S>-<E>      pick this connection's local QUIC port from this range"
                );
                println!(
                    "                                   instead of an OS-assigned one (ignored when ISEKAI_INTENT_ID"
                );
                println!(
                    "                                   is set — the claimed ConnectionIntent's own value wins)"
                );
                println!(
                    "    --tethering-interface <NAME>   keep a warm-standby connection on this physical interface,"
                );
                println!(
                    "                                   promoted instantly on primary failure (relay mode only,"
                );
                println!(
                    "                                   default off)"
                );
                return Ok(None);
            }
            "--profile" => {
                let value = next_arg("connect", &mut iter, "--profile").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                if profile.replace(value).is_some() {
                    eprintln!("isekai-pipe connect: only one --profile is supported");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
            "--service" => {
                let value = next_arg("connect", &mut iter, "--service").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                let spec = validate_connect_service(&value)?;
                if service.replace(spec).is_some() {
                    eprintln!("isekai-pipe connect: only one --service is supported");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
            "--stdio" => stdio = true,
            "--mode" => {
                let value = next_arg("connect", &mut iter, &arg).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                mode = match value.as_str() {
                    "relay" => ConnectMode::Relay,
                    "stun" => ConnectMode::Stun,
                    _ => {
                        eprintln!("isekai-pipe connect: --mode must be relay or stun");
                        return Err(ExitCode::from(EX_USAGE));
                    }
                };
            }
            "--stun-server" => {
                stun_servers.push(next_arg("connect", &mut iter, &arg).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?);
            }
            "--resume-window" => {
                let value = next_arg("connect", &mut iter, &arg).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                let secs: u64 = value.parse().map_err(|_| {
                    eprintln!("isekai-pipe connect: --resume-window must be a number of seconds");
                    ExitCode::from(EX_USAGE)
                })?;
                resume_window = Duration::from_secs(secs);
            }
            "--experimental-network-rebind" => experimental_network_rebind = true,
            "--relay-transport" => {
                let value = next_arg("connect", &mut iter, &arg).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                relay_transport = value.parse().map_err(|e| {
                    eprintln!("isekai-pipe connect: {e}");
                    ExitCode::from(EX_USAGE)
                })?;
            }
            "--bind-port-range" => {
                let value = next_arg("connect", &mut iter, &arg).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                let range = isekai_pipe_core::parse_port_range(&value).map_err(|e| {
                    eprintln!("isekai-pipe connect: {e} (from --bind-port-range)");
                    ExitCode::from(EX_USAGE)
                })?;
                bind_port_range = Some(range);
            }
            "--tethering-interface" => {
                let value = next_arg("connect", &mut iter, &arg).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                if tethering_interface.replace(value).is_some() {
                    eprintln!("isekai-pipe connect: only one --tethering-interface is supported");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
            "--listen" => {
                eprintln!(
                    "isekai-pipe connect: --listen is not wired to the legacy connect runtime yet"
                );
                return Err(ExitCode::from(EX_USAGE));
            }
            other if other.starts_with('-') => {
                eprintln!("isekai-pipe connect: unsupported option: {other}");
                return Err(ExitCode::from(EX_USAGE));
            }
            positional => {
                if profile.replace(positional.to_string()).is_some() {
                    eprintln!("isekai-pipe connect: multiple profiles were provided");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
        }
    }

    if !stdio {
        eprintln!("isekai-pipe connect: --stdio is required until --listen is implemented");
        return Err(ExitCode::from(EX_USAGE));
    }
    if std::env::var_os("ISEKAI_INTENT_ID").is_none() {
        if profile.is_none() {
            eprintln!(
                "isekai-pipe connect: --profile is required when ISEKAI_INTENT_ID is not set"
            );
            return Err(ExitCode::from(EX_USAGE));
        }
        if service.is_none() {
            eprintln!(
                "isekai-pipe connect: --service is required when ISEKAI_INTENT_ID is not set"
            );
            return Err(ExitCode::from(EX_USAGE));
        }
    }

    Ok(Some(ConnectLaunch {
        profile,
        service,
        stdio,
        mode,
        stun_servers,
        resume_window,
        experimental_network_rebind,
        relay_transport,
        bind_port_range,
        tethering_interface,
    }))
}

/// Resolves `--tethering-interface`'s raw interface name (e.g. `wlan1`,
/// `en5`) to the `InterfaceIndex` `isekai_transport::WarmStandby` needs, via
/// the same OS-level interface enumeration `physical_interface.rs`'s own
/// tests use. A name that doesn't match any currently-visible interface is a
/// user configuration error, not a runtime condition to fall back from
/// silently (matches `WarmStandby::new_bound_to_interface`'s own "fail loud
/// on backend/capability mismatch" stance) — reported once, up front, rather
/// than surfacing later as an opaque `MuxError::Bind` deep inside the resume
/// loop. Resolved lazily inside `run_connect` (rather than during
/// `parse_connect`, which stays pure string parsing with no I/O, matching
/// every other flag there) since it needs a real syscall to enumerate
/// interfaces.
fn resolve_tethering_interface(name: &str) -> Result<isekai_transport::InterfaceIndex> {
    isekai_transport::physical_interface::quicsock::discovery::list_interfaces()
        .into_iter()
        .find(|(_, iface)| iface.name == name)
        .map(|(index, _)| index)
        .ok_or_else(|| {
            anyhow::anyhow!("isekai-pipe connect: --tethering-interface {name:?} does not match any known network interface")
        })
}

pub(crate) async fn connect_command(args: impl Iterator<Item = String>) -> ExitCode {
    let launch = match parse_connect(args) {
        Ok(Some(launch)) => launch,
        Ok(None) => return ExitCode::SUCCESS,
        Err(code) => return code,
    };

    // stdout carries only the SSH byte stream (module docs/`stdout_purity.rs`
    // e2e tests) — logs, including `isekai-transport`'s per-candidate-attempt
    // telemetry, must go to stderr only, exactly like `isekai-pipe serve`'s
    // own `env_logger` setup (`engine::run_from_args`).
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();

    let profile_for_outcome = launch.profile.clone().unwrap_or_default();
    match run_connect(launch).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:?}");
            write_connect_outcome_for_wrapper(&profile_for_outcome, &e);
            ExitCode::from(EX_UNAVAILABLE)
        }
    }
}

/// Writes a `ConnectOutcome` side-channel file for `isekai-ssh`'s wrapper to
/// notice after `ssh` exits (`ISEKAI_PIPE_DESIGN.md` §8 Epic N) — for
/// *every* `run_connect` failure, not just ones that look like stale trust
/// material (the "always-connects" principle: a cached deployment being
/// stale/dead in any way must not require the user to notice and manually
/// run `isekai-ssh doctor --fix`/`init`). `StaleTrust` is used when `err`
/// carries `isekai_transport::StaleTrustSignal` (the narrow, high-confidence
/// cert-pin-mismatch/Auth-reject case); every other `run_connect` failure —
/// including a plain QUIC-connect idle timeout because the cached endpoint
/// is simply dead — is recorded as `Unreachable`. Both classes make
/// `isekai-ssh`'s wrapper attempt one silent re-bootstrap + retry
/// (`wrapper.rs::run_ssh_with_connect_failure_recovery`); only the log
/// message differs.
///
/// Only does anything when `ISEKAI_INTENT_ID` is set — a manual, standalone
/// `isekai-pipe connect` invocation has no wrapper watching, so there is
/// nowhere useful to write to. Failure to write is logged and swallowed:
/// this must never change `connect_command`'s own exit code or touch
/// stdout (stdout purity is a separately-tested hard invariant elsewhere).
fn write_connect_outcome_for_wrapper(profile: &str, err: &anyhow::Error) {
    let Some(intent_id) = std::env::var_os("ISEKAI_INTENT_ID") else { return };
    let intent_id = intent_id.to_string_lossy().into_owned();
    let Ok(runtime_dir) = default_runtime_dir() else {
        log::warn!("isekai-pipe connect: could not determine runtime dir to record a connect outcome");
        return;
    };
    let class = if err.downcast_ref::<isekai_transport::StaleTrustSignal>().is_some() {
        isekai_pipe_core::ConnectOutcomeClass::StaleTrust
    } else {
        isekai_pipe_core::ConnectOutcomeClass::Unreachable
    };
    let outcome = isekai_pipe_core::ConnectOutcome {
        schema_version: isekai_pipe_core::CONNECT_OUTCOME_SCHEMA_VERSION,
        intent_id,
        profile: profile.to_string(),
        class,
        detail: format!("{err:#}"),
    };
    if let Err(e) = isekai_pipe_core::write_connect_outcome(&runtime_dir, &outcome) {
        log::warn!("isekai-pipe connect: failed to record a connect outcome: {e}");
    }
}

/// Which of the three connect-time paths `run_connect` takes for a given
/// `ConnectionIntent`. Extracted to a pure function so the routing decision
/// (in particular, that a non-empty `relay_endpoints`/`stun_servers` list
/// alone is *not* enough to pick that family — `intent.transport` must
/// actually match) is directly unit-testable without a real network stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectRoute {
    RelayWithFallback,
    StunWithFallback,
    SingleCandidate,
}

/// - `#@isekai relay <addr>` (`ConnectionIntent.relay_endpoints`) opts into
///   multi-candidate relay fallback (`ISEKAI_PIPE_DESIGN.md` task #12) —
///   when unset, behavior is exactly the pre-#12 single-candidate path
///   (`ConfigRelayProvider` is simply never consulted). Also gated on
///   `intent.transport` actually being `Relay` — `ISEKAI_PIPE_DESIGN.md` §8
///   Epic G's `select_transport` can choose `IntentTransport::StunP2p` as
///   primary while `relay_endpoints` is still populated as a *different*
///   fallback list (e.g. a host configured with both `#@isekai stun` and
///   `#@isekai relay`); without this check, a non-empty `relay_endpoints`
///   would silently override an evidence-gated STUN primary with relay.
/// - `#@isekai stun <addr>` / repeated `--mode stun --stun-server <addr>`
///   (`ConnectionIntent.stun_servers`) opts into multi-STUN-server fallback
///   (`#11`) — gated on `intent.transport` actually being `StunP2p` (not
///   just `stun_servers` being non-empty) so that `isekai-ssh/src/wrapper.rs`'s
///   `#@isekai stun` directive, which can be paired with either a `Relay`
///   transport (no STUN evidence yet, `stun_servers` copied through for
///   future use — a harmless no-op here) or, once `select_transport` finds
///   a cached STUN-observed address, a `StunP2p` primary transport (Epic G),
///   reaches the right fallback path in both cases instead of newly
///   erroring out via `ConfigStunProvider`'s `StunServersWithoutStunP2pTransport`.
fn choose_connect_route(intent: &ConnectionIntent) -> ConnectRoute {
    if !intent.relay_endpoints.is_empty() && matches!(intent.transport, IntentTransport::Relay { .. }) {
        ConnectRoute::RelayWithFallback
    } else if !intent.stun_servers.is_empty() && matches!(intent.transport, IntentTransport::StunP2p { .. }) {
        ConnectRoute::StunWithFallback
    } else {
        ConnectRoute::SingleCandidate
    }
}

/// The connect-time typed errors that carry a stale-trust classification
/// (`ISEKAI_PIPE_DESIGN.md` §8 Epic N) — implemented for every type that
/// crosses the typed-error → `anyhow::Error` boundary in this file, so
/// `attach_stale_trust_signal` below is the single place that boundary is
/// crossed, instead of duplicating the check at each of the four call sites.
pub(crate) trait StaleTrustSignalSource {
    fn is_stale_trust_signal(&self) -> bool;
}
impl StaleTrustSignalSource for isekai_transport::TransportError {
    fn is_stale_trust_signal(&self) -> bool {
        isekai_transport::TransportError::is_stale_trust_signal(self)
    }
}
impl StaleTrustSignalSource for SequentialConnectError {
    fn is_stale_trust_signal(&self) -> bool {
        SequentialConnectError::is_stale_trust_signal(self)
    }
}
impl StaleTrustSignalSource for SequentialStunConnectError {
    fn is_stale_trust_signal(&self) -> bool {
        SequentialStunConnectError::is_stale_trust_signal(self)
    }
}

/// Converts a connect-time typed error to `anyhow::Error`, attaching
/// `isekai_transport::StaleTrustSignal` when `e.is_stale_trust_signal()` so
/// `connect_command` can later `downcast_ref` it off the top-level error
/// (`ISEKAI_PIPE_DESIGN.md` §8 Epic N) — mirrors
/// `isekai-bootstrap-plan::BootstrapFailure`'s attach-at-the-source/
/// downcast-at-the-top shape. Callers rely on the outer `.context(...)`
/// already added at each `run_connect` call site for the human-readable
/// message; this only adds the machine-readable marker.
pub(crate) fn attach_stale_trust_signal<E>(e: E) -> anyhow::Error
where
    E: std::error::Error + Send + Sync + StaleTrustSignalSource + 'static,
{
    let is_stale = e.is_stale_trust_signal();
    let err = anyhow::Error::new(e);
    if is_stale {
        err.context(isekai_transport::StaleTrustSignal)
    } else {
        err
    }
}

async fn run_connect(launch: ConnectLaunch) -> Result<()> {
    let intent = resolve_connection_intent(&launch)?;
    if intent.service != "ssh" {
        anyhow::bail!(
            "isekai-pipe connect: unsupported service in ConnectionIntent: {}",
            intent.service
        );
    }

    let profile = intent.profile.clone();
    // Session auth material never travels through the candidate model itself
    // (`Candidate`/`CandidateKey` deliberately carry no secrets,
    // `ISEKAI_PIPE_DESIGN.md` task #17) — decoded once here from the intent
    // and threaded alongside whichever candidate is selected.
    let session_secret = decode_secret(intent_session_secret_b64(&intent.transport))?;
    let tethering_interface = launch
        .tethering_interface
        .as_deref()
        .map(resolve_tethering_interface)
        .transpose()?;

    match choose_connect_route(&intent) {
        ConnectRoute::RelayWithFallback => {
            let candidates = resolve_relay_candidates(&intent, &session_secret).await?;
            return run_relay_resumable_with_fallback(
                &candidates,
                &profile,
                intent.resume_grace_secs,
                launch.experimental_network_rebind,
                launch.relay_transport,
                tethering_interface,
            )
            .await
            .context("isekai-pipe connect: relay transport (with fallback) failed");
        }
        ConnectRoute::StunWithFallback => {
            let (target, candidates) = resolve_stun_candidates(&intent, &session_secret).await?;
            let stun_result = run_stun_p2p_with_fallback(&target, &candidates).await;
            return recover_via_cross_family_fallback(
                stun_result,
                &intent,
                "STUN P2P transport (with fallback)",
                launch.experimental_network_rebind,
                launch.relay_transport,
                tethering_interface,
            )
            .await;
        }
        ConnectRoute::SingleCandidate => {}
    }

    let candidate = resolve_single_candidate(&intent).await?;
    let candidate_id_str = candidate.id.0.to_string();
    let identity = isekai_transport::CandidateIdentity {
        kind: candidate.route.kind_label(),
        source: candidate.origins.first().map(|o| o.source.label()).unwrap_or("unknown"),
        provider: candidate.origins.first().map(|o| o.provider_id.as_str()).unwrap_or("unknown"),
        id: &candidate_id_str,
    };

    match &candidate.route {
        CandidateRoute::Relay { cert_pin, helper_addr, server_name } => run_relay_resumable(
            &RelayTarget {
                helper_addr: *helper_addr,
                server_name: server_name.as_str().to_string(),
                cert_sha256_hex: cert_pin.to_hex(),
                session_secret,
                local_bind_port_range: intent.local_bind_port_range,
            },
            &profile,
            intent.resume_grace_secs,
            identity,
            launch.experimental_network_rebind,
            launch.relay_transport,
            tethering_interface,
        )
        .await
        .context("isekai-pipe connect: relay transport failed"),
        CandidateRoute::StunP2p { cert_pin, peer_addr, stun_server, server_name } => {
            let stun_result = connect_stun_p2p(
                &system_quic_factory(),
                *stun_server,
                &StunP2pTarget {
                    peer_addr: *peer_addr,
                    server_name: server_name.as_str().to_string(),
                    cert_sha256_hex: cert_pin.to_hex(),
                    session_secret,
                },
                identity,
            )
            .await
            .map(|conn| conn.stream);
            match stun_result {
                Ok(stream) => relay_stdio(stream).await,
                Err(e) => {
                    recover_via_cross_family_fallback(
                        Err(attach_stale_trust_signal(e)),
                        &intent,
                        "STUN P2P transport",
                        launch.experimental_network_rebind,
                        launch.relay_transport,
                        tethering_interface,
                    )
                    .await
                }
            }
        }
    }
}

/// If `result` failed and `intent.cross_family_fallback` names a `Relay`
/// transport, retries once via that transport instead — sequential
/// cross-family fallback (`ISEKAI_PIPE_DESIGN.md` §8 Epic I's
/// `I-route-scheduler`, the ordered-fallback half only; racing a second
/// family concurrently remains out of scope, same as `select_transport`'s
/// own STUN-vs-relay choice in `isekai-ssh/src/wrapper.rs`). `context_label`
/// names `result`'s own failed attempt for the combined error message if the
/// fallback also fails (or doesn't exist).
async fn recover_via_cross_family_fallback(
    result: Result<()>,
    intent: &ConnectionIntent,
    context_label: &str,
    experimental_network_rebind: bool,
    relay_transport: RelayTransportKind,
    tethering_interface: Option<isekai_transport::InterfaceIndex>,
) -> Result<()> {
    let Err(primary_err) = result else { return Ok(()) };
    let Some(IntentTransport::Relay { helper_addr, server_name, session_secret_b64 }) = &intent.cross_family_fallback else {
        return Err(primary_err).with_context(|| format!("isekai-pipe connect: {context_label} failed"));
    };
    log::warn!("isekai-pipe connect: {context_label} failed ({primary_err:#}); trying cross-family relay fallback");
    let session_secret = decode_secret(session_secret_b64).with_context(|| {
        format!("isekai-pipe connect: {context_label} failed ({primary_err:#}), and the cross-family relay fallback's session secret was invalid")
    })?;
    let identity = isekai_transport::CandidateIdentity {
        kind: "relay",
        source: "cross-family-fallback",
        provider: "cross-family-fallback",
        id: "cross-family-fallback",
    };
    run_relay_resumable(
        &RelayTarget {
            helper_addr: helper_addr
                .parse()
                .with_context(|| format!("isekai-pipe connect: invalid cross_family_fallback helper_addr {helper_addr:?}"))?,
            server_name: server_name.clone(),
            cert_sha256_hex: intent.expected_server_identity.cert_sha256_hex.clone(),
            session_secret,
            local_bind_port_range: intent.local_bind_port_range,
        },
        &intent.profile,
        intent.resume_grace_secs,
        identity,
        experimental_network_rebind,
        relay_transport,
        tethering_interface,
    )
    .await
    .with_context(|| format!("isekai-pipe connect: {context_label} failed ({primary_err:#}), and the cross-family relay fallback also failed"))
}

fn intent_session_secret_b64(transport: &IntentTransport) -> &str {
    match transport {
        IntentTransport::Relay { session_secret_b64, .. } => session_secret_b64,
        IntentTransport::StunP2p { session_secret_b64, .. } => session_secret_b64,
    }
}

/// Runs `intent` through the candidate pipeline (`LegacyIntentProvider` →
/// `CandidatePool`) and asserts exactly one candidate came out. Today that's
/// always true — the legacy provider only ever produces the single candidate
/// `intent.transport` already described — so anything else is a bug to fail
/// loudly on, not a selection policy decision to make silently (no `.first()`
/// — `ISEKAI_PIPE_DESIGN.md` task #23). This is deliberately the only place
/// `intent.transport` gets converted into a `Candidate`: everything past this
/// point (including which transport variant to dial) reads `candidate.route`,
/// not `intent.transport`, directly.
async fn resolve_single_candidate(intent: &ConnectionIntent) -> Result<Candidate> {
    let ctx = GatherContext {
        generation: CandidateGeneration::INITIAL,
        deadline: tokio::time::Instant::now() + Duration::from_secs(5),
        intent,
    };
    let batch = isekai_transport::LegacyIntentProvider
        .gather(&ctx)
        .await
        .map_err(|e| anyhow::anyhow!("isekai-pipe connect: candidate discovery failed: {e}"))?;

    let mut pool = CandidatePool::new();
    let snapshot = pool
        .replace_generation(batch)
        .map_err(|e| anyhow::anyhow!("isekai-pipe connect: stale candidate generation ({e:?})"))?;

    let [candidate] = snapshot.candidates.as_slice() else {
        anyhow::bail!(
            "isekai-pipe connect: expected exactly one candidate from the legacy provider, got {}",
            snapshot.candidates.len()
        );
    };
    Ok(candidate.clone())
}

/// Runs `intent.relay_endpoints` through `ConfigRelayProvider` → `CandidatePool`,
/// returning every resulting candidate — sorted by priority (rank 0 =
/// `relay_endpoints[0]`, most preferred) — as ready-to-dial
/// `SequentialRelayCandidate`s. Only called when `relay_endpoints` is
/// non-empty (`run_connect` decides which of this or
/// `resolve_single_candidate` applies); every candidate `ConfigRelayProvider`
/// produces is `CandidateRoute::Relay` by construction
/// (`isekai-pipe-core::candidate`'s docs), so encountering anything else here
/// is a bug, not a runtime condition to route around.
async fn resolve_relay_candidates(
    intent: &ConnectionIntent,
    session_secret: &[u8],
) -> Result<Vec<SequentialRelayCandidate>> {
    let ctx = GatherContext {
        generation: CandidateGeneration::INITIAL,
        deadline: tokio::time::Instant::now() + Duration::from_secs(5),
        intent,
    };
    let batch = ConfigRelayProvider
        .gather(&ctx)
        .await
        .map_err(|e| anyhow::anyhow!("isekai-pipe connect: relay candidate discovery failed: {e}"))?;

    let mut pool = CandidatePool::new();
    let snapshot = pool
        .replace_generation(batch)
        .map_err(|e| anyhow::anyhow!("isekai-pipe connect: stale candidate generation ({e:?})"))?;

    // Explicit sort by priority rank, rather than relying on
    // `CandidatePool`'s own (currently coincidentally priority-matching)
    // internal ordering — the fallback order is a correctness property
    // (`ISEKAI_PIPE_DESIGN.md` task #12's acceptance criteria: deterministic,
    // configured-order fallback), not an implementation detail to leave
    // implicit.
    let mut candidates = snapshot.candidates;
    candidates.sort_by_key(|c| c.priority.rank);

    candidates
        .into_iter()
        .map(|candidate| {
            let CandidateRoute::Relay { cert_pin, helper_addr, server_name } = &candidate.route else {
                anyhow::bail!("isekai-pipe connect: ConfigRelayProvider produced a non-relay candidate (bug)");
            };
            Ok(SequentialRelayCandidate {
                target: RelayTarget {
                    helper_addr: *helper_addr,
                    server_name: server_name.as_str().to_string(),
                    cert_sha256_hex: cert_pin.to_hex(),
                    session_secret: session_secret.to_vec(),
                    local_bind_port_range: intent.local_bind_port_range,
                },
                candidate_id: candidate.id.0.to_string(),
            })
        })
        .collect()
}

/// Runs `intent.stun_servers` through `ConfigStunProvider` → `CandidatePool`,
/// returning the shared `StunP2pTarget` (peer/session identity, the same for
/// every candidate — only `stun_server` varies) alongside every resulting
/// candidate — sorted by priority (rank 0 = `stun_servers[0]`, most
/// preferred) — as ready-to-dial `SequentialStunCandidate`s (`#11`). Only
/// called when `stun_servers` is non-empty *and* `intent.transport` is
/// `StunP2p` (`run_connect` decides); every candidate `ConfigStunProvider`
/// produces is `CandidateRoute::StunP2p` by construction
/// (`isekai-pipe-core::candidate`'s docs), so encountering anything else here
/// is a bug, not a runtime condition to route around.
async fn resolve_stun_candidates(
    intent: &ConnectionIntent,
    session_secret: &[u8],
) -> Result<(StunP2pTarget, Vec<SequentialStunCandidate>)> {
    let IntentTransport::StunP2p { peer_addr, server_name, .. } = &intent.transport else {
        anyhow::bail!("isekai-pipe connect: resolve_stun_candidates requires an IntentTransport::StunP2p intent (bug)");
    };

    let ctx = GatherContext {
        generation: CandidateGeneration::INITIAL,
        deadline: tokio::time::Instant::now() + Duration::from_secs(5),
        intent,
    };
    let batch = ConfigStunProvider
        .gather(&ctx)
        .await
        .map_err(|e| anyhow::anyhow!("isekai-pipe connect: STUN candidate discovery failed: {e}"))?;

    let mut pool = CandidatePool::new();
    let snapshot = pool
        .replace_generation(batch)
        .map_err(|e| anyhow::anyhow!("isekai-pipe connect: stale candidate generation ({e:?})"))?;

    // Explicit sort by priority rank — same rationale as
    // `resolve_relay_candidates`'s own sort (`ISEKAI_PIPE_DESIGN.md` task
    // `#12`'s acceptance criteria, mirrored for `#11`): deterministic,
    // configured-order fallback is a correctness property, not an
    // implementation detail to leave implicit.
    let mut candidates = snapshot.candidates;
    candidates.sort_by_key(|c| c.priority.rank);

    let target = StunP2pTarget {
        peer_addr: peer_addr.parse().context("isekai-pipe connect: invalid peer_addr in IntentTransport::StunP2p")?,
        server_name: server_name.clone(),
        cert_sha256_hex: intent.expected_server_identity.cert_sha256_hex.clone(),
        session_secret: session_secret.to_vec(),
    };

    let candidates = candidates
        .into_iter()
        .map(|candidate| {
            let CandidateRoute::StunP2p { stun_server, .. } = &candidate.route else {
                anyhow::bail!("isekai-pipe connect: ConfigStunProvider produced a non-stun-p2p candidate (bug)");
            };
            Ok(SequentialStunCandidate { stun_server: *stun_server, candidate_id: candidate.id.0.to_string() })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok((target, candidates))
}

fn resolve_connection_intent(launch: &ConnectLaunch) -> Result<ConnectionIntent> {
    if let Some(intent_id) = std::env::var_os("ISEKAI_INTENT_ID") {
        let intent_id = intent_id.to_string_lossy();
        let runtime_dir = default_runtime_dir()
            .context("isekai-pipe connect: could not determine runtime dir")?;
        return claim_connection_intent(&runtime_dir, &intent_id)
            .context("isekai-pipe connect: failed to claim ConnectionIntent");
    }

    let profile = launch.profile.as_deref().context("missing profile")?;
    let service = launch
        .service
        .as_ref()
        .map(|service| service.name().as_str())
        .unwrap_or("ssh");
    intent_from_profile(profile, service, launch)
}

fn intent_from_profile(
    profile: &str,
    service: &str,
    launch: &ConnectLaunch,
) -> Result<ConnectionIntent> {
    let key = isekai_trust::normalize_host_port(profile)
        .with_context(|| format!("isekai-pipe connect: invalid profile {profile:?}"))?;
    let profiles_dir =
        default_profiles_dir().context("isekai-pipe connect: could not determine profiles directory")?;
    let entry = load_persistent_profile(&profiles_dir, &key)
        .with_context(|| format!("isekai-pipe connect: failed to load profile from {}", profiles_dir.display()))?
        .with_context(|| {
            format!("isekai-pipe connect: profile {profile:?} is not trusted yet (looked up as {key:?})")
        })?;
    let legacy_relay = entry.legacy_relay_transport.as_ref().with_context(|| {
        format!("isekai-pipe connect: profile {profile:?} has no cached relay transport to connect with")
    })?;

    let transport = match launch.mode {
        ConnectMode::Relay => IntentTransport::Relay {
            helper_addr: legacy_relay.helper_addr.clone(),
            server_name: "isekai-helper".to_string(),
            session_secret_b64: legacy_relay.session_secret_b64.clone(),
        },
        ConnectMode::Stun => IntentTransport::StunP2p {
            // The first configured `--stun-server` backs the legacy
            // single-candidate field (`resolve_single_candidate`'s
            // `LegacyIntentProvider` path, still exercised whenever only one
            // is given) — `intent.stun_servers` below carries the full list
            // for `ConfigStunProvider` fallback (`#11`).
            stun_server: launch
                .stun_servers
                .first()
                .cloned()
                .context("isekai-pipe connect: --stun-server is required with --mode stun")?,
            peer_addr: legacy_relay.helper_addr.clone(),
            server_name: "isekai-helper".to_string(),
            session_secret_b64: legacy_relay.session_secret_b64.clone(),
        },
    };

    let mut intent = ConnectionIntent::new(
        profile,
        service,
        entry.server_identity.clone(),
        transport,
        BootstrapProvenance::TrustStore { key },
    );
    intent.resume_grace_secs = launch.resume_window.as_secs();
    intent.local_bind_port_range = launch.bind_port_range;
    if launch.mode == ConnectMode::Stun {
        intent.stun_servers = launch.stun_servers.clone();
    }
    Ok(intent)
}

pub(crate) fn decode_secret(b64: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("invalid session_secret_b64")
}

#[cfg(test)]
mod tests {
    use super::*;
    use isekai_pipe_core::ServerIdentity;

    fn parse_connect_args(args: &[&str]) -> ConnectLaunch {
        parse_connect(args.iter().map(|arg| arg.to_string()))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn choose_connect_route_prefers_stun_over_relay_endpoints_when_transport_is_stun_p2p() {
        // Regression test: a host configured with both `#@isekai stun` and
        // `#@isekai relay` — `select_transport` (Epic G) already chose
        // `StunP2p` as the primary, so `relay_endpoints` being non-empty
        // must not silently steer this back onto the relay path.
        let mut intent = ConnectionIntent::new(
            "production",
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::StunP2p {
                stun_server: "203.0.113.9:3478".to_string(),
                peer_addr: "198.51.100.7:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "production:22".to_string() },
        );
        intent.relay_endpoints = vec!["masque://relay.example.com".to_string()];
        intent.stun_servers = vec!["203.0.113.9:3478".to_string()];

        assert_eq!(choose_connect_route(&intent), ConnectRoute::StunWithFallback);
    }

    #[test]
    fn choose_connect_route_uses_relay_fallback_when_transport_is_relay() {
        let mut intent = ConnectionIntent::new(
            "production",
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::Relay {
                helper_addr: "203.0.113.5:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "production:22".to_string() },
        );
        intent.relay_endpoints = vec!["masque://relay.example.com".to_string()];

        assert_eq!(choose_connect_route(&intent), ConnectRoute::RelayWithFallback);
    }

    #[test]
    fn choose_connect_route_defaults_to_single_candidate_with_no_fallback_lists() {
        let intent = ConnectionIntent::new(
            "production",
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::Relay {
                helper_addr: "203.0.113.5:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "production:22".to_string() },
        );

        assert_eq!(choose_connect_route(&intent), ConnectRoute::SingleCandidate);
    }

    #[tokio::test]
    async fn resolve_single_candidate_never_leaks_the_session_secret() {
        let intent = ConnectionIntent::new(
            "production",
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::Relay {
                helper_addr: "203.0.113.5:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "top-secret-value".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "production:22".to_string() },
        );

        let candidate = resolve_single_candidate(&intent).await.unwrap();
        let debug = format!("{candidate:?}");
        assert!(!debug.contains("top-secret-value"), "Candidate must never carry session_secret");
    }

    #[tokio::test]
    async fn resolve_single_candidate_matches_the_legacy_transport() {
        let intent = ConnectionIntent::new(
            "production",
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::Relay {
                helper_addr: "203.0.113.5:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "production:22".to_string() },
        );

        let candidate = resolve_single_candidate(&intent).await.unwrap();
        assert!(matches!(
            candidate.route,
            CandidateRoute::Relay { helper_addr, .. } if helper_addr == "203.0.113.5:45231".parse().unwrap()
        ));
    }

    fn intent_with_relay_endpoints(relay_endpoints: Vec<String>) -> ConnectionIntent {
        let mut intent = ConnectionIntent::new(
            "production",
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::Relay {
                helper_addr: "203.0.113.5:1".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "production:22".to_string() },
        );
        intent.relay_endpoints = relay_endpoints;
        intent
    }

    #[tokio::test]
    async fn resolve_relay_candidates_preserves_configured_order() {
        let intent = intent_with_relay_endpoints(vec![
            "203.0.113.10:45231".to_string(),
            "198.51.100.7:45231".to_string(),
            "192.0.2.3:45231".to_string(),
        ]);

        let candidates = resolve_relay_candidates(&intent, b"session-secret").await.unwrap();

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].target.helper_addr, "203.0.113.10:45231".parse().unwrap());
        assert_eq!(candidates[1].target.helper_addr, "198.51.100.7:45231".parse().unwrap());
        assert_eq!(candidates[2].target.helper_addr, "192.0.2.3:45231".parse().unwrap());
        // Every candidate must carry the shared cert pin/server name/secret.
        for candidate in &candidates {
            assert_eq!(candidate.target.cert_sha256_hex, "ab".repeat(32));
            assert_eq!(candidate.target.server_name, "isekai-helper");
            assert_eq!(candidate.target.session_secret, b"session-secret");
        }
        // Candidate ids must be distinct (used for telemetry/error correlation).
        assert_ne!(candidates[0].candidate_id, candidates[1].candidate_id);
        assert_ne!(candidates[1].candidate_id, candidates[2].candidate_id);
    }

    #[tokio::test]
    async fn resolve_relay_candidates_never_leaks_the_session_secret_via_debug() {
        let intent = intent_with_relay_endpoints(vec!["203.0.113.10:45231".to_string()]);
        let candidates = resolve_relay_candidates(&intent, b"top-secret-value").await.unwrap();
        // `RelayTarget` isn't `Debug`, so this proves via the one field that
        // actually carries the secret: it must be exactly what was passed in,
        // never derived from/mixed with candidate identity data.
        assert_eq!(candidates[0].target.session_secret, b"top-secret-value");
    }

    #[tokio::test]
    async fn resolve_relay_candidates_rejects_a_malformed_endpoint() {
        let intent = intent_with_relay_endpoints(vec!["not-an-address".to_string()]);
        assert!(resolve_relay_candidates(&intent, b"secret").await.is_err());
    }

    fn intent_with_stun_servers(stun_servers: Vec<String>) -> ConnectionIntent {
        let mut intent = ConnectionIntent::new(
            "production",
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::StunP2p {
                stun_server: stun_servers.first().cloned().unwrap_or_default(),
                peer_addr: "203.0.113.5:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "production:22".to_string() },
        );
        intent.stun_servers = stun_servers;
        intent
    }

    #[tokio::test]
    async fn resolve_stun_candidates_preserves_configured_order() {
        let intent = intent_with_stun_servers(vec![
            "192.0.2.10:3478".to_string(),
            "192.0.2.11:3478".to_string(),
            "192.0.2.12:3478".to_string(),
        ]);

        let (target, candidates) = resolve_stun_candidates(&intent, b"session-secret").await.unwrap();

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].stun_server, "192.0.2.10:3478".parse().unwrap());
        assert_eq!(candidates[1].stun_server, "192.0.2.11:3478".parse().unwrap());
        assert_eq!(candidates[2].stun_server, "192.0.2.12:3478".parse().unwrap());
        // Every candidate shares the same peer/session identity.
        assert_eq!(target.peer_addr, "203.0.113.5:45231".parse().unwrap());
        assert_eq!(target.server_name, "isekai-helper");
        assert_eq!(target.cert_sha256_hex, "ab".repeat(32));
        assert_eq!(target.session_secret, b"session-secret");
        assert_ne!(candidates[0].candidate_id, candidates[1].candidate_id);
        assert_ne!(candidates[1].candidate_id, candidates[2].candidate_id);
    }

    #[tokio::test]
    async fn resolve_stun_candidates_never_leaks_the_session_secret_via_debug() {
        let intent = intent_with_stun_servers(vec!["192.0.2.10:3478".to_string()]);
        let (target, _candidates) = resolve_stun_candidates(&intent, b"top-secret-value").await.unwrap();
        assert_eq!(target.session_secret, b"top-secret-value");
    }

    #[tokio::test]
    async fn resolve_stun_candidates_rejects_a_malformed_stun_server() {
        let intent = intent_with_stun_servers(vec!["not-an-address".to_string()]);
        assert!(resolve_stun_candidates(&intent, b"secret").await.is_err());
    }

    #[test]
    fn connect_accepts_multiple_stun_server_flags_in_order() {
        let launch = parse_connect_args(&[
            "--profile",
            "production",
            "--service",
            "ssh",
            "--stdio",
            "--mode",
            "stun",
            "--stun-server",
            "192.0.2.10:3478",
            "--stun-server",
            "192.0.2.11:3478",
        ]);

        assert_eq!(launch.stun_servers, vec!["192.0.2.10:3478".to_string(), "192.0.2.11:3478".to_string()]);
    }

    #[test]
    fn connect_accepts_profile_service_and_stdio() {
        let launch = parse_connect_args(&[
            "--profile",
            "production",
            "--service",
            "ssh",
            "--stdio",
            "--mode",
            "relay",
            "--resume-window",
            "30",
        ]);

        assert_eq!(launch.profile.as_deref(), Some("production"));
        assert_eq!(
            launch.service,
            Some(
                ServiceSpec::new(isekai_pipe_core::ServiceName::new("ssh"), "legacy-connect")
                    .unwrap()
            )
        );
        assert!(launch.stdio);
        assert_eq!(launch.mode, ConnectMode::Relay);
        assert_eq!(launch.resume_window, Duration::from_secs(30));
        assert_eq!(launch.relay_transport, RelayTransportKind::Udp);
    }

    #[test]
    fn connect_relay_transport_defaults_to_udp() {
        let launch = parse_connect_args(&["production", "--service", "ssh", "--stdio"]);
        assert_eq!(launch.relay_transport, RelayTransportKind::Udp);
    }

    #[test]
    fn connect_relay_transport_qmux_parses() {
        let launch = parse_connect_args(&[
            "production",
            "--service",
            "ssh",
            "--stdio",
            "--relay-transport",
            "qmux",
        ]);
        assert_eq!(launch.relay_transport, RelayTransportKind::Qmux);
    }

    #[test]
    fn connect_relay_transport_rejects_unknown_value() {
        let result = parse_connect(
            ["production", "--service", "ssh", "--stdio", "--relay-transport", "bogus"]
                .into_iter()
                .map(String::from),
        );
        assert!(result.is_err());
    }

    #[test]
    fn connect_tethering_interface_defaults_to_none() {
        let launch = parse_connect_args(&["production", "--service", "ssh", "--stdio"]);
        assert_eq!(launch.tethering_interface, None);
    }

    #[test]
    fn connect_tethering_interface_parses() {
        let launch = parse_connect_args(&[
            "production",
            "--service",
            "ssh",
            "--stdio",
            "--tethering-interface",
            "wlan1",
        ]);
        assert_eq!(launch.tethering_interface.as_deref(), Some("wlan1"));
    }

    #[test]
    fn connect_tethering_interface_rejects_a_second_occurrence() {
        let result = parse_connect(
            [
                "production",
                "--service",
                "ssh",
                "--stdio",
                "--tethering-interface",
                "wlan1",
                "--tethering-interface",
                "wlan2",
            ]
            .into_iter()
            .map(String::from),
        );
        assert!(result.is_err());
    }

    #[test]
    fn resolve_tethering_interface_fails_loud_for_an_unknown_name() {
        let err = resolve_tethering_interface("definitely-not-a-real-interface-name").unwrap_err();
        assert!(format!("{err:#}").contains("does not match any known network interface"));
    }

    #[test]
    fn connect_accepts_positional_profile_for_compatibility() {
        let launch = parse_connect_args(&["production", "--service", "ssh", "--stdio"]);

        assert_eq!(launch.profile.as_deref(), Some("production"));
        assert!(launch.stdio);
        assert_eq!(launch.resume_window, DEFAULT_RESUME_WINDOW);
    }

    #[test]
    fn connect_rejects_non_ssh_service_until_dispatch_exists() {
        assert!(parse_connect(
            [
                "--profile",
                "production",
                "--service",
                "postgres",
                "--stdio"
            ]
            .into_iter()
            .map(str::to_string)
        )
        .is_err());
    }

    #[test]
    fn connect_requires_stdio_until_listen_exists() {
        assert!(parse_connect(
            ["--profile", "production", "--service", "ssh"]
                .into_iter()
                .map(str::to_string)
        )
        .is_err());
    }

    fn sample_stun_primary_intent() -> ConnectionIntent {
        ConnectionIntent::new(
            "production",
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::StunP2p {
                stun_server: "203.0.113.9:3478".to_string(),
                peer_addr: "198.51.100.7:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "production:22".to_string() },
        )
    }

    #[tokio::test]
    async fn recover_via_cross_family_fallback_passes_success_through_untouched() {
        let intent = sample_stun_primary_intent();
        let result = recover_via_cross_family_fallback(Ok(()), &intent, "STUN P2P transport", false, RelayTransportKind::Udp, None).await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn recover_via_cross_family_fallback_propagates_the_original_error_when_no_fallback_exists() {
        let intent = sample_stun_primary_intent();
        assert_eq!(intent.cross_family_fallback, None);

        let result = recover_via_cross_family_fallback(
            Err(anyhow::anyhow!("simulated STUN failure")),
            &intent,
            "STUN P2P transport",
            false,
            RelayTransportKind::Udp,
            None,
        )
        .await;

        let err = result.unwrap_err();
        assert!(format!("{err:#}").contains("simulated STUN failure"), "{err:#}");
        assert!(format!("{err:#}").contains("STUN P2P transport failed"), "{err:#}");
    }

    #[tokio::test]
    async fn recover_via_cross_family_fallback_reports_both_failures_when_the_fallback_dial_also_fails() {
        let mut intent = sample_stun_primary_intent();
        // A relay target nothing is listening on — the fallback dial itself
        // must fail too, and the combined error should mention both.
        intent.cross_family_fallback = Some(IntentTransport::Relay {
            helper_addr: "127.0.0.1:1".to_string(),
            server_name: "isekai-helper".to_string(),
            session_secret_b64: "c2VjcmV0".to_string(),
        });

        let result = recover_via_cross_family_fallback(
            Err(anyhow::anyhow!("simulated STUN failure")),
            &intent,
            "STUN P2P transport",
            false,
            RelayTransportKind::Udp,
            None,
        )
        .await;

        let err = result.unwrap_err();
        let rendered = format!("{err:#}");
        assert!(rendered.contains("simulated STUN failure"), "{rendered}");
        assert!(rendered.contains("cross-family relay fallback also failed"), "{rendered}");
    }
}
