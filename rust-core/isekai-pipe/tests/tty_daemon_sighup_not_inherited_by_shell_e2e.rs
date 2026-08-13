//! Regression coverage for a real bug found via Opus pre-mortem review
//! (2026-08-12, `pty.rs::spawn`'s own doc comment near its `pre_exec` has
//! the full mechanism): `daemon.rs::spawn_detached` sets `SIGHUP` to
//! `SIG_IGN` for the *daemon* process itself (necessary so it survives the
//! SSH session that spawned it hanging up). Signal *dispositions* (unlike
//! blocked masks) survive `exec()`, so without an explicit reset, the pty
//! shell `pty.rs::spawn` execs from *within* the already-`SIG_IGN`'d
//! daemon process — and everything that shell later runs — silently
//! inherited `SIGHUP` ignored too. This has nothing to do with the tty
//! session's own controlling terminal; it was purely an accidental side
//! effect of which process happened to spawn the shell. A real login
//! shell (and everything it execs) always starts with ordinary,
//! unmodified signal dispositions, and anything inside this persistent
//! session that relies on normal `SIGHUP` behavior (some long-running
//! tool explicitly handling terminal hangup, `nohup`-adjacent semantics,
//! ...) would have silently seen the wrong thing.
//!
//! Fixed by resetting `SIGHUP` to `SIG_DFL` in `pty::spawn`'s own
//! `pre_exec`, right alongside `login_tty`.

// `/proc/<pid>/status`'s `SigIgn` field is Linux-specific (unlike the rest
// of this feature's e2e tests, which are `#![cfg(unix)]` and run on macOS
// CI too — checking a signal *disposition* portably without it would need
// a purpose-built helper binary this one check doesn't warrant, and the
// daemon this exercises is only ever deployed to Linux servers anyway,
// per `tty/mod.rs::default_shell_command`'s own doc comment).
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

fn isekai_pipe_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-pipe"))
}

fn short_tmp_home() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("e").tempdir_in("/tmp").expect("failed to create a short-path temp dir under /tmp")
}

async fn wait_until<F: Fn() -> bool>(timeout: Duration, poll_interval: Duration, condition: F) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(poll_interval).await;
    }
    condition()
}

/// Bit 0 of Linux's `/proc/<pid>/status` `SigIgn` hex bitmask is signal 1
/// (`SIGHUP`) — see `signal(7)`'s numbering, bit `N-1` for signal `N`.
fn sighup_bit_is_set(sig_ign_hex: &str) -> bool {
    let mask = u64::from_str_radix(sig_ign_hex.trim(), 16).expect("SigIgn must be a valid hex mask");
    (mask & 0b1) != 0
}

/// Drives this entirely through `isekai-pipe tty daemon`'s own custom-
/// command support (bypassing `tty attach`'s auto-spawn, which always
/// exec's a `$SHELL` this test would then need to reach through) with a
/// command that dumps its *own* `/proc/self/status` — the exact process
/// `pty::spawn` execs, checked directly rather than inferred from shell
/// behavior (real shells often reset signal handling themselves as part
/// of their own interactive-mode setup, which would mask this specific
/// bug even though non-shell commands run after wouldn't be protected).
#[tokio::test]
async fn the_ptys_child_process_does_not_inherit_sighup_ignored_from_the_daemon() {
    let home = short_tmp_home();
    let name = "e2e-sighup-not-inherited";
    let output_file = home.path().join("sigign.txt");

    let mut daemon = tokio::process::Command::new(isekai_pipe_bin_path())
        .args([
            "tty",
            "daemon",
            name,
            "--",
            "sh",
            "-c",
            &format!("grep SigIgn: /proc/self/status | awk '{{print $2}}' > {}; sleep 30", output_file.display()),
        ])
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn `isekai-pipe tty daemon`");

    let wrote_output = wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
        std::fs::metadata(&output_file).is_ok_and(|m| m.len() > 0)
    })
    .await;
    assert!(wrote_output, "the pty-owned process must run and write its /proc/self/status SigIgn field within 5s");

    let sig_ign = std::fs::read_to_string(&output_file).expect("failed to read the SigIgn output file");
    assert!(
        !sighup_bit_is_set(&sig_ign),
        "the pty shell (and anything it execs) must not inherit SIGHUP=SIG_IGN from the daemon that spawned it — \
         regression: signal dispositions survive exec(), so the daemon's own defensive SIG_IGN (needed for it to \
         survive the SSH session that spawned *it* hanging up) used to leak into every session this feature owns; \
         SigIgn was {sig_ign:?}"
    );

    let _ = daemon.kill().await;
    let _ = daemon.wait().await;
}
