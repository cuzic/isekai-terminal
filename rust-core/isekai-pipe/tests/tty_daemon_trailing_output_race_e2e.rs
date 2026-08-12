//! Regression coverage for a real bug found via Opus pre-mortem review
//! (2026-08-12, `daemon.rs::run`'s own doc comment near
//! `READ_LOOP_DRAIN_TIMEOUT` has the full mechanism): `run()` used to call
//! `attach_slot.notify_exit()` immediately after `child.wait()` resolved,
//! with no synchronization against `read_loop` — a fully independent task
//! that reads pty output and broadcasts it to the current occupant. Since
//! `child.wait()` only reports that the shell process has *terminated*
//! (which says nothing about whether every byte it wrote to the pty right
//! before exiting has actually been read and relayed yet), a burst of
//! trailing output followed immediately by exit could race: if
//! `notify_exit`'s `Frame::Exit` reached the attached client's writer task
//! before some trailing `Frame::Stdout` chunk(s) `read_loop` hadn't
//! broadcast yet, the writer task processed `Exit` first and returned
//! immediately — permanently dropping whatever output was still queued
//! behind it. A real user impact: the last line of a command's output
//! right before it exits (a final error message, a build's "done" line)
//! could silently never reach the attached client.
//!
//! Fixed by awaiting `read_loop`'s own completion (bounded by
//! `READ_LOOP_DRAIN_TIMEOUT`) before calling `notify_exit` — `read_loop`
//! only finishes once the pty gives EOF, i.e. after every byte it ever
//! held has already been broadcast, so this closes the race entirely.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn isekai_pipe_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-pipe"))
}

/// See `tty_attach_exit_and_rawmode_e2e.rs`'s identical helper's doc
/// comment: a short, `/tmp`-rooted `$HOME` sidesteps macOS CI's long
/// default temp-dir base, which otherwise overflows `sockaddr_un`'s
/// `sun_path` for this feature's own daemon socket.
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

const LAST_LINE: &str = "line-200";

/// A modest burst followed *immediately* by the shell exiting — no trailing
/// delay at all, maximizing the chance that `child.wait()` resolves before
/// `read_loop` has caught up, which is exactly the window the original bug
/// raced in. The leading `sleep 1` gives this test time to attach *before*
/// the burst starts, so the assertion below exercises the *live* broadcast
/// path (the one the bug actually affects), not `AttachSlot::install`'s
/// replay-from-ring-buffer path (which would mask the bug — see this
/// file's own module docs).
///
/// Deliberately kept modest (200 lines, not thousands): a large enough
/// burst starts tripping a *separate*, already-known limitation
/// (`attach_slot.rs`'s `OCCUPANT_CHANNEL_CAPACITY` — live delivery uses a
/// bounded, non-blocking `try_send` that silently drops once the occupant
/// channel is full, a deliberate trade-off documented on `broadcast`'s own
/// doc comment, not something this fix touches). This file's regression
/// target is specifically the `notify_exit`-vs-`read_loop` *ordering* race,
/// which a burst well within that separate capacity limit is sufficient to
/// exercise on its own.
fn burst_then_exit_script() -> String {
    "sleep 1; i=1; while [ \"$i\" -le 200 ]; do echo \"line-$i\"; i=$((i+1)); done".to_string()
}

#[tokio::test]
async fn trailing_output_right_before_exit_always_reaches_the_attached_client() {
    let home = short_tmp_home();
    let name = "e2e-trailing-output-race";
    let sock_path = home.path().join(".cache").join("isekai-pipe").join("tty").join(format!("{name}.sock"));

    // Spawn the daemon directly (bypassing `tty attach`'s auto-spawn) so a
    // custom command can be given — production always goes through
    // `spawn_detached`, but the code path under test (`daemon::run`) is
    // identical either way.
    let mut daemon = tokio::process::Command::new(isekai_pipe_bin_path())
        .args(["tty", "daemon", name, "--", "sh", "-c", &burst_then_exit_script()])
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn `isekai-pipe tty daemon`");

    let bound = wait_until(Duration::from_secs(5), Duration::from_millis(50), || sock_path.exists()).await;
    assert!(bound, "the daemon must bind its socket within 5s");

    let mut attach = tokio::process::Command::new(isekai_pipe_bin_path())
        .args(["tty", "attach", name])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn `isekai-pipe tty attach`");
    drop(attach.stdin.take());
    let mut stdout = attach.stdout.take().expect("stdout must be piped");

    // Collect every byte received until the connection ends (the shell's
    // `sleep 1` head start means the burst+exit happens well after this
    // loop is already reading live).
    let mut collected = Vec::new();
    let mut buf = [0u8; 8192];
    let drained = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let n = stdout.read(&mut buf).await.expect("read from tty attach's stdout failed");
            if n == 0 {
                break;
            }
            collected.extend_from_slice(&buf[..n]);
        }
    })
    .await;
    assert!(drained.is_ok(), "tty attach's stdout must close (the connection ending) within 10s of the shell exiting");

    let output = String::from_utf8_lossy(&collected);
    assert!(
        output.contains(LAST_LINE),
        "the shell's last line of output, printed immediately before it exited, must reach the attached client — \
         regression: notify_exit used to race ahead of read_loop's still-in-flight broadcasts, silently dropping \
         trailing output; last 200 chars received: {:?}",
        &output[output.len().saturating_sub(200)..]
    );

    let exited = tokio::time::timeout(Duration::from_secs(5), attach.wait()).await;
    assert!(exited.is_ok(), "isekai-pipe tty attach must exit within 5s of the connection ending");

    let _ = daemon.kill().await;
    let _ = daemon.wait().await;
}
