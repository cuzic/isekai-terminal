//! Regression coverage for a real gap found via Opus pre-mortem review
//! (2026-08-12, `daemon.rs`'s own `PTY_TERM` doc comment has the full
//! design rationale): the pty shell's own `$TERM` used to be
//! `std::env::var("TERM")` read from the *daemon* process's own
//! environment — itself just whatever `$TERM` the *first* `tty attach`
//! invocation to ever spawn this daemon happened to have. Every later
//! reconnecting client's own, possibly different, `$TERM` (carried in its
//! `Frame::Hello`) was silently ignored — unlike `cols`/`rows`, which are
//! correctly refreshed on every reconnect. Since a running shell's
//! environment can't be changed retroactively from outside, and this
//! daemon deliberately isn't a terminal multiplexer with its own
//! capability-translation layer (see `daemon.rs`'s module docs), the fix
//! settles for a well-known, honestly-documented fixed value
//! (`PTY_TERM`) instead of the previous "correct for whichever client
//! happened to be first, wrong for everyone else" behavior.

#![cfg(unix)]

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

#[tokio::test]
async fn the_ptys_term_is_the_fixed_default_regardless_of_the_daemons_own_environment() {
    let home = short_tmp_home();
    let name = "e2e-fixed-term";
    let output_file = home.path().join("term.txt");

    let mut daemon = tokio::process::Command::new(isekai_pipe_bin_path())
        .args(["tty", "daemon", name, "--", "sh", "-c", &format!("printf '%s' \"$TERM\" > {}; sleep 30", output_file.display())])
        .env("HOME", home.path())
        // A deliberately *different* value from the fixed default this
        // daemon must use for its pty shell — proving the shell's own
        // `$TERM` doesn't come from here.
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn `isekai-pipe tty daemon`");

    let wrote_output = wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
        std::fs::metadata(&output_file).is_ok_and(|m| m.len() > 0)
    })
    .await;
    assert!(wrote_output, "the pty-owned process must run and write out its own $TERM within 5s");

    let term = std::fs::read_to_string(&output_file).expect("failed to read the $TERM output file");
    assert_eq!(
        term, "xterm-256color",
        "the pty shell's $TERM must always be the fixed default, not whatever the daemon process's own \
         environment (here deliberately set to \"dumb\") happened to have — regression: this used to leak \
         through from std::env::var(\"TERM\"), which itself was really just an accident of whichever `tty \
         attach` invocation first spawned the daemon"
    );

    let _ = daemon.kill().await;
    let _ = daemon.wait().await;
}
