//! Regression coverage for a real bug found while writing
//! `tty_daemon_trailing_output_race_e2e.rs` (2026-08-12): a large enough
//! trailing burst overflows `attach_slot.rs`'s `OCCUPANT_CHANNEL_CAPACITY`
//! (256) — live delivery there is a non-blocking `try_send` that used to
//! just drop the chunk on the floor once full, with no recovery for a
//! client that stayed *continuously attached* through the drop (a
//! *reconnecting* client always got a correct ring-buffer replay via
//! `AttachSlot::install`; a live one had no equivalent). See
//! `attach_slot.rs`'s own module docs (point 1) for the full design this
//! fixes: `broadcast()` now flags `SlotState::missed` on a dropped
//! `try_send`, and `daemon.rs::handle_client`'s writer task checks
//! `take_missed` before forwarding *every* message, resyncing from
//! `AttachSlot::current_replay()` (a fresh ring-buffer snapshot — the ring
//! always has everything, since `broadcast` appends to it before ever
//! attempting live delivery) whenever it finds a drop happened.
//!
//! This uses the exact same shape of reproduction as the ordering-race
//! test (a large burst immediately followed by exit) but sized well past
//! the channel capacity specifically to exercise *this* mechanism, not
//! that one — see this file's own burst size choice below.

#![cfg(unix)]

use std::collections::HashSet;
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

/// Comfortably past `OCCUPANT_CHANNEL_CAPACITY` (256) — this exact size
/// (4000 lines) is what originally surfaced the drop while developing the
/// ordering-race fix in the same PR, before that test was deliberately
/// scaled down to avoid this separate mechanism. Kept here, at full size,
/// as the dedicated regression test for the drop itself.
const LINE_COUNT: usize = 4000;

fn burst_then_exit_script() -> String {
    format!("sleep 1; i=1; while [ \"$i\" -le {LINE_COUNT} ]; do echo \"line-$i\"; i=$((i+1)); done")
}

#[tokio::test]
async fn a_burst_that_overflows_the_occupant_channel_still_reaches_the_client_via_resync() {
    let home = short_tmp_home();
    let name = "e2e-dropped-output-resync";
    let sock_path = home.path().join(".cache").join("isekai-pipe").join("tty").join(format!("{name}.sock"));

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

    let mut collected = Vec::new();
    let mut buf = [0u8; 8192];
    let drained = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let n = stdout.read(&mut buf).await.expect("read from tty attach's stdout failed");
            if n == 0 {
                break;
            }
            collected.extend_from_slice(&buf[..n]);
        }
    })
    .await;
    assert!(drained.is_ok(), "tty attach's stdout must close (the connection ending) within 15s of the shell exiting");

    let output = String::from_utf8_lossy(&collected);
    let seen: HashSet<&str> = output.lines().collect();
    let missing: Vec<usize> = (1..=LINE_COUNT).filter(|i| !seen.contains(format!("line-{i}").as_str())).collect();
    assert!(
        missing.is_empty(),
        "every line from a burst that overflows the occupant channel must still reach the client via resync — \
         regression: a dropped live chunk used to be lost forever for a continuously-attached client; \
         {} of {} lines missing, first few: {:?}",
        missing.len(),
        LINE_COUNT,
        &missing[..missing.len().min(10)]
    );

    let exited = tokio::time::timeout(Duration::from_secs(5), attach.wait()).await;
    assert!(exited.is_ok(), "isekai-pipe tty attach must exit within 5s of the connection ending");

    let _ = daemon.kill().await;
    let _ = daemon.wait().await;
}
