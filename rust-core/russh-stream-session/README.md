# russh-stream-session

Establish and authenticate an SSH client session (built on [`russh`]) over
any `AsyncRead + AsyncWrite` byte stream — not just a raw TCP socket. The
connect/handshake functions are generic over the `russh` `client::Handler`
you supply, so callers that need more than host-key verification (agent
forwarding, remote port forwards, other server-initiated channel requests)
can plug in their own handler. If you only need host-key verification, use
the bundled [`VerifyingHandler`] (via [`verifying_handler`]) instead of
writing one — it delegates to a small [`HostKeyVerifier`] trait, so this
crate has no opinion on how (or whether) you persist a trust-on-first-use
store.

This crate deliberately does **not** implement port forwarding, SSH agent
forwarding, or any application-specific channel protocol itself — it covers
exactly the "get me an authenticated `russh::client::Handle` and a session
channel" part. Everything past that (the actual I/O loop, resize handling,
forwards) is left to the caller, since those tend to be application-specific.

## Example

```rust,no_run
# use std::sync::Arc;
# use russh_stream_session::{Credential, HostKeyVerifier, SessionKind};
struct AcceptAll;
#[async_trait::async_trait]
impl HostKeyVerifier for AcceptAll {
    async fn verify(&self, _fingerprint: &str) -> bool { true }
}

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let verifier = Arc::new(AcceptAll);
let config = Arc::new(russh::client::Config::default());
let mut session = russh_stream_session::connect_via_jump_or_direct(
    None, config, "example.com", 22,
    || russh_stream_session::verifying_handler(&verifier),
).await?;

let authed = russh_stream_session::authenticate_session(
    &mut session.handle, "alice", &Credential::Password("hunter2".into()),
).await?;
assert!(authed);

let mut channel = russh_stream_session::open_channel(
    &session.handle, &SessionKind::Exec { command: "echo hi".into() },
).await?;
# Ok(())
# }
```

Callers that need agent forwarding, remote port forwards, or other
server-initiated channel requests implement their own `client::Handler` and
pass a closure constructing it instead of `verifying_handler`.

## SSH agent authentication

[`authenticate_with_signer`] authenticates using anything implementing
`russh::Signer` — `russh` itself provides that impl for
[`russh_keys::agent::client::AgentClient`], over any transport it exposes
(Unix socket, Windows named pipe, or Pageant), so signing happens inside the
agent process without this crate (or the calling application) ever holding
the private key material:

```rust,no_run
# async fn run() -> Result<(), Box<dyn std::error::Error>> {
# use std::sync::Arc;
# struct AcceptAll;
# #[async_trait::async_trait]
# impl russh_stream_session::HostKeyVerifier for AcceptAll {
#     async fn verify(&self, _fingerprint: &str) -> bool { true }
# }
# let verifier = Arc::new(AcceptAll);
# let config = Arc::new(russh::client::Config::default());
let mut session = russh_stream_session::connect_via_jump_or_direct(
    None, config, "example.com", 22,
    || russh_stream_session::verifying_handler(&verifier),
).await?;

let mut agent = russh_keys::agent::client::AgentClient::connect_env().await?;
let identities = agent.request_identities().await?;
let public_key = identities.into_iter().next().ok_or("agent has no identities")?;
let authed = russh_stream_session::authenticate_with_signer(
    &mut session.handle, "alice", public_key, &mut agent,
).await?;
assert!(authed);
# Ok(())
# }
```

## Jump hosts

Passing a [`JumpHost`] to [`connect_via_jump_or_direct`] authenticates to the
jump host first, then opens a `direct-tcpip` channel through it and layers a
second, nested SSH handshake on top (`ssh -J` equivalent, single hop). The
returned [`Session`] keeps the jump host's `client::Handle` alive internally
for as long as the target session is in use.

[`russh`]: https://docs.rs/russh
