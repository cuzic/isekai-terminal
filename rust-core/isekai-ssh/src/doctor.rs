//! `isekai-ssh doctor <host> [--fix]`: manual diagnostic, never part of
//! `isekai-ssh <host>`'s own connection path (`ISEKAI_PIPE_DESIGN.md` §8
//! Epic N's "always-connects" principle). That path already detects any
//! connect-layer failure — stale trust material *or* a plain unreachable/dead
//! cached deployment — and silently recovers from it on its own
//! (`wrapper.rs::run_ssh_with_connect_failure_recovery`) — `doctor` exists
//! purely so a human can ask "what's the state of this host's trust right
//! now?" on demand, without waiting for a real connection attempt to fail
//! first.
//!
//! Reuses `wrapper.rs`'s own `~/.ssh/config`/`#@isekai` directive resolution
//! (`wrapper::resolve_profile_for_destination`) and `bootstrap_and_register`
//! (for `--fix`) rather than duplicating either. Reachability/staleness
//! checking itself shells out to the already-stable `isekai-pipe probe
//! --json` (Epic J) rather than reimplementing connection logic here or
//! restructuring `isekai-pipe`'s binary-only `run_probe`/`ProbeReport` into
//! a shared library just for this one command — `doctor` is an occasional,
//! manual diagnostic, not a per-connection hot path, so the extra process
//! spawn costs nothing.

use anyhow::{anyhow, Context, Result};
use isekai_pipe_core::{default_profiles_dir, load_persistent_profile};

use crate::cli::DoctorArgs;

/// Mirrors just the fields of `isekai-pipe probe --json`'s `ProbeReport`
/// this command needs to display and act on — `ProbeReport` itself is
/// private to the `isekai-pipe` binary crate (Epic J deliberately never
/// promoted `run_probe`/`ProbeReport` to a shared library, see this
/// module's docs), so `doctor` parses the stable JSON output instead of
/// linking against it directly.
#[derive(Debug, serde::Deserialize)]
struct ProbeReportView {
    transport: String,
    dns_resolution: ProbeStageView,
    stun_discovery: ProbeStageView,
    handshake: ProbeStageView,
    target_reachability: ProbeStageView,
    #[serde(default)]
    stale_trust_suspected: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ProbeStageView {
    Ok { detail: Option<String> },
    Failed { detail: String },
    Skipped { reason: String },
    NotAttempted { reason: String },
}

fn print_stage(label: &str, stage: &ProbeStageView) {
    match stage {
        ProbeStageView::Ok { detail } => {
            println!("[ok]          {label}{}", detail.as_deref().map(|d| format!(" -- {d}")).unwrap_or_default());
        }
        ProbeStageView::Failed { detail } => println!("[failed]      {label} -- {detail}"),
        ProbeStageView::Skipped { reason } => println!("[skipped]     {label} -- {reason}"),
        ProbeStageView::NotAttempted { reason } => println!("[not-reached] {label} -- {reason}"),
    }
}

pub async fn run(args: DoctorArgs) -> Result<()> {
    let mut extra_isekai_args = Vec::new();
    if let Some(helper_binary) = &args.helper_binary {
        extra_isekai_args.push("--isekai-helper-binary".to_string());
        extra_isekai_args.push(helper_binary.display().to_string());
    }
    let (plan, resolution) = crate::wrapper::resolve_profile_for_destination(&args.host, extra_isekai_args)
        .await
        .with_context(|| format!("isekai-ssh doctor: failed to resolve {:?}", args.host))?;
    let profile = resolution.profile().to_string();

    let profiles_dir = default_profiles_dir().context("isekai-ssh doctor: could not determine the profiles directory")?;
    let key = isekai_trust::normalize_host_port(&profile).with_context(|| format!("isekai-ssh doctor: invalid profile {profile:?}"))?;
    if load_persistent_profile(&profiles_dir, &key)?.is_none() {
        return Err(anyhow!(
            "{profile:?} has never been bootstrapped -- run `isekai-ssh {}` to set it up (TOFU confirmation required).",
            args.host
        ));
    }

    let mut cmd = tokio::process::Command::new(plan.pipe_path());
    cmd.args(["probe", "--profile", &profile, "--json"]);
    if let Some(stun_server) = &args.stun_server {
        cmd.args(["--stun-server", &stun_server.to_string()]);
    }
    let output = cmd.output().await.with_context(|| format!("isekai-ssh doctor: failed to run {:?} probe", plan.pipe_path().display()))?;
    let report: ProbeReportView = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "isekai-ssh doctor: failed to parse `isekai-pipe probe --json` output: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })?;

    println!("profile:   {profile}");
    println!("transport: {}", report.transport);
    print_stage("dns resolution", &report.dns_resolution);
    print_stage("stun discovery", &report.stun_discovery);
    print_stage("handshake (relay-auth/quic-connect/cert-pin/hello-ack)", &report.handshake);
    print_stage("target reachability", &report.target_reachability);

    if !report.stale_trust_suspected {
        if output.status.success() {
            return Ok(());
        }
        return Err(anyhow!("{profile:?} is not fully reachable right now (see stage results above)"));
    }

    println!();
    println!(
        "This looks like the cached trust for this host is stale -- the deployed isekai-pipe serve \
         process likely restarted and regenerated its session secret/certificate \
         (ISEKAI_PIPE_DESIGN.md §8 Epic N)."
    );
    if !args.fix {
        return Err(anyhow!(
            "run `isekai-ssh doctor {} --fix` to refresh it now, or just run `isekai-ssh {}` again \
             -- it self-heals automatically.",
            args.host,
            args.host
        ));
    }

    println!("Refreshing trust for {profile:?} automatically (no confirmation needed; already trusted)...");
    crate::wrapper::bootstrap_and_register(&plan, &resolution, crate::wrapper::TofuConfirmation::Silent)
        .await
        .context("isekai-ssh doctor: --fix failed")?;
    println!("Refreshed. Try connecting again.");
    Ok(())
}
