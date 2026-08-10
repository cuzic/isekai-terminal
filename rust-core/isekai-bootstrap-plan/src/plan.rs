//! `--via` jump-host chain validation: the pure hop-count/cycle checks that
//! `isekai-ssh init` and `isekai-ssh`'s wrapper auto-bootstrap
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic K's planner `2-a`, "unsupported構成判定")
//! share instead of each hand-rolling its own chain validation.
//!
//! Reuses `isekai-bootstrap`'s per-hop value types ([`HostSpec`]/[`JumpSpec`])
//! rather than redefining them.

use isekai_bootstrap::{HostSpec, JumpSpec};

/// The final SSH bootstrap destination — where `isekai-pipe serve` gets
/// installed and started. Distinct name from [`HostSpec`] at this crate's
/// API boundary even though the shape is identical today, so a future
/// destination-only field (e.g. a service-target override) doesn't force a
/// matching change onto every [`JumpHost`] in the chain.
pub type BootstrapTarget = HostSpec;

/// One hop in a `--via` jump chain, reusing `isekai-bootstrap`'s existing
/// `-J`/`ProxyJump` value type.
pub type JumpHost = JumpSpec;

/// Hop chains longer than this are rejected outright rather than attempted.
/// Chosen generously above any topology this project expects to support
/// (`ISEKAI_PIPE_DESIGN.md` §8 Epic K only asks for "multi-hop", not a
/// specific bound) while still catching a malformed/looping config before
/// it reaches `ssh(1)`.
pub const MAX_JUMP_HOPS: usize = 8;

/// Validates a `--via` jump-host chain (hop-count/cycle checks only):
/// I/O-less hop normalization/cycle detection/max-hop-count judgment that
/// both `isekai-ssh init` and `isekai-ssh`'s wrapper auto-bootstrap share.
pub fn validate_jump_chain(destination: &BootstrapTarget, jump_chain: &[JumpHost]) -> Result<(), PlanError> {
    if jump_chain.len() > MAX_JUMP_HOPS {
        return Err(PlanError::TooManyHops { got: jump_chain.len(), max: MAX_JUMP_HOPS });
    }
    check_no_repeated_host(jump_chain, destination)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("jump chain has {got} hops, exceeding the maximum of {max}")]
    TooManyHops { got: usize, max: usize },
    #[error("jump chain visits {host:?} more than once — a bootstrap chain must not loop")]
    RepeatedHost { host: String },
}

/// Two hops are "the same host" for cycle-detection purposes when their
/// (lowercased host, port) pair matches — `None` port is its own bucket
/// (an unspecified port is never known to coincide with an explicit one at
/// this pure-value layer, which has no config-file defaults to consult).
fn host_key(host: &str, port: Option<u16>) -> (String, Option<u16>) {
    (host.to_ascii_lowercase(), port)
}

fn check_no_repeated_host(jump_chain: &[JumpHost], destination: &BootstrapTarget) -> Result<(), PlanError> {
    let mut seen = std::collections::HashSet::new();
    for hop in jump_chain {
        let key = host_key(&hop.host, hop.port);
        if !seen.insert(key) {
            return Err(PlanError::RepeatedHost { host: hop.host.clone() });
        }
    }
    let dest_key = host_key(&destination.host, destination.port);
    if !seen.insert(dest_key) {
        return Err(PlanError::RepeatedHost { host: destination.host.clone() });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str) -> HostSpec {
        HostSpec::new(name)
    }

    fn jump(name: &str) -> JumpSpec {
        JumpSpec::new(name)
    }

    #[test]
    fn accepts_a_valid_multi_hop_chain() {
        assert_eq!(validate_jump_chain(&host("dest"), &[jump("bastion-a"), jump("bastion-b")]), Ok(()));
    }

    #[test]
    fn rejects_a_jump_chain_that_repeats_a_host() {
        let err = validate_jump_chain(&host("dest"), &[jump("bastion-a"), jump("bastion-a")]).unwrap_err();
        assert_eq!(err, PlanError::RepeatedHost { host: "bastion-a".to_string() });
    }

    #[test]
    fn rejects_a_jump_chain_that_loops_back_to_the_destination() {
        let err = validate_jump_chain(&host("dest"), &[jump("bastion-a"), jump("dest")]).unwrap_err();
        assert_eq!(err, PlanError::RepeatedHost { host: "dest".to_string() });
    }

    #[test]
    fn host_repetition_check_is_case_insensitive() {
        let err = validate_jump_chain(&host("Dest.Example"), &[jump("dest.example")]).unwrap_err();
        assert!(matches!(err, PlanError::RepeatedHost { .. }));
    }

    #[test]
    fn distinct_ports_on_the_same_host_are_not_a_cycle() {
        assert_eq!(validate_jump_chain(&host("dest"), &[JumpSpec::new("dest").with_port(2222)]), Ok(()));
    }

    #[test]
    fn rejects_a_chain_longer_than_the_max_hop_count() {
        let chain: Vec<JumpHost> = (0..=MAX_JUMP_HOPS).map(|i| jump(&format!("hop-{i}"))).collect();
        let err = validate_jump_chain(&host("dest"), &chain).unwrap_err();
        assert_eq!(err, PlanError::TooManyHops { got: MAX_JUMP_HOPS + 1, max: MAX_JUMP_HOPS });
    }

    #[test]
    fn accepts_exactly_the_max_hop_count() {
        let chain: Vec<JumpHost> = (0..MAX_JUMP_HOPS).map(|i| jump(&format!("hop-{i}"))).collect();
        assert_eq!(validate_jump_chain(&host("dest"), &chain), Ok(()));
    }
}
