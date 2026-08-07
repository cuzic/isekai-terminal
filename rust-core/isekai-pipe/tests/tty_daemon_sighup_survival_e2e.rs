//! End-to-end test for `isekai-pipe tty daemon`/`tty attach`'s single most
//! important property — the one a design review found missing before any
//! code shipped (see `src/tty/daemon.rs::spawn_detached`'s doc comment):
//! the daemon must survive its *spawning session* being torn down via
//! `SIGHUP`, not just an ordinary clean exit. A daemon that only survives a
//! clean detach can still look like it works in casual manual testing while
//! failing the exact scenario this whole feature exists for.
//!
//! This deliberately does **not** drive the test through a real `sshd`: the
//! SSH transport layer isn't what's new or risky here (`isekai-ssh`
//! exercises that extensively elsewhere already) — what's new, and what
//! this test actually needs to prove, is `tty-daemon`'s own
//! session/process-group detachment. `setsid(1)` — the same "create a new
//! session for this child" primitive `sshd` itself uses for an interactive
//! session — stands in for the SSH channel, and a `SIGHUP` to that
//! session's process group stands in for `sshd` tearing the session down on
//! a dropped connection. Both are the actual OS-level mechanism this
//! feature has to survive, just reached without an intervening network hop.
//!
//! Unix-only (matches the feature itself, `#[cfg(unix)]` throughout
//! `src/tty/`) and skipped outright if `setsid(1)` isn't on `$PATH` (not
//! installed on every minimal container image) rather than failing —
//! opportunistic in the same spirit as this project's other environment
//! dependent tests (see `environment-skip` conventions elsewhere in this
//! workspace).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

fn isekai_pipe_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-pipe"))
}

fn setsid_available() -> bool {
    std::process::Command::new("setsid").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

fn socket_path(home: &Path, name: &str) -> PathBuf {
    home.join(".cache").join("isekai-pipe").join("tty").join(format!("{name}.sock"))
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

/// Finds the pid of a running `isekai-pipe tty daemon <name>` process by
/// scanning `/proc/*/cmdline` — the most direct way to confirm "is this the
/// *same* daemon instance as before" (identical pid across the `SIGHUP`)
/// rather than merely "is *a* daemon now reachable" (which a respawned
/// daemon would also satisfy, silently masking exactly the bug this test
/// exists to catch).
fn find_daemon_pid(name: &str) -> Option<u32> {
    let needle_daemon = "daemon";
    let needle_name = name;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let pid: u32 = match entry.file_name().to_str().and_then(|s| s.parse().ok()) {
            Some(pid) => pid,
            None => continue,
        };
        let cmdline = match std::fs::read(entry.path().join("cmdline")) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        // `cmdline` is NUL-separated argv, e.g. b"isekai-pipe\0tty\0daemon\0<name>\0".
        let args: Vec<&str> = cmdline.split(|&b| b == 0).filter_map(|s| std::str::from_utf8(s).ok()).filter(|s| !s.is_empty()).collect();
        if args.iter().any(|a| a.ends_with("isekai-pipe")) && args.contains(&"tty") && args.contains(&needle_daemon) && args.contains(&needle_name) {
            return Some(pid);
        }
    }
    None
}

#[tokio::test]
async fn tty_daemon_survives_sighup_to_its_spawning_session() {
    if !setsid_available() {
        eprintln!("skipping: setsid(1) not found on $PATH");
        return;
    }

    let home = tempfile::tempdir().unwrap();
    let name = "e2e-sighup-survival";

    // Stands in for the SSH channel's own session: a genuinely new
    // session+process-group, exactly like what `sshd` creates for an
    // interactive session. `tty attach` here will find no daemon running,
    // spawn one (via `spawn_detached`, which must escape *this* session),
    // and then sit relaying stdio.
    let mut fake_session = std::process::Command::new("setsid")
        .arg(isekai_pipe_bin_path())
        .args(["tty", "attach", name])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn `setsid isekai-pipe tty attach`");
    let fake_session_pid = fake_session.id() as i32;

    // Wait for the daemon to exist and bind its socket (connect -> fail ->
    // spawn_detached -> daemon acquires its lock + binds -> tty attach's
    // own retry loop connects).
    let socket = socket_path(home.path(), name);
    assert!(wait_until(Duration::from_secs(10), Duration::from_millis(100), || socket.exists()).await, "the daemon never bound its socket at {socket:?}");

    let pid_before = find_daemon_pid(name).expect("the spawned daemon's pid must be discoverable via /proc before the SIGHUP");

    // The actual OS-level event this feature must survive: `sshd` sending
    // `SIGHUP` to the whole session when the connection drops. `killpg`
    // targets every process whose *process group* is `fake_session_pid`
    // (which `setsid` made both the new session id and the initial process
    // group id of) — the daemon, if it correctly called its own `setsid`,
    // is by now in a *different* process group and must not receive this.
    // SAFETY: `fake_session_pid` is a valid pid this process just spawned
    // and still owns (no wait() has reaped it yet).
    let rc = unsafe { libc::killpg(fake_session_pid, libc::SIGHUP) };
    assert_eq!(rc, 0, "killpg(SIGHUP) itself must succeed: {}", std::io::Error::last_os_error());

    // The fake session (still holding the SSH-channel role) must actually
    // die from this -- otherwise the SIGHUP was never delivered/effective
    // and the rest of this test would trivially "pass" for the wrong reason.
    let fake_session_exit = tokio::time::timeout(Duration::from_secs(5), tokio::task::spawn_blocking(move || fake_session.wait()))
        .await
        .expect("the fake session must exit soon after SIGHUP, not hang")
        .expect("wait() task panicked")
        .expect("wait() itself failed");
    assert!(!fake_session_exit.success(), "SIGHUP must have actually terminated the fake session for this test to mean anything");

    // The decisive assertion: the daemon is still running, as the *same*
    // process (identical pid), well after its spawning session was torn
    // down -- not merely "some daemon is reachable again" (a respawn would
    // also satisfy that, silently masking the bug this test exists for).
    tokio::time::sleep(Duration::from_millis(200)).await;
    let pid_after = find_daemon_pid(name);
    assert_eq!(pid_after, Some(pid_before), "the daemon must survive as the SAME process across a SIGHUP to its spawning session");

    // Cleanup: a fresh `tty attach` can still reach it and cleanly ends the
    // shell (`exit`), which per this feature's own "daemon lifetime = shell
    // lifetime" design tears the daemon down too — leaves no process or
    // socket behind for this test to leak.
    cleanup(&home, name).await;
}

async fn cleanup(home: &tempfile::TempDir, name: &str) {
    use tokio::io::AsyncWriteExt as _;

    let mut cleanup_attach = tokio::process::Command::new(isekai_pipe_bin_path())
        .args(["tty", "attach", name])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn cleanup tty attach");
    if let Some(mut stdin) = cleanup_attach.stdin.take() {
        let _ = stdin.write_all(b"exit\n").await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), cleanup_attach.wait()).await;
}
