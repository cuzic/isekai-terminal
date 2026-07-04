//! `isekai-ssh connect` (`ISEKAI_SSH_DESIGN.md` "接続シーケンス", `connect`
//! side). Phase S-2 scope: resolve the target isekai-helper via the trust
//! store (`isekai-trust`, `~/.config/isekai-ssh/known_helpers.toml`) instead
//! of the dev-only bypass, HELLO/proof/ACK against it, then a simple,
//! non-resumable stdin/stdout <-> QUIC relay. `init`/`--via`-driven
//! re-deployment (S-3) and resume (S-4) are still out of scope.
//!
//! **stdout purity is the load-bearing invariant of this whole module**: this
//! process is invoked as `ssh`'s `ProxyCommand`, so anything written to
//! stdout other than bytes read from the QUIC stream corrupts the SSH
//! session from `ssh`'s point of view. All diagnostics go through the `log`
//! crate (routed to stderr by `main.rs`'s `env_logger` setup), and trust
//! store resolution failures are turned into plain `anyhow::Error`s that
//! `main.rs` prints to stderr directly — never to stdout.

use std::net::SocketAddr;

#[cfg(all(debug_assertions, feature = "dev-insecure"))]
use anyhow::bail;
use anyhow::{Context, Result};
use isekai_transport::{connect_via_relay, ByteStream, RelayTarget, SystemQuicEndpointFactory};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::cli::ConnectArgs;

/// Marker error so `main.rs` can tell "host has no trust store entry" apart
/// from every other failure and map it to a dedicated exit code
/// (`ISEKAI_SSH_DESIGN.md` フェーズ分割案 S-2 "exit codeの分類"). Carried as the
/// root cause of the `anyhow::Error` returned by `resolve_from_trust_store`;
/// `main.rs` finds it via `anyhow::Error::chain()`.
#[derive(Debug)]
pub struct TrustNotInitialized;

impl std::fmt::Display for TrustNotInitialized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host is not registered in the isekai-ssh trust store")
    }
}

impl std::error::Error for TrustNotInitialized {}

/// Which of `resolve_target`'s two paths produced a `RelayTarget`. Only used
/// to pick the right error message if the subsequent HELLO/proof/ACK fails —
/// see `run`'s use of it.
enum TargetSource {
    /// Debug + `dev-insecure`-feature builds only (`cli.rs`); does not exist
    /// in a release binary.
    #[cfg(all(debug_assertions, feature = "dev-insecure"))]
    DevInsecureBypass,
    TrustStore,
}

pub async fn run(args: ConnectArgs) -> Result<()> {
    let (target, source) = resolve_target(&args)?;
    log::info!("isekai-ssh: connecting to isekai-helper at {}", target.helper_addr);

    let factory = SystemQuicEndpointFactory;
    let stream = connect_via_relay(&factory, &target).await.with_context(|| match source {
        // Cached trust-store credentials are only valid until isekai-helper
        // restarts (its session_secret changes on restart). A HELLO/proof
        // rejection here is exactly that signal; today this just fails
        // closed with actionable guidance, but it is also the intended
        // future trigger for the `--via`-driven automatic re-deployment
        // fallback described in `ISEKAI_SSH_DESIGN.md`'s "CLIコマンド構成" /
        // "オープンな課題" (not implemented yet — S-3).
        TargetSource::TrustStore => format!(
            "isekai-ssh: HELLO/proof/ACK with isekai-helper for '{host}' failed using cached trust-store \
             credentials. isekai-helper may have restarted since the last `isekai-ssh init` (its \
             session_secret changes on restart) — re-run `isekai-ssh init {host}` to refresh trust. \
             (Automatic re-deployment via --via on this failure is not implemented yet, \
             ISEKAI_SSH_DESIGN.md フェーズ分割案 S-3.)",
            host = args.host,
        ),
        #[cfg(all(debug_assertions, feature = "dev-insecure"))]
        TargetSource::DevInsecureBypass => {
            "isekai-ssh: failed to establish HELLO/proof/ACK with isekai-helper".to_string()
        }
    })?;

    log::info!("isekai-ssh: HELLO/ACK complete — relaying stdin/stdout <-> QUIC");
    relay_stdio(stream).await
}

/// Resolves `args.host` to a `RelayTarget`, either via the dev-only bypass
/// (debug + `dev-insecure`-feature builds only, see `cli.rs`) or — the path
/// every other build takes — via the trust store
/// (`~/.config/isekai-ssh/known_helpers.toml`). No network I/O happens in
/// this function; an unregistered host fails closed here, before any QUIC
/// connection attempt or stdout write.
fn resolve_target(args: &ConnectArgs) -> Result<(RelayTarget, TargetSource)> {
    #[cfg(all(debug_assertions, feature = "dev-insecure"))]
    {
        if let Some(target) = dev_insecure_target(args)? {
            log::warn!(
                "isekai-ssh: using --dev-insecure-* bypass instead of the trust store for host '{}' \
                 (debug + dev-insecure build only; this path does not exist in a release binary)",
                args.host
            );
            return Ok((target, TargetSource::DevInsecureBypass));
        }
    }

    let target = resolve_from_trust_store(&args.host)?;
    Ok((target, TargetSource::TrustStore))
}

/// Looks `host` up in the trust store and builds a `RelayTarget` from its
/// cached handshake info (`ISEKAI_SSH_DESIGN.md` "trust store のファイル形式").
/// Fails closed — via the `TrustNotInitialized` marker error — if `host`
/// (normalized) has no entry; `main.rs` maps that to a dedicated exit code
/// and this function's caller never attempts any network I/O in that case.
fn resolve_from_trust_store(host: &str) -> Result<RelayTarget> {
    let key = isekai_trust::normalize_host_port(host)
        .with_context(|| format!("isekai-ssh: invalid host spec '{host}'"))?;

    let store_path = isekai_trust::default_trust_store_path()
        .context("isekai-ssh: could not determine the trust store path (is $HOME set?)")?;
    let store = isekai_trust::load_trust_store(&store_path)
        .with_context(|| format!("isekai-ssh: failed to load trust store at {}", store_path.display()))?;

    let Some(entry) = store.get(&key) else {
        return Err(anyhow::Error::new(TrustNotInitialized).context(format!(
            "isekai-ssh: '{host}' is not a trusted host yet (looked up as '{key}' in {}).\n\
             Run:\n  isekai-ssh init {host}\n\
             once to deploy isekai-helper there and register trust.",
            store_path.display(),
        )));
    };

    let helper_addr: SocketAddr = entry.cached_relay_addr.parse().with_context(|| {
        format!(
            "isekai-ssh: trust store entry for '{key}' has an invalid cached_relay_addr '{}'",
            entry.cached_relay_addr
        )
    })?;
    let session_secret =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &entry.cached_session_secret)
            .with_context(|| format!("isekai-ssh: trust store entry for '{key}' has invalid base64 in cached_session_secret"))?;

    Ok(RelayTarget {
        helper_addr,
        // isekai-helper ignores the SNI it's presented (see
        // `isekai_transport::RelayTarget::server_name`'s docs); the trust
        // store doesn't need to record one.
        server_name: "isekai-helper".to_string(),
        cert_sha256_hex: entry.cached_cert_sha256.clone(),
        session_secret,
    })
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
