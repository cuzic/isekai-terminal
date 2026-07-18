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

use super::protocol::{spawn_frame_reader, write_frame, Frame, MUX_PROTOCOL_VERSION};

/// How a client session ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClientOutcome {
    /// The remote shell exited (cleanly relayed `Exit` frame) with this code.
    Exited(u8),
    /// The owner connection dropped without a clean `Exit` — the owner process
    /// died. Maps to [`crate::EXIT_MUX_OWNER_LOST`] at the call site.
    OwnerLost,
    /// The owner rejected the connection during the initial handshake
    /// (protocol version mismatch, a stale auth token from an owner-turnover
    /// race, or an unexpected first frame) — no shell session was ever
    /// established. Unlike `OwnerLost`, there is nothing in flight to lose,
    /// so the caller can always safely fall back to an unmultiplexed direct
    /// connect instead of treating this as fatal (`always-connects.md`: a mux
    /// hiccup must never block connecting).
    Rejected { reason: String },
}

/// What [`run`] did with an established owner connection.
pub(crate) enum ClientRunResult {
    /// The session ran to some conclusion; this is this process's own exit
    /// code (either the remote shell's real exit code, or
    /// [`crate::EXIT_MUX_OWNER_LOST`] if the owner was lost mid-session).
    ExitCode(u8),
    /// The owner rejected the connection before any shell session started
    /// (see [`ClientOutcome::Rejected`]) — the caller should fall back to an
    /// unmultiplexed direct connect rather than treat this as fatal.
    Rejected { reason: String },
}

/// Drives a client session against `conn` (an established owner connection),
/// wiring the real local terminal (raw mode + current size) as the I/O. On a
/// lost owner (after a shell session was already established) it prints the
/// reconnect guidance and returns [`crate::EXIT_MUX_OWNER_LOST`] as an exit
/// code — there is nothing left to fall back to at that point. On a rejection
/// during the handshake (before any shell existed), it instead returns
/// [`ClientRunResult::Rejected`] so the caller can retry unmultiplexed.
pub(crate) async fn run<Conn>(conn: Conn, token: &[u8]) -> Result<ClientRunResult>
where
    Conn: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(conn);
    let (cols, rows) = super::super::console::terminal_size();
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());

    let _raw_mode = super::super::console::RawModeGuard::enable().map_err(|e| anyhow!("isekai-ssh: failed to enable raw terminal mode: {e}"))?;
    let outcome = run_inner(
        reader,
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
        ClientOutcome::Exited(code) => Ok(ClientRunResult::ExitCode(code)),
        ClientOutcome::OwnerLost => {
            log_line!(
                "isekai-ssh: connection to the isekai-ssh owner process was lost — \
                 reconnect with `isekai-ssh <host>`."
            );
            Ok(ClientRunResult::ExitCode(crate::EXIT_MUX_OWNER_LOST))
        }
        ClientOutcome::Rejected { reason } => Ok(ClientRunResult::Rejected { reason }),
    }
}

/// The body of [`run`] with the terminal streams injected, so tests can drive
/// it against in-memory buffers. Sends a [`Frame::Hello`], waits for the
/// owner's `HelloAck`/`Rejected`, then relays local stdin as [`Frame::Stdin`]
/// frames (and a final [`Frame::Shutdown`] on local EOF) while writing the
/// owner's `Stdout`/`Stderr` frames to the local streams until an [`Frame::Exit`]
/// (clean end) or a dropped connection (owner lost).
///
/// The owner connection's read half is owned by a dedicated frame-reader task
/// (see [`spawn_frame_reader`]) so the `select!` loop can await frames on a
/// cancel-safe `recv()`; reading `read_frame` directly in the `select!` arm
/// would drop a half-read frame whenever the stdin branch won the race and
/// desync the stream.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_inner<CR, CW, I, O, E>(
    conn_read: CR,
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
    CR: AsyncRead + Unpin + Send + 'static,
    CW: AsyncWrite + Unpin,
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    write_frame(conn_write, &Frame::Hello { version: MUX_PROTOCOL_VERSION, token: token.to_vec(), term, cols, rows })
        .await
        .map_err(|e| anyhow!("isekai-ssh: failed to send Hello to the owner: {e}"))?;

    let mut frame_rx = spawn_frame_reader(conn_read);

    match frame_rx.recv().await {
        Some(Ok(Some(Frame::HelloAck { version }))) => {
            if version != MUX_PROTOCOL_VERSION {
                return Ok(ClientOutcome::Rejected {
                    reason: format!("owner speaks mux protocol version {version}, we speak {MUX_PROTOCOL_VERSION}"),
                });
            }
        }
        Some(Ok(Some(Frame::Rejected { reason }))) => return Ok(ClientOutcome::Rejected { reason }),
        Some(Ok(Some(other))) => {
            return Ok(ClientOutcome::Rejected { reason: format!("expected HelloAck from the owner, got {other:?}") })
        }
        // Owner vanished during the handshake — treat as owner-lost so the
        // user gets the reconnect guidance rather than an opaque error.
        Some(Ok(None)) | Some(Err(_)) | None => return Ok(ClientOutcome::OwnerLost),
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
            frame = frame_rx.recv() => {
                match frame {
                    Some(Ok(Some(Frame::Stdout(data)))) => {
                        let _ = stdout.write_all(&data).await;
                        let _ = stdout.flush().await;
                    }
                    Some(Ok(Some(Frame::Stderr(data)))) => {
                        let _ = stderr.write_all(&data).await;
                        let _ = stderr.flush().await;
                    }
                    Some(Ok(Some(Frame::Ctl(data)))) => {
                        // A control-plane message (`#@isekai ctl-socket`) the
                        // owner relayed from this tab's remote forward. Decode
                        // and apply it to *this* client's own terminal (OSC
                        // title/clipboard); a malformed or no-op message is
                        // silently ignored (opportunistic feature).
                        if let Ok(msg) = isekai_protocol::decode_ctl_message(&data) {
                            if let Some(seq) = crate::ctl_forward::osc_sequence_for(&msg) {
                                let _ = stderr.write_all(seq.as_bytes()).await;
                                let _ = stderr.flush().await;
                            }
                        }
                    }
                    Some(Ok(Some(Frame::Exit(code)))) => return Ok(ClientOutcome::Exited(code)),
                    Some(Ok(Some(other))) => return Err(anyhow!("isekai-ssh: unexpected frame from the owner: {other:?}")),
                    // A clean close without an Exit, any read error (a reset
                    // pipe), or the reader task ending all mean the owner died
                    // mid-session.
                    Some(Ok(None)) | Some(Err(_)) | None => return Ok(ClientOutcome::OwnerLost),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::protocol::read_frame;
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
        let (cr, mut cw) = tokio::io::split(client_conn);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let outcome = run_inner(
            cr,
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

    /// A `Ctl` frame (relayed `#@isekai ctl-socket` message) is decoded and
    /// applied to the client's own terminal as an OSC sequence on stderr — a
    /// `SetTitle` becomes OSC 0. Proves the owner→client control-plane relay
    /// reaches this process's terminal, not the owner's.
    #[tokio::test]
    async fn client_applies_a_ctl_frame_as_an_osc_sequence_on_stderr() {
        let (outcome, stdout, stderr) = drive_client(b"", |owner_conn| {
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(owner_conn);
                let _ = read_frame(&mut r).await;
                write_frame(&mut w, &Frame::HelloAck { version: MUX_PROTOCOL_VERSION }).await.unwrap();
                write_frame(&mut w, &Frame::Ctl(br#"{"op":"title","value":"hi"}"#.to_vec())).await.unwrap();
                write_frame(&mut w, &Frame::Exit(0)).await.unwrap();
            })
        })
        .await;

        assert_eq!(outcome.unwrap(), ClientOutcome::Exited(0));
        assert!(stdout.is_empty(), "a ctl message must never leak onto the client's stdout");
        assert_eq!(stderr, b"\x1b]0;hi\x07", "a SetTitle ctl message must become an OSC 0 sequence on the client's stderr");
    }

    /// A malformed `Ctl` payload is ignored (opportunistic feature): no OSC is
    /// emitted and the session continues to a clean Exit rather than erroring.
    #[tokio::test]
    async fn client_ignores_a_malformed_ctl_frame() {
        let (outcome, _stdout, stderr) = drive_client(b"", |owner_conn| {
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(owner_conn);
                let _ = read_frame(&mut r).await;
                write_frame(&mut w, &Frame::HelloAck { version: MUX_PROTOCOL_VERSION }).await.unwrap();
                write_frame(&mut w, &Frame::Ctl(b"not valid ctl json".to_vec())).await.unwrap();
                write_frame(&mut w, &Frame::Exit(0)).await.unwrap();
            })
        })
        .await;

        assert_eq!(outcome.unwrap(), ClientOutcome::Exited(0), "a malformed ctl message must not fail the session");
        assert!(stderr.is_empty(), "a malformed ctl message must produce no OSC output");
    }

    /// The owner refuses the Hello (e.g. a stale token from an owner-turnover
    /// race) — surfaced as `ClientOutcome::Rejected` carrying the owner's
    /// reason, not an `Err` and not `OwnerLost`. Nothing was lost (no shell
    /// session ever started), so `run_as_client` can safely fall back to an
    /// unmultiplexed direct connect instead of failing the invocation.
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

        match outcome.unwrap() {
            ClientOutcome::Rejected { reason } => {
                assert!(reason.contains("token mismatch"), "the owner's reject reason must reach the caller, got {reason:?}");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    /// A protocol-version mismatch (e.g. an old owner and a freshly-upgraded
    /// client binary) is also `Rejected`, not a hard `Err` — the version
    /// field exists specifically so this degrades gracefully to a direct
    /// connect instead of blocking the client from connecting at all.
    #[tokio::test]
    async fn client_treats_a_version_mismatch_as_rejected_not_an_error() {
        let (outcome, _out, _err) = drive_client(b"", |owner_conn| {
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(owner_conn);
                let _ = read_frame(&mut r).await;
                write_frame(&mut w, &Frame::HelloAck { version: MUX_PROTOCOL_VERSION + 1 }).await.unwrap();
            })
        })
        .await;

        match outcome.unwrap() {
            ClientOutcome::Rejected { reason } => {
                assert!(reason.contains("version"), "the reason should mention the version mismatch, got {reason:?}");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
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

        let (cr, mut cw) = tokio::io::split(client_conn);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let outcome = run_inner(cr, &mut cw, b"tok", "xterm".to_string(), 80, 24, &b"echo hi\n"[..], &mut stdout, &mut stderr)
            .await
            .unwrap();

        assert_eq!(outcome, ClientOutcome::Exited(0));
        let got_stdin = owner.await.unwrap();
        assert_eq!(got_stdin, b"echo hi\n", "local stdin must be relayed to the owner verbatim");
    }

    /// Regression for the frame-reader cancel-safety fix: the owner streams many
    /// distinct `Stdout` frames while local stdin trickles bytes, so the stdin
    /// `select!` branch repeatedly wins the race against an in-flight frame read.
    /// Every stdout payload must still arrive intact and in order — with the old
    /// design (calling the non-cancel-safe `read_frame` directly in the `select!`
    /// arm) a half-read frame would be dropped and desync the stream.
    #[tokio::test]
    async fn frames_are_not_lost_when_the_stdin_branch_keeps_winning() {
        const N: usize = 64;
        let payload_of = |i: usize| format!("frame-{i:04}\n").into_bytes();

        let (client_conn, owner_conn) = duplex(64 * 1024);
        // The owner keeps its read half alive for the whole test (draining the
        // client's small stdin→owner writes so they never fail) and is aborted
        // at the end. It deliberately does not try to end on the client's EOF:
        // `tokio::io::split`'s WriteHalf drop does not shut the DuplexStream
        // while the client's orphaned frame-reader task still holds its read
        // half, so an EOF-driven owner would never terminate.
        let owner = tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(owner_conn);
            let _ = read_frame(&mut r).await; // consume Hello
            write_frame(&mut w, &Frame::HelloAck { version: MUX_PROTOCOL_VERSION }).await.unwrap();
            for i in 0..N {
                write_frame(&mut w, &Frame::Stdout(format!("frame-{i:04}\n").into_bytes())).await.unwrap();
                // Yield so the client often observes a frame mid-arrival, keeping
                // the stdin branch racing an in-flight frame read.
                tokio::task::yield_now().await;
            }
            write_frame(&mut w, &Frame::Exit(0)).await.unwrap();
            while let Ok(Some(_)) = read_frame(&mut r).await {}
        });

        // Trickle stdin one byte at a time with a yield between, keeping the
        // stdin branch hot while frame reads are pending.
        let (mut stdin_w, stdin_r) = duplex(64 * 1024);
        let feeder = tokio::spawn(async move {
            for _ in 0..N {
                if stdin_w.write_all(b"x").await.is_err() {
                    break;
                }
                let _ = stdin_w.flush().await;
                tokio::task::yield_now().await;
            }
            drop(stdin_w); // stdin EOF → client sends Shutdown
        });

        let (cr, mut cw) = tokio::io::split(client_conn);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let outcome = run_inner(cr, &mut cw, b"tok", "xterm".to_string(), 80, 24, stdin_r, &mut stdout, &mut stderr)
            .await
            .unwrap();

        assert_eq!(outcome, ClientOutcome::Exited(0), "a clean remote exit must be reported even under stdin-branch pressure");
        let mut expected = Vec::new();
        for i in 0..N {
            expected.extend_from_slice(&payload_of(i));
        }
        assert_eq!(stdout, expected, "no stdout frame may be lost, corrupted, or reordered when the stdin branch keeps winning the select");

        let _ = feeder.await;
        owner.abort();
    }
}
