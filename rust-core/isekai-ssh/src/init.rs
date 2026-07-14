//! `isekai-ssh init` (`archive/ISEKAI_SSH_DESIGN.md` "接続シーケンス", `init` side;
//! フェーズ分割案 S-3). Unlike `connect`, this is an explicitly interactive,
//! one-time-per-host command: it deploys/starts `isekai-helper` on `<host>`
//! (optionally via a jump host) using `isekai-bootstrap::OpenSshBackend`,
//! shows the operator what it just talked to, waits for an explicit `[y/N]`
//! confirmation, and — only on `y`/`Y` — writes a `PersistentProfile`
//! (`isekai-pipe-core`) that `connect` reads from (`ISEKAI_PIPE_DESIGN.md`
//! §8 Epic B).
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

use anyhow::{anyhow, Context, Result};
use isekai_auth::TokenProvider;
use isekai_bootstrap::{BootstrapBackend, HostSpec, JumpSpec, LaunchSpec, OpenSshBackend, RelayLaunchSpec};
use isekai_pipe_core::{default_profiles_dir, write_persistent_profile, PersistentProfile};
use isekai_trust::{HelperTrust, UpdatePolicy};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::cli::InitArgs;

pub async fn run(args: InitArgs) -> Result<()> {
    let target = parse_host_spec(&args.host)
        .with_context(|| format!("isekai-ssh: invalid host spec '{}'", args.host))?;
    let via = parse_via_chain(&target, &args.via)?;
    let backend = match &args.ssh_path {
        Some(ssh_path) => OpenSshBackend::new().with_ssh_program(ssh_path.to_string_lossy().into_owned()),
        None => OpenSshBackend::new(),
    };

    let helper_binary = crate::helper_download::resolve_helper_binary(
        args.helper_binary.as_deref(),
        &backend,
        &target,
        &via,
        &crate::helper_download::ReleaseSource { repo: args.helper_release_repo.clone(), tag: args.helper_release_tag.clone() },
    )
    .await
    .context(
        "isekai-ssh: no --helper-binary given and auto-download failed; pass --helper-binary explicitly \
         (or check --helper-release-repo/--helper-release-tag)",
    )?;
    let helper_sha256 = hex_sha256(&helper_binary);

    let relay_jwt = resolve_relay_jwt(&args)?;
    let relay = RelayLaunchSpec {
        relay_addr: args.relay_addr,
        relay_sni: args.relay_sni.clone(),
        relay_jwt,
        relay_transport: args.relay_transport.into(),
        idle_lifetime_secs: args.idle_lifetime,
        remote_log_level: args.remote_log_level.clone(),
    };

    println!("Deploying isekai-helper to {}...", args.host);
    let report = backend
        .install_and_start(&target, &via, &helper_binary, &LaunchSpec::Relay(relay), None, &args.stun_servers)
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
    if !args.via.is_empty() {
        println!("Via:             {}", args.via.join(" -> "));
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

    let profiles_dir = default_profiles_dir()
        .context("isekai-ssh: could not determine the profiles directory (is $HOME set?)")?;

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

    let trust = HelperTrust {
        identity_pubkey: identity,
        trusted_helper_sha256: helper_sha256,
        trusted_helper_version: args.helper_version.clone(),
        update_policy: UpdatePolicy::ExactDigestOnly,
        release_channel: args.release_channel.clone(),
        last_via: (!args.via.is_empty()).then(|| args.via.join(",")),
        trusted_at: now.clone(),
        last_seen_at: now,
        cached_relay_addr,
        cached_cert_sha256: handshake.cert_sha256().to_string(),
        cached_session_secret: handshake.session_secret.clone(),
        cached_stun_observed_addr: handshake.stun_observed_addr().map(str::to_string),
    };
    let profile = PersistentProfile::migrate_legacy_helper_trust(&key, &trust);
    let path = write_persistent_profile(&profiles_dir, &profile)
        .with_context(|| format!("isekai-ssh: failed to write profile to {}", profiles_dir.display()))?;

    println!("Registered '{key}' in {}", path.display());
    Ok(())
}

/// Resolves the relay bearer token for `RelayLaunchSpec::relay_jwt`: either
/// `--relay-jwt` verbatim, or (when `--relay-jwt-from-login` is set)
/// `isekai-ssh login`'s saved token file via `isekai_auth::FileTokenProvider`
/// (`ISEKAI_PIPE_DESIGN.md` §8 Epic F). `cli::InitArgs`'s `required_unless_present`/
/// `conflicts_with` clap attributes already guarantee exactly one of the two
/// was given, so the `None`/`None` and `Some`/`true` cases below are
/// unreachable in practice — handled anyway rather than trusting that
/// invariant blindly.
fn resolve_relay_jwt(args: &InitArgs) -> Result<String> {
    match (&args.relay_jwt, args.relay_jwt_from_login) {
        (Some(jwt), false) => Ok(jwt.clone()),
        (None, true) => isekai_auth::FileTokenProvider::from_default_path()
            .and_then(|provider| provider.get_relay_jwt())
            .context("isekai-ssh: failed to load a relay token from `isekai-ssh login` — run `isekai-ssh login` first"),
        (Some(_), true) => Err(anyhow!("--relay-jwt and --relay-jwt-from-login are mutually exclusive")),
        (None, false) => Err(anyhow!("one of --relay-jwt or --relay-jwt-from-login is required")),
    }
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

/// Parses every `--via` occurrence (in the order given, the traversal
/// order — first hop reached from the client, last hop before `target`)
/// into a jump-host chain and validates it with
/// `isekai-bootstrap-plan`'s shared hop-count/cycle checks
/// (`ISEKAI_PIPE_DESIGN.md` §8 Epic K), the same validation
/// `wrapper.rs`'s auto-bootstrap `--via` handling reuses rather than each
/// duplicating its own.
fn parse_via_chain(target: &HostSpec, via: &[String]) -> Result<Vec<JumpSpec>> {
    let chain: Vec<JumpSpec> =
        via.iter().map(|spec| parse_jump_spec(spec)).collect::<Result<_>>().with_context(|| {
            format!("isekai-ssh: invalid --via spec in {via:?}")
        })?;
    isekai_bootstrap_plan::BootstrapPlan::validate_jump_chain(target, &chain)
        .with_context(|| format!("isekai-ssh: invalid --via chain {via:?}"))?;
    Ok(chain)
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

    fn sample_init_args() -> InitArgs {
        InitArgs {
            host: "myhost".to_string(),
            via: Vec::new(),
            helper_binary: Some(std::path::PathBuf::from("/dev/null")),
            helper_release_repo: crate::helper_download::ReleaseSource::DEFAULT_REPO.to_string(),
            helper_release_tag: None,
            relay_addr: "127.0.0.1:1".parse().unwrap(),
            relay_sni: "relay.example.test".to_string(),
            relay_transport: crate::cli::RelayTransportArg::Udp,
            relay_jwt: Some("test-jwt".to_string()),
            relay_jwt_from_login: false,
            helper_version: "unknown".to_string(),
            release_channel: None,
            idle_lifetime: 2_592_000,
            stun_servers: Vec::new(),
            remote_log_level: "info".to_string(),
            ssh_path: None,
        }
    }

    #[test]
    fn resolve_relay_jwt_uses_the_explicit_flag_when_given() {
        let mut args = sample_init_args();
        args.relay_jwt = Some("explicit-token".to_string());
        args.relay_jwt_from_login = false;
        assert_eq!(resolve_relay_jwt(&args).unwrap(), "explicit-token");
    }

    #[test]
    fn resolve_relay_jwt_sources_from_the_login_token_file() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("isekai-ssh-init-relay-jwt-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);

        isekai_auth::FileTokenProvider::from_default_path().unwrap().save_relay_jwt("token-from-login").unwrap();

        let mut args = sample_init_args();
        args.relay_jwt = None;
        args.relay_jwt_from_login = true;
        assert_eq!(resolve_relay_jwt(&args).unwrap(), "token-from-login");

        if let Some(old_home) = old_home {
            std::env::set_var("HOME", old_home);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn resolve_relay_jwt_errors_when_login_requested_but_no_token_file_exists() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("isekai-ssh-init-relay-jwt-missing-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);

        let mut args = sample_init_args();
        args.relay_jwt = None;
        args.relay_jwt_from_login = true;
        let err = resolve_relay_jwt(&args).unwrap_err();
        assert!(err.to_string().contains("isekai-ssh login"), "{err}");

        if let Some(old_home) = old_home {
            std::env::set_var("HOME", old_home);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn resolve_relay_jwt_errors_when_neither_option_is_set() {
        let mut args = sample_init_args();
        args.relay_jwt = None;
        args.relay_jwt_from_login = false;
        assert!(resolve_relay_jwt(&args).is_err());
    }

}
