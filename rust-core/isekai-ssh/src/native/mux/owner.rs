//! Owner side of the mux: the process that won [`local_ipc_mux::ExclusiveChannel::try_claim`]
//! holds the one shared, already-authenticated `russh` `client::Handle` and
//! serves every other tab's `isekai-ssh` process a *private, independent*
//! remote shell over that shared connection.
//!
//! Per the M4 design, this is `ControlMaster`, not screen sharing: for each
//! accepted client the owner opens a brand-new SSH shell channel on the shared
//! handle ([`open_channel`] with [`SessionKind::Shell`], or a
//! `$ISEKAI_CTL_SOCK`-exporting login shell when `#@isekai ctl-socket` is on,
//! see [`relay_client`]) and relays *that one channel's* traffic to *that one
//! client*. Clients never see each other's terminals; only the authenticated
//! connection (and the auth/TOFU work behind it) is shared. When ctl-socket is
//! on, each client also gets its own *private* remote-forward whose messages
//! are relayed to it as [`Frame::Ctl`], never crossing between clients.
//!
//! **Ordering**: [`relay_loop`] pumps the client→remote direction in a single
//! sequential branch — each [`Frame::Stdin`]/[`Frame::Resize`] is applied to
//! the remote channel before the next client frame is read — so a resize the
//! client sent before some input bytes reaches the remote PTY before those
//! bytes, never reordered after them.
//!
//! **Backpressure**: the remote→client direction awaits each [`Frame::Stdout`]/
//! [`Frame::Stderr`] write before reading the next chunk off the SSH channel,
//! so a slow client stops the owner draining the SSH channel, which propagates
//! SSH-level flow control back to the remote — no unbounded buffer grows
//! between the two.

use anyhow::{anyhow, Context, Result};
use local_ipc_mux::ExclusiveChannel;
use russh::client;
use russh_stream_session::{open_channel, ForwardRoutes, SessionKind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, Mutex, Notify};

use crate::log_file::log_line;

use super::ctl_forward;
use super::protocol::{read_frame, spawn_frame_reader, token_eq, write_frame, Frame, MUX_PROTOCOL_VERSION};

/// How long the very first accept loop iteration waits for the *first ever*
/// client to connect before giving up — generous, since a detached
/// `ControlPersist`-equivalent holder (`native/mux/holder.rs`) is spawned
/// *just before* its spawner tries to connect, and the spawner itself may
/// still be doing its own SSH-target host-key/agent lookups first.
const WARMUP_GRACE: Duration = Duration::from_secs(30);
/// How long the accept loop waits, once it has served at least one client and
/// the count has dropped back to zero, before exiting — this is the actual
/// `ControlPersist`-equivalent lifetime policy: short enough that a holder
/// with no real tabs left doesn't linger holding an authenticated SSH session
/// forever, long enough to absorb a user quickly closing one tab and opening
/// another to the same host.
const IDLE_GRACE: Duration = Duration::from_secs(10);

/// Accepts clients on `channel` until either `accept` itself fails (the
/// underlying IPC channel died — a genuine local-pipe infrastructure
/// problem), or the client count drops to (and stays at) zero for the
/// relevant grace window (see [`WARMUP_GRACE`]/[`IDLE_GRACE`]) — the
/// `ControlPersist`-equivalent idle-exit policy for the detached holder
/// process ([`super::holder`]) that calls this. Returns `Ok(())` either way;
/// the caller (the holder's own body) treats both as "done, exit cleanly" —
/// there is nothing left to serve.
///
/// Spawns an independent relay task per client (each opening its own shell
/// channel on the shared `handle`). The handle is shared via `Arc<Mutex<_>>`
/// because `russh`'s `client::Handle` is not `Clone`; the mutex is held only
/// for the brief `channel_open_session`/`streamlocal_forward` calls (the
/// latter needs `&mut self`), never across a client's relay loop, so clients
/// still stream concurrently. A single client's relay error is logged and
/// contained, never propagated to sibling clients or this accept loop.
pub(crate) async fn serve_clients<C, H>(
    mut channel: C,
    handle: Arc<Mutex<client::Handle<H>>>,
    token: Arc<Vec<u8>>,
    ctl_routes: Option<ForwardRoutes>,
) -> Result<()>
where
    C: ExclusiveChannel,
    H: client::Handler + 'static,
{
    let active_clients = Arc::new(AtomicUsize::new(0));
    // Notified on every count change (increment *or* decrement) — the
    // idle-exit wait below only actually cares about decrements reaching
    // zero, but re-checking on every notification is cheap and keeps the
    // signaling side (the per-client task below) simple: it doesn't need to
    // know which transitions the waiter cares about.
    let count_changed = Arc::new(Notify::new());
    let mut ever_served_a_client = false;

    loop {
        let grace = if ever_served_a_client { IDLE_GRACE } else { WARMUP_GRACE };
        tokio::select! {
            conn = channel.accept() => {
                let conn = conn.context("isekai-ssh mux owner: accepting a client connection failed")?;
                ever_served_a_client = true;
                active_clients.fetch_add(1, Ordering::SeqCst);
                count_changed.notify_one();
                let handle = handle.clone();
                let token = token.clone();
                // `Some` iff `#@isekai ctl-socket` is on — each client then gets its
                // own private per-tab forward (see [`relay_client`]).
                let ctl_routes = ctl_routes.clone();
                let active_clients = active_clients.clone();
                let count_changed = count_changed.clone();
                tokio::spawn(async move {
                    if let Err(e) = relay_client(conn, &handle, token.as_slice(), ctl_routes.as_ref()).await {
                        // One client's session ending badly must not disturb the
                        // owner or its other clients (session isolation).
                        log_line!("isekai-ssh mux owner: a client session ended with an error: {e:#}");
                    }
                    active_clients.fetch_sub(1, Ordering::SeqCst);
                    count_changed.notify_one();
                });
            }
            _ = wait_for_idle_exit(&active_clients, &count_changed, grace) => {
                log_line!("isekai-ssh mux owner: no clients for {grace:?}; exiting (ControlPersist-equivalent idle-exit)");
                return Ok(());
            }
        }
    }
}

/// Resolves once the client count has been zero continuously for `grace` —
/// i.e. it *never* resolves while `active_clients` is nonzero, and restarts
/// its internal timer every time the count changes (a new client connecting,
/// or another one disconnecting) while still zero. Split out as its own
/// function (rather than inlined in `serve_clients`'s `select!`) purely so
/// the zero-clients-forever case, the immediate-non-zero case, and the
/// "reset on change" case can each be unit-tested directly against a plain
/// `AtomicUsize`/`Notify` pair, without a real `ExclusiveChannel`.
async fn wait_for_idle_exit(active_clients: &AtomicUsize, count_changed: &Notify, grace: Duration) {
    loop {
        if active_clients.load(Ordering::SeqCst) > 0 {
            count_changed.notified().await;
            continue;
        }
        tokio::select! {
            _ = tokio::time::sleep(grace) => return,
            _ = count_changed.notified() => continue,
        }
    }
}

/// Serves exactly one client: reads its [`Frame::Hello`] (validating the
/// protocol version and auth token), opens a private remote shell channel with
/// the client's requested PTY geometry, then relays until either side ends.
/// When `ctl_routes` is `Some` (`#@isekai ctl-socket` on), also requests a
/// *private* per-tab streamlocal forward for this client, opens the shell with
/// `$ISEKAI_CTL_SOCK` exported, and relays each incoming ctl message to the
/// client as a [`Frame::Ctl`] so it lands on *that* client's own terminal.
pub(crate) async fn relay_client<Conn, H>(
    conn: Conn,
    handle: &Mutex<client::Handle<H>>,
    expected_token: &[u8],
    ctl_routes: Option<&ForwardRoutes>,
) -> Result<()>
where
    Conn: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    H: client::Handler,
{
    let (mut reader, mut writer) = tokio::io::split(conn);

    let hello = read_frame(&mut reader)
        .await
        .context("isekai-ssh mux owner: reading the client Hello failed")?
        .ok_or_else(|| anyhow!("isekai-ssh mux owner: client closed before sending Hello"))?;

    let (term, cols, rows) = match hello {
        Frame::Hello { version, token, term, cols, rows } => {
            if version != MUX_PROTOCOL_VERSION {
                let reason = format!("protocol version mismatch: owner speaks {MUX_PROTOCOL_VERSION}, client speaks {version}");
                let _ = write_frame(&mut writer, &Frame::Rejected { reason: reason.clone() }).await;
                return Err(anyhow!("isekai-ssh mux owner: rejected client — {reason}"));
            }
            if !token_eq(&token, expected_token) {
                let _ = write_frame(&mut writer, &Frame::Rejected { reason: "authentication token mismatch".to_string() }).await;
                return Err(anyhow!("isekai-ssh mux owner: rejected client — invalid auth token"));
            }
            (term, cols, rows)
        }
        other => return Err(anyhow!("isekai-ssh mux owner: expected Hello as the first frame, got {other:?}")),
    };

    write_frame(&mut writer, &Frame::HelloAck { version: MUX_PROTOCOL_VERSION })
        .await
        .context("isekai-ssh mux owner: sending HelloAck failed")?;

    // This client's own private ctl-socket forward (opportunistic: a failed
    // setup just leaves `ctl` as `None`).
    let ctl = match ctl_routes {
        Some(routes) => ctl_forward::request(handle, routes).await,
        None => None,
    };

    let open_result = {
        // Lock held only for the open; the relay below runs lock-free so one
        // client's traffic never blocks another's channel open or forward. The
        // guard is dropped at the end of this block, before any `ctl_forward`
        // cleanup below re-locks the handle.
        let guard = handle.lock().await;
        match &ctl {
            Some(fwd) => ctl_forward::open_login_shell(&guard, &term, cols as u32, rows as u32, &fwd.remote_path)
                .await
                .context("isekai-ssh mux owner: failed to open a ctl-socket login shell for the client"),
            None => open_channel(&guard, &SessionKind::Shell { term, cols: cols as u32, rows: rows as u32, terminal_modes: vec![] })
                .await
                .context("isekai-ssh mux owner: failed to open a shell channel for the client"),
        }
    };
    let mut channel = match open_result {
        Ok(channel) => channel,
        Err(e) => {
            // The channel open failed *after* we'd already requested this
            // client's private ctl-socket forward. Tear that forward down before
            // bailing so it doesn't leak on the remote (and its route entry
            // linger locally) — every exit path must release a requested forward.
            if let (Some(fwd), Some(routes)) = (&ctl, ctl_routes) {
                ctl_forward::cancel(handle, routes, &fwd.remote_path).await;
            }
            return Err(e);
        }
    };

    // Pump this client's forwarded ctl channels into an mpsc the relay loop
    // drains and wraps in `Frame::Ctl` (so all owner→client writes stay on the
    // single relay-loop writer).
    let ctl_remote_path = ctl.as_ref().map(|fwd| fwd.remote_path.clone());
    let ctl_frame_rx = ctl.map(|fwd| {
        let (tx, rx) = mpsc::unbounded_channel::<ctl_forward::CtlRelayEvent>();
        tokio::spawn(ctl_forward::pump_to_frames(fwd.channels, tx));
        rx
    });

    let result = relay_loop(reader, &mut writer, &mut channel, ctl_frame_rx).await;

    // Best-effort teardown of this client's forward.
    if let (Some(path), Some(routes)) = (&ctl_remote_path, ctl_routes) {
        ctl_forward::cancel(handle, routes, path).await;
    }

    result
}

/// The core owner-side relay: client frames drive the remote channel, and
/// remote channel messages become owner→client frames. See the module docs on
/// ordering and backpressure — both properties come from this being one
/// `select!` loop with a single sequential branch per direction.
///
/// The client connection's read half is owned by a dedicated frame-reader task
/// ([`spawn_frame_reader`]) and delivered over a cancel-safe `recv()`, rather
/// than calling the non-cancel-safe `read_frame` directly in the `select!` arm
/// (which would drop a half-read frame whenever another branch won the race and
/// desync the client's frame stream). Ordering is unaffected: the mpsc is FIFO
/// and each frame is fully applied to the remote channel before the next is
/// received.
async fn relay_loop<R, W>(
    reader: R,
    writer: &mut W,
    channel: &mut russh::Channel<client::Msg>,
    mut ctl_frame_rx: Option<mpsc::UnboundedReceiver<ctl_forward::CtlRelayEvent>>,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    let mut frame_rx = spawn_frame_reader(reader);
    let mut exit_code: Option<u8> = None;
    // After the client sends `Shutdown` (its local stdin hit EOF) we stop
    // *forwarding* its input, but keep reading the connection so a subsequent
    // client disconnect is still noticed promptly (`None` below) — otherwise a
    // client that quits while the remote shell is idle would leak this relay
    // task and its channel forever (session cleanup). We do not gate the read
    // branch off, precisely so that drop is always observable.
    let mut stdin_done = false;
    // Set while a mux client is streaming a build's output back to us (Epic P
    // Phase 2, `ctl_forward::pump_to_frames`'s `CtlRelayEvent::BuildStarted`)
    // — routes the client's own `Frame::Ctl` replies into the pump task that
    // owns the real ctl-socket channel. Cleared (dropping the sender, which
    // ends that pump task's `reply_rx.recv()` loop) as soon as a message
    // passing through here — in *either* direction — decodes as
    // `BuildFinished`: a real client-originated completion (below, in the
    // client-frame match) or the pump task's own synthesized abort sentinel
    // (in the ctl-branch match), so state never outlives the build it
    // belongs to regardless of which side ended it.
    let mut active_build_reply_tx: Option<mpsc::UnboundedSender<Vec<u8>>> = None;

    loop {
        tokio::select! {
            frame = frame_rx.recv() => {
                match frame {
                    Some(Ok(Some(Frame::Stdin(data)))) if !stdin_done => {
                        if channel.data(&data[..]).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Some(Frame::Resize { cols, rows }))) if !stdin_done => {
                        // Applied in receive order relative to the Stdin above
                        // (the ordering guarantee); pixel dims are 0 like the
                        // rest of this client (character-cell terminals).
                        let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
                    }
                    // A well-behaved client sends nothing after Shutdown; ignore
                    // any stray input/resize rather than reopening the closed
                    // remote stdin.
                    Some(Ok(Some(Frame::Stdin(_)))) | Some(Ok(Some(Frame::Resize { .. }))) => {}
                    Some(Ok(Some(Frame::Shutdown))) => {
                        if !stdin_done {
                            let _ = channel.eof().await;
                            stdin_done = true;
                        }
                    }
                    // A mux client's build streaming its output back to us
                    // (Epic P Phase 2) — the wire-format-symmetric counterpart
                    // of the owner→client `Frame::Ctl` relay below. Only
                    // meaningful while a build is active (`active_build_reply_tx`
                    // is `Some`, set by the ctl branch's `BuildStarted` event);
                    // a stray `Frame::Ctl` with no active build (a race with an
                    // already-aborted/finished build) is silently ignored
                    // rather than treated as a protocol error.
                    Some(Ok(Some(Frame::Ctl(bytes)))) => {
                        if let Some(tx) = &active_build_reply_tx {
                            let is_finished = matches!(
                                isekai_protocol::decode_ctl_message(&bytes),
                                Ok(isekai_protocol::CtlMessage::BuildFinished { .. })
                            );
                            let _ = tx.send(bytes);
                            if is_finished {
                                active_build_reply_tx = None;
                            }
                        }
                    }
                    // A clean client close (`Ok(None)`) or the reader task ending
                    // (`None`) both mean the client is gone: tear down its remote
                    // shell (dropping `channel` on return closes it) rather than
                    // leaking a session (session cleanup).
                    Some(Ok(None)) | None => break,
                    // A truncated or malformed frame is a hard error, surfaced to
                    // `serve_clients` (which logs and contains it per-client).
                    Some(Err(e)) => return Err(anyhow!("isekai-ssh mux owner: reading a client frame failed: {e}")),
                    Some(Ok(Some(other))) => return Err(anyhow!("isekai-ssh mux owner: unexpected frame from client: {other:?}")),
                }
            }
            msg = channel.wait() => {
                match msg {
                    Some(russh::ChannelMsg::Data { data }) => {
                        write_frame(writer, &Frame::Stdout(data.to_vec())).await?;
                    }
                    Some(russh::ChannelMsg::ExtendedData { data, .. }) => {
                        write_frame(writer, &Frame::Stderr(data.to_vec())).await?;
                    }
                    Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                        exit_code = Some(exit_status as u8);
                    }
                    // Like the single-process loop, `Eof` is a no-op (an
                    // exit-status may still legally follow it, RFC 4254); only
                    // `Close`/`None` end the session.
                    Some(russh::ChannelMsg::Close) | None => break,
                    _ => {}
                }
            }
            ctl = recv_ctl_bytes(&mut ctl_frame_rx) => {
                match ctl {
                    // A ctl message this client received over its private
                    // forward: relay it as a `Frame::Ctl` on the same writer as
                    // stdout/stderr (so all owner→client writes stay ordered on
                    // one stream). Also covers `pump_to_frames`'s synthesized
                    // abort sentinel (Epic P Phase 2) — from this branch's
                    // perspective it's just another message to relay, but
                    // decoding it as `BuildFinished` here too keeps
                    // `active_build_reply_tx` from outliving a build that
                    // ended because the *remote* went away rather than
                    // because the client finished normally (see the
                    // client-frame `Frame::Ctl` arm above for that path).
                    Some(ctl_forward::CtlRelayEvent::Message(bytes)) => {
                        if matches!(
                            isekai_protocol::decode_ctl_message(&bytes),
                            Ok(isekai_protocol::CtlMessage::BuildFinished { .. })
                        ) {
                            active_build_reply_tx = None;
                        }
                        if write_frame(writer, &Frame::Ctl(bytes)).await.is_err() {
                            break;
                        }
                    }
                    // A `BuildRequest` was just relayed above (Epic P Phase 2):
                    // remember this reply channel so future client-originated
                    // `Frame::Ctl` frames get routed to the pump task that owns
                    // the real ctl-socket channel.
                    //
                    // If one is *already* active, this is a second, distinct
                    // remote `isekai-pipe ctl build` invocation for the same
                    // tab overlapping the first (`client.rs`'s one-build-per-
                    // tab guard only blocks the *client* from spawning a
                    // second build — it can't stop a second `BuildRequest`
                    // from being relayed in the first place, since that
                    // happens here, before the client ever sees it — review
                    // finding: an earlier version of this arm unconditionally
                    // overwrote `active_build_reply_tx`, which would have
                    // cross-wired the first build's real output into this
                    // second remote channel instead of rejecting it cleanly).
                    // Reject it on its own channel — via the very `reply_tx`
                    // just received, exactly like the "unknown profile" reply
                    // shape — rather than disturb the build already in
                    // flight. Dropping `reply_tx` afterward (implicit, since
                    // it's never stored) lets that second pump task's next
                    // `reply_rx.recv()` return `None` and end cleanly once
                    // it has relayed these two messages.
                    Some(ctl_forward::CtlRelayEvent::BuildStarted { reply_tx }) => {
                        if active_build_reply_tx.is_some() {
                            if let Ok(chunk) = crate::build_exec::encode_build_output_chunk(
                                isekai_protocol::BuildOutputStream::Stderr,
                                b"isekai-ssh: a build is already running for this tab\n".to_vec(),
                            ) {
                                let _ = reply_tx.send(chunk);
                            }
                            if let Ok(finished) = crate::build_exec::encode_build_finished(125, Vec::new()) {
                                let _ = reply_tx.send(finished);
                            }
                        } else {
                            active_build_reply_tx = Some(reply_tx);
                        }
                    }
                    // The ctl pump ended (forward cancelled / all senders gone):
                    // stop selecting this branch so it doesn't busy-loop.
                    None => ctl_frame_rx = None,
                }
            }
        }
    }

    // Report the session's end to the client. 255 stands in for "closed
    // without an exit status" (abnormal disconnect), matching the
    // single-process path's `NO_EXIT_STATUS_RECEIVED`. Best-effort: if the
    // client already vanished, there's no one to tell.
    let _ = write_frame(writer, &Frame::Exit(exit_code.unwrap_or(255))).await;
    Ok(())
}

/// `recv` on the optional ctl-frame channel, or a future that never resolves
/// when there is no ctl forward (so the `select!` branch is simply inert rather
/// than needing to be conditionally present).
async fn recv_ctl_bytes(rx: &mut Option<mpsc::UnboundedReceiver<ctl_forward::CtlRelayEvent>>) -> Option<ctl_forward::CtlRelayEvent> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use russh::server::{self, Auth, Msg as ServerMsg, Server as _, Session as ServerSession};
    use russh::{Channel as RusshChannel, CryptoVec};
    use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey as SshPrivateKey};
    use russh_stream_session::{
        authenticate_session, establish_over_stream, verifying_handler, verifying_handler_with_routes, Credential, HostKeyVerifier,
        VerifyOutcome,
    };
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    struct AcceptAllHostKeys;
    #[async_trait]
    impl HostKeyVerifier for AcceptAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> VerifyOutcome {
            VerifyOutcome::Accepted
        }
    }

    // -- wait_for_idle_exit -----------------------------------------------

    #[tokio::test]
    async fn wait_for_idle_exit_resolves_immediately_after_the_grace_when_already_zero() {
        let count = AtomicUsize::new(0);
        let notify = Notify::new();
        let start = tokio::time::Instant::now();
        wait_for_idle_exit(&count, &notify, Duration::from_millis(20)).await;
        assert!(start.elapsed() >= Duration::from_millis(20), "must actually wait out the grace period");
    }

    #[tokio::test]
    async fn wait_for_idle_exit_never_resolves_while_the_count_is_nonzero() {
        let count = AtomicUsize::new(1);
        let notify = Notify::new();
        let result = tokio::time::timeout(Duration::from_millis(100), wait_for_idle_exit(&count, &notify, Duration::from_millis(20))).await;
        assert!(result.is_err(), "must not resolve while a client is still counted as active");
    }

    #[tokio::test]
    async fn wait_for_idle_exit_restarts_the_grace_when_a_new_client_arrives_then_leaves_again() {
        let count = Arc::new(AtomicUsize::new(0));
        let notify = Arc::new(Notify::new());

        let count2 = count.clone();
        let notify2 = notify.clone();
        // Simulate: a new client connects shortly after the wait starts (resetting
        // the grace), then leaves again — the *second* zero-to-grace window is
        // what must actually elapse before the wait resolves.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            count2.fetch_add(1, Ordering::SeqCst);
            notify2.notify_one();
            tokio::time::sleep(Duration::from_millis(10)).await;
            count2.fetch_sub(1, Ordering::SeqCst);
            notify2.notify_one();
        });

        let start = tokio::time::Instant::now();
        wait_for_idle_exit(&count, &notify, Duration::from_millis(30)).await;
        // Must have waited past the point the client disconnected (~20ms) plus
        // its own grace window (30ms) — comfortably more than the naive
        // (wrong) "first grace window from t=0" would give (30ms).
        assert!(start.elapsed() >= Duration::from_millis(45), "a new client arriving must reset the idle-exit grace, not just be ignored");
    }

    // -- serve_clients idle-exit end-to-end (InMemoryChannel) ---------------

    async fn authed_test_handle() -> client::Handle<russh_stream_session::VerifyingHandler<AcceptAllHostKeys>> {
        let keypair = Ed25519Keypair::from_seed(&[150; 32]);
        let host_key = SshPrivateKey::from(keypair);
        let config = Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut server = EchoShellServer;
        tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });

        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let handler = verifying_handler(&verifier);
        let mut handle = establish_over_stream(Arc::new(client::Config::default()), stream, handler).await.unwrap();
        assert!(authenticate_session(&mut handle, "tester", &Credential::Password("x".to_string())).await.unwrap());
        handle
    }

    /// End-to-end: `serve_clients` must exit on its own (`Ok(())`) once a
    /// client has connected and then fully disconnected and the idle grace
    /// has elapsed — the `ControlPersist`-equivalent lifetime policy the
    /// detached holder process relies on to eventually let go of an
    /// authenticated connection nobody is using anymore.
    #[tokio::test]
    async fn serve_clients_exits_after_the_idle_grace_once_the_last_client_disconnects() {
        let name = "isekai-ssh-mux-idle-exit-test";
        let token = Arc::new(b"tok".to_vec());
        // The real TCP handshake against the mock sshd must run with real time
        // (russh's own handshake has timer-based internals that can misbehave
        // under a paused clock) — only pause *after* authentication completes,
        // for the idle-grace fast-forwarding below.
        let handle = Arc::new(Mutex::new(authed_test_handle().await));
        tokio::time::pause();

        let owner_channel = local_ipc_mux::InMemoryChannel::try_claim(name).await.unwrap();
        let serve_task = tokio::spawn(serve_clients(owner_channel, handle, token, None));

        // One client connects, sends Hello, then immediately disconnects
        // (drops without Shutdown) — `relay_client`'s per-client task ends
        // (successfully or not doesn't matter here) and decrements the count.
        {
            let conn = local_ipc_mux::InMemoryChannel::connect(name).await.unwrap();
            let (_r, mut w) = tokio::io::split(conn);
            write_frame(&mut w, &Frame::Hello { version: MUX_PROTOCOL_VERSION, token: b"tok".to_vec(), term: "xterm".to_string(), cols: 80, rows: 24 })
                .await
                .unwrap();
            // conn drops here.
        }
        // Let the spawned relay task actually run to completion (read Hello,
        // fail to write HelloAck to the now-dropped connection, decrement the
        // count) *before* advancing the clock — otherwise the idle-exit grace
        // sleep hasn't even started counting down yet when we jump the clock
        // past it, and the assertion below races against a grace window that
        // effectively restarts after the jump.
        tokio::task::yield_now().await;

        // Advance the paused clock past both the (short, since a client did
        // connect) idle grace and give spawned tasks a chance to run.
        tokio::time::advance(IDLE_GRACE + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        let result = tokio::time::timeout(Duration::from_secs(5), serve_task).await;
        assert!(result.is_ok(), "serve_clients must exit once idle past the grace window, not hang forever");
        assert!(result.unwrap().unwrap().is_ok(), "an idle-exit is a clean Ok(()), not an error");
    }

    /// If a second client connects while the accept loop is inside its
    /// post-disconnect grace window, the exit must *not* fire — the loop goes
    /// on serving instead of racing a shutdown against a legitimately new tab.
    #[tokio::test]
    async fn serve_clients_does_not_exit_if_a_new_client_arrives_during_the_grace_window() {
        let name = "isekai-ssh-mux-idle-exit-reset-test";
        let token = Arc::new(b"tok".to_vec());
        let handle = Arc::new(Mutex::new(authed_test_handle().await));
        tokio::time::pause();

        let owner_channel = local_ipc_mux::InMemoryChannel::try_claim(name).await.unwrap();
        let serve_task = tokio::spawn(serve_clients(owner_channel, handle, token, None));

        // First client connects then disconnects immediately.
        {
            let conn = local_ipc_mux::InMemoryChannel::connect(name).await.unwrap();
            let (_r, mut w) = tokio::io::split(conn);
            write_frame(&mut w, &Frame::Hello { version: MUX_PROTOCOL_VERSION, token: b"tok".to_vec(), term: "xterm".to_string(), cols: 80, rows: 24 }).await.unwrap();
        }
        tokio::task::yield_now().await;

        // Well within the idle grace, a second client connects and stays
        // connected (holds its Hello/HelloAck round-trip open).
        tokio::time::advance(Duration::from_secs(1)).await;
        let conn2 = local_ipc_mux::InMemoryChannel::connect(name).await.unwrap();
        let (mut r2, mut w2) = tokio::io::split(conn2);
        write_frame(&mut w2, &Frame::Hello { version: MUX_PROTOCOL_VERSION, token: b"tok".to_vec(), term: "xterm".to_string(), cols: 80, rows: 24 }).await.unwrap();
        match read_frame(&mut r2).await.unwrap().unwrap() {
            Frame::HelloAck { .. } => {}
            other => panic!("expected HelloAck, got {other:?}"),
        }

        // Advance well past what the *first* client's grace window would have
        // been — the loop must still be alive, serving the still-connected
        // second client, not exited.
        tokio::time::advance(IDLE_GRACE + Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert!(!serve_task.is_finished(), "a still-connected client must prevent the idle-exit from firing");

        drop(w2);
        serve_task.abort();
    }

    /// A mock sshd whose shell echoes back a fixed banner plus whatever stdin
    /// bytes it receives, so an owner→client relay test can prove real remote
    /// stdout flows back through an independent channel. Modeled on
    /// `native/connect.rs`'s own mock servers.
    #[derive(Clone)]
    struct EchoShellServer;

    impl server::Server for EchoShellServer {
        type Handler = EchoShellHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> EchoShellHandler {
            EchoShellHandler
        }
    }

    #[derive(Clone)]
    struct EchoShellHandler;

    #[async_trait]
    impl server::Handler for EchoShellHandler {
        type Error = russh::Error;

        async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_session(&mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession) -> Result<bool, Self::Error> {
            Ok(true)
        }

        async fn shell_request(&mut self, channel: russh::ChannelId, session: &mut ServerSession) -> Result<(), Self::Error> {
            session.data(channel, CryptoVec::from(b"shell-ready\n".to_vec()))?;
            Ok(())
        }

        // Echo stdin straight back as stdout so the client sees its input
        // relayed through the remote shell.
        async fn data(&mut self, channel: russh::ChannelId, data: &[u8], session: &mut ServerSession) -> Result<(), Self::Error> {
            session.data(channel, CryptoVec::from(data.to_vec()))?;
            Ok(())
        }
    }

    async fn spawn_echo_server(seed: u8) -> SocketAddr {
        let keypair = Ed25519Keypair::from_seed(&[seed; 32]);
        let host_key = SshPrivateKey::from(keypair);
        let config = Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut server = EchoShellServer;
        tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });
        addr
    }

    async fn authed_handle(addr: SocketAddr) -> client::Handle<russh_stream_session::VerifyingHandler<AcceptAllHostKeys>> {
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let config = Arc::new(client::Config::default());
        let handler = verifying_handler(&verifier);
        let mut handle = establish_over_stream(config, stream, handler).await.unwrap();
        let ok = authenticate_session(&mut handle, "tester", &Credential::Password("unused".to_string())).await.unwrap();
        assert!(ok, "EchoShellServer accepts any password");
        handle
    }

    /// End-to-end owner relay over an in-memory duplex standing in for the IPC
    /// connection: a well-formed Hello is accepted, a real shell channel opens
    /// against the mock sshd, the banner and echoed stdin come back as
    /// `Stdout` frames, and a clean `Shutdown` yields an `Exit` frame.
    #[tokio::test]
    async fn relay_client_opens_a_shell_and_relays_stdin_and_stdout() {
        let addr = spawn_echo_server(120).await;
        let handle = Mutex::new(authed_handle(addr).await);
        let token = b"correct-horse-battery-staple".to_vec();

        let (mut client, owner_side) = tokio::io::duplex(64 * 1024);
        let relay = tokio::spawn(async move { relay_client(owner_side, &handle, &token, None).await });

        // Client sends Hello, then some stdin, then Shutdown.
        write_frame(&mut client, &Frame::Hello { version: MUX_PROTOCOL_VERSION, token: b"correct-horse-battery-staple".to_vec(), term: "xterm".to_string(), cols: 80, rows: 24 }).await.unwrap();

        // First owner frame must be HelloAck.
        match read_frame(&mut client).await.unwrap().unwrap() {
            Frame::HelloAck { version } => assert_eq!(version, MUX_PROTOCOL_VERSION),
            other => panic!("expected HelloAck, got {other:?}"),
        }

        // Collect stdout frames until we've seen the banner and our echo.
        write_frame(&mut client, &Frame::Stdin(b"ping".to_vec())).await.unwrap();

        let mut seen = Vec::new();
        loop {
            match read_frame(&mut client).await.unwrap() {
                Some(Frame::Stdout(data)) => {
                    seen.extend_from_slice(&data);
                    if seen.windows(4).any(|w| w == b"ping") {
                        break;
                    }
                }
                Some(_) => {}
                None => panic!("stream ended before the echoed stdin arrived; saw {:?}", String::from_utf8_lossy(&seen)),
            }
        }
        assert!(seen.starts_with(b"shell-ready\n"), "the shell banner must be relayed as stdout");
        assert!(seen.windows(4).any(|w| w == b"ping"), "echoed stdin must come back as stdout");

        write_frame(&mut client, &Frame::Shutdown).await.unwrap();
        drop(client);
        let _ = relay.await.unwrap();
    }

    #[tokio::test]
    async fn relay_client_rejects_a_version_mismatch() {
        let addr = spawn_echo_server(121).await;
        let handle = Mutex::new(authed_handle(addr).await);
        let token = b"tok".to_vec();

        let (mut client, owner_side) = tokio::io::duplex(4096);
        let relay = tokio::spawn(async move { relay_client(owner_side, &handle, &token, None).await });

        write_frame(&mut client, &Frame::Hello { version: MUX_PROTOCOL_VERSION + 1, token: b"tok".to_vec(), term: "xterm".to_string(), cols: 80, rows: 24 }).await.unwrap();

        match read_frame(&mut client).await.unwrap().unwrap() {
            Frame::Rejected { reason } => assert!(reason.contains("version"), "reject reason should mention version, got {reason:?}"),
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert!(relay.await.unwrap().is_err(), "a version mismatch must fail the client relay");
    }

    #[tokio::test]
    async fn relay_client_rejects_a_bad_token() {
        let addr = spawn_echo_server(122).await;
        let handle = Mutex::new(authed_handle(addr).await);
        let token = b"the-real-token".to_vec();

        let (mut client, owner_side) = tokio::io::duplex(4096);
        let relay = tokio::spawn(async move { relay_client(owner_side, &handle, &token, None).await });

        write_frame(&mut client, &Frame::Hello { version: MUX_PROTOCOL_VERSION, token: b"a-wrong-token".to_vec(), term: "xterm".to_string(), cols: 80, rows: 24 }).await.unwrap();

        match read_frame(&mut client).await.unwrap().unwrap() {
            Frame::Rejected { reason } => assert!(reason.contains("token"), "reject reason should mention the token, got {reason:?}"),
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert!(relay.await.unwrap().is_err(), "a bad token must fail the client relay");
    }

    /// A client that sends a non-Hello frame first is refused before any shell
    /// channel is opened.
    #[tokio::test]
    async fn relay_client_requires_hello_first() {
        let addr = spawn_echo_server(123).await;
        let handle = Mutex::new(authed_handle(addr).await);
        let token = b"tok".to_vec();

        let (mut client, owner_side) = tokio::io::duplex(4096);
        let relay = tokio::spawn(async move { relay_client(owner_side, &handle, &token, None).await });

        write_frame(&mut client, &Frame::Stdin(b"no hello".to_vec())).await.unwrap();
        assert!(relay.await.unwrap().is_err(), "a missing Hello must fail the client relay");
    }

    /// A mock sshd that, when a client's per-tab `streamlocal_forward` is
    /// requested, pushes one ctl message (`SetTitle`) back over a
    /// `forwarded-streamlocal` channel — and answers the login-shell `exec`
    /// (ctl-socket opens the shell via pty+exec, not `shell_request`) with a
    /// banner. Lets the owner→client ctl relay be tested end-to-end.
    #[derive(Clone)]
    struct CtlPushShellServer;
    impl server::Server for CtlPushShellServer {
        type Handler = CtlPushShellHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> CtlPushShellHandler {
            CtlPushShellHandler
        }
    }
    #[derive(Clone)]
    struct CtlPushShellHandler;
    #[async_trait]
    impl server::Handler for CtlPushShellHandler {
        type Error = russh::Error;
        async fn auth_password(&mut self, _u: &str, _p: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }
        async fn channel_open_session(&mut self, _c: RusshChannel<ServerMsg>, _s: &mut ServerSession) -> Result<bool, Self::Error> {
            Ok(true)
        }
        async fn exec_request(&mut self, channel: russh::ChannelId, _data: &[u8], session: &mut ServerSession) -> Result<(), Self::Error> {
            session.data(channel, CryptoVec::from(b"login-ready\n".to_vec()))?;
            Ok(())
        }
        async fn streamlocal_forward(&mut self, socket_path: &str, session: &mut ServerSession) -> Result<bool, Self::Error> {
            let handle = session.handle();
            let path = socket_path.to_string();
            tokio::spawn(async move {
                if let Ok(channel) = handle.channel_open_forwarded_streamlocal(path.clone()).await {
                    // The real `isekai-pipe ctl` always sends the secret-preamble
                    // line (the socket path) before the message — see
                    // `ctl_forward.rs`'s `CtlPushServer` mock docs for why this
                    // matters (a preamble-less mock previously hid a real bug).
                    let _ = channel.data(format!("{path}\n").as_bytes()).await;
                    let _ = channel.data(&br#"{"op":"title","value":"tab-title"}"#[..]).await;
                    let _ = channel.data(&b"\n"[..]).await;
                    let _ = channel.eof().await;
                }
            });
            Ok(true)
        }
    }

    /// End-to-end for the mux-client ctl relay: with `ctl_routes` set, the owner
    /// requests a private forward for the client, the mock sshd pushes a
    /// `SetTitle` over it, and the owner must relay it to *this* client as a
    /// `Frame::Ctl` carrying the message's bytes (never onto stdout).
    #[tokio::test]
    async fn relay_client_relays_a_ctl_message_to_the_client_as_a_ctl_frame() {
        let keypair = Ed25519Keypair::from_seed(&[131; 32]);
        let host_key = SshPrivateKey::from(keypair);
        let config = Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut server = CtlPushShellServer;
        tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });

        // The handler must carry the same route table the owner registers the
        // client's forward into, so the pushed channel is delivered in-process.
        let routes = ForwardRoutes::new();
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let handler = verifying_handler_with_routes(&verifier, &routes);
        let mut handle = establish_over_stream(Arc::new(client::Config::default()), stream, handler).await.unwrap();
        assert!(authenticate_session(&mut handle, "tester", &Credential::Password("x".to_string())).await.unwrap());
        let handle = Mutex::new(handle);
        let token = b"tok".to_vec();

        let (mut client, owner_side) = tokio::io::duplex(64 * 1024);
        let relay = tokio::spawn(async move { relay_client(owner_side, &handle, &token, Some(&routes)).await });

        write_frame(&mut client, &Frame::Hello { version: MUX_PROTOCOL_VERSION, token: b"tok".to_vec(), term: "xterm".to_string(), cols: 80, rows: 24 })
            .await
            .unwrap();
        match read_frame(&mut client).await.unwrap().unwrap() {
            Frame::HelloAck { .. } => {}
            other => panic!("expected HelloAck, got {other:?}"),
        }

        // Read frames until the relayed ctl message arrives; its bytes must
        // decode to the SetTitle the mock sshd pushed, and it must never come
        // through as stdout.
        let ctl_bytes = loop {
            match read_frame(&mut client).await.unwrap() {
                Some(Frame::Ctl(bytes)) => break bytes,
                Some(Frame::Stdout(data)) => assert!(
                    !data.windows(3).any(|w| w == b"tab"),
                    "a ctl message must never be relayed as stdout"
                ),
                Some(_) => {}
                None => panic!("the owner closed before relaying the ctl message"),
            }
        };
        let msg = isekai_protocol::decode_ctl_message(&ctl_bytes).expect("the relayed ctl bytes must decode");
        assert_eq!(msg, isekai_protocol::CtlMessage::SetTitle { value: "tab-title".to_string() });

        drop(client);
        let _ = relay.await.unwrap();
    }

    /// A mock sshd that, on `streamlocal_forward`, opens a
    /// `forwarded-streamlocal` channel, sends a `BuildRequest` over it, and
    /// hands the *channel object itself* out via `channel_tx` — rather than
    /// writing more to it and closing, like `CtlPushShellServer` does —
    /// so the test can keep it open and observe whatever the owner relays
    /// back onto it afterward (Epic P Phase 2's client→owner direction).
    #[derive(Clone)]
    struct CtlBuildForwardServer {
        channel_tx: mpsc::UnboundedSender<RusshChannel<ServerMsg>>,
    }
    impl server::Server for CtlBuildForwardServer {
        type Handler = CtlBuildForwardHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> CtlBuildForwardHandler {
            CtlBuildForwardHandler(self.channel_tx.clone())
        }
    }
    #[derive(Clone)]
    struct CtlBuildForwardHandler(mpsc::UnboundedSender<RusshChannel<ServerMsg>>);
    #[async_trait]
    impl server::Handler for CtlBuildForwardHandler {
        type Error = russh::Error;
        async fn auth_password(&mut self, _u: &str, _p: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }
        async fn channel_open_session(&mut self, _c: RusshChannel<ServerMsg>, _s: &mut ServerSession) -> Result<bool, Self::Error> {
            Ok(true)
        }
        async fn exec_request(&mut self, channel: russh::ChannelId, _data: &[u8], session: &mut ServerSession) -> Result<(), Self::Error> {
            session.data(channel, CryptoVec::from(b"login-ready\n".to_vec()))?;
            Ok(())
        }
        async fn streamlocal_forward(&mut self, socket_path: &str, session: &mut ServerSession) -> Result<bool, Self::Error> {
            let handle = session.handle();
            let path = socket_path.to_string();
            let tx = self.0.clone();
            tokio::spawn(async move {
                if let Ok(channel) = handle.channel_open_forwarded_streamlocal(path.clone()).await {
                    let _ = channel.data(format!("{path}\n").as_bytes()).await;
                    let _ = channel.data(&br#"{"op":"build_request","profile":"t"}"#[..]).await;
                    let _ = channel.data(&b"\n"[..]).await;
                    let _ = tx.send(channel);
                }
            });
            Ok(true)
        }
    }

    /// Sets up the same owner/mock-sshd/mux-client harness as
    /// `relay_client_relays_a_ctl_message_to_the_client_as_a_ctl_frame`, but
    /// with `CtlBuildForwardServer` (which sends a `BuildRequest` and keeps
    /// its channel open) instead. Returns the driving pieces so each test
    /// can read/write frames on the mux-client duplex and, separately,
    /// observe what the owner relays onto the real (mock-remote) channel.
    async fn build_relay_harness() -> (
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<Result<()>>,
        mpsc::UnboundedReceiver<RusshChannel<ServerMsg>>,
    ) {
        let keypair = Ed25519Keypair::from_seed(&[137; 32]);
        let host_key = SshPrivateKey::from(keypair);
        let config = Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (channel_tx, channel_rx) = mpsc::unbounded_channel();
        let mut server = CtlBuildForwardServer { channel_tx };
        tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });

        let routes = ForwardRoutes::new();
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let handler = verifying_handler_with_routes(&verifier, &routes);
        let mut handle = establish_over_stream(Arc::new(client::Config::default()), stream, handler).await.unwrap();
        assert!(authenticate_session(&mut handle, "tester", &Credential::Password("x".to_string())).await.unwrap());
        let handle = Mutex::new(handle);
        let token = b"tok".to_vec();

        let (mut client, owner_side) = tokio::io::duplex(64 * 1024);
        let relay = tokio::spawn(async move { relay_client(owner_side, &handle, &token, Some(&routes)).await });

        write_frame(&mut client, &Frame::Hello { version: MUX_PROTOCOL_VERSION, token: b"tok".to_vec(), term: "xterm".to_string(), cols: 80, rows: 24 })
            .await
            .unwrap();
        match read_frame(&mut client).await.unwrap().unwrap() {
            Frame::HelloAck { .. } => {}
            other => panic!("expected HelloAck, got {other:?}"),
        }

        (client, relay, channel_rx)
    }

    /// Reads frames off `client` until a `Frame::Ctl` arrives, tolerating
    /// (and discarding) the mock shell's own `Stdout` banner in between —
    /// the shell channel and the ctl pump relay onto the same client
    /// connection, so their relative arrival order isn't guaranteed.
    async fn recv_ctl_frame(client: &mut tokio::io::DuplexStream) -> Vec<u8> {
        loop {
            match read_frame(client).await.unwrap() {
                Some(Frame::Ctl(bytes)) => return bytes,
                Some(_) => {}
                None => panic!("the owner closed before a ctl frame arrived"),
            }
        }
    }

    /// The bidirectional half of Epic P Phase 2: a mux client's own
    /// `Frame::Ctl` reply (standing in for `client.rs::spawn_client_build`'s
    /// real output) must be routed by `relay_loop` onto the *same* real ctl
    /// channel the `BuildRequest` arrived on — proving the
    /// `CtlRelayEvent::BuildStarted`/`active_build_reply_tx` plumbing
    /// actually connects the two directions, not just that each direction
    /// works in isolation.
    #[tokio::test]
    async fn relay_client_routes_a_build_reply_from_the_client_onto_the_real_ctl_channel() {
        use tokio::time::{timeout, Duration};

        let (mut client, relay, mut channel_rx) = build_relay_harness().await;

        // First ctl frame in must be the `BuildRequest` itself, relayed verbatim.
        let request_bytes = recv_ctl_frame(&mut client).await;
        assert_eq!(
            isekai_protocol::decode_ctl_message(&request_bytes).unwrap(),
            isekai_protocol::CtlMessage::BuildRequest { profile: "t".to_string() }
        );

        // The "client" (this test, standing in for `client.rs`) sends back a
        // build reply exactly the way `spawn_client_build` would.
        let finished = crate::build_exec::encode_build_finished(3, vec!["out.bin".to_string()]).unwrap();
        write_frame(&mut client, &Frame::Ctl(finished.clone())).await.unwrap();

        // The owner must have routed it onto the real channel the mock sshd
        // is still holding — not dropped it, not looped it back to the client.
        let mut server_channel = timeout(Duration::from_secs(5), channel_rx.recv())
            .await
            .expect("the mock sshd's channel should have been captured")
            .unwrap();
        match server_channel.wait().await {
            Some(russh::ChannelMsg::Data { data }) => assert_eq!(data.as_ref(), finished.as_slice()),
            other => panic!("expected the relayed build reply as Data, got {other:?}"),
        }

        drop(client);
        let _ = relay.await.unwrap();
    }

    /// The remote side of the ctl channel going away mid-build (the mock
    /// sshd drops its channel without ever eof/closing cleanly) must reach
    /// the mux client as a synthesized `BuildFinished` carrying
    /// `build_relay::BUILD_ABORTED_SENTINEL` — the signal `client.rs` uses to
    /// kill its still-running child instead of streaming into a channel
    /// nobody is reading from anymore.
    #[tokio::test]
    async fn relay_client_synthesizes_an_abort_sentinel_when_the_remote_channel_closes_mid_build() {
        let (mut client, relay, mut channel_rx) = build_relay_harness().await;

        // Consume the BuildRequest relay, then let the mock sshd's channel
        // drop (simulating the remote disconnecting mid-build) without ever
        // sending a reply.
        let request_bytes = recv_ctl_frame(&mut client).await;
        assert_eq!(
            isekai_protocol::decode_ctl_message(&request_bytes).unwrap(),
            isekai_protocol::CtlMessage::BuildRequest { profile: "t".to_string() }
        );
        let server_channel = channel_rx.recv().await.unwrap();
        let _ = server_channel.close().await;
        drop(server_channel);

        let abort_bytes = recv_ctl_frame(&mut client).await;
        assert_eq!(
            isekai_protocol::decode_ctl_message(&abort_bytes).unwrap(),
            isekai_protocol::CtlMessage::BuildFinished {
                exit_code: super::super::build_relay::BUILD_ABORTED_SENTINEL,
                result_paths: Vec::new(),
            }
        );

        drop(client);
        let _ = relay.await.unwrap();
    }

    /// A mock sshd that, on `streamlocal_forward`, opens *two* independent
    /// `forwarded-streamlocal` channels in quick succession — each sending
    /// its own `BuildRequest` — standing in for two separate remote
    /// `isekai-pipe ctl build` invocations for the same tab that happen to
    /// overlap (the second started before the first finished). A short
    /// delay between the two keeps the first's `BuildStarted` registration
    /// deterministically ahead of the second's in this test.
    #[derive(Clone)]
    struct OverlappingCtlBuildForwardServer {
        channel_tx: mpsc::UnboundedSender<RusshChannel<ServerMsg>>,
    }
    impl server::Server for OverlappingCtlBuildForwardServer {
        type Handler = OverlappingCtlBuildForwardHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> OverlappingCtlBuildForwardHandler {
            OverlappingCtlBuildForwardHandler(self.channel_tx.clone())
        }
    }
    #[derive(Clone)]
    struct OverlappingCtlBuildForwardHandler(mpsc::UnboundedSender<RusshChannel<ServerMsg>>);
    #[async_trait]
    impl server::Handler for OverlappingCtlBuildForwardHandler {
        type Error = russh::Error;
        async fn auth_password(&mut self, _u: &str, _p: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }
        async fn channel_open_session(&mut self, _c: RusshChannel<ServerMsg>, _s: &mut ServerSession) -> Result<bool, Self::Error> {
            Ok(true)
        }
        async fn exec_request(&mut self, channel: russh::ChannelId, _data: &[u8], session: &mut ServerSession) -> Result<(), Self::Error> {
            session.data(channel, CryptoVec::from(b"login-ready\n".to_vec()))?;
            Ok(())
        }
        async fn streamlocal_forward(&mut self, socket_path: &str, session: &mut ServerSession) -> Result<bool, Self::Error> {
            let handle = session.handle();
            let path = socket_path.to_string();
            let tx = self.0.clone();
            tokio::spawn(async move {
                for profile in ["a", "b"] {
                    if let Ok(channel) = handle.channel_open_forwarded_streamlocal(path.clone()).await {
                        let _ = channel.data(format!("{path}\n").as_bytes()).await;
                        let _ = channel.data(format!(r#"{{"op":"build_request","profile":"{profile}"}}"#).as_bytes()).await;
                        let _ = channel.data(&b"\n"[..]).await;
                        let _ = tx.send(channel);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            });
            Ok(true)
        }
    }

    /// Regression for a review finding: an earlier version of the
    /// `BuildStarted` handling in `relay_loop` unconditionally overwrote
    /// `active_build_reply_tx`, which would have cross-wired a first build's
    /// real output into a second, unrelated remote channel instead of
    /// rejecting the second cleanly. A second, distinct remote
    /// `isekai-pipe ctl build` invocation overlapping a first must instead
    /// be rejected *on its own channel* (a clear stderr message +
    /// `BuildFinished{exit_code: 125}`), leaving the first build's routing
    /// untouched.
    #[tokio::test]
    async fn relay_client_rejects_a_second_overlapping_build_request_without_disturbing_the_first() {
        use tokio::time::{timeout, Duration};

        let keypair = Ed25519Keypair::from_seed(&[149; 32]);
        let host_key = SshPrivateKey::from(keypair);
        let config = Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (channel_tx, mut channel_rx) = mpsc::unbounded_channel();
        let mut server = OverlappingCtlBuildForwardServer { channel_tx };
        tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });

        let routes = ForwardRoutes::new();
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let handler = verifying_handler_with_routes(&verifier, &routes);
        let mut handle = establish_over_stream(Arc::new(client::Config::default()), stream, handler).await.unwrap();
        assert!(authenticate_session(&mut handle, "tester", &Credential::Password("x".to_string())).await.unwrap());
        let handle = Mutex::new(handle);
        let token = b"tok".to_vec();

        let (mut client, owner_side) = tokio::io::duplex(64 * 1024);
        let relay = tokio::spawn(async move { relay_client(owner_side, &handle, &token, Some(&routes)).await });

        write_frame(&mut client, &Frame::Hello { version: MUX_PROTOCOL_VERSION, token: b"tok".to_vec(), term: "xterm".to_string(), cols: 80, rows: 24 })
            .await
            .unwrap();
        match read_frame(&mut client).await.unwrap().unwrap() {
            Frame::HelloAck { .. } => {}
            other => panic!("expected HelloAck, got {other:?}"),
        }

        // Both BuildRequests get relayed to the client regardless of the
        // owner's own bookkeeping below (the client independently ignores
        // the second one via its own one-build-per-tab guard) — drain past
        // whichever non-Ctl frames (e.g. the mock shell's banner) show up
        // first.
        let request_a = recv_ctl_frame(&mut client).await;
        assert_eq!(
            isekai_protocol::decode_ctl_message(&request_a).unwrap(),
            isekai_protocol::CtlMessage::BuildRequest { profile: "a".to_string() }
        );
        let request_b = recv_ctl_frame(&mut client).await;
        assert_eq!(
            isekai_protocol::decode_ctl_message(&request_b).unwrap(),
            isekai_protocol::CtlMessage::BuildRequest { profile: "b".to_string() }
        );

        // Channel "a" (registered first) must NOT receive the rejection —
        // it stays untouched, waiting for a real reply that never comes in
        // this test (proving the owner didn't route anything into it).
        let mut channel_a = timeout(Duration::from_secs(5), channel_rx.recv()).await.unwrap().unwrap();
        // Channel "b" (the overlapping second request) must receive the
        // rejection on its own channel.
        let mut channel_b = timeout(Duration::from_secs(5), channel_rx.recv()).await.unwrap().unwrap();

        let rejection_stderr = match channel_b.wait().await {
            Some(russh::ChannelMsg::Data { data }) => {
                let msg = isekai_protocol::decode_ctl_message(&data).unwrap();
                match msg {
                    isekai_protocol::CtlMessage::BuildOutputChunk { stream, data_b64 } => {
                        assert_eq!(stream, isekai_protocol::BuildOutputStream::Stderr);
                        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64).unwrap()
                    }
                    other => panic!("expected a rejection BuildOutputChunk on channel b, got {other:?}"),
                }
            }
            other => panic!("expected Data on channel b, got {other:?}"),
        };
        assert!(String::from_utf8_lossy(&rejection_stderr).contains("already running"));
        match channel_b.wait().await {
            Some(russh::ChannelMsg::Data { data }) => {
                assert_eq!(
                    isekai_protocol::decode_ctl_message(&data).unwrap(),
                    isekai_protocol::CtlMessage::BuildFinished { exit_code: 125, result_paths: Vec::new() }
                );
            }
            other => panic!("expected the rejection BuildFinished on channel b, got {other:?}"),
        }

        // Channel "a" must still be alive and untouched by the rejection —
        // give the owner a moment to (not) write anything to it, then
        // confirm nothing arrived.
        let saw_nothing = timeout(Duration::from_millis(200), channel_a.wait()).await;
        assert!(saw_nothing.is_err(), "channel a must not receive anything as a side effect of rejecting channel b");

        drop(client);
        let _ = relay.await.unwrap();
    }

    /// A mock sshd that accepts a `streamlocal_forward` (so the owner's per-tab
    /// ctl forward is successfully requested) but *rejects every session channel
    /// open* (so opening the ctl-socket login shell fails afterwards). It records
    /// each `cancel_streamlocal_forward` it receives, letting the test assert the
    /// owner tears the forward down on the failed-open path (no leak).
    #[derive(Clone)]
    struct RejectShellServer {
        cancelled_tx: mpsc::UnboundedSender<String>,
    }
    impl server::Server for RejectShellServer {
        type Handler = RejectShellHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> RejectShellHandler {
            RejectShellHandler { cancelled_tx: self.cancelled_tx.clone() }
        }
    }
    #[derive(Clone)]
    struct RejectShellHandler {
        cancelled_tx: mpsc::UnboundedSender<String>,
    }
    #[async_trait]
    impl server::Handler for RejectShellHandler {
        type Error = russh::Error;
        async fn auth_password(&mut self, _u: &str, _p: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }
        // Reject every session channel open, so the ctl-socket login shell the
        // owner tries to open (after the forward is already set up) fails.
        async fn channel_open_session(&mut self, _c: RusshChannel<ServerMsg>, _s: &mut ServerSession) -> Result<bool, Self::Error> {
            Ok(false)
        }
        async fn streamlocal_forward(&mut self, _socket_path: &str, _session: &mut ServerSession) -> Result<bool, Self::Error> {
            Ok(true)
        }
        async fn cancel_streamlocal_forward(&mut self, socket_path: &str, _session: &mut ServerSession) -> Result<bool, Self::Error> {
            let _ = self.cancelled_tx.send(socket_path.to_string());
            Ok(true)
        }
    }

    /// Regression for the ctl-forward leak: when the ctl-socket login shell fails
    /// to open *after* the per-tab forward was already requested, the owner must
    /// cancel that forward before returning the error — otherwise the remote
    /// streamlocal forward (and its local route) leak for the life of the owner.
    #[tokio::test]
    async fn relay_client_cancels_the_ctl_forward_when_the_login_shell_fails_to_open() {
        use tokio::time::{timeout, Duration};

        let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel::<String>();

        let keypair = Ed25519Keypair::from_seed(&[140; 32]);
        let host_key = SshPrivateKey::from(keypair);
        let config = Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut server = RejectShellServer { cancelled_tx };
        tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });

        // The handler must share the owner's route table so the requested
        // forward is registered/cancelled against the same map.
        let routes = ForwardRoutes::new();
        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let handler = verifying_handler_with_routes(&verifier, &routes);
        let mut handle = establish_over_stream(Arc::new(client::Config::default()), stream, handler).await.unwrap();
        assert!(authenticate_session(&mut handle, "tester", &Credential::Password("x".to_string())).await.unwrap());
        let handle = Mutex::new(handle);
        let token = b"tok".to_vec();

        let (mut client, owner_side) = tokio::io::duplex(64 * 1024);
        let relay = tokio::spawn(async move { relay_client(owner_side, &handle, &token, Some(&routes)).await });

        write_frame(&mut client, &Frame::Hello { version: MUX_PROTOCOL_VERSION, token: b"tok".to_vec(), term: "xterm".to_string(), cols: 80, rows: 24 })
            .await
            .unwrap();
        match read_frame(&mut client).await.unwrap().unwrap() {
            Frame::HelloAck { .. } => {}
            other => panic!("expected HelloAck, got {other:?}"),
        }

        // The forward the owner requested must be cancelled once the login-shell
        // open fails — the server observes a cancel for this client's ctl path.
        let cancelled_path = timeout(Duration::from_secs(5), cancelled_rx.recv())
            .await
            .expect("the ctl forward must be cancelled after the failed login-shell open (no leak)")
            .expect("the cancel sender must still be live");
        assert!(
            cancelled_path.starts_with(crate::ctl_forward::REMOTE_SOCK_PREFIX),
            "the cancelled forward must be this client's own ctl socket, got {cancelled_path:?}"
        );

        assert!(relay.await.unwrap().is_err(), "a failed ctl-socket login-shell open must fail the client relay");
    }
}
