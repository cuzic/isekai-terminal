//! Windows-native `#@isekai ctl-socket` control-plane (`ISEKAI_PIPE_DESIGN.md`
//! §8 Epic M) wiring for the `russh`-based path — the native counterpart of
//! the Unix `ssh(1)` `-R` bridge in [`crate::ctl_forward`].
//!
//! On Unix, `isekai-ssh` shells out to `ssh(1)`, which can only deliver a
//! remote UNIX-socket forward (`-R`) to a *local* socket/port `isekai-ssh`
//! then listens on. The native path *is* the SSH client, so it instead:
//!
//! 1. requests the streamlocal forward directly on its own `client::Handle`
//!    (`streamlocal_forward(remote_path)` — needs `&mut self`, hence the
//!    shared handle is behind a [`Mutex`](tokio::sync::Mutex)),
//! 2. registers a [`ForwardRoutes`] route for that path, so the handler
//!    (`russh_stream_session::VerifyingHandler`) hands each server-initiated
//!    `forwarded-streamlocal` channel straight to us in-process — no local
//!    socket bridge, and no TCP-port + 128-bit-token access control, because
//!    the forwarded `Channel` is an SSH-protocol object no other local process
//!    can connect to, and
//! 3. opens the interactive login shell with `ISEKAI_CTL_SOCK` exported (the
//!    same `export ...; exec "$SHELL" -i -l` replacement the Unix path uses,
//!    so the remote `isekai-pipe ctl` finds the forward), reusing
//!    [`crate::ctl_forward`]'s pure helpers rather than duplicating them.
//!
//! The forwarded channel carries exactly one `isekai_protocol::CtlMessage`
//! line (the `isekai-pipe ctl` contract). For the owner's own / single-process
//! foreground shell the message is applied directly as an OSC escape on this
//! process's stderr ([`pump_to_stderr`]); for a *mux client's* session the
//! owner instead relays the raw bytes to that client as a
//! [`Frame::Ctl`](super::protocol::Frame::Ctl), which the client applies to
//! *its own* terminal ([`pump_to_frames`] + `client::run_inner`).
//!
//! Everything here is opportunistic (`ISEKAI_PIPE_DESIGN.md` Epic M): a forward
//! that can't be established, or a malformed ctl message, is logged/ignored and
//! never fails the SSH session.

use russh::client;
use russh_stream_session::ForwardRoutes;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

use crate::log_file::log_line;
use crate::wrapper::{WrapperPlan, WrapperResolution};

/// A live per-tab ctl-socket forward: the remote socket path the shell must
/// export as `$ISEKAI_CTL_SOCK`, and the receiver of forwarded ctl channels.
pub(crate) struct CtlForward {
    pub(crate) remote_path: String,
    pub(crate) channels: mpsc::UnboundedReceiver<russh::Channel<client::Msg>>,
}

/// Whether this native invocation should set up a ctl-socket forward — i.e.
/// `#@isekai ctl-socket yes` is set and the session is interactive (no trailing
/// remote command). Reuses the exact predicate the Unix path uses.
pub(crate) fn should_forward(plan: &WrapperPlan, resolution: &WrapperResolution) -> bool {
    crate::ctl_forward::should_attempt_ctl_forward(
        resolution.ctl_socket_enabled(),
        plan.ssh_args_len(),
        plan.destination_index(),
    )
}

/// Requests a fresh per-tab streamlocal forward on the shared handle and
/// registers its route in `routes`. Returns the forward on success; on any
/// failure logs and returns `None` (opportunistic — never fails the session).
/// The mutex is held only for the brief `streamlocal_forward` request.
pub(crate) async fn request<H: client::Handler>(
    handle: &Mutex<client::Handle<H>>,
    routes: &ForwardRoutes,
) -> Option<CtlForward> {
    let remote_path = format!("{}{}.sock", crate::ctl_forward::REMOTE_SOCK_PREFIX, crate::ctl_forward::new_ctl_token());
    let channels = routes.register(&remote_path);
    let result = {
        let mut guard = handle.lock().await;
        guard.streamlocal_forward(remote_path.clone()).await
    };
    if let Err(e) = result {
        routes.unregister(&remote_path);
        log_line!("isekai-ssh: ctl-socket forward unavailable, continuing without it: {e}");
        return None;
    }
    Some(CtlForward { remote_path, channels })
}

/// Best-effort teardown when a session ends: cancels the remote forward and
/// drops the local route so a late channel is closed rather than routed. The
/// mutex is held only for the brief `cancel_streamlocal_forward` request.
pub(crate) async fn cancel<H: client::Handler>(handle: &Mutex<client::Handle<H>>, routes: &ForwardRoutes, remote_path: &str) {
    let _ = handle.lock().await.cancel_streamlocal_forward(remote_path.to_string()).await;
    routes.unregister(remote_path);
}

/// Opens an interactive login-shell channel that also exports
/// `$ISEKAI_CTL_SOCK=<remote_path>`, so the remote `isekai-pipe ctl` can find
/// this tab's forward. Uses a PTY + `exec` of the same
/// `export ...; exec "$SHELL" -i -l` replacement command the Unix `-R` path
/// hands `sshd`, rather than a plain `request_shell`, because `SetEnv`/env
/// requests would need a remote `sshd_config` opt-in most users don't control.
pub(crate) async fn open_login_shell<H: client::Handler>(
    handle: &client::Handle<H>,
    term: &str,
    cols: u32,
    rows: u32,
    remote_path: &str,
) -> Result<russh::Channel<client::Msg>, russh::Error> {
    let channel = handle.channel_open_session().await?;
    channel.request_pty(false, term, cols, rows, 0, 0, &[]).await?;
    let command = format!("export ISEKAI_CTL_SOCK={remote_path:?}; exec \"${{SHELL:-/bin/sh}}\" -i -l");
    channel.exec(false, command.as_str()).await?;
    Ok(channel)
}

/// Consumes forwarded ctl channels and applies each message to *this* process's
/// terminal as an OSC escape on stderr — the owner's own / single-process
/// foreground shell direction. Runs until the route's sender is dropped (the
/// forward is cancelled / the session ends).
pub(crate) async fn pump_to_stderr(mut channels: mpsc::UnboundedReceiver<russh::Channel<client::Msg>>) {
    while let Some(mut channel) = channels.recv().await {
        tokio::spawn(async move {
            let Some(line) = read_ctl_line(&mut channel).await else {
                return;
            };
            if let Ok(msg) = isekai_protocol::decode_ctl_message(&line) {
                if let Some(seq) = crate::ctl_forward::osc_sequence_for(&msg) {
                    let _ = crate::ctl_forward::emit_osc(&seq);
                }
            }
        });
    }
}

/// Consumes forwarded ctl channels and relays each message's raw bytes to a mux
/// *client* via `frame_tx` (which the owner's relay loop wraps in a
/// [`Frame::Ctl`](super::protocol::Frame::Ctl)). Runs until the route's sender
/// is dropped or the client's relay loop drops `frame_tx`.
pub(crate) async fn pump_to_frames(
    mut channels: mpsc::UnboundedReceiver<russh::Channel<client::Msg>>,
    frame_tx: mpsc::UnboundedSender<Vec<u8>>,
) {
    while let Some(mut channel) = channels.recv().await {
        let frame_tx = frame_tx.clone();
        tokio::spawn(async move {
            if let Some(line) = read_ctl_line(&mut channel).await {
                let _ = frame_tx.send(line);
            }
        });
    }
}

/// Reads the two-line `isekai-pipe ctl` wire format off a forwarded
/// streamlocal channel — the secret-preamble line (the remote socket path;
/// see `isekai-pipe/src/ctl.rs`'s `secret_preamble`/`send_ctl_message`, which
/// unconditionally send it first, unix-only source but every remote host is
/// Linux per this project's design) followed by the actual `CtlMessage`
/// line — and returns only the `CtlMessage` line's bytes, without its
/// trailing newline. There is nothing to validate the preamble against here
/// (unlike the Unix `ssh(1)` path's `handle_ctl_connection`, which checks it
/// against a shared secret because a bare loopback TCP port has no other
/// access control): each per-tab forward is already exclusively scoped by
/// its own unique remote path, so the preamble is simply consumed and
/// discarded. `None` if the channel closes before a complete `CtlMessage`
/// line arrives (including if it closes mid-preamble).
///
/// Both lines can arrive in the same `Data` chunk (or split across several),
/// so this carries any bytes read past the preamble's newline forward into
/// the second line's search rather than discarding them.
async fn read_ctl_line(channel: &mut russh::Channel<client::Msg>) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    // The preamble line: read and discard it.
    loop {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.drain(..=pos);
            break;
        }
        match channel.wait().await {
            Some(russh::ChannelMsg::Data { data }) => buf.extend_from_slice(&data),
            // Closed before the preamble even completed — nothing usable.
            Some(russh::ChannelMsg::Eof) | Some(russh::ChannelMsg::Close) | None => return None,
            _ => {}
        }
    }
    // The actual CtlMessage line, continuing from any bytes already buffered
    // past the preamble's newline.
    loop {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.truncate(pos);
            return Some(buf);
        }
        match channel.wait().await {
            Some(russh::ChannelMsg::Data { data }) => buf.extend_from_slice(&data),
            // No newline arrived, but the peer closed: treat whatever we have
            // as the message (a message without a trailing newline is still
            // valid), or `None` if nothing came at all.
            Some(russh::ChannelMsg::Eof) | Some(russh::ChannelMsg::Close) | None => {
                return if buf.is_empty() { None } else { Some(buf) };
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use russh::server::{self, Auth, Msg as ServerMsg, Server as _, Session as ServerSession};
    use russh::Channel as RusshChannel;
    use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey as SshPrivateKey};
    use russh_stream_session::{authenticate_session, establish_over_stream, verifying_handler_with_routes, Credential, HostKeyVerifier, VerifyOutcome};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tokio::net::TcpListener;

    struct AcceptAllHostKeys;
    #[async_trait]
    impl HostKeyVerifier for AcceptAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> VerifyOutcome {
            VerifyOutcome::Accepted
        }
    }

    /// A mock sshd that, on a `streamlocal_forward` request, opens a
    /// `forwarded-streamlocal` channel back for that path and writes the real
    /// two-line `isekai-pipe ctl` wire format — the secret-preamble line (the
    /// socket path itself, exactly as the real `isekai-pipe ctl` binary always
    /// sends first; see `isekai-pipe/src/ctl.rs`'s `secret_preamble`) followed
    /// by one ctl message line (a `SetTitle`) — standing in for a remote
    /// `isekai-pipe ctl` pushing a title through the tab's forward. Omitting
    /// the preamble here previously let a real bug slip past this test: an
    /// earlier `read_ctl_line` read only the *first* line off the channel and
    /// treated it as the message, so it would have silently misread the real
    /// preamble as (invalid, discarded) JSON in production while this
    /// preamble-less mock kept passing.
    #[derive(Clone)]
    struct CtlPushServer;
    impl server::Server for CtlPushServer {
        type Handler = CtlPushHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> CtlPushHandler {
            CtlPushHandler
        }
    }
    #[derive(Clone)]
    struct CtlPushHandler;
    #[async_trait]
    impl server::Handler for CtlPushHandler {
        type Error = russh::Error;
        async fn auth_password(&mut self, _u: &str, _p: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }
        async fn channel_open_session(&mut self, _c: RusshChannel<ServerMsg>, _s: &mut ServerSession) -> Result<bool, Self::Error> {
            Ok(true)
        }
        async fn streamlocal_forward(&mut self, socket_path: &str, session: &mut ServerSession) -> Result<bool, Self::Error> {
            let handle = session.handle();
            let path = socket_path.to_string();
            tokio::spawn(async move {
                if let Ok(channel) = handle.channel_open_forwarded_streamlocal(path.clone()).await {
                    let _ = channel.data(format!("{path}\n").as_bytes()).await;
                    let _ = channel.data(&br#"{"op":"title","value":"hello-ctl"}"#[..]).await;
                    let _ = channel.data(&b"\n"[..]).await;
                    let _ = channel.eof().await;
                }
            });
            Ok(true)
        }
    }

    async fn authed_handle_with_routes(
        routes: &ForwardRoutes,
    ) -> client::Handle<russh_stream_session::VerifyingHandler<AcceptAllHostKeys>> {
        let keypair = Ed25519Keypair::from_seed(&[151; 32]);
        let host_key = SshPrivateKey::from(keypair);
        let config = Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut server = CtlPushServer;
        tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });

        let verifier = Arc::new(AcceptAllHostKeys);
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let handler = verifying_handler_with_routes(&verifier, routes);
        let mut handle = establish_over_stream(Arc::new(client::Config::default()), stream, handler).await.unwrap();
        assert!(authenticate_session(&mut handle, "tester", &Credential::Password("x".to_string())).await.unwrap());
        handle
    }

    /// End-to-end: `request` sets up a streamlocal forward, the mock sshd pushes
    /// a ctl message back over it, and `pump_to_frames` relays the raw bytes
    /// (which decode to the pushed `SetTitle`).
    #[tokio::test]
    async fn request_and_pump_relay_a_ctl_message_from_the_remote_forward() {
        use tokio::time::{timeout, Duration};

        let routes = ForwardRoutes::new();
        let handle = Mutex::new(authed_handle_with_routes(&routes).await);

        let forward = request(&handle, &routes).await.expect("streamlocal forward should be requested");
        assert!(forward.remote_path.starts_with(crate::ctl_forward::REMOTE_SOCK_PREFIX));

        let (frame_tx, mut frame_rx) = mpsc::unbounded_channel();
        tokio::spawn(pump_to_frames(forward.channels, frame_tx));

        let bytes = timeout(Duration::from_secs(5), frame_rx.recv())
            .await
            .expect("a ctl message should arrive before the timeout")
            .expect("the frame sender must not have been dropped");
        let msg = isekai_protocol::decode_ctl_message(&bytes).expect("relayed bytes must decode as a ctl message");
        assert_eq!(msg, isekai_protocol::CtlMessage::SetTitle { value: "hello-ctl".to_string() });
    }

    /// A ctl message pushed over the forward is applied to *this* process's own
    /// terminal via `pump_to_stderr` — exercised here only for the read/decode
    /// path (emit_osc writes to the real stderr); reaching the sender proves the
    /// route + channel plumbing works for the owner's own foreground shell too.
    #[tokio::test]
    async fn request_delivers_a_channel_to_the_owner_own_pump() {
        use tokio::time::{timeout, Duration};

        let routes = ForwardRoutes::new();
        let handle = Mutex::new(authed_handle_with_routes(&routes).await);
        let mut forward = request(&handle, &routes).await.expect("forward requested");

        // Rather than run pump_to_stderr (which writes to the real stderr),
        // assert the channel is actually delivered to the receiver — the same
        // receiver pump_to_stderr would consume — and that its line decodes.
        let mut channel = timeout(Duration::from_secs(5), forward.channels.recv())
            .await
            .expect("a forwarded ctl channel should arrive")
            .expect("the route sender must be live");
        let line = read_ctl_line(&mut channel).await.expect("the ctl channel must carry a line");
        let msg = isekai_protocol::decode_ctl_message(&line).unwrap();
        assert_eq!(msg, isekai_protocol::CtlMessage::SetTitle { value: "hello-ctl".to_string() });
    }
}
