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
use isekai_release_verify::{verify_artifact, ExpectedTarget, SignedManifest, TrustedReleaseKeys};
use isekai_trust::{HelperTrust, UpdatePolicy};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::cli::InitArgs;

pub async fn run(args: InitArgs) -> Result<()> {
    let helper_binary = std::fs::read(&args.helper_binary)
        .with_context(|| format!("isekai-ssh: failed to read helper binary at {}", args.helper_binary.display()))?;
    let helper_sha256 = hex_sha256(&helper_binary);

    if let Some(manifest_path) = &args.helper_manifest {
        verify_helper_manifest(&args, manifest_path, &helper_binary)
            .with_context(|| "isekai-ssh: release manifest verification failed — refusing to deploy an unverified binary".to_string())?;
    }

    let target = parse_host_spec(&args.host)
        .with_context(|| format!("isekai-ssh: invalid host spec '{}'", args.host))?;
    let via = parse_via_chain(&target, &args.via)?;
    let relay_jwt = resolve_relay_jwt(&args)?;
    let relay = RelayLaunchSpec {
        relay_addr: args.relay_addr,
        relay_sni: args.relay_sni.clone(),
        relay_jwt,
        idle_lifetime_secs: args.idle_lifetime,
    };

    println!("Deploying isekai-helper to {}...", args.host);
    let backend = OpenSshBackend::new();
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

/// Loads `--helper-manifest`, builds a [`TrustedReleaseKeys`] registry from
/// `--trusted-release-key`, and verifies `helper_binary` against it
/// (`isekai-release-verify`, `ISEKAI_PIPE_DESIGN.md` §8 Epic D). Returns an
/// error — never deploys — on any verification failure.
fn verify_helper_manifest(args: &InitArgs, manifest_path: &std::path::Path, helper_binary: &[u8]) -> Result<()> {
    let manifest_bytes = std::fs::read(manifest_path)
        .with_context(|| format!("failed to read --helper-manifest at {}", manifest_path.display()))?;
    let signed: SignedManifest = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("failed to parse --helper-manifest at {} as a signed release manifest", manifest_path.display()))?;

    let mut keys = TrustedReleaseKeys::new();
    for entry in &args.trusted_release_keys {
        let (key_id, hex_pubkey) = entry
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid --trusted-release-key {entry:?}: expected KEY_ID=HEXPUBKEY"))?;
        keys.insert_hex(key_id, hex_pubkey).with_context(|| format!("invalid --trusted-release-key for key_id {key_id:?}"))?;
    }
    if keys.is_empty() {
        return Err(anyhow!("--helper-manifest was given but no --trusted-release-key was provided"));
    }
    let expect_platform = args.expect_platform.as_deref().ok_or_else(|| anyhow!("--expect-platform is required when --helper-manifest is given"))?;
    let expect_architecture =
        args.expect_architecture.as_deref().ok_or_else(|| anyhow!("--expect-architecture is required when --helper-manifest is given"))?;

    verify_artifact(&signed, helper_binary, &keys, ExpectedTarget { platform: expect_platform, architecture: expect_architecture })
        .map_err(|e| anyhow!("{e}"))?;
    println!(
        "Release manifest verified: version={} key_id={} channel={}",
        signed.manifest.version, signed.manifest.key_id, signed.manifest.release_channel
    );
    Ok(())
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
            helper_binary: std::path::PathBuf::from("/dev/null"),
            relay_addr: "127.0.0.1:1".parse().unwrap(),
            relay_sni: "relay.example.test".to_string(),
            relay_jwt: Some("test-jwt".to_string()),
            relay_jwt_from_login: false,
            helper_version: "unknown".to_string(),
            release_channel: None,
            idle_lifetime: 2_592_000,
            stun_servers: Vec::new(),
            helper_manifest: None,
            trusted_release_keys: Vec::new(),
            expect_platform: None,
            expect_architecture: None,
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

    fn signed_manifest_for(binary: &[u8], signing_key: &ed25519_dalek::SigningKey) -> isekai_release_verify::SignedManifest {
        isekai_release_verify::sign_manifest(
            isekai_release_verify::ReleaseManifest {
                version: "0.5.0".to_string(),
                platform: "linux".to_string(),
                architecture: "x86_64".to_string(),
                artifact_filename: "isekai-pipe".to_string(),
                size: binary.len() as u64,
                sha256: hex_sha256(binary),
                protocol_compat: "isekai-pipe/1".to_string(),
                release_channel: "stable".to_string(),
                key_id: "test-key".to_string(),
            },
            signing_key,
        )
    }

    fn write_manifest(dir: &std::path::Path, signed: &isekai_release_verify::SignedManifest) -> std::path::PathBuf {
        let path = dir.join("manifest.json");
        std::fs::write(&path, serde_json::to_vec(signed).unwrap()).unwrap();
        path
    }

    #[test]
    fn verify_helper_manifest_accepts_a_valid_matching_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = b"pretend-isekai-pipe-bytes".to_vec();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let signed = signed_manifest_for(&binary, &signing_key);
        let manifest_path = write_manifest(tmp.path(), &signed);

        let mut args = sample_init_args();
        let pubkey_hex: String = signing_key.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        args.trusted_release_keys = vec![format!("test-key={pubkey_hex}")];
        args.expect_platform = Some("linux".to_string());
        args.expect_architecture = Some("x86_64".to_string());

        assert!(verify_helper_manifest(&args, &manifest_path, &binary).is_ok());
    }

    #[test]
    fn verify_helper_manifest_rejects_a_tampered_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = b"pretend-isekai-pipe-bytes".to_vec();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let signed = signed_manifest_for(&binary, &signing_key);
        let manifest_path = write_manifest(tmp.path(), &signed);

        let mut args = sample_init_args();
        let pubkey_hex: String = signing_key.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        args.trusted_release_keys = vec![format!("test-key={pubkey_hex}")];
        args.expect_platform = Some("linux".to_string());
        args.expect_architecture = Some("x86_64".to_string());

        let tampered = b"pretend-isekai-pipe-BYTES".to_vec();
        assert!(verify_helper_manifest(&args, &manifest_path, &tampered).is_err());
    }

    #[test]
    fn verify_helper_manifest_rejects_when_no_trusted_key_given() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = b"pretend-isekai-pipe-bytes".to_vec();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let signed = signed_manifest_for(&binary, &signing_key);
        let manifest_path = write_manifest(tmp.path(), &signed);

        let mut args = sample_init_args();
        args.expect_platform = Some("linux".to_string());
        args.expect_architecture = Some("x86_64".to_string());
        // Deliberately no --trusted-release-key.

        let err = verify_helper_manifest(&args, &manifest_path, &binary).unwrap_err();
        assert!(err.to_string().contains("no --trusted-release-key"), "{err}");
    }

    #[test]
    fn verify_helper_manifest_rejects_missing_expect_platform() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = b"pretend-isekai-pipe-bytes".to_vec();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let signed = signed_manifest_for(&binary, &signing_key);
        let manifest_path = write_manifest(tmp.path(), &signed);

        let mut args = sample_init_args();
        let pubkey_hex: String = signing_key.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        args.trusted_release_keys = vec![format!("test-key={pubkey_hex}")];
        args.expect_architecture = Some("x86_64".to_string());
        // Deliberately no --expect-platform.

        let err = verify_helper_manifest(&args, &manifest_path, &binary).unwrap_err();
        assert!(err.to_string().contains("--expect-platform"), "{err}");
    }

    #[test]
    fn verify_helper_manifest_rejects_a_signature_from_an_untrusted_key() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = b"pretend-isekai-pipe-bytes".to_vec();
        let attacker_key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let signed = signed_manifest_for(&binary, &attacker_key);
        let manifest_path = write_manifest(tmp.path(), &signed);

        let mut args = sample_init_args();
        // Trust a *different* key under the same key_id the manifest uses.
        let real_key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let pubkey_hex: String = real_key.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        args.trusted_release_keys = vec![format!("test-key={pubkey_hex}")];
        args.expect_platform = Some("linux".to_string());
        args.expect_architecture = Some("x86_64".to_string());

        assert!(verify_helper_manifest(&args, &manifest_path, &binary).is_err());
    }
}
