mod ctl;
mod engine;

use std::collections::VecDeque;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use isekai_pipe_core::{
    claim_connection_intent, default_profiles_dir, default_runtime_dir, load_persistent_profile,
    BootstrapProvenance, Candidate, CandidateGeneration, CandidateRoute, ConnectionIntent, IntentTransport,
    ServiceSpec,
};
#[cfg(test)]
use isekai_pipe_core::ServerIdentity;
use isekai_transport::{
    compute_proof, connect_stun_p2p, connect_stun_p2p_with_fallback, connect_via_relay_resumable,
    connect_via_relay_resumable_with_fallback, open_control_stream, qmux_relay_factory, reconnect_and_resume,
    spawn_app_ack_tasks, system_quic_factory, AnyByteStream, AnyByteStreamReadHalf, AnyByteStreamWriteHalf,
    AnyMuxConnection, AnyMuxFactory, AnyMuxRebinder, AppAckCounters, AppAckTasks, AttemptFailure, BackoffPolicy,
    BindSpec, C2hSentOffset, CandidatePool, CandidateProvider, ConfigRelayProvider, ConfigStunProvider, GatherContext,
    H2cClientDeliveredOffset, LegacyIntentProvider, RelayTarget, SequentialConnectError, SequentialFailure,
    SequentialRelayCandidate, SequentialStunCandidate, SequentialStunConnectError, StunP2pTarget,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const EX_USAGE: u8 = 64;
const EX_UNAVAILABLE: u8 = 69;

/// Serializes tests (across `main.rs`/`ctl.rs`) that mutate process-global
/// env vars (`ISEKAI_PIPE_PROFILES_DIR`/`ISEKAI_CTL_SOCK`). `cargo test`
/// runs `#[test]` functions on multiple threads within the same process by
/// default, and `std::env::set_var` has no thread-local scoping — without
/// this, one test's mutation can be clobbered mid-flight by a concurrently
/// running test in a different module (matches `isekai-ssh`'s
/// `HOME_ENV_LOCK` for the same reason).
#[cfg(test)]
pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());
const DEFAULT_RESUME_WINDOW: Duration = Duration::from_secs(120);
const C2H_REPLAY_BUFFER_CAPACITY: usize = 4 * 1024 * 1024;
const RESUME_BACKOFF: BackoffPolicy = BackoffPolicy {
    initial: Duration::from_millis(500),
    max: Duration::from_secs(10),
    jitter: 0.0,
};
const BACKPRESSURE_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How often `run_resume_loop`'s background task calls
/// `WarmStandby::ensure_warm` while `--tethering-interface` is set. Matches
/// the "~15-30s while the primary looks healthy" half of the
/// `pc-tethering-warm-standby-design` memory's agreed tiering — the more
/// aggressive "~1-3s once the primary looks like it's degrading" half is not
/// implemented (this loop has no independent signal that the primary is
/// degrading, only that it's already dead, at which point promotion is
/// already being attempted).
const WARM_STANDBY_PROBE_INTERVAL: Duration = Duration::from_secs(20);

fn print_help() {
    println!("isekai-pipe - data plane for isekai-ssh");
    println!();
    println!("USAGE:");
    println!("    isekai-pipe <COMMAND> [OPTIONS]");
    println!();
    println!("COMMANDS:");
    println!("    connect    local stdio side");
    println!("    serve      remote service side");
    println!("    probe      connectivity probe (skeleton)");
    println!("    inspect    passive profile inspection (--profile, --json, --redact, --verbose)");
    println!("    ctl        title/clipboard control-plane client (see `isekai-pipe ctl --help`)");
    println!("    version    print version");
    println!();
    println!(
        "The command surface is reserved for the staged isekai-helper -> isekai-pipe migration."
    );
}

#[derive(Debug)]
struct ServeLaunch {
    service: ServiceSpec,
    helper_args: Vec<String>,
}

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
enum RelayTransportKind {
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
fn relay_endpoint_factory(kind: RelayTransportKind) -> AnyMuxFactory {
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

fn next_arg(
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
                let (start, end) = value.split_once('-').ok_or_else(|| {
                    eprintln!("isekai-pipe connect: invalid --bind-port-range value {value:?} (expected <START>-<END>)");
                    ExitCode::from(EX_USAGE)
                })?;
                let parse_port = |s: &str| -> Result<u16, ExitCode> {
                    s.parse().map_err(|_| {
                        eprintln!("isekai-pipe connect: invalid --bind-port-range bound {s:?}");
                        ExitCode::from(EX_USAGE)
                    })
                };
                let (start, end) = (parse_port(start)?, parse_port(end)?);
                if start > end {
                    eprintln!("isekai-pipe connect: invalid --bind-port-range {value:?}: start must be <= end");
                    return Err(ExitCode::from(EX_USAGE));
                }
                bind_port_range = Some((start, end));
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

async fn connect_command(args: impl Iterator<Item = String>) -> ExitCode {
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
trait StaleTrustSignalSource {
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
fn attach_stale_trust_signal<E>(e: E) -> anyhow::Error
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
    let batch = LegacyIntentProvider
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

fn decode_secret(b64: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("invalid session_secret_b64")
}

const PROBE_SCHEMA_VERSION: u32 = 1;

/// One [`ProbeReport`] stage's outcome. Unlike `inspect`'s success/failure
/// binary, `probe` (`ISEKAI_PIPE_DESIGN.md` §8, the paragraph right before
/// Epic K: "「成功/失敗」の二値ではなくどの段階まで成功したかを返す") needs
/// four distinct states per stage: a stage can succeed, fail, not apply to
/// this profile/transport at all, or never get attempted because an earlier
/// stage already failed.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ProbeStageStatus {
    Ok { detail: Option<String> },
    Failed { detail: String },
    Skipped { reason: String },
    NotAttempted { reason: String },
}

impl ProbeStageStatus {
    fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }

    fn is_ok_or_skipped(&self) -> bool {
        matches!(self, Self::Ok { .. } | Self::Skipped { .. })
    }
}

#[derive(Debug, serde::Serialize)]
struct ProbeReport {
    probe_schema_version: u32,
    profile: String,
    transport: &'static str,
    dns_resolution: ProbeStageStatus,
    stun_discovery: ProbeStageStatus,
    handshake: ProbeStageStatus,
    target_reachability: ProbeStageStatus,
    /// Whether the `handshake` failure looks like stale cached trust
    /// material (cert pin mismatch / session-secret auth reject —
    /// `TransportError::is_stale_trust_signal`, `ISEKAI_PIPE_DESIGN.md` §8
    /// Epic N) rather than a plain connectivity problem. `isekai-ssh doctor`
    /// surfaces this as "run with --fix to refresh"; unrelated to
    /// `handshake`'s own `Ok`/`Failed` status (`false` whenever `handshake`
    /// succeeded).
    #[serde(default)]
    stale_trust_suspected: bool,
}

impl ProbeReport {
    /// Whether every stage that applies to this profile's transport
    /// succeeded — `probe_command`'s exit code, and what "the profile is
    /// reachable end to end" means for this command.
    fn fully_reachable(&self) -> bool {
        self.stun_discovery.is_ok_or_skipped() && self.handshake.is_ok() && self.target_reachability.is_ok()
    }
}

/// Maps a bundled relay-auth/QUIC-connect/cert-pin/HELLO-ACK attempt's
/// [`AttemptFailure`] onto this command's `(handshake, target_reachability)`
/// stage pair. These four sub-stages aren't separately observable through
/// any function `isekai-pipe`/`isekai-transport` expose publicly today
/// (`connect_and_handshake` is `pub(crate)` to `isekai-transport`, see
/// `attempt.rs`'s module docs) — reported as one combined `handshake` stage
/// rather than fabricating a finer breakdown this command cannot actually
/// observe.
///
/// `AttemptFailure::DefinitiveRejectNotRetryable` is produced *exclusively*
/// by the server's `AttachRejectReason::Target` (`isekai-transport/src/attempt.rs`'s
/// `From<ConnectAttemptError>` match has no other arm that produces it) — the
/// only variant that means every earlier sub-stage (QUIC connect, cert pin,
/// HELLO/ACK) already succeeded and only the remote helper's own target dial
/// failed, so it's the only variant that reports a *failed*
/// `target_reachability` rather than `NotAttempted`.
fn stage_from_attempt_failure(failure: &AttemptFailure) -> (ProbeStageStatus, ProbeStageStatus) {
    let not_attempted = || ProbeStageStatus::NotAttempted { reason: "handshake did not complete".to_string() };
    match failure {
        AttemptFailure::DefinitiveRejectNotRetryable { source } => (
            ProbeStageStatus::Ok {
                detail: Some("reached ATTACH_HELLO/ACK; remote helper rejected due to its own target".to_string()),
            },
            ProbeStageStatus::Failed { detail: source.to_string() },
        ),
        AttemptFailure::RetryablePreAttach { source } => {
            (ProbeStageStatus::Failed { detail: source.to_string() }, not_attempted())
        }
        AttemptFailure::AmbiguousAfterAttach { source } => (
            ProbeStageStatus::Failed { detail: format!("outcome unobservable after sending ATTACH_HELLO: {source}") },
            not_attempted(),
        ),
        AttemptFailure::LostRace { source } => (
            ProbeStageStatus::Failed { detail: format!("another attempt already attached this session: {source}") },
            not_attempted(),
        ),
        AttemptFailure::StaleAttempt { source, .. } => {
            (ProbeStageStatus::Failed { detail: format!("stale generation: {source}") }, not_attempted())
        }
        AttemptFailure::MustResume { source } => (
            ProbeStageStatus::Failed {
                detail: format!("session already established; needs RESUME, not a fresh attach: {source}"),
            },
            not_attempted(),
        ),
        AttemptFailure::Terminal { source } => {
            (ProbeStageStatus::Failed { detail: source.to_string() }, not_attempted())
        }
    }
}

/// `SequentialConnectError` (the relay-resumable path's error type) has
/// three variants `SequentialStunConnectError` doesn't: resumable-session
/// setup can fail *after* a successful attach (`AttachedButControlStreamFailed`),
/// after a forced resume (`MustResumeButResumeFailed`), or exhaust its
/// generation-retry budget entirely (`GaveUpAfterGenerationRetries`) — none
/// of which STUN P2P has, since it has no resume/control-stream concept at
/// all (`stun_p2p.rs`'s module docs).
fn stage_from_relay_connect_error(error: &SequentialConnectError) -> (ProbeStageStatus, ProbeStageStatus) {
    match error {
        SequentialConnectError::NoCandidates => unreachable!("probe always passes exactly one candidate"),
        SequentialConnectError::AllCandidatesFailed { failures } => stage_from_sequential_failures(failures),
        SequentialConnectError::StoppedEarly { failure, .. } => stage_from_attempt_failure(failure),
        // The data-stream HELLO/ACK already succeeded — per
        // `stage_from_attempt_failure`'s docs, that implies the remote
        // helper already dialed its target successfully — so only the
        // subsequent (probe-irrelevant) resumable control-stream open
        // failed. `handshake` still reports the control-stream failure
        // (this attempt as a whole did not cleanly succeed), but
        // `target_reachability` reports what's actually known: reachable.
        SequentialConnectError::AttachedButControlStreamFailed { source, .. } => (
            ProbeStageStatus::Failed {
                detail: format!("attached successfully, but opening the resumable control stream failed: {source}"),
            },
            ProbeStageStatus::Ok { detail: Some("attach succeeded before the control-stream failure".to_string()) },
        ),
        SequentialConnectError::MustResumeButResumeFailed { source, .. } => (
            ProbeStageStatus::Failed {
                detail: format!("session already established; the follow-up RESUME attempt also failed: {source}"),
            },
            ProbeStageStatus::NotAttempted { reason: "handshake did not complete".to_string() },
        ),
        SequentialConnectError::GaveUpAfterGenerationRetries { failures, budget } => (
            ProbeStageStatus::Failed {
                detail: format!(
                    "gave up after exhausting the generation-retry budget ({budget:?}); last failure(s): {}",
                    failures.iter().map(|f| format!("[{}: {}]", f.candidate_id, f.failure)).collect::<Vec<_>>().join(", ")
                ),
            },
            ProbeStageStatus::NotAttempted { reason: "handshake did not complete".to_string() },
        ),
    }
}

fn stage_from_sequential_failures(failures: &[SequentialFailure]) -> (ProbeStageStatus, ProbeStageStatus) {
    match failures.first() {
        Some(f) => stage_from_attempt_failure(&f.failure),
        // `probe` always passes exactly one candidate to the `_with_fallback`
        // connectors, so `AllCandidatesFailed` always carries exactly one
        // failure — an empty list here would be a bug in those connectors,
        // not a real runtime state, but still reported rather than panicking.
        None => (
            ProbeStageStatus::Failed { detail: "no candidates were attempted (bug)".to_string() },
            ProbeStageStatus::NotAttempted { reason: "no candidates were attempted (bug)".to_string() },
        ),
    }
}

/// Queries `stun_server` for this process's own observed address, as its own
/// standalone probe stage (`ISEKAI_PIPE_DESIGN.md`'s "STUN discovery" stage)
/// — reusing the same `isekai_stun::query_stun` the bundled STUN P2P connect
/// attempt performs internally. Deliberately redundant with that internal
/// query (two STUN round trips instead of one) rather than trying to observe
/// `connect_stun_p2p_with_fallback`'s own internal query, which isn't
/// exposed as a separate step — see `stage_from_attempt_failure`'s docs for
/// why the same "don't fabricate observability that doesn't exist" rule
/// applies here.
async fn probe_stun_discovery(stun_server: std::net::SocketAddr) -> ProbeStageStatus {
    let socket = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
        Ok(socket) => socket,
        Err(e) => return ProbeStageStatus::Failed { detail: format!("failed to bind a UDP socket: {e}") },
    };
    match isekai_stun::query_stun(&socket, stun_server).await {
        Ok(observed_addr) => ProbeStageStatus::Ok { detail: Some(format!("observed address: {observed_addr}")) },
        Err(e) => ProbeStageStatus::Failed { detail: e.to_string() },
    }
}

#[derive(Debug)]
struct ProbeLaunch {
    profile: String,
    stun_servers: Vec<String>,
    json: bool,
}

fn parse_probe(args: impl Iterator<Item = String>) -> Result<Option<ProbeLaunch>, ExitCode> {
    let mut profile: Option<String> = None;
    let mut stun_servers: Vec<String> = Vec::new();
    let mut json = false;
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("USAGE:");
                println!("    isekai-pipe probe --profile production [--stun-server host:port] [--json]");
                return Ok(None);
            }
            "--profile" => {
                let value = next_arg("probe", &mut iter, "--profile").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                if profile.replace(value).is_some() {
                    eprintln!("isekai-pipe probe: only one --profile is supported");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
            "--stun-server" => {
                stun_servers.push(next_arg("probe", &mut iter, &arg).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?);
            }
            "--json" => json = true,
            other if other.starts_with('-') => {
                eprintln!("isekai-pipe probe: unsupported option: {other}");
                return Err(ExitCode::from(EX_USAGE));
            }
            positional => {
                if profile.replace(positional.to_string()).is_some() {
                    eprintln!("isekai-pipe probe: multiple profiles were provided");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
        }
    }

    let Some(profile) = profile else {
        eprintln!("isekai-pipe probe: --profile is required");
        return Err(ExitCode::from(EX_USAGE));
    };

    Ok(Some(ProbeLaunch { profile, stun_servers, json }))
}

async fn probe_command(args: impl Iterator<Item = String>) -> ExitCode {
    let launch = match parse_probe(args) {
        Ok(Some(launch)) => launch,
        Ok(None) => return ExitCode::SUCCESS,
        Err(code) => return code,
    };
    let json = launch.json;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();

    match run_probe(launch).await {
        Ok(report) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&report).expect("ProbeReport always serializes"));
            } else {
                print_probe_report(&report);
            }
            if report.fully_reachable() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(EX_UNAVAILABLE)
            }
        }
        Err(e) => {
            eprintln!("{e:?}");
            ExitCode::from(EX_UNAVAILABLE)
        }
    }
}

fn print_probe_report(report: &ProbeReport) {
    println!("profile:              {}", report.profile);
    println!("transport:            {}", report.transport);
    print_probe_stage("dns resolution", &report.dns_resolution);
    print_probe_stage("stun discovery", &report.stun_discovery);
    print_probe_stage("handshake (relay-auth/quic-connect/cert-pin/hello-ack)", &report.handshake);
    print_probe_stage("target reachability", &report.target_reachability);
    if report.stale_trust_suspected {
        println!("[stale-trust] cached trust material looks stale (cert pin mismatch or session-secret rejected) -- `isekai-ssh doctor <host> --fix` or `isekai-ssh <host>` (self-heals automatically) can refresh it");
    }
}

fn print_probe_stage(label: &str, status: &ProbeStageStatus) {
    match status {
        ProbeStageStatus::Ok { detail } => {
            println!("[ok]          {label}{}", detail.as_deref().map(|d| format!(" -- {d}")).unwrap_or_default());
        }
        ProbeStageStatus::Failed { detail } => println!("[failed]      {label} -- {detail}"),
        ProbeStageStatus::Skipped { reason } => println!("[skipped]     {label} -- {reason}"),
        ProbeStageStatus::NotAttempted { reason } => println!("[not-reached] {label} -- {reason}"),
    }
}

/// Actually dials `launch.profile`'s cached transport and reports how far it
/// got, stage by stage (`ISEKAI_PIPE_DESIGN.md` §8, right before Epic K) —
/// unlike `inspect`, this performs real network I/O, but like every other
/// bootstrap-adjacent command here, it never mutates `PersistentProfile`
/// (no persistence step exists in this function at all).
///
/// Route choice mirrors `isekai-ssh/src/wrapper.rs::select_transport`'s
/// evidence-gating (`ISEKAI_PIPE_DESIGN.md` §8 Epic G): STUN P2P is only
/// probed when both a `--stun-server` was given *and* the profile has
/// `cached_stun_observed_addr` evidence from bootstrap; otherwise the
/// relay/direct-by-bootstrap-host transport is probed.
async fn run_probe(launch: ProbeLaunch) -> Result<ProbeReport> {
    let key = isekai_trust::normalize_host_port(&launch.profile)
        .with_context(|| format!("isekai-pipe probe: invalid profile {:?}", launch.profile))?;
    let profiles_dir =
        default_profiles_dir().context("isekai-pipe probe: could not determine profiles directory")?;
    let entry = load_persistent_profile(&profiles_dir, &key)
        .with_context(|| format!("isekai-pipe probe: failed to load profile from {}", profiles_dir.display()))?
        .with_context(|| {
            format!("isekai-pipe probe: profile {:?} not found (looked up as {key:?} in {})", launch.profile, profiles_dir.display())
        })?;

    // Never a step anywhere in this pipeline: every address a profile caches
    // (`legacy_relay_transport.helper_addr`, `cached_stun_observed_addr`) is
    // already a resolved `SocketAddr` string, parsed directly — there is no
    // hostname to resolve. Reported honestly as not-applicable rather than
    // fabricated as a no-op success.
    let dns_resolution = ProbeStageStatus::Skipped {
        reason: "profiles store pre-resolved addresses; isekai-pipe never performs hostname DNS resolution"
            .to_string(),
    };

    if let (Some(peer_addr), Some(stun_server_str)) = (&entry.cached_stun_observed_addr, launch.stun_servers.first()) {
        let stun_server: std::net::SocketAddr = stun_server_str
            .parse()
            .with_context(|| format!("isekai-pipe probe: invalid --stun-server {stun_server_str:?}"))?;
        let legacy_relay = entry.legacy_relay_transport.as_ref().with_context(|| {
            format!("isekai-pipe probe: profile {:?} has no cached session secret to probe with", launch.profile)
        })?;
        let session_secret = decode_secret(&legacy_relay.session_secret_b64)?;
        let stun_discovery = probe_stun_discovery(stun_server).await;

        let target = StunP2pTarget {
            peer_addr: peer_addr
                .parse()
                .with_context(|| format!("isekai-pipe probe: invalid cached_stun_observed_addr {peer_addr:?}"))?,
            server_name: "isekai-helper".to_string(),
            cert_sha256_hex: entry.server_identity.cert_sha256_hex.clone(),
            session_secret,
        };
        let candidates = vec![SequentialStunCandidate { stun_server, candidate_id: "probe".to_string() }];
        let stun_result = connect_stun_p2p_with_fallback(&system_quic_factory(), &target, &candidates).await;
        let stale_trust_suspected = stun_result.as_ref().err().is_some_and(|e| e.is_stale_trust_signal());
        let (handshake, target_reachability) = match stun_result {
            Ok(_established) => (ProbeStageStatus::Ok { detail: None }, ProbeStageStatus::Ok { detail: None }),
            Err(SequentialStunConnectError::NoCandidates) => unreachable!("probe always passes exactly one candidate"),
            Err(SequentialStunConnectError::StoppedEarly { failure, .. }) => stage_from_attempt_failure(&failure),
            Err(SequentialStunConnectError::AllCandidatesFailed { failures }) => stage_from_sequential_failures(&failures),
        };

        return Ok(ProbeReport {
            probe_schema_version: PROBE_SCHEMA_VERSION,
            profile: launch.profile,
            transport: "stun-p2p",
            dns_resolution,
            stun_discovery,
            handshake,
            target_reachability,
            stale_trust_suspected,
        });
    }

    let legacy_relay = entry.legacy_relay_transport.as_ref().with_context(|| {
        format!("isekai-pipe probe: profile {:?} has no cached relay transport to probe", launch.profile)
    })?;
    let session_secret = decode_secret(&legacy_relay.session_secret_b64)?;
    let stun_discovery = if launch.stun_servers.is_empty() {
        ProbeStageStatus::Skipped { reason: "no --stun-server given".to_string() }
    } else {
        ProbeStageStatus::Skipped {
            reason: "profile has no cached STUN evidence from bootstrap (cached_stun_observed_addr unset)".to_string(),
        }
    };
    let target = RelayTarget {
        helper_addr: legacy_relay
            .helper_addr
            .parse()
            .with_context(|| format!("isekai-pipe probe: invalid cached helper_addr {:?}", legacy_relay.helper_addr))?,
        server_name: "isekai-helper".to_string(),
        cert_sha256_hex: entry.server_identity.cert_sha256_hex.clone(),
        session_secret,
        // `isekai-pipe probe` diagnoses reachability against a
        // `PersistentProfile`, which doesn't carry a configured local
        // bind-port-range (that lives on `ConnectionIntent`, the real
        // connect path's input) — probing unrestricted is the closest
        // available approximation.
        local_bind_port_range: None,
    };
    let candidates = vec![SequentialRelayCandidate { target, candidate_id: "probe".to_string() }];
    // `requested_resume_grace_secs: 0` — a probe is a one-shot diagnostic
    // check, not a session to keep alive; dropping the returned session
    // immediately below closes the QUIC connection cleanly (the server sees
    // an ordinary disconnect, not a leak — no resume loop is ever started).
    let relay_result = connect_via_relay_resumable_with_fallback(&system_quic_factory(), &candidates, 0).await;
    let stale_trust_suspected = relay_result.as_ref().err().is_some_and(|e| e.is_stale_trust_signal());
    let (handshake, target_reachability) = match relay_result {
        Ok((session, _winning_target)) => {
            drop(session);
            (ProbeStageStatus::Ok { detail: None }, ProbeStageStatus::Ok { detail: None })
        }
        Err(e) => stage_from_relay_connect_error(&e),
    };

    Ok(ProbeReport {
        probe_schema_version: PROBE_SCHEMA_VERSION,
        profile: launch.profile,
        transport: "relay",
        dns_resolution,
        stun_discovery,
        handshake,
        target_reachability,
        stale_trust_suspected,
    })
}

async fn relay_stdio(stream: AnyByteStream) -> Result<()> {
    let (mut quic_read, mut quic_write) = stream.split();
    let mut c2h = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = stdin.read(&mut buf).await.context("reading stdin failed")?;
            if n == 0 {
                let _ = quic_write.shutdown().await;
                return Ok::<_, anyhow::Error>(());
            }
            quic_write
                .write_all(&buf[..n])
                .await
                .context("writing to remote stream failed")?;
        }
    });
    let mut h2c = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = quic_read
                .read(&mut buf)
                .await
                .context("reading remote stream failed")?;
            if n == 0 {
                return Ok::<_, anyhow::Error>(());
            }
            stdout
                .write_all(&buf[..n])
                .await
                .context("writing stdout failed")?;
            stdout.flush().await.context("flushing stdout failed")?;
        }
    });

    let (mut c2h_done, mut h2c_done) = (false, false);
    while !c2h_done || !h2c_done {
        tokio::select! {
            res = &mut c2h, if !c2h_done => {
                c2h_done = true;
                res.context("stdin->remote task panicked")??;
            }
            res = &mut h2c, if !h2c_done => {
                h2c_done = true;
                res.context("remote->stdout task panicked")??;
            }
        }
    }
    Ok(())
}

/// Narrow signal a retried-connect error type must expose for
/// [`retry_while_busy_other_session`] — named distinctly from the underlying
/// `TransportError`/`SequentialConnectError::is_busy_other_session` inherent
/// methods it delegates to, so calling `self.is_busy_other_session()` inside
/// each impl unambiguously reaches the inherent one rather than recursing.
trait BusyOtherSessionSignal {
    fn signals_busy_other_session(&self) -> bool;
}

impl BusyOtherSessionSignal for isekai_transport::TransportError {
    fn signals_busy_other_session(&self) -> bool {
        self.is_busy_other_session()
    }
}

impl BusyOtherSessionSignal for SequentialConnectError {
    fn signals_busy_other_session(&self) -> bool {
        self.is_busy_other_session()
    }
}

/// Retries `attempt` while — and only while — it fails with
/// `BUSY_OTHER_SESSION`, for up to `resume_window_for(requested_resume_grace_secs)`
/// (the same deadline a resume loop would use, since a `BUSY_OTHER_SESSION`
/// reject on the very first connect most often means *this same client's*
/// previous session is still parked on the remote helper, waiting out that
/// exact window after an earlier ungraceful disconnect — see
/// `TransportError::is_busy_other_session`'s docs). Every other failure is
/// returned immediately on the first attempt, unchanged from before this
/// wrapper existed: this only closes the gap where a fresh `isekai-pipe
/// connect` process (a brand new `session_id` every time, since neither
/// `connect_via_relay_resumable` nor `_with_fallback` persist one across
/// invocations) would otherwise fail outright instead of waiting the same
/// window a same-process resume would have.
async fn retry_while_busy_other_session<T, E, F, Fut>(requested_resume_grace_secs: u32, mut attempt: F) -> Result<T, E>
where
    E: BusyOtherSessionSignal,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let deadline = Instant::now() + resume_window_for(requested_resume_grace_secs);
    let mut attempt_no: u32 = 0;
    loop {
        let err = match attempt().await {
            Ok(ok) => return Ok(ok),
            Err(err) => err,
        };
        let now = Instant::now();
        if !err.signals_busy_other_session() || now >= deadline {
            return Err(err);
        }
        let delay = RESUME_BACKOFF.base_delay(attempt_no).min(deadline - now);
        attempt_no = attempt_no.saturating_add(1);
        eprintln!(
            "isekai-pipe connect: remote helper reports BUSY_OTHER_SESSION (likely this client's own prior \
             session still parked from an earlier disconnect); retrying in {delay:?}"
        );
        tokio::time::sleep(delay).await;
    }
}

async fn run_relay_resumable(
    target: &RelayTarget,
    profile: &str,
    requested_resume_grace_secs: u64,
    identity: isekai_transport::CandidateIdentity<'_>,
    experimental_network_rebind: bool,
    relay_transport: RelayTransportKind,
    tethering_interface: Option<isekai_transport::InterfaceIndex>,
) -> Result<()> {
    let factory = relay_endpoint_factory(relay_transport);
    let requested = u32::try_from(requested_resume_grace_secs).unwrap_or(u32::MAX);
    let established = retry_while_busy_other_session(requested, || connect_via_relay_resumable(&factory, target, requested, identity))
        .await
        .map_err(attach_stale_trust_signal)?;
    run_resume_loop(&factory, target, profile, established, experimental_network_rebind, tethering_interface).await
}

/// Like `run_relay_resumable`, but tries `candidates` in priority order
/// (`ISEKAI_PIPE_DESIGN.md` task #12: relay-endpoint fallback) instead of
/// dialing a single fixed target. Falls back only across pre-attach
/// failures — see `connect_via_relay_resumable_with_fallback`'s and
/// `AttemptFailure`'s docs for why an ambiguous or terminal failure on one
/// candidate stops the whole attempt rather than trying the next one.
async fn run_relay_resumable_with_fallback(
    candidates: &[SequentialRelayCandidate],
    profile: &str,
    requested_resume_grace_secs: u64,
    experimental_network_rebind: bool,
    relay_transport: RelayTransportKind,
    tethering_interface: Option<isekai_transport::InterfaceIndex>,
) -> Result<()> {
    let factory = relay_endpoint_factory(relay_transport);
    let requested = u32::try_from(requested_resume_grace_secs).unwrap_or(u32::MAX);
    let (established, winning_target) =
        retry_while_busy_other_session(requested, || connect_via_relay_resumable_with_fallback(&factory, candidates, requested))
            .await
            .map_err(attach_stale_trust_signal)?;
    run_resume_loop(&factory, &winning_target, profile, established, experimental_network_rebind, tethering_interface).await
}

/// Like the single-candidate `CandidateRoute::StunP2p` path in `run_connect`,
/// but tries `candidates` (each a different STUN server against the same
/// peer) in priority order (`#11`) instead of dialing a single fixed STUN
/// server. STUN P2P has no resume/control-stream concept (`stun_p2p.rs`'s
/// module docs), so — unlike `run_relay_resumable_with_fallback` — there is
/// no `run_resume_loop` step here: the winning candidate's stream goes
/// straight into `relay_stdio`, exactly like the legacy single-candidate path
/// already does.
async fn run_stun_p2p_with_fallback(target: &StunP2pTarget, candidates: &[SequentialStunCandidate]) -> Result<()> {
    let (connection, _winning_stun_server) = connect_stun_p2p_with_fallback(&system_quic_factory(), target, candidates)
        .await
        .map_err(attach_stale_trust_signal)?;
    relay_stdio(connection.stream).await
}

/// Runs the C2H/H2C data pump against `established`, resuming (via
/// `reconnect_and_resume` against `target` — the *specific* candidate that
/// won, in the fallback case) across disconnects until either the local side
/// closes cleanly or the resume window is exceeded. Shared by both
/// `run_relay_resumable` (single fixed target) and
/// `run_relay_resumable_with_fallback` (the winning target out of several
/// candidates) — resuming a session is always scoped to the one connection
/// that established it, never a fresh candidate search.
/// Picks an OS-assigned-ephemeral-port wildcard bind address matching
/// `remote`'s address family — the same "let the OS pick a fresh source"
/// approach `BindSpec::any_ipv4()` already uses for every *new* connection,
/// reused here for `AnyMuxRebinder::rebind`'s replacement socket. Not
/// an explicit interface choice (see `AnyMuxRebinder::rebind`'s docs):
/// just a fresh socket for the OS to route via its current default path,
/// which is what actually helps after e.g. a Wi-Fi disconnect where the OS
/// has since switched its default route to something else.
fn remote_bind_spec(remote: std::net::SocketAddr, local_bind_port_range: Option<(u16, u16)>) -> BindSpec {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
    let local_addr = if remote.is_ipv4() {
        SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)
    } else {
        SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0)
    };
    BindSpec { local_addr, port_range: local_bind_port_range }
}

/// Spawns this connection generation's "the current connection should be
/// abandoned and reconnected via RESUME" signal source for `run_resume_loop`,
/// and returns a task to `.abort()` once the caller's own `select!` resolves
/// (unconditionally — cheap/harmless to abort either shape below) alongside
/// a receiver that yields exactly once, the moment reconnection should
/// happen.
///
/// Two shapes, chosen by whether `rebinder` is both present and
/// `experimental_network_rebind` is set:
///
/// - **Default** (`experimental_network_rebind` off, or this generation's
///   `AnyMuxFactory` doesn't support rebinding): every OS-reported
///   network change (`isekai-netmon`; a no-op on platforms other than
///   Windows/macOS today) is forwarded immediately — this is exactly the
///   behavior this function replaced (`network_monitor.next_change()` raced
///   directly against `run_data_pump` in the same `select!`), just moved
///   into its own task so both shapes can feed the same channel.
/// - **Experimental with a rebinder**: tries `AnyMuxRebinder::rebind`
///   first on every change; only a *failed* rebind attempt is forwarded,
///   and this task then stops (that generation's endpoint is about to be
///   abandoned by the RESUME reconnect the failure triggers, so continuing
///   to watch it is pointless). A *successful* rebind is invisible to the
///   caller's `select!` entirely — `run_data_pump`'s QUIC stream keeps
///   running untouched, because `rebind` only swaps the endpoint's local
///   socket, never the connection/stream objects above it (the same
///   property Android's `multipath_transport.rs` relies on for its own
///   `rebind_abstract()`-based failover, verified there on real hardware).
///   `rebind`'s own success only means "the local socket switch itself
///   succeeded" — not that the new path can actually reach the peer, which
///   this task has no way to confirm; a rebind that succeeds but doesn't
///   restore connectivity eventually surfaces as an ordinary QUIC idle
///   timeout, same as before this feature existed.
///
/// `monitor` is a fresh `isekai_netmon::system_monitor()` from the caller
/// (rather than one long-lived instance shared across every generation)
/// because a rebinder is only valid for the specific endpoint it came from —
/// once a RESUME reconnect replaces that endpoint, the old rebinder (and, by
/// construction, the old task holding it) must not keep running, so each
/// connection generation gets its own task and its own OS registration
/// rather than one shared across the whole `run_resume_loop` call. Taken as
/// a parameter rather than constructed inside this function so tests can
/// inject a controllable mock instead of the real (on this development
/// platform, Linux, always-`NoopNetworkChangeMonitor`) OS-backed one.
/// Minimal async rebind interface this function needs — generic (not
/// boxed as `dyn`) so both the real `isekai_transport::AnyMuxRebinder` and
/// this module's own test-only mock can satisfy it. `AnyMuxRebinder` is a
/// plain enum (see its own docs on why: exactly one real backend supports
/// rebinding today, so a trait-object hierarchy would be overkill) with no
/// public constructor for a fake value, so a test that wants to exercise
/// "rebind succeeds"/"rebind fails" without a real `noq` endpoint needs its
/// own minimal seam instead of constructing an `AnyMuxRebinder` directly.
trait Rebindable: Send {
    fn rebind(&self, bind: BindSpec) -> impl std::future::Future<Output = Result<(), isekai_transport::MuxError>> + Send;
}

impl Rebindable for AnyMuxRebinder {
    fn rebind(&self, bind: BindSpec) -> impl std::future::Future<Output = Result<(), isekai_transport::MuxError>> + Send {
        AnyMuxRebinder::rebind(self, bind)
    }
}

fn spawn_reconnect_signal<R: Rebindable + 'static>(
    monitor: Box<dyn isekai_netmon::NetworkChangeMonitor>,
    rebinder: Option<R>,
    experimental_network_rebind: bool,
    helper_addr: std::net::SocketAddr,
    local_bind_port_range: Option<(u16, u16)>,
) -> (tokio::task::JoinHandle<()>, tokio::sync::mpsc::Receiver<()>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<()>(1);
    let handle = tokio::spawn(async move {
        let mut network_monitor = monitor;
        match (experimental_network_rebind, rebinder) {
            (true, Some(rebinder)) => {
                let bind = remote_bind_spec(helper_addr, local_bind_port_range);
                while network_monitor.next_change().await.is_some() {
                    log::info!("isekai-pipe connect: rebind_attempted");
                    match rebinder.rebind(bind).await {
                        Ok(()) => {
                            log::info!(
                                "isekai-pipe connect: rebind ok, continuing existing connection"
                            );
                        }
                        Err(e) => {
                            log::warn!("isekai-pipe connect: rebind_immediate_error: {e}");
                            let _ = tx.send(()).await;
                            return;
                        }
                    }
                }
            }
            _ => {
                if network_monitor.next_change().await.is_some() {
                    log::info!(
                        "isekai-pipe connect: OS reported a network change; treating the current connection \
                         as stale and reconnecting now instead of waiting for it to time out"
                    );
                    let _ = tx.send(()).await;
                }
            }
        }
    });
    (handle, rx)
}

/// Writes `replay`'s buffered-but-unacknowledged bytes past `committed_offset`
/// onto a freshly (re)established `stream`, then discards them from `replay`
/// on success — shared by both `run_resume_loop`'s `WarmStandby::promote`
/// fast path and its ordinary `reconnect_and_resume` retry loop below, since
/// both hand back a resumed connection with the same "helper says it
/// committed up to X" offset semantics. Returns `false` (leaving `replay`
/// untouched past `committed_offset`) if the write itself fails, so the
/// caller knows to discard `stream` and retry instead of treating it as
/// live.
///
/// Also returns `false` — without touching `replay` at all — when
/// `committed_offset` is outside `replay`'s buffered range
/// (`ReplayBuffer::replay_from` returning `None`): either the helper's
/// claimed offset is *behind* what this client already discarded as
/// confirmed (bytes were dropped without ever actually being acknowledged —
/// data loss), or *ahead* of everything this client has ever sent (the
/// helper claims to have committed bytes that don't exist). Both are
/// protocol inconsistencies, not "nothing to replay" — silently proceeding
/// (as an earlier version of this function did) would desync this client's
/// own offset bookkeeping from the helper's, corrupting every future
/// `client_sent_offset` this session reports (codex review,
/// quicmux-server-resume).
async fn replay_and_advance(replay: &Mutex<C2hReplayBuffer>, committed_offset: u64, stream: &mut AnyByteStream) -> bool {
    let Some(bytes) = replay.lock().unwrap().replay_from(committed_offset) else {
        eprintln!(
            "isekai-pipe connect: helper's committed_offset={committed_offset} is outside the local \
             replay buffer's range — treating this resumed connection as unusable and retrying"
        );
        return false;
    };
    if !bytes.is_empty() && stream.write_all(&bytes).await.is_err() {
        return false;
    }
    replay.lock().unwrap().advance_start(committed_offset);
    true
}

/// Reestablishes the control stream on `conn` — a freshly resumed connection
/// from either `reconnect_and_resume` or `WarmStandby::promote` — and
/// resumes the `APP_ACK` background exchange against the *same* `counters`
/// this whole `run_resume_loop` call already uses (not a fresh
/// `AppAckCounters`: `pump_c2h`'s backpressure trim reads
/// `counters.c2h_helper_committed_offset()` directly every iteration, so a
/// new instance here would silently desync from what the data pump is
/// actually watching). Without this, `counters.c2h_helper_committed_offset`
/// would freeze at whatever it was when the *first* disconnect happened —
/// `pump_c2h` would then never see it advance again, and the C2H replay
/// buffer would fill to `C2H_REPLAY_BUFFER_CAPACITY` and stall stdin reads
/// (codex review, quicmux-server-resume — the same class of gap already
/// fixed for `isekai-terminal-core`'s three Android transports via
/// `spawn_control_stream_reestablishment_after_resume`, just missed here
/// since this is the separate CLI binary).
///
/// Synchronous (unlike the Android fix's fire-and-forget/timeout-bounded
/// spawn) to match this function's own caller: `connect_via_relay_resumable`
/// already treats the *initial* control stream as a required, synchronous
/// step (`?`, not a best-effort background task) — Android's leniency is
/// specifically about not delaying an SSH shell handoff for a possibly-slow
/// legacy helper, which doesn't apply to reattaching an already-open resume
/// loop against isekai's own server.
async fn reestablish_control_stream(
    conn: &AnyMuxConnection,
    session_secret: &[u8],
    counters: &Arc<AppAckCounters>,
) -> Result<AppAckTasks> {
    let proof = compute_proof(conn, session_secret, b"").await?;
    let control = open_control_stream(conn, &proof).await?;
    Ok(spawn_app_ack_tasks(control.stream, counters.clone()))
}

/// The server clamps our request to its own configured max (or applies its
/// own default when we requested `0`) and echoes back what it actually
/// granted — that, not our own request, is the real deadline: the server
/// will have already discarded the parked session past this point
/// regardless of how long we keep retrying (`ISEKAI_PIPE_DESIGN.md`).
///
/// `0` itself is treated as "no real value was ever learned" rather than a
/// literal zero-second window: `isekai-transport::resume::finish_via_resume`
/// (the `MustResume` ambiguous-attach convergence path) has no ATTACH_HELLO
/// exchange to learn the server's actual grant from, and — even after that
/// function's own fix to fall back to the caller's originally *requested*
/// grace period instead of hardcoding `0` — a caller that itself requested
/// `0` (isekai-ssh/isekai-pipe connect's own "let the server pick its
/// default" convention) still produces `0` here. Without this fallback, any
/// session that ever passed through that convergence path would give up on
/// its very first subsequent disconnect instead of resuming at all (codex
/// review, quicmux-server-resume).
fn resume_window_for(effective_resume_grace_secs: u32) -> Duration {
    match effective_resume_grace_secs {
        0 => DEFAULT_RESUME_WINDOW,
        secs => Duration::from_secs(secs.into()),
    }
}

// ── tssh風のライブ再接続表示(`run_resume_loop`専用) ──────────────
//
// `isekai-pipe connect` は `ssh(1)` の ProxyCommand として起動され、OpenSSH の
// 仕様上 stderr は通常 ssh 自身の stderr(＝ユーザーの実端末)にそのまま
// 継承される。tssh(trzsz-ssh)本家のUDPモードreconnectと同じく、stderrに
// 直接 `\r` + ANSI エスケープでその場書き換えするだけで、Android アプリ側
// (`rust-core/src/orchestrator.rs`)のように新しいUI基盤を用意しなくても
// ライブな状態表示ができる。
//
// ただし `isekai-ssh --log-file` 相当が有効な場合、`ssh` の stderr は
// 端末ではなくログファイルへpipeされる(`isekai-ssh/src/wrapper.rs`の
// `log_file::is_enabled()`)。この場合に `\r`/ANSI を出すとログファイルが
// 読めない制御文字だらけになるため、`is_terminal()` で分岐し、非TTY時は
// 改行区切りの平文へフォールバックする。

/// 再接続中の状態メッセージを組み立てる。TTY時は`\r`+ANSI色でその場書き換え
/// 用(呼び出し側で`eprint!`し、改行しない)、非TTY時はログファイル向けの
/// 改行区切り平文(呼び出し側で`eprintln!`する)。副作用を持たない純粋関数
/// として切り出してあり、単体テストしやすい。
fn format_reconnect_status(is_tty: bool, elapsed_secs: u64, total_secs: u64) -> String {
    if is_tty {
        format!(
            "\r\x1b[0;33misekai-pipe connect: connection lost, trying to reconnect... ({elapsed_secs}s/{total_secs}s)\x1b[0m\x1b[K"
        )
    } else {
        format!(
            "isekai-pipe connect: connection lost, trying to reconnect... ({elapsed_secs}s/{total_secs}s elapsed)"
        )
    }
}

fn print_reconnect_status(is_tty: bool, disconnected_at: Instant, resume_window: Duration) {
    let elapsed_secs = Instant::now().saturating_duration_since(disconnected_at).as_secs();
    let msg = format_reconnect_status(is_tty, elapsed_secs, resume_window.as_secs());
    if is_tty {
        eprint!("{msg}");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    } else {
        eprintln!("{msg}");
    }
}

fn print_reconnect_success(is_tty: bool) {
    if is_tty {
        eprintln!("\r\x1b[0;32misekai-pipe connect: reconnected.\x1b[0m\x1b[K");
    } else {
        eprintln!("isekai-pipe connect: reconnected.");
    }
}

/// TTY時のみ呼ばれる: 1回のバックオフ待機(`delay`)を最大1秒刻みに分割し、
/// 都度その場書き換えでカウントダウンを再描画する。`delay`全体を素通しで
/// 待つのと合計の待ち時間は変わらない(`RESUME_BACKOFF`/`deadline`の意味は
/// 変えない、表示だけの変更)。
/// タイミング(何回・どれだけ待つか)と実際の描画処理(`on_tick`)を分離してある
/// ―― `print_reconnect_status`が直接I/Oを行うため、タイミングだけを
/// `tokio::time::pause()`で決定的にテストできるようにするため。
async fn sleep_with_live_status(delay: Duration, mut on_tick: impl FnMut()) {
    // `tokio::time::Instant`を使う(`std::time::Instant`ではない) —
    // `tokio::time::pause()`/`advance()`が影響するのはtokio自身の時計だけで、
    // OSの実時計(`std::time::Instant::now()`)は素通りする。混在させると
    // テストでpause中に`remaining`がほぼ縮まらずビジーループする
    // (実際にこの取り違えで発生した不具合、テストで検出)。
    let wake_at = tokio::time::Instant::now() + delay;
    loop {
        let remaining = wake_at.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(remaining.min(Duration::from_secs(1))).await;
        on_tick();
    }
}

async fn run_resume_loop(
    factory: &AnyMuxFactory,
    target: &RelayTarget,
    profile: &str,
    established: isekai_transport::ResumableRelaySession,
    experimental_network_rebind: bool,
    tethering_interface: Option<isekai_transport::InterfaceIndex>,
) -> Result<()> {
    let session_id = established.session_id;
    drop(established.connection);

    let resume_window = resume_window_for(established.effective_resume_grace_secs);

    let counters = Arc::new(AppAckCounters::new());
    let mut app_ack_tasks = spawn_app_ack_tasks(established.control_stream, counters.clone());
    let replay = Arc::new(Mutex::new(C2hReplayBuffer::new(C2H_REPLAY_BUFFER_CAPACITY)));

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut data_stream = established.data_stream;
    let mut disconnected_since: Option<Instant> = None;
    let mut attempt: u32 = 0;
    let mut network_rebinder = established.network_rebinder;
    // tssh風のライブ再接続表示(このループ内でのみ使う、詳細は上のモジュール
    // ドキュメント参照)。プロセスの生存期間中に変わることは無いのでループ
    // 開始前に1回だけ判定する。
    let is_tty = std::io::stderr().is_terminal();
    let mut last_resume_error: Option<String> = None;

    // `--tethering-interface`: keeps a second connection warm on a specific
    // physical interface and promotes it (no fresh dial, no backoff wait) as
    // the first thing tried on disconnect, below — see `warm_standby.rs`'s
    // module docs. `None` when the flag wasn't given; every use below is a
    // no-op in that case, matching this codebase's "opportunistic,
    // default-off" convention for experimental features.
    let warm_standby = tethering_interface
        .map(|iface| Arc::new(isekai_transport::WarmStandby::new_bound_to_interface(factory.clone(), target.clone(), session_id, iface)));
    let warm_standby_task = warm_standby.clone().map(|ws| {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(WARM_STANDBY_PROBE_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                if let Err(e) = ws.ensure_warm().await {
                    log::warn!("isekai-pipe connect: warm-standby ensure_warm failed: {e:#}");
                }
            }
        })
    });

    loop {
        // See `spawn_reconnect_signal`'s docs for the full design rationale
        // (this replaces what used to be a single `network_monitor` shared
        // across the whole loop, racing `run_data_pump` directly — that
        // shape cancelled the data pump, and the QUIC stream halves split
        // out of `data_stream` below with it, the instant *any* network
        // change fired, leaving no way to try a fast rebind without losing
        // the stream first).
        let (reconnect_signal_task, mut reconnect_signal_rx) = spawn_reconnect_signal(
            isekai_netmon::system_monitor(),
            network_rebinder.take(),
            experimental_network_rebind,
            target.helper_addr,
            target.local_bind_port_range,
        );

        let (mut quic_read, mut quic_write) = data_stream.split();
        let outcome = tokio::select! {
            outcome = run_data_pump(&mut stdin, &mut stdout, &mut quic_read, &mut quic_write, &replay, &counters) => outcome,
            Some(()) = reconnect_signal_rx.recv() => {
                Err(anyhow::anyhow!("network change detected, reconnecting"))
            }
        };
        reconnect_signal_task.abort();
        app_ack_tasks.abort();

        if outcome.is_ok() {
            if let Some(t) = &warm_standby_task {
                t.abort();
            }
            return Ok(());
        }

        // Abandoning this connection (network change, or run_data_pump's own
        // I/O failure) — explicitly reset the send side instead of letting
        // it drop gracefully. `noq`/`qmux`'s `Drop` for a send stream calls
        // `finish()` (a clean FIN) by default, which `isekai-pipe serve`'s
        // `relay_buffered` cannot distinguish from a legitimate half-close
        // (e.g. stdin EOF, where S→C must keep flowing) — so it leaves the
        // session `Established`-but-never-parked on this now-dead
        // connection instead of parking it for resume, and every subsequent
        // RESUME then fails as "not resumable" (`UnknownSession`) forever
        // (found via live debugging, 2026-07-11: the very next reconnect
        // attempt after a network-change-triggered abandon got exactly this
        // rejection). A reset instead makes the server's read return an
        // error, correctly classified as `RelayOutcome::DataStreamDied` and
        // parked.
        quic_write.reset(0);

        // The resume window's clock starts here, at disconnect detection —
        // before the fast-path promote attempt below, not after it, so a
        // slow-to-fail promote still counts against the deadline the same as
        // a slow-to-fail `reconnect_and_resume` attempt would.
        let disconnected_at = *disconnected_since.get_or_insert_with(Instant::now);
        let deadline = disconnected_at + resume_window;
        // tssh風のライブ再接続表示: 切断検知の瞬間に即座に1回出す
        // (これが無いと、最初の再接続試行が失敗するまで何も表示されない)。
        print_reconnect_status(is_tty, disconnected_at, resume_window);

        // Fast path: promote the already-warm standby connection instead of
        // waiting through the backoff loop below — the entire point of
        // keeping one warm (`warm_standby.rs`'s module docs). A missing
        // standby, an in-flight promotion, or a transport failure all fall
        // straight through to the ordinary `reconnect_and_resume` retry loop
        // unchanged; this is a latency optimization, not a correctness
        // dependency.
        let mut promoted_stream = None;
        if let Some(ws) = &warm_standby {
            let client_sent_offset = C2hSentOffset::new(replay.lock().unwrap().end_offset());
            let client_delivered_offset = H2cClientDeliveredOffset::new(counters.h2c_client_delivered_offset());
            match ws.promote(client_sent_offset, client_delivered_offset).await {
                Ok(mut promoted) => {
                    if replay_and_advance(&replay, promoted.helper_committed_offset.get(), &mut promoted.data_stream).await {
                        log::info!("isekai-pipe connect: promoted warm-standby connection for session_id={session_id}");
                        print_reconnect_success(is_tty);
                        match reestablish_control_stream(&promoted.connection, &target.session_secret, &counters).await {
                            Ok(new_tasks) => app_ack_tasks = new_tasks,
                            Err(e) => eprintln!(
                                "isekai-pipe connect: control stream re-establishment after promote failed ({e:#}), \
                                 continuing without resume support until the next reattach"
                            ),
                        }
                        drop(promoted.connection);
                        // The promoted connection was dialed directly by
                        // `WarmStandby`, not via the endpoint this
                        // generation's `network_rebinder` came from — no
                        // rebinder to carry over; the next disconnect just
                        // falls back to a full resume, same as any other
                        // rebinder-less generation.
                        network_rebinder = None;
                        promoted_stream = Some(promoted.data_stream);
                    } else {
                        let msg = "warm-standby promote succeeded but replay failed; falling back to full resume";
                        // TTY時はその場書き換え中のライブ表示行を壊さないよう、
                        // まず行をクリアしてから改行付きで出す(このメッセージ自体は
                        // 1episodeにつき最大1回で、per-attempt的な連発ではないため
                        // debugログへは落とさない)。
                        if is_tty {
                            eprintln!("\r\x1b[Kisekai-pipe connect: {msg}");
                        } else {
                            eprintln!("isekai-pipe connect: {msg}");
                        }
                        last_resume_error = Some(msg.to_string());
                    }
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    log::info!("isekai-pipe connect: warm-standby promote unavailable ({msg}); falling back to full resume");
                    last_resume_error = Some(msg);
                }
            }
        }

        let new_stream = match promoted_stream {
            Some(stream) => stream,
            None => loop {
                let now = Instant::now();
                if now >= deadline {
                    let exceeded_by = now.saturating_duration_since(deadline);
                    if is_tty {
                        // その場書き換え中だったライブ表示行をクリアしてから
                        // ギブアップメッセージを改行付きで出す。
                        eprint!("\r\x1b[K");
                    }
                    let last_error_suffix = last_resume_error
                        .as_deref()
                        .map(|e| format!(" Last error: {e}."))
                        .unwrap_or_default();
                    eprintln!(
                        "isekai-pipe connect: giving up on session_id={session_id} for '{profile}' - \
                         the resume window ({resume_window:?}) was exceeded by {exceeded_by:?}.{last_error_suffix} \
                         Closing stdin/stdout; ssh will treat this as a lost connection."
                    );
                    let _ = stdout.shutdown().await;
                    drop(stdin);
                    if let Some(t) = &warm_standby_task {
                        t.abort();
                    }
                    return Ok(());
                }

                let delay = RESUME_BACKOFF.base_delay(attempt).min(deadline - now);
                attempt = attempt.saturating_add(1);
                if is_tty {
                    sleep_with_live_status(delay, || print_reconnect_status(true, disconnected_at, resume_window)).await;
                } else {
                    tokio::time::sleep(delay).await;
                }

                let client_sent_offset = C2hSentOffset::new(replay.lock().unwrap().end_offset());
                let client_delivered_offset =
                    H2cClientDeliveredOffset::new(counters.h2c_client_delivered_offset());
                match reconnect_and_resume(
                    factory,
                    target,
                    session_id,
                    client_sent_offset,
                    client_delivered_offset,
                )
                .await
                {
                    Ok(mut resumed) => {
                        if !replay_and_advance(&replay, resumed.helper_committed_offset.get(), &mut resumed.data_stream).await {
                            // resume自体は成功したがreplayが不整合 —実質「この
                            // 試行は失敗した」ので、既存のErr(e)アームと同じ
                            // TTY/非TTY分岐・last_resume_error更新を行う
                            // (Codexレビューで指摘: このcontinue経路だけ元々
                            // 何も表示せず`last_resume_error`も更新していなかった)。
                            let msg = "resume succeeded but replay failed".to_string();
                            if is_tty {
                                log::debug!("isekai-pipe connect: resume attempt {attempt} {msg}");
                            } else {
                                eprintln!("isekai-pipe connect: resume attempt {attempt} {msg}");
                            }
                            last_resume_error = Some(msg);
                            continue;
                        }
                        print_reconnect_success(is_tty);
                        match reestablish_control_stream(&resumed.connection, &target.session_secret, &counters).await {
                            Ok(new_tasks) => app_ack_tasks = new_tasks,
                            Err(e) => eprintln!(
                                "isekai-pipe connect: control stream re-establishment after resume failed ({e:#}), \
                                 continuing without resume support until the next reattach"
                            ),
                        }
                        drop(resumed.connection);
                        network_rebinder = resumed.network_rebinder;
                        break resumed.data_stream;
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        // TTY時はその場書き換えのライブ表示とスクロール表示が
                        // 混ざるとUXを壊すため、個々の失敗はdebugログへ格下げする
                        // (既定の`info`フィルタでは表示されない、`RUST_LOG=debug`
                        // で見られる)。非TTY(ログファイル等)では引き続き
                        // eprintln!のまま残す — ログでは個々の失敗を追えることの
                        // 方が重要なため。
                        if is_tty {
                            log::debug!("isekai-pipe connect: resume attempt {attempt} failed: {msg}");
                        } else {
                            eprintln!("isekai-pipe connect: resume attempt {attempt} failed: {msg}");
                        }
                        last_resume_error = Some(msg);
                    }
                }
            },
        };

        data_stream = new_stream;
        disconnected_since = None;
        last_resume_error = None;
        attempt = 0;
    }
}

async fn run_data_pump(
    stdin: &mut (impl AsyncRead + Unpin),
    stdout: &mut (impl AsyncWrite + Unpin),
    quic_read: &mut AnyByteStreamReadHalf,
    quic_write: &mut AnyByteStreamWriteHalf,
    replay: &Arc<Mutex<C2hReplayBuffer>>,
    counters: &Arc<AppAckCounters>,
) -> Result<()> {
    let c2h_fut = pump_c2h(stdin, quic_write, replay.clone(), counters.clone());
    let h2c_fut = pump_h2c(quic_read, stdout, counters.clone());
    tokio::pin!(c2h_fut);
    tokio::pin!(h2c_fut);

    let mut c2h_done = false;
    let mut h2c_done = false;
    loop {
        tokio::select! {
            res = &mut c2h_fut, if !c2h_done => {
                res.context("isekai-pipe connect: C2H pump failed")?;
                c2h_done = true;
            }
            res = &mut h2c_fut, if !h2c_done => {
                res.context("isekai-pipe connect: H2C pump failed")?;
                h2c_done = true;
            }
        }
        if c2h_done && h2c_done {
            return Ok(());
        }
    }
}

async fn pump_c2h(
    stdin: &mut (impl AsyncRead + Unpin),
    quic_write: &mut AnyByteStreamWriteHalf,
    replay: Arc<Mutex<C2hReplayBuffer>>,
    counters: Arc<AppAckCounters>,
) -> Result<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        loop {
            let mut r = replay.lock().unwrap();
            r.advance_start(counters.c2h_helper_committed_offset());
            if !r.is_full() {
                break;
            }
            drop(r);
            tokio::time::sleep(BACKPRESSURE_POLL_INTERVAL).await;
        }

        let read_len = buf.len().min(replay.lock().unwrap().remaining_capacity());
        let n = stdin
            .read(&mut buf[..read_len])
            .await
            .context("reading stdin failed")?;
        if n == 0 {
            let _ = quic_write.shutdown().await;
            return Ok(());
        }
        quic_write
            .write_all(&buf[..n])
            .await
            .context("writing to remote stream failed")?;
        replay.lock().unwrap().append(&buf[..n]);
    }
}

async fn pump_h2c(
    quic_read: &mut AnyByteStreamReadHalf,
    stdout: &mut (impl AsyncWrite + Unpin),
    counters: Arc<AppAckCounters>,
) -> Result<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = quic_read
            .read(&mut buf)
            .await
            .context("reading remote stream failed")?;
        if n == 0 {
            return Ok(());
        }
        stdout
            .write_all(&buf[..n])
            .await
            .context("writing stdout failed")?;
        stdout.flush().await.context("flushing stdout failed")?;
        counters.advance_h2c_client_delivered_offset(n as u64);
    }
}

struct C2hReplayBuffer {
    data: VecDeque<u8>,
    start_offset: u64,
    capacity: usize,
}

impl C2hReplayBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(capacity.min(1 << 20)),
            start_offset: 0,
            capacity,
        }
    }

    fn is_full(&self) -> bool {
        self.data.len() >= self.capacity
    }

    fn remaining_capacity(&self) -> usize {
        self.capacity.saturating_sub(self.data.len())
    }

    fn append(&mut self, bytes: &[u8]) {
        debug_assert!(
            self.data.len() + bytes.len() <= self.capacity,
            "C2hReplayBuffer::append called past capacity"
        );
        self.data.extend(bytes.iter().copied());
    }

    fn advance_start(&mut self, confirmed_offset: u64) {
        let wanted = confirmed_offset.saturating_sub(self.start_offset) as usize;
        let drop_count = wanted.min(self.data.len());
        self.data.drain(..drop_count);
        self.start_offset += drop_count as u64;
        if confirmed_offset > self.start_offset {
            self.start_offset = confirmed_offset;
        }
    }

    fn end_offset(&self) -> u64 {
        self.start_offset + self.data.len() as u64
    }

    fn replay_from(&self, from: u64) -> Option<Vec<u8>> {
        if from < self.start_offset || from > self.end_offset() {
            return None;
        }
        let skip = (from - self.start_offset) as usize;
        Some(self.data.iter().skip(skip).copied().collect())
    }
}

fn parse_serve(args: impl Iterator<Item = String>) -> Result<Option<ServeLaunch>, ExitCode> {
    let mut service: Option<ServiceSpec> = None;
    let mut helper_args = Vec::new();
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("USAGE:");
                println!("    isekai-pipe serve --service ssh=127.0.0.1:22 [HELPER_OPTIONS]");
                println!();
                println!("COMPATIBILITY:");
                println!("    --target 127.0.0.1:22 is accepted as --service ssh=127.0.0.1:22");
                println!("    Existing helper protocol clients are still supported.");
                return Ok(None);
            }
            "--service" => {
                let value = next_arg("serve", &mut iter, "--service").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                let spec = ServiceSpec::parse(&value).map_err(|e| {
                    eprintln!("isekai-pipe serve: invalid --service {value:?}: {e}");
                    ExitCode::from(EX_USAGE)
                })?;
                if spec.name().as_str() != "ssh" {
                    eprintln!(
                        "isekai-pipe serve: only ssh service is wired to the helper runtime for now"
                    );
                    return Err(ExitCode::from(EX_USAGE));
                }
                if service.replace(spec).is_some() {
                    eprintln!("isekai-pipe serve: only one --service is supported for now");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
            "--target" => {
                let value = next_arg("serve", &mut iter, "--target").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                let spec = ServiceSpec::ssh_target(value).map_err(|e| {
                    eprintln!("isekai-pipe serve: invalid --target: {e}");
                    ExitCode::from(EX_USAGE)
                })?;
                if service.replace(spec).is_some() {
                    eprintln!("isekai-pipe serve: --target conflicts with --service");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
            "--once" => helper_args.push(arg),
            "--bind"
            | "--idle-timeout"
            | "--resume-window"
            | "--resume-buffer-size"
            | "--max-idle-lifetime"
            | "--max-sessions"
            | "--stun-server"
            | "--punch-peer"
            | "--relay"
            | "--relay-sni"
            | "--relay-jwt"
            | "--relay-jwt-file"
            | "--bootstrap-request-file"
            | "--log-level" => {
                let value = next_arg("serve", &mut iter, &arg).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                helper_args.push(arg);
                helper_args.push(value);
            }
            other => {
                eprintln!("isekai-pipe serve: unsupported option: {other}");
                return Err(ExitCode::from(EX_USAGE));
            }
        }
    }

    let Some(service) = service else {
        eprintln!("isekai-pipe serve: at least one --service is required");
        return Err(ExitCode::from(EX_USAGE));
    };
    Ok(Some(ServeLaunch {
        service,
        helper_args,
    }))
}

/// The `-R` remote path convention `isekai-ssh`'s `ctl_forward.rs` uses
/// (`/tmp/isekai-pipe-ctl-<128bit hex>.sock`, opt-in `#@isekai ctl-socket
/// yes`, `ISEKAI_PIPE_DESIGN.md` §8 Epic M). `sshd` owns cleaning up the
/// actual streamlocal forward bind on a normal disconnect; this sweep only
/// catches what's left behind by abnormal exits (crash, `kill -9`, a
/// network drop that skipped `ssh -O cancel -R`).
const CTL_SOCKET_REMOTE_PREFIX: &str = "isekai-pipe-ctl-";
const CTL_SOCKET_STALE_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60);

/// Best-effort, non-fatal: a sweep failure (e.g. `/tmp` unreadable for
/// some reason) should never block `serve` from starting.
fn sweep_stale_ctl_sockets_on_remote() {
    match isekai_pipe_core::sweep_stale_sockets(
        std::path::Path::new("/tmp"),
        CTL_SOCKET_REMOTE_PREFIX,
        CTL_SOCKET_STALE_THRESHOLD,
    ) {
        Ok(removed) if !removed.is_empty() => {
            log::info!("isekai-pipe serve: swept {} stale ctl-socket file(s) under /tmp", removed.len());
        }
        Ok(_) => {}
        Err(e) => {
            log::warn!("isekai-pipe serve: failed to sweep stale ctl-socket files under /tmp: {e}");
        }
    }
}

async fn serve_command(args: impl Iterator<Item = String>) -> ExitCode {
    let launch = match parse_serve(args) {
        Ok(Some(launch)) => launch,
        Ok(None) => return ExitCode::SUCCESS,
        Err(code) => return code,
    };

    sweep_stale_ctl_sockets_on_remote();

    let mut helper_args = launch.helper_args;
    helper_args.push("--service-name".to_string());
    helper_args.push(launch.service.name().as_str().to_string());
    helper_args.push("--target".to_string());
    helper_args.push(launch.service.target().to_string());

    match engine::run_from_args(helper_args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:?}");
            ExitCode::from(EX_UNAVAILABLE)
        }
    }
}

// ---------------------------------------------------------------------
// inspect: passive `PersistentProfile` state dump (`ISEKAI_PIPE_DESIGN.md`
// §8 Epic E). Never opens a socket — everything here reads only what's
// already on disk. Secrets (`legacy_relay_transport.session_secret_b64`)
// are never surfaced, with or without `--redact`; `--redact` additionally
// hides other network-topology-identifying values (full endpoint lists,
// `last_via`, `cached_stun_observed_addr`, and truncates the cert
// fingerprint) so output can be pasted into a bug report without leaking
// where a profile actually points.
// ---------------------------------------------------------------------

const INSPECT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug)]
struct InspectLaunch {
    profile: String,
    json: bool,
    redact: bool,
    verbose: bool,
}

fn parse_inspect(args: impl Iterator<Item = String>) -> Result<Option<InspectLaunch>, ExitCode> {
    let mut profile: Option<String> = None;
    let mut json = false;
    let mut redact = false;
    let mut verbose = false;
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("USAGE:");
                println!("    isekai-pipe inspect --profile production [--json] [--redact] [--verbose]");
                return Ok(None);
            }
            "--profile" => {
                let value = next_arg("inspect", &mut iter, "--profile").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                if profile.replace(value).is_some() {
                    eprintln!("isekai-pipe inspect: only one --profile is supported");
                    return Err(ExitCode::from(EX_USAGE));
                }
            }
            "--json" => json = true,
            "--redact" => redact = true,
            "--verbose" => verbose = true,
            other => {
                eprintln!("isekai-pipe inspect: unknown argument {other:?}");
                return Err(ExitCode::from(EX_USAGE));
            }
        }
    }
    let Some(profile) = profile else {
        eprintln!("isekai-pipe inspect: --profile is required");
        return Err(ExitCode::from(EX_USAGE));
    };
    Ok(Some(InspectLaunch { profile, json, redact, verbose }))
}

async fn inspect_command(args: impl Iterator<Item = String>) -> ExitCode {
    let launch = match parse_inspect(args) {
        Ok(Some(launch)) => launch,
        Ok(None) => return ExitCode::SUCCESS,
        Err(code) => return code,
    };
    match run_inspect(launch) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:?}");
            ExitCode::from(EX_UNAVAILABLE)
        }
    }
}

fn run_inspect(launch: InspectLaunch) -> Result<()> {
    // Profiles are always written under the normalized `host:port` key
    // (`isekai-ssh`'s `init`/wrapper, `isekai-pipe connect`'s own
    // `intent_from_profile` path) — `inspect` must resolve the same way, or
    // `--profile myhost` (without an explicit port) would look for a
    // `myhost.json` that never exists.
    let key = isekai_trust::normalize_host_port(&launch.profile)
        .with_context(|| format!("isekai-pipe inspect: invalid profile {:?}", launch.profile))?;
    let profiles_dir = default_profiles_dir().context("isekai-pipe inspect: could not determine profiles directory")?;
    let profile = load_persistent_profile(&profiles_dir, &key)
        .with_context(|| format!("isekai-pipe inspect: failed to load profile from {}", profiles_dir.display()))?
        .with_context(|| format!("isekai-pipe inspect: profile {:?} not found (looked up as {key:?} in {})", launch.profile, profiles_dir.display()))?;

    let report = build_inspect_report(&profile, launch.verbose, launch.redact);
    if launch.json {
        println!("{}", serde_json::to_string_pretty(&report).expect("InspectReport always serializes"));
    } else {
        print_inspect_report_human(&report);
    }
    Ok(())
}

#[derive(Debug, serde::Serialize)]
struct InspectReport {
    inspect_schema_version: u32,
    profile: String,
    profile_schema_version: u32,
    service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_id: Option<String>,
    server_identity_cert_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_channel: Option<String>,
    update_policy: isekai_trust::UpdatePolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_bootstrap_at: Option<String>,
    last_seen_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_via: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_path_hint: Option<InspectPathHint>,
    endpoints: InspectEndpoints,
    credential: InspectCredential,
    #[serde(skip_serializing_if = "Option::is_none")]
    stun_observed_addr: Option<String>,
    redacted: bool,
}

#[derive(Debug, serde::Serialize)]
struct InspectPathHint {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct InspectEndpoints {
    link_count: usize,
    rendezvous_count: usize,
    stun_server_count: usize,
    relay_endpoint_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    link: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rendezvous: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stun_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    relay_endpoints: Option<Vec<String>>,
}

#[derive(Debug, serde::Serialize)]
struct InspectCredential {
    present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
}

/// Truncates a hex certificate fingerprint for `--redact` — enough left to
/// eyeball "is this the same profile as before", not enough to identify the
/// actual peer.
fn redact_cert_sha256(hex: &str) -> String {
    match hex.get(..12) {
        Some(prefix) => format!("{prefix}…"),
        None => "…".to_string(),
    }
}

fn build_inspect_report(profile: &isekai_pipe_core::PersistentProfile, verbose: bool, redact: bool) -> InspectReport {
    let show_lists = verbose && !redact;
    InspectReport {
        inspect_schema_version: INSPECT_SCHEMA_VERSION,
        profile: profile.profile.clone(),
        profile_schema_version: profile.schema_version,
        service: profile.service.clone(),
        peer_id: profile.peer_id.clone(),
        server_identity_cert_sha256: if redact {
            redact_cert_sha256(&profile.server_identity.cert_sha256_hex)
        } else {
            profile.server_identity.cert_sha256_hex.clone()
        },
        remote_version: profile.remote_version.clone(),
        release_channel: profile.release_channel.clone(),
        update_policy: profile.update_policy,
        last_bootstrap_at: profile.last_bootstrap_at.clone(),
        last_seen_at: profile.last_seen_at.clone(),
        last_via: if redact { None } else { profile.last_via.clone() },
        last_path_hint: profile
            .last_path_hint
            .as_ref()
            .map(|hint| InspectPathHint { kind: hint.kind.clone(), expires_at: hint.expires_at.clone() }),
        endpoints: InspectEndpoints {
            link_count: profile.link_endpoints.len(),
            rendezvous_count: profile.rendezvous.len(),
            stun_server_count: profile.stun_servers.len(),
            relay_endpoint_count: profile.relay_endpoints.len(),
            link: show_lists.then(|| profile.link_endpoints.clone()),
            rendezvous: show_lists.then(|| profile.rendezvous.clone()),
            stun_servers: show_lists.then(|| profile.stun_servers.clone()),
            relay_endpoints: show_lists.then(|| profile.relay_endpoints.clone()),
        },
        credential: InspectCredential {
            present: profile.legacy_relay_transport.is_some(),
            kind: profile.legacy_relay_transport.as_ref().map(|_| "legacy-relay-session-secret"),
        },
        stun_observed_addr: if redact { None } else { profile.cached_stun_observed_addr.clone() },
        redacted: redact,
    }
}

fn print_inspect_report_human(report: &InspectReport) {
    println!("profile:              {}", report.profile);
    println!("schema version:       {} (inspect output: {})", report.profile_schema_version, report.inspect_schema_version);
    println!("service:              {}", report.service);
    if let Some(peer_id) = &report.peer_id {
        println!("peer id:              {peer_id}");
    }
    println!("helper identity:      {}", report.server_identity_cert_sha256);
    if let Some(v) = &report.remote_version {
        println!("remote version:       {v}");
    }
    if let Some(ch) = &report.release_channel {
        println!("release channel:      {ch}");
    }
    println!("update policy:        {:?}", report.update_policy);
    if let Some(t) = &report.last_bootstrap_at {
        println!("last bootstrap at:    {t}");
    }
    println!("last seen at:         {}", report.last_seen_at);
    if let Some(via) = &report.last_via {
        println!("last via:             {via}");
    }
    if let Some(hint) = &report.last_path_hint {
        match &hint.expires_at {
            Some(exp) => println!("last path hint:       {} (expires {exp})", hint.kind),
            None => println!("last path hint:       {}", hint.kind),
        }
    }
    println!(
        "endpoints:            link={} rendezvous={} stun={} relay={}",
        report.endpoints.link_count, report.endpoints.rendezvous_count, report.endpoints.stun_server_count, report.endpoints.relay_endpoint_count
    );
    for (label, values) in [
        ("link", &report.endpoints.link),
        ("rendezvous", &report.endpoints.rendezvous),
        ("stun servers", &report.endpoints.stun_servers),
        ("relay endpoints", &report.endpoints.relay_endpoints),
    ] {
        if let Some(values) = values {
            for v in values {
                println!("  {label}: {v}");
            }
        }
    }
    match report.credential.kind {
        Some(kind) => println!("credential:           present ({kind}, value not shown)"),
        None => println!("credential:           {}", if report.credential.present { "present" } else { "none" }),
    }
    if let Some(addr) = &report.stun_observed_addr {
        println!("stun observed addr:   {addr}");
    }
    if report.redacted {
        println!();
        println!("(--redact: network-identifying fields hidden/truncated above)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_connect_args(args: &[&str]) -> ConnectLaunch {
        parse_connect(args.iter().map(|arg| arg.to_string()))
            .unwrap()
            .unwrap()
    }

    fn parse_serve_args(args: &[&str]) -> ServeLaunch {
        parse_serve(args.iter().map(|arg| arg.to_string()))
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

    #[test]
    fn run_inspect_normalizes_a_bare_profile_alias() {
        // Regression test: profiles are written under the normalized
        // `host:port` key, but `--profile myhost` (no explicit port) must
        // still resolve to it, matching every other command
        // (`connect`/`init`/wrapper) that normalizes before lookup.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let old = std::env::var_os("ISEKAI_PIPE_PROFILES_DIR");
        std::env::set_var("ISEKAI_PIPE_PROFILES_DIR", dir.path());

        let profile = sample_profile_for_inspect();
        isekai_pipe_core::write_persistent_profile(dir.path(), &profile).unwrap();

        let result = run_inspect(InspectLaunch { profile: "myhost".to_string(), json: true, redact: false, verbose: false });

        if let Some(old) = old {
            std::env::set_var("ISEKAI_PIPE_PROFILES_DIR", old);
        } else {
            std::env::remove_var("ISEKAI_PIPE_PROFILES_DIR");
        }
        assert!(result.is_ok(), "{result:?}");
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

    #[test]
    fn replay_buffer_replays_unconfirmed_suffix() {
        let mut buffer = C2hReplayBuffer::new(16);
        buffer.append(b"hello ");
        buffer.append(b"world");

        assert_eq!(buffer.end_offset(), 11);
        assert_eq!(buffer.replay_from(6).unwrap(), b"world");
        buffer.advance_start(6);
        assert_eq!(buffer.remaining_capacity(), 11);
        assert!(buffer.replay_from(0).is_none());
    }

    #[test]
    fn replay_buffer_replay_from_beyond_end_offset_is_none() {
        // The other boundary of `replay_from`'s range check (the "helper
        // claims to have committed bytes this client never even sent" case
        // `replay_and_advance` must treat as a protocol inconsistency, not
        // "nothing to replay" — codex review, quicmux-server-resume).
        let mut buffer = C2hReplayBuffer::new(16);
        buffer.append(b"hello");
        assert_eq!(buffer.end_offset(), 5);
        assert!(buffer.replay_from(6).is_none());
    }

    #[test]
    fn resume_window_for_zero_falls_back_to_the_default_window_instead_of_zero_seconds() {
        // `0` means "no real value was ever learned" (the `MustResume`
        // convergence path, or a caller that itself requested `0`), not a
        // literal zero-second resume window that would give up on the very
        // next disconnect (codex review, quicmux-server-resume).
        assert_eq!(resume_window_for(0), DEFAULT_RESUME_WINDOW);
    }

    #[test]
    fn resume_window_for_a_real_value_uses_it_verbatim() {
        assert_eq!(resume_window_for(180), Duration::from_secs(180));
    }

    #[test]
    fn replay_buffer_backpressures_at_capacity() {
        let mut buffer = C2hReplayBuffer::new(4);
        buffer.append(b"abcd");

        assert!(buffer.is_full());
        assert_eq!(buffer.remaining_capacity(), 0);
        buffer.advance_start(2);
        assert!(!buffer.is_full());
        assert_eq!(buffer.replay_from(2).unwrap(), b"cd");
    }

    #[test]
    fn serve_accepts_named_ssh_service() {
        let launch = parse_serve_args(&[
            "--service",
            "ssh=127.0.0.1:22",
            "--bind",
            "127.0.0.1:0",
            "--once",
        ]);

        assert_eq!(
            launch.service,
            ServiceSpec::ssh_target("127.0.0.1:22").unwrap()
        );
        assert_eq!(launch.helper_args, vec!["--bind", "127.0.0.1:0", "--once"]);
    }

    #[test]
    fn serve_maps_legacy_target_to_ssh_service() {
        let launch = parse_serve_args(&["--target", "127.0.0.1:2222"]);

        assert_eq!(
            launch.service,
            ServiceSpec::ssh_target("127.0.0.1:2222").unwrap()
        );
        assert!(launch.helper_args.is_empty());
    }

    #[test]
    fn serve_rejects_unknown_services_until_dispatch_exists() {
        assert!(parse_serve(
            ["--service", "postgres=127.0.0.1:5432"]
                .into_iter()
                .map(str::to_string)
        )
        .is_err());
    }

    /// A `NetworkChangeMonitor` that fires exactly one event, then never
    /// resolves again — enough to prove `run_resume_loop`'s `tokio::select!`
    /// (`#20b`'s follow-on network-change wiring) actually treats a signal
    /// arriving *before* the data pump finishes as a reason to abandon the
    /// current connection and reconnect, without needing a real OS backend
    /// or a real QUIC connection to exercise that race in isolation.
    struct FireOnceNetworkChangeMonitor {
        fired: bool,
    }

    #[async_trait::async_trait]
    impl isekai_netmon::NetworkChangeMonitor for FireOnceNetworkChangeMonitor {
        async fn next_change(&mut self) -> Option<isekai_netmon::NetworkChangeEvent> {
            if self.fired {
                std::future::pending().await
            } else {
                self.fired = true;
                Some(isekai_netmon::NetworkChangeEvent)
            }
        }
    }

    #[tokio::test]
    async fn network_change_event_wins_the_race_against_a_pump_that_never_finishes() {
        let mut monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> =
            Box::new(FireOnceNetworkChangeMonitor { fired: false });
        // Stands in for `run_data_pump` (which would otherwise only resolve
        // on clean stdin EOF or a real I/O error) — mirrors the general
        // "pump vs. network-change signal" `tokio::select!` shape
        // `run_resume_loop` uses (today via `spawn_reconnect_signal`'s
        // channel rather than polling a monitor directly in this exact
        // `select!`, but the race semantics under test here are the same
        // either way), without needing real stdin/stdout or a QUIC
        // connection.
        let never_finishes = std::future::pending::<Result<()>>();

        let outcome: Result<()> = tokio::select! {
            outcome = never_finishes => outcome,
            Some(_) = monitor.next_change() => Err(anyhow::anyhow!("network change detected, reconnecting early")),
        };

        assert!(outcome.is_err(), "a network-change event must win the race and produce an early-reconnect signal");
    }

    #[tokio::test]
    async fn no_network_change_event_leaves_the_pump_to_finish_on_its_own() {
        let mut monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> = Box::new(isekai_netmon::NoopNetworkChangeMonitor);
        let finishes_soon = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok::<(), anyhow::Error>(())
        };

        let outcome: Result<()> = tokio::select! {
            outcome = finishes_soon => outcome,
            Some(_) = monitor.next_change() => Err(anyhow::anyhow!("network change detected, reconnecting early")),
        };

        assert!(outcome.is_ok(), "with no network-change signal, the pump's own outcome must be used unchanged");
    }

    struct MockRebinder {
        should_succeed: bool,
    }

    impl Rebindable for MockRebinder {
        async fn rebind(&self, _bind: BindSpec) -> Result<(), isekai_transport::MuxError> {
            if self.should_succeed {
                Ok(())
            } else {
                Err(isekai_transport::MuxError::Rebind("mock failure".to_string()))
            }
        }
    }

    const TEST_HELPER_ADDR: &str = "127.0.0.1:9";

    #[tokio::test]
    async fn spawn_reconnect_signal_forwards_plain_network_change_when_not_experimental() {
        let monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> =
            Box::new(FireOnceNetworkChangeMonitor { fired: false });
        let (task, mut rx) =
            spawn_reconnect_signal(monitor, None::<MockRebinder>, /* experimental */ false, TEST_HELPER_ADDR.parse().unwrap(), None);

        assert!(rx.recv().await.is_some(), "a plain network change must be forwarded when experimental rebind is off");
        task.abort();
    }

    #[tokio::test]
    async fn spawn_reconnect_signal_forwards_plain_network_change_when_experimental_but_no_rebinder() {
        // Experimental is on, but this generation's endpoint factory doesn't
        // support rebinding (`rebinder: None`) - must fall back to exactly
        // the non-experimental behavior, not silently drop the event.
        let monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> =
            Box::new(FireOnceNetworkChangeMonitor { fired: false });
        let (task, mut rx) =
            spawn_reconnect_signal(monitor, None::<MockRebinder>, /* experimental */ true, TEST_HELPER_ADDR.parse().unwrap(), None);

        assert!(rx.recv().await.is_some(), "with no rebinder available, a network change must still be forwarded");
        task.abort();
    }

    #[derive(Debug)]
    struct FakeConnectError(bool);

    impl BusyOtherSessionSignal for FakeConnectError {
        fn signals_busy_other_session(&self) -> bool {
            self.0
        }
    }

    #[tokio::test]
    async fn retry_while_busy_other_session_does_not_retry_other_failures() {
        let mut calls = 0u32;
        let result: Result<(), FakeConnectError> = retry_while_busy_other_session(1, || {
            calls += 1;
            async { Err(FakeConnectError(false)) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls, 1, "a non-BUSY_OTHER_SESSION failure must not be retried");
    }

    #[tokio::test]
    async fn retry_while_busy_other_session_retries_until_a_later_attempt_succeeds() {
        let calls = std::cell::Cell::new(0u32);
        let result = retry_while_busy_other_session(1, || {
            let n = calls.get();
            calls.set(n + 1);
            async move { if n == 0 { Err(FakeConnectError(true)) } else { Ok::<(), FakeConnectError>(()) } }
        })
        .await;
        assert!(result.is_ok(), "a BUSY_OTHER_SESSION failure must be retried until it succeeds");
        assert_eq!(calls.get(), 2);
    }

    #[tokio::test]
    async fn retry_while_busy_other_session_gives_up_once_the_resume_window_elapses() {
        let result: Result<(), FakeConnectError> =
            retry_while_busy_other_session(1, || async { Err(FakeConnectError(true)) }).await;
        assert!(result.is_err(), "must stop retrying once the resume window has elapsed");
    }

    #[tokio::test]
    async fn spawn_reconnect_signal_does_not_forward_after_a_successful_rebind() {
        let monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> =
            Box::new(FireOnceNetworkChangeMonitor { fired: false });
        let rebinder = MockRebinder { should_succeed: true };
        let (task, mut rx) = spawn_reconnect_signal(
            monitor,
            Some(rebinder),
            /* experimental */ true,
            TEST_HELPER_ADDR.parse().unwrap(),
            None,
        );

        let result = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(
            result.is_err(),
            "a successful rebind must not forward a reconnect signal - the caller's data pump should keep running untouched"
        );
        task.abort();
    }

    #[tokio::test]
    async fn spawn_reconnect_signal_forwards_after_a_failed_rebind() {
        let monitor: Box<dyn isekai_netmon::NetworkChangeMonitor> =
            Box::new(FireOnceNetworkChangeMonitor { fired: false });
        let rebinder = MockRebinder { should_succeed: false };
        let (task, mut rx) = spawn_reconnect_signal(
            monitor,
            Some(rebinder),
            /* experimental */ true,
            TEST_HELPER_ADDR.parse().unwrap(),
            None,
        );

        assert!(rx.recv().await.is_some(), "a failed rebind attempt must fall back to the reconnect signal");
        task.abort();
    }

    fn parse_inspect_args(args: &[&str]) -> InspectLaunch {
        parse_inspect(args.iter().map(|arg| arg.to_string())).unwrap().unwrap()
    }

    fn sample_profile_for_inspect() -> isekai_pipe_core::PersistentProfile {
        isekai_pipe_core::PersistentProfile::migrate_legacy_helper_trust(
            "myhost:22",
            &isekai_trust::HelperTrust {
                identity_pubkey: "pk-abc".to_string(),
                trusted_helper_sha256: "a".repeat(64),
                trusted_helper_version: "0.5.0".to_string(),
                update_policy: isekai_trust::UpdatePolicy::ExactDigestOnly,
                release_channel: Some("stable".to_string()),
                last_via: Some("bastion.example.com".to_string()),
                trusted_at: "2026-07-04T00:00:00Z".to_string(),
                last_seen_at: "2026-07-08T00:00:00Z".to_string(),
                cached_relay_addr: "203.0.113.10:45231".to_string(),
                cached_cert_sha256: "3a7f".repeat(16),
                cached_session_secret: "super-secret-value".to_string(),
                cached_stun_observed_addr: Some("198.51.100.7:45231".to_string()),
            },
        )
    }

    #[test]
    fn parse_inspect_requires_profile() {
        assert!(parse_inspect(std::iter::empty()).is_err());
    }

    #[test]
    fn parse_inspect_reads_flags() {
        let launch = parse_inspect_args(&["--profile", "prod", "--json", "--redact", "--verbose"]);
        assert_eq!(launch.profile, "prod");
        assert!(launch.json);
        assert!(launch.redact);
        assert!(launch.verbose);
    }

    #[test]
    fn parse_inspect_defaults_flags_to_false() {
        let launch = parse_inspect_args(&["--profile", "prod"]);
        assert!(!launch.json);
        assert!(!launch.redact);
        assert!(!launch.verbose);
    }

    #[test]
    fn inspect_report_never_contains_the_session_secret_even_unredacted() {
        let profile = sample_profile_for_inspect();
        for redact in [false, true] {
            for verbose in [false, true] {
                let report = build_inspect_report(&profile, verbose, redact);
                let json = serde_json::to_string(&report).unwrap();
                assert!(!json.contains("super-secret-value"), "redact={redact} verbose={verbose}: {json}");
            }
        }
    }

    #[test]
    fn inspect_report_default_view_omits_endpoint_lists_but_keeps_counts() {
        let mut profile = sample_profile_for_inspect();
        profile.link_endpoints = vec!["https://link.example.com".to_string()];
        let report = build_inspect_report(&profile, false, false);
        assert_eq!(report.endpoints.link_count, 1);
        assert_eq!(report.endpoints.link, None);
    }

    #[test]
    fn inspect_report_verbose_includes_endpoint_lists() {
        let mut profile = sample_profile_for_inspect();
        profile.link_endpoints = vec!["https://link.example.com".to_string()];
        let report = build_inspect_report(&profile, true, false);
        assert_eq!(report.endpoints.link, Some(vec!["https://link.example.com".to_string()]));
    }

    #[test]
    fn inspect_report_redact_hides_lists_even_when_verbose() {
        let mut profile = sample_profile_for_inspect();
        profile.link_endpoints = vec!["https://link.example.com".to_string()];
        let report = build_inspect_report(&profile, true, true);
        assert_eq!(report.endpoints.link, None, "redact must win over verbose");
    }

    #[test]
    fn inspect_report_redact_truncates_cert_and_hides_via_and_stun_addr() {
        let profile = sample_profile_for_inspect();
        let report = build_inspect_report(&profile, false, true);
        assert!(report.server_identity_cert_sha256.ends_with('…'));
        assert!(report.server_identity_cert_sha256.len() < profile.server_identity.cert_sha256_hex.len());
        assert_eq!(report.last_via, None);
        assert_eq!(report.stun_observed_addr, None);
        assert!(report.redacted);
    }

    #[test]
    fn inspect_report_unredacted_shows_full_cert_via_and_stun_addr() {
        let profile = sample_profile_for_inspect();
        let report = build_inspect_report(&profile, false, false);
        assert_eq!(report.server_identity_cert_sha256, profile.server_identity.cert_sha256_hex);
        assert_eq!(report.last_via, profile.last_via);
        assert_eq!(report.stun_observed_addr, profile.cached_stun_observed_addr);
        assert!(!report.redacted);
    }

    #[test]
    fn inspect_report_reports_credential_presence_without_the_secret() {
        let profile = sample_profile_for_inspect();
        let report = build_inspect_report(&profile, false, false);
        assert!(report.credential.present);
        assert_eq!(report.credential.kind, Some("legacy-relay-session-secret"));

        let mut profile_without = profile;
        profile_without.legacy_relay_transport = None;
        let report_without = build_inspect_report(&profile_without, false, false);
        assert!(!report_without.credential.present);
        assert_eq!(report_without.credential.kind, None);
    }

    #[test]
    fn inspect_report_carries_the_output_schema_version() {
        let profile = sample_profile_for_inspect();
        let report = build_inspect_report(&profile, false, false);
        assert_eq!(report.inspect_schema_version, INSPECT_SCHEMA_VERSION);
    }

    fn parse_probe_args(args: &[&str]) -> ProbeLaunch {
        parse_probe(args.iter().map(|arg| arg.to_string())).unwrap().unwrap()
    }

    #[test]
    fn parse_probe_requires_profile() {
        assert!(parse_probe(std::iter::empty()).is_err());
    }

    #[test]
    fn parse_probe_reads_flags() {
        let launch = parse_probe_args(&["--profile", "prod", "--stun-server", "203.0.113.9:3478", "--json"]);
        assert_eq!(launch.profile, "prod");
        assert_eq!(launch.stun_servers, vec!["203.0.113.9:3478".to_string()]);
        assert!(launch.json);
    }

    #[test]
    fn parse_probe_accepts_a_bare_positional_profile() {
        let launch = parse_probe_args(&["prod"]);
        assert_eq!(launch.profile, "prod");
        assert!(launch.stun_servers.is_empty());
        assert!(!launch.json);
    }

    #[test]
    fn parse_probe_rejects_multiple_profiles() {
        assert!(parse_probe(["--profile", "a", "b"].into_iter().map(str::to_string)).is_err());
    }

    fn sample_transport_error() -> isekai_transport::TransportError {
        isekai_transport::TransportError::Mux(isekai_transport::MuxError::Handshake("simulated failure".to_string()))
    }

    #[test]
    fn stage_from_attempt_failure_target_reject_reports_handshake_ok_and_target_failed() {
        // `DefinitiveRejectNotRetryable` is produced exclusively by
        // `AttachRejectReason::Target` (`isekai-transport/src/attempt.rs`) —
        // the one variant where the handshake itself fully succeeded and
        // only the remote's own target dial failed.
        let failure = AttemptFailure::DefinitiveRejectNotRetryable { source: sample_transport_error() };
        let (handshake, target_reachability) = stage_from_attempt_failure(&failure);
        assert!(handshake.is_ok(), "{handshake:?}");
        assert!(matches!(target_reachability, ProbeStageStatus::Failed { .. }), "{target_reachability:?}");
    }

    #[test]
    fn stage_from_attempt_failure_pre_attach_reports_both_stages_unreached() {
        let failure = AttemptFailure::RetryablePreAttach { source: sample_transport_error() };
        let (handshake, target_reachability) = stage_from_attempt_failure(&failure);
        assert!(matches!(handshake, ProbeStageStatus::Failed { .. }), "{handshake:?}");
        assert!(matches!(target_reachability, ProbeStageStatus::NotAttempted { .. }), "{target_reachability:?}");
    }

    #[test]
    fn stage_from_relay_connect_error_attached_but_control_stream_failed_still_confirms_target_reachable() {
        // The subtle case: the data-stream attach genuinely succeeded (which
        // already implies the remote reached its target) before the
        // *separate* resumable control-stream open failed — `handshake`
        // should report the failure, but `target_reachability` should still
        // report `Ok`, since that fact is independently known regardless of
        // the later control-stream problem.
        let error = SequentialConnectError::AttachedButControlStreamFailed {
            candidate_id: "probe".to_string(),
            source: sample_transport_error(),
        };
        let (handshake, target_reachability) = stage_from_relay_connect_error(&error);
        assert!(matches!(handshake, ProbeStageStatus::Failed { .. }), "{handshake:?}");
        assert!(target_reachability.is_ok(), "{target_reachability:?}");
    }

    #[test]
    fn probe_report_fully_reachable_requires_handshake_and_target_ok_and_stun_ok_or_skipped() {
        let base = ProbeReport {
            probe_schema_version: PROBE_SCHEMA_VERSION,
            profile: "prod".to_string(),
            transport: "relay",
            dns_resolution: ProbeStageStatus::Skipped { reason: "n/a".to_string() },
            stun_discovery: ProbeStageStatus::Skipped { reason: "no --stun-server given".to_string() },
            handshake: ProbeStageStatus::Ok { detail: None },
            target_reachability: ProbeStageStatus::Ok { detail: None },
            stale_trust_suspected: false,
        };
        assert!(base.fully_reachable());

        let mut failed_target = base;
        failed_target.target_reachability = ProbeStageStatus::Failed { detail: "unreachable".to_string() };
        assert!(!failed_target.fully_reachable());

        let mut failed_stun = ProbeReport {
            probe_schema_version: PROBE_SCHEMA_VERSION,
            profile: "prod".to_string(),
            transport: "stun-p2p",
            dns_resolution: ProbeStageStatus::Skipped { reason: "n/a".to_string() },
            stun_discovery: ProbeStageStatus::Failed { detail: "no response".to_string() },
            handshake: ProbeStageStatus::Ok { detail: None },
            target_reachability: ProbeStageStatus::Ok { detail: None },
            stale_trust_suspected: false,
        };
        assert!(!failed_stun.fully_reachable());
        failed_stun.stun_discovery = ProbeStageStatus::Ok { detail: None };
        assert!(failed_stun.fully_reachable());
    }

    #[test]
    fn probe_report_serializes_stage_status_with_a_tagged_shape() {
        let status = ProbeStageStatus::Failed { detail: "boom".to_string() };
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#"{"status":"failed","detail":"boom"}"#);
    }

    #[test]
    fn probe_report_stale_trust_suspected_is_independent_of_fully_reachable() {
        let mut report = ProbeReport {
            probe_schema_version: PROBE_SCHEMA_VERSION,
            profile: "prod".to_string(),
            transport: "relay",
            dns_resolution: ProbeStageStatus::Skipped { reason: "n/a".to_string() },
            stun_discovery: ProbeStageStatus::Skipped { reason: "no --stun-server given".to_string() },
            handshake: ProbeStageStatus::Failed { detail: "isekai-helper rejected the connection: Auth".to_string() },
            target_reachability: ProbeStageStatus::NotAttempted { reason: "handshake did not complete".to_string() },
            stale_trust_suspected: true,
        };
        assert!(!report.fully_reachable());
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(r#""stale_trust_suspected":true"#), "{json}");

        report.stale_trust_suspected = false;
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(r#""stale_trust_suspected":false"#), "{json}");
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

    /// Minimal real-QUIC fixture for `reestablish_control_stream`/
    /// `replay_and_advance`'s new behavior (codex review,
    /// quicmux-server-resume): a listener that accepts one connection and
    /// speaks just enough of the control-stream wire format
    /// (`CONTROL_HELLO`/`CONTROL_ACK`, `archive/HELPER_PROTOCOL.md` §7.3) to
    /// let `open_control_stream` succeed — mirrors
    /// `isekai-transport::warm_standby`'s own test listener, minus the
    /// RESUME dispatch this doesn't need.
    mod resume_control_stream_tests {
        use super::*;
        use isekai_protocol::hello::{ALPN, EXPORTER_LABEL};
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        const CONTROL_HELLO: u8 = 0x10;
        const CONTROL_ACK: u8 = 0x11;
        const CONTROL_HELLO_FRAME_LEN: usize = 1 + 32; // type byte + 32-byte proof
        const CONTROL_ACK_FRAME_LEN: usize = 1 + 16; // type byte + 16-byte session_id

        fn test_server_config() -> (quicmux::MuxServerConfig, String) {
            let cert = rcgen::generate_simple_self_signed(vec!["isekai-pipe.local".to_string()]).unwrap();
            let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().clone());
            let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
            let cert_sha256_hex = {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(cert_der.as_ref());
                hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
            };
            let config = quicmux::MuxServerConfig {
                alpn: ALPN.to_vec(),
                exporter_label: EXPORTER_LABEL.to_vec(),
                max_idle_timeout: Duration::from_secs(15),
                keep_alive_interval: Duration::from_secs(5),
                max_concurrent_bidi_streams: 4,
                max_concurrent_uni_streams: 0,
                multipath: false,
                cert_chain: vec![cert_der],
                private_key: key_der,
            };
            (config, cert_sha256_hex)
        }

        /// Accepts exactly one connection and, on its first bidi stream,
        /// reads a `CONTROL_HELLO` frame (ignoring the proof — this fixture
        /// isn't testing auth) and replies with `CONTROL_ACK` plus a fixed
        /// session_id, then holds the connection open by looping
        /// `accept_bi()` (matching `warm_standby.rs`'s own listener, which
        /// documents why: dropping the connection right after the write can
        /// race the client's read of that same write).
        async fn spawn_control_hello_listener() -> (SocketAddr, String) {
            let (server_config, cert_sha256_hex) = test_server_config();
            let listener = quicmux::AnyMuxListener::bind_noq(server_config, quicmux::BindSpec::any_ipv4()).await.unwrap();
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listener.local_addr().unwrap().port());
            tokio::spawn(async move {
                let Some(incoming) = listener.accept().await else { return };
                let Ok(conn) = incoming.accept().await else { return };
                loop {
                    let Ok(stream) = conn.accept_bi().await else { break };
                    let (mut recv, mut send) = stream.split();
                    let mut hello = [0u8; CONTROL_HELLO_FRAME_LEN];
                    if recv.read(&mut hello).await.unwrap_or(0) == 0 || hello[0] != CONTROL_HELLO {
                        continue;
                    }
                    let mut ack = vec![CONTROL_ACK];
                    ack.extend_from_slice(&[0x7Fu8; CONTROL_ACK_FRAME_LEN - 1]);
                    let _ = send.write_all(&ack).await;
                }
            });
            (addr, cert_sha256_hex)
        }

        async fn connect(addr: SocketAddr, cert_sha256_hex: String) -> quicmux::AnyMuxConnection {
            let factory = system_quic_factory();
            let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await.unwrap();
            endpoint
                .connect(quicmux::RemoteSpec { addr, server_name: "isekai-pipe.local".to_string(), cert_sha256_hex })
                .await
                .unwrap()
        }

        #[tokio::test]
        async fn reestablish_control_stream_succeeds_against_a_real_listener() {
            let (addr, cert_sha256_hex) = spawn_control_hello_listener().await;
            let conn = connect(addr, cert_sha256_hex).await;
            let counters = Arc::new(AppAckCounters::new());

            let tasks = reestablish_control_stream(&conn, b"any-session-secret", &counters).await;
            assert!(tasks.is_ok(), "{:?}", tasks.err());
            tasks.unwrap().abort();
        }

        #[tokio::test]
        async fn replay_and_advance_rejects_a_committed_offset_beyond_what_was_ever_sent() {
            let (addr, cert_sha256_hex) = spawn_control_hello_listener().await;
            let conn = connect(addr, cert_sha256_hex).await;
            let mut stream = conn.open_bi().await.unwrap();

            let replay = Mutex::new(C2hReplayBuffer::new(1024));
            replay.lock().unwrap().append(b"hello");

            // The helper claims committed_offset=999, but this client never
            // sent more than 5 bytes — a protocol inconsistency that must
            // not be silently accepted (codex review).
            let ok = replay_and_advance(&replay, 999, &mut stream).await;
            assert!(!ok, "an out-of-range committed_offset must be rejected, not silently accepted");
            assert_eq!(replay.lock().unwrap().end_offset(), 5, "the replay buffer must be untouched on rejection");
        }

        #[tokio::test]
        async fn replay_and_advance_still_replays_a_valid_in_range_offset() {
            // Regression check: the new out-of-range rejection above must
            // not have broken the ordinary, already-tested in-range path.
            let (addr, cert_sha256_hex) = spawn_control_hello_listener().await;
            let conn = connect(addr, cert_sha256_hex).await;
            let mut stream = conn.open_bi().await.unwrap();

            let replay = Mutex::new(C2hReplayBuffer::new(1024));
            replay.lock().unwrap().append(b"hello world");

            let ok = replay_and_advance(&replay, 6, &mut stream).await;
            assert!(ok);
            assert_eq!(replay.lock().unwrap().end_offset(), 11);
        }
    }

    mod reconnect_status_tests {
        use super::*;

        #[test]
        fn format_reconnect_status_tty_uses_in_place_ansi_redraw() {
            let msg = format_reconnect_status(true, 3, 60);
            assert!(msg.starts_with('\r'), "TTY表示はその場書き換え(\\r開始)のはず: {msg:?}");
            assert!(msg.contains("\x1b[0;33m"), "黄色のANSIエスケープを含むはず: {msg:?}");
            assert!(msg.ends_with("\x1b[K"), "行末までクリアするはず: {msg:?}");
            assert!(msg.contains("3s/60s"), "経過/上限秒数を含むはず: {msg:?}");
            assert!(!msg.contains('\n'), "改行を含んではいけない(呼び出し側がeprint!でその場書き換えする前提): {msg:?}");
        }

        #[test]
        fn format_reconnect_status_non_tty_is_plain_text_without_ansi() {
            let msg = format_reconnect_status(false, 3, 60);
            assert!(!msg.contains('\r'), "非TTY時は\\rを含んではいけない: {msg:?}");
            assert!(!msg.contains('\x1b'), "非TTY時はANSIエスケープを含んではいけない: {msg:?}");
            assert!(msg.contains("3s"), "経過秒数を含むはず: {msg:?}");
            assert!(msg.contains("60s"), "上限秒数を含むはず: {msg:?}");
        }

        // `sleep_with_live_status`本体はタイミングだけを担当し(実際の描画は
        // `on_tick`コールバックに委譲)、`tokio::time::pause()`で仮想時間を
        // 進めれば実時間を待たずに決定的に検証できる。
        #[tokio::test(start_paused = true)]
        async fn sleep_with_live_status_ticks_once_per_second_until_delay_elapses() {
            // `#[tokio::test(start_paused = true)]`下では、他にやることが
            // 無い間はtokioの仮想時計が次のタイマーまで自動的に早送りされる
            // ため、手動で`tokio::time::advance`を挟まずそのまま`.await`
            // すればよい(spawn+手動advanceは、spawnされたタスクが実際に
            // 最初のtimer登録を終える前にadvanceが先に走ってしまう競合が
            // あり不安定だった)。
            // 2.5秒の待機は 1s + 1s + 0.5s の3チャンクに分かれ、3回tickするはず。
            let mut tick_count = 0;
            sleep_with_live_status(Duration::from_millis(2500), || tick_count += 1).await;
            assert_eq!(tick_count, 3);
        }

        #[tokio::test(start_paused = true)]
        async fn sleep_with_live_status_ticks_once_for_a_sub_second_delay() {
            let mut tick_count = 0;
            sleep_with_live_status(Duration::from_millis(300), || tick_count += 1).await;
            assert_eq!(
                tick_count, 1,
                "1秒未満の待機でも最低1回はtickして呼び出し元に経過を伝えるはず"
            );
        }
    }
}

/// Larger than the OS-default *process* main thread stack, which is fixed
/// at link time and cannot be grown at runtime -- notably 1 MiB on Windows
/// (vs. ~8 MiB typical on Linux/macOS). `tokio::main`'s generated
/// `block_on` runs the top-level future's synchronous work (e.g. the QUIC
/// handshake in `isekai-transport`'s `quicmux`/`noq` backend, which is deep
/// and un-inlined in debug builds) directly on the calling thread, so a
/// small main-thread stack overflows there before this fix -- observed in
/// practice as `thread 'main' has overflowed its stack` right after
/// `quicmux::noq_backend` starts connecting, when `isekai-pipe connect` runs
/// as an `isekai-ssh`-launched `ssh(1)` `ProxyCommand` on a debug build on
/// Windows. Running the whole async body on a freshly spawned thread with
/// an explicit stack size sidesteps the platform-fixed main-thread limit;
/// the size chosen here is generous headroom rather than a tight bound.
const MAIN_WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

fn main() -> ExitCode {
    std::thread::Builder::new()
        .name("isekai-pipe-main".to_string())
        .stack_size(MAIN_WORKER_STACK_SIZE)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime")
                .block_on(run())
        })
        .expect("failed to spawn isekai-pipe main worker thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

async fn run() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("-h") | Some("--help") | Some("help") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("version") | Some("--version") => {
            println!("isekai-pipe {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("connect") => connect_command(args).await,
        Some("serve") => serve_command(args).await,
        Some("probe") => probe_command(args).await,
        Some("inspect") => inspect_command(args).await,
        Some("ctl") => ctl::ctl_command(args).await,
        Some(other) => {
            eprintln!("isekai-pipe: unknown command: {other}");
            eprintln!("try `isekai-pipe --help`");
            ExitCode::from(EX_USAGE)
        }
    }
}
