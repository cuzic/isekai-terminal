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

use std::path::Path;
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
pub(crate) fn spawn_isekai_pipe_connect(
    isekai_pipe_path: &Path,
    runtime_dir: &Path,
    intent: &ConnectionIntent,
) -> Result<Child> {
    isekai_pipe_core::write_connection_intent(runtime_dir, intent)
        .with_context(|| format!("failed to write ConnectionIntent {} to {}", intent.intent_id, runtime_dir.display()))?;

    Command::new(isekai_pipe_path)
        .env("ISEKAI_INTENT_ID", &intent.intent_id)
        .env("ISEKAI_PIPE_RUNTIME_DIR", runtime_dir)
        .arg("connect")
        .arg("--profile")
        .arg(&intent.profile)
        .arg("--service")
        .arg(&intent.service)
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to spawn {}", isekai_pipe_path.display()))
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
    /// `isekai-ssh` has no `lib.rs`, so this can't be moved to
    /// `tests/` either — integration tests can't reach `pub(crate)` items
    /// of a binary-only crate). Deriving it from `current_exe()` (the
    /// running test binary's own path,
    /// `target/<profile>/deps/isekai_ssh-<hash>`) is the standard
    /// workaround: strip the trailing `deps` component to reach
    /// `target/<profile>/`, where `cargo build`/`cargo test` always also
    /// places every `[[bin]]`/`src/bin/*.rs` target under its own name.
    fn echo_test_shim_path() -> std::path::PathBuf {
        let mut path = std::env::current_exe().expect("current_exe() should succeed under `cargo test`");
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        path.push(if cfg!(windows) { "echo_test_shim.exe" } else { "echo_test_shim" });
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
    /// `runtime_dir`, `ISEKAI_INTENT_ID`/`ISEKAI_PIPE_RUNTIME_DIR` are set
    /// and inherited by the child (verified via the child's own
    /// `ECHO_TEST_SHIM_ANNOUNCE_ENV` preamble — the exact bug the Codex
    /// review on the first version of this file caught: without these env
    /// vars, `isekai-pipe connect` silently falls back to a different,
    /// non-`ConnectOutcome`-recording code path), and bytes round-trip
    /// through `ChildStdio` afterward.
    #[tokio::test]
    async fn spawn_writes_intent_sets_env_vars_and_round_trips_bytes() {
        let runtime_dir = tempfile::tempdir().unwrap();
        let intent = sample_intent("example-profile", "ssh");

        let mut child = spawn_isekai_pipe_connect(&echo_test_shim_path(), runtime_dir.path(), &intent)
            .expect("spawn_isekai_pipe_connect should succeed");

        let intent_path = runtime_dir.path().join("intents").join(format!("{}.json", intent.intent_id));
        let written: ConnectionIntent =
            serde_json::from_str(&std::fs::read_to_string(&intent_path).unwrap()).unwrap();
        assert_eq!(written, intent, "the exact intent passed in must be what's on disk");

        let mut stdio = ChildStdio::take_from(&mut child).expect("both stdin and stdout were piped");

        let mut announced = [0u8; 256];
        let n = stdio.read(&mut announced).await.unwrap();
        let announced = String::from_utf8_lossy(&announced[..n]);
        assert!(
            announced.contains(&format!("ISEKAI_INTENT_ID={}", intent.intent_id)),
            "child must see the same intent_id via env: {announced:?}"
        );
        assert!(
            announced.contains(&format!("ISEKAI_PIPE_RUNTIME_DIR={}", runtime_dir.path().display())),
            "child must see the runtime_dir via env: {announced:?}"
        );

        stdio.write_all(b"hello from ChildStdio\n").await.unwrap();
        stdio.flush().await.unwrap();
        let mut buf = [0u8; 64];
        let n = stdio.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello from ChildStdio\n");

        drop(stdio);
        let _ = child.wait().await;
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
