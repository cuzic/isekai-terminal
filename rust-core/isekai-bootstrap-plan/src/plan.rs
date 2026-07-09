//! [`BootstrapPlan`]: the pure value a caller builds once (from CLI args,
//! `#@isekai` config, or a saved profile) and hands to a route executor.
//! Reuses `isekai-bootstrap`'s per-hop value types ([`HostSpec`]/[`JumpSpec`])
//! rather than redefining them — this crate's contribution is the *chain*
//! (validated multi-hop ordering, cycle/hop-count checks) and the policy
//! fields (`route_policy`/`credential_source`/`persistence_policy`) that
//! `isekai-bootstrap` has no reason to know about.

use isekai_bootstrap::{HostSpec, JumpSpec};

/// The final SSH bootstrap destination — where `isekai-pipe serve` gets
/// installed and started. Distinct name from [`HostSpec`] at this crate's
/// API boundary even though the shape is identical today, so a future
/// destination-only field (e.g. a service-target override) doesn't force a
/// matching change onto every [`JumpHost`] in the chain.
pub type BootstrapTarget = HostSpec;

/// One hop in [`BootstrapPlan::jump_chain`], reusing `isekai-bootstrap`'s
/// existing `-J`/`ProxyJump` value type.
pub type JumpHost = JumpSpec;

/// A candidate connection route family a [`BootstrapPlan`] may attempt, in
/// the order [`RoutePolicy::allowed`] lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteKind {
    /// `direct-by-bootstrap-host`: dial the same host the SSH bootstrap
    /// itself used. No STUN/relay infrastructure required.
    Direct,
    /// STUN-observed address + hole-punch (Epic G, not yet implemented by
    /// any executor — a plan may still declare intent to use it).
    Stun,
    /// Relay-mediated (Epic H, not yet implemented by any executor).
    Relay,
}

/// Which route families a plan may attempt, and in what preference order.
/// `allowed[0]` is tried first; an executor falls back to the next entry
/// only after the previous one's [`crate::BootstrapBudget`] phase is
/// exhausted or the route is definitively unreachable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePolicy {
    allowed: Vec<RouteKind>,
}

impl RoutePolicy {
    /// Builds a policy from an explicit preference order. Rejects an empty
    /// list and a list containing the same [`RouteKind`] twice — both would
    /// silently make some later validation (e.g. "does this plan ever try
    /// relay") ambiguous.
    pub fn new(allowed: Vec<RouteKind>) -> Result<Self, PlanError> {
        if allowed.is_empty() {
            return Err(PlanError::EmptyRoutePolicy);
        }
        for (i, kind) in allowed.iter().enumerate() {
            if allowed[..i].contains(kind) {
                return Err(PlanError::DuplicateRouteKind(*kind));
            }
        }
        Ok(Self { allowed })
    }

    /// Today's only implemented route: `direct-by-bootstrap-host`, matching
    /// the wrapper's current single-hop auto-bootstrap behavior.
    pub fn direct_only() -> Self {
        Self { allowed: vec![RouteKind::Direct] }
    }

    pub fn allowed(&self) -> &[RouteKind] {
        &self.allowed
    }

    pub fn allows(&self, kind: RouteKind) -> bool {
        self.allowed.contains(&kind)
    }
}

/// Where the credentials needed to actually run this plan's hops (SSH auth
/// for the jump chain, relay JWT for a `Relay`-route attempt) come from.
/// Deliberately minimal today — Epic F (login/token provider) owns the real
/// token-sourcing design; this only distinguishes "usable with what
/// `ssh(1)` already resolves on its own" from "needs a relay token this
/// plan cannot obtain by itself".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// `ssh(1)`'s own agent/identity-file/config resolution — sufficient for
    /// every hop in the chain and for a `Direct`-route attempt.
    SshDefault,
    /// A relay-scoped credential (short-lived JWT) is required and must be
    /// supplied by the caller before a `Relay`-route attempt in this plan
    /// can run. No executor can source one on its own until Epic F lands.
    RelayToken,
}

/// Whether a successful route attempt's resulting candidate(s) should be
/// written to persistent storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistencePolicy {
    /// Persist only candidates that were actually confirmed reachable.
    /// Never persist a candidate this plan merely attempted or discovered
    /// but never validated — required so a cancelled/timed-out run leaves
    /// no unverified state behind (Epic A cancellation contract).
    PersistOnSuccess,
    /// Never write to persistent storage regardless of outcome (e.g.
    /// `isekai-pipe probe`/`inspect`, which must not mutate state as a
    /// side effect of a diagnostic run).
    Ephemeral,
}

/// Hop chains longer than this are rejected outright rather than attempted.
/// Chosen generously above any topology this project expects to support
/// (`ISEKAI_PIPE_DESIGN.md` §8 Epic K only asks for "multi-hop", not a
/// specific bound) while still catching a malformed/looping config before
/// it reaches `ssh(1)`.
pub const MAX_JUMP_HOPS: usize = 8;

/// A validated, I/O-less bootstrap plan: *what* to do, not *how* to do it.
/// Route executors (Epic G/H/I/K) consume a `BootstrapPlan`; this crate
/// never runs `ssh(1)` or dials QUIC itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapPlan {
    pub destination: BootstrapTarget,
    pub jump_chain: Vec<JumpHost>,
    pub route_policy: RoutePolicy,
    pub credential_source: CredentialSource,
    pub persistence_policy: PersistencePolicy,
}

impl BootstrapPlan {
    /// Validates and builds a plan. Rejects chains that are too long
    /// ([`MAX_JUMP_HOPS`]) or that visit the same host twice (a jump chain
    /// that loops back on itself, or that jumps through the destination
    /// before reaching it, can never actually complete).
    pub fn new(
        destination: BootstrapTarget,
        jump_chain: Vec<JumpHost>,
        route_policy: RoutePolicy,
        credential_source: CredentialSource,
        persistence_policy: PersistencePolicy,
    ) -> Result<Self, PlanError> {
        Self::validate_jump_chain(&destination, &jump_chain)?;
        Ok(Self { destination, jump_chain, route_policy, credential_source, persistence_policy })
    }

    /// Validates a `--via` jump-host chain on its own (hop-count/cycle
    /// checks only — no route policy, credential source, or persistence
    /// policy needed), per `ISEKAI_PIPE_DESIGN.md` §8 Epic K's planner
    /// (`2-a`): I/O-less hop normalization/cycle detection/max-hop-count
    /// judgment that both `isekai-ssh init` and `isekai-ssh`'s wrapper
    /// auto-bootstrap (`ISEKAI_PIPE_DESIGN.md`'s "unsupported構成判定")
    /// share, instead of each hand-rolling its own chain validation or
    /// inventing placeholder `RoutePolicy`/`CredentialSource` values just to
    /// call [`Self::new`]. [`Self::new`] itself is defined in terms of this.
    pub fn validate_jump_chain(destination: &BootstrapTarget, jump_chain: &[JumpHost]) -> Result<(), PlanError> {
        if jump_chain.len() > MAX_JUMP_HOPS {
            return Err(PlanError::TooManyHops { got: jump_chain.len(), max: MAX_JUMP_HOPS });
        }
        check_no_repeated_host(jump_chain, destination)
    }

    /// Today's default shape: no jump hops, `direct-by-bootstrap-host`
    /// only, `ssh(1)`'s own credential resolution, persist on success.
    /// Matches the wrapper's current auto-bootstrap behavior exactly
    /// (`wrapper.rs`'s single-hop, `LaunchSpec::Direct`-only path) — kept
    /// here so Epic I's wrapper integration has a one-line migration path
    /// rather than needing to hand-assemble every field.
    pub fn single_hop_direct(destination: BootstrapTarget) -> Self {
        Self {
            destination,
            jump_chain: Vec::new(),
            route_policy: RoutePolicy::direct_only(),
            credential_source: CredentialSource::SshDefault,
            persistence_policy: PersistencePolicy::PersistOnSuccess,
        }
    }

    /// Number of hops before reaching [`Self::destination`] (`0` for a
    /// direct connection, matching [`MAX_JUMP_HOPS`]'s unit).
    pub fn hop_count(&self) -> usize {
        self.jump_chain.len()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("jump chain has {got} hops, exceeding the maximum of {max}")]
    TooManyHops { got: usize, max: usize },
    #[error("jump chain visits {host:?} more than once — a bootstrap chain must not loop")]
    RepeatedHost { host: String },
    #[error("route policy must allow at least one route kind")]
    EmptyRoutePolicy,
    #[error("route policy lists {0:?} more than once")]
    DuplicateRouteKind(RouteKind),
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
    fn single_hop_direct_matches_todays_wrapper_default() {
        let plan = BootstrapPlan::single_hop_direct(host("prod.example"));
        assert_eq!(plan.hop_count(), 0);
        assert_eq!(plan.route_policy, RoutePolicy::direct_only());
        assert_eq!(plan.credential_source, CredentialSource::SshDefault);
        assert_eq!(plan.persistence_policy, PersistencePolicy::PersistOnSuccess);
    }

    #[test]
    fn accepts_a_valid_multi_hop_chain() {
        let plan = BootstrapPlan::new(
            host("dest"),
            vec![jump("bastion-a"), jump("bastion-b")],
            RoutePolicy::direct_only(),
            CredentialSource::SshDefault,
            PersistencePolicy::PersistOnSuccess,
        )
        .unwrap();
        assert_eq!(plan.hop_count(), 2);
    }

    #[test]
    fn rejects_a_jump_chain_that_repeats_a_host() {
        let err = BootstrapPlan::new(
            host("dest"),
            vec![jump("bastion-a"), jump("bastion-a")],
            RoutePolicy::direct_only(),
            CredentialSource::SshDefault,
            PersistencePolicy::PersistOnSuccess,
        )
        .unwrap_err();
        assert_eq!(err, PlanError::RepeatedHost { host: "bastion-a".to_string() });
    }

    #[test]
    fn rejects_a_jump_chain_that_loops_back_to_the_destination() {
        let err = BootstrapPlan::new(
            host("dest"),
            vec![jump("bastion-a"), jump("dest")],
            RoutePolicy::direct_only(),
            CredentialSource::SshDefault,
            PersistencePolicy::PersistOnSuccess,
        )
        .unwrap_err();
        assert_eq!(err, PlanError::RepeatedHost { host: "dest".to_string() });
    }

    #[test]
    fn host_repetition_check_is_case_insensitive() {
        let err = BootstrapPlan::new(
            host("Dest.Example"),
            vec![jump("dest.example")],
            RoutePolicy::direct_only(),
            CredentialSource::SshDefault,
            PersistencePolicy::PersistOnSuccess,
        )
        .unwrap_err();
        assert!(matches!(err, PlanError::RepeatedHost { .. }));
    }

    #[test]
    fn distinct_ports_on_the_same_host_are_not_a_cycle() {
        let plan = BootstrapPlan::new(
            host("dest"),
            vec![JumpSpec::new("dest").with_port(2222)],
            RoutePolicy::direct_only(),
            CredentialSource::SshDefault,
            PersistencePolicy::PersistOnSuccess,
        )
        .unwrap();
        assert_eq!(plan.hop_count(), 1);
    }

    #[test]
    fn rejects_a_chain_longer_than_the_max_hop_count() {
        let chain: Vec<JumpHost> = (0..=MAX_JUMP_HOPS).map(|i| jump(&format!("hop-{i}"))).collect();
        let err = BootstrapPlan::new(
            host("dest"),
            chain,
            RoutePolicy::direct_only(),
            CredentialSource::SshDefault,
            PersistencePolicy::PersistOnSuccess,
        )
        .unwrap_err();
        assert_eq!(err, PlanError::TooManyHops { got: MAX_JUMP_HOPS + 1, max: MAX_JUMP_HOPS });
    }

    #[test]
    fn accepts_exactly_the_max_hop_count() {
        let chain: Vec<JumpHost> = (0..MAX_JUMP_HOPS).map(|i| jump(&format!("hop-{i}"))).collect();
        let plan = BootstrapPlan::new(
            host("dest"),
            chain,
            RoutePolicy::direct_only(),
            CredentialSource::SshDefault,
            PersistencePolicy::PersistOnSuccess,
        )
        .unwrap();
        assert_eq!(plan.hop_count(), MAX_JUMP_HOPS);
    }

    #[test]
    fn validate_jump_chain_matches_new_for_a_valid_chain() {
        assert_eq!(BootstrapPlan::validate_jump_chain(&host("dest"), &[jump("bastion-a"), jump("bastion-b")]), Ok(()));
    }

    #[test]
    fn validate_jump_chain_matches_new_for_a_repeated_host() {
        assert_eq!(
            BootstrapPlan::validate_jump_chain(&host("dest"), &[jump("bastion-a"), jump("bastion-a")]),
            Err(PlanError::RepeatedHost { host: "bastion-a".to_string() })
        );
    }

    #[test]
    fn validate_jump_chain_matches_new_for_a_too_long_chain() {
        let chain: Vec<JumpHost> = (0..=MAX_JUMP_HOPS).map(|i| jump(&format!("hop-{i}"))).collect();
        assert_eq!(
            BootstrapPlan::validate_jump_chain(&host("dest"), &chain),
            Err(PlanError::TooManyHops { got: MAX_JUMP_HOPS + 1, max: MAX_JUMP_HOPS })
        );
    }

    #[test]
    fn route_policy_rejects_empty_list() {
        assert_eq!(RoutePolicy::new(vec![]).unwrap_err(), PlanError::EmptyRoutePolicy);
    }

    #[test]
    fn route_policy_rejects_duplicate_route_kind() {
        let err = RoutePolicy::new(vec![RouteKind::Direct, RouteKind::Stun, RouteKind::Direct]).unwrap_err();
        assert_eq!(err, PlanError::DuplicateRouteKind(RouteKind::Direct));
    }

    #[test]
    fn route_policy_preserves_preference_order() {
        let policy = RoutePolicy::new(vec![RouteKind::Relay, RouteKind::Direct]).unwrap();
        assert_eq!(policy.allowed(), &[RouteKind::Relay, RouteKind::Direct]);
        assert!(policy.allows(RouteKind::Relay));
        assert!(!policy.allows(RouteKind::Stun));
    }
}
