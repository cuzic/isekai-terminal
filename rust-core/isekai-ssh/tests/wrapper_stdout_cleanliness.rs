//! stdout purity for `isekai-ssh`'s wrapper mode (`isekai-ssh <destination>
//! [args...]`, `wrapper.rs`), the entry point `archive/ISEKAI_PIPE_MIGRATION.md` P4
//! introduced alongside the legacy `isekai-ssh connect` subcommand already
//! covered by `stdout_cleanliness.rs`.
//!
//! The wrapper never owns the SSH byte stream itself (`main.rs`'s docs:
//! `isekai-ssh` "はIPアドレスやUDP socketを所有しない" — once it decides to
//! proceed, it execs the real `ssh` with an injected
//! `ProxyCommand=isekai-pipe connect ...` and inherits stdio straight
//! through to that child process). So the only stdout-purity invariant to
//! check here is the *error* paths that fire before `ssh` is ever spawned:
//! today, that is exactly the "this destination has no trust store entry
//! yet" case (`wrapper::build_connection_intent`), since automatic
//! `isekai-pipe serve` bootstrap deployment is not wired up yet
//! (`wrapper.rs`'s own `run()` doc comment). Every one of those paths must
//! write nothing to stdout and report the failure on stderr only — a
//! polluted stdout here would corrupt whatever real `ProxyCommand` byte
//! stream `ssh` was expecting to read, exactly like the legacy `connect`
//! subcommand's invariant.

use std::path::PathBuf;
use std::process::Stdio as StdStdio;
use std::time::Duration;

use tokio::process::Command as TokioCommand;

fn isekai_ssh_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-ssh"))
}

fn ssh_available() -> bool {
    std::process::Command::new("ssh")
        .arg("-V")
        .stdin(StdStdio::null())
        .stdout(StdStdio::null())
        .stderr(StdStdio::null())
        .status()
        .is_ok()
}

/// Runs `isekai-ssh <destination> [extra_args...]` directly (this test's
/// own `Stdio::piped()`, not through a real `ssh` parent process) and waits
/// for it to exit, bounded by a generous timeout. Every scenario in this
/// file fails closed before spawning the real `ssh` child, so none of them
/// should ever approach the timeout in practice.
async fn run_wrapper_to_completion(
    home: &std::path::Path,
    args: &[&str],
    rust_log: Option<&str>,
) -> std::process::Output {
    let mut cmd = TokioCommand::new(isekai_ssh_bin_path());
    cmd.args(args)
        .env("HOME", home)
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped());
    match rust_log {
        Some(level) => {
            cmd.env("RUST_LOG", level);
        }
        None => {
            cmd.env_remove("RUST_LOG");
        }
    }

    tokio::time::timeout(Duration::from_secs(15), cmd.output())
        .await
        .expect("wrapper should fail closed quickly on every path exercised in this file, not hang")
        .expect("failed to spawn isekai-ssh")
}

#[tokio::test(flavor = "multi_thread")]
async fn wrapper_stdout_empty_for_untrusted_destination_default_logging() {
    if !ssh_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // Deliberately no known_helpers.toml and no ~/.ssh/config: the wrapper's
    // default `bootstrap-policy` (`auto`) means the trust-store miss routes
    // into "bootstrap is required, but automatic deployment is not wired
    // yet" (`wrapper.rs::run`), which must fail before ever touching stdout.

    let output = run_wrapper_to_completion(&home, &["wrapper-untrusted-host"], None).await;

    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty for an untrusted destination, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!output.status.success(), "wrapper must exit non-zero for an untrusted destination");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not a trusted host") || stderr.contains("bootstrap"),
        "stderr should explain the trust-store miss / bootstrap requirement, got: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wrapper_stdout_empty_for_untrusted_destination_with_trace_logging() {
    if !ssh_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let output = run_wrapper_to_completion(&home, &["wrapper-untrusted-host"], Some("trace")).await;

    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty for an untrusted destination even under RUST_LOG=trace, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!output.status.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn wrapper_stdout_empty_for_untrusted_destination_with_no_bootstrap_flag() {
    if !ssh_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    // `--isekai-no-bootstrap` takes the *other* branch out of
    // `build_connection_intent`'s `Err` match in `wrapper::run` (the plain
    // `isekai_trust`-miss error, not the "bootstrap required" wrapper) --
    // exercise it too, since it is a materially different code path that
    // must uphold the same stdout-purity invariant.
    let output =
        run_wrapper_to_completion(&home, &["--isekai-no-bootstrap", "wrapper-untrusted-host"], None)
            .await;

    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty for an untrusted destination with --isekai-no-bootstrap, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not a trusted host"),
        "stderr should explain the trust-store miss, got: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wrapper_dry_run_explain_output_goes_to_stderr_only() {
    if !ssh_available() {
        eprintln!("skipping: ssh(1) not available in this environment");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    // `--isekai-dry-run` succeeds (exit 0) without ever building a
    // `ConnectionIntent` or spawning `ssh` -- it prints the resolved plan via
    // `eprintln!` (`wrapper.rs::run`) and returns. stdout must still stay
    // completely empty.
    let output = run_wrapper_to_completion(
        &home,
        &["--isekai-dry-run", "wrapper-dry-run-host"],
        None,
    )
    .await;

    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty for --isekai-dry-run, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(output.status.success(), "--isekai-dry-run should exit 0");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("resolved OpenSSH config") && stderr.contains("resolved isekai config"),
        "stderr should contain the resolved plan, got: {stderr}"
    );
}
