//! An opt-in, executable-file-based extension point for external consumers
//! to react to this tab's aggregate-state transitions without touching this
//! crate's Rust code — the same shape as a git hook. Purely additive to
//! [`super::delivery`], never a replacement: `daemon.rs::execute_actions`
//! calls both for every `Action`. Follows `delivery.rs`'s blanket policy (see
//! its module docs) that a misconfigured or unusual environment must never
//! make a Claude Code hook fail — a missing, non-executable, or failing
//! command hook is always a silent no-op here too.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::AsyncWriteExt as _;

use super::state::TabState;

/// `<hooks_dir>/on-<aggregate_name>` — `hooks_dir` itself (normally
/// `~/.config/claude-hookd/hooks`) is resolved once in `main.rs` at spawn
/// time and threaded through [`super::daemon::DaemonConfig`], so this
/// function stays a pure path computation with no environment access of its
/// own (consistent with the rest of this crate's config-is-passed-in, not
/// read-internally, convention — see `main.rs`'s `daemon_sock_dir`).
fn hook_path(hooks_dir: &Path, aggregate_name: &str) -> PathBuf {
    hooks_dir.join(format!("on-{aggregate_name}"))
}

/// True iff `path` exists, is a regular file, and has at least one
/// executable bit set for *somebody* (owner/group/other) — deliberately not
/// "executable by this process' uid specifically": a hook file with the
/// wrong owner bits set is just treated as "no hook configured," the same
/// silent no-op as a missing file, not a distinguished error.
pub(crate) async fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    match tokio::fs::metadata(path).await {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// Fires `hooks_dir`'s `on-<aggregate_name>` command hook, if one exists and
/// is executable: `argv[1]` is `aggregate_name`, stdin is `state`'s JSON
/// representation (`TabState::to_hook_json`, using the same `now` the caller
/// already used for the transition that triggered this call — consistent
/// with `to_hook_json`'s own pure-function shape, and avoids a second,
/// slightly-later `Instant::now()` call for no reason). `hooks_dir` being
/// `None` (e.g. `$HOME` was unset when `main.rs` resolved it) is itself a
/// silent no-op, same as every other failure mode here.
///
/// Best-effort and non-blocking: `spawn`ing the child, writing its stdin,
/// and reaping its exit status all happen on a detached task this function
/// never awaits — not just the reap. A hook script that never reads stdin
/// and a payload larger than the pipe buffer (64KiB on Linux) would
/// otherwise make `write_all` block forever (adversarial review, 2026-08:
/// an earlier version only detached the `wait()`, leaving the write on this
/// daemon's own event loop — harmless for today's small payloads, but a
/// latent full-loop stall waiting to happen once a hook script or a
/// session's session count grows).
///
/// The child inherits this *daemon* process's environment and working
/// directory — the daemon spawned by whichever hook event won the spawn
/// race (see `main.rs::spawn_detached_daemon`'s own docs on why colors are
/// passed as explicit arguments rather than left to env var inheritance for
/// the same reason) — **not** the environment of the Claude Code session
/// whose transition triggered this call. A hook script must treat stdin's
/// JSON as the only trustworthy source of "which session, what state";
/// reading `$CLAUDE_*`/`$PWD` here would silently pick up the wrong
/// session's values. Also worth knowing: Rust sets `SIGPIPE` to `SIG_IGN`
/// and child processes inherit that disposition, which can make a POSIX
/// shell pipeline *inside* a hook script (e.g. `cmd | head`) hang instead of
/// the writer receiving `SIGPIPE` the way it would from an interactive
/// shell — a hook script that pipes its own output should account for this.
pub(crate) async fn run_hook(hooks_dir: Option<&Path>, aggregate_name: &str, state: &TabState, now: std::time::Instant) {
    let Some(hooks_dir) = hooks_dir else { return };
    let path = hook_path(hooks_dir, aggregate_name);
    if !is_executable(&path).await {
        return;
    }
    let payload = state.to_hook_json(aggregate_name, now).to_string();
    let aggregate_name = aggregate_name.to_string();
    let mut cmd = tokio::process::Command::new(&path);
    cmd.arg(&aggregate_name).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else { return };
    let stdin = child.stdin.take();
    tokio::spawn(async move {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(payload.as_bytes()).await;
            // `stdin` drops here, closing the pipe's write end — a hook
            // script reading to EOF (e.g. `cat`) sees one, instead of
            // hanging until this whole daemon process exits.
        }
        // Reaps the child so it never lingers as a zombie. The exit status
        // itself is intentionally discarded: nothing here is in a position
        // to act on a failing hook script beyond "it ran".
        let _ = child.wait().await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    async fn poll_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> Result<(), ()> {
        let deadline = Instant::now() + timeout;
        loop {
            if condition() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Writes an executable shell script at `hooks_dir/on-<aggregate_name>`
    /// that appends `argv[1]` and its stdin to `out_path`, so a test can
    /// later assert on exactly what `run_hook` invoked it with.
    fn write_recording_hook(hooks_dir: &Path, aggregate_name: &str, out_path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(hooks_dir).unwrap();
        let script_path = hooks_dir.join(format!("on-{aggregate_name}"));
        std::fs::write(&script_path, format!("#!/bin/sh\necho \"$1\" > {out_path:?}.arg\ncat > {out_path:?}.stdin\n")).unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
    }

    #[tokio::test]
    async fn no_hooks_dir_is_a_silent_no_op() {
        // `hooks_dir: None` (the `$HOME`-unset case) must never panic or
        // hang — this is the "behavior is completely unchanged when nothing
        // is configured" guarantee the rest of this crate promises for every
        // optional mechanism.
        run_hook(None, "attention", &TabState::new(), Instant::now()).await;
    }

    #[tokio::test]
    async fn nonexistent_hooks_dir_is_a_silent_no_op() {
        // The realistic default for most users: `main.rs::hooks_dir()`
        // returns `Some(~/.config/claude-hookd/hooks)` unconditionally
        // whenever `$HOME` is set, whether or not that directory (or anyone
        // in it) actually exists — so `hooks_dir: Some(_)` pointing at
        // nothing is the common case this crate must tolerate, not `None`
        // (adversarial review, 2026-08: an earlier test pass over-relied on
        // `None`, which only the `$HOME`-unset case actually produces).
        let dir = tempfile::tempdir().unwrap();
        run_hook(Some(&dir.path().join("does-not-exist")), "attention", &TabState::new(), Instant::now()).await;
    }

    #[tokio::test]
    async fn missing_hook_file_is_a_silent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        run_hook(Some(dir.path()), "attention", &TabState::new(), Instant::now()).await;
        // Nothing to assert beyond "did not panic/hang" — there is no hook
        // file, so by construction nothing else could have happened.
    }

    #[tokio::test]
    async fn non_executable_hook_file_is_a_silent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("marker");
        // The script *would* touch `out` if it ran — deliberately not
        // `echo`/stdout (adversarial review, 2026-08: a prior version wrote
        // to stdout, which `run_hook` redirects to `/dev/null` and which the
        // script never touched `out` at all either way, so the assertion
        // below passed regardless of whether the exec-bit check actually
        // rejected the file — a vacuous test).
        std::fs::write(dir.path().join("on-attention"), format!("#!/bin/sh\ntouch {out:?}\n")).unwrap();
        // Deliberately left at the default (non-executable) mode `write`
        // creates a new file with.
        run_hook(Some(dir.path()), "attention", &TabState::new(), Instant::now()).await;
        // Give a spawn-that-should-not-happen a brief window, then confirm
        // the marker never appeared — `is_executable` must have rejected the
        // file before `Command::spawn` was ever reached.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!out.exists());
    }

    #[tokio::test]
    async fn existing_executable_hook_is_invoked_with_argv1_and_stdin_json() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        write_recording_hook(dir.path(), "attention", &out);

        let now = Instant::now();
        let (state, _) =
            crate::state::apply_event(&TabState::new(), "s1", crate::state::HookEvent::Notify, now, Duration::from_secs(600), Duration::from_secs(1800));
        run_hook(Some(dir.path()), "attention", &state, now).await;

        // Polls on the *stdin* file actually containing complete, parseable
        // JSON — not merely existing (adversarial review, 2026-08-09: under
        // load, `> file` truncate-creates it before the redirected `cat`
        // has copied every byte from the pipe, so an existence-only check
        // flaked with a real, reproducible "EOF while parsing a value" on
        // this exact sandbox).
        poll_until(Duration::from_secs(2), || {
            out.with_extension("arg").exists()
                && std::fs::read_to_string(out.with_extension("stdin"))
                    .ok()
                    .is_some_and(|s| serde_json::from_str::<serde_json::Value>(&s).is_ok())
        })
        .await
        .expect("hook script must run and produce both output files");

        assert_eq!(std::fs::read_to_string(out.with_extension("arg")).unwrap().trim(), "attention");
        let stdin_json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(out.with_extension("stdin")).unwrap()).unwrap();
        assert_eq!(stdin_json["aggregate"], "attention");
        assert_eq!(stdin_json["sessions"]["s1"]["kind"], "attention");
    }

    #[tokio::test]
    async fn a_hanging_hook_does_not_block_run_hook_itself() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("on-idle");
        // Sleeps far longer than any sane test timeout — if `run_hook`
        // awaited the child instead of detaching it, this test would hang.
        std::fs::write(&script_path, "#!/bin/sh\nsleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        tokio::time::timeout(Duration::from_secs(2), run_hook(Some(dir.path()), "idle", &TabState::new(), Instant::now()))
            .await
            .expect("run_hook must return promptly even when the hook script hangs");
    }

    #[tokio::test]
    async fn a_hook_that_never_reads_stdin_does_not_block_run_hook_itself() {
        // Pins the fix for the write-side non-blocking bug (adversarial
        // review, 2026-08): a hook script that immediately exits without
        // reading stdin at all, fed a payload comfortably larger than a
        // pipe buffer (64KiB on Linux), used to make `write_all` block on
        // this daemon's own event loop until the *whole daemon process*
        // exited — because only `child.wait()`, not the write, was ever
        // detached onto a spawned task.
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("on-attention");
        std::fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let now = Instant::now();
        let mut state = TabState::new();
        for i in 0..5000 {
            let (next, _) = crate::state::apply_event(
                &state,
                &format!("session-{i}"),
                crate::state::HookEvent::Notify,
                now,
                Duration::from_secs(600),
                Duration::from_secs(1800),
            );
            state = next;
        }

        tokio::time::timeout(Duration::from_secs(2), run_hook(Some(dir.path()), "attention", &state, now))
            .await
            .expect("run_hook must return promptly even when the hook never reads a large stdin payload");
    }
}
