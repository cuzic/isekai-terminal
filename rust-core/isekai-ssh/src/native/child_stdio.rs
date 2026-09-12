//! Adapts a child process's stdin/stdout into a single
//! `AsyncRead + AsyncWrite`, so a spawned `isekai-pipe connect --stdio`
//! child (exactly the same binary/arguments the Unix `ssh(1)` ProxyCommand
//! path already spawns, `wrapper.rs::proxy_command`) can be handed straight
//! to `russh_stream_session::establish_over_stream` as if it were a raw
//! socket.
//!
//! This is deliberately the *only* new piece of connect-path code for the
//! native route (see the plan's M1 note on why route-selection/resume logic
//! itself is not being refactored): `isekai-pipe connect`'s own route
//! selection, resume-on-disconnect, and `ConnectOutcome` bookkeeping are
//! completely unchanged — the native path just runs the same binary as a
//! child process instead of leaving that job to a real `ssh(1)`.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result};
use isekai_pipe_core::ConnectionIntent;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Spawns `isekai_pipe_path connect --profile <profile from intent> --service
/// <service from intent> --stdio` with stdin/stdout piped (so
/// [`ChildStdio::take_from`] can adapt them) and stderr inherited (so the
/// child's own diagnostic logging is still visible to the user, same as
/// today's `ssh(1)` ProxyCommand case).
///
/// **Must** write `intent` to `runtime_dir` first and set
/// `ISEKAI_INTENT_ID`/`ISEKAI_PIPE_RUNTIME_DIR` on the child — mirroring
/// `wrapper.rs::run_ssh_once`'s `write_connection_intent(...)` +
/// `.env("ISEKAI_INTENT_ID", ...).env("ISEKAI_PIPE_RUNTIME_DIR", ...)`
/// exactly. Without these, `isekai-pipe connect` (`connect.rs`'s
/// `resolve_connection_intent`) falls back to resolving the profile from
/// scratch instead of claiming this specific intent, which silently skips
/// `ConnectOutcome` bookkeeping (`always-connects.md`) — the entire reason
/// this native path spawns the real `isekai-pipe connect` binary instead of
/// reimplementing its route/resume logic (Codex review finding, see the
/// plan's M1 notes).
///
/// `kill_on_drop` is set so dropping the returned `Child` on an early error
/// doesn't leak the subprocess — but note this cuts both ways: the caller
/// must keep the returned `Child` alive for as long as the SSH session is in
/// use. Dropping it early (even after taking its stdio via
/// [`ChildStdio::take_from`]) kills a perfectly healthy long-running
/// connection, not just an errored one.
///
/// `log_file_override` is `plan.log_file()` (`--isekai-log-file`, if given)
/// — see [`resolve_pipe_log_file`] for why this path always sets
/// `ISEKAI_PIPE_LOG_FILE` (unlike the Unix ProxyCommand path, which only
/// does so in the no-flag default case). `channel_name` is
/// `naming::channel_name(..)` for this same destination — the same identity
/// `mux/mod.rs` uses to decide which holder this invocation shares with —
/// reused (see [`resolve_pipe_log_file`]) so the holder-only log path is
/// unique per holder rather than a single name shared by every
/// concurrently-active destination (code review finding: two `isekai-ssh
/// <host>` tabs to different destinations, each with their own detached
/// holder, would otherwise both point their `isekai-pipe connect` children's
/// `RotatingLogFile` at the exact same file and race each other's rotations).
pub(crate) fn spawn_isekai_pipe_connect(
    isekai_pipe_path: &Path,
    runtime_dir: &Path,
    intent: &ConnectionIntent,
    log_file_override: Option<&Path>,
    channel_name: &str,
) -> Result<Child> {
    isekai_pipe_core::write_connection_intent(runtime_dir, intent)
        .with_context(|| format!("failed to write ConnectionIntent {} to {}", intent.intent_id, runtime_dir.display()))?;

    let mut command = Command::new(isekai_pipe_path);
    command
        .env("ISEKAI_INTENT_ID", &intent.intent_id)
        .env("ISEKAI_PIPE_RUNTIME_DIR", runtime_dir);
    if let Some(log_file) =
        resolve_pipe_log_file(log_file_override, crate::native::mux::holder::is_holder_reexec(), channel_name)
    {
        command.env("ISEKAI_PIPE_LOG_FILE", log_file);
    }
    command
        .arg("connect")
        .arg("--profile")
        .arg(&intent.profile)
        .arg("--service")
        .arg(&intent.service)
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    // Without this, a console-subsystem child spawned from a process that
    // itself has no console (the detached ControlPersist-equivalent mux
    // holder, `native/mux/holder.rs` — `DETACHED_PROCESS`-spawned, so it has
    // nothing for this child to inherit) gets a brand-new *visible* console
    // window auto-allocated by Windows, since neither `Stdio::piped()`
    // (stdin/stdout) nor `Stdio::inherit()` (stderr, an explicitly duplicated
    // handle) suppresses that on their own — only an explicit creation flag
    // does. Harmless for the ordinary foreground case (parent already has a
    // console): the inherited stderr handle stays a valid, writable duplicate
    // of the parent's console screen buffer regardless of this flag, so the
    // child's diagnostic logging is still visible there.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    command.spawn().with_context(|| format!("failed to spawn {}", isekai_pipe_path.display()))
}

/// Priority order for the `isekai-pipe connect` child's `ISEKAI_PIPE_LOG_FILE`
/// target (`ADR_ISEKAI_SSH_OBSERVABILITY.md` §3.1): (1) `--isekai-log-file`
/// explicit override, if given; (2) the holder-specific rotating log
/// ([`holder_log_file`]), if `is_holder` (the caller passes
/// `native::mux::holder::is_holder_reexec()` — kept as a plain `bool`
/// parameter here, rather than calling that directly, purely so this
/// priority logic stays unit-testable without mutating the real
/// process-wide `ISEKAI_SSH_MUX_HOLDER` env var `is_holder_reexec` reads,
/// which would race against other tests); (3) the ordinary
/// `isekai_pipe_core::default_log_file()` otherwise. Unlike the Unix
/// ProxyCommand path (`wrapper.rs::run_ssh_once`, which only sets this env
/// var in the no-flag default case and otherwise relies on piping the
/// child's stderr for aggregation), this path must set it in *every* case:
/// the native path never pipes the child's stderr, so an unset env var here
/// means the child's diagnostics vanish into whatever `Stdio::inherit()`
/// resolves to — nothing at all once the holder's null-stderr parent
/// (`native/mux/holder.rs`) is in the picture, which was the actual bug this
/// ADR investigates.
fn resolve_pipe_log_file(explicit_override: Option<&Path>, is_holder: bool, channel_name: &str) -> Option<PathBuf> {
    if let Some(explicit) = explicit_override {
        return Some(explicit.to_path_buf());
    }
    if is_holder {
        return holder_log_file(channel_name).ok();
    }
    isekai_pipe_core::default_log_file().ok()
}

/// `isekai-ssh-holder-<hex>.log`, alongside `default_log_file()`'s own
/// `isekai-ssh.log` — a separate file *per holder* (`<hex>` is
/// `channel_name`'s own trailing SHA-256 hex digest — the exact identity
/// `mux/mod.rs` uses to decide which holder an invocation shares with, so
/// this stays 1:1 with "one holder" the same way `channel_name` itself
/// does) so the long-lived holder's child (which needs runtime log
/// rotation, `isekai-pipe/src/connect.rs`'s `RotatingLogFile`) never shares
/// a path with *either* a short-lived foreground client's child (which
/// keeps appending to `default_log_file()` forever, same as before this
/// ADR) *or* a different destination's own holder — two concurrently-active
/// `isekai-ssh <host>` destinations (an ordinary multi-tab usage pattern)
/// each get their own detached holder (`native/mux/holder.rs`), and without
/// this per-holder split their `isekai-pipe connect` children's
/// `RotatingLogFile` instances would collide on one shared file, each
/// independently tracking size and rotating out from under the other's open
/// handle (a real bug an earlier code review caught in this ADR's first
/// draft, which used a single global `isekai-ssh-holder.log` name). Env
/// inheritance means `default_log_file()` itself resolves identically
/// regardless of holder (`holder.rs` doesn't touch the env block), so this
/// is purely a naming split, not a different resolution mechanism.
fn holder_log_file(channel_name: &str) -> std::io::Result<PathBuf> {
    let mut path = isekai_pipe_core::default_log_file()?;
    // `channel_name` is `\\.\pipe\isekai-ssh-mux-<hex>` — hyphens appear
    // only inside the literal `isekai-ssh-mux-` prefix, never inside the
    // hex digest itself, so splitting on the *last* `-` reliably isolates
    // just the digest regardless of how many hyphens precede it.
    let digest = channel_name.rsplit('-').next().unwrap_or(channel_name);
    path.set_file_name(format!("isekai-ssh-holder-{digest}.log"));
    Ok(path)
}

/// A child process's piped stdin+stdout, combined into one
/// `AsyncRead + AsyncWrite` value.
pub(crate) struct ChildStdio {
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl ChildStdio {
    /// Takes ownership of `child`'s stdin/stdout. Returns `None` if either
    /// is missing — meaning `child` wasn't spawned with both piped (a
    /// caller bug, not a runtime failure), since [`spawn_isekai_pipe_connect`]
    /// always pipes both.
    pub(crate) fn take_from(child: &mut Child) -> Option<Self> {
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        Some(Self { stdin, stdout })
    }
}

impl AsyncRead for ChildStdio {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for ChildStdio {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().stdin).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stdin).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stdin).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isekai_pipe_core::{BootstrapProvenance, IntentTransport, ServerIdentity};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Path to the compiled `echo_test_shim` binary (module docs) — a
    /// genuine `.exe`, not `cat`(1)/a shell script, so these tests behave
    /// the same on Windows CI (`ssh_test_shim.rs`'s established precedent
    /// for exactly this reason).
    ///
    /// `env!("CARGO_BIN_EXE_echo_test_shim")` isn't available here — Cargo
    /// only populates `CARGO_BIN_EXE_*` for integration test/bench targets,
    /// not for a binary crate's own internal unit test harness (and
    /// `isekai-ssh` has no `lib.rs`, so this can't be moved to `tests/`
    /// either — integration tests can't reach `pub(crate)` items of a
    /// binary-only crate). Deriving the path from `current_exe()` (the
    /// running test binary's own path, `target/<profile>/deps/isekai_ssh-
    /// <hash>` → strip `deps` to reach `target/<profile>/`, where every
    /// `[[bin]]`/`src/bin/*.rs` target also lands) is the standard
    /// workaround for locating a sibling binary — but that only tells you
    /// *where it would be*, not that it's actually been built: running the
    /// narrower `cargo test -p isekai-ssh --bin isekai-ssh` (as opposed to
    /// the whole-package `cargo test -p isekai-ssh`) does **not** build
    /// `echo_test_shim` as a side effect (confirmed empirically — this bug
    /// shipped once already and was only caught by Codex review, not by
    /// running the test itself under that narrower invocation). So this
    /// always builds it first, making the test self-sufficient regardless
    /// of which `cargo test` invocation runs it — `cargo build` is a fast
    /// no-op on repeat calls once it's already up to date.
    fn echo_test_shim_path() -> std::path::PathBuf {
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--bin", "echo_test_shim", "-p", "isekai-ssh"])
            .status()
            .expect("invoking `cargo build --bin echo_test_shim` should succeed");
        assert!(status.success(), "building the echo_test_shim test fixture failed");

        let mut path = std::env::current_exe().expect("current_exe() should succeed under `cargo test`");
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        path.push(if cfg!(windows) { "echo_test_shim.exe" } else { "echo_test_shim" });
        assert!(path.exists(), "expected {} to exist after building it", path.display());
        path
    }

    fn sample_intent(profile: &str, service: &str) -> ConnectionIntent {
        ConnectionIntent::new(
            profile,
            service,
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::Relay {
                helper_addr: "203.0.113.5:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "example.com:22".to_string() },
        )
    }

    /// Exercises the actual `spawn_isekai_pipe_connect` contract end to
    /// end (not just `ChildStdio` in isolation): the intent gets written to
    /// `runtime_dir`, `ISEKAI_INTENT_ID`/`ISEKAI_PIPE_RUNTIME_DIR`/
    /// `ISEKAI_PIPE_LOG_FILE` are set and inherited by the child (verified
    /// via the child's own env-var preamble — the exact bug an earlier
    /// Codex review caught for the first two, and the exact bug
    /// `ADR_ISEKAI_SSH_OBSERVABILITY.md` §1.3 found for the third: without
    /// it, `isekai-pipe connect`'s diagnostic logging vanishes into the
    /// holder's null stderr), and bytes round-trip through `ChildStdio`
    /// afterward.
    #[tokio::test]
    async fn spawn_writes_intent_sets_env_vars_and_round_trips_bytes() {
        let runtime_dir = tempfile::tempdir().unwrap();
        let intent = sample_intent("example-profile", "ssh");
        let explicit_log_file = runtime_dir.path().join("explicit.log");

        let mut child = spawn_isekai_pipe_connect(
            &echo_test_shim_path(),
            runtime_dir.path(),
            &intent,
            Some(&explicit_log_file),
            r"\\.\pipe\isekai-ssh-mux-test",
        )
        .expect("spawn_isekai_pipe_connect should succeed");

        let intent_path = runtime_dir.path().join("intents").join(format!("{}.json", intent.intent_id));
        let written: ConnectionIntent =
            serde_json::from_str(&std::fs::read_to_string(&intent_path).unwrap()).unwrap();
        assert_eq!(written, intent, "the exact intent passed in must be what's on disk");

        let mut stdio = ChildStdio::take_from(&mut child).expect("both stdin and stdout were piped");

        // `AsyncRead` is explicitly allowed to return a partial read (this
        // is a pipe, not a fixed-size in-memory buffer) — the 3-line env-var
        // preamble can arrive split across more than one `read()` call, so
        // accumulate until all expected lines have shown up rather than
        // assuming one `read()` sees everything (a real flake Codex review
        // caught: the original version of this test only read once).
        let expected_intent_line = format!("ISEKAI_INTENT_ID={}\n", intent.intent_id);
        let expected_runtime_dir_line = format!("ISEKAI_PIPE_RUNTIME_DIR={}\n", runtime_dir.path().display());
        let expected_log_file_line = format!("ISEKAI_PIPE_LOG_FILE={}\n", explicit_log_file.display());
        let mut announced = Vec::new();
        let mut chunk = [0u8; 256];
        loop {
            let n = tokio::time::timeout(std::time::Duration::from_secs(10), stdio.read(&mut chunk))
                .await
                .expect("reading the env-var preamble should not hang")
                .unwrap();
            assert!(n > 0, "child closed its stdout before announcing all env vars: {announced:?}");
            announced.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&announced);
            if text.contains(&expected_intent_line)
                && text.contains(&expected_runtime_dir_line)
                && text.contains(&expected_log_file_line)
            {
                break;
            }
        }

        stdio.write_all(b"hello from ChildStdio\n").await.unwrap();
        stdio.flush().await.unwrap();
        let mut buf = [0u8; 64];
        let n = stdio.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello from ChildStdio\n");

        drop(stdio);
        let _ = child.wait().await;
    }

    const TEST_CHANNEL_NAME: &str = r"\\.\pipe\isekai-ssh-mux-abcdef0123456789";

    #[test]
    fn resolve_pipe_log_file_prioritizes_explicit_override_over_holder_status() {
        let explicit = Path::new("/tmp/explicit.log");
        assert_eq!(resolve_pipe_log_file(Some(explicit), true, TEST_CHANNEL_NAME), Some(explicit.to_path_buf()));
        assert_eq!(resolve_pipe_log_file(Some(explicit), false, TEST_CHANNEL_NAME), Some(explicit.to_path_buf()));
    }

    #[test]
    fn resolve_pipe_log_file_splits_holder_and_foreground_paths_when_no_override() {
        let holder = resolve_pipe_log_file(None, true, TEST_CHANNEL_NAME)
            .expect("holder_log_file should resolve on a test host with $HOME/%LOCALAPPDATA%");
        let foreground = resolve_pipe_log_file(None, false, TEST_CHANNEL_NAME)
            .expect("default_log_file should resolve on a test host with $HOME/%LOCALAPPDATA%");
        assert_ne!(holder, foreground, "holder and foreground must never share a log path (ADR §3.2)");
        assert_eq!(holder.file_name().unwrap(), "isekai-ssh-holder-abcdef0123456789.log");
        assert_eq!(holder.parent(), foreground.parent(), "only the file name should differ, not the directory");
    }

    #[test]
    fn resolve_pipe_log_file_gives_different_channels_different_holder_paths() {
        // Regression test for a code-review finding on this ADR's first
        // draft: two concurrently-active destinations (two `isekai-ssh
        // <host>` tabs, each with their own detached holder) must never
        // collide on the same holder log path.
        let first = resolve_pipe_log_file(None, true, r"\\.\pipe\isekai-ssh-mux-aaaa").unwrap();
        let second = resolve_pipe_log_file(None, true, r"\\.\pipe\isekai-ssh-mux-bbbb").unwrap();
        assert_ne!(first, second, "two different holders (different channel_name) must get different log paths");
    }

    #[tokio::test]
    async fn take_from_returns_none_after_stdio_already_taken() {
        let mut child = Command::new(echo_test_shim_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        assert!(ChildStdio::take_from(&mut child).is_some());
        assert!(ChildStdio::take_from(&mut child).is_none(), "stdin/stdout were already taken");
    }
}
