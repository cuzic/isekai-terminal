//! Regression coverage for a real bug found via Opus pre-mortem review
//! (2026-08-12, `daemon.rs::run`'s own doc comment on `ctl_sock_file` has
//! the full mechanism): `isekai-ssh`'s ctl-socket forward
//! (`ctl_forward.rs`) exports `$ISEKAI_CTL_SOCK` fresh on *every* SSH
//! invocation (a brand-new random `-R` remote path each time), but
//! `isekai-pipe tty daemon`'s pty shell only ever inherits that value once,
//! at the moment its daemon happens to be spawned — every later reconnect
//! to the same `--isekai-tty` session just dials the already-running
//! daemon directly (`attach.rs::run`'s `spawn_detached` is only called on
//! the very first connection to a session name), so the persistent shell's
//! `$ISEKAI_CTL_SOCK` goes stale the moment that first SSH connection ends,
//! breaking every `isekai-pipe ctl` (title/clipboard/notify) invocation
//! from inside that shell for the rest of the daemon's life.
//!
//! Fixed by threading a stable *indirection* (`$ISEKAI_TTY_CTL_SOCK_FILE`,
//! set once at shell-spawn time, pointing at a small file `attach.rs`
//! rewrites with its own fresh `$ISEKAI_CTL_SOCK` on every invocation
//! including reconnects) instead of relying on the shell's own frozen
//! environment — see `ctl.rs`'s `ENV_TTY_CTL_SOCK_FILE`/
//! `resolve_ctl_socket_path_with` for the reader side (covered by unit
//! tests there; this file covers the daemon/attach process-spawning side,
//! which those unit tests cannot reach).

#![cfg(unix)]

use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

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

fn open_pty_pair() -> (OwnedFd, RawFd) {
    let mut master_fd: libc::c_int = -1;
    let mut slave_fd: libc::c_int = -1;
    // SAFETY: `openpty` writes valid, newly-opened fds into both out-params
    // on success; `name`/`termp`/`winp` null is documented as accepted
    // (defaults).
    let rc = unsafe { libc::openpty(&mut master_fd, &mut slave_fd, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(rc, 0, "openpty failed: {}", std::io::Error::last_os_error());
    // SAFETY: `master_fd` was just returned by `openpty` as a valid, open,
    // uniquely-owned descriptor.
    (unsafe { OwnedFd::from_raw_fd(master_fd) }, slave_fd)
}

fn dup_stdio(slave_fd: RawFd) -> Stdio {
    // SAFETY: `slave_fd` is a valid, open fd for the duration of this call
    // (the caller keeps its own copy open until every dup has been made).
    let dup_fd = unsafe { libc::dup(slave_fd) };
    assert!(dup_fd >= 0, "dup failed: {}", std::io::Error::last_os_error());
    // SAFETY: `dup_fd` was just returned by `dup` as a valid, open,
    // uniquely-owned descriptor.
    unsafe { Stdio::from_raw_fd(dup_fd) }
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

/// The decisive end-to-end assertion: reconnecting to an already-running
/// `--isekai-tty` session with a *different* `$ISEKAI_CTL_SOCK` (standing
/// in for a real second SSH connection's own fresh `-R` forward) must
/// update the daemon-tracked indirection file to the new value — not leave
/// it pinned to whatever the first connection happened to export. Before
/// this fix there was no such file at all, so a naive read of "did the
/// daemon-tracked value follow the second connection" would trivially fail;
/// the meaningful regression this guards is a *correct-looking but
/// incomplete* fix that only writes the file on first spawn (the same
/// shape as the original bug, one layer down).
#[tokio::test]
async fn reconnecting_with_a_different_ctl_sock_updates_the_daemon_tracked_value() {
    let home = short_tmp_home();
    let name = "e2e-ctl-sock-staleness";
    let ctl_sock_file = home.path().join(".cache").join("isekai-pipe").join("tty").join(format!("{name}.ctl_sock"));

    // First "SSH connection": spawns the daemon, exports the first
    // session's own randomly-chosen ctl-socket forward path.
    let (master1, slave_fd1) = open_pty_pair();
    let mut attach1 = tokio::process::Command::new(isekai_pipe_bin_path())
        .args(["tty", "attach", name])
        .env("HOME", home.path())
        .env("ISEKAI_CTL_SOCK", "/tmp/isekai-pipe-ctl-session-one.sock")
        .stdin(dup_stdio(slave_fd1))
        .stdout(dup_stdio(slave_fd1))
        .stderr(dup_stdio(slave_fd1))
        .spawn()
        .expect("failed to spawn the first `isekai-pipe tty attach`");
    unsafe { libc::close(slave_fd1) };
    // `master1` is intentionally kept alive (not read from, not dropped)
    // for the rest of this function — closing a pty's master end can
    // deliver `SIGHUP` to the slave-side process, which would kill
    // `attach1` before it ever reaches `refresh_ctl_sock_file()`, defeating
    // the point of this test.

    let saw_first_value = wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
        std::fs::read_to_string(&ctl_sock_file).is_ok_and(|c| c.trim() == "/tmp/isekai-pipe-ctl-session-one.sock")
    })
    .await;
    assert!(saw_first_value, "the first connection must record its own $ISEKAI_CTL_SOCK into the daemon-tracked file");

    // Simulate the first SSH connection dropping (network loss, the user
    // closing their terminal) *without* the shell itself exiting — this is
    // exactly the scenario `--isekai-tty` exists to survive, and precisely
    // the moment the original bug's `$ISEKAI_CTL_SOCK` went stale forever.
    attach1.kill().await.expect("failed to kill the first attach process");
    let _ = attach1.wait().await;

    // Second "SSH connection", reconnecting to the *same* session name with
    // its own fresh ctl-socket forward path — the daemon is already
    // running, so this never re-execs the shell (the original bug's root
    // cause: nothing else would ever have propagated this fresh value).
    let (master2, slave_fd2) = open_pty_pair();
    let mut attach2 = tokio::process::Command::new(isekai_pipe_bin_path())
        .args(["tty", "attach", name])
        .env("HOME", home.path())
        .env("ISEKAI_CTL_SOCK", "/tmp/isekai-pipe-ctl-session-two.sock")
        .stdin(dup_stdio(slave_fd2))
        .stdout(dup_stdio(slave_fd2))
        .stderr(dup_stdio(slave_fd2))
        .spawn()
        .expect("failed to spawn the second `isekai-pipe tty attach`");
    unsafe { libc::close(slave_fd2) };
    // Same reasoning as `master1` above: kept alive, not dropped.

    let saw_second_value = wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
        std::fs::read_to_string(&ctl_sock_file).is_ok_and(|c| c.trim() == "/tmp/isekai-pipe-ctl-session-two.sock")
    })
    .await;
    assert!(
        saw_second_value,
        "reconnecting with a different $ISEKAI_CTL_SOCK must update the daemon-tracked file to the fresh value, \
         not leave it pinned to the first connection's now-dead forward — regression: this is the exact staleness \
         bug found via pre-mortem review, current file content: {:?}",
        std::fs::read_to_string(&ctl_sock_file)
    );

    let _ = attach2.kill().await;
    let _ = attach2.wait().await;
}

/// Scans `output` for a `<<...>>`-delimited match that looks like real
/// command output rather than the pty's own echo of the still-unexpanded
/// typed command line — see the caller's doc comment for why both can
/// appear in the same stream. Returns the *last* such match (a real shell
/// only executes the command once, so if this is called repeatedly as more
/// output arrives, the most recent valid match is the most complete one).
fn extract_reported_path(output: &str) -> Option<String> {
    output
        .split("<<")
        .skip(1)
        // `s.split_once(">>")` (not the unconditional `.split("...").next()`)
        // matters: on a partial read that has `<<` but no `>>` yet, treating
        // the unterminated remainder as a "match" would report a truncated
        // path as if it were complete.
        .filter_map(|s| s.split_once(">>"))
        .map(|(candidate, _rest)| candidate.trim())
        .filter(|candidate| !candidate.is_empty() && !candidate.contains('$') && !candidate.contains("printf"))
        .last()
        .map(str::to_string)
}

/// The pty shell itself must see a *stable* `$ISEKAI_TTY_CTL_SOCK_FILE`
/// pointing at exactly the file the first assertion above observes being
/// kept fresh — proving `pty.rs::spawn`'s env wiring actually reaches the
/// real shell process, not just the daemon's own bookkeeping.
#[tokio::test]
async fn the_ptys_shell_sees_isekai_tty_ctl_sock_file_pointing_at_the_daemon_tracked_file() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let home = short_tmp_home();
    let name = "e2e-ctl-sock-file-env";
    let expected_file = home.path().join(".cache").join("isekai-pipe").join("tty").join(format!("{name}.ctl_sock"));

    let mut attach = tokio::process::Command::new(isekai_pipe_bin_path())
        .args(["tty", "attach", name])
        .env("HOME", home.path())
        .env("ISEKAI_CTL_SOCK", "/tmp/isekai-pipe-ctl-probe.sock")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn `isekai-pipe tty attach`");

    let mut stdin = attach.stdin.take().expect("stdin must be piped");
    let mut stdout = attach.stdout.take().expect("stdout must be piped");

    stdin
        .write_all(b"printf '<<%s>>\\n' \"$ISEKAI_TTY_CTL_SOCK_FILE\"\n")
        .await
        .expect("failed to write the probe command to tty attach's stdin");

    // Real bug found writing this test (2026-08-12, caught by its own real
    // CI run, not a regression in the fix under test): the pty this probe
    // command runs over is a normal cooked-mode interactive shell, which
    // echoes back the *typed* command line (literally containing
    // `<<%s>>` as source text, since that's what was typed) before it ever
    // executes and produces the *real* substituted output. A naive
    // "stop reading at the first `>>`" scan matches that echo, not the real
    // output. `extract_reported_path` below only accepts a `<<...>>` match
    // whose content contains neither `$` nor the literal command name —
    // both are reliable markers of "this is still the unexpanded echoed
    // input line, not real command output" — and keeps reading until it
    // finds one or times out.
    let reported_path = tokio::time::timeout(Duration::from_secs(5), async {
        let mut collected = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stdout.read(&mut buf).await.expect("read from tty attach's stdout failed");
            assert_ne!(n, 0, "tty attach's stdout closed before the probe command's real output ever appeared; got so far: {:?}", String::from_utf8_lossy(&collected));
            collected.extend_from_slice(&buf[..n]);
            if let Some(path) = extract_reported_path(&String::from_utf8_lossy(&collected)) {
                return path;
            }
        }
    })
    .await
    .expect("did not see the probe command's real (non-echoed) output within 5s");
    assert_eq!(
        reported_path,
        expected_file.to_string_lossy(),
        "the pty shell's own $ISEKAI_TTY_CTL_SOCK_FILE must point at the exact file attach.rs keeps fresh, got {reported_path:?}"
    );

    let _ = stdin.write_all(b"exit 0\n").await;
    let _ = tokio::time::timeout(Duration::from_secs(5), attach.wait()).await;
}
