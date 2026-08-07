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

/// Opens a login-shell-shaped channel that also exports
/// `$ISEKAI_CTL_SOCK=<remote_path>` when `remote_path` is `Some` (this tab's
/// ctl-socket forward), and/or execs `exec_target` in place of an ordinary
/// login shell when that's `Some` (`--isekai-tty`, `tty_attach.rs`) — the two
/// compose into one remote command line via
/// [`crate::ctl_forward::build_login_shell_command`] rather than one
/// disabling the other. Always requests a PTY + `exec`s the composed
/// command, rather than a plain `request_shell`, both because `SetEnv`/env
/// requests would need a remote `sshd_config` opt-in most users don't
/// control, and because `exec_target` (when present) needs one regardless.
pub(crate) async fn open_login_shell<H: client::Handler>(
    handle: &client::Handle<H>,
    term: &str,
    cols: u32,
    rows: u32,
    remote_path: Option<&str>,
    tab_idle_color: Option<(u8, u8, u8)>,
    tab_attention_color: Option<(u8, u8, u8)>,
    exec_target: Option<&str>,
) -> Result<russh::Channel<client::Msg>, russh::Error> {
    let channel = handle.channel_open_session().await?;
    channel.request_pty(false, term, cols, rows, 0, 0, &[]).await?;
    let command = crate::ctl_forward::build_login_shell_command(remote_path, tab_idle_color, tab_attention_color, exec_target);
    channel.exec(false, command.as_str()).await?;
    Ok(channel)
}

/// Backs `SetVar`/`GetVarRequest` for [`pump_to_stderr`] — the single-process
/// foreground path is exactly the "one process per tab" shape
/// `isekai_protocol::ctl_vars` module docs say makes one shared store
/// correct for `Tab`/`Session`/`Global` alike (matching the Unix `ssh(1)`
/// wrapper path's own `ctl_forward.rs::CTL_VARS`, a separate `#[cfg(unix)]`
/// instance — this one is native/Windows-only, hence its own static rather
/// than reusing that one). Review finding: previously `SetVar`/
/// `GetVarRequest` silently fell through `osc_sequence_for`'s `None` return
/// (correct for e.g. `ClipboardPullRequest`, which has no OSC form by
/// design, but wrong here) — `SetVar` was dropped with no storage at all,
/// and a remote `isekai-pipe ctl getvar` hung forever waiting for a
/// `GetVarResponse` that never arrived.
static NATIVE_CTL_VARS: std::sync::LazyLock<isekai_protocol::CtlVarStore> = std::sync::LazyLock::new(isekai_protocol::CtlVarStore::new);

/// Consumes forwarded ctl channels and applies each message to *this*
/// process's terminal — the owner's own / single-process foreground shell
/// direction. `SetVar`/`GetVarRequest` are answered via [`NATIVE_CTL_VARS`]
/// (see its docs); every other message except `BuildRequest` is applied in
/// place as an OSC escape on stderr, matching Epic M's original design.
/// `BuildRequest` (Epic P Phase 2) has no OSC equivalent and keeps the
/// channel busy for the whole build's duration instead of a single reply —
/// see [`super::build_relay::run_build_over_channel`]. Runs until the
/// route's sender is dropped (the forward is cancelled / the session ends).
pub(crate) async fn pump_to_stderr(mut channels: mpsc::UnboundedReceiver<russh::Channel<client::Msg>>, host: String) {
    while let Some(mut channel) = channels.recv().await {
        let host = host.clone();
        tokio::spawn(async move {
            let Some(line) = read_ctl_line(&mut channel).await else {
                return;
            };
            let Ok(msg) = isekai_protocol::decode_ctl_message(&line) else {
                return;
            };
            match &msg {
                isekai_protocol::CtlMessage::BuildRequest { profile } => {
                    if let Err(e) = super::build_relay::run_build_over_channel(&mut channel, &host, profile).await {
                        log_line!("isekai-ssh: build over ctl channel ended with an error: {e:#}");
                    }
                }
                isekai_protocol::CtlMessage::SetVar { key, value, .. } => {
                    NATIVE_CTL_VARS.set(key.clone(), value.clone());
                }
                isekai_protocol::CtlMessage::GetVarRequest { key, .. } => {
                    let response = isekai_protocol::CtlMessage::GetVarResponse { value: NATIVE_CTL_VARS.get(key) };
                    if let Ok(mut out) = serde_json::to_vec(&response) {
                        out.push(b'\n');
                        let _ = channel.data(&out[..]).await;
                    }
                    let _ = channel.close().await;
                }
                _ => {
                    // iTerm2はmacOS専用でWindowsには存在しないため、この
                    // Windows-native経路では常にWindowsTerminal互換の変換で
                    // 固定でよい(`ctl_forward::TerminalKind`のdoc comment参照)。
                    if let Some(seq) = crate::ctl_forward::osc_sequence_for(&msg, crate::ctl_forward::TerminalKind::WindowsTerminal) {
                        let _ = crate::ctl_forward::emit_osc(&seq);
                    }
                }
            }
        });
    }
}

/// One event `pump_to_frames` sends to `owner.rs::relay_loop` over its
/// `ctl_frame_rx`. Every existing message kind (title/clip/setvar/getvar,
/// and the initial `BuildRequest` line itself) is a one-shot
/// [`Message`](CtlRelayEvent::Message) exactly as before, now tagged with its
/// originating channel's id — review finding: without it, a hand-crafted
/// (non-`isekai-pipe ctl`) line on an *unrelated* forwarded channel that
/// happens to decode as `BuildFinished` would clear `active_build_reply_tx`/
/// `active_build_channel_id` for whichever build is actually in flight, the
/// same cross-channel confusion class [`BuildAborted`](CtlRelayEvent::BuildAborted)
/// exists to prevent. `BuildRequest` (Epic P Phase 2) additionally follows up
/// with [`BuildStarted`](CtlRelayEvent::BuildStarted): `relay_loop` must
/// remember `reply_tx` so it can forward the client's own `Frame::Ctl`
/// replies into it, since only the owner holds the real SSH channel a mux
/// client's build needs to stream its output back over
/// (`super::owner`/`super::client`'s module docs cover the full round trip).
pub(crate) enum CtlRelayEvent {
    Message { channel_id: russh::ChannelId, bytes: Vec<u8> },
    BuildStarted { channel_id: russh::ChannelId, reply_tx: mpsc::UnboundedSender<Vec<u8>> },
    /// The forwarded channel a build was running over disconnected
    /// (`Close`/`None`, never `Eof` — see `pump_to_frames`'s docs) before a
    /// real `BuildFinished` arrived. Carries the *originating* channel's id
    /// so `relay_loop` can tell an abort for a build that's no longer the
    /// active one (e.g. a since-rejected overlapping `BuildRequest`'s own
    /// channel closing) apart from one for the build it's actually
    /// tracking — synthesizing and relaying the abort sentinel only in the
    /// latter case. Review finding: an earlier version folded this into a
    /// plain `Message` carrying an already-encoded `BuildFinished{exit_code:
    /// BUILD_ABORTED_SENTINEL}}`, indistinguishable from any other channel's
    /// abort, which let a second (already-rejected) build's channel closing
    /// kill an unrelated, still-running first build.
    BuildAborted { channel_id: russh::ChannelId },
}

/// Consumes forwarded ctl channels and relays each message to a mux *client*
/// via `frame_tx` (`owner.rs::relay_loop` wraps a [`CtlRelayEvent::Message`]
/// in a [`Frame::Ctl`](super::protocol::Frame::Ctl); a
/// [`CtlRelayEvent::BuildStarted`] is internal bookkeeping only, never sent
/// to the client directly). Runs until the route's sender is dropped or the
/// client's relay loop drops `frame_tx`.
///
/// For every message except `BuildRequest`, this is exactly the original
/// one-shot behavior: read one line, relay it, let `channel` drop. For
/// `BuildRequest` (Epic P Phase 2), the spawned per-channel task instead
/// keeps `channel` alive and becomes a small bidirectional relay: it forwards
/// whatever the mux client later sends back (via `reply_rx`, fed by
/// `relay_loop` routing the client's own `Frame::Ctl` frames) onto the real
/// channel with `channel.data()`, and forwards `channel.wait()` reporting the
/// remote side closing (`Close`/`None` — **not** `Eof`, see the note on that
/// arm below) as a [`CtlRelayEvent::BuildAborted`] tagged with this channel's
/// id, so `relay_loop` can synthesize+relay an abort only if this channel is
/// still the one it's actually tracking, rather than blindly killing
/// whichever build happens to be active. This task never decodes replies to
/// look for `BuildFinished` itself — `relay_loop` already has to do that (to
/// know when to stop routing), and clearing its `active_build_reply_tx`
/// there drops `reply_tx`, which naturally ends this task's
/// `reply_rx.recv()` loop without a second, redundant decode here.
pub(crate) async fn pump_to_frames(
    mut channels: mpsc::UnboundedReceiver<russh::Channel<client::Msg>>,
    frame_tx: mpsc::UnboundedSender<CtlRelayEvent>,
) {
    while let Some(mut channel) = channels.recv().await {
        let frame_tx = frame_tx.clone();
        // Captured before `tokio::spawn` moves `channel` — needed to tag
        // this channel's `BuildStarted`/`BuildAborted` events so
        // `relay_loop` can tell them apart from a different build's.
        let channel_id = channel.id();
        tokio::spawn(async move {
            let Some(line) = read_ctl_line(&mut channel).await else {
                return;
            };
            let decoded = isekai_protocol::decode_ctl_message(&line);

            // `GetVarRequest`/`SetVar` are answered/logged by the owner
            // itself here, never relayed as a `Message` to the mux client:
            // a mux client has no way to answer a `GetVarRequest` (its only
            // upstream is `Frame::Ctl`, which `relay_loop` routes solely
            // into `active_build_reply_tx` — a client-emitted
            // `GetVarResponse` while a build is in flight would land on the
            // *build's* channel instead and break it, per review finding).
            // Full `VarScope`-aware native var-store parity across a mux
            // session (one holder, potentially many tabs) is deliberately
            // out of scope here — `VarScope::Tab` doesn't fit a
            // one-process-serves-many-tabs shape the way it does for
            // `pump_to_stderr`'s single-process-per-tab case (see that
            // function's `NATIVE_CTL_VARS` docs) — so `GetVarRequest` always
            // answers "unset" and `SetVar` is a no-op, both honestly logged
            // rather than silently dropped as before.
            if let Ok(isekai_protocol::CtlMessage::GetVarRequest { .. }) = &decoded {
                log_line!("isekai-ssh: ignoring a getvar request over a multiplexed (mux) session — answering \"unset\" (native var-store parity for mux sessions is not implemented yet)");
                let response = isekai_protocol::CtlMessage::GetVarResponse { value: None };
                if let Ok(mut out) = serde_json::to_vec(&response) {
                    out.push(b'\n');
                    let _ = channel.data(&out[..]).await;
                }
                let _ = channel.close().await;
                return;
            }
            if let Ok(isekai_protocol::CtlMessage::SetVar { key, .. }) = &decoded {
                log_line!("isekai-ssh: ignoring a setvar request for {key:?} over a multiplexed (mux) session (native var-store parity for mux sessions is not implemented yet)");
                let _ = channel.close().await;
                return;
            }

            let is_build_request = matches!(decoded, Ok(isekai_protocol::CtlMessage::BuildRequest { .. }));
            if frame_tx.send(CtlRelayEvent::Message { channel_id, bytes: line }).is_err() {
                return;
            }
            if !is_build_request {
                return;
            }

            let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Vec<u8>>();
            if frame_tx.send(CtlRelayEvent::BuildStarted { channel_id, reply_tx }).is_err() {
                return;
            }
            loop {
                tokio::select! {
                    bytes = reply_rx.recv() => {
                        match bytes {
                            Some(bytes) => {
                                if channel.data(&bytes[..]).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    msg = channel.wait() => {
                        // `Eof` deliberately excluded: RFC 4254 makes it a
                        // read-half-only half-close an exit-status may still
                        // legally follow, and the real `isekai-pipe ctl
                        // build` client always produces one right after
                        // sending `BuildRequest` (it shares every other ctl
                        // message's fire-and-forget `shutdown()` convention)
                        // — treating it as a real disconnect killed every
                        // real remote-triggered build before it could stream
                        // anything (see `build_relay.rs`'s module docs for
                        // the identical fix there).
                        if matches!(msg, None | Some(russh::ChannelMsg::Close)) {
                            let _ = frame_tx.send(CtlRelayEvent::BuildAborted { channel_id });
                            break;
                        }
                    }
                }
            }
            // `russh::Channel` has no `Drop` impl, so nothing sends
            // `CHANNEL_CLOSE` unless we do (same rationale as
            // `build_relay.rs::run_build_over_channel`'s identical call) —
            // harmless no-op if the channel is already closing (the
            // `disconnected`/`BuildAborted` exit above).
            let _ = channel.close().await;
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
///
/// `buf` is capped at [`isekai_protocol::MAX_CTL_MESSAGE_LINE_LEN`] across
/// *both* loops combined (review finding: this reader had no cap at all,
/// unlike every other reader in this codebase — `isekai_protocol::
/// decode_ctl_message`'s own cap doesn't help here since it only applies
/// *after* a `\n` is already found, and a peer that never sends one could
/// otherwise grow `buf` unboundedly inside the shared detached holder
/// process, where that's worse than an ordinary single-process crash). A
/// peer that exceeds it is treated the same as a malformed/absent message.
async fn read_ctl_line(channel: &mut russh::Channel<client::Msg>) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    // The preamble line: read and discard it.
    loop {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.drain(..=pos);
            break;
        }
        if buf.len() > isekai_protocol::MAX_CTL_MESSAGE_LINE_LEN {
            return None;
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
        if buf.len() > isekai_protocol::MAX_CTL_MESSAGE_LINE_LEN {
            return None;
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

        let event = timeout(Duration::from_secs(5), frame_rx.recv())
            .await
            .expect("a ctl message should arrive before the timeout")
            .expect("the frame sender must not have been dropped");
        let bytes = match event {
            CtlRelayEvent::Message { bytes, .. } => bytes,
            CtlRelayEvent::BuildStarted { .. } => panic!("SetTitle must relay as a plain Message, not BuildStarted"),
            CtlRelayEvent::BuildAborted { .. } => panic!("SetTitle must relay as a plain Message, not BuildAborted"),
        };
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
