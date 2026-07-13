//! `isekai-pipe inspect`: passive `PersistentProfile` state dump
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic E). Never opens a socket — everything
//! here reads only what's already on disk. Secrets
//! (`legacy_relay_transport.session_secret_b64`) are never surfaced, with or
//! without `--redact`; `--redact` additionally hides other
//! network-topology-identifying values (full endpoint lists, `last_via`,
//! `cached_stun_observed_addr`, and truncates the cert fingerprint) so
//! output can be pasted into a bug report without leaking where a profile
//! actually points.

use anyhow::{Context, Result};
use isekai_pipe_core::{default_profiles_dir, load_persistent_profile};
use std::process::ExitCode;

use crate::connect::next_arg;
use crate::{EX_UNAVAILABLE, EX_USAGE};

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

pub(crate) async fn inspect_command(args: impl Iterator<Item = String>) -> ExitCode {
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
    fn run_inspect_normalizes_a_bare_profile_alias() {
        // Regression test: profiles are written under the normalized
        // `host:port` key, but `--profile myhost` (no explicit port) must
        // still resolve to it, matching every other command
        // (`connect`/`init`/wrapper) that normalizes before lookup.
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
}
