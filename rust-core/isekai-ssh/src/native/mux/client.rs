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
///
/// Propagates local terminal resize events to the owner via [`Frame::Resize`]
/// frames (which the owner forwards to the remote PTY).
pub(crate) async fn run<Conn>(conn: Conn, token: &[u8], host: String) -> Result<ClientRunResult>
where
    Conn: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(conn);
    let (cols, rows) = super::super::console::terminal_size();
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());

    let resize_rx = super::super::console::spawn_resize_watcher();

    let _raw_mode = super::super::console::RawModeGuard::enable().map_err(|e| anyhow!("isekai-ssh: failed to enable raw terminal mode: {e}"))?;
    let outcome = run_inner(
        reader,
        &mut writer,
        token,
        term,
        cols as u16,
        rows as u16,
        super::super::console_stdin::ConsoleStdin::open(),
        tokio::io::stdout(),
        tokio::io::stderr(),
        resize_rx,
        host,
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

/// The body of [`run`] with the terminal streams plus an optional resize
/// event channel injected, so tests can drive it against in-memory buffers.
/// Sends a [`Frame::Hello`], waits for the owner's `HelloAck`/`Rejected`,
/// then relays local stdin as [`Frame::Stdin`] frames (and a final
/// [`Frame::Shutdown`] on local EOF), local resize events as
/// [`Frame::Resize`] frames, while writing the owner's `Stdout`/`Stderr`
/// frames to the local streams until an [`Frame::Exit`] (clean end) or a
/// dropped connection (owner lost).
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
    mut resize_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(u32, u32)>>,
    host: String,
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
        // The owner connection dropped *during the handshake* — before any
        // shell session ever existed. Unlike a mid-session drop (below, in
        // the main loop), nothing was lost here, so this must be `Rejected`
        // (safe to fall back to a direct connect), not `OwnerLost` (which the
        // caller maps to a hard exit code with reconnect guidance). This
        // matters specifically for a foreground tab that just spawned a
        // detached holder (`ControlPersist`-equivalent, `native/mux/holder.rs`):
        // the holder may still be silently authenticating (or may fail to,
        // e.g. a passphrase/keyboard-interactive prompt it can't answer) when
        // this tab's `try_claim`-losing connect races ahead of it — that must
        // degrade to an ordinary direct connect, not a scary "owner lost" exit.
        Some(Ok(None)) | Some(Err(_)) | None => {
            return Ok(ClientOutcome::Rejected { reason: "the owner connection was lost during the handshake".to_string() })
        }
    }

    let mut buf = [0u8; 8192];
    let mut stdin_open = true;

    // Epic P Phase 2: at most one build in flight per tab (a second
    // `BuildRequest` while one is already running is logged and ignored —
    // see the `Frame::Ctl` arm below). `build_out_tx` is kept alive here for
    // the whole loop (never dropped) so `build_out_rx.recv()` only ever
    // yields real build output, never a spurious `None`.
    let (build_out_tx, mut build_out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let mut active_build: Option<super::build_relay::ActiveBuild> = None;

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
                            if let Some(build) = &mut active_build {
                                build.abort();
                            }
                            return Ok(ClientOutcome::OwnerLost);
                        }
                    }
                }
            }
            // A build task's `BuildOutputChunk`/`BuildFinished` bytes, relayed
            // to the owner (which routes them onto the real ctl channel —
            // `super::owner`'s module docs) as `Frame::Ctl` on this same
            // writer, exactly like `Stdin`/`Resize` above. Any `BuildFinished`
            // seen here is always a *real* completion (`run_client_build`'s
            // own, genuine exit code) — the abort sentinel only ever arrives
            // the other way, via the `frame_rx` branch below — so this is the
            // only place a normal completion needs to clear `active_build`
            // (found missing in review: without this, a tab could only ever
            // run one build for its whole lifetime, since `active_build`
            // stayed `Some` forever after the first one finished).
            bytes = build_out_rx.recv() => {
                if let Some(bytes) = bytes {
                    let is_finished = matches!(
                        isekai_protocol::decode_ctl_message(&bytes),
                        Ok(isekai_protocol::CtlMessage::BuildFinished { .. })
                    );
                    if write_frame(conn_write, &Frame::Ctl(bytes)).await.is_err() {
                        if let Some(build) = &mut active_build {
                            build.abort();
                        }
                        return Ok(ClientOutcome::OwnerLost);
                    }
                    if is_finished {
                        active_build = None;
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
                        // owner relayed from this tab's remote forward.
                        match isekai_protocol::decode_ctl_message(&data) {
                            // Epic P Phase 2: run the build profile this tab
                            // owns (unlike title/clip, this can't be applied
                            // as an OSC sequence — it's real work only *this*
                            // process can do, streamed back via `build_out_tx`
                            // rather than applied in place). A second request
                            // while one is already running is logged and
                            // ignored rather than starting a concurrent build.
                            Ok(isekai_protocol::CtlMessage::BuildRequest { profile }) => {
                                if active_build.is_some() {
                                    log_line!("isekai-ssh: ignoring a BuildRequest for {profile:?} — a build is already running for this tab");
                                } else {
                                    active_build = Some(super::build_relay::spawn_client_build(host.clone(), profile, build_out_tx.clone()));
                                }
                            }
                            // The owner's synthesized abort signal (the real
                            // remote ctl channel closed mid-build — this tab
                            // never receives its *own* real `BuildFinished`
                            // this way, only ever this sentinel) — kill the
                            // still-running child instead of streaming into a
                            // channel nobody on the other end is reading.
                            Ok(isekai_protocol::CtlMessage::BuildFinished { exit_code, .. })
                                if exit_code == super::build_relay::BUILD_ABORTED_SENTINEL =>
                            {
                                if let Some(mut build) = active_build.take() {
                                    build.abort();
                                }
                            }
                            Ok(msg) => {
                                if let Some(seq) = crate::ctl_forward::osc_sequence_for(&msg) {
                                    let _ = stderr.write_all(seq.as_bytes()).await;
                                    let _ = stderr.flush().await;
                                }
                            }
                            // A malformed message is ignored (opportunistic feature).
                            Err(_) => {}
                        }
                    }
                    Some(Ok(Some(Frame::Exit(code)))) => return Ok(ClientOutcome::Exited(code)),
                    Some(Ok(Some(other))) => return Err(anyhow!("isekai-ssh: unexpected frame from the owner: {other:?}")),
                    // A clean close without an Exit, any read error (a reset
                    // pipe), or the reader task ending all mean the owner died
                    // mid-session.
                    Some(Ok(None)) | Some(Err(_)) | None => {
                        if let Some(build) = &mut active_build {
                            build.abort();
                        }
                        return Ok(ClientOutcome::OwnerLost);
                    }
                }
            }
            resize = recv_resize(&mut resize_rx) => {
                if let Some((cols, rows)) = resize {
                    if write_frame(conn_write, &Frame::Resize { cols: cols as u16, rows: rows as u16 }).await.is_err() {
                        if let Some(build) = &mut active_build {
                            build.abort();
                        }
                        return Ok(ClientOutcome::OwnerLost);
                    }
                }
            }
        }
    }
}

/// `recv` on the optional resize channel, or a future that never resolves
/// when there is no watcher (so the `select!` branch is inert).
async fn recv_resize(
    rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<(u32, u32)>>,
) -> Option<(u32, u32)> {
    match rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
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
            None,
            "mybox".to_string(),
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

    /// The owner connection drops *before ever sending HelloAck* (e.g. a
    /// just-spawned `ControlPersist`-equivalent holder that's still silently
    /// authenticating, or failed to and exited) — must be `Rejected`, not
    /// `OwnerLost`: no shell session ever existed, so it's always safe for
    /// the caller to fall back to a direct connect (`always-connects.md`).
    #[tokio::test]
    async fn client_treats_a_drop_before_hello_ack_as_rejected_not_owner_lost() {
        let (outcome, _out, _err) = drive_client(b"", |owner_conn| {
            tokio::spawn(async move {
                let (mut r, _w) = tokio::io::split(owner_conn);
                let _ = read_frame(&mut r).await; // consume Hello
                // Drop immediately, without ever sending HelloAck/Rejected.
            })
        })
        .await;

        match outcome.unwrap() {
            ClientOutcome::Rejected { .. } => {}
            other => panic!("a pre-HelloAck drop must be Rejected (safe to fall back), got {other:?}"),
        }
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
        let outcome = run_inner(cr, &mut cw, b"tok", "xterm".to_string(), 80, 24, &b"echo hi\n"[..], &mut stdout, &mut stderr, None, "mybox".to_string())
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
        let outcome = run_inner(cr, &mut cw, b"tok", "xterm".to_string(), 80, 24, stdin_r, &mut stdout, &mut stderr, None, "mybox".to_string())
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

    /// Points `$HOME` at a fresh tempdir and writes `profiles` to
    /// `build_profiles.toml` there — same `HOME_ENV_LOCK`-guarded pattern
    /// `build_relay.rs`'s own tests use.
    fn with_build_profiles(profiles: Vec<crate::build_profile::BuildProfile>) -> (tempfile::TempDir, HomeRestoreGuard) {
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let mut store = crate::build_profile::BuildProfileStore::default();
        for profile in profiles {
            crate::build_profile::upsert_profile(&mut store, profile).unwrap();
        }
        let path = crate::build_profile::default_build_profiles_path().unwrap();
        crate::build_profile::save_build_profiles(&path, &store).unwrap();
        (home, HomeRestoreGuard(old_home))
    }

    struct HomeRestoreGuard(Option<std::ffi::OsString>);
    impl Drop for HomeRestoreGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(old) => std::env::set_var("HOME", old),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// End-to-end for Epic P Phase 2's mux-client path: the owner relays a
    /// `BuildRequest` as `Frame::Ctl`; `run_inner` must run the matching
    /// profile and relay its `BuildOutputChunk`/`BuildFinished` back as
    /// further `Frame::Ctl`s on the *same* connection, all while the normal
    /// stdin/stdout/resize relay keeps working (proven by the session still
    /// ending cleanly on `Exit`).
    #[tokio::test]
    async fn client_runs_a_build_profile_and_streams_output_to_the_owner() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let workdir = tempfile::tempdir().unwrap();
        let (_home, _restore) = with_build_profiles(vec![crate::build_profile::BuildProfile {
            host: "mybox".to_string(),
            name: "t".to_string(),
            dir: workdir.path().to_string_lossy().into_owned(),
            command: if cfg!(windows) {
                "echo out-line& echo err-line 1>&2& exit 5".to_string()
            } else {
                "printf 'out-line\\n'; printf 'err-line\\n' 1>&2; exit 5".to_string()
            },
            result_glob: None,
            dest_dir: None,
        }]);

        let (client_conn, owner_conn) = duplex(64 * 1024);
        let (mut or, mut ow) = tokio::io::split(owner_conn);

        let owner = tokio::spawn(async move {
            match read_frame(&mut or).await.unwrap().unwrap() {
                Frame::Hello { .. } => {}
                other => panic!("expected Hello, got {other:?}"),
            }
            write_frame(&mut ow, &Frame::HelloAck { version: MUX_PROTOCOL_VERSION }).await.unwrap();

            let request = serde_json::to_vec(&isekai_protocol::CtlMessage::BuildRequest { profile: "t".to_string() }).unwrap();
            write_frame(&mut ow, &Frame::Ctl(request)).await.unwrap();

            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let exit_code = loop {
                match read_frame(&mut or).await.unwrap().unwrap() {
                    Frame::Ctl(bytes) => match isekai_protocol::decode_ctl_message(&bytes).unwrap() {
                        isekai_protocol::CtlMessage::BuildOutputChunk { stream, data_b64 } => {
                            let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64).unwrap();
                            match stream {
                                isekai_protocol::BuildOutputStream::Stdout => stdout.extend(decoded),
                                isekai_protocol::BuildOutputStream::Stderr => stderr.extend(decoded),
                            }
                        }
                        isekai_protocol::CtlMessage::BuildFinished { exit_code, .. } => break exit_code,
                        other => panic!("unexpected message: {other:?}"),
                    },
                    Frame::Shutdown => {} // empty test stdin hits EOF immediately; irrelevant here
                    other => panic!("unexpected frame: {other:?}"),
                }
            };
            write_frame(&mut ow, &Frame::Exit(0)).await.unwrap();
            (stdout, stderr, exit_code)
        });

        let (cr, mut cw) = tokio::io::split(client_conn);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let outcome = run_inner(cr, &mut cw, b"tok", "xterm".to_string(), 80, 24, &b""[..], &mut stdout, &mut stderr, None, "mybox".to_string())
            .await
            .unwrap();

        assert_eq!(outcome, ClientOutcome::Exited(0));
        let (build_stdout, build_stderr, exit_code) = owner.await.unwrap();
        assert!(String::from_utf8_lossy(&build_stdout).contains("out-line"));
        assert!(String::from_utf8_lossy(&build_stderr).contains("err-line"));
        assert_eq!(exit_code, 5);
    }

    /// Regression for a review finding: `active_build` must be cleared once a
    /// build finishes normally, not just on the owner-relayed abort sentinel
    /// — otherwise a tab could only ever run one build for its whole
    /// lifetime (every subsequent `BuildRequest` silently ignored by the
    /// `active_build.is_some()` guard). Runs two builds back to back over the
    /// same `run_inner` session and requires both to actually execute.
    #[tokio::test]
    async fn client_can_run_a_second_build_after_the_first_finishes() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let workdir = tempfile::tempdir().unwrap();
        let (_home, _restore) = with_build_profiles(vec![crate::build_profile::BuildProfile {
            host: "mybox".to_string(),
            name: "t".to_string(),
            dir: workdir.path().to_string_lossy().into_owned(),
            command: "exit 0".to_string(),
            result_glob: None,
            dest_dir: None,
        }]);

        let (client_conn, owner_conn) = duplex(64 * 1024);
        let (mut or, mut ow) = tokio::io::split(owner_conn);

        let owner = tokio::spawn(async move {
            match read_frame(&mut or).await.unwrap().unwrap() {
                Frame::Hello { .. } => {}
                other => panic!("expected Hello, got {other:?}"),
            }
            write_frame(&mut ow, &Frame::HelloAck { version: MUX_PROTOCOL_VERSION }).await.unwrap();

            let mut finished_count = 0;
            for _ in 0..2 {
                let request = serde_json::to_vec(&isekai_protocol::CtlMessage::BuildRequest { profile: "t".to_string() }).unwrap();
                write_frame(&mut ow, &Frame::Ctl(request)).await.unwrap();

                loop {
                    match read_frame(&mut or).await.unwrap().unwrap() {
                        Frame::Ctl(bytes) => {
                            if matches!(isekai_protocol::decode_ctl_message(&bytes), Ok(isekai_protocol::CtlMessage::BuildFinished { .. })) {
                                finished_count += 1;
                                break;
                            }
                        }
                        Frame::Shutdown => {}
                        other => panic!("unexpected frame: {other:?}"),
                    }
                }
            }
            write_frame(&mut ow, &Frame::Exit(0)).await.unwrap();
            finished_count
        });

        let (cr, mut cw) = tokio::io::split(client_conn);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_inner(cr, &mut cw, b"tok", "xterm".to_string(), 80, 24, &b""[..], &mut stdout, &mut stderr, None, "mybox".to_string()),
        )
        .await
        .expect("run_inner must not hang waiting on a second build that the active_build guard silently ignores")
        .unwrap();

        assert_eq!(outcome, ClientOutcome::Exited(0));
        assert_eq!(owner.await.unwrap(), 2, "both builds must have reached BuildFinished, not just the first");
    }

    /// The owner relaying the synthesized abort sentinel (a real remote ctl
    /// channel closing mid-build, `super::owner`'s module docs) must reach
    /// `run_inner` and kill its still-running child rather than let it keep
    /// streaming into a connection nobody on the far end is reading from —
    /// and the session must still end cleanly afterward (proving `run_inner`
    /// itself doesn't hang or error out reacting to the sentinel).
    #[tokio::test]
    async fn client_kills_the_build_when_the_owner_relays_the_abort_sentinel() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let workdir = tempfile::tempdir().unwrap();
        let (_home, _restore) = with_build_profiles(vec![crate::build_profile::BuildProfile {
            host: "mybox".to_string(),
            name: "infinite".to_string(),
            dir: workdir.path().to_string_lossy().into_owned(),
            command: if cfg!(windows) {
                ":loop& echo x& goto loop".to_string()
            } else {
                "while true; do printf x; sleep 0.01; done".to_string()
            },
            result_glob: None,
            dest_dir: None,
        }]);

        let (client_conn, owner_conn) = duplex(64 * 1024);
        let (mut or, mut ow) = tokio::io::split(owner_conn);

        let owner = tokio::spawn(async move {
            match read_frame(&mut or).await.unwrap().unwrap() {
                Frame::Hello { .. } => {}
                other => panic!("expected Hello, got {other:?}"),
            }
            write_frame(&mut ow, &Frame::HelloAck { version: MUX_PROTOCOL_VERSION }).await.unwrap();

            let request = serde_json::to_vec(&isekai_protocol::CtlMessage::BuildRequest { profile: "infinite".to_string() }).unwrap();
            write_frame(&mut ow, &Frame::Ctl(request)).await.unwrap();

            // Wait for at least one real output chunk (proves the build
            // actually started) before telling the client to abort it.
            loop {
                match read_frame(&mut or).await.unwrap().unwrap() {
                    Frame::Ctl(bytes) => {
                        if matches!(isekai_protocol::decode_ctl_message(&bytes), Ok(isekai_protocol::CtlMessage::BuildOutputChunk { .. })) {
                            break;
                        }
                    }
                    Frame::Shutdown => {} // empty test stdin hits EOF immediately; irrelevant here
                    other => panic!("unexpected frame: {other:?}"),
                }
            }

            let abort = serde_json::to_vec(&isekai_protocol::CtlMessage::BuildFinished {
                exit_code: super::super::build_relay::BUILD_ABORTED_SENTINEL,
                result_paths: Vec::new(),
            })
            .unwrap();
            write_frame(&mut ow, &Frame::Ctl(abort)).await.unwrap();
            write_frame(&mut ow, &Frame::Exit(0)).await.unwrap();
        });

        let (cr, mut cw) = tokio::io::split(client_conn);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_inner(cr, &mut cw, b"tok", "xterm".to_string(), 80, 24, &b""[..], &mut stdout, &mut stderr, None, "mybox".to_string()),
        )
        .await
        .expect("run_inner must not hang after the abort sentinel and a clean Exit")
        .unwrap();

        assert_eq!(outcome, ClientOutcome::Exited(0));
        owner.await.unwrap();
    }
}
