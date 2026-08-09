//! Shared shell-command spawn + result-glob matching for Epic P build
//! profiles (`ISEKAI_PIPE_DESIGN.md` §8 Epic P), used both by
//! `isekai-ssh build-profile test` (`build_profile_cli.rs`, a local dry run)
//! and every real remote-triggered dispatch path — kept in one place so none
//! of them drift on how a profile's `command`/`result_glob` is actually
//! interpreted.
//!
//! Also home to the pieces of build-request handling that are identical
//! regardless of *which* ctl-socket dispatch path is driving them: reading a
//! spawned child's stdout/stderr into chunks ([`pump_bytes`]), encoding a
//! chunk/finish message ([`encode_build_output_chunk`]/
//! [`encode_build_finished`]), pushing a finished build's result files back
//! to the remote host ([`spawn_result_push`]) — and, since the Epic P ×
//! 2026-08 unification, [`run_build`] itself: the full 9-step
//! resolve-profile → spawn → pump → relay-until-disconnected →
//! kill-on-abort → report-exit-code → glob-results+push → `BuildFinished`
//! sequence, shared by the Unix ctl-socket path (`ctl_forward.rs::run_build`),
//! the Windows-native owner-foreground path (`native/mux/build_relay.rs::run_build_over_channel`),
//! and the Windows-native mux-client path (`native/mux/build_relay.rs`'s
//! `run_client_build`).
//!
//! Those 3 call sites used to each hand-roll this whole sequence, differing
//! only in their sink type (a plain `AsyncWrite` half, a `russh::Channel`, an
//! mpsc sender) and disconnect-detection story (write-failure only,
//! write-failure *and* a concurrent channel-close signal, or an external
//! abort signal) — abstracted away here by the single [`BuildRelay`] trait
//! (deliberately *one* trait covering both "send" and "watch for an external
//! disconnect signal", not two independent generic parameters: the
//! `russh::Channel`-backed implementation needs both methods to reborrow the
//! *same* `&mut Channel` at different points of one `select!` loop inside
//! [`run_build`], which only type-checks if they're both methods on one
//! `&mut self` — see that implementation's doc comment in `native/mux/build_relay.rs`).
//!
//! That duplication had already caused real drift twice before this
//! unification: one copy (the Unix path) was missing a `BuildFinished(126)`
//! reply on a spawn failure that the other two copies sent, and OSC 9;4
//! progress reporting ([`crate::ctl_forward::emit_build_progress_start`]/
//! `emit_build_progress_result`) existed only on the Unix path. Both are now
//! deliberately uniform across all 3 dispatch paths — an intentional
//! behavior change riding along with this refactor, not an accident.

use std::path::PathBuf;

use anyhow::{Context, Result};
use base64::Engine as _;
use isekai_protocol::{BuildOutputStream, CtlMessage};
use tokio::process::Command;

/// Builds (but does not spawn) the platform shell invocation for `command`
/// in `dir`, so a profile's `command` field can contain `&&`/pipes/etc.
/// rather than being limited to a single argv. Caller decides stdio wiring
/// (`build_profile_cli::test` inherits this process's own stdio;
/// `ctl_forward.rs` pipes stdout/stderr to stream them over the ctl-socket).
pub fn spawn_shell_command(command: &str, dir: &str) -> Command {
    let mut cmd = if cfg!(windows) {
        Command::new("cmd")
    } else {
        Command::new("sh")
    };
    if cfg!(windows) {
        cmd.arg("/C");
    } else {
        cmd.arg("-c");
    }
    cmd.arg(command);
    cmd.current_dir(dir);
    cmd
}

/// Matches `glob_pattern` (relative to `dir`) and returns the matched paths,
/// capped at `isekai_protocol::MAX_BUILD_RESULT_PATHS` entries — the same
/// cap `CtlMessage::BuildFinished`'s wire format enforces, so a build with
/// an overly broad glob is truncated here rather than being rejected later
/// by `validate_ctl_message` after the fact.
pub fn glob_results(dir: &str, glob_pattern: &str) -> Result<Vec<PathBuf>> {
    let pattern = std::path::Path::new(dir).join(glob_pattern);
    let pattern_str = pattern.to_string_lossy();
    let mut matches = Vec::new();
    for entry in
        glob::glob(&pattern_str).with_context(|| format!("isekai-ssh: invalid result glob {pattern_str:?}"))?
    {
        let path = entry.with_context(|| format!("isekai-ssh: failed to read a glob match under {dir:?}"))?;
        matches.push(path);
        if matches.len() >= isekai_protocol::MAX_BUILD_RESULT_PATHS {
            break;
        }
    }
    Ok(matches)
}

/// Reads `reader` in fixed-size chunks (not line-buffered — build tool
/// output isn't guaranteed to be UTF-8 or newline-terminated, e.g. a
/// carriage-return progress bar) and forwards each chunk to `tx`, stopping at
/// EOF, a read error, or once `tx`'s receiver is gone (the caller tore down
/// its side of the relay — e.g. the ctl connection broke).
pub async fn pump_bytes(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    stream: BuildOutputStream,
    tx: tokio::sync::mpsc::Sender<(BuildOutputStream, Vec<u8>)>,
) {
    use tokio::io::AsyncReadExt as _;
    // Comfortably under `MAX_BUILD_CHUNK_DECODED_LEN` (64 KiB) so every
    // chunk this sends passes the far end's `validate_ctl_message` cap.
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if tx.send((stream, buf[..n].to_vec())).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Encodes one `BuildOutputChunk` line (base64 + JSON + trailing newline) —
/// the wire-level part of relaying a build's stdout/stderr, shared by every
/// dispatch path so they can never disagree on encoding even if their sinks
/// differ.
pub fn encode_build_output_chunk(stream: BuildOutputStream, bytes: Vec<u8>) -> Result<Vec<u8>> {
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut out = serde_json::to_vec(&CtlMessage::BuildOutputChunk { stream, data_b64 })
        .context("isekai-ssh: failed to encode build output chunk")?;
    out.push(b'\n');
    Ok(out)
}

/// Encodes one `BuildFinished` line (JSON + trailing newline). See
/// [`encode_build_output_chunk`]'s docs for why this is shared rather than
/// duplicated per dispatch path.
pub fn encode_build_finished(exit_code: i32, result_paths: Vec<String>) -> Result<Vec<u8>> {
    let mut out = serde_json::to_vec(&CtlMessage::BuildFinished { exit_code, result_paths })
        .context("isekai-ssh: failed to encode build finished message")?;
    out.push(b'\n');
    Ok(out)
}

/// Pushes each of `result_paths` to `host`'s `dest_dir` via a recursive
/// `isekai-ssh <host> -- mkdir -p ... && cat > ...` invocation — reusing the
/// "`isekai-ssh <host>` always connects" machinery (bootstrap, resilience,
/// retries) rather than inventing a new bulk-transfer protocol
/// (`ISEKAI_PIPE_DESIGN.md` §8 Epic P). Spawned in the background rather
/// than awaited: the ctl connection (and the `BuildFinished` the caller sends
/// alongside) shouldn't stay open for however long the push takes, especially
/// for a large artifact over a slow link. A failed push is logged to this
/// process's own stderr, not surfaced back to the remote — there's no
/// channel left to report it on once the build's own ctl connection has
/// already sent `BuildFinished` and closed (a known v1 limitation,
/// `ISEKAI_PIPE_DESIGN.md` §8 Epic P).
///
/// `dest_dir`/`local_path` are entirely local, trusted config (the profile
/// the remote merely *named*, never authored) — not remote-supplied, so
/// they're interpolated into the remote shell command as-is, the same trust
/// boundary a profile's own `command` already gets. This also matters for
/// `~` in `dest_dir`: quoting it would suppress the remote shell's tilde
/// expansion.
pub fn spawn_result_push(host: String, dest_dir: String, result_paths: Vec<String>) {
    if result_paths.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for local_path in result_paths {
            if let Err(e) = push_result_file(&host, &dest_dir, &local_path).await {
                eprintln!("isekai-ssh: failed to push build result {local_path:?} to {host:?}:{dest_dir:?}: {e:#}");
            }
        }
    });
}

/// The remote command a recursive `isekai-ssh <host> -- <command>` runs to
/// place a pushed build result at `dest_dir`/`file_name`. Split out from
/// [`push_result_file`] so this string-construction logic is unit-testable
/// without a real recursive process spawn (see that function's docs on why
/// a unit test can't exercise the spawn itself). `dest_dir`/`file_name` are
/// local, trusted config/build-output — not remote-supplied — so they're
/// interpolated as-is rather than shell-quoted; quoting `dest_dir` would
/// also break `~` expansion in the remote shell.
pub(crate) fn build_push_remote_command(dest_dir: &str, file_name: &str) -> String {
    format!("mkdir -p {dest_dir} && cat > {dest_dir}/{file_name}")
}

async fn push_result_file(host: &str, dest_dir: &str, local_path: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt as _;

    // `spawn_blocking` (same convention as `login.rs`/`helper_download.rs`'s
    // blocking-work wrapping) rather than a plain `std::fs::read`: a build
    // artifact can be a large binary, and blocking a tokio worker thread on
    // it would stall whatever else is scheduled on that thread.
    let local_path_owned = local_path.to_string();
    let bytes = tokio::task::spawn_blocking(move || std::fs::read(&local_path_owned))
        .await
        .context("isekai-ssh: build result read task panicked")?
        .with_context(|| format!("isekai-ssh: failed to read build result {local_path:?}"))?;
    let file_name = std::path::Path::new(local_path)
        .file_name()
        .with_context(|| format!("isekai-ssh: result path {local_path:?} has no file name"))?
        .to_string_lossy()
        .into_owned();
    let remote_command = build_push_remote_command(dest_dir, &file_name);

    // `current_exe()` correctly self-references the real `isekai-ssh` binary
    // in production (that's what's running), but note for anyone testing
    // this: under `cargo test`, it resolves to the *test* binary, not
    // `isekai-ssh` — so exercising the actual recursive spawn needs an
    // integration test that drives the real compiled binary as the outer
    // process (`env!("CARGO_BIN_EXE_isekai-ssh")`), not a unit test in this
    // module. `build_push_remote_command` above is unit-tested directly
    // instead of going through a real spawn.
    let exe = std::env::current_exe().context("isekai-ssh: failed to resolve its own executable path")?;
    let mut child = tokio::process::Command::new(exe)
        .arg(host)
        .arg(&remote_command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("isekai-ssh: failed to spawn a recursive isekai-ssh invocation to push the build result")?;
    {
        let mut stdin = child.stdin.take().expect("stdin was piped");
        stdin
            .write_all(&bytes)
            .await
            .context("isekai-ssh: failed to write the build result to the recursive isekai-ssh's stdin")?;
        // `stdin` drops here (end of block), closing it so the remote `cat`
        // sees EOF and exits instead of blocking forever on more input.
    }
    let status = child
        .wait()
        .await
        .context("isekai-ssh: failed to wait for the recursive isekai-ssh result push")?;
    if !status.success() {
        anyhow::bail!("isekai-ssh: pushing build result {local_path:?} to {host:?} exited with {status}");
    }
    Ok(())
}

/// Where a build's `BuildOutputChunk`/`BuildFinished` bytes go, and how to
/// detect the remote/owner going away independent of a failed `send` — see
/// this module's doc comment for why one trait covers both concerns and why
/// a call site's `russh::Channel`-backed implementation needs that.
pub(crate) trait BuildRelay {
    /// Sends one already-encoded `CtlMessage` line. Returns whether it
    /// succeeded; once this returns `false`, [`run_build`] treats the
    /// remote/owner as gone and kills the still-running child.
    async fn send(&mut self, bytes: Vec<u8>) -> bool;

    /// Waits for an external signal that the remote/owner is gone,
    /// independent of a failed `send` — e.g. a forwarded `russh::Channel`
    /// reporting `Close`, or an explicit abort `oneshot`. [`run_build`] calls
    /// this once per `select!` loop iteration and treats *any* resolution as
    /// "gone," so implementations that see intermediate, non-disconnect
    /// events (e.g. `Channel::wait()` returning `Data`) must loop internally
    /// and only resolve once a real disconnect is confirmed. Implementations
    /// with no such external signal (the Unix `write_half` path, which only
    /// ever learns of a lost peer via a failed `send`) never resolve
    /// (`std::future::pending`).
    async fn watch(&mut self);

    /// Called once, right after the last successful `send` on a normal
    /// finish (never after a `watch`-detected disconnect — there is nowhere
    /// left to signal at that point). No-op default for sinks with no
    /// explicit close primitive (the mux-client mpsc sender).
    async fn close(&mut self) {}
}

/// How a [`run_build`] call ended. Deliberately not folded into `Result`'s
/// `Err` variant for the two non-error outcomes (`UnknownProfile`/
/// `SpawnFailed`/`Disconnected` are all ordinary, already-reported-to-the-peer
/// endings, not programming errors) — callers branch on this to decide
/// whether their own return type needs to `bail!`, log, or just return.
pub(crate) enum BuildOutcome {
    /// The build ran to completion (regardless of exit code); carries it.
    Finished(i32),
    /// No build profile matched `(host, profile_name)` — a `BuildFinished(127)`
    /// reply was already sent.
    UnknownProfile,
    /// The profile's command could not even be spawned — a
    /// `BuildFinished(126)` reply was already sent.
    SpawnFailed,
    /// `relay` reported the remote/owner is gone mid-build (a failed `send`
    /// or `watch` resolving); the child was killed. No `BuildFinished` was
    /// sent — there is nowhere left to send it.
    Disconnected,
}

/// The unified build-runner body (`ISEKAI_PIPE_DESIGN.md` §8 Epic P) shared
/// by all 3 dispatch paths — see this module's doc comment for the full
/// 9-step sequence and the drift this unification closes. `terminal` decides
/// which OSC 9;4 encoding [`crate::ctl_forward::emit_build_progress_start`]/
/// `emit_build_progress_result` use (`TerminalKind::resolve()` on the Unix
/// caller, always `TerminalKind::WindowsTerminal` on the two Windows-native
/// callers — same "resolve once, thread the value through" convention
/// `osc_sequence_for` itself documents).
pub(crate) async fn run_build(
    relay: &mut impl BuildRelay,
    host: &str,
    profile_name: &str,
    terminal: crate::ctl_forward::TerminalKind,
) -> Result<BuildOutcome> {
    let profile = crate::build_profile::default_build_profiles_path()
        .and_then(|path| crate::build_profile::load_build_profiles(&path))
        .ok()
        .and_then(|store| crate::build_profile::find_profile(&store, host, profile_name).cloned());

    let Some(profile) = profile else {
        let chunk = encode_build_output_chunk(
            BuildOutputStream::Stderr,
            format!("isekai-ssh: no build profile registered for {host:?}/{profile_name:?}\n").into_bytes(),
        )?;
        let _ = relay.send(chunk).await;
        let finished = encode_build_finished(127, Vec::new())?;
        let _ = relay.send(finished).await;
        relay.close().await;
        // No `emit_build_progress_start`/`_result` here: nothing was ever
        // shown yet for this build attempt (the indicator only starts once a
        // profile is confirmed to exist, just below), so there is nothing to
        // clear.
        return Ok(BuildOutcome::UnknownProfile);
    };

    // Epic P × 2026-08のOSC 9;4連携: ビルドの成否は事前に分からないので
    // `Indeterminate`(スピナー)で開始を知らせる。`emit_build_progress_result`と
    // 対で、doc commentはそちらを参照。
    crate::ctl_forward::emit_build_progress_start(terminal);

    let mut child = match spawn_shell_command(&profile.command, &profile.dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            crate::log_file::log_line!("isekai-ssh: failed to spawn build profile {host:?}/{profile_name:?}: {e}");
            // `Indeterminate` was already sent above — every exit past that
            // point must clear it, this one included, or the spinner is
            // left stuck on the user's tab/taskbar forever.
            crate::ctl_forward::emit_build_progress_result(false, terminal);
            let chunk = encode_build_output_chunk(
                BuildOutputStream::Stderr,
                format!("isekai-ssh: failed to spawn build profile {host:?}/{profile_name:?}: {e}\n").into_bytes(),
            )?;
            let _ = relay.send(chunk).await;
            let finished = encode_build_finished(126, Vec::new())?;
            let _ = relay.send(finished).await;
            relay.close().await;
            return Ok(BuildOutcome::SpawnFailed);
        }
    };
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let (tx, mut rx) = tokio::sync::mpsc::channel::<(BuildOutputStream, Vec<u8>)>(32);
    let stdout_task = tokio::spawn(pump_bytes(stdout, BuildOutputStream::Stdout, tx.clone()));
    let stderr_task = tokio::spawn(pump_bytes(stderr, BuildOutputStream::Stderr, tx.clone()));
    drop(tx);

    let mut disconnected = false;
    loop {
        tokio::select! {
            recv = rx.recv() => {
                match recv {
                    Some((stream, bytes)) => {
                        let chunk = encode_build_output_chunk(stream, bytes)?;
                        if !relay.send(chunk).await {
                            disconnected = true;
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = relay.watch() => {
                disconnected = true;
                break;
            }
        }
    }
    // Dropping `rx` (implicit at scope end below doesn't happen yet — done
    // explicitly here) unblocks any pump task still waiting on a full
    // channel: their next `tx.send(...)` sees the receiver gone and returns
    // immediately instead of hanging, so awaiting them below can't deadlock.
    drop(rx);

    if disconnected {
        // The remote/owner is gone; there is nothing left to report to. Kill
        // the child rather than let it keep running unattended (see this
        // module's docs on the fencing-slot-lesson parallel).
        let _ = child.start_kill();
        let _ = child.wait().await;
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        crate::ctl_forward::emit_build_progress_result(false, terminal);
        return Ok(BuildOutcome::Disconnected);
    }
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let status = match child.wait().await {
        Ok(status) => status,
        Err(e) => {
            // Same reasoning as the spawn-failure arm above: `Indeterminate`
            // is already showing and must be cleared on every exit path.
            crate::ctl_forward::emit_build_progress_result(false, terminal);
            return Err(e).context("isekai-ssh: failed to wait for the build child process");
        }
    };
    // `status.code()` is `None` only if the process was killed by a signal
    // (not the `disconnected` kill path above, which already returned) —
    // `-1` is not a valid process exit code on any platform, so it can't be
    // confused with a real one.
    let exit_code = status.code().unwrap_or(-1);
    crate::ctl_forward::emit_build_progress_result(exit_code == 0, terminal);

    let result_paths: Vec<String> = match (&profile.result_glob, &profile.dest_dir) {
        (Some(glob), Some(_dest_dir)) => glob_results(&profile.dir, glob)
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        _ => Vec::new(),
    };
    if let Some(dest_dir) = &profile.dest_dir {
        spawn_result_push(host.to_string(), dest_dir.clone(), result_paths.clone());
    }
    let finished = encode_build_finished(exit_code, result_paths)?;
    let _ = relay.send(finished).await;
    relay.close().await;
    Ok(BuildOutcome::Finished(exit_code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_shell_command_runs_in_the_given_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "hi").unwrap();
        let command = if cfg!(windows) { "dir marker.txt" } else { "ls marker.txt" };
        let status = spawn_shell_command(command, &dir.path().to_string_lossy())
            .status()
            .await
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn glob_results_matches_relative_to_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.exe"), b"binary").unwrap();
        std::fs::write(dir.path().join("app.pdb"), b"debug").unwrap();

        let matches = glob_results(&dir.path().to_string_lossy(), "*.exe").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file_name().unwrap(), "app.exe");
    }

    #[test]
    fn glob_results_returns_empty_for_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        let matches = glob_results(&dir.path().to_string_lossy(), "*.exe").unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn encode_build_output_chunk_round_trips_through_decode() {
        let encoded = encode_build_output_chunk(BuildOutputStream::Stdout, b"hello\n".to_vec()).unwrap();
        let line = &encoded[..encoded.len() - 1]; // strip the trailing '\n' this function appends
        let msg = isekai_protocol::decode_ctl_message(line).unwrap();
        match msg {
            CtlMessage::BuildOutputChunk { stream, data_b64 } => {
                assert_eq!(stream, BuildOutputStream::Stdout);
                let decoded = base64::engine::general_purpose::STANDARD.decode(&data_b64).unwrap();
                assert_eq!(decoded, b"hello\n");
            }
            other => panic!("expected BuildOutputChunk, got {other:?}"),
        }
    }

    #[test]
    fn encode_build_finished_round_trips_through_decode() {
        let encoded = encode_build_finished(5, vec!["out.bin".to_string()]).unwrap();
        let line = &encoded[..encoded.len() - 1];
        let msg = isekai_protocol::decode_ctl_message(line).unwrap();
        assert_eq!(msg, CtlMessage::BuildFinished { exit_code: 5, result_paths: vec!["out.bin".to_string()] });
    }

    /// The Windows-native abort sentinel (Phase 2, `native/mux/build_relay.rs`)
    /// is just an ordinary (if unusual) `i32` value — `BuildFinished`'s wire
    /// format has no range restriction on `exit_code`, so no protocol change
    /// was needed to introduce it. This locks in that assumption: a sentinel
    /// at `i32::MIN` must encode/decode exactly like any other exit code.
    #[test]
    fn encode_build_finished_accepts_the_i32_min_abort_sentinel() {
        let encoded = encode_build_finished(i32::MIN, Vec::new()).unwrap();
        let line = &encoded[..encoded.len() - 1];
        let msg = isekai_protocol::decode_ctl_message(line).unwrap();
        assert_eq!(msg, CtlMessage::BuildFinished { exit_code: i32::MIN, result_paths: Vec::new() });
    }

    #[tokio::test]
    async fn pump_bytes_forwards_chunks_until_eof() {
        let data = b"chunk-of-output".to_vec();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        pump_bytes(&data[..], BuildOutputStream::Stderr, tx).await;
        let (stream, bytes) = rx.recv().await.unwrap();
        assert_eq!(stream, BuildOutputStream::Stderr);
        assert_eq!(bytes, data);
        assert!(rx.recv().await.is_none(), "pump_bytes must stop at EOF");
    }

    #[tokio::test]
    async fn pump_bytes_stops_once_the_receiver_is_gone() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        // Must return promptly instead of hanging on a send with no receiver.
        tokio::time::timeout(std::time::Duration::from_secs(5), pump_bytes(&b"x"[..], BuildOutputStream::Stdout, tx))
            .await
            .expect("pump_bytes must not hang once its receiver is dropped");
    }

    #[test]
    fn build_push_remote_command_creates_the_dest_dir_and_writes_the_file() {
        let cmd = build_push_remote_command("~/isekai-build-results/win", "app.exe");
        assert_eq!(
            cmd,
            "mkdir -p ~/isekai-build-results/win && cat > ~/isekai-build-results/win/app.exe"
        );
    }

    #[test]
    fn build_push_remote_command_does_not_quote_tilde_out_of_expanding() {
        // A quoted `"~/dest"` would suppress the remote shell's tilde
        // expansion (POSIX shells never expand `~` inside quotes) — this
        // guards against that regression by asserting `~` stays bare.
        let cmd = build_push_remote_command("~/dest", "out.bin");
        assert!(!cmd.contains("\"~"), "tilde must not be quoted: {cmd:?}");
    }

    /// `spawn_result_push` with no result paths must not spawn anything —
    /// nothing to assert on the "did it spawn" side directly, but this at
    /// least guards against a panic/attempted push with an empty file list.
    #[tokio::test]
    async fn spawn_result_push_is_a_no_op_for_empty_result_paths() {
        spawn_result_push("mybox".to_string(), "~/dest".to_string(), Vec::new());
        // Give any (incorrectly) spawned task a chance to run before the
        // test process exits, so a regression would at least have a chance
        // to panic visibly rather than being silently dropped.
        tokio::task::yield_now().await;
    }
}
