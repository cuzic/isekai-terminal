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
use isekai_pipe_core::{default_log_file, default_profiles_dir, load_persistent_profile};
use std::path::Path;

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

/// Prints the user-collectable diagnostic files that live next to
/// `isekai_pipe_core::default_log_file()`.
///
/// `doctor` already answers "is this host reachable right now?"; this block
/// answers the follow-up that motivated ADR_ISEKAI_SSH_EXIT_DIAGNOSTICS:
/// "if a detached Windows holder died earlier, which files should I send
/// for that post-mortem?" The holder filenames are digest-based because the
/// digest is the mux identity, not a human host label, so listing the whole
/// directory is intentionally more useful than trying to predict just one
/// file from `doctor <host>` and hiding the rest.
///
/// The holder-log section is Windows-only: the detached mux holder
/// (`native/mux/holder.rs`) only exists on the native Windows path, so
/// listing an always-empty "holder log directory" on Unix would read as
/// something broken rather than simply not applicable there.
fn print_log_locations(default_log: &Path) {
    println!();
    println!("diagnostic logs:");
    println!(
        "[ok]          default isekai-ssh log -- {} (panic and verbose bootstrap diagnostics; often absent until needed)",
        default_log.display()
    );

    #[cfg(windows)]
    print_holder_log_locations(default_log);
}

#[cfg(windows)]
fn print_holder_log_locations(default_log: &Path) {
    let Some(log_dir) = default_log.parent() else {
        println!("[skipped]     holder logs -- default log path has no parent directory");
        return;
    };
    println!("[ok]          holder log directory -- {}", log_dir.display());

    // `collect_holder_logs` already tolerates a missing directory (returns
    // empty rather than erroring), so a non-existent and an empty directory
    // collapse into the same one "not found yet" message below instead of
    // two copies of it.
    let mut entries = collect_holder_logs(log_dir);
    if entries.is_empty() {
        println!(
            "[not-found]   holder logs -- no isekai-ssh-holder-*.log or isekai-ssh-holder-*-ssh.log files have been generated yet"
        );
        return;
    }
    // Most-recently-modified first: the file relevant to "what just
    // happened" belongs at the top, not buried alphabetically among every
    // destination this host has ever holder-connected to.
    entries.sort_by(|a, b| b.modified_unix_secs.cmp(&a.modified_unix_secs));
    for entry in entries {
        println!(
            "[ok]          {} -- {} ({} bytes, modified {})",
            entry.kind,
            entry.path.display(),
            entry.len,
            entry.modified_unix_secs.map(isekai_trust::format_rfc3339_utc).unwrap_or_else(|| "unknown".to_string())
        );
    }
}

#[cfg(windows)]
struct HolderLogEntry {
    path: std::path::PathBuf,
    kind: &'static str,
    len: u64,
    modified_unix_secs: Option<u64>,
}

/// Best-effort directory listing: a single unreadable entry (e.g. a live
/// holder mid-`rename` during its own `RotatingLogFile` rotation racing this
/// scan) must not hide every *other* log this call could otherwise report —
/// so a failure at any per-entry step is skipped rather than propagated
/// (ADR_ISEKAI_SSH_EXIT_DIAGNOSTICS.md §C5). `read_dir` itself failing
/// (the directory disappearing between the `exists()` check and here) is the
/// one case still surfaced to the caller, since there is nothing left to
/// list at all.
#[cfg(windows)]
fn collect_holder_logs(log_dir: &Path) -> Vec<HolderLogEntry> {
    let Ok(read_dir) = std::fs::read_dir(log_dir) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for entry in read_dir {
        let Ok(entry) = entry else { continue };
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else { continue };
        // Matches both a holder log's live file and its single rotated-out
        // `.1` generation (`isekai_pipe_core::RotatingLogFile`) — the `.1`
        // often holds exactly the run that just crashed, since rotation
        // happens *because* the live file just crossed the size threshold.
        let kind = if file_name.starts_with("isekai-ssh-holder-") && (file_name.ends_with("-ssh.log") || file_name.ends_with("-ssh.log.1")) {
            "isekai-ssh holder log"
        } else if file_name.starts_with("isekai-ssh-holder-") && (file_name.ends_with(".log") || file_name.ends_with(".log.1")) {
            "isekai-pipe connect log"
        } else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else { continue };
        if !metadata.is_file() {
            continue;
        }
        let modified_unix_secs = metadata.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
        entries.push(HolderLogEntry { path: entry.path(), kind, len: metadata.len(), modified_unix_secs });
    }
    entries
}

pub async fn run(args: DoctorArgs) -> Result<()> {
    let mut extra_isekai_args = Vec::new();
    if let Some(helper_binary) = &args.helper_binary {
        extra_isekai_args.push("--isekai-helper-binary".to_string());
        extra_isekai_args.push(helper_binary.display().to_string());
    }
    if let Some(ssh_path) = &args.ssh_path {
        extra_isekai_args.push("--isekai-ssh-path".to_string());
        extra_isekai_args.push(ssh_path.display().to_string());
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
    // Best-effort: this listing is a pure diagnostic nicety layered on top
    // of the reachability check above, and `doctor` (including its `--fix`
    // repair path below) must keep working even in the degraded environment
    // (`%LOCALAPPDATA%`/`$HOME` unset) `default_log_file()` itself can fail
    // to resolve in -- the same fail-open policy `log_file.rs` applies to
    // every write it makes (ADR_ISEKAI_SSH_EXIT_DIAGNOSTICS.md §C3).
    if let Ok(default_log) = default_log_file() {
        print_log_locations(&default_log);
    }

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

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Pins down `collect_holder_logs`'s filename classification (`/code-
    /// review` finding: this pure string logic had no test, despite sitting
    /// right next to `naming.rs`'s carefully-tested `channel_digest`).
    /// Covers: both live files, both rotated `.1` companions, an unrelated
    /// file that must be ignored, and a directory that happens to match the
    /// naming pattern (must not be treated as a log).
    #[test]
    fn collect_holder_logs_classifies_live_and_rotated_files_and_ignores_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let touch = |name: &str| std::fs::write(dir.path().join(name), b"line\n").unwrap();

        touch("isekai-ssh-holder-abc123.log");
        touch("isekai-ssh-holder-abc123.log.1");
        touch("isekai-ssh-holder-abc123-ssh.log");
        touch("isekai-ssh-holder-abc123-ssh.log.1");
        touch("isekai-ssh.log");
        touch("some-other-file.txt");
        // A directory whose *name* matches the pattern exactly -- must be
        // skipped by the `metadata.is_file()` check, not just the string
        // match (a distinct digest so it doesn't collide with the real file
        // of the same name above).
        std::fs::create_dir(dir.path().join("isekai-ssh-holder-dirtest-ssh.log")).unwrap();

        let mut entries = collect_holder_logs(dir.path());
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let names_and_kinds: Vec<(String, &str)> =
            entries.iter().map(|e| (e.path.file_name().unwrap().to_string_lossy().into_owned(), e.kind)).collect();

        assert_eq!(
            names_and_kinds,
            vec![
                ("isekai-ssh-holder-abc123-ssh.log".to_string(), "isekai-ssh holder log"),
                ("isekai-ssh-holder-abc123-ssh.log.1".to_string(), "isekai-ssh holder log"),
                ("isekai-ssh-holder-abc123.log".to_string(), "isekai-pipe connect log"),
                ("isekai-ssh-holder-abc123.log.1".to_string(), "isekai-pipe connect log"),
            ],
            "must classify live+rotated pipe/ssh logs correctly, ignore isekai-ssh.log and unrelated files, and skip the look-alike directory"
        );
    }
}
