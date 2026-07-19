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
    WrapperPlan,
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
            .unwrap_or(plan.destination.as_str()),
        openssh.port.unwrap_or(22)
    );
    let mut builder = IsekaiConfigBuilder {
        enabled: None,
        bootstrap_policy: None,
        profile: None,
        remote_path: None,
        services: Vec::new(),
        bootstrap_candidates: Vec::new(),
        link_endpoints: Vec::new(),
        rendezvous: Vec::new(),
        stun_servers: Vec::new(),
        relay_endpoints: Vec::new(),
        resume_grace_secs: None,
        candidate_race_delay_ms: None,
        relay_delay_ms: None,
        install_mode: None,
        bootstrap_relay: None,
        ctl_socket_enabled: None,
        remote_log_level: None,
        remote_bind_port_range: None,
        local_bind_port_range: None,
    };
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
            alias: Some(plan.destination.clone()),
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
        profile: builder.profile.unwrap_or_else(|| plan.destination.clone()),
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
    })
}

#[derive(Debug)]
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
}

fn apply_isekai_directive(builder: &mut IsekaiConfigBuilder, directive: IsekaiDirective) -> Result<()> {
    match directive.name.as_str() {
        "enabled" => set_once(
            &mut builder.enabled,
            parse_yes_no(one_arg(&directive)?)?,
            "enabled",
        ),
        "bootstrap-policy" => set_once(
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
            "bootstrap-policy",
        ),
        "profile" => set_once(
            &mut builder.profile,
            one_arg(&directive)?.to_string(),
            "profile",
        ),
        "remote-path" => set_once(
            &mut builder.remote_path,
            one_arg(&directive)?.to_string(),
            "remote-path",
        ),
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
        "resume-grace" => set_once(
            &mut builder.resume_grace_secs,
            parse_duration_ms(one_arg(&directive)?, "resume-grace")?.div_ceil(1000),
            "resume-grace",
        ),
        "candidate-race-delay" => set_once(
            &mut builder.candidate_race_delay_ms,
            parse_duration_ms(one_arg(&directive)?, "candidate-race-delay")?,
            "candidate-race-delay",
        ),
        "relay-delay" => set_once(
            &mut builder.relay_delay_ms,
            parse_duration_ms(one_arg(&directive)?, "relay-delay")?,
            "relay-delay",
        ),
        "bootstrap-relay" => set_once(
            &mut builder.bootstrap_relay,
            parse_bootstrap_relay(&directive.args)?,
            "bootstrap-relay",
        ),
        "install-mode" => set_once(
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
            "install-mode",
        ),
        "ctl-socket" => set_once(
            &mut builder.ctl_socket_enabled,
            parse_yes_no(one_arg(&directive)?)?,
            "ctl-socket",
        ),
        "remote-log-level" => set_once(
            &mut builder.remote_log_level,
            match one_arg(&directive)? {
                level @ ("error" | "warn" | "info" | "debug" | "trace") => level.to_string(),
                other => {
                    return Err(anyhow!(
                        "isekai-ssh: invalid #@isekai remote-log-level {other:?} (expected one of error|warn|info|debug|trace)"
                    ))
                }
            },
            "remote-log-level",
        ),
        "remote-bind-port-range" => set_once(
            &mut builder.remote_bind_port_range,
            parse_bind_port_range(one_arg(&directive)?)?,
            "remote-bind-port-range",
        ),
        "local-bind-port-range" => set_once(
            &mut builder.local_bind_port_range,
            parse_bind_port_range(one_arg(&directive)?)?,
            "local-bind-port-range",
        ),
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

fn set_once<T>(slot: &mut Option<T>, value: T, name: &str) -> Result<()> {
    if slot.is_none() {
        *slot = Some(value);
    }
    let _ = name;
    Ok(())
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

/// Parses `#@isekai remote-bind-port-range <START>-<END>` into an inclusive
/// `(start, end)` pair, passed straight through to `isekai-helper
/// --bind-port-range` (`engine::parse_bind_port_range` in `isekai-pipe`
/// applies the identical `start <= end` validation server-side; this
/// duplicate client-side check exists only to fail closed at config
/// resolution time instead of after an SSH round-trip).
fn parse_bind_port_range(value: &str) -> Result<(u16, u16)> {
    let (start, end) = value.split_once('-').ok_or_else(|| {
        anyhow!("isekai-ssh: invalid #@isekai remote-bind-port-range {value:?} (expected <START>-<END>)")
    })?;
    let start: u16 = start
        .parse()
        .map_err(|_| anyhow!("isekai-ssh: invalid #@isekai remote-bind-port-range start {start:?}"))?;
    let end: u16 = end
        .parse()
        .map_err(|_| anyhow!("isekai-ssh: invalid #@isekai remote-bind-port-range end {end:?}"))?;
    if start > end {
        return Err(anyhow!(
            "isekai-ssh: invalid #@isekai remote-bind-port-range {value:?}: start must be <= end"
        ));
    }
    Ok((start, end))
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
}
