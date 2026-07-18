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

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Spawns `isekai_pipe_path connect --profile <profile> --service <service>
/// --stdio` with stdin/stdout piped (so [`ChildStdio::take_from`] can adapt
/// them) and stderr inherited (so the child's own diagnostic logging is
/// still visible to the user, same as today's `ssh(1)` ProxyCommand case).
/// `kill_on_drop` is set so dropping the returned `Child` (e.g. on an early
/// error) doesn't leak the subprocess.
pub(crate) fn spawn_isekai_pipe_connect(
    isekai_pipe_path: &Path,
    profile: &str,
    service: &str,
) -> std::io::Result<Child> {
    Command::new(isekai_pipe_path)
        .arg("connect")
        .arg("--profile")
        .arg(profile)
        .arg("--service")
        .arg(service)
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Uses `cat`(1) — not the real `isekai-pipe` binary — as a stand-in
    /// child process that just echoes stdin back to stdout, purely to
    /// exercise `ChildStdio`'s bidirectional byte plumbing in isolation
    /// from isekai-pipe's own connect logic (which has its own e2e tests
    /// elsewhere).
    #[tokio::test]
    async fn round_trips_bytes_through_a_real_child_process() {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("spawning `cat` should succeed in this test environment");

        let mut stdio = ChildStdio::take_from(&mut child).expect("both stdin and stdout were piped");

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
        let mut child = Command::new("cat")
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
