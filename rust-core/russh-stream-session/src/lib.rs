//! Establish and authenticate an SSH client session (built on [`russh`]) over
//! any `AsyncRead + AsyncWrite` byte stream — not just a raw TCP socket.
//!
//! Host-key verification is delegated to [`HostKeyVerifier`], so this crate
//! has no opinion on how (or whether) a caller persists a trust-on-first-use
//! store. Port forwarding, SSH agent forwarding, and any other
//! application-specific channel protocol are deliberately out of scope —
//! this crate covers exactly "authenticate a `russh::client::Handle` and
//! open one session channel (shell or exec)"; the I/O loop past that point
//! is left to the caller.
//!
//! [`russh`]: https://docs.rs/russh

use std::sync::Arc;

use async_trait::async_trait;
use russh::client;
use russh_keys::{HashAlg, PrivateKey, PublicKey};

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
    #[error("channel operation failed: {0}")]
    Channel(russh::Error),
}

/// Verifies a server's host-key fingerprint (SHA-256, as produced by
/// `PublicKey::fingerprint(HashAlg::Sha256)`). Return `true` to accept the
/// connection, `false` to abort the handshake. Implementations typically
/// consult a trust-on-first-use store and/or prompt the user.
#[async_trait]
pub trait HostKeyVerifier: Send + Sync {
    async fn verify(&self, fingerprint: &str) -> bool;
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

/// A single-hop jump host (`ssh -J` equivalent) to tunnel through before
/// reaching the real target.
pub struct JumpHost {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub credential: Credential,
}

/// A minimal `client::Handler` that does nothing but delegate host-key
/// verification to a [`HostKeyVerifier`]. All other `client::Handler`
/// methods use russh's defaults (reject/no-op), so this handler is only
/// suitable for sessions that don't need agent forwarding, remote port
/// forwards, or other server-initiated channel requests — callers that need
/// those should implement their own `client::Handler`.
pub struct VerifyingHandler<V> {
    verifier: Arc<V>,
}

#[async_trait]
impl<V: HostKeyVerifier + 'static> client::Handler for VerifyingHandler<V> {
    type Error = russh::Error;

    async fn check_server_key(&mut self, server_public_key: &PublicKey) -> Result<bool, Self::Error> {
        let fingerprint = server_public_key.fingerprint(HashAlg::Sha256).to_string();
        Ok(self.verifier.verify(&fingerprint).await)
    }
}

/// An established (not yet authenticated) SSH connection, possibly tunneled
/// through a jump host. The jump host's own `client::Handle` (if any) is
/// kept alive internally for as long as this session is in use — dropping
/// [`Session`] tears down the tunnel too.
pub struct Session<V: HostKeyVerifier + 'static> {
    pub handle: client::Handle<VerifyingHandler<V>>,
    _jump_handle: Option<client::Handle<VerifyingHandler<V>>>,
}

/// Connects to `target_host:target_port`, either directly or (if `jump` is
/// given) by first authenticating to the jump host and tunneling through a
/// `direct-tcpip` channel (`ssh -J` equivalent, single hop). The returned
/// [`Session`] is connected but not yet authenticated to the target —
/// call [`authenticate_session`] next.
pub async fn connect_via_jump_or_direct<V>(
    jump: Option<&JumpHost>,
    russh_config: Arc<client::Config>,
    target_host: &str,
    target_port: u16,
    verifier: Arc<V>,
) -> Result<Session<V>, SessionError>
where
    V: HostKeyVerifier + 'static,
{
    let Some(jump) = jump else {
        let addr = format!("{target_host}:{target_port}");
        let handler = VerifyingHandler { verifier };
        let handle = client::connect(russh_config, addr.as_str(), handler)
            .await
            .map_err(|source| SessionError::Connect { addr, source })?;
        return Ok(Session { handle, _jump_handle: None });
    };

    let jump_addr = format!("{}:{}", jump.host, jump.port);
    let jump_handler = VerifyingHandler { verifier: verifier.clone() };
    let mut jump_handle = client::connect(russh_config.clone(), jump_addr.as_str(), jump_handler)
        .await
        .map_err(|source| SessionError::Connect { addr: jump_addr.clone(), source })?;

    let authenticated = authenticate_session(&mut jump_handle, &jump.username, &jump.credential).await;
    if !authenticated {
        return Err(SessionError::JumpAuthFailed { username: jump.username.clone(), addr: jump_addr });
    }

    let channel = jump_handle
        .channel_open_direct_tcpip(target_host, target_port as u32, "127.0.0.1", 0)
        .await
        .map_err(|source| SessionError::JumpTunnel { host: target_host.to_string(), port: target_port, source })?;
    let stream = channel.into_stream();

    let target_handler = VerifyingHandler { verifier };
    let handle = client::connect_stream(russh_config, stream, target_handler)
        .await
        .map_err(SessionError::Handshake)?;

    Ok(Session { handle, _jump_handle: Some(jump_handle) })
}

/// Runs the SSH handshake directly over an already-established byte stream
/// (e.g. a QUIC stream, or any other tunnel a caller has already set up) —
/// the "isekai-pipe QUIC transport" case, where the caller has its own way
/// of reaching the target and just needs SSH layered on top. Not yet
/// authenticated — call [`authenticate_session`] next.
pub async fn establish_over_stream<S, V>(
    russh_config: Arc<client::Config>,
    stream: S,
    verifier: Arc<V>,
) -> Result<client::Handle<VerifyingHandler<V>>, SessionError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    V: HostKeyVerifier + 'static,
{
    let handler = VerifyingHandler { verifier };
    client::connect_stream(russh_config, stream, handler).await.map_err(SessionError::Handshake)
}

/// Authenticates `session` as `username` using `credential`. Returns whether
/// authentication succeeded. Does not zeroize `credential` — call
/// [`Credential::zeroize`] once you're done with it.
pub async fn authenticate_session<H: client::Handler>(
    session: &mut client::Handle<H>,
    username: &str,
    credential: &Credential,
) -> bool {
    match credential {
        Credential::Password(password) => {
            session.authenticate_password(username, password).await.ok().unwrap_or(false)
        }
        Credential::PublicKey { private_key_pem } => match PrivateKey::from_openssh(private_key_pem) {
            Ok(key) => session.authenticate_publickey(username, Arc::new(key)).await.ok().unwrap_or(false),
            Err(_) => false,
        },
    }
}

/// What kind of session channel to open: an interactive PTY+shell, or a
/// single non-interactive command (`ssh host 'command'` equivalent).
pub enum SessionKind {
    Shell { term: String, cols: u32, rows: u32 },
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
        SessionKind::Shell { term, cols, rows } => {
            channel.request_pty(false, term, *cols, *rows, 0, 0, &[]).await.map_err(SessionError::Channel)?;
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
        async fn verify(&self, _fingerprint: &str) -> bool {
            true
        }
    }

    /// Minimal server: accepts any password, accepts session-channel opens,
    /// and echoes exec commands back as a single line of output followed by
    /// exit status 0. Enough to prove `open_channel`'s `Exec` path actually
    /// round-trips data through a real (in-process) SSH server.
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

        async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
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
            None, Arc::new(client::Config::default()), &addr.ip().to_string(), addr.port(), verifier,
        )
        .await
        .expect("direct connect should succeed");

        let authed = authenticate_session(
            &mut session.handle, "tester", &Credential::Password("anything".into()),
        )
        .await;
        assert!(authed, "password auth should succeed against a server that accepts any password");

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
            credential: Credential::Password("anything".into()),
        };

        let verifier = Arc::new(AcceptAllHostKeys);
        let mut session = connect_via_jump_or_direct(
            Some(&jump), Arc::new(client::Config::default()), &target_addr.ip().to_string(), target_addr.port(), verifier,
        )
        .await
        .expect("jump connect should succeed");

        let authed = authenticate_session(
            &mut session.handle, "tester", &Credential::Password("anything".into()),
        )
        .await;
        assert!(authed, "authentication over the jump-tunneled session should succeed");

        // Confirm the tunneled session behaves like an ordinary connection
        // beyond just authenticating: open a real channel through it.
        session.handle.channel_open_session().await.expect("opening a channel through the jump tunnel should succeed");
    }
}
