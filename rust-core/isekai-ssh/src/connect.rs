//! `isekai-ssh connect` (`ISEKAI_SSH_DESIGN.md` "接続シーケンス", `connect`
//! side). Phase S-1 scope only: HELLO/proof/ACK against a directly-specified
//! isekai-helper endpoint (real trust-store-driven resolution lands in
//! S-2/S-3), then a simple, non-resumable stdin/stdout <-> QUIC relay.
//!
//! **stdout purity is the load-bearing invariant of this whole module**: this
//! process is invoked as `ssh`'s `ProxyCommand`, so anything written to
//! stdout other than bytes read from the QUIC stream corrupts the SSH
//! session from `ssh`'s point of view. All diagnostics go through the `log`
//! crate (routed to stderr by `main.rs`'s `env_logger` setup).

use anyhow::{bail, Context, Result};
use isekai_transport::{connect_via_relay, ByteStream, RelayTarget, SystemQuicEndpointFactory};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::cli::ConnectArgs;

pub async fn run(args: ConnectArgs) -> Result<()> {
    let target = resolve_target(&args)?;
    log::info!("isekai-ssh: connecting to isekai-helper at {}", target.helper_addr);

    let factory = SystemQuicEndpointFactory;
    let stream = connect_via_relay(&factory, &target)
        .await
        .context("isekai-ssh: failed to establish HELLO/proof/ACK with isekai-helper")?;

    log::info!("isekai-ssh: HELLO/ACK complete — relaying stdin/stdout <-> QUIC");
    relay_stdio(stream).await
}

/// Resolves `args.host` to a `RelayTarget`. Real trust-store-backed
/// resolution isn't implemented yet (S-2/S-3): the only way to reach this
/// phase's end-to-end test is the `--dev-insecure-*` bypass, gated (see
/// `cli.rs`) to debug + `dev-insecure`-feature builds only. Anything else
/// (including any release build) fails closed here, before any network I/O
/// or stdout write happens.
fn resolve_target(args: &ConnectArgs) -> Result<RelayTarget> {
    #[cfg(all(debug_assertions, feature = "dev-insecure"))]
    {
        if let Some(target) = dev_insecure_target(args)? {
            log::warn!(
                "isekai-ssh: using --dev-insecure-* bypass instead of the trust store for host '{}' \
                 (debug + dev-insecure build only; this path does not exist in a release binary)",
                args.host
            );
            return Ok(target);
        }
    }

    bail!(
        "isekai-ssh: trust store lookup for host '{host}' is not implemented yet \
         (ISEKAI_SSH_DESIGN.md フェーズ分割案 S-2/S-3). Once available, run `isekai-ssh init {host}` \
         once per host. Until then, only a debug build compiled with `--features dev-insecure` can \
         connect, via its --dev-insecure-* flags.",
        host = args.host,
    );
}

#[cfg(all(debug_assertions, feature = "dev-insecure"))]
fn dev_insecure_target(args: &ConnectArgs) -> Result<Option<RelayTarget>> {
    use std::net::SocketAddr;

    let d = &args.dev_insecure;
    let any_set = d.dev_insecure_target.is_some()
        || d.dev_insecure_cert_sha256.is_some()
        || d.dev_insecure_session_secret.is_some();

    let (Some(target_addr), Some(cert_sha256_hex), Some(session_secret_b64)) = (
        d.dev_insecure_target.as_deref(),
        d.dev_insecure_cert_sha256.as_deref(),
        d.dev_insecure_session_secret.as_deref(),
    ) else {
        if any_set {
            bail!(
                "isekai-ssh: --dev-insecure-target, --dev-insecure-cert-sha256, and \
                 --dev-insecure-session-secret must all be given together"
            );
        }
        return Ok(None);
    };

    let helper_addr: SocketAddr =
        target_addr.parse().with_context(|| format!("isekai-ssh: invalid --dev-insecure-target '{target_addr}'"))?;
    let session_secret = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, session_secret_b64)
        .context("isekai-ssh: --dev-insecure-session-secret must be valid base64")?;

    Ok(Some(RelayTarget {
        helper_addr,
        server_name: d.dev_insecure_server_name.clone(),
        cert_sha256_hex: cert_sha256_hex.to_string(),
        session_secret,
    }))
}

/// Runs the two independent copy tasks (`ISEKAI_SSH_DESIGN.md`: not
/// `tokio::io::copy_bidirectional`, since stdin/stdout are two separate
/// handles, not one duplex object) and returns once *either* direction
/// finishes — aborting the other. A clean process exit at that point closes
/// this process's stdout, which is how `ssh` (reading our stdout) learns the
/// pass-through has ended; S-1 deliberately does not try to keep the pipe
/// alive across a QUIC disconnect (that's resume, S-4).
async fn relay_stdio(stream: Box<dyn ByteStream>) -> Result<()> {
    let (quic_read, quic_write) = stream.split();

    let mut c2h = tokio::spawn(pump_stdin_to_quic(quic_write));
    let mut h2c = tokio::spawn(pump_quic_to_stdout(quic_read));

    tokio::select! {
        res = &mut c2h => {
            h2c.abort();
            res.context("isekai-ssh: stdin->QUIC relay task panicked")??;
        }
        res = &mut h2c => {
            c2h.abort();
            res.context("isekai-ssh: QUIC->stdout relay task panicked")??;
        }
    }
    Ok(())
}

/// C2H direction: `ssh` (our stdin) -> isekai-helper (the QUIC stream's send
/// side). On stdin EOF, finishes (shuts down) the QUIC send side so
/// isekai-helper sees a clean half-close rather than a reset.
async fn pump_stdin_to_quic(mut quic_write: Box<dyn isekai_transport::ByteStreamWriteHalf>) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = stdin.read(&mut buf).await.context("isekai-ssh: reading from stdin failed")?;
        if n == 0 {
            break;
        }
        quic_write.write_all(&buf[..n]).await.context("isekai-ssh: writing to isekai-helper failed")?;
    }
    // Best-effort: isekai-helper is free to have already gone away.
    let _ = quic_write.shutdown().await;
    Ok(())
}

/// H2C direction: isekai-helper (the QUIC stream's receive side) -> `ssh`
/// (our stdout). Every successful chunk is flushed immediately — `ssh`
/// expects to see SSH protocol bytes promptly, not batched.
async fn pump_quic_to_stdout(mut quic_read: Box<dyn isekai_transport::ByteStreamReadHalf>) -> Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = quic_read.read(&mut buf).await.context("isekai-ssh: reading from isekai-helper failed")?;
        if n == 0 {
            break;
        }
        stdout.write_all(&buf[..n]).await.context("isekai-ssh: writing to stdout failed")?;
        stdout.flush().await.context("isekai-ssh: flushing stdout failed")?;
    }
    Ok(())
}
