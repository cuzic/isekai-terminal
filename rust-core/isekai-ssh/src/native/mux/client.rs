//! Client side of the mux: the process whose [`local_ipc_mux::ExclusiveChannel::try_claim`]
//! lost (an owner already exists), so instead of dialing SSH itself it relays
//! its local terminal to the owner over one IPC connection and receives its
//! own private remote shell's output back.
//!
//! **Re-election model** (decided in the M4 plan, not re-litigated here): if
//! the owner dies, this client's remote shell — multiplexed on the now-dead
//! owner's connection — is gone too, so there is nothing for *this* process to
//! recover. It does not try to promote itself. It prints a clear message and
//! exits with the dedicated [`crate::EXIT_MUX_OWNER_LOST`] code; the user
//! reconnecting with a fresh `isekai-ssh <host>` simply goes through the
//! ordinary `try_claim` path and becomes the new owner (the old owner's claim
//! having been released) — there is no special recovery code path.

use anyhow::{anyhow, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::log_file::log_line;

use super::protocol::{read_frame, write_frame, Frame, MUX_PROTOCOL_VERSION};

/// How a client session ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClientOutcome {
    /// The remote shell exited (cleanly relayed `Exit` frame) with this code.
    Exited(u8),
    /// The owner connection dropped without a clean `Exit` — the owner process
    /// died. Maps to [`crate::EXIT_MUX_OWNER_LOST`] at the call site.
    OwnerLost,
}

/// Drives a client session against `conn` (an established owner connection),
/// wiring the real local terminal (raw mode + current size) as the I/O. On a
/// lost owner it prints the reconnect guidance and returns
/// [`crate::EXIT_MUX_OWNER_LOST`]; otherwise it returns the remote shell's
/// exit code.
pub(crate) async fn run<Conn>(conn: Conn, token: &[u8]) -> Result<u8>
where
    Conn: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (mut reader, mut writer) = tokio::io::split(conn);
    let (cols, rows) = super::super::console::terminal_size();
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());

    let _raw_mode = super::super::console::RawModeGuard::enable().map_err(|e| anyhow!("isekai-ssh: failed to enable raw terminal mode: {e}"))?;
    let outcome = run_inner(
        &mut reader,
        &mut writer,
        token,
        term,
        cols as u16,
        rows as u16,
        tokio::io::stdin(),
        tokio::io::stdout(),
        tokio::io::stderr(),
    )
    .await?;

    match outcome {
        ClientOutcome::Exited(code) => Ok(code),
        ClientOutcome::OwnerLost => {
            log_line!(
                "isekai-ssh: connection to the isekai-ssh owner process was lost — \
                 reconnect with `isekai-ssh <host>`."
            );
            Ok(crate::EXIT_MUX_OWNER_LOST)
        }
    }
}

/// The body of [`run`] with the terminal streams injected, so tests can drive
/// it against in-memory buffers. Sends a [`Frame::Hello`], waits for the
/// owner's `HelloAck`/`Rejected`, then relays local stdin as [`Frame::Stdin`]
/// frames (and a final [`Frame::Shutdown`] on local EOF) while writing the
/// owner's `Stdout`/`Stderr` frames to the local streams until an [`Frame::Exit`]
/// (clean end) or a dropped connection (owner lost).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_inner<CR, CW, I, O, E>(
    conn_read: &mut CR,
    conn_write: &mut CW,
    token: &[u8],
    term: String,
    cols: u16,
    rows: u16,
    mut stdin: I,
    mut stdout: O,
    mut stderr: E,
) -> Result<ClientOutcome>
where
    CR: AsyncRead + Unpin,
    CW: AsyncWrite + Unpin,
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    write_frame(conn_write, &Frame::Hello { version: MUX_PROTOCOL_VERSION, token: token.to_vec(), term, cols, rows })
        .await
        .map_err(|e| anyhow!("isekai-ssh: failed to send Hello to the owner: {e}"))?;

    match read_frame(conn_read).await {
        Ok(Some(Frame::HelloAck { version })) => {
            if version != MUX_PROTOCOL_VERSION {
                return Err(anyhow!("isekai-ssh: owner acknowledged with protocol version {version}, expected {MUX_PROTOCOL_VERSION}"));
            }
        }
        Ok(Some(Frame::Rejected { reason })) => {
            return Err(anyhow!("isekai-ssh: the owner process rejected this connection: {reason}"));
        }
        Ok(Some(other)) => return Err(anyhow!("isekai-ssh: expected HelloAck from the owner, got {other:?}")),
        // Owner vanished during the handshake — treat as owner-lost so the
        // user gets the reconnect guidance rather than an opaque error.
        Ok(None) | Err(_) => return Ok(ClientOutcome::OwnerLost),
    }

    let mut buf = [0u8; 8192];
    let mut stdin_open = true;

    loop {
        tokio::select! {
            n = stdin.read(&mut buf), if stdin_open => {
                match n {
                    Ok(0) | Err(_) => {
                        // Local stdin EOF: tell the owner to send channel EOF,
                        // but keep receiving any in-flight remote output.
                        stdin_open = false;
                        let _ = write_frame(conn_write, &Frame::Shutdown).await;
                    }
                    Ok(n) => {
                        if write_frame(conn_write, &Frame::Stdin(buf[..n].to_vec())).await.is_err() {
                            return Ok(ClientOutcome::OwnerLost);
                        }
                    }
                }
            }
            frame = read_frame(conn_read) => {
                match frame {
                    Ok(Some(Frame::Stdout(data))) => {
                        let _ = stdout.write_all(&data).await;
                        let _ = stdout.flush().await;
                    }
                    Ok(Some(Frame::Stderr(data))) => {
                        let _ = stderr.write_all(&data).await;
                        let _ = stderr.flush().await;
                    }
                    Ok(Some(Frame::Exit(code))) => return Ok(ClientOutcome::Exited(code)),
                    Ok(Some(other)) => return Err(anyhow!("isekai-ssh: unexpected frame from the owner: {other:?}")),
                    // A clean close without an Exit, or any read error (a reset
                    // pipe), both mean the owner died mid-session.
                    Ok(None) | Err(_) => return Ok(ClientOutcome::OwnerLost),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    /// Runs `run_inner` against an in-memory owner connection whose behavior a
    /// closure supplies, plus a canned stdin. Returns the client's outcome and
    /// the bytes it wrote to its local stdout/stderr.
    async fn drive_client(
        stdin_bytes: &'static [u8],
        owner: impl FnOnce(tokio::io::DuplexStream) -> tokio::task::JoinHandle<()> + Send,
    ) -> (Result<ClientOutcome>, Vec<u8>, Vec<u8>) {
        let (client_conn, owner_conn) = duplex(64 * 1024);
        let _owner_task = owner(owner_conn);
        let (mut cr, mut cw) = tokio::io::split(client_conn);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let outcome = run_inner(
            &mut cr,
            &mut cw,
            b"tok",
            "xterm".to_string(),
            80,
            24,
            stdin_bytes,
            &mut stdout,
            &mut stderr,
        )
        .await;
        (outcome, stdout, stderr)
    }

    /// Happy path: owner acks, sends stdout + stderr, then a clean Exit — the
    /// client returns `Exited(code)` and routes each stream correctly.
    #[tokio::test]
    async fn client_relays_streams_and_returns_the_exit_code() {
        let (outcome, stdout, stderr) = drive_client(b"", |owner_conn| {
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(owner_conn);
                // Expect Hello first.
                match read_frame(&mut r).await.unwrap().unwrap() {
                    Frame::Hello { version, .. } => assert_eq!(version, MUX_PROTOCOL_VERSION),
                    other => panic!("expected Hello, got {other:?}"),
                }
                write_frame(&mut w, &Frame::HelloAck { version: MUX_PROTOCOL_VERSION }).await.unwrap();
                write_frame(&mut w, &Frame::Stdout(b"out".to_vec())).await.unwrap();
                write_frame(&mut w, &Frame::Stderr(b"err".to_vec())).await.unwrap();
                write_frame(&mut w, &Frame::Exit(7)).await.unwrap();
            })
        })
        .await;

        assert_eq!(outcome.unwrap(), ClientOutcome::Exited(7));
        assert_eq!(stdout, b"out", "Stdout frames must land on local stdout");
        assert_eq!(stderr, b"err", "Stderr frames must land on local stderr, not stdout");
    }

    /// The owner drops the connection without ever sending an `Exit` — the
    /// client reports `OwnerLost` (which the caller maps to the dedicated
    /// re-election exit code), not a spurious success.
    #[tokio::test]
    async fn client_reports_owner_lost_on_an_abrupt_drop() {
        let (outcome, _out, _err) = drive_client(b"", |owner_conn| {
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(owner_conn);
                let _ = read_frame(&mut r).await;
                write_frame(&mut w, &Frame::HelloAck { version: MUX_PROTOCOL_VERSION }).await.unwrap();
                // Drop without an Exit frame — simulates the owner dying.
            })
        })
        .await;

        assert_eq!(outcome.unwrap(), ClientOutcome::OwnerLost, "an owner that drops without Exit must be OwnerLost");
    }

    /// The owner refuses the Hello (e.g. version/token mismatch) — surfaced as
    /// an error carrying the owner's reason, not OwnerLost.
    #[tokio::test]
    async fn client_surfaces_a_rejection_reason() {
        let (outcome, _out, _err) = drive_client(b"", |owner_conn| {
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(owner_conn);
                let _ = read_frame(&mut r).await;
                write_frame(&mut w, &Frame::Rejected { reason: "authentication token mismatch".to_string() }).await.unwrap();
            })
        })
        .await;

        let err = outcome.unwrap_err();
        assert!(format!("{err:#}").contains("token mismatch"), "the owner's reject reason must reach the user, got {err:#}");
    }

    /// Local stdin is relayed to the owner as `Stdin` frames, and local EOF
    /// produces a `Shutdown` frame.
    #[tokio::test]
    async fn client_forwards_stdin_then_shutdown_on_eof() {
        let (client_conn, owner_conn) = duplex(64 * 1024);
        let (mut or, mut ow) = tokio::io::split(owner_conn);

        let owner = tokio::spawn(async move {
            // Hello, then ack.
            match read_frame(&mut or).await.unwrap().unwrap() {
                Frame::Hello { .. } => {}
                other => panic!("expected Hello, got {other:?}"),
            }
            write_frame(&mut ow, &Frame::HelloAck { version: MUX_PROTOCOL_VERSION }).await.unwrap();
            // Expect the stdin bytes, then a Shutdown, then send Exit so the
            // client loop terminates.
            let mut got_stdin = Vec::new();
            loop {
                match read_frame(&mut or).await.unwrap() {
                    Some(Frame::Stdin(d)) => got_stdin.extend_from_slice(&d),
                    Some(Frame::Shutdown) => break,
                    Some(other) => panic!("unexpected client frame {other:?}"),
                    None => panic!("client closed before Shutdown"),
                }
            }
            write_frame(&mut ow, &Frame::Exit(0)).await.unwrap();
            got_stdin
        });

        let (mut cr, mut cw) = tokio::io::split(client_conn);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let outcome = run_inner(&mut cr, &mut cw, b"tok", "xterm".to_string(), 80, 24, &b"echo hi\n"[..], &mut stdout, &mut stderr)
            .await
            .unwrap();

        assert_eq!(outcome, ClientOutcome::Exited(0));
        let got_stdin = owner.await.unwrap();
        assert_eq!(got_stdin, b"echo hi\n", "local stdin must be relayed to the owner verbatim");
    }
}
