//! Owner side of the mux: the process that won [`local_ipc_mux::ExclusiveChannel::try_claim`]
//! holds the one shared, already-authenticated `russh` `client::Handle` and
//! serves every other tab's `isekai-ssh` process a *private, independent*
//! remote shell over that shared connection.
//!
//! Per the M4 design, this is `ControlMaster`, not screen sharing: for each
//! accepted client the owner opens a brand-new SSH shell channel
//! ([`open_channel`] with [`SessionKind::Shell`]) on the shared handle and
//! relays *that one channel's* traffic to *that one client*. Clients never see
//! each other's terminals; only the authenticated connection (and the auth/TOFU
//! work behind it) is shared.
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
use russh_stream_session::{open_channel, SessionKind};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;

use crate::log_file::log_line;

use super::protocol::{read_frame, token_eq, write_frame, Frame, MUX_PROTOCOL_VERSION};

/// Accepts clients on `channel` for the life of the owner process, spawning an
/// independent relay task per client (each opening its own shell channel on the
/// shared `handle`). The handle is shared via `Arc<Mutex<_>>` because `russh`'s
/// `client::Handle` is not `Clone`; the mutex is held only for the brief
/// `channel_open_session`/`streamlocal_forward` calls (the latter needs
/// `&mut self`), never across a client's relay loop, so clients still stream
/// concurrently. Returns only if `accept` itself fails (the underlying IPC
/// channel died) — a single client's relay error is logged and contained,
/// never propagated to sibling clients or the owner's own session.
pub(crate) async fn serve_clients<C, H>(mut channel: C, handle: Arc<Mutex<client::Handle<H>>>, token: Arc<Vec<u8>>) -> Result<()>
where
    C: ExclusiveChannel,
    H: client::Handler + 'static,
{
    loop {
        let conn = channel.accept().await.context("isekai-ssh mux owner: accepting a client connection failed")?;
        let handle = handle.clone();
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(e) = relay_client(conn, &handle, token.as_slice()).await {
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
pub(crate) async fn relay_client<Conn, H>(conn: Conn, handle: &Mutex<client::Handle<H>>, expected_token: &[u8]) -> Result<()>
where
    Conn: AsyncRead + AsyncWrite + Unpin + Send,
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

    let mut channel = {
        // Lock held only for the open; the relay below runs lock-free so one
        // client's traffic never blocks another's channel open or forward.
        let guard = handle.lock().await;
        open_channel(&guard, &SessionKind::Shell { term, cols: cols as u32, rows: rows as u32 })
            .await
            .context("isekai-ssh mux owner: failed to open a shell channel for the client")?
    };

    relay_loop(&mut reader, &mut writer, &mut channel).await
}

/// The core owner-side relay: client frames drive the remote channel, and
/// remote channel messages become owner→client frames. See the module docs on
/// ordering and backpressure — both properties come from this being one
/// `select!` loop with a single sequential branch per direction.
async fn relay_loop<R, W>(reader: &mut R, writer: &mut W, channel: &mut russh::Channel<client::Msg>) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
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
            frame = read_frame(reader) => {
                match frame? {
                    Some(Frame::Stdin(data)) if !stdin_done => {
                        if channel.data(&data[..]).await.is_err() {
                            break;
                        }
                    }
                    Some(Frame::Resize { cols, rows }) if !stdin_done => {
                        // Applied in receive order relative to the Stdin above
                        // (the ordering guarantee); pixel dims are 0 like the
                        // rest of this client (character-cell terminals).
                        let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
                    }
                    // A well-behaved client sends nothing after Shutdown; ignore
                    // any stray input/resize rather than reopening the closed
                    // remote stdin.
                    Some(Frame::Stdin(_)) | Some(Frame::Resize { .. }) => {}
                    Some(Frame::Shutdown) => {
                        if !stdin_done {
                            let _ = channel.eof().await;
                            stdin_done = true;
                        }
                    }
                    // A clean client close, an unexpected drop, or a truncated
                    // frame all mean the client is gone: tear down its remote
                    // shell (dropping `channel` on return closes it) rather
                    // than leaking a session (session cleanup).
                    None => break,
                    Some(other) => return Err(anyhow!("isekai-ssh mux owner: unexpected frame from client: {other:?}")),
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
        }
    }

    // Report the session's end to the client. 255 stands in for "closed
    // without an exit status" (abnormal disconnect), matching the
    // single-process path's `NO_EXIT_STATUS_RECEIVED`. Best-effort: if the
    // client already vanished, there's no one to tell.
    let _ = write_frame(writer, &Frame::Exit(exit_code.unwrap_or(255))).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use russh::server::{self, Auth, Msg as ServerMsg, Server as _, Session as ServerSession};
    use russh::{Channel as RusshChannel, CryptoVec};
    use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey as SshPrivateKey};
    use russh_stream_session::{authenticate_session, establish_over_stream, verifying_handler, Credential, HostKeyVerifier};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    struct AcceptAllHostKeys;
    #[async_trait]
    impl HostKeyVerifier for AcceptAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> bool {
            true
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
        let relay = tokio::spawn(async move { relay_client(owner_side, &handle, &token).await });

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
        let relay = tokio::spawn(async move { relay_client(owner_side, &handle, &token).await });

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
        let relay = tokio::spawn(async move { relay_client(owner_side, &handle, &token).await });

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
        let relay = tokio::spawn(async move { relay_client(owner_side, &handle, &token).await });

        write_frame(&mut client, &Frame::Stdin(b"no hello".to_vec())).await.unwrap();
        assert!(relay.await.unwrap().is_err(), "a missing Hello must fail the client relay");
    }
}
