//! Wires `russh_stream_session::authenticate_with_signer` (M0) to an
//! external SSH agent for the native path, per `openssh_config::HostConfig
//! ::identity_agent`.
//!
//! **Windows-only in practice**: the actual agent *connection* (named pipe
//! or Pageant) only exists on `cfg(windows)` — see [`connect_agent`]. The
//! identity-selection logic ([`try_each_identity`]) is platform-generic and
//! tested here on Linux against a fake in-process `Signer` (same technique
//! `russh-stream-session`'s own `authenticate_with_signer` test uses), since
//! it doesn't care what's on the other end of the `Signer` trait.
//!
//! `russh-keys` (0.48.1) already provides `AgentClient::connect_named_pipe`/
//! `connect_pageant`, and `russh` already provides a blanket `Signer` impl
//! for `AgentClient<S>` — this module is glue, not new protocol
//! implementation.

use russh::client;
use russh_keys::ssh_key::PublicKey;
use russh_stream_session::authenticate_with_signer;

/// Well-known Windows OpenSSH agent named pipe (`ssh-agent` service default,
/// matching `ssh_config(5)`'s own default when `IdentityAgent` is unset —
/// same convention Win32-OpenSSH's own `ssh.exe` uses).
pub(crate) const DEFAULT_WINDOWS_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

/// What agent (if any) to try, resolved from `openssh_config::HostConfig
/// ::identity_agent`'s raw value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentTarget {
    /// No `IdentityAgent` configured — try the platform default.
    Default,
    /// `IdentityAgent none` — explicitly disabled, never try any agent.
    None,
    /// `IdentityAgent <path>` — a specific named pipe (or, in principle, a
    /// Unix socket path, though the native path is Windows-only in
    /// practice).
    Path(String),
}

/// Resolves `openssh_config::HostConfig::identity_agent`'s raw value (a
/// `PathBuf` that may hold a real path or the sentinel `"none"`/
/// `"SSH_AUTH_SOCK"` — see that crate's docs) into an [`AgentTarget`].
pub(crate) fn resolve_agent_target(identity_agent: Option<&std::path::Path>) -> AgentTarget {
    resolve_agent_target_from(identity_agent, |key| std::env::var(key).ok())
}

/// Pure helper split out of [`resolve_agent_target`] purely so the
/// `SSH_AUTH_SOCK` sentinel can be unit-tested with an injected environment
/// lookup instead of mutating the real process environment
/// (`std::env::set_var` is process-global and races against
/// concurrently-running tests — same rationale as
/// `isekai-fs-guard::resolve_home_dir_from`/`openssh_config::expand_tilde_with`).
///
/// Codex review finding: `IdentityAgent SSH_AUTH_SOCK` is a documented
/// `ssh_config(5)` sentinel meaning "use whatever the `SSH_AUTH_SOCK`
/// environment variable currently holds" — it is not a literal named pipe
/// called `SSH_AUTH_SOCK`. The first version of this function treated it as
/// a literal path, which would have tried (and failed) to connect to a pipe
/// that doesn't exist instead of resolving the env var.
fn resolve_agent_target_from(
    identity_agent: Option<&std::path::Path>,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> AgentTarget {
    match identity_agent {
        None => AgentTarget::Default,
        Some(path) => match path.to_str() {
            Some(s) if s.eq_ignore_ascii_case("none") => AgentTarget::None,
            Some(s) if s.eq_ignore_ascii_case("SSH_AUTH_SOCK") => match env_lookup("SSH_AUTH_SOCK") {
                Some(value) if !value.is_empty() => AgentTarget::Path(value),
                // The env var this sentinel explicitly points at isn't
                // set — there's nothing to connect to. Deliberately not
                // AgentTarget::Default: the user asked for this specific
                // env var, not "fall back to whatever the platform
                // default would have been".
                _ => AgentTarget::None,
            },
            _ => AgentTarget::Path(path.display().to_string()),
        },
    }
}

/// Whether a failure to *connect* to the agent should be treated as "no agent
/// is available" (fall through to the final `no configured private key or SSH
/// agent identity was accepted` guidance) rather than surfaced as a hard
/// error. Only the platform-*default* agent gets this grace: a default
/// `ssh-agent` that simply isn't running is the ordinary Windows state (the
/// service is opt-in), and reporting its named-pipe connect failure would bury
/// the real, actionable message under a confusing error about an agent the
/// user never configured. An explicitly configured `IdentityAgent <path>` that
/// fails to connect, by contrast, is a genuine misconfiguration the user asked
/// for and must keep its hard error. ([`AgentTarget::None`] never connects at
/// all — [`connect_agent`] returns `Ok(None)` for it — so it never reaches
/// this check in practice; `false` is the correct, harmless answer regardless.)
pub(crate) fn agent_connect_failure_is_benign(target: &AgentTarget) -> bool {
    matches!(target, AgentTarget::Default)
}

/// Connects to the agent named by `target`, Windows-only (named pipe or
/// Pageant) — the actual point of this whole module. Not exercised by any
/// test in this codebase: doing so needs a real Windows OpenSSH agent or
/// Pageant instance, neither of which exists in this Linux development
/// environment. Verified only via `cargo check --target
/// x86_64-pc-windows-gnu` (cross-compiles cleanly) — a real Windows machine
/// must confirm this actually works before it's relied on.
#[cfg(windows)]
pub(crate) async fn connect_agent(
    target: &AgentTarget,
) -> anyhow::Result<Option<russh_keys::agent::client::AgentClient<tokio::net::windows::named_pipe::NamedPipeClient>>> {
    use anyhow::Context;

    let pipe_name = match target {
        AgentTarget::None => return Ok(None),
        AgentTarget::Default => DEFAULT_WINDOWS_AGENT_PIPE.to_string(),
        AgentTarget::Path(path) => path.clone(),
    };
    let agent = russh_keys::agent::client::AgentClient::connect_named_pipe(&pipe_name)
        .await
        .with_context(|| format!("failed to connect to SSH agent at {pipe_name}"))?;
    Ok(Some(agent))
}

/// Tries each of `identities` in order against `session` (matching `ssh(1)`
/// itself: on a per-agent-offered-identity basis, not "give up after the
/// first"), returning `true` as soon as one is accepted. `Ok(false)` if the
/// *server* rejected every identity (each cleanly declined at query time, so
/// the session stays usable for the next one). `Err` if the *signer/agent*
/// itself fails on a key — including a per-key refusal
/// (`AgentAuthError::Key(russh_keys::Error::AgentFailure)`, e.g. a declined
/// hardware-token touch) as well as the agent connection dropping
/// (`AgentAuthError::Send`).
///
/// **Why a signer error must abort and cannot "try the next key"** (this is
/// the counter-intuitive part, so don't "fix" it into a `continue`): russh
/// 0.48.2's session task, once the server answers a key query with `PK_OK`,
/// blocks in an inner loop that consumes messages until it sees `Msg::Signed`
/// (`russh/src/client/encrypted.rs`, the `FuturePublicKey` arm). If the signer
/// returns an error, `authenticate_publickey_with` returns without ever sending
/// `Msg::Signed`, leaving that task **permanently wedged** on the aborted key —
/// it silently discards every later message, including the `Msg::Authenticate`
/// a subsequent `authenticate_with_signer` call would send. Continuing to the
/// next identity on the same `session` therefore *hangs* rather than moving on
/// (observed directly: a mock agent that refuses one key deadlocked the whole
/// test). Recovering a declined key would require a fresh SSH connection, which
/// is out of this function's scope; a declined agent touch is also the kind of
/// inherently-interactive case `always-connects.md` exempts. So the honest
/// behavior is to surface the error and let the connection fail, not to hang
/// pretending we can offer the next key.
pub(crate) async fn try_each_identity<H, S>(
    session: &mut client::Handle<H>,
    username: &str,
    identities: &[PublicKey],
    signer: &mut S,
) -> Result<bool, russh_stream_session::SessionError>
where
    H: client::Handler,
    S: russh::Signer<Error = russh::AgentAuthError>,
{
    for identity in identities {
        if authenticate_with_signer(session, username, identity.clone(), signer).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
    use russh::Channel as RusshChannel;
    use russh_keys::ssh_key::private::{Ed25519Keypair, PrivateKey};
    use russh_stream_session::{verifying_handler, HostKeyVerifier, VerifyOutcome};
    use signature::Signer as _;
    use ssh_encoding::Encode;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::net::TcpListener;

    #[test]
    fn resolve_agent_target_unset_means_default() {
        assert_eq!(resolve_agent_target(None), AgentTarget::Default);
    }

    #[test]
    fn resolve_agent_target_none_sentinel_disables_agent() {
        assert_eq!(resolve_agent_target(Some(&PathBuf::from("none"))), AgentTarget::None);
        assert_eq!(resolve_agent_target(Some(&PathBuf::from("None"))), AgentTarget::None, "case-insensitive, like ssh_config(5)");
    }

    #[test]
    fn resolve_agent_target_explicit_path_is_used_verbatim() {
        assert_eq!(
            resolve_agent_target(Some(&PathBuf::from(r"\\.\pipe\my-custom-agent"))),
            AgentTarget::Path(r"\\.\pipe\my-custom-agent".to_string())
        );
    }

    #[test]
    fn resolve_agent_target_ssh_auth_sock_sentinel_reads_the_env_var() {
        let target = resolve_agent_target_from(
            Some(&PathBuf::from("SSH_AUTH_SOCK")),
            |key| if key == "SSH_AUTH_SOCK" { Some(r"\\.\pipe\from-env".to_string()) } else { None },
        );
        assert_eq!(target, AgentTarget::Path(r"\\.\pipe\from-env".to_string()));
    }

    #[test]
    fn resolve_agent_target_ssh_auth_sock_sentinel_is_case_insensitive() {
        let target = resolve_agent_target_from(
            Some(&PathBuf::from("ssh_auth_sock")),
            |_| Some(r"\\.\pipe\from-env".to_string()),
        );
        assert_eq!(target, AgentTarget::Path(r"\\.\pipe\from-env".to_string()));
    }

    #[test]
    fn resolve_agent_target_ssh_auth_sock_sentinel_with_unset_env_disables_agent() {
        let target = resolve_agent_target_from(Some(&PathBuf::from("SSH_AUTH_SOCK")), |_| None);
        assert_eq!(
            target, AgentTarget::None,
            "an explicit SSH_AUTH_SOCK sentinel with the env var unset must not silently fall back to the platform default"
        );
    }

    /// Regression: a *default* agent that fails to connect (the ordinary
    /// Windows state — the `ssh-agent` service isn't running) must be treated
    /// as "no agent available", letting `try_agent_auth` return `Ok(false)`
    /// and the caller reach its clear final error, rather than propagating a
    /// confusing named-pipe connect failure for an agent the user never
    /// configured. An *explicitly* configured `IdentityAgent <path>` that
    /// fails, by contrast, is a real misconfiguration and keeps its hard error.
    /// (`try_agent_auth` itself is `cfg(windows)`-only — it needs a real
    /// Windows agent transport — so the platform-generic *decision* is what's
    /// unit-tested here.)
    #[test]
    fn a_default_agent_connect_failure_is_benign_but_an_explicit_one_is_not() {
        assert!(
            agent_connect_failure_is_benign(&AgentTarget::Default),
            "a default ssh-agent that isn't running is the normal state, not a hard error"
        );
        assert!(
            !agent_connect_failure_is_benign(&AgentTarget::Path(r"\\.\pipe\configured-agent".to_string())),
            "an explicitly configured IdentityAgent that won't connect must surface as a real error"
        );
        assert!(
            !agent_connect_failure_is_benign(&AgentTarget::None),
            "IdentityAgent none never connects, so it is not a 'benign connect failure' either"
        );
    }

    struct AcceptAllHostKeys;
    #[async_trait]
    impl HostKeyVerifier for AcceptAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> VerifyOutcome {
            VerifyOutcome::Accepted
        }
    }

    /// Accepts only the second of two keys it's shown (`auth_publickey`
    /// gates which key even gets a sign challenge; the wire-level
    /// `FuturePublicKey` path used by `authenticate_publickey_with` still
    /// consults this first) — a stand-in for a real server whose
    /// `authorized_keys` only lists one of several keys an agent offers.
    #[derive(Clone)]
    struct SelectiveServer {
        accepted_key: russh_keys::ssh_key::PublicKey,
    }

    impl server::Server for SelectiveServer {
        type Handler = SelectiveHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> SelectiveHandler {
            SelectiveHandler { accepted_key: self.accepted_key.clone() }
        }
    }

    #[derive(Clone)]
    struct SelectiveHandler {
        accepted_key: russh_keys::ssh_key::PublicKey,
    }

    #[async_trait]
    impl server::Handler for SelectiveHandler {
        type Error = russh::Error;

        async fn auth_publickey(
            &mut self, _user: &str, public_key: &russh_keys::ssh_key::PublicKey,
        ) -> Result<Auth, Self::Error> {
            Ok(if *public_key == self.accepted_key { Auth::Accept } else { Auth::Reject { proceed_with_methods: None } })
        }

        async fn channel_open_session(
            &mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
    }

    struct FakeMultiKeySigner {
        keys: Vec<PrivateKey>,
    }

    #[async_trait]
    impl russh::Signer for FakeMultiKeySigner {
        type Error = russh::AgentAuthError;

        async fn auth_publickey_sign(
            &mut self, key: &PublicKey, mut to_sign: russh::CryptoVec,
        ) -> Result<russh::CryptoVec, Self::Error> {
            let signing_key = self.keys.iter().find(|k| k.public_key() == key).expect("test only signs with keys it was given");
            let signature = signing_key.try_sign(&to_sign).expect("signing with a known-good in-memory test key must not fail");
            let mut sig_bytes = Vec::new();
            signature.encode(&mut sig_bytes).expect("encoding a signature must not fail");
            (sig_bytes.len() as u32).encode(&mut to_sign).expect("encoding a length prefix must not fail");
            for byte in sig_bytes {
                to_sign.push(byte);
            }
            Ok(to_sign)
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
    async fn tries_identities_in_order_and_stops_at_the_first_accepted() {
        let key_a = PrivateKey::from(Ed25519Keypair::from_seed(&[1u8; 32]));
        let key_b = PrivateKey::from(Ed25519Keypair::from_seed(&[2u8; 32]));
        let key_c = PrivateKey::from(Ed25519Keypair::from_seed(&[3u8; 32]));
        // Server only accepts key_b — the middle identity offered.
        let addr = spawn_server(SelectiveServer { accepted_key: key_b.public_key().clone() }, 99).await;

        let verifier = Arc::new(AcceptAllHostKeys);
        let mut session = russh_stream_session::connect_via_jump_or_direct(
            None, Arc::new(client::Config::default()), &addr.ip().to_string(), addr.port(),
            |_leg| verifying_handler(&verifier),
        )
        .await
        .expect("direct connect should succeed");

        let identities = vec![key_a.public_key().clone(), key_b.public_key().clone(), key_c.public_key().clone()];
        let mut signer = FakeMultiKeySigner { keys: vec![key_a, key_b, key_c] };

        let authed = try_each_identity(&mut session.handle, "tester", &identities, &mut signer)
            .await
            .expect("should not error — every rejection is a normal per-identity outcome");
        assert!(authed, "the second identity should have been accepted");
    }

    #[tokio::test]
    async fn returns_false_when_the_server_rejects_every_identity() {
        let key_a = PrivateKey::from(Ed25519Keypair::from_seed(&[4u8; 32]));
        let key_b = PrivateKey::from(Ed25519Keypair::from_seed(&[5u8; 32]));
        let unrelated_key = PrivateKey::from(Ed25519Keypair::from_seed(&[6u8; 32]));
        // Server only accepts a key that's never offered.
        let addr = spawn_server(SelectiveServer { accepted_key: unrelated_key.public_key().clone() }, 100).await;

        let verifier = Arc::new(AcceptAllHostKeys);
        let mut session = russh_stream_session::connect_via_jump_or_direct(
            None, Arc::new(client::Config::default()), &addr.ip().to_string(), addr.port(),
            |_leg| verifying_handler(&verifier),
        )
        .await
        .expect("direct connect should succeed");

        let identities = vec![key_a.public_key().clone(), key_b.public_key().clone()];
        let mut signer = FakeMultiKeySigner { keys: vec![key_a, key_b] };

        let authed = try_each_identity(&mut session.handle, "tester", &identities, &mut signer).await.unwrap();
        assert!(!authed, "no offered identity was accepted, so this must be false, not an error");
    }
}
