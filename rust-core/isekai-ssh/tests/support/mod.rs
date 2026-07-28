//! Shared helper for isekai-ssh's e2e tests: [`spawn_sshd`] — the mock-sshd
//! bring-up boilerplate (host key generation from a seed, host key
//! fingerprint, `russh::server::Config`, binding `127.0.0.1:0`, spawning
//! `run_on_socket`) that turned out to be byte-for-byte identical across
//! every `tests/*_e2e.rs` file that stands up an in-process `russh::server`
//! mock (confirmed identical across `doctor_e2e.rs` and
//! `wrapper_stale_trust_auto_recovery_e2e.rs` before extracting this).
//!
//! This is a narrow, deliberate exception to this crate's documented
//! self-containment convention for `tests/*_e2e.rs` files (helpers are
//! normally duplicated per file rather than shared via a `tests/common`-style
//! module, so a regression in one shared module can't break many
//! independent, CI-flaky e2e files at once). The exception is scoped
//! tightly: only the semantically-inert "how do I stand up a TCP-bound SSH
//! server" plumbing lives here. Each test file's own `Server`/`Handler`
//! implementation — the auth logic and channel/exec/shell behavior that
//! actually makes each test meaningful and distinct — stays fully local to
//! that file, unchanged.
//!
//! `tests/support/mod.rs` (a directory, not a top-level `tests/support.rs`)
//! is deliberate: Cargo auto-discovers every direct `tests/*.rs` file as its
//! own integration-test binary, but `tests/<dir>/mod.rs` is not — the
//! standard Rust idiom for sharing test code without adding an empty test
//! target. Each e2e file that uses this adds `mod support;` and calls
//! `support::spawn_sshd(seed, MyServer { ... }).await`.

/// Starts a mock `sshd` on `127.0.0.1:<random port>` whose host key is
/// deterministically derived from `host_key_seed` (each caller picks its own
/// seed so multiple mock servers in the same test process never collide),
/// backed by the given `russh::server::Server` implementation. Returns the
/// listen address and the host key's SHA256 fingerprint (the format
/// `isekai_trust::SshHostKeyTrust::fingerprint` stores, for pre-seeding a
/// trust store so the native path's own TOFU prompt never fires).
pub(crate) async fn spawn_sshd<S>(host_key_seed: u8, mut server: S) -> (std::net::SocketAddr, String)
where
    S: russh::server::Server + Send + 'static,
    S::Handler: Send + 'static,
{
    use russh_keys::ssh_key::private::Ed25519Keypair;
    use russh_keys::PrivateKey;

    let keypair = Ed25519Keypair::from_seed(&[host_key_seed; 32]);
    let host_key = PrivateKey::from(keypair);
    let fingerprint = host_key.public_key().fingerprint(russh_keys::HashAlg::Sha256).to_string();
    let config = std::sync::Arc::new(russh::server::Config { keys: vec![host_key], ..Default::default() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = server.run_on_socket(config, &listener).await;
    });
    (addr, fingerprint)
}
