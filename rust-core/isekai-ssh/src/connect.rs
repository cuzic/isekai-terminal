//! `isekai-ssh connect` (`ISEKAI_SSH_DESIGN.md` "接続シーケンス", `connect`
//! side). Phase S-2 scope: resolve the target isekai-helper via the trust
//! store (`isekai-trust`, `~/.config/isekai-ssh/known_helpers.toml`) instead
//! of the dev-only bypass, HELLO/proof/ACK against it, then a simple,
//! non-resumable stdin/stdout <-> QUIC relay. `init`/`--via`-driven
//! re-deployment (S-3) and resume (S-4) are still out of scope.
//!
//! Phase S-6 adds `--mode <relay|stun>` (`ConnectArgs::mode`, default
//! `relay`): which of `isekai-transport`'s two HELLO/proof/ACK paths
//! (`connect_via_relay` vs `connect_stun_p2p`) actually reaches the target
//! isekai-helper. **The trust store schema itself is unchanged** — see
//! `resolve_stun_from_trust_store`'s docs for how its
//! `cached_relay_addr`/`cached_cert_sha256`/`cached_session_secret` fields
//! are reinterpreted for STUN mode. `--mode stun` carries a known,
//! documented limitation (no recovery from NAT mapping loss mid-session,
//! `ISEKAI_SSH_DESIGN.md` "isekai-sshでのNAT越え方式の既定") that `run` warns
//! about on stderr every time it is used.
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
use isekai_transport::{
    connect_stun_p2p, connect_via_relay, ByteStream, RelayTarget, StunP2pTarget, SystemQuicEndpointFactory,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::cli::{ConnectArgs, ConnectMode};

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

/// Which of `resolve_target`'s paths produced the target. Only used to pick
/// the right error message if the subsequent HELLO/proof/ACK fails — see
/// `run`'s use of it.
enum TargetSource {
    /// Debug + `dev-insecure`-feature builds only (`cli.rs`); does not exist
    /// in a release binary. Always carries a `RelayTarget` regardless of
    /// `--mode` — see `dev_insecure_target`'s docs.
    #[cfg(all(debug_assertions, feature = "dev-insecure"))]
    DevInsecureBypass,
    TrustStore,
}

/// A resolved connection target, tagged by which `isekai-transport`
/// HELLO/proof/ACK path (`ConnectArgs::mode`) it must be handed to. Built by
/// `resolve_target`; `run` matches on this to call `connect_via_relay` or
/// `connect_stun_p2p`.
enum ResolvedTarget {
    Relay(RelayTarget),
    Stun { stun_server: SocketAddr, target: StunP2pTarget },
}

pub async fn run(args: ConnectArgs) -> Result<()> {
    if args.mode == ConnectMode::Stun {
        // Known, documented limitation (`ISEKAI_SSH_DESIGN.md`
        // "isekai-sshでのNAT越え方式の既定"): STUN+SSH rendezvous P2P has no
        // relay fallback once established, so a NAT mapping change mid-
        // session (Wi-Fi<->cellular tethering roaming, etc.) simply ends the
        // session — there is nothing to reconnect to. Deliberately
        // `eprintln!`, not `log::warn!`: this warning must be visible every
        // time `--mode stun` is used regardless of `RUST_LOG` (unlike this
        // module's other, log-level-gated diagnostics) — it goes to stderr
        // either way, so this module's stdout-purity invariant still holds.
        eprintln!(
            "isekai-ssh: --mode stun in use for '{host}' — this session cannot recover from NAT mapping \
             loss (e.g. Wi-Fi<->cellular tethering roaming): unlike the default --mode relay, there is no \
             relay fallback path once the QUIC connection to isekai-helper is lost this way. Use the \
             default --mode relay if session resilience matters more than avoiding the relay hop.",
            host = args.host,
        );
    }

    let (target, source) = resolve_target(&args)?;

    let factory = SystemQuicEndpointFactory;
    let stream = match target {
        ResolvedTarget::Relay(relay_target) => {
            log::info!("isekai-ssh: connecting to isekai-helper at {} (--mode relay)", relay_target.helper_addr);
            connect_via_relay(&factory, &relay_target).await
        }
        ResolvedTarget::Stun { stun_server, target } => {
            log::info!(
                "isekai-ssh: connecting to isekai-helper at {} via STUN+SSH rendezvous P2P (--mode stun, \
                 stun_server={stun_server})",
                target.peer_addr
            );
            connect_stun_p2p(stun_server, &target).await.map(|conn| conn.stream)
        }
    }
    .with_context(|| match (&source, args.mode) {
        // Cached trust-store credentials are only valid until isekai-helper
        // restarts (its session_secret changes on restart). A HELLO/proof
        // rejection here is exactly that signal; today this just fails
        // closed with actionable guidance, but it is also the intended
        // future trigger for the `--via`-driven automatic re-deployment
        // fallback described in `ISEKAI_SSH_DESIGN.md`'s "CLIコマンド構成" /
        // "オープンな課題" (not implemented yet — S-3).
        (TargetSource::TrustStore, ConnectMode::Relay) => format!(
            "isekai-ssh: HELLO/proof/ACK with isekai-helper for '{host}' failed using cached trust-store \
             credentials (--mode relay). isekai-helper may have restarted since the last `isekai-ssh init` \
             (its session_secret changes on restart) — re-run `isekai-ssh init {host}` to refresh trust. \
             (Automatic re-deployment via --via on this failure is not implemented yet, \
             ISEKAI_SSH_DESIGN.md フェーズ分割案 S-3.)",
            host = args.host,
        ),
        // Distinct wording from the relay-mode message above: a HELLO/proof
        // failure here is much more likely to mean "simultaneous open never
        // punched through" than "isekai-helper restarted", so point the user
        // at the fallback that doesn't depend on hole punching at all.
        (TargetSource::TrustStore, ConnectMode::Stun) => format!(
            "isekai-ssh: HELLO/proof/ACK with isekai-helper for '{host}' failed over STUN+SSH rendezvous \
             P2P (--mode stun). NAT越えが不成立だった可能性がある — this can happen when hole punching \
             does not succeed (e.g. symmetric NAT on either side) or the trust store's cached STUN-observed \
             address for isekai-helper is stale. `--mode relay`への切り替えを検討してください: re-run with \
             `--mode relay` (the default), which does not depend on simultaneous open succeeding. If the \
             cached address itself is stale, re-run `isekai-ssh init {host}` to refresh trust.",
            host = args.host,
        ),
        #[cfg(all(debug_assertions, feature = "dev-insecure"))]
        (TargetSource::DevInsecureBypass, _) => {
            "isekai-ssh: failed to establish HELLO/proof/ACK with isekai-helper".to_string()
        }
    })?;

    log::info!("isekai-ssh: HELLO/ACK complete — relaying stdin/stdout <-> QUIC");
    relay_stdio(stream).await
}

/// Resolves `args.host` (and, for `--mode stun`, `args.stun_server`) to a
/// `ResolvedTarget`, either via the dev-only bypass (debug +
/// `dev-insecure`-feature builds only, see `cli.rs`) or — the path every
/// other build takes — via the trust store
/// (`~/.config/isekai-ssh/known_helpers.toml`). No network I/O happens in
/// this function; an unregistered host fails closed here, before any QUIC
/// connection attempt or stdout write.
fn resolve_target(args: &ConnectArgs) -> Result<(ResolvedTarget, TargetSource)> {
    #[cfg(all(debug_assertions, feature = "dev-insecure"))]
    {
        if let Some(target) = dev_insecure_target(args)? {
            log::warn!(
                "isekai-ssh: using --dev-insecure-* bypass instead of the trust store for host '{}' \
                 (debug + dev-insecure build only; this path does not exist in a release binary; always \
                 relay-mode regardless of --mode, see dev_insecure_target's docs)",
                args.host
            );
            return Ok((ResolvedTarget::Relay(target), TargetSource::DevInsecureBypass));
        }
    }

    match args.mode {
        ConnectMode::Relay => {
            let target = resolve_relay_from_trust_store(&args.host)?;
            Ok((ResolvedTarget::Relay(target), TargetSource::TrustStore))
        }
        ConnectMode::Stun => {
            // clap's `required_if_eq("mode", "stun")` on `ConnectArgs::stun_server`
            // guarantees this is `Some` by the time argument parsing succeeds.
            let stun_server = args
                .stun_server
                .context("isekai-ssh: --stun-server is required with --mode stun (should be enforced by clap)")?;
            let target = resolve_stun_from_trust_store(&args.host)?;
            Ok((ResolvedTarget::Stun { stun_server, target }, TargetSource::TrustStore))
        }
    }
}

/// Looks `host` up in the trust store, failing closed — via the
/// `TrustNotInitialized` marker error — if `host` (normalized) has no entry;
/// `main.rs` maps that to a dedicated exit code and callers never attempt any
/// network I/O in that case. Shared by both `--mode`s: which fields of the
/// returned `HelperTrust` mean what differs (see
/// `resolve_relay_from_trust_store`/`resolve_stun_from_trust_store`), but the
/// lookup itself — and its trust store schema — does not.
fn load_trust_entry(host: &str) -> Result<(String, isekai_trust::schema::HelperTrust)> {
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

    Ok((key, entry.clone()))
}

/// Builds a `RelayTarget` (`--mode relay`, the default) from `host`'s trust
/// store entry (`ISEKAI_SSH_DESIGN.md` "trust store のファイル形式"):
/// `cached_relay_addr`/`cached_cert_sha256`/`cached_session_secret` are used
/// at face value, exactly as their names say.
fn resolve_relay_from_trust_store(host: &str) -> Result<RelayTarget> {
    let (key, entry) = load_trust_entry(host)?;

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

/// Builds a `StunP2pTarget` (`--mode stun`) from `host`'s trust store entry.
///
/// **The trust store schema is not changed for STUN mode** (deliberate,
/// `ISEKAI_SSH_DESIGN.md` S-6 task scope): a `HelperTrust` entry always means
/// "however this isekai-helper instance is reachable, here is the
/// information needed to do so", and for both `--mode`s that boils down to
/// the same three pieces of data — an address to dial, and the cert/session
/// credentials the HELLO/proof/ACK handshake needs. `--mode relay`
/// (`resolve_relay_from_trust_store`) reads them as the relay-assigned
/// address; `--mode stun` reads the *exact same fields* as the peer's own
/// STUN-observed address instead (`HandshakeJson::stun_observed_addr`, as
/// captured by `isekai-ssh init`/a re-deployment at `--stun-server` query
/// time) plus the same cert/session credentials:
///
/// - `cached_relay_addr` -> `StunP2pTarget::peer_addr`
/// - `cached_cert_sha256` -> `StunP2pTarget::cert_sha256_hex`
/// - `cached_session_secret` -> `StunP2pTarget::session_secret`
///
/// Renaming these fields to something mode-agnostic was considered and
/// rejected for this phase: the task's explicit scope is "trust storeのスキーマは
/// 今回変更しない", so field names stay as `isekai-trust::schema::HelperTrust`
/// already defines them; this function (and its relay-mode sibling) is the
/// one place the reinterpretation is spelled out.
fn resolve_stun_from_trust_store(host: &str) -> Result<StunP2pTarget> {
    let (key, entry) = load_trust_entry(host)?;

    let peer_addr: SocketAddr = entry.cached_relay_addr.parse().with_context(|| {
        format!(
            "isekai-ssh: trust store entry for '{key}' has an invalid cached_relay_addr \
             (read as the peer's STUN-observed address for --mode stun) '{}'",
            entry.cached_relay_addr
        )
    })?;
    let session_secret =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &entry.cached_session_secret)
            .with_context(|| format!("isekai-ssh: trust store entry for '{key}' has invalid base64 in cached_session_secret"))?;

    Ok(StunP2pTarget {
        peer_addr,
        // isekai-helper ignores the SNI it's presented (see
        // `isekai_transport::RelayTarget::server_name`'s docs, shared by
        // `StunP2pTarget`); the trust store doesn't need to record one.
        server_name: "isekai-helper".to_string(),
        cert_sha256_hex: entry.cached_cert_sha256.clone(),
        session_secret,
    })
}

/// DEV/TEST ONLY (see `cli.rs::DevInsecureArgs`): always builds a
/// `RelayTarget`, regardless of `ConnectArgs::mode`. This bypass predates
/// `--mode`/STUN support (S-1, before the trust store existed) and exists
/// only to unblock a debug-build end-to-end test against a fixed
/// relay-assigned endpoint; wiring it up to also short-circuit STUN mode
/// would just be more untested surface for a flag that must never ship in a
/// release binary in the first place (`main.rs`'s `compile_error!` guard).
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
/// handles, not one duplex object). A clean EOF on one side does **not**
/// abort the other: e.g. `ssh` closing its write end of our stdin (C2H EOF)
/// is routine well before the server has said everything it's going to say,
/// and prematurely cutting H2C off there would truncate output the user was
/// still supposed to see. Both directions are allowed to run to their own
/// completion; only a genuine error (or task panic) on either side aborts
/// the other and returns early, since at that point continuing the survivor
/// alone serves no purpose. Once both sides have finished, this process
/// exits and closes its stdout, which is how `ssh` (reading our stdout)
/// learns the pass-through has ended; S-1 deliberately does not try to keep
/// the pipe alive across a QUIC disconnect (that's resume, S-4).
async fn relay_stdio(stream: Box<dyn ByteStream>) -> Result<()> {
    let (quic_read, quic_write) = stream.split();

    let mut c2h = tokio::spawn(pump_stdin_to_quic(quic_write));
    let mut h2c = tokio::spawn(pump_quic_to_stdout(quic_read));
    let (mut c2h_done, mut h2c_done) = (false, false);

    while !c2h_done || !h2c_done {
        tokio::select! {
            res = &mut c2h, if !c2h_done => {
                c2h_done = true;
                if let Err(err) = join_result("isekai-ssh: stdin->QUIC relay task panicked", res) {
                    h2c.abort();
                    return Err(err);
                }
            }
            res = &mut h2c, if !h2c_done => {
                h2c_done = true;
                if let Err(err) = join_result("isekai-ssh: QUIC->stdout relay task panicked", res) {
                    c2h.abort();
                    return Err(err);
                }
            }
        }
    }
    Ok(())
}

/// Flattens a `tokio::spawn` result (`Result<Result<()>, JoinError>`) into a
/// single `Result<()>`, attaching `panic_ctx` if the task itself panicked
/// rather than returning an error.
fn join_result(panic_ctx: &str, res: std::result::Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    res.map_err(|e| anyhow::Error::new(e).context(panic_ctx.to_string()))?
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
