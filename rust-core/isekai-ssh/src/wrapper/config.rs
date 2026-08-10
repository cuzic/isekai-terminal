//! Resolves the `#@isekai` [`super::directive::IsekaiDirective`]s found by
//! [`super::directive`] into a final [`super::IsekaiConfig`] — applying
//! each directive to an [`IsekaiConfigBuilder`] in order (first occurrence
//! wins per [`set_once`], matching `ssh(1)`'s own "first match wins"
//! `Host`/config semantics) and filling in defaults for whatever no
//! directive set.

use std::net::SocketAddr;

use anyhow::{anyhow, Result};
use isekai_bootstrap::RelayTransportKind;
use isekai_pipe_core::{ServiceSpec, DEFAULT_CANDIDATE_RACE_DELAY_MS, DEFAULT_RELAY_DELAY_MS};

use super::directive::{load_isekai_directives, IsekaiDirective};
use super::{
    BootstrapCandidate, BootstrapPolicy, BootstrapRelayTarget, InstallMode, IsekaiConfig, OpenSshEffectiveConfig,
    TtyDirective, WrapperPlan,
};

pub(super) fn resolve_isekai_config(
    plan: &WrapperPlan,
    openssh: &OpenSshEffectiveConfig,
) -> Result<IsekaiConfig> {
    let directives = load_isekai_directives(plan)?;
    let default_target = format!(
        "{}:{}",
        openssh
            .hostname
            .as_deref()
            .unwrap_or(plan.destination_host()),
        openssh.port.unwrap_or(22)
    );
    let mut builder = IsekaiConfigBuilder::default();
    for directive in directives {
        apply_isekai_directive(&mut builder, directive)?;
    }
    if builder.bootstrap_candidates.is_empty() {
        builder.bootstrap_candidates.push(BootstrapCandidate {
            target: default_target,
            via: openssh
                .proxy_jump
                .as_deref()
                .map(parse_jump_chain)
                .unwrap_or_default(),
            priority: 100,
            alias: Some(plan.destination_host().to_string()),
        });
    }
    if builder.services.is_empty() {
        builder
            .services
            .push(ServiceSpec::ssh_target("127.0.0.1:22").expect("default service is valid"));
    }
    // `install-mode=system` needs sudo handling, ownership/permissions,
    // overwrite-of-an-existing-binary semantics, and update/rollback — none
    // of which exist, and none of which are currently planned (if ever
    // pursued, a separate `curl ... | sudo bash`-style installer script/
    // wrapper is the likely shape, not native support inside `isekai-ssh`
    // itself). Rather than silently wiring it through as if it were
    // equivalent to `user` (or silently ignoring it), fail closed here at
    // config-resolution time so a typo'd or aspirational `#@isekai
    // install-mode system` never gets treated as meaning something it
    // doesn't (`ISEKAI_PIPE_DESIGN.md`).
    if builder.install_mode == Some(InstallMode::System) {
        return Err(anyhow!(
            "isekai-ssh: '#@isekai install-mode system' is not supported (no sudo/ownership/\
             rollback design exists, and none is planned) — remove the directive or use \
             'install-mode user'"
        ));
    }
    Ok(IsekaiConfig {
        enabled: builder.enabled.unwrap_or(true),
        bootstrap_policy: builder.bootstrap_policy.unwrap_or(BootstrapPolicy::Auto),
        profile: builder.profile.unwrap_or_else(|| plan.destination_host().to_string()),
        remote_path: builder.remote_path,
        services: builder.services,
        bootstrap_candidates: builder.bootstrap_candidates,
        link_endpoints: builder.link_endpoints,
        rendezvous: builder.rendezvous,
        stun_servers: builder.stun_servers,
        relay_endpoints: builder.relay_endpoints,
        resume_grace_secs: builder.resume_grace_secs.unwrap_or(isekai_pipe_core::DEFAULT_RESUME_GRACE_SECS),
        candidate_race_delay_ms: builder
            .candidate_race_delay_ms
            .unwrap_or(DEFAULT_CANDIDATE_RACE_DELAY_MS),
        relay_delay_ms: builder.relay_delay_ms.unwrap_or(DEFAULT_RELAY_DELAY_MS),
        install_mode: builder.install_mode.unwrap_or(InstallMode::User),
        bootstrap_relay: builder.bootstrap_relay,
        ctl_socket_enabled: builder.ctl_socket_enabled.unwrap_or(false),
        remote_log_level: builder.remote_log_level.unwrap_or_else(|| "info".to_string()),
        remote_bind_port_range: builder.remote_bind_port_range,
        local_bind_port_range: builder.local_bind_port_range,
        tab_idle_color: builder.tab_idle_color,
        tab_attention_color: builder.tab_attention_color,
        tty: builder.tty,
    })
}

#[derive(Debug, Default)]
struct IsekaiConfigBuilder {
    enabled: Option<bool>,
    bootstrap_policy: Option<BootstrapPolicy>,
    profile: Option<String>,
    remote_path: Option<String>,
    services: Vec<ServiceSpec>,
    bootstrap_candidates: Vec<BootstrapCandidate>,
    link_endpoints: Vec<String>,
    rendezvous: Vec<String>,
    stun_servers: Vec<String>,
    relay_endpoints: Vec<String>,
    bootstrap_relay: Option<BootstrapRelayTarget>,
    resume_grace_secs: Option<u64>,
    candidate_race_delay_ms: Option<u64>,
    relay_delay_ms: Option<u64>,
    install_mode: Option<InstallMode>,
    ctl_socket_enabled: Option<bool>,
    remote_log_level: Option<String>,
    remote_bind_port_range: Option<(u16, u16)>,
    local_bind_port_range: Option<(u16, u16)>,
    tab_idle_color: Option<(u8, u8, u8)>,
    tab_attention_color: Option<(u8, u8, u8)>,
    tty: Option<TtyDirective>,
}

fn apply_isekai_directive(builder: &mut IsekaiConfigBuilder, directive: IsekaiDirective) -> Result<()> {
    match directive.name.as_str() {
        "enabled" => {
            set_once(&mut builder.enabled, parse_yes_no(one_arg(&directive)?)?);
            Ok(())
        }
        "bootstrap-policy" => {
            set_once(
                &mut builder.bootstrap_policy,
                match one_arg(&directive)? {
                    "auto" => BootstrapPolicy::Auto,
                    "always" => BootstrapPolicy::Always,
                    "never" => BootstrapPolicy::Never,
                    other => {
                        return Err(anyhow!(
                            "isekai-ssh: invalid #@isekai bootstrap-policy {other:?}"
                        ))
                    }
                },
            );
            Ok(())
        }
        "profile" => {
            set_once(&mut builder.profile, one_arg(&directive)?.to_string());
            Ok(())
        }
        "remote-path" => {
            set_once(&mut builder.remote_path, one_arg(&directive)?.to_string());
            Ok(())
        }
        "service" => {
            for arg in &directive.args {
                builder.services.push(
                    ServiceSpec::parse(arg).map_err(|e| {
                        anyhow!("isekai-ssh: invalid #@isekai service {arg:?}: {e}")
                    })?,
                );
            }
            Ok(())
        }
        "bootstrap-candidate" => {
            builder
                .bootstrap_candidates
                .push(parse_bootstrap_candidate(&directive.args)?);
            Ok(())
        }
        "link" => append_args(&mut builder.link_endpoints, &directive),
        "rendezvous" => append_args(&mut builder.rendezvous, &directive),
        "stun" => append_args(&mut builder.stun_servers, &directive),
        "relay" => append_args(&mut builder.relay_endpoints, &directive),
        "resume-grace" => {
            set_once(
                &mut builder.resume_grace_secs,
                parse_duration_ms(one_arg(&directive)?, "resume-grace")?.div_ceil(1000),
            );
            Ok(())
        }
        "candidate-race-delay" => {
            set_once(
                &mut builder.candidate_race_delay_ms,
                parse_duration_ms(one_arg(&directive)?, "candidate-race-delay")?,
            );
            Ok(())
        }
        "relay-delay" => {
            set_once(
                &mut builder.relay_delay_ms,
                parse_duration_ms(one_arg(&directive)?, "relay-delay")?,
            );
            Ok(())
        }
        "bootstrap-relay" => {
            set_once(&mut builder.bootstrap_relay, parse_bootstrap_relay(&directive.args)?);
            Ok(())
        }
        "install-mode" => {
            set_once(
                &mut builder.install_mode,
                match one_arg(&directive)? {
                    "user" => InstallMode::User,
                    "system" => InstallMode::System,
                    other => {
                        return Err(anyhow!(
                            "isekai-ssh: invalid #@isekai install-mode {other:?}"
                        ))
                    }
                },
            );
            Ok(())
        }
        "ctl-socket" => {
            set_once(&mut builder.ctl_socket_enabled, parse_yes_no(one_arg(&directive)?)?);
            Ok(())
        }
        "remote-log-level" => {
            set_once(
                &mut builder.remote_log_level,
                match one_arg(&directive)? {
                    level @ ("error" | "warn" | "info" | "debug" | "trace") => level.to_string(),
                    other => {
                        return Err(anyhow!(
                            "isekai-ssh: invalid #@isekai remote-log-level {other:?} (expected one of error|warn|info|debug|trace)"
                        ))
                    }
                },
            );
            Ok(())
        }
        "remote-bind-port-range" => {
            set_once(&mut builder.remote_bind_port_range, parse_bind_port_range(one_arg(&directive)?, "remote-bind-port-range")?);
            Ok(())
        }
        "local-bind-port-range" => {
            set_once(&mut builder.local_bind_port_range, parse_bind_port_range(one_arg(&directive)?, "local-bind-port-range")?);
            Ok(())
        }
        "tab-idle-color" => {
            apply_optional_tab_color(&mut builder.tab_idle_color, "tab-idle-color", &directive);
            Ok(())
        }
        "tab-attention-color" => {
            apply_optional_tab_color(&mut builder.tab_attention_color, "tab-attention-color", &directive);
            Ok(())
        }
        "tty" => {
            apply_optional_tty(&mut builder.tty, &directive);
            Ok(())
        }
        other => Err(anyhow!("isekai-ssh: unknown #@isekai directive {other:?}")),
    }
}

fn append_args(target: &mut Vec<String>, directive: &IsekaiDirective) -> Result<()> {
    if directive.args.is_empty() {
        return Err(anyhow!(
            "isekai-ssh: #@isekai {} expects at least one argument",
            directive.name
        ));
    }
    target.extend(directive.args.iter().cloned());
    Ok(())
}

/// First-match-wins assignment (`ssh(1)`'s own `Host`/config semantics,
/// matching every directive in this file): a no-op once `slot` is already
/// set. Used to take a `name: &str` purely for a later error message that
/// never actually got produced (the body ended in an unconditional `Ok(())`
/// regardless) — dropped along with the `Result` return type it never used
/// either, since every real caller already had its own `?`-propagated error
/// from parsing the value *before* calling this.
fn set_once<T>(slot: &mut Option<T>, value: T) {
    if slot.is_none() {
        *slot = Some(value);
    }
}

fn one_arg(directive: &IsekaiDirective) -> Result<&str> {
    match directive.args.as_slice() {
        [single] => Ok(single),
        _ => Err(anyhow!(
            "isekai-ssh: #@isekai {} expects exactly one argument",
            directive.name
        )),
    }
}

fn parse_yes_no(value: &str) -> Result<bool> {
    match value {
        "yes" | "true" | "on" | "1" => Ok(true),
        "no" | "false" | "off" | "0" => Ok(false),
        _ => Err(anyhow!("isekai-ssh: expected yes/no, got {value:?}")),
    }
}

fn parse_duration_ms(value: &str, field: &str) -> Result<u64> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1000)
    } else {
        (value, 1000)
    };
    let amount: u64 = number
        .parse()
        .map_err(|_| anyhow!("isekai-ssh: invalid #@isekai {field} duration {value:?}"))?;
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("isekai-ssh: #@isekai {field} duration is too large"))
}

/// Applies a `tty` directive leniently, matching [`apply_optional_tab_color`]'s
/// warn-and-ignore convention (`.claude/rules/always-connects.md`) rather
/// than this file's usual `?`-propagating fail-closed one: unlike most
/// directives here, a bad `tty` value must not be the reason
/// `isekai-ssh <host>` refuses to connect at all — a typo'd or
/// space-containing value in a broadly-matching `Host *` block would
/// otherwise break every host it matches. Two ways a value can be bad, both
/// handled the same way (warn to stderr, leave `slot` unset so the caller
/// falls through to a plain login shell):
///
/// - Wrong argument count (`one_arg`'s own failure mode) — e.g. a bare
///   `#@isekai tty` or an unquoted name containing whitespace.
/// - An explicit `<name>` that `is_valid_tty_name` would reject (path
///   separator, `.`/`..`, embedded NUL, empty, or over 200 bytes) — mirrors
///   how `resolve_name_from`'s `Auto` branch already *skips* (never errors
///   on) an invalid *derived* candidate; treating an explicit directive
///   value more strictly than an auto-derived one would be inconsistent, and
///   would otherwise only be caught after a wasted connection attempt (the
///   remote `isekai-pipe tty attach` exits immediately with `EX_USAGE`).
///
/// `auto`/`off` are reserved keywords, not validated against as a literal
/// session name: a project genuinely named `auto`/`off` can't be selected
/// this way (use `--isekai-tty=auto`/`--isekai-tty=off` instead, which has
/// no such ambiguity — see `README.md`'s `--isekai-tty` section), a small,
/// deliberately accepted trade-off next to not needing a separate keyword
/// syntax.
fn apply_optional_tty(slot: &mut Option<TtyDirective>, directive: &IsekaiDirective) {
    apply_lenient(slot, "connecting with a plain login shell instead", directive, |directive| {
        let value = one_arg(directive).map_err(|e| format!("tty: {e}"))?;
        match value {
            "auto" => Ok(TtyDirective::Auto),
            "off" => Ok(TtyDirective::Off),
            name if crate::tty_attach::is_valid_tty_name(name) => Ok(TtyDirective::Named(name.to_string())),
            name => Err(format!(
                "tty {name:?}: not a valid isekai-pipe tty session name \
                 (must not be empty/\".\"/\"..\", must not contain '/' or NUL, must be <= 200 bytes)"
            )),
        }
    });
}

/// Shared "leniently apply a directive" skeleton
/// (`.claude/rules/always-connects.md`): a no-op once `slot` is already set
/// (`set_once`'s first-match-wins semantics, matching every directive in
/// this file), and a `parse` failure prints `isekai-ssh: ignoring #@isekai
/// {tail} — {fallback}` to stderr and leaves `slot` unset rather than
/// aborting config resolution — [`apply_optional_tab_color`] and
/// [`apply_optional_tty`] each hand-rolled this exact shape independently.
/// `parse`'s `Err` is the message *tail* (after "ignoring #@isekai ",
/// before the em dash) rather than a bare `anyhow::Error`, so each caller
/// keeps full control of exactly how the directive name/bad value are
/// worded in it — `apply_optional_tty` alone has two differently-worded
/// failure modes, and `apply_optional_tab_color`'s own wording differs from
/// both.
fn apply_lenient<T>(
    slot: &mut Option<T>,
    fallback: &str,
    directive: &IsekaiDirective,
    parse: impl FnOnce(&IsekaiDirective) -> std::result::Result<T, String>,
) {
    if slot.is_some() {
        return;
    }
    match parse(directive) {
        Ok(value) => *slot = Some(value),
        Err(tail) => eprintln!("isekai-ssh: ignoring #@isekai {tail} — {fallback}"),
    }
}

/// Parses `#@isekai remote-bind-port-range`/`local-bind-port-range
/// <START>-<END>` into an inclusive `(start, end)` pair. The
/// `remote-bind-port-range` value is passed straight through to
/// `isekai-helper --bind-port-range` (`engine::parse_bind_port_range` in
/// `isekai-pipe` applies the identical `start <= end` validation
/// server-side; this duplicate client-side check exists only to fail closed
/// at config resolution time instead of after an SSH round-trip);
/// `local-bind-port-range` is client-side only and has no such server-side
/// counterpart. `field` (matching the `parse_duration_ms`/`parse_tab_color`
/// pattern above) names the actual directive in every error message — this
/// function used to hardcode `"remote-bind-port-range"` in all four even
/// when called for `local-bind-port-range`, copy-paste residue from when
/// there was only the one caller, which produced a misleading error message
/// pointing at the wrong directive.
fn parse_bind_port_range(value: &str, field: &str) -> Result<(u16, u16)> {
    let (start, end) = value
        .split_once('-')
        .ok_or_else(|| anyhow!("isekai-ssh: invalid #@isekai {field} {value:?} (expected <START>-<END>)"))?;
    let start: u16 = start.parse().map_err(|_| anyhow!("isekai-ssh: invalid #@isekai {field} start {start:?}"))?;
    let end: u16 = end.parse().map_err(|_| anyhow!("isekai-ssh: invalid #@isekai {field} end {end:?}"))?;
    if start > end {
        return Err(anyhow!("isekai-ssh: invalid #@isekai {field} {value:?}: start must be <= end"));
    }
    Ok((start, end))
}

/// Parses `#@isekai tab-idle-color`/`tab-attention-color <rrggbb>` using the
/// same validator `isekai-pipe ctl tab-color` and `claude-hookd` use
/// (`isekai_pipe_core::parse_hex_color`, `ISEKAI_PIPE_DESIGN.md` §8 Epic Q).
/// A caller in this file must **not** propagate this `Err` out of
/// `apply_isekai_directive` via `?` (see [`apply_optional_tab_color`],
/// the only caller) — that would abort config resolution, and therefore the
/// whole `isekai-ssh <host>` connection attempt, over a typo in a purely
/// cosmetic feature. Validating the *syntax* here still matters even though
/// a failure is non-fatal: an unvalidated value would otherwise reach
/// `ctl_forward.rs::build_login_shell_command`, which embeds it directly into the
/// shell command line `isekai-ssh` execs on the remote to establish the
/// session (see that function's doc comment on why bare validated hex,
/// never arbitrary text, is required there) — this function is what
/// guarantees only a validated `(u8, u8, u8)` or nothing ever reaches that
/// point, regardless of how the caller handles the error.
fn parse_tab_color(value: &str, field: &str) -> Result<(u8, u8, u8)> {
    isekai_pipe_core::parse_hex_color(value).map_err(|e| anyhow!("isekai-ssh: invalid #@isekai {field}: {e}"))
}

/// Applies a `tab-idle-color`/`tab-attention-color` directive leniently:
/// unlike every other directive in this file (which fail closed via `?`,
/// aborting config resolution on a syntax error), a malformed value here
/// logs a warning to stderr and leaves `slot` unset — `claude-hookd` then
/// falls back to its own built-in default color, and the connection
/// proceeds normally. This is deliberately inconsistent with the rest of
/// this file: those directives change *how* or *whether* the connection is
/// established, so failing fast on a typo protects the user from a silently
/// wrong connection; this one only changes a decorative tab color, and
/// `.claude/rules/always-connects.md` — this feature's own design doc cites
/// the same principle `apply_ctl_socket_forward` already applies to a
/// failed ctl-socket forward — is unambiguous that a cosmetic feature must
/// never be the reason `isekai-ssh <host>` refuses to connect at all.
/// (Found by Codex code review, 2026-07-25: the original `?`-propagating
/// version did exactly that.)
fn apply_optional_tab_color(slot: &mut Option<(u8, u8, u8)>, field: &str, directive: &IsekaiDirective) {
    apply_lenient(slot, "claude-hookd will use its built-in default color instead", directive, |directive| {
        one_arg(directive)
            .and_then(|value| parse_tab_color(value, field))
            .map_err(|e| format!("{field}: {e}"))
    });
}

fn parse_bootstrap_candidate(args: &[String]) -> Result<BootstrapCandidate> {
    let mut target = None;
    let mut via = Vec::new();
    let mut priority = 100;
    for arg in args {
        let Some((key, value)) = arg.split_once('=') else {
            return Err(anyhow!(
                "isekai-ssh: bootstrap-candidate argument must be key=value: {arg:?}"
            ));
        };
        match key {
            "target" => target = Some(value.to_string()),
            "via" => via = parse_jump_chain(value),
            "priority" => {
                priority = value.parse().map_err(|_| {
                    anyhow!("isekai-ssh: invalid bootstrap-candidate priority {value:?}")
                })?;
            }
            _ => {
                return Err(anyhow!(
                    "isekai-ssh: unknown bootstrap-candidate key {key:?}"
                ))
            }
        }
    }
    Ok(BootstrapCandidate {
        target: target
            .ok_or_else(|| anyhow!("isekai-ssh: bootstrap-candidate requires target=..."))?,
        via,
        priority,
        alias: None,
    })
}

fn parse_bootstrap_relay(args: &[String]) -> Result<BootstrapRelayTarget> {
    let mut relay_addr = None;
    let mut relay_sni = None;
    let mut relay_transport = RelayTransportKind::Udp;
    for arg in args {
        let Some((key, value)) = arg.split_once('=') else {
            return Err(anyhow!("isekai-ssh: bootstrap-relay argument must be key=value: {arg:?}"));
        };
        match key {
            "addr" => {
                relay_addr = Some(
                    value.parse::<SocketAddr>().map_err(|e| anyhow!("isekai-ssh: invalid bootstrap-relay addr {value:?}: {e}"))?,
                )
            }
            "sni" => {
                if value.is_empty() {
                    return Err(anyhow!("isekai-ssh: bootstrap-relay sni must not be empty"));
                }
                relay_sni = Some(value.to_string())
            }
            "transport" => {
                relay_transport = match value {
                    "udp" => RelayTransportKind::Udp,
                    "qmux" => RelayTransportKind::Qmux,
                    other => {
                        return Err(anyhow!("isekai-ssh: invalid bootstrap-relay transport {other:?} (expected udp|qmux)"))
                    }
                }
            }
            _ => return Err(anyhow!("isekai-ssh: unknown bootstrap-relay key {key:?}")),
        }
    }
    Ok(BootstrapRelayTarget {
        relay_addr: relay_addr.ok_or_else(|| anyhow!("isekai-ssh: bootstrap-relay requires addr=..."))?,
        relay_sni: relay_sni.ok_or_else(|| anyhow!("isekai-ssh: bootstrap-relay requires sni=..."))?,
        relay_transport,
    })
}

fn parse_jump_chain(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|hop| !hop.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn parse_bootstrap_relay_accepts_addr_and_sni() {
        let target = parse_bootstrap_relay(&s(&["addr=203.0.113.10:443", "sni=relay.example.com"])).unwrap();
        assert_eq!(
            target,
            BootstrapRelayTarget {
                relay_addr: "203.0.113.10:443".parse().unwrap(),
                relay_sni: "relay.example.com".to_string(),
                relay_transport: RelayTransportKind::Udp,
            }
        );
    }

    #[test]
    fn parse_bootstrap_relay_accepts_transport_qmux() {
        let target =
            parse_bootstrap_relay(&s(&["addr=203.0.113.10:443", "sni=relay.example.com", "transport=qmux"])).unwrap();
        assert_eq!(target.relay_transport, RelayTransportKind::Qmux);
    }

    #[test]
    fn parse_bootstrap_relay_rejects_unknown_transport() {
        let err = parse_bootstrap_relay(&s(&["addr=203.0.113.10:443", "sni=relay.example.com", "transport=bogus"]));
        assert!(err.is_err());
    }

    #[test]
    fn parse_bootstrap_relay_rejects_missing_addr() {
        assert!(parse_bootstrap_relay(&s(&["sni=relay.example.com"])).is_err());
    }

    #[test]
    fn parse_bootstrap_relay_rejects_missing_sni() {
        assert!(parse_bootstrap_relay(&s(&["addr=203.0.113.10:443"])).is_err());
    }

    #[test]
    fn parse_bootstrap_relay_rejects_invalid_addr() {
        assert!(parse_bootstrap_relay(&s(&["addr=not-an-addr", "sni=relay.example.com"])).is_err());
    }

    #[test]
    fn parse_bootstrap_relay_rejects_empty_sni() {
        assert!(parse_bootstrap_relay(&s(&["addr=203.0.113.10:443", "sni="])).is_err());
    }

    #[test]
    fn parse_bootstrap_relay_rejects_unknown_key() {
        assert!(parse_bootstrap_relay(&s(&["addr=203.0.113.10:443", "sni=relay.example.com", "jwt=abc"])).is_err());
    }

    #[test]
    fn parse_tab_color_accepts_bare_and_hash_prefixed_hex() {
        assert_eq!(parse_tab_color("ff0000", "tab-idle-color").unwrap(), (0xff, 0x00, 0x00));
        assert_eq!(parse_tab_color("#00ff80", "tab-attention-color").unwrap(), (0x00, 0xff, 0x80));
    }

    #[test]
    fn parse_tab_color_rejects_invalid_values() {
        assert!(parse_tab_color("not-a-color", "tab-idle-color").is_err());
        assert!(parse_tab_color("$(id)", "tab-idle-color").is_err());
        assert!(parse_tab_color("", "tab-idle-color").is_err());
    }

    #[test]
    fn apply_isekai_directive_sets_tab_colors_once() {
        let mut builder = empty_builder();
        apply_isekai_directive(&mut builder, IsekaiDirective { name: "tab-idle-color".to_string(), args: s(&["202020"]) }).unwrap();
        apply_isekai_directive(&mut builder, IsekaiDirective { name: "tab-attention-color".to_string(), args: s(&["ff8800"]) })
            .unwrap();
        assert_eq!(builder.tab_idle_color, Some((0x20, 0x20, 0x20)));
        assert_eq!(builder.tab_attention_color, Some((0xff, 0x88, 0x00)));

        // `set_once`: a second `tab-idle-color` directive (e.g. a later
        // `Host` block matching the same connection) must not override the
        // first, matching `ssh(1)`'s own "first match wins" semantics.
        apply_isekai_directive(&mut builder, IsekaiDirective { name: "tab-idle-color".to_string(), args: s(&["ffffff"]) }).unwrap();
        assert_eq!(builder.tab_idle_color, Some((0x20, 0x20, 0x20)));
    }

    #[test]
    fn tty_directive_parses_auto_off_and_a_literal_name() {
        let mut builder = empty_builder();
        apply_isekai_directive(&mut builder, IsekaiDirective { name: "tty".to_string(), args: s(&["auto"]) }).unwrap();
        assert_eq!(builder.tty, Some(TtyDirective::Auto));

        let mut builder = empty_builder();
        apply_isekai_directive(&mut builder, IsekaiDirective { name: "tty".to_string(), args: s(&["off"]) }).unwrap();
        assert_eq!(builder.tty, Some(TtyDirective::Off));

        let mut builder = empty_builder();
        apply_isekai_directive(&mut builder, IsekaiDirective { name: "tty".to_string(), args: s(&["work"]) }).unwrap();
        assert_eq!(builder.tty, Some(TtyDirective::Named("work".to_string())));
    }

    #[test]
    fn tty_directive_is_set_once() {
        let mut builder = empty_builder();
        apply_isekai_directive(&mut builder, IsekaiDirective { name: "tty".to_string(), args: s(&["auto"]) }).unwrap();
        // A later `Host` block's `tty off` must not override an earlier
        // match's `tty auto`, same first-match-wins rule as every other
        // directive in this file.
        apply_isekai_directive(&mut builder, IsekaiDirective { name: "tty".to_string(), args: s(&["off"]) }).unwrap();
        assert_eq!(builder.tty, Some(TtyDirective::Auto));
    }

    /// Unlike most directives in this file, a malformed `tty` value must
    /// never abort config resolution (and therefore the whole `isekai-ssh
    /// <host>` connection attempt) — same `always-connects.md` convention
    /// `invalid_tab_color_directive_does_not_abort_config_resolution` pins
    /// for `tab-idle-color`/`tab-attention-color`. The wrong-argument-count
    /// case (`one_arg`'s own failure mode) is a no-op, same as an invalid
    /// explicit `<name>` below.
    #[test]
    fn a_malformed_tty_directive_is_a_no_op_not_an_error() {
        let mut builder = empty_builder();
        let result = apply_isekai_directive(&mut builder, IsekaiDirective { name: "tty".to_string(), args: s(&[]) });
        assert!(result.is_ok(), "a malformed directive must never abort config resolution");
        assert_eq!(builder.tty, None);

        let result =
            apply_isekai_directive(&mut builder, IsekaiDirective { name: "tty".to_string(), args: s(&["auto", "extra"]) });
        assert!(result.is_ok());
        assert_eq!(builder.tty, None);
    }

    /// Mirrors how `resolve_name_from`'s `Auto` branch already treats an
    /// invalid *derived* candidate (skip, don't error) — an explicit
    /// directive value getting stricter treatment would be inconsistent,
    /// and would otherwise only surface after a wasted connection attempt
    /// (the remote `isekai-pipe tty attach` rejects it with `EX_USAGE`).
    #[test]
    fn tty_directive_with_an_invalid_name_is_a_no_op_not_an_error() {
        for invalid in ["", ".", "..", "has/slash", &"x".repeat(201)] {
            let mut builder = empty_builder();
            let result =
                apply_isekai_directive(&mut builder, IsekaiDirective { name: "tty".to_string(), args: s(&[invalid]) });
            assert!(result.is_ok(), "{invalid:?} must not abort config resolution");
            assert_eq!(builder.tty, None, "{invalid:?} must not be accepted as a session name");
        }
    }

    /// Pins the fix for the bug Codex code review found (2026-07-25): a
    /// malformed `tab-idle-color`/`tab-attention-color` value used to
    /// propagate its parse error out of `apply_isekai_directive` via `?`,
    /// which `resolve_isekai_config` would in turn propagate out as a
    /// connection-resolution failure — a typo in this purely cosmetic
    /// directive used to abort the entire `isekai-ssh <host>` connection
    /// attempt, violating `.claude/rules/always-connects.md`. It must
    /// instead be a no-op (`Ok(())`, `slot` stays `None`) so the connection
    /// proceeds and `claude-hookd` just falls back to its default color.
    #[test]
    fn invalid_tab_color_directive_does_not_abort_config_resolution() {
        let mut builder = empty_builder();
        let result = apply_isekai_directive(&mut builder, IsekaiDirective { name: "tab-idle-color".to_string(), args: s(&["not-a-color"]) });
        assert!(result.is_ok(), "a cosmetic directive's typo must never fail config resolution");
        assert_eq!(builder.tab_idle_color, None);

        // Same for the wrong-argument-count case (`one_arg`'s own failure
        // mode), not just `parse_hex_color`'s.
        let result = apply_isekai_directive(&mut builder, IsekaiDirective { name: "tab-attention-color".to_string(), args: s(&[]) });
        assert!(result.is_ok());
        assert_eq!(builder.tab_attention_color, None);
    }

    fn empty_builder() -> IsekaiConfigBuilder {
        IsekaiConfigBuilder::default()
    }
}
