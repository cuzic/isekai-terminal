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
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, Mutex};

use crate::log_file::log_line;

use super::ctl_forward;
use super::protocol::{read_frame, spawn_frame_reader, token_eq, write_frame, Frame, MUX_PROTOCOL_VERSION};

/// Accepts clients on `channel` for the life of the owner process, spawning an
/// independent relay task per client (each opening its own shell channel on the
/// shared `handle`). The handle is shared via `Arc<Mutex<_>>` because `russh`'s
/// `client::Handle` is not `Clone`; the mutex is held only for the brief
/// `channel_open_session`/`streamlocal_forward` calls (the latter needs
/// `&mut self`), never across a client's relay loop, so clients still stream
/// concurrently. Returns only if `accept` itself fails (the underlying IPC
/// channel died) — a single client's relay error is logged and contained,
/// never propagated to sibling clients or the owner's own session.
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
    loop {
        let conn = channel.accept().await.context("isekai-ssh mux owner: accepting a client connection failed")?;
        let handle = handle.clone();
        let token = token.clone();
        // `Some` iff `#@isekai ctl-socket` is on — each client then gets its
        // own private per-tab forward (see [`relay_client`]).
        let ctl_routes = ctl_routes.clone();
        tokio::spawn(async move {
            if let Err(e) = relay_client(conn, &handle, token.as_slice(), ctl_routes.as_ref()).await {
                // One client's session ending badly must not disturb the
                // owner or its other clients (session isolation).
                log_line!("isekai-ssh mux owner: a client session ended with an error: {e:#}");
            }
        });
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
            None => open_channel(&guard, &SessionKind::Shell { term, cols: cols as u32, rows: rows as u32 })
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
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
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
    mut ctl_frame_rx: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
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
                    // one stream).
                    Some(bytes) => {
                        if write_frame(writer, &Frame::Ctl(bytes)).await.is_err() {
                            break;
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
async fn recv_ctl_bytes(rx: &mut Option<mpsc::UnboundedReceiver<Vec<u8>>>) -> Option<Vec<u8>> {
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
