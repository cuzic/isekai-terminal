//! `CandidateProvider`: the async I/O boundary that discovers candidates
//! (`ISEKAI_PIPE_DESIGN.md`, ChatGPT second-opinion consultations
//! 2026-07-08). Pure candidate value types live in
//! `isekai_pipe_core::candidate` — this module only adds the runtime trait
//! and its first (legacy) implementation.
//!
//! v1 is deliberately a one-shot async batch fetch, not a stream:
//!
//! ```text
//! async fn gather(&self, ctx: &GatherContext<'_>) -> Result<CandidateDraftBatch, _>;
//! ```
//!
//! A Trickle-ICE-style incremental/streaming provider would need its own
//! answers for collection-completion signaling, partial-provider-failure
//! handling, late results from a superseded generation, cancellation, and
//! backpressure — real complexity that today's candidate count (one direct
//! candidate, one relay candidate, at most a handful more) doesn't justify.
//! If a future provider genuinely needs to stream (e.g. a relay control
//! plane that pushes new endpoints mid-session), add a separate
//! `TrickleCandidateProvider` trait rather than reshaping this one.

use isekai_pipe_core::{
    CandidateConversionError, CandidateDraft, CandidateDraftBatch, CandidateGeneration, CandidateOrigin,
    CandidateOriginKind, CandidatePriority, CandidateRoute, CandidateValidity, ConnectionIntent, IntentTransport,
};
use tokio::time::Instant;

/// Everything a `CandidateProvider::gather` call needs to know about the
/// collection round it's participating in.
pub struct GatherContext<'a> {
    pub generation: CandidateGeneration,
    /// Soft deadline for this gather call — unused by `LegacyIntentProvider`
    /// (pure, synchronous-fast conversion) but part of the shape every
    /// provider receives, since real network-querying providers (STUN, a
    /// relay control plane) need a budget to bound how long they'll wait
    /// before returning whatever they've found so far.
    pub deadline: Instant,
    pub intent: &'a ConnectionIntent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateProviderError {
    /// `IntentTransport` → `CandidateDraft` conversion failed (malformed
    /// `ConnectionIntent` data — see `CandidateConversionError`'s variants).
    Conversion(CandidateConversionError),
    /// An entry in `ConnectionIntent::relay_endpoints` (`ConfigRelayProvider`)
    /// could not be turned into a relay candidate — either it did not parse as
    /// a `SocketAddr`, or the shared cert pin / server name was itself
    /// malformed. `entry` names the offending endpoint string so a
    /// misconfiguration points at the exact input.
    InvalidRelayEndpoint { entry: String, reason: String },
    /// An entry in `ConnectionIntent::stun_servers` (`ConfigStunProvider`,
    /// `#11`) could not be turned into a STUN-P2P candidate — either it did
    /// not parse as a `SocketAddr`, or the shared cert pin / peer address /
    /// server name (from `IntentTransport::StunP2p`) was itself malformed.
    InvalidStunServer { entry: String, reason: String },
    /// `ConnectionIntent::stun_servers` was non-empty, but `intent.transport`
    /// was not `IntentTransport::StunP2p` — there is no `peer_addr`/
    /// `server_name` to pair the configured STUN servers with. Today's only
    /// producer of `stun_servers` (`isekai-ssh/src/wrapper.rs`'s `#@isekai
    /// stun` directive) always builds a `Relay` transport, so this is
    /// expected to fire whenever that combination reaches this provider —
    /// surfaced as a distinct, named error rather than silently producing no
    /// candidates, so a caller that *does* expect STUN P2P candidates here
    /// notices the misconfiguration instead of quietly falling through to
    /// relay-only.
    StunServersWithoutStunP2pTransport,
}

impl std::fmt::Display for CandidateProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conversion(e) => write!(f, "candidate conversion failed: {e}"),
            Self::InvalidRelayEndpoint { entry, reason } => {
                write!(f, "invalid relay endpoint {entry:?}: {reason}")
            }
            Self::InvalidStunServer { entry, reason } => {
                write!(f, "invalid stun server {entry:?}: {reason}")
            }
            Self::StunServersWithoutStunP2pTransport => {
                write!(f, "ConnectionIntent.stun_servers is non-empty but intent.transport is not StunP2p")
            }
        }
    }
}

impl std::error::Error for CandidateProviderError {}

impl From<CandidateConversionError> for CandidateProviderError {
    fn from(value: CandidateConversionError) -> Self {
        Self::Conversion(value)
    }
}

/// Discovers candidates for one collection round. Implementations must
/// return every candidate they found tagged with `ctx.generation` — the
/// pool, not the provider, decides what to do with a stale-generation
/// result.
#[async_trait::async_trait]
pub trait CandidateProvider: Send + Sync {
    async fn gather(&self, ctx: &GatherContext<'_>) -> Result<CandidateDraftBatch, CandidateProviderError>;
}

/// Converts the legacy single-transport `ConnectionIntent` into exactly one
/// `CandidateDraft` (`isekai_pipe_core::candidate`'s `TryFrom` impl does the
/// actual pure conversion; this type just exposes it through the async
/// `CandidateProvider` trait boundary). Does not change any connection
/// behavior by itself — it exists so the rest of the candidate pipeline
/// (`CandidatePool`, the connection entry point) can be built and tested
/// against today's one-candidate reality before any real multi-candidate
/// provider exists.
pub struct LegacyIntentProvider;

#[async_trait::async_trait]
impl CandidateProvider for LegacyIntentProvider {
    async fn gather(&self, ctx: &GatherContext<'_>) -> Result<CandidateDraftBatch, CandidateProviderError> {
        let draft = CandidateDraft::try_from(ctx.intent)?;
        Ok(CandidateDraftBatch { generation: ctx.generation, candidates: vec![draft] })
    }
}

/// `provider_id` `ConfigRelayProvider` stamps onto every [`CandidateOrigin`]
/// it produces. There is one instance of this provider kind, so a fixed
/// literal is enough — the individual endpoint is distinguished by its
/// `helper_addr`/priority rank, not by `provider_id`.
pub const CONFIG_RELAY_PROVIDER_ID: &str = "config-relay";

/// Expands `ConnectionIntent::relay_endpoints` — a list of alternate
/// already-resolved relay-assigned addresses for the *same* isekai-helper
/// instance (same cert pin, session secret, and server name) — into one relay
/// [`CandidateDraft`] per entry, for relay-endpoint fallback (`#12`).
///
/// Vec order is preference order: entry index becomes the
/// [`CandidatePriority`] rank (index 0 = rank 0 = most preferred), so a caller
/// can list its primary relay address first and its fallbacks after. Every
/// entry shares `ConnectionIntent::expected_server_identity.cert_sha256_hex`
/// as its cert pin and the literal `"isekai-helper"` server name, matching the
/// hardcoded relay-mode convention used elsewhere (`isekai-ssh/src/wrapper.rs`,
/// `isekai-pipe`'s `intent_from_profile`).
///
/// An empty `relay_endpoints` is a valid (if useless) input and yields an
/// empty batch, not an error — deciding whether to use this provider or
/// [`LegacyIntentProvider`] based on whether `relay_endpoints` is populated is
/// the caller's job, not this one's.
pub struct ConfigRelayProvider;

#[async_trait::async_trait]
impl CandidateProvider for ConfigRelayProvider {
    async fn gather(&self, ctx: &GatherContext<'_>) -> Result<CandidateDraftBatch, CandidateProviderError> {
        let intent = ctx.intent;
        let mut candidates = Vec::with_capacity(intent.relay_endpoints.len());

        for (index, entry) in intent.relay_endpoints.iter().enumerate() {
            let (cert_pin, server_name) = isekai_pipe_core::validate_endpoint_identity(
                &intent.expected_server_identity.cert_sha256_hex,
                "isekai-helper",
            )
            .map_err(|e| CandidateProviderError::InvalidRelayEndpoint { entry: entry.clone(), reason: e.to_string() })?;
            let helper_addr = entry.parse().map_err(|_| CandidateProviderError::InvalidRelayEndpoint {
                entry: entry.clone(),
                reason: "not a valid socket address".to_string(),
            })?;

            candidates.push(CandidateDraft {
                route: CandidateRoute::Relay { cert_pin, helper_addr, server_name },
                origin: CandidateOrigin {
                    source: CandidateOriginKind::ConfigRelay,
                    provider_id: CONFIG_RELAY_PROVIDER_ID.to_string(),
                },
                priority: CandidatePriority { rank: index as u16 },
                validity: CandidateValidity::Static,
            });
        }

        Ok(CandidateDraftBatch { generation: ctx.generation, candidates })
    }
}

/// `provider_id` `ConfigStunProvider` stamps onto every [`CandidateOrigin`]
/// it produces. There is one instance of this provider kind, so a fixed
/// literal is enough — the individual STUN server is distinguished by its
/// `stun_server` field / priority rank, not by `provider_id`.
pub const CONFIG_STUN_PROVIDER_ID: &str = "config-stun";

/// Expands `ConnectionIntent::stun_servers` — a list of alternate STUN
/// servers to use when learning this side's own observed address before
/// hole-punching toward the *same* peer (`#11`) — into one STUN-P2P
/// [`CandidateDraft`] per entry.
///
/// `peer_addr`/`server_name` come from `intent.transport`
/// (`IntentTransport::StunP2p`'s own fields, shared by every candidate this
/// provider produces — only `stun_server` varies per entry, matching
/// `CandidateRoute::StunP2p`'s dedup-identity docs). If `intent.transport` is
/// not `StunP2p` while `stun_servers` is non-empty, there is no `peer_addr`
/// to pair the configured STUN servers with —
/// [`CandidateProviderError::StunServersWithoutStunP2pTransport`] is
/// returned rather than silently producing no candidates (module docs on
/// that variant explain when this legitimately fires today).
///
/// Vec order is preference order: entry index becomes the
/// [`CandidatePriority`] rank (index 0 = rank 0 = most preferred).
///
/// An empty `stun_servers` is a valid (if useless) input and yields an empty
/// batch, not an error — deciding whether to use this provider or
/// [`LegacyIntentProvider`] based on whether `stun_servers` is populated is
/// the caller's job, not this one's (mirrors [`ConfigRelayProvider`]'s same
/// convention).
pub struct ConfigStunProvider;

#[async_trait::async_trait]
impl CandidateProvider for ConfigStunProvider {
    async fn gather(&self, ctx: &GatherContext<'_>) -> Result<CandidateDraftBatch, CandidateProviderError> {
        let intent = ctx.intent;
        if intent.stun_servers.is_empty() {
            return Ok(CandidateDraftBatch { generation: ctx.generation, candidates: Vec::new() });
        }

        let IntentTransport::StunP2p { peer_addr, server_name, .. } = &intent.transport else {
            return Err(CandidateProviderError::StunServersWithoutStunP2pTransport);
        };

        let mut candidates = Vec::with_capacity(intent.stun_servers.len());
        for (index, entry) in intent.stun_servers.iter().enumerate() {
            let (cert_pin, server_name) =
                isekai_pipe_core::validate_endpoint_identity(&intent.expected_server_identity.cert_sha256_hex, server_name)
                    .map_err(|e| CandidateProviderError::InvalidStunServer { entry: entry.clone(), reason: e.to_string() })?;
            let stun_server = entry.parse().map_err(|_| CandidateProviderError::InvalidStunServer {
                entry: entry.clone(),
                reason: "not a valid socket address".to_string(),
            })?;
            let peer_addr = peer_addr.parse().map_err(|_| CandidateProviderError::InvalidStunServer {
                entry: entry.clone(),
                reason: "intent.transport's peer_addr is not a valid socket address".to_string(),
            })?;

            candidates.push(CandidateDraft {
                route: CandidateRoute::StunP2p { cert_pin, peer_addr, stun_server, server_name },
                origin: CandidateOrigin {
                    source: CandidateOriginKind::ConfigStun,
                    provider_id: CONFIG_STUN_PROVIDER_ID.to_string(),
                },
                priority: CandidatePriority { rank: index as u16 },
                validity: CandidateValidity::Static,
            });
        }

        Ok(CandidateDraftBatch { generation: ctx.generation, candidates })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isekai_pipe_core::{BootstrapProvenance, IntentTransport, ServerIdentity};

    fn sample_intent() -> ConnectionIntent {
        ConnectionIntent::new(
            "production",
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::Relay {
                helper_addr: "203.0.113.5:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "production:22".to_string() },
        )
    }

    #[tokio::test]
    async fn legacy_intent_provider_yields_exactly_one_candidate_tagged_with_the_requested_generation() {
        let intent = sample_intent();
        let ctx = GatherContext { generation: CandidateGeneration(7), deadline: Instant::now(), intent: &intent };

        let batch = LegacyIntentProvider.gather(&ctx).await.unwrap();

        assert_eq!(batch.generation, CandidateGeneration(7));
        assert_eq!(batch.candidates.len(), 1);
    }

    #[tokio::test]
    async fn legacy_intent_provider_surfaces_conversion_errors() {
        let mut intent = sample_intent();
        intent.expected_server_identity.cert_sha256_hex = "not-hex".to_string();
        let ctx = GatherContext { generation: CandidateGeneration::INITIAL, deadline: Instant::now(), intent: &intent };

        let err = LegacyIntentProvider.gather(&ctx).await.unwrap_err();
        assert!(matches!(err, CandidateProviderError::Conversion(_)));
    }

    fn relay_route(batch: &CandidateDraftBatch, index: usize) -> &CandidateRoute {
        &batch.candidates[index].route
    }

    #[tokio::test]
    async fn config_relay_provider_yields_one_candidate_per_endpoint_in_priority_order() {
        let mut intent = sample_intent();
        intent.relay_endpoints = vec![
            "203.0.113.10:45231".to_string(),
            "198.51.100.7:45231".to_string(),
            "192.0.2.3:45231".to_string(),
        ];
        let ctx = GatherContext { generation: CandidateGeneration(3), deadline: Instant::now(), intent: &intent };

        let batch = ConfigRelayProvider.gather(&ctx).await.unwrap();

        assert_eq!(batch.candidates.len(), 3);
        for (index, expected_addr) in ["203.0.113.10:45231", "198.51.100.7:45231", "192.0.2.3:45231"].iter().enumerate() {
            assert_eq!(batch.candidates[index].priority, CandidatePriority { rank: index as u16 });
            match relay_route(&batch, index) {
                CandidateRoute::Relay { helper_addr, server_name, .. } => {
                    assert_eq!(helper_addr, &expected_addr.parse().unwrap());
                    assert_eq!(server_name.as_str(), "isekai-helper");
                }
                other => panic!("expected relay route, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn config_relay_provider_empty_endpoints_yields_empty_batch_not_error() {
        let intent = sample_intent();
        assert!(intent.relay_endpoints.is_empty());
        let ctx = GatherContext { generation: CandidateGeneration(5), deadline: Instant::now(), intent: &intent };

        let batch = ConfigRelayProvider.gather(&ctx).await.unwrap();

        assert_eq!(batch.generation, CandidateGeneration(5));
        assert!(batch.candidates.is_empty());
    }

    #[tokio::test]
    async fn config_relay_provider_rejects_malformed_endpoint_naming_the_offender() {
        let mut intent = sample_intent();
        intent.relay_endpoints = vec!["203.0.113.10:45231".to_string(), "not-an-address".to_string()];
        let ctx = GatherContext { generation: CandidateGeneration::INITIAL, deadline: Instant::now(), intent: &intent };

        let err = ConfigRelayProvider.gather(&ctx).await.unwrap_err();

        match &err {
            CandidateProviderError::InvalidRelayEndpoint { entry, .. } => assert_eq!(entry, "not-an-address"),
            other => panic!("expected InvalidRelayEndpoint, got {other:?}"),
        }
        assert!(err.to_string().contains("not-an-address"), "error message must name the offending entry: {err}");
    }

    #[tokio::test]
    async fn config_relay_provider_output_generation_matches_context() {
        let mut intent = sample_intent();
        intent.relay_endpoints = vec!["203.0.113.10:45231".to_string()];
        let ctx = GatherContext { generation: CandidateGeneration(42), deadline: Instant::now(), intent: &intent };

        let batch = ConfigRelayProvider.gather(&ctx).await.unwrap();

        assert_eq!(batch.generation, CandidateGeneration(42));
    }

    #[tokio::test]
    async fn config_relay_provider_stamps_config_relay_origin() {
        let mut intent = sample_intent();
        intent.relay_endpoints = vec!["203.0.113.10:45231".to_string(), "198.51.100.7:45231".to_string()];
        let ctx = GatherContext { generation: CandidateGeneration::INITIAL, deadline: Instant::now(), intent: &intent };

        let batch = ConfigRelayProvider.gather(&ctx).await.unwrap();

        for candidate in &batch.candidates {
            assert_eq!(candidate.origin.source, CandidateOriginKind::ConfigRelay);
            assert_eq!(candidate.origin.provider_id, CONFIG_RELAY_PROVIDER_ID);
            assert_eq!(candidate.origin.provider_id, "config-relay");
        }
    }

    fn sample_stun_intent() -> ConnectionIntent {
        ConnectionIntent::new(
            "production",
            "ssh",
            ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            IntentTransport::StunP2p {
                stun_server: "192.0.2.1:3478".to_string(),
                peer_addr: "203.0.113.5:45231".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore { key: "production:22".to_string() },
        )
    }

    fn stun_route(batch: &CandidateDraftBatch, index: usize) -> &CandidateRoute {
        &batch.candidates[index].route
    }

    #[tokio::test]
    async fn config_stun_provider_yields_one_candidate_per_stun_server_in_priority_order() {
        let mut intent = sample_stun_intent();
        intent.stun_servers = vec![
            "192.0.2.10:3478".to_string(),
            "192.0.2.11:3478".to_string(),
            "192.0.2.12:3478".to_string(),
        ];
        let ctx = GatherContext { generation: CandidateGeneration(3), deadline: Instant::now(), intent: &intent };

        let batch = ConfigStunProvider.gather(&ctx).await.unwrap();

        assert_eq!(batch.candidates.len(), 3);
        for (index, expected_stun) in ["192.0.2.10:3478", "192.0.2.11:3478", "192.0.2.12:3478"].iter().enumerate() {
            assert_eq!(batch.candidates[index].priority, CandidatePriority { rank: index as u16 });
            match stun_route(&batch, index) {
                CandidateRoute::StunP2p { stun_server, peer_addr, server_name, .. } => {
                    assert_eq!(stun_server, &expected_stun.parse().unwrap());
                    assert_eq!(peer_addr, &"203.0.113.5:45231".parse().unwrap());
                    assert_eq!(server_name.as_str(), "isekai-helper");
                }
                other => panic!("expected stun-p2p route, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn config_stun_provider_empty_stun_servers_yields_empty_batch_not_error() {
        let intent = sample_stun_intent();
        assert!(intent.stun_servers.is_empty());
        let ctx = GatherContext { generation: CandidateGeneration(5), deadline: Instant::now(), intent: &intent };

        let batch = ConfigStunProvider.gather(&ctx).await.unwrap();

        assert_eq!(batch.generation, CandidateGeneration(5));
        assert!(batch.candidates.is_empty());
    }

    #[tokio::test]
    async fn config_stun_provider_rejects_malformed_stun_server_naming_the_offender() {
        let mut intent = sample_stun_intent();
        intent.stun_servers = vec!["192.0.2.10:3478".to_string(), "not-an-address".to_string()];
        let ctx = GatherContext { generation: CandidateGeneration::INITIAL, deadline: Instant::now(), intent: &intent };

        let err = ConfigStunProvider.gather(&ctx).await.unwrap_err();

        match &err {
            CandidateProviderError::InvalidStunServer { entry, .. } => assert_eq!(entry, "not-an-address"),
            other => panic!("expected InvalidStunServer, got {other:?}"),
        }
        assert!(err.to_string().contains("not-an-address"), "error message must name the offending entry: {err}");
    }

    #[tokio::test]
    async fn config_stun_provider_rejects_stun_servers_without_stun_p2p_transport() {
        let mut intent = sample_intent(); // transport: Relay
        intent.stun_servers = vec!["192.0.2.10:3478".to_string()];
        let ctx = GatherContext { generation: CandidateGeneration::INITIAL, deadline: Instant::now(), intent: &intent };

        let err = ConfigStunProvider.gather(&ctx).await.unwrap_err();
        assert!(matches!(err, CandidateProviderError::StunServersWithoutStunP2pTransport));
    }

    #[tokio::test]
    async fn config_stun_provider_output_generation_matches_context() {
        let mut intent = sample_stun_intent();
        intent.stun_servers = vec!["192.0.2.10:3478".to_string()];
        let ctx = GatherContext { generation: CandidateGeneration(42), deadline: Instant::now(), intent: &intent };

        let batch = ConfigStunProvider.gather(&ctx).await.unwrap();

        assert_eq!(batch.generation, CandidateGeneration(42));
    }

    #[tokio::test]
    async fn config_stun_provider_stamps_config_stun_origin() {
        let mut intent = sample_stun_intent();
        intent.stun_servers = vec!["192.0.2.10:3478".to_string(), "192.0.2.11:3478".to_string()];
        let ctx = GatherContext { generation: CandidateGeneration::INITIAL, deadline: Instant::now(), intent: &intent };

        let batch = ConfigStunProvider.gather(&ctx).await.unwrap();

        for candidate in &batch.candidates {
            assert_eq!(candidate.origin.source, CandidateOriginKind::ConfigStun);
            assert_eq!(candidate.origin.provider_id, CONFIG_STUN_PROVIDER_ID);
            assert_eq!(candidate.origin.provider_id, "config-stun");
        }
    }
}
