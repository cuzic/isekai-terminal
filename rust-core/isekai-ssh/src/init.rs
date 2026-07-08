//! `isekai-ssh init` (`archive/ISEKAI_SSH_DESIGN.md` "接続シーケンス", `init` side;
//! フェーズ分割案 S-3). Unlike `connect`, this is an explicitly interactive,
//! one-time-per-host command: it deploys/starts `isekai-helper` on `<host>`
//! (optionally via a jump host) using `isekai-bootstrap::OpenSshBackend`,
//! shows the operator what it just talked to, waits for an explicit `[y/N]`
//! confirmation, and — only on `y`/`Y` — writes a `HelperTrust` entry to the
//! trust store `connect` reads from
//! (`~/.config/isekai-ssh/known_helpers.toml`, `isekai-trust`).
//!
//! Unlike `connect.rs`, stdout purity is *not* a constraint here — `init` is
//! never invoked as `ssh`'s `ProxyCommand` (`archive/ISEKAI_SSH_DESIGN.md` "ユーザー
//!体験の流れ" A節) — so the confirmation prompt and its supporting summary
//! are written directly to stdout.
//!
//! Scope of this phase (S-3): the helper binary is supplied explicitly via
//! `--helper-binary <path>` (see `cli::InitArgs`'s docs for why there is no
//! embedded-binary default yet), and the relay endpoint/JWT are supplied
//! directly via `--relay-addr`/`--relay-sni`/--relay-jwt` rather than
//! discovered/issued automatically (`isekai-ssh login`'s Device
//! Authorization Flow is S-5). `--via`-driven automatic re-deployment from
//! `connect` is still out of scope (that's `connect.rs`'s own future work,
//! not this module's).

use std::io::Write as _;

use anyhow::{Context, Result};
use isekai_bootstrap::{BootstrapBackend, HostSpec, JumpSpec, LaunchSpec, OpenSshBackend, RelayLaunchSpec};
use isekai_trust::{HelperTrust, UpdatePolicy};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::cli::InitArgs;

pub async fn run(args: InitArgs) -> Result<()> {
    let helper_binary = std::fs::read(&args.helper_binary)
        .with_context(|| format!("isekai-ssh: failed to read helper binary at {}", args.helper_binary.display()))?;
    let helper_sha256 = hex_sha256(&helper_binary);

    let target = parse_host_spec(&args.host)
        .with_context(|| format!("isekai-ssh: invalid host spec '{}'", args.host))?;
    let via = args
        .via
        .as_deref()
        .map(parse_jump_spec)
        .transpose()
        .with_context(|| format!("isekai-ssh: invalid --via spec '{}'", args.via.as_deref().unwrap_or_default()))?;
    let relay = RelayLaunchSpec {
        relay_addr: args.relay_addr,
        relay_sni: args.relay_sni.clone(),
        relay_jwt: args.relay_jwt.clone(),
        idle_lifetime_secs: args.idle_lifetime,
    };

    println!("Deploying isekai-helper to {}...", args.host);
    let backend = OpenSshBackend::new();
    let report = backend
        .install_and_start(&target, via.as_ref(), &helper_binary, &LaunchSpec::Relay(relay), None)
        .await
        .with_context(|| format!("isekai-ssh: failed to deploy/start isekai-helper on '{}'", args.host))?;
    let handshake = &report.handshake;

    // `HandshakeJson` (`isekai-protocol`) carries no separate identity-pubkey
    // field yet (that's still `cert_sha256`'s job — see the crate's own
    // module docs) — use it as the displayed "identity" until isekai-helper
    // grows a dedicated identity key.
    let identity = handshake.cert_sha256().to_string();

    println!();
    println!("Host:            {}", args.host);
    if let Some(via) = &args.via {
        println!("Via:             {via}");
    }
    println!("Helper identity: {identity}");
    println!("Binary sha256:   {helper_sha256}");
    if args.helper_version != "unknown" {
        println!("Version:         {}", args.helper_version);
    }
    println!("Relay:           {}", args.relay_addr);
    println!();
    print!("Trust this isekai-helper and register it for '{}'? [y/N] ", args.host);
    std::io::stdout().flush().ok();

    let mut reader = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    reader.read_line(&mut line).await.context("isekai-ssh: failed to read confirmation from stdin")?;
    let approved = matches!(line.trim(), "y" | "Y");
    if !approved {
        println!("Aborted — nothing was written to the trust store.");
        return Ok(());
    }

    let store_path = isekai_trust::default_trust_store_path()
        .context("isekai-ssh: could not determine the trust store path (is $HOME set?)")?;
    let mut store = isekai_trust::load_trust_store(&store_path)
        .with_context(|| format!("isekai-ssh: failed to load trust store at {}", store_path.display()))?;

    let key = isekai_trust::normalize_host_port(&args.host)
        .with_context(|| format!("isekai-ssh: invalid host spec '{}'", args.host))?;
    let now = now_rfc3339();
    // `relay_public_addr` is the address a real deployment's isekai-helper
    // reports back once its `--relay` tunnel is up (`archive/HELPER_PROTOCOL.md`).
    // Falling back to the `--relay-addr` we were given only guards against a
    // helper that (e.g. in a test double) never populated the field; a real
    // isekai-helper launched with `--relay` always sets it.
    let cached_relay_addr = handshake
        .relay_public_addr()
        .map(str::to_string)
        .unwrap_or_else(|| args.relay_addr.to_string());

    store.insert(
        key.clone(),
        HelperTrust {
            identity_pubkey: identity,
            trusted_helper_sha256: helper_sha256,
            trusted_helper_version: args.helper_version.clone(),
            update_policy: UpdatePolicy::ExactDigestOnly,
            release_channel: args.release_channel.clone(),
            last_via: args.via.clone(),
            trusted_at: now.clone(),
            last_seen_at: now,
            cached_relay_addr,
            cached_cert_sha256: handshake.cert_sha256().to_string(),
            cached_session_secret: handshake.session_secret.clone(),
        },
    );

    isekai_trust::save_trust_store(&store_path, &store)
        .with_context(|| format!("isekai-ssh: failed to write trust store at {}", store_path.display()))?;

    println!("Registered '{key}' in {}", store_path.display());
    Ok(())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Current UTC time formatted as RFC 3339 (`trusted_at`/`last_seen_at` are
/// purely informational per `isekai-trust`'s schema docs, so a hand-rolled
/// formatter — rather than pulling in a full datetime crate for this alone —
/// is enough).
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_rfc3339_utc(secs)
}

/// Minimal civil-calendar conversion from a Unix timestamp to
/// `YYYY-MM-DDTHH:MM:SSZ`, good for any date this project will ever run at
/// (proleptic Gregorian, UTC only — exactly what `trusted_at`/`last_seen_at`
/// need and nothing more).
fn format_rfc3339_utc(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let secs_of_day = unix_secs % 86_400;
    let (hour, minute, second) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);

    // Civil-from-days algorithm (Howard Hinnant's `civil_from_days`),
    // proleptic Gregorian, days since 1970-01-01.
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Parses a `[user@]host[:port]` spec into a `HostSpec`, reusing
/// `isekai_trust::split_user_host_port`'s tokenization but keeping user/port
/// as separate optional fields (as `HostSpec`/`ssh(1)` want them) instead of
/// collapsing to a single normalized `host:port` string.
fn parse_host_spec(spec: &str) -> Result<HostSpec> {
    let (host, port, user) = isekai_trust::split_user_host_port(spec)?;
    let mut hs = HostSpec::new(host);
    if let Some(port) = port {
        hs = hs.with_port(port);
    }
    if let Some(user) = user {
        hs = hs.with_user(user);
    }
    Ok(hs)
}

/// Same tokenization as `parse_host_spec`, for the `--via` jump host.
fn parse_jump_spec(spec: &str) -> Result<JumpSpec> {
    let (host, port, user) = isekai_trust::split_user_host_port(spec)?;
    let mut js = JumpSpec::new(host);
    if let Some(port) = port {
        js = js.with_port(port);
    }
    if let Some(user) = user {
        js = js.with_user(user);
    }
    Ok(js)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_host() {
        let hs = parse_host_spec("myhost").unwrap();
        assert_eq!(hs, HostSpec::new("myhost"));
    }

    #[test]
    fn parses_host_with_port() {
        let hs = parse_host_spec("myhost:2222").unwrap();
        assert_eq!(hs, HostSpec::new("myhost").with_port(2222));
    }

    #[test]
    fn parses_user_and_host() {
        let hs = parse_host_spec("alice@myhost").unwrap();
        assert_eq!(hs, HostSpec::new("myhost").with_user("alice"));
    }

    #[test]
    fn parses_user_host_and_port() {
        let hs = parse_host_spec("alice@myhost:2222").unwrap();
        assert_eq!(hs, HostSpec::new("myhost").with_port(2222).with_user("alice"));
    }

    #[test]
    fn rejects_empty_spec() {
        assert!(parse_host_spec("").is_err());
        assert!(parse_host_spec("   ").is_err());
    }

    #[test]
    fn rejects_bad_port() {
        assert!(parse_host_spec("myhost:notaport").is_err());
    }

    #[test]
    fn jump_spec_parses_the_same_way() {
        let js = parse_jump_spec("bastion.example.com:2200").unwrap();
        assert_eq!(js, JumpSpec::new("bastion.example.com").with_port(2200));
    }

    #[test]
    fn rfc3339_formats_a_known_timestamp() {
        // 2026-07-04T00:00:00Z, matching the fixtures used across
        // isekai-trust's own tests.
        let unix_secs = 1_783_123_200u64;
        assert_eq!(format_rfc3339_utc(unix_secs), "2026-07-04T00:00:00Z");
    }

    #[test]
    fn hex_sha256_matches_known_vector() {
        // sha256("") — a standard test vector.
        assert_eq!(hex_sha256(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }
}
