//! `isekai-pipe probe`: dials a cached profile's transport and reports how
//! far it got, stage by stage (dns resolution/stun discovery/handshake/
//! target reachability), instead of a plain success/failure binary
//! (`ISEKAI_PIPE_DESIGN.md` §8, right before Epic K). Unlike `inspect`, this
//! performs real network I/O; unlike `connect`, it never hands off to
//! [`crate::resume_loop`] — the connection is dropped immediately after the
//! stage outcome is observed.

use anyhow::{Context, Result};
use isekai_pipe_core::{default_profiles_dir, load_persistent_profile};
use isekai_transport::{
    connect_stun_p2p_with_fallback, connect_via_relay_resumable_with_fallback, system_quic_factory, AttemptFailure,
    RelayTarget, SequentialConnectError, SequentialFailure, SequentialRelayCandidate, SequentialStunCandidate,
    SequentialStunConnectError, StunP2pTarget,
};
use std::process::ExitCode;

use crate::connect::{decode_secret, next_arg};
use crate::{EX_UNAVAILABLE, EX_USAGE};

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

pub(crate) async fn probe_command(args: impl Iterator<Item = String>) -> ExitCode {
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

        // Validates the cached identity before dialing rather than letting a
        // corrupted `PersistentProfile` (e.g. a truncated cert_sha256_hex)
        // surface as an opaque TLS/handshake failure deep inside quicmux.
        let (cert_pin, server_name) =
            isekai_pipe_core::validate_endpoint_identity(&entry.server_identity.cert_sha256_hex, "isekai-helper")
                .with_context(|| format!("isekai-pipe probe: profile {:?} has an invalid cached identity", launch.profile))?;
        let target = StunP2pTarget {
            peer_addr: peer_addr
                .parse()
                .with_context(|| format!("isekai-pipe probe: invalid cached_stun_observed_addr {peer_addr:?}"))?,
            server_name: server_name.as_str().to_string(),
            cert_sha256_hex: cert_pin.to_hex(),
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
    let (cert_pin, server_name) =
        isekai_pipe_core::validate_endpoint_identity(&entry.server_identity.cert_sha256_hex, "isekai-helper")
            .with_context(|| format!("isekai-pipe probe: profile {:?} has an invalid cached identity", launch.profile))?;
    let target = RelayTarget {
        helper_addr: legacy_relay
            .helper_addr
            .parse()
            .with_context(|| format!("isekai-pipe probe: invalid cached helper_addr {:?}", legacy_relay.helper_addr))?,
        server_name: server_name.as_str().to_string(),
        cert_sha256_hex: cert_pin.to_hex(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
