//! `ControlMaster`-equivalent multiplexer for the Windows-native path: when
//! several tabs each run `isekai-ssh <host>` to the *same* fully-resolved
//! destination, exactly one process (the *owner*) holds the single
//! authenticated `russh` connection and every other process (a *client*)
//! reaches its own private remote shell through the owner over a
//! `local-ipc-mux` named-pipe channel, instead of each independently
//! re-authenticating a fresh SSH connection.
//!
//! Submodules: [`protocol`] (the SSH-specific frame codec), [`naming`] (how a
//! resolved config maps to a pipe name), [`owner`] (the accept loop + per-client
//! relay over the shared handle), [`client`] (the local terminal ↔ owner
//! relay). The generic dispatch ([`dispatch`]) is written against the
//! [`local_ipc_mux::ExclusiveChannel`] trait so it's unit-tested end-to-end
//! with `InMemoryChannel`; [`run`] is the one place that names the concrete
//! `WindowsNamedPipeChannel`.
//!
//! ## Relationship to the declined "standing QUIC broker" ADR
//!
//! `ISEKAI_PIPE_DESIGN.md`'s ADR *「複数isekai-sshプロセスによるisekai-pipe共有
//! (マルチプレクス)」* declined to build a standing QUIC broker for sharing an
//! `isekai-pipe` **transport** across processes, on the grounds that SSH's own
//! `ControlMaster`/`ControlPersist` (CLI) already solves it more simply — and
//! it listed an explicit reconsideration trigger: *「ControlMasterが使えない
//! クライアントが主要用途になった」*. Windows without a real `ssh(1)`/ControlMaster
//! is exactly that situation, so this feature deliberately revisits that
//! trigger.
//!
//! Crucially this is **a different kind of thing** from what the ADR declined:
//! it shares the SSH *protocol-layer* `client::Handle` (which multiplexes
//! independent channels natively), not a QUIC transport broker. The ADR's list
//! of costs it declined to pay still applies, and is addressed (or knowingly
//! accepted) here rather than dismissed:
//!
//! * **常駐broker / process lifecycle**: no separate daemon — the owner *is* a
//!   normal `isekai-ssh` tab that also serves siblings. Its lifetime is tied to
//!   its own foreground shell (see the "known limitation" below), so there is
//!   no daemon to supervise, upgrade, or reap.
//! * **ローカルIPC / multiplex protocol**: [`local_ipc_mux`] (named pipe, same-
//!   user ACL) plus this crate's small versioned frame protocol ([`protocol`]),
//!   with an explicit size cap, version field, and auth token.
//! * **crash recovery / re-election**: deliberately *not* an election. If the
//!   owner dies, each client's multiplexed shell is gone too, so a client just
//!   reports the loss and exits ([`client::ClientOutcome::OwnerLost`] →
//!   [`crate::EXIT_MUX_OWNER_LOST`]); a fresh `isekai-ssh <host>` becomes the
//!   new owner through the ordinary claim path.
//! * **session isolation**: every client gets an independent SSH shell channel;
//!   one client's error is logged and contained ([`owner::serve_clients`]),
//!   never propagated to siblings or the owner.
//! * **per-session flow control**: each relay is a single sequential loop per
//!   direction, so a slow client back-pressures only its own SSH channel (see
//!   [`owner`]'s module docs).
//! * **stale session cleanup**: an owner exit drops the handle (closing all
//!   channels); a client exit drops its pipe connection (the owner's relay task
//!   ends and closes that one channel). The token file is the only on-disk
//!   artifact and is best-effort unlinked by the owner on exit.
//!
//! **Known limitation (deferred)**: true `ControlPersist` — the shared
//! connection outliving the tab that created it — is *not* implemented. The
//! owner tears down when its own foreground shell exits, at which point
//! connected clients hit the owner-lost path and must reconnect (becoming the
//! new owner). Decoupling the master's lifetime from its initiator needs a
//! detached background master process, which is out of scope for this pass and
//! left as follow-up work.

pub(crate) mod build_relay;
pub(crate) mod client;
pub(crate) mod ctl_forward;
pub(crate) mod holder;
pub(crate) mod naming;
pub(crate) mod owner;
pub(crate) mod protocol;

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use local_ipc_mux::{ConnectError, ExclusiveChannel};

use crate::log_file::log_line;
use crate::native::connect::{self, OwnerHook, Prepared};
use holder::HolderSpawner;

/// How long the foreground process waits, after successfully spawning a
/// detached holder, for that holder to actually claim the channel and start
/// accepting before giving up and falling back to a plain direct connect — a
/// slow or failed holder must never block this tab from connecting at all
/// (the always-connects principle).
const HOLDER_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// `isekai-ssh <destination>` entrypoint on Windows: resolves the config, then
/// dispatches through the concrete named-pipe channel. Swapping in a different
/// [`ExclusiveChannel`] implementation later (e.g. a Unix one) is the single
/// concrete type here.
#[cfg(windows)]
pub(crate) async fn run(args: Vec<String>) -> Result<u8> {
    let prepared = connect::prepare(args.clone()).await?;
    dispatch::<local_ipc_mux::WindowsNamedPipeChannel, _>(prepared, args, &holder::DetachedProcessSpawner).await
}

/// The holder re-exec entrypoint: `main.rs` calls this instead of [`run`] when
/// [`holder::is_holder_reexec`] is true. Claims the channel and serves clients
/// only — no foreground shell (see [`run_as_holder`]'s docs).
#[cfg(windows)]
pub(crate) async fn run_as_holder_entrypoint(args: Vec<String>) -> Result<u8> {
    let prepared = connect::prepare(args).await?;
    let channel_name = naming::channel_name(prepared.host_config(), prepared.resolution(), prepared.plan().destination_host());
    let token_path = prepared.runtime_dir().join(naming::token_file_name(&channel_name));
    let holder_channel = local_ipc_mux::WindowsNamedPipeChannel::try_claim(&channel_name)
        .await
        .context("isekai-ssh mux holder: failed to claim the channel it was spawned to serve")?;
    run_as_holder(prepared, holder_channel, &token_path).await
}

/// The role-selecting core, generic over the IPC channel (and the holder
/// spawner) so it's testable with `InMemoryChannel`/a fake spawner. The
/// foreground process is **always a client**: it never claims the channel
/// itself. If no holder is currently listening, it spawns a detached one
/// (Phase 1 `ControlPersist`-equivalent redesign — see this module's docs)
/// and retries as a client; any failure along the way (spawn failure, the
/// holder never coming up, a genuine pipe-infrastructure problem) falls back
/// to a plain single-process connect so a mux hiccup never blocks connecting
/// at all (the always-connects principle).
async fn dispatch<C, S>(prepared: Prepared, holder_args: Vec<String>, spawner: &S) -> Result<u8>
where
    C: ExclusiveChannel + Send + 'static,
    S: HolderSpawner,
{
    let channel_name = naming::channel_name(prepared.host_config(), prepared.resolution(), prepared.plan().destination_host());
    let token_path = prepared.runtime_dir().join(naming::token_file_name(&channel_name));

    match C::connect(&channel_name).await {
        Ok(conn) => run_as_client_over(prepared, conn, &token_path).await,
        Err(ConnectError::NotFound { .. }) => {
            if let Err(e) = spawner.spawn(&holder_args, None) {
                log_line!("isekai-ssh: failed to spawn a detached mux holder ({e}); connecting directly");
                return connect::run_prepared(prepared, None).await;
            }
            match connect_with_retry::<C>(&channel_name, HOLDER_STARTUP_TIMEOUT).await {
                Ok(conn) => run_as_client_over(prepared, conn, &token_path).await,
                Err(e) => {
                    log_line!("isekai-ssh: the detached mux holder never came up ({e}); connecting directly");
                    connect::run_prepared(prepared, None).await
                }
            }
        }
        // A transient local-pipe I/O error (not "no owner exists" — e.g. the
        // pipe infrastructure itself failed, or `ERROR_PIPE_BUSY` retries
        // exhausted reaching a holder that does exist). Per the
        // always-connects principle a mux hiccup must never block
        // connecting, so fall back rather than hard-failing this invocation.
        Err(ConnectError::Io { source, .. }) => {
            log_line!("isekai-ssh: local mux channel unavailable ({source}); connecting directly without multiplexing");
            connect::run_prepared(prepared, None).await
        }
    }
}

/// Retries [`ExclusiveChannel::connect`] while it keeps reporting
/// [`ConnectError::NotFound`] (the holder hasn't claimed the channel yet),
/// giving up once `deadline` has elapsed. Any other error (or a `NotFound`
/// past the deadline) is returned to the caller, which falls back to a plain
/// direct connect.
async fn connect_with_retry<C>(channel_name: &str, deadline: Duration) -> Result<C::Connection, ConnectError>
where
    C: ExclusiveChannel,
{
    let start = tokio::time::Instant::now();
    loop {
        match C::connect(channel_name).await {
            Ok(conn) => return Ok(conn),
            Err(ConnectError::NotFound { .. }) if start.elapsed() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Claimed by no one *yet*: this process's foreground path spawned a detached
/// holder (or is a re-exec'd holder itself) which claims the channel, writes
/// the per-session auth token, then runs the ordinary connect+auth+recovery
/// with an [`OwnerHook`] that starts accepting clients the moment the shared
/// session authenticates. Unlike a plain single-process connect, this process
/// opens **no foreground shell of its own** — [`connect::run_prepared`]
/// returns as soon as the accept loop itself ends (idle-exit or a fatal
/// local-IPC error), which is this function's entire body once the hook
/// fires (see [`OwnerHook`]'s docs on why `run_authenticated_session` skips
/// the shell in this mode).
async fn run_as_holder<C>(prepared: Prepared, holder_channel: C, token_path: &Path) -> Result<u8>
where
    C: ExclusiveChannel + Send + 'static,
{
    let token = Arc::new(write_owner_token(token_path)?);
    let cleanup_path = token_path.to_path_buf();
    let hook: OwnerHook = Box::new(move |handle, ctl_routes| {
        tokio::spawn(async move {
            if let Err(e) = owner::serve_clients(holder_channel, handle, token, ctl_routes).await {
                log_line!("isekai-ssh mux holder: the client accept loop ended: {e:#}");
            }
        })
    });
    let result = connect::run_prepared(prepared, Some(hook)).await;
    // Best-effort: don't leave the token file behind once this holder exits.
    let _ = std::fs::remove_file(&cleanup_path);
    result
}

/// Relays this terminal to an already-connected holder. If the holder rejects
/// the connection before any shell session existed, falls back to a plain
/// single-process connect.
async fn run_as_client_over<Conn>(prepared: Prepared, conn: Conn, token_path: &Path) -> Result<u8>
where
    Conn: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Extracted before `prepared` potentially moves into `connect::run_prepared`
    // below (the fallback branches take it by value) — this is the same host/
    // profile identity Epic P Phase 2's build-profile lookup uses on the Unix
    // path (`resolution.profile()`).
    let host = prepared.resolution().profile().to_string();
    let token = match read_owner_token_or_fall_back(token_path) {
        ClientToken::Ready(token) => token,
        // The holder released its claim (or hadn't finished writing the token
        // file) in the race between our successful connect and now. A mux
        // hiccup must never block connecting (the always-connects
        // principle) — dial SSH ourselves, unmultiplexed.
        ClientToken::FallBack => return connect::run_prepared(prepared, None).await,
    };
    match client::run(conn, &token, host).await? {
        client::ClientRunResult::ExitCode(code) => Ok(code),
        // The holder rejected us before any shell session existed (protocol
        // version mismatch, or a stale token read in the window before a new
        // holder rewrote it — see `ClientOutcome::Rejected`'s docs). Nothing
        // was lost, so it's always safe to fall back to a fresh unmultiplexed
        // connect rather than fail this invocation outright.
        client::ClientRunResult::Rejected { reason } => {
            log_line!("isekai-ssh: the mux holder rejected this connection ({reason}); connecting directly");
            connect::run_prepared(prepared, None).await
        }
    }
}

/// Generates a fresh 32-byte token and writes it where only the owning OS user
/// can read it. On Unix the file is chmod 0600 (belt-and-suspenders for the
/// Linux test build); on Windows the runtime dir already lives under the user
/// profile, so the named pipe's same-user ACL is the primary control and this
/// token is defense-in-depth beneath it.
fn write_owner_token(path: &Path) -> Result<Vec<u8>> {
    use rand::RngCore as _;
    let mut token = vec![0u8; 32];
    rand::thread_rng().fill_bytes(&mut token);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating mux token dir {}", parent.display()))?;
    }
    std::fs::write(path, &token).with_context(|| format!("writing mux token file {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on mux token file {}", path.display()))?;
    }
    Ok(token)
}

/// Whether a would-be client obtained the owner's token, or must fall back to a
/// plain single-process connect.
enum ClientToken {
    /// The token was read — connect to the owner and relay to it.
    Ready(Vec<u8>),
    /// The token couldn't be read (the owner released its claim, or hadn't
    /// finished writing the token file, in the claim race). Per the
    /// always-connects principle a mux hiccup must never block connecting, so
    /// the caller connects directly (unmultiplexed) instead of failing.
    FallBack,
}

/// Reads the owner's token, degrading to [`ClientToken::FallBack`] (logging the
/// cause) rather than erroring when it can't be read — so a lost/racing owner
/// never turns a would-be client into a hard connect failure.
fn read_owner_token_or_fall_back(path: &Path) -> ClientToken {
    match read_owner_token(path) {
        Ok(token) => ClientToken::Ready(token),
        Err(e) => {
            log_line!("isekai-ssh: could not read the mux owner's auth token ({e:#}); connecting directly");
            ClientToken::FallBack
        }
    }
}

/// Reads the owner's token, retrying briefly to cover the small window where a
/// client's claim failed but the freshly-elected owner hasn't finished writing
/// the token file yet.
fn read_owner_token(path: &Path) -> Result<Vec<u8>> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match std::fs::read(path) {
            Ok(token) => return Ok(token),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(anyhow::Error::new(e).context(format!("reading mux token file {}", path.display()))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use local_ipc_mux::{ClaimError, InMemoryChannel};
    use russh::client;
    use russh::server::{self, Auth, Msg as ServerMsg, Server as _, Session as ServerSession};
    use russh::{Channel as RusshChannel, CryptoVec};
    use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey as SshPrivateKey};
    use russh_stream_session::{authenticate_session, establish_over_stream, verifying_handler, Credential, HostKeyVerifier, VerifyOutcome};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    struct AcceptAllHostKeys;
    #[async_trait]
    impl HostKeyVerifier for AcceptAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> VerifyOutcome {
            VerifyOutcome::Accepted
        }
    }

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
        async fn auth_password(&mut self, _u: &str, _p: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }
        async fn channel_open_session(&mut self, _c: RusshChannel<ServerMsg>, _s: &mut ServerSession) -> Result<bool, Self::Error> {
            Ok(true)
        }
        async fn shell_request(&mut self, channel: russh::ChannelId, session: &mut ServerSession) -> Result<(), Self::Error> {
            session.data(channel, CryptoVec::from(b"ready\n".to_vec()))?;
            Ok(())
        }
        // Echo stdin back, then cleanly end the session so the client's relay
        // terminates deterministically (no timeout) with a real Exit(0).
        async fn data(&mut self, channel: russh::ChannelId, data: &[u8], session: &mut ServerSession) -> Result<(), Self::Error> {
            session.data(channel, CryptoVec::from(data.to_vec()))?;
            session.exit_status_request(channel, 0)?;
            session.close(channel)?;
            Ok(())
        }
    }

    async fn authed_handle() -> client::Handle<russh_stream_session::VerifyingHandler<AcceptAllHostKeys>> {
        let keypair = Ed25519Keypair::from_seed(&[130; 32]);
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

    /// The full owner+client path over `InMemoryChannel`: an owner serves an
    /// accept loop on a real (mock) SSH handle; a client connects through the
    /// channel, drives `client::run_inner` with canned stdin, and receives the
    /// remote shell banner plus its echoed stdin relayed all the way back —
    /// proving the two halves interoperate through the actual frame protocol.
    #[tokio::test]
    async fn owner_and_client_relay_end_to_end_over_in_memory_channel() {
        let name = "isekai-ssh-mux-e2e-test";
        let token = Arc::new(b"shared-secret-token".to_vec());
        let handle = authed_handle().await;

        let owner_channel = InMemoryChannel::try_claim(name).await.unwrap();
        let serve_token = token.clone();
        tokio::spawn(async move {
            let _ = owner::serve_clients(owner_channel, Arc::new(tokio::sync::Mutex::new(handle)), serve_token, None).await;
        });

        // A second try_claim must fail (owner exists) — the real dispatch's
        // signal to become a client.
        assert!(matches!(InMemoryChannel::try_claim(name).await, Err(ClaimError::AlreadyClaimed { .. })));

        let conn = InMemoryChannel::connect(name).await.unwrap();
        let (cr, mut cw) = tokio::io::split(conn);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        // Drive the real client relay: it sends Hello, streams "hello\n" then
        // EOF, and receives the banner + echoed stdin back before the mock
        // shell cleanly exits (Exit(0)). No timeout: the server ends the
        // session deterministically after echoing.
        // `super::client` (the mux client module), not `russh::client` which
        // is imported as `client` above for `client::Handle`.
        let outcome = super::client::run_inner(cr, &mut cw, &token, "xterm".to_string(), 80, 24, &b"hello\n"[..], &mut stdout, &mut stderr, None, "mybox".to_string())
            .await
            .unwrap();

        assert_eq!(outcome, super::client::ClientOutcome::Exited(0), "a clean remote exit must reach the client as Exited(0)");
        assert!(
            stdout.windows(6).any(|w| w == b"ready\n"),
            "the remote banner must be relayed to the client's stdout, saw {:?}",
            String::from_utf8_lossy(&stdout)
        );
        assert!(
            stdout.windows(6).any(|w| w == b"hello\n"),
            "the client's stdin must be echoed back through the remote shell, saw {:?}",
            String::from_utf8_lossy(&stdout)
        );
    }

    /// A missing owner token (the owner released its claim / hadn't written the
    /// file in the claim race) must degrade to a fall-back single-process
    /// connect, not a hard error — the always-connects principle for a mux
    /// hiccup. Guards `run_as_client`'s token-read step.
    #[test]
    fn a_missing_owner_token_falls_back_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such.token");
        assert!(
            matches!(read_owner_token_or_fall_back(&missing), ClientToken::FallBack),
            "a token that can't be read must fall back to a direct connect, never fail"
        );
    }

    /// The happy path still yields the real token so a client relays to the
    /// owner rather than needlessly falling back.
    #[test]
    fn a_present_owner_token_is_used_rather_than_falling_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mux.token");
        let written = write_owner_token(&path).unwrap();
        match read_owner_token_or_fall_back(&path) {
            ClientToken::Ready(token) => assert_eq!(token, written, "the token used must be the one on disk"),
            ClientToken::FallBack => panic!("a readable token must be used, not fall back to a direct connect"),
        }
    }

    #[test]
    fn token_write_then_read_round_trips_and_is_restricted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("mux.token");
        let written = write_owner_token(&path).unwrap();
        assert_eq!(written.len(), 32, "token must be 32 bytes");
        let read = read_owner_token(&path).unwrap();
        assert_eq!(written, read, "the token read back must match what was written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the token file must be owner-only (0600)");
        }
    }

    // -- connect_with_retry ---------------------------------------------

    #[tokio::test]
    async fn connect_with_retry_succeeds_immediately_when_a_holder_is_already_there() {
        let name = "isekai-ssh-mux-retry-immediate-test";
        // `connect` requires no `accept` on the other side to succeed at this
        // layer (see `InMemoryChannel::connect`'s implementation) — the claim
        // just needs to stay alive for the channel to be reachable, so
        // leaking it (never dropped) keeps it alive without an explicit
        // `accept` loop this test doesn't need.
        let owner_channel = InMemoryChannel::try_claim(name).await.unwrap();
        std::mem::forget(owner_channel);
        let result = connect_with_retry::<InMemoryChannel>(name, Duration::from_secs(1)).await;
        assert!(result.is_ok(), "an already-claimed channel must connect on the first try, not retry");
    }

    #[tokio::test]
    async fn connect_with_retry_succeeds_once_a_holder_claims_the_channel_mid_wait() {
        tokio::time::pause();
        let name = "isekai-ssh-mux-retry-eventual-test";
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _channel = InMemoryChannel::try_claim(name).await.expect("the simulated holder must win the claim");
            // Held alive for the rest of the test via this task's own scope.
            std::future::pending::<()>().await;
        });
        let result = connect_with_retry::<InMemoryChannel>(name, Duration::from_secs(5)).await;
        assert!(result.is_ok(), "a holder claiming the channel mid-wait must eventually be reachable, not time out");
    }

    #[tokio::test]
    async fn connect_with_retry_gives_up_once_the_deadline_elapses() {
        tokio::time::pause();
        let name = "isekai-ssh-mux-retry-never-test";
        let result = connect_with_retry::<InMemoryChannel>(name, Duration::from_millis(200)).await;
        assert!(matches!(result, Err(ConnectError::NotFound { .. })), "a channel nobody ever claims must give up past the deadline, not hang forever");
    }

    // -- dispatch fallback sequencing ------------------------------------

    /// Builds a throwaway `Prepared` for `dispatch` tests: a bogus
    /// `--isekai-pipe-path` makes the ultimate fallback (`connect::run_prepared`)
    /// fail fast and deterministically instead of hanging on a real network
    /// dial, mirroring `native/connect.rs`'s own `bogus_pipe`-based recovery
    /// tests.
    fn test_prepared(destination: &str, runtime_dir: &std::path::Path) -> connect::Prepared {
        use isekai_pipe_core::{BootstrapProvenance, ConnectionIntent, IntentTransport, ServerIdentity};

        let bogus_pipe = std::env::temp_dir().join(format!("isekai-mux-dispatch-test-nonexistent-pipe-binary-{destination}"));
        let plan = crate::wrapper::parse_wrapper(vec!["--isekai-pipe-path".to_string(), bogus_pipe.display().to_string(), destination.to_string()])
            .expect("parse_wrapper");
        let (resolution, host_config) = crate::wrapper::resolve_for_native(&plan).expect("resolve_for_native");
        let intent = ConnectionIntent::new(
            destination,
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::Relay {
                helper_addr: "203.0.113.5:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "example.com:22".to_string() },
        );
        connect::Prepared::for_test(plan, resolution, host_config, intent, runtime_dir.to_path_buf())
    }

    /// A holder spawn failure must degrade `dispatch` to a plain direct
    /// connect, never a hard error of its own or a hang — the always-connects
    /// principle for a mux hiccup. The direct connect itself then fails too
    /// here (the bogus pipe path), but that's `test_prepared`'s own
    /// deliberately-unreachable target, not the mux layer misbehaving.
    #[tokio::test]
    async fn dispatch_falls_back_to_a_direct_connect_when_spawning_the_holder_fails() {
        let runtime_dir = tempfile::tempdir().unwrap();
        let prepared = test_prepared("dispatch-spawn-failure-host", runtime_dir.path());
        let spawner = holder::tests_support::RecordingSpawner::failing();

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            dispatch::<InMemoryChannel, _>(prepared, vec!["dispatch-spawn-failure-host".to_string()], &spawner),
        )
        .await
        .expect("dispatch must not hang when the holder spawn itself fails");

        assert!(result.is_err(), "the fallback direct connect against the bogus pipe path must still fail here, but via the fallback, not a hang");
        assert_eq!(spawner.calls.lock().unwrap().len(), 1, "dispatch must attempt to spawn a holder exactly once before falling back");
    }

    /// A holder that was spawned successfully but never actually claims the
    /// channel (crashed before `try_claim`, or was simply slow) must not wedge
    /// `dispatch` forever — it gives up once its own startup-wait deadline
    /// elapses and falls back to a direct connect, same as a spawn failure.
    #[tokio::test]
    async fn dispatch_falls_back_to_a_direct_connect_when_the_spawned_holder_never_claims_the_channel() {
        tokio::time::pause();
        let runtime_dir = tempfile::tempdir().unwrap();
        let prepared = test_prepared("dispatch-holder-never-arrives-host", runtime_dir.path());
        let spawner = holder::tests_support::RecordingSpawner::succeeding();

        let result = tokio::time::timeout(
            HOLDER_STARTUP_TIMEOUT + Duration::from_secs(5),
            dispatch::<InMemoryChannel, _>(prepared, vec!["dispatch-holder-never-arrives-host".to_string()], &spawner),
        )
        .await
        .expect("dispatch must give up waiting for the holder within its own startup timeout, not hang");

        assert!(result.is_err(), "a holder that spawned but never claims the channel must fall back to a direct connect (which itself fails against the bogus pipe path here, but via the fallback path, not a hang)");
    }
}
