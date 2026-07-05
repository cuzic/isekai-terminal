//! Turns `ConnectArgs` into a `RelayTarget`/`StunP2pTarget`, either via the
//! trust store (`~/.config/isekai-ssh/known_helpers.toml`) or — debug +
//! `dev-insecure`-feature builds only — the dev bypass. No network I/O
//! happens here; an unregistered host fails closed before any QUIC
//! connection attempt or stdout write (see `super`'s module docs on stdout
//! purity).

use std::net::SocketAddr;

#[cfg(all(debug_assertions, feature = "dev-insecure"))]
use anyhow::bail;
use anyhow::{Context, Result};
use isekai_transport::{RelayTarget, StunP2pTarget};

#[cfg(all(debug_assertions, feature = "dev-insecure"))]
use crate::cli::ConnectArgs;

/// Marker error so `main.rs` can tell "host has no trust store entry" apart
/// from every other failure and map it to a dedicated exit code
/// (`ISEKAI_SSH_DESIGN.md` フェーズ分割案 S-2 "exit codeの分類"). Carried as the
/// root cause of the `anyhow::Error` returned by `load_trust_entry`;
/// `main.rs` finds it via `anyhow::Error::chain()`.
#[derive(Debug)]
pub struct TrustNotInitialized;

impl std::fmt::Display for TrustNotInitialized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host is not registered in the isekai-ssh trust store")
    }
}

impl std::error::Error for TrustNotInitialized {}

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

/// Parses a trust store entry's `cached_relay_addr` field, tagging parse
/// failures with `key` and — for `--mode stun`, where this same field is
/// reinterpreted as the peer's STUN-observed address (see
/// `resolve_stun_from_trust_store`'s docs) — a note to that effect.
fn parse_cached_relay_addr(key: &str, raw: &str, stun_reinterpreted: bool) -> Result<SocketAddr> {
    let note = if stun_reinterpreted {
        " (read as the peer's STUN-observed address for --mode stun)"
    } else {
        ""
    };
    raw.parse()
        .with_context(|| format!("isekai-ssh: trust store entry for '{key}' has an invalid cached_relay_addr{note} '{raw}'"))
}

/// Base64-decodes a session secret, attaching `context` (a `format!`'d
/// message, evaluated lazily like `with_context`'s closure) to any failure.
/// Shared by the trust store path (`resolve_relay_from_trust_store`/
/// `resolve_stun_from_trust_store`) and the dev-insecure bypass
/// (`dev_insecure_target`), whose error messages otherwise differ (one names
/// the trust store entry's key, the other names the CLI flag).
fn decode_session_secret_base64(b64: &str, context: impl std::fmt::Display) -> Result<Vec<u8>> {
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).with_context(|| context.to_string())
}

/// Builds a `RelayTarget` (`--mode relay`, the default) from `host`'s trust
/// store entry (`ISEKAI_SSH_DESIGN.md` "trust store のファイル形式"):
/// `cached_relay_addr`/`cached_cert_sha256`/`cached_session_secret` are used
/// at face value, exactly as their names say.
pub fn resolve_relay_from_trust_store(host: &str) -> Result<RelayTarget> {
    let (key, entry) = load_trust_entry(host)?;

    let helper_addr = parse_cached_relay_addr(&key, &entry.cached_relay_addr, false)?;
    let session_secret = decode_session_secret_base64(
        &entry.cached_session_secret,
        format_args!("isekai-ssh: trust store entry for '{key}' has invalid base64 in cached_session_secret"),
    )?;

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
pub fn resolve_stun_from_trust_store(host: &str) -> Result<StunP2pTarget> {
    let (key, entry) = load_trust_entry(host)?;

    let peer_addr = parse_cached_relay_addr(&key, &entry.cached_relay_addr, true)?;
    let session_secret = decode_session_secret_base64(
        &entry.cached_session_secret,
        format_args!("isekai-ssh: trust store entry for '{key}' has invalid base64 in cached_session_secret"),
    )?;

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
pub fn dev_insecure_target(args: &ConnectArgs) -> Result<Option<RelayTarget>> {
    use std::net::SocketAddr;

    let d = &args.dev_insecure;
    // All-or-nothing: expressed as a single match over the three flags
    // rather than a separate "was any given at all" check plus a let-else,
    // so the "none given" / "all given" / "some but not all given" cases
    // each read as one arm instead of being reconstructed from two
    // conditions.
    let (target_addr, cert_sha256_hex, session_secret_b64) = match (
        d.dev_insecure_target.as_deref(),
        d.dev_insecure_cert_sha256.as_deref(),
        d.dev_insecure_session_secret.as_deref(),
    ) {
        (None, None, None) => return Ok(None),
        (Some(target_addr), Some(cert_sha256_hex), Some(session_secret_b64)) => {
            (target_addr, cert_sha256_hex, session_secret_b64)
        }
        _ => bail!(
            "isekai-ssh: --dev-insecure-target, --dev-insecure-cert-sha256, and \
             --dev-insecure-session-secret must all be given together"
        ),
    };

    let helper_addr: SocketAddr =
        target_addr.parse().with_context(|| format!("isekai-ssh: invalid --dev-insecure-target '{target_addr}'"))?;
    let session_secret = decode_session_secret_base64(
        session_secret_b64,
        "isekai-ssh: --dev-insecure-session-secret must be valid base64",
    )?;

    Ok(Some(RelayTarget {
        helper_addr,
        server_name: d.dev_insecure_server_name.clone(),
        cert_sha256_hex: cert_sha256_hex.to_string(),
        session_secret,
    }))
}
