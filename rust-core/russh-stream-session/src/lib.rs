//! Establish and authenticate an SSH client session (built on [`russh`]) over
//! any `AsyncRead + AsyncWrite` byte stream — not just a raw TCP socket.
//!
//! The connect/handshake functions are generic over the `russh`
//! `client::Handler` a caller supplies, so callers that need more than
//! host-key verification (agent forwarding, remote port forwards, other
//! server-initiated channel requests) can plug in their own handler.
//! Callers that only need host-key verification can use the bundled
//! [`VerifyingHandler`] (via [`verifying_handler`]) instead of writing one —
//! it delegates to a small [`HostKeyVerifier`] trait, so this crate has no
//! opinion on how (or whether) a caller persists a trust-on-first-use store.
//! Port forwarding, SSH agent forwarding, and any other
//! application-specific channel protocol are otherwise deliberately out of
//! scope — this crate covers exactly "authenticate a `russh::client::Handle`
//! and open one session channel (shell or exec)"; the I/O loop past that
//! point is left to the caller.
//!
//! [`russh`]: https://docs.rs/russh

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use russh::client;
use russh_keys::{HashAlg, PrivateKey, PublicKey};
use tokio::sync::mpsc;

/// Errors that can occur while connecting, authenticating, or opening a
/// channel. None of these variants carry credential material.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("TCP connect to {addr} failed: {source}")]
    Connect { addr: String, #[source] source: russh::Error },
    #[error("SSH handshake failed: {0}")]
    Handshake(russh::Error),
    #[error("jump host authentication failed for {username}@{addr}")]
    JumpAuthFailed { username: String, addr: String },
    #[error("jump host direct-tcpip tunnel to {host}:{port} failed: {source}")]
    JumpTunnel { host: String, port: u16, #[source] source: russh::Error },
    #[error("SSH handshake over jump tunnel to {host}:{port} failed: {source}")]
    JumpHandshake { host: String, port: u16, #[source] source: russh::Error },
    #[error("channel operation failed: {0}")]
    Channel(russh::Error),
    #[error("private key could not be parsed as OpenSSH format: {0}")]
    InvalidPrivateKey(russh_keys::ssh_key::Error),
    #[error("authentication request failed: {0}")]
    Auth(russh::Error),
    #[error("agent-backed authentication failed: {0}")]
    AgentAuth(russh::AgentAuthError),
}

/// The result of a [`HostKeyVerifier::verify`] call. Unlike a plain `bool`,
/// a rejection carries a human-readable reason — `VerifyingHandler` stores it
/// in a caller-supplied [`RejectionReason`] slot, since `check_server_key`'s
/// `Result<bool, russh::Error>` return has no room for one (`russh::Error` is
/// a closed enum with no caller-message-carrying variant).
#[derive(Debug, Clone)]
pub enum VerifyOutcome {
    Accepted,
    Rejected(String),
}

/// Verifies a server's host-key fingerprint (SHA-256, as produced by
/// `PublicKey::fingerprint(HashAlg::Sha256)`). Implementations typically
/// consult a trust-on-first-use store and/or prompt the user.
#[async_trait]
pub trait HostKeyVerifier: Send + Sync {
    async fn verify(&self, fingerprint: &str) -> VerifyOutcome;
}

/// A shared slot a caller can inspect *after* a handshake fails, to recover
/// the human-readable reason a [`HostKeyVerifier::verify`] rejection carried
/// — see [`VerifyOutcome`]'s docs for why this indirection exists instead of
/// threading the reason through `check_server_key`'s return type directly.
///
/// Follows the same `Arc`-backed, `Clone`-shares-state shape as
/// [`ForwardRoutes`]: construct one, pass `&reason` into
/// [`verifying_handler_with_reason`]/[`verifying_handler_with_routes_and_reason`],
/// and keep your own clone to call [`take`](Self::take) on once the
/// handshake has failed — the clone installed inside the (otherwise
/// unreachable, once handed to `new_handler`) handler and the clone the
/// caller kept refer to the same slot.
#[derive(Clone, Default)]
pub struct RejectionReason(Arc<Mutex<Option<String>>>);

impl RejectionReason {
    pub fn new() -> Self {
        Self::default()
    }

    fn set(&self, reason: String) {
        *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason);
    }

    /// Takes (and clears) the last recorded rejection reason, if any. `None`
    /// if no `verify` call on this slot's handler ever rejected — e.g. the
    /// handshake failed for an unrelated reason (network error, auth
    /// failure past the host-key step).
    pub fn take(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()
    }
}

/// Authentication material for one `authenticate_session` call. Callers are
/// responsible for zeroizing this after use (see [`Credential::zeroize`]) —
/// this crate does not retain a copy once authentication completes.
pub enum Credential {
    Password(String),
    PublicKey { private_key_pem: Vec<u8> },
}

impl Credential {
    pub fn zeroize(&mut self) {
        use zeroize::Zeroize;
        match self {
            Credential::Password(password) => password.zeroize(),
            Credential::PublicKey { private_key_pem } => private_key_pem.zeroize(),
        }
    }
}

/// Zeroizes automatically on every drop, not just when a caller remembers to
/// call [`Credential::zeroize`] explicitly — a caller that does call it
/// first just makes this a harmless no-op pass over an already-zeroed
/// buffer (defense in depth: minimizes the exposure *window* on the happy
/// path, while this `Drop` impl closes the gap on every early-return error
/// path a caller might not have covered, e.g. a `?` between constructing a
/// `Credential` and its own explicit `zeroize()` call — a real instance of
/// exactly this gap was found by Codex review in `isekai-bootstrap::
/// RusshBackend::connect_and_authenticate`).
impl Drop for Credential {
    fn drop(&mut self) {
        self.zeroize();
    }
}

/// Which leg of a connection [`connect_via_jump_or_direct`] is asking a
/// caller's handler factory to build a `client::Handler` for. Passed
/// explicitly (rather than left for the caller to infer from call order) so
/// that a caller that needs a *different* handler per leg — e.g. a distinct
/// host-key trust-store entry for the jump host vs. the target — can never
/// silently desync if this function's internal connection sequence ever
/// changes (adds a retry, a probe, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionLeg {
    /// The jump host itself (only ever constructed when a `JumpHost` is
    /// passed).
    Jump,
    /// The final target — the only leg in a direct (no-jump) connection, and
    /// the second leg when tunneling through a jump host.
    Target,
}

/// A single-hop jump host (`ssh -J` equivalent) to tunnel through before
/// reaching the real target.
pub struct JumpHost {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub credential: Credential,
}

/// A registry of *forwarded-streamlocal* routes: maps a remote socket path
/// (the routing key a caller passed to
/// [`russh::client::Handle::streamlocal_forward`]) to a sink that receives
/// each server-initiated `forwarded-streamlocal@openssh.com` channel opened
/// for that path.
///
/// This is how a caller consumes remote UNIX-domain-socket forwards
/// (`ssh -R <remote-sock>:...`) *in-process*, without this crate taking any
/// opinion on what the forwarded bytes mean. The usual sequence is:
///
/// 1. build [`ForwardRoutes::new`],
/// 2. [`register`](ForwardRoutes::register) each remote socket path (keeping
///    the returned receiver),
/// 3. install the routes on the handler with [`verifying_handler_with_routes`],
/// 4. request the forward with `handle.streamlocal_forward(path)`.
///
/// Each incoming channel for a registered path is delivered to that path's
/// receiver; a channel whose path has no live route is dropped (which closes
/// it), matching what an `ssh -R` peer would see for a cancelled forward.
///
/// Cloning shares the same underlying table (it is `Arc`-backed), so the copy
/// held inside the handler and the copy a caller keeps for registration stay
/// in sync.
#[derive(Clone, Default)]
pub struct ForwardRoutes {
    inner: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<russh::Channel<client::Msg>>>>>,
}

impl ForwardRoutes {
    /// An empty route table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `key` (a remote socket path) and returns the receiving end
    /// for channels routed to it. Any previously-registered sender for the
    /// same key is replaced.
    pub fn register(&self, key: impl Into<String>) -> mpsc::UnboundedReceiver<russh::Channel<client::Msg>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.lock().insert(key.into(), tx);
        rx
    }

    /// Removes `key`'s route, if any. Call this after
    /// `handle.cancel_streamlocal_forward(key)` so a channel that races in for
    /// an already-cancelled forward is dropped (closed) rather than routed.
    pub fn unregister(&self, key: &str) {
        self.lock().remove(key);
    }

    /// Routes `channel` to `key`'s registered sink. Returns `true` if a live
    /// route consumed it; `false` (the channel is dropped, and thus closed) if
    /// there was no route or its receiver had already been dropped — in which
    /// case the stale entry is pruned.
    fn dispatch(&self, key: &str, channel: russh::Channel<client::Msg>) -> bool {
        let mut guard = self.lock();
        let Some(tx) = guard.get(key) else {
            return false;
        };
        if tx.send(channel).is_ok() {
            true
        } else {
            guard.remove(key);
            false
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, mpsc::UnboundedSender<russh::Channel<client::Msg>>>> {
        // A poisoned lock means a caller panicked mid-registration; the map is
        // still structurally intact, so recover the guard rather than
        // cascading the panic into every later route operation.
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A minimal `client::Handler` that delegates host-key verification to a
/// [`HostKeyVerifier`] and, optionally, routes server-initiated
/// `forwarded-streamlocal@openssh.com` channels to a caller-supplied
/// [`ForwardRoutes`]. All other `client::Handler` methods use russh's defaults
/// (reject/no-op), so this handler is suitable for sessions that need only
/// host-key verification and (optionally) remote UNIX-socket forwards —
/// callers that need agent forwarding or other server-initiated channel
/// requests should implement their own `client::Handler`.
pub struct VerifyingHandler<V> {
    verifier: Arc<V>,
    forward_routes: Option<ForwardRoutes>,
    rejection: Option<RejectionReason>,
}

impl<V: HostKeyVerifier + 'static> VerifyingHandler<V> {
    /// A handler that does only host-key verification — no forward routing,
    /// no rejection-reason capture. `verifier` is cloned once (cheap: it's
    /// an `Arc`). Chain [`with_forward_routes`](Self::with_forward_routes)/
    /// [`with_rejection_reason`](Self::with_rejection_reason) to opt into
    /// either (or both) of the optional features — simplification (Codex
    /// review finding): this replaces what would otherwise be a combinatorial
    /// explosion of `verifying_handler_with_x_and_y` free functions as more
    /// optional features are added.
    pub fn new(verifier: &Arc<V>) -> Self {
        Self { verifier: verifier.clone(), forward_routes: None, rejection: None }
    }

    /// Routes server-initiated `forwarded-streamlocal@openssh.com` channels
    /// (from an `ssh -R <remote-sock>:...` this session requests via
    /// `handle.streamlocal_forward(...)`) to `routes` instead of dropping
    /// them.
    pub fn with_forward_routes(mut self, routes: &ForwardRoutes) -> Self {
        self.forward_routes = Some(routes.clone());
        self
    }

    /// Installs `reason` so a caller can recover a rejected [`VerifyOutcome`]'s
    /// human-readable message after the handshake fails — see
    /// [`RejectionReason`]'s docs.
    pub fn with_rejection_reason(mut self, reason: &RejectionReason) -> Self {
        self.rejection = Some(reason.clone());
        self
    }
}

#[async_trait]
impl<V: HostKeyVerifier + 'static> client::Handler for VerifyingHandler<V> {
    type Error = russh::Error;

    async fn check_server_key(&mut self, server_public_key: &PublicKey) -> Result<bool, Self::Error> {
        let fingerprint = server_public_key.fingerprint(HashAlg::Sha256).to_string();
        match self.verifier.verify(&fingerprint).await {
            VerifyOutcome::Accepted => Ok(true),
            VerifyOutcome::Rejected(reason) => {
                if let Some(slot) = &self.rejection {
                    slot.set(reason);
                }
                Ok(false)
            }
        }
    }

    /// The server opened a channel for a new connection to a remote socket
    /// this session had requested via `streamlocal_forward` (`ssh -R` over a
    /// UNIX domain socket). If a [`ForwardRoutes`] was installed and has a
    /// live route for `socket_path`, the channel is handed to that route's
    /// receiver (fire-and-forget); otherwise the channel is dropped, which
    /// closes it — the same thing the peer sees for a forward that was never
    /// requested or has since been cancelled.
    async fn server_channel_open_forwarded_streamlocal(
        &mut self,
        channel: russh::Channel<client::Msg>,
        socket_path: &str,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        if let Some(routes) = &self.forward_routes {
            let _ = routes.dispatch(socket_path, channel);
        }
        Ok(())
    }
}

/// An established (not yet authenticated) SSH connection, possibly tunneled
/// through a jump host. The jump host's own `client::Handle` (if any) is
/// kept alive internally for as long as this session is in use — dropping
/// [`Session`] tears down the tunnel too.
pub struct Session<H: client::Handler + 'static> {
    pub handle: client::Handle<H>,
    _jump_handle: Option<client::Handle<H>>,
}

/// Connects to `target_host:target_port`, either directly or (if `jump` is
/// given) by first authenticating to the jump host and tunneling through a
/// `direct-tcpip` channel (`ssh -J` equivalent, single hop). The returned
/// [`Session`] is connected but not yet authenticated to the target —
/// call [`authenticate_session`] next.
///
/// Generic over the `client::Handler` type `H` so callers that need more
/// than host-key verification (agent forwarding, remote port forwards,
/// other server-initiated channel requests) can plug in their own handler —
/// `new_handler` is called once per connection leg (twice total when a jump
/// host is used: once with [`ConnectionLeg::Jump`], then once with
/// [`ConnectionLeg::Target`]; exactly once with [`ConnectionLeg::Target`]
/// for a direct connection). The leg is passed explicitly so a caller that
/// needs a per-leg handler (e.g. a distinct host-key verifier for the jump
/// host vs. the target) selects it from the `ConnectionLeg` argument rather
/// than counting calls — see [`ConnectionLeg`]'s own docs. Callers that only
/// need host-key verification can use [`VerifyingHandler`] via
/// [`verifying_handler`] instead of writing their own `client::Handler`.
pub async fn connect_via_jump_or_direct<H, F>(
    jump: Option<&JumpHost>,
    russh_config: Arc<client::Config>,
    target_host: &str,
    target_port: u16,
    mut new_handler: F,
) -> Result<Session<H>, SessionError>
where
    H: client::Handler<Error = russh::Error> + Send + 'static,
    F: FnMut(ConnectionLeg) -> H,
{
    let Some(jump) = jump else {
        let addr = format!("{target_host}:{target_port}");
        let handle = client::connect(russh_config, addr.as_str(), new_handler(ConnectionLeg::Target))
            .await
            .map_err(|source| SessionError::Connect { addr, source })?;
        return Ok(Session { handle, _jump_handle: None });
    };

    let jump_addr = format!("{}:{}", jump.host, jump.port);
    let mut jump_handle = client::connect(russh_config.clone(), jump_addr.as_str(), new_handler(ConnectionLeg::Jump))
        .await
        .map_err(|source| SessionError::Connect { addr: jump_addr.clone(), source })?;

    let authenticated = authenticate_session(&mut jump_handle, &jump.username, &jump.credential).await?;
    if !authenticated {
        return Err(SessionError::JumpAuthFailed { username: jump.username.clone(), addr: jump_addr });
    }

    let channel = jump_handle
        .channel_open_direct_tcpip(target_host, target_port as u32, "127.0.0.1", 0)
        .await
        .map_err(|source| SessionError::JumpTunnel { host: target_host.to_string(), port: target_port, source })?;
    let stream = channel.into_stream();

    let handle = client::connect_stream(russh_config, stream, new_handler(ConnectionLeg::Target))
        .await
        .map_err(|source| SessionError::JumpHandshake { host: target_host.to_string(), port: target_port, source })?;

    Ok(Session { handle, _jump_handle: Some(jump_handle) })
}

/// Runs the SSH handshake directly over an already-established byte stream
/// (e.g. a QUIC stream, or any other application-provided tunnel) — for
/// callers that have their own way of reaching the target and just need SSH
/// layered on top. Not yet authenticated — call [`authenticate_session`]
/// next.
pub async fn establish_over_stream<S, H>(
    russh_config: Arc<client::Config>,
    stream: S,
    handler: H,
) -> Result<client::Handle<H>, SessionError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    H: client::Handler<Error = russh::Error> + Send + 'static,
{
    client::connect_stream(russh_config, stream, handler).await.map_err(SessionError::Handshake)
}

/// Builds a [`VerifyingHandler`] for `verifier` — a convenience for the
/// common case of [`connect_via_jump_or_direct`]'s `new_handler` argument
/// when a caller only needs host-key verification and nothing else
/// (no agent forwarding, no remote forwards). `verifier` is cloned once per
/// call (cheap: it's an `Arc`).
pub fn verifying_handler<V: HostKeyVerifier + 'static>(verifier: &Arc<V>) -> VerifyingHandler<V> {
    VerifyingHandler::new(verifier)
}

/// Like [`verifying_handler`], but also installs `routes` — see
/// [`VerifyingHandler::with_forward_routes`].
pub fn verifying_handler_with_routes<V: HostKeyVerifier + 'static>(
    verifier: &Arc<V>,
    routes: &ForwardRoutes,
) -> VerifyingHandler<V> {
    VerifyingHandler::new(verifier).with_forward_routes(routes)
}

/// Like [`verifying_handler`], but also installs `reason` — see
/// [`VerifyingHandler::with_rejection_reason`].
pub fn verifying_handler_with_reason<V: HostKeyVerifier + 'static>(
    verifier: &Arc<V>,
    reason: &RejectionReason,
) -> VerifyingHandler<V> {
    VerifyingHandler::new(verifier).with_rejection_reason(reason)
}

/// Combines [`verifying_handler_with_routes`] and
/// [`verifying_handler_with_reason`] for callers that need both (e.g. the
/// day-to-day native connect path, which routes ctl-socket forwards *and*
/// wants a host-key-rejection reason for its top-level error).
pub fn verifying_handler_with_routes_and_reason<V: HostKeyVerifier + 'static>(
    verifier: &Arc<V>,
    routes: &ForwardRoutes,
    reason: &RejectionReason,
) -> VerifyingHandler<V> {
    VerifyingHandler::new(verifier).with_forward_routes(routes).with_rejection_reason(reason)
}

/// Authenticates `session` as `username` using `credential`. `Ok(false)`
/// means the server declined the credential (wrong password, unauthorized
/// key); `Err` means authentication couldn't even be attempted (malformed
/// private key) or the underlying SSH request itself failed (transport
/// error). Does not zeroize `credential` — call [`Credential::zeroize`]
/// once you're done with it.
pub async fn authenticate_session<H: client::Handler>(
    session: &mut client::Handle<H>,
    username: &str,
    credential: &Credential,
) -> Result<bool, SessionError> {
    match credential {
        Credential::Password(password) => {
            session.authenticate_password(username, password).await.map_err(SessionError::Auth)
        }
        Credential::PublicKey { private_key_pem } => {
            let key = PrivateKey::from_openssh(private_key_pem).map_err(SessionError::InvalidPrivateKey)?;
            session.authenticate_publickey(username, Arc::new(key)).await.map_err(SessionError::Auth)
        }
    }
}

/// Authenticates `session` as `username` by asking `signer` — typically a
/// [`russh_keys::agent::client::AgentClient`] (russh provides a blanket
/// [`russh::Signer`] impl for it, over any `AsyncRead + AsyncWrite`
/// transport: a Unix socket, a Windows named pipe, or Pageant) — to sign the
/// server's challenge for `public_key`, instead of holding private key
/// material in this process at all. `Ok(false)` means the server declined
/// (`public_key` isn't authorized); `Err` means the signer itself failed
/// (agent connection dropped mid-request, agent declined to sign, ...).
///
/// Callers are responsible for choosing *which* `public_key` to try (e.g.
/// via the agent's own `request_identities()`) — this function attempts
/// exactly one.
pub async fn authenticate_with_signer<H, S>(
    session: &mut client::Handle<H>,
    username: &str,
    public_key: PublicKey,
    signer: &mut S,
) -> Result<bool, SessionError>
where
    H: client::Handler,
    S: russh::Signer<Error = russh::AgentAuthError>,
{
    session.authenticate_publickey_with(username, public_key, signer).await.map_err(SessionError::AgentAuth)
}

/// What kind of session channel to open: an interactive PTY+shell, or a
/// single non-interactive command (`ssh host 'command'` equivalent).
pub enum SessionKind {
    Shell { term: String, cols: u32, rows: u32, terminal_modes: Vec<(russh::Pty, u32)> },
    Exec { command: String },
}

/// Opens one session channel on `handle` and requests either a PTY+shell or
/// a single exec, per `kind`. The returned channel is ready for the caller
/// to drive its own I/O loop (read `ChannelMsg::Data`/`ExitStatus`, write
/// via `channel.data(...)`).
pub async fn open_channel<H: client::Handler>(
    handle: &client::Handle<H>,
    kind: &SessionKind,
) -> Result<russh::Channel<client::Msg>, SessionError> {
    let channel = handle.channel_open_session().await.map_err(SessionError::Channel)?;
    match kind {
        SessionKind::Shell { term, cols, rows, terminal_modes } => {
            channel.request_pty(false, term, *cols, *rows, 0, 0, terminal_modes).await.map_err(SessionError::Channel)?;
            channel.request_shell(false).await.map_err(SessionError::Channel)?;
        }
        SessionKind::Exec { command } => {
            channel.exec(false, command.as_str()).await.map_err(SessionError::Channel)?;
        }
    }
    Ok(channel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
    use russh::{Channel as RusshChannel, ChannelId, ChannelMsg, CryptoVec};
    use russh_keys::ssh_key::private::Ed25519Keypair;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    struct AcceptAllHostKeys;

    #[async_trait]
    impl HostKeyVerifier for AcceptAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> VerifyOutcome {
            VerifyOutcome::Accepted
        }
    }

    /// Minimal server: accepts password `"correct-password"` (rejects any
    /// other password) and any public key, accepts session-channel opens,
    /// and echoes exec commands back as a single line of output followed by
    /// exit status 0. Enough to prove `open_channel`'s `Exec` path actually
    /// round-trips data through a real (in-process) SSH server, and that
    /// `authenticate_session` actually distinguishes accepted vs rejected
    /// credentials rather than always succeeding.
    #[derive(Clone)]
    struct EchoExecServer;

    impl server::Server for EchoExecServer {
        type Handler = EchoExecHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> EchoExecHandler {
            EchoExecHandler
        }
    }

    #[derive(Clone)]
    struct EchoExecHandler;

    #[async_trait]
    impl server::Handler for EchoExecHandler {
        type Error = russh::Error;

        async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
            Ok(if password == "correct-password" {
                Auth::Accept
            } else {
                Auth::Reject { proceed_with_methods: None }
            })
        }

        async fn auth_publickey(
            &mut self, _user: &str, _public_key: &russh_keys::ssh_key::PublicKey,
        ) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }

        async fn exec_request(
            &mut self, channel: ChannelId, data: &[u8], session: &mut ServerSession,
        ) -> Result<(), Self::Error> {
            let command = String::from_utf8_lossy(data).into_owned();
            session.data(channel, CryptoVec::from(format!("ran: {command}\n").into_bytes()))?;
            session.exit_status_request(channel, 0)?;
            session.channel_success(channel)?;
            session.close(channel)?;
            Ok(())
        }
    }

    /// Jump server: accepts any password and tunnels `direct-tcpip` requests
    /// to a real TCP connection, exactly like a real sshd's `-J` support.
    #[derive(Clone)]
    struct JumpServer;

    impl server::Server for JumpServer {
        type Handler = JumpHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> JumpHandler {
            JumpHandler
        }
    }

    #[derive(Clone)]
    struct JumpHandler;

    #[async_trait]
    impl server::Handler for JumpHandler {
        type Error = russh::Error;

        async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_direct_tcpip(
            &mut self,
            channel: RusshChannel<ServerMsg>,
            host_to_connect: &str,
            port_to_connect: u32,
            _originator_address: &str,
            _originator_port: u32,
            _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            let target = format!("{host_to_connect}:{port_to_connect}");
            tokio::spawn(async move {
                let Ok(mut outbound) = tokio::net::TcpStream::connect(&target).await else { return };
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut outbound).await;
            });
            Ok(true)
        }
    }

    async fn spawn_server<S, H>(mut server: S, seed: u8) -> SocketAddr
    where
        S: server::Server<Handler = H> + Send + 'static,
        H: server::Handler + Send + 'static,
    {
        let keypair = Ed25519Keypair::from_seed(&[seed; 32]);
        let host_key = PrivateKey::from(keypair);
        let config = Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });
        addr
    }

    #[tokio::test]
    async fn direct_connect_authenticate_and_exec_round_trips() {
        let addr = spawn_server(EchoExecServer, 1).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let mut session = connect_via_jump_or_direct(
            None, Arc::new(client::Config::default()), &addr.ip().to_string(), addr.port(),
            |_leg| verifying_handler(&verifier),
        )
        .await
        .expect("direct connect should succeed");

        let authed = authenticate_session(
            &mut session.handle, "tester", &Credential::Password("correct-password".into()),
        )
        .await
        .expect("authenticate_session should not error for a well-formed password credential");
        assert!(authed, "password auth should succeed with the password the server accepts");

        let mut channel = open_channel(
            &session.handle, &SessionKind::Exec { command: "echo hi".into() },
        )
        .await
        .expect("exec channel should open");

        let mut saw_data = false;
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    assert_eq!(&data[..], b"ran: echo hi\n");
                    saw_data = true;
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    assert_eq!(exit_status, 0);
                }
                None => break,
                _ => {}
            }
        }
        assert!(saw_data, "expected the server's echoed exec output");
    }

    #[tokio::test]
    async fn jump_host_tunnels_to_target_and_authenticates() {
        let target_addr = spawn_server(EchoExecServer, 2).await;
        let jump_addr = spawn_server(JumpServer, 3).await;

        let jump = JumpHost {
            host: jump_addr.ip().to_string(),
            port: jump_addr.port(),
            username: "jumper".into(),
            credential: Credential::Password("correct-password".into()),
        };

        let verifier = Arc::new(AcceptAllHostKeys);
        let mut session = connect_via_jump_or_direct(
            Some(&jump), Arc::new(client::Config::default()), &target_addr.ip().to_string(), target_addr.port(),
            |_leg| verifying_handler(&verifier),
        )
        .await
        .expect("jump connect should succeed");

        let authed = authenticate_session(
            &mut session.handle, "tester", &Credential::Password("correct-password".into()),
        )
        .await
        .expect("authenticate_session should not error over a jump-tunneled session");
        assert!(authed, "authentication over the jump-tunneled session should succeed");

        // Confirm the tunneled session behaves like an ordinary connection
        // beyond just authenticating: open a real channel through it.
        session.handle.channel_open_session().await.expect("opening a channel through the jump tunnel should succeed");
    }

    #[tokio::test]
    async fn public_key_authentication_succeeds() {
        let addr = spawn_server(EchoExecServer, 4).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let mut session = connect_via_jump_or_direct(
            None, Arc::new(client::Config::default()), &addr.ip().to_string(), addr.port(),
            |_leg| verifying_handler(&verifier),
        )
        .await
        .expect("direct connect should succeed");

        let keypair = Ed25519Keypair::from_seed(&[99u8; 32]);
        let key = PrivateKey::from(keypair);
        let pem = key.to_openssh(Default::default()).unwrap().as_bytes().to_vec();

        let authed = authenticate_session(
            &mut session.handle, "tester", &Credential::PublicKey { private_key_pem: pem },
        )
        .await
        .expect("authenticate_session should not error for a well-formed key");
        assert!(authed, "the server accepts any public key");
    }

    #[tokio::test]
    async fn wrong_password_is_rejected_not_an_error() {
        let addr = spawn_server(EchoExecServer, 5).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let mut session = connect_via_jump_or_direct(
            None, Arc::new(client::Config::default()), &addr.ip().to_string(), addr.port(),
            |_leg| verifying_handler(&verifier),
        )
        .await
        .expect("direct connect should succeed");

        let authed = authenticate_session(
            &mut session.handle, "tester", &Credential::Password("wrong-password".into()),
        )
        .await
        .expect("a rejected credential is Ok(false), not an error");
        assert!(!authed, "the server should have rejected this password");
    }

    #[tokio::test]
    async fn malformed_private_key_returns_invalid_private_key_error() {
        let addr = spawn_server(EchoExecServer, 6).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let mut session = connect_via_jump_or_direct(
            None, Arc::new(client::Config::default()), &addr.ip().to_string(), addr.port(),
            |_leg| verifying_handler(&verifier),
        )
        .await
        .expect("direct connect should succeed");

        let result = authenticate_session(
            &mut session.handle, "tester",
            &Credential::PublicKey { private_key_pem: b"not a real openssh private key".to_vec() },
        )
        .await;
        assert!(
            matches!(result, Err(SessionError::InvalidPrivateKey(_))),
            "malformed key material should surface as InvalidPrivateKey, not a silent false: {result:?}"
        );
    }

    struct RejectAllHostKeys;

    #[async_trait]
    impl HostKeyVerifier for RejectAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> VerifyOutcome {
            VerifyOutcome::Rejected("rejected by test double".to_string())
        }
    }

    #[tokio::test]
    async fn rejecting_host_key_aborts_the_handshake() {
        let addr = spawn_server(EchoExecServer, 7).await;
        let verifier = Arc::new(RejectAllHostKeys);
        let result = connect_via_jump_or_direct(
            None, Arc::new(client::Config::default()), &addr.ip().to_string(), addr.port(),
            |_leg| verifying_handler(&verifier),
        )
        .await;
        assert!(
            result.is_err(),
            "a HostKeyVerifier that always returns false must abort the connection, not silently proceed"
        );
    }

    /// A `russh::Signer` that signs locally with an in-memory key instead of
    /// talking to a real external agent process — this test's whole point is
    /// to prove `authenticate_with_signer` correctly drives russh's
    /// `authenticate_publickey_with`/`Signer` flow, which is identical
    /// whether the signer on the other end is a real
    /// `russh_keys::agent::client::AgentClient` (Unix socket, Windows named
    /// pipe, or Pageant — russh provides the `Signer` impl for all of them)
    /// or, as here, anything else implementing the same trait. No real OS
    /// agent process needed to exercise this.
    struct FakeSigner {
        key: PrivateKey,
    }

    #[async_trait]
    impl russh::Signer for FakeSigner {
        type Error = russh::AgentAuthError;

        async fn auth_publickey_sign(
            &mut self,
            _key: &PublicKey,
            mut to_sign: russh::CryptoVec,
        ) -> Result<russh::CryptoVec, Self::Error> {
            use signature::Signer as _;
            use ssh_encoding::Encode;

            // Reproduces exactly the wire format a real agent's
            // `SIGN_RESPONSE` produces (russh-keys'
            // `AgentClient::write_signature`): the original challenge bytes,
            // followed by a 4-byte length prefix, followed by the
            // signature blob (`Signature`'s own `Encode` impl already
            // writes `[string algorithm][string raw_bytes]`, which is that
            // same blob).
            let signature = self.key.try_sign(&to_sign).expect("signing with a known-good in-memory test key must not fail");
            let mut sig_bytes = Vec::new();
            signature.encode(&mut sig_bytes).expect("encoding a signature must not fail");
            (sig_bytes.len() as u32).encode(&mut to_sign).expect("encoding a length prefix must not fail");
            for byte in sig_bytes {
                to_sign.push(byte);
            }
            Ok(to_sign)
        }
    }

    #[tokio::test]
    async fn authenticate_with_signer_succeeds_against_a_real_server() {
        let addr = spawn_server(EchoExecServer, 8).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let mut session = connect_via_jump_or_direct(
            None, Arc::new(client::Config::default()), &addr.ip().to_string(), addr.port(),
            |_leg| verifying_handler(&verifier),
        )
        .await
        .expect("direct connect should succeed");

        let keypair = Ed25519Keypair::from_seed(&[42u8; 32]);
        let private_key = PrivateKey::from(keypair);
        let public_key = private_key.public_key().clone();
        let mut signer = FakeSigner { key: private_key };

        let authed = authenticate_with_signer(&mut session.handle, "tester", public_key, &mut signer)
            .await
            .expect("authenticate_with_signer should not error against a server that accepts any public key");
        assert!(authed, "the server accepts any public key, so a correctly-signed challenge must succeed");
    }

    #[tokio::test]
    async fn direct_connection_constructs_exactly_one_target_leg_handler() {
        let addr = spawn_server(EchoExecServer, 9).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let legs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let legs_recorder = legs.clone();
        let _session = connect_via_jump_or_direct(
            None, Arc::new(client::Config::default()), &addr.ip().to_string(), addr.port(),
            move |leg| {
                legs_recorder.lock().unwrap().push(leg);
                verifying_handler(&verifier)
            },
        )
        .await
        .expect("direct connect should succeed");

        assert_eq!(
            *legs.lock().unwrap(),
            vec![ConnectionLeg::Target],
            "a direct connection must build exactly one handler, for the target leg"
        );
    }

    /// A server that accepts any password and, when a `streamlocal_forward`
    /// (`ssh -R <sock>`) is requested, immediately opens a
    /// `forwarded-streamlocal@openssh.com` channel back for that exact socket
    /// path, writes a fixed payload, and closes it — standing in for a real
    /// sshd delivering a remote-forward connection to the client.
    #[derive(Clone)]
    struct StreamlocalForwardServer;

    impl server::Server for StreamlocalForwardServer {
        type Handler = StreamlocalForwardHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> StreamlocalForwardHandler {
            StreamlocalForwardHandler
        }
    }

    #[derive(Clone)]
    struct StreamlocalForwardHandler;

    #[async_trait]
    impl server::Handler for StreamlocalForwardHandler {
        type Error = russh::Error;

        async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn streamlocal_forward(&mut self, socket_path: &str, session: &mut ServerSession) -> Result<bool, Self::Error> {
            let handle = session.handle();
            let path = socket_path.to_string();
            // Open the forwarded channel off-thread: doing it inline would
            // deadlock, since the client is still awaiting this request's
            // reply and can't process the channel-open until it arrives.
            tokio::spawn(async move {
                if let Ok(channel) = handle.channel_open_forwarded_streamlocal(path).await {
                    let _ = channel.data(&b"ctl-payload\n"[..]).await;
                    let _ = channel.eof().await;
                }
            });
            Ok(true)
        }
    }

    /// A `streamlocal_forward` request must round-trip: the server opens a
    /// `forwarded-streamlocal` channel back for that socket path, and a
    /// [`ForwardRoutes`] installed via [`verifying_handler_with_routes`] must
    /// deliver that channel to the matching receiver so the caller can read
    /// the forwarded bytes in-process.
    #[tokio::test]
    async fn streamlocal_forward_delivers_channels_to_registered_routes() {
        use tokio::time::{timeout, Duration};

        let addr = spawn_server(StreamlocalForwardServer, 12).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let routes = ForwardRoutes::new();
        let remote_path = "/tmp/isekai-pipe-ctl-roundtrip.sock";
        let mut rx = routes.register(remote_path);

        let mut handle = establish_over_stream(
            Arc::new(client::Config::default()),
            tokio::net::TcpStream::connect(addr).await.unwrap(),
            verifying_handler_with_routes(&verifier, &routes),
        )
        .await
        .expect("handshake should succeed");
        let authed = authenticate_session(&mut handle, "tester", &Credential::Password("x".into())).await.unwrap();
        assert!(authed, "the forward server accepts any password");

        handle.streamlocal_forward(remote_path).await.expect("streamlocal_forward should be accepted");

        let mut channel = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a forwarded channel should arrive before the timeout")
            .expect("the route sender must not have been dropped");

        let mut got = Vec::new();
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => got.extend_from_slice(&data),
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        assert_eq!(got, b"ctl-payload\n", "the forwarded channel's bytes must reach the registered route");
    }

    /// A forwarded channel whose socket path has no registered route is simply
    /// dropped (closed): [`ForwardRoutes::dispatch`] returns `false`, and the
    /// handler's default is to close the channel rather than error the session.
    /// Registering a *different* path proves the un-routed one never arrives.
    #[tokio::test]
    async fn streamlocal_forward_without_a_matching_route_is_dropped() {
        use tokio::time::{timeout, Duration};

        let addr = spawn_server(StreamlocalForwardServer, 13).await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let routes = ForwardRoutes::new();
        // Register some *other* path; the server will forward for the path we
        // actually request, which has no route.
        let mut rx = routes.register("/tmp/some-other-path.sock");

        let mut handle = establish_over_stream(
            Arc::new(client::Config::default()),
            tokio::net::TcpStream::connect(addr).await.unwrap(),
            verifying_handler_with_routes(&verifier, &routes),
        )
        .await
        .expect("handshake should succeed");
        assert!(authenticate_session(&mut handle, "tester", &Credential::Password("x".into())).await.unwrap());

        handle.streamlocal_forward("/tmp/isekai-pipe-ctl-unrouted.sock").await.expect("forward accepted");

        // The unrouted channel must not be misdelivered to the other path's
        // receiver; a short wait that times out is the expected outcome.
        assert!(
            timeout(Duration::from_millis(300), rx.recv()).await.is_err(),
            "a forwarded channel for an unregistered path must not reach an unrelated route"
        );
    }

    #[tokio::test]
    async fn jump_connection_constructs_jump_then_target_legs_in_order() {
        let target_addr = spawn_server(EchoExecServer, 10).await;
        let jump_addr = spawn_server(JumpServer, 11).await;

        let jump = JumpHost {
            host: jump_addr.ip().to_string(),
            port: jump_addr.port(),
            username: "jumper".into(),
            credential: Credential::Password("correct-password".into()),
        };

        let verifier = Arc::new(AcceptAllHostKeys);
        let legs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let legs_recorder = legs.clone();
        let _session = connect_via_jump_or_direct(
            Some(&jump), Arc::new(client::Config::default()), &target_addr.ip().to_string(), target_addr.port(),
            move |leg| {
                legs_recorder.lock().unwrap().push(leg);
                verifying_handler(&verifier)
            },
        )
        .await
        .expect("jump connect should succeed");

        assert_eq!(
            *legs.lock().unwrap(),
            vec![ConnectionLeg::Jump, ConnectionLeg::Target],
            "a jump connection must build the jump-leg handler first, then the target-leg handler"
        );
    }
}
