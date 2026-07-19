//! `CandidatePool`: generation/expiry/dedup/provenance-merge management for
//! discovered candidates (`ISEKAI_PIPE_DESIGN.md`, ChatGPT second-opinion
//! consultations 2026-07-08). Sits between `CandidateProvider` (discovery)
//! and the connection entry point (selection/dialing) — this module owns
//! *which candidates currently exist and are still fresh*, nothing about
//! *how* or *when* to dial them (that's the connection entry point today,
//! and a future `RaceScheduler`).

use std::collections::HashMap;

use crate::candidate::{
    Candidate, CandidateDraft, CandidateDraftBatch, CandidateGeneration, CandidateId, CandidateKey,
    CandidateOrigin, CandidatePriority, CandidateSnapshot, CandidateValidity,
};
use tokio::time::Instant;

/// Abstracts the monotonic clock `CandidatePool` reads expiry against, so
/// tests can control time without real sleeps. Production code uses
/// [`SystemClock`]; `crate::candidate` deliberately has no equivalent (that
/// module stays free of runtime-clock concerns entirely).
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// The real clock (`tokio::time::Instant::now()`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A `replace_generation` call attempted to apply a batch from a generation
/// older than one already applied — almost certainly a late result from a
/// superseded collection round arriving after a newer one, which must be
/// discarded rather than resurrecting stale candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleGeneration {
    pub current: CandidateGeneration,
    pub attempted: CandidateGeneration,
}

struct CandidateRecord {
    candidate: Candidate,
    /// The most recent generation whose batch refreshed this entry — used
    /// only for internal bookkeeping today (v1's `replace_generation` always
    /// re-derives the whole live set from the latest batch, so nothing
    /// currently reads this after insert); kept so a future
    /// `begin_generation`/`apply_batch`/`finish_generation` API (multiple
    /// concurrent providers, `#11`/`#20`) can tell "was this refreshed by the
    /// in-progress generation, or is it a leftover from before it started"
    /// without redesigning the record shape.
    #[allow(dead_code)]
    generation: CandidateGeneration,
    observed_at: Instant,
    /// `None` for `CandidateValidity::Static` (never aged out by a TTL
    /// policy — see that variant's docs for what "Static" does and does not
    /// promise).
    expires_at: Option<Instant>,
}

/// Owns the live candidate set for one `ConnectionIntent`/auth context (a
/// pool must never be shared across two unrelated intents — candidates carry
/// no per-candidate credential scope in this v1 model, so mixing intents
/// into one pool would let a session leak into the wrong auth context).
pub struct CandidatePool<C: Clock = SystemClock> {
    clock: C,
    next_id: u64,
    current_generation: Option<CandidateGeneration>,
    entries: HashMap<CandidateKey, CandidateRecord>,
}

impl CandidatePool<SystemClock> {
    pub fn new() -> Self {
        Self::with_clock(SystemClock)
    }
}

impl Default for CandidatePool<SystemClock> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Clock> CandidatePool<C> {
    pub fn with_clock(clock: C) -> Self {
        Self { clock, next_id: 0, current_generation: None, entries: HashMap::new() }
    }

    /// Applies `batch` as the authoritative candidate set for its
    /// generation: every draft in it is inserted/merged, and any existing
    /// entry from an older generation *not* present in this batch is
    /// dropped. This "whole snapshot per generation" semantic is correct for
    /// v1 (exactly one provider, `LegacyIntentProvider`, whose batch always
    /// contains its complete output) — it will need to become
    /// `begin_generation`/`apply_batch` (one call per concurrently-running
    /// provider)/`finish_generation` (only then reconcile removals) once a
    /// second provider exists, since a single provider's partial view must
    /// not be treated as "everything else is gone" while siblings are still
    /// running.
    ///
    /// Rejects (without mutating anything) a batch whose generation is older
    /// than one already applied.
    pub fn replace_generation(
        &mut self,
        batch: CandidateDraftBatch,
    ) -> Result<CandidateSnapshot, StaleGeneration> {
        if let Some(current) = self.current_generation {
            if batch.generation < current {
                return Err(StaleGeneration { current, attempted: batch.generation });
            }
        }

        let mut refreshed_keys = std::collections::HashSet::with_capacity(batch.candidates.len());
        for draft in batch.candidates {
            let key = draft.route.key();
            refreshed_keys.insert(key.clone());
            self.insert_draft(batch.generation, draft);
        }
        self.entries.retain(|key, _| refreshed_keys.contains(key));

        self.current_generation = Some(batch.generation);
        Ok(self.eligible_candidates())
    }

    /// Merges one draft into the pool: a fresh [`CandidateKey`] gets a new
    /// [`CandidateId`]; a known key keeps its existing id and merges origins
    /// (union, deduplicated and stably ordered), priority (the better/lower
    /// rank wins), and validity (`CandidateValidity::merge`'s rule — `Static`
    /// always wins, otherwise the longer-lived lease wins). `observed_at` is
    /// always bumped to now and `expires_at` recomputed from the merged
    /// validity, since a fresh observation is itself evidence extending
    /// reusability.
    fn insert_draft(&mut self, generation: CandidateGeneration, draft: CandidateDraft) {
        let key = draft.route.key();
        let now = self.clock.now();

        if let Some(existing) = self.entries.get_mut(&key) {
            let mut origins = existing.candidate.origins.clone();
            merge_origin(&mut origins, draft.origin);
            let priority = better_priority(existing.candidate.priority, draft.priority);
            let validity = existing.candidate.validity.merge(draft.validity);

            existing.candidate.origins = origins;
            existing.candidate.priority = priority;
            existing.candidate.validity = validity;
            existing.generation = generation;
            existing.observed_at = now;
            existing.expires_at = expires_at_for(now, validity);
            return;
        }

        let id = CandidateId(self.next_id);
        self.next_id += 1;
        let validity = draft.validity;
        let candidate = Candidate {
            id,
            route: draft.route,
            origins: vec![draft.origin],
            priority: draft.priority,
            validity,
        };
        self.entries.insert(
            key,
            CandidateRecord { candidate, generation, observed_at: now, expires_at: expires_at_for(now, validity) },
        );
    }

    /// Current, non-expired candidates, newest generation. Stably ordered by
    /// [`CandidateId`] so callers (and tests) never depend on `HashMap`
    /// iteration order.
    pub fn eligible_candidates(&mut self) -> CandidateSnapshot {
        self.prune_expired();
        let mut candidates: Vec<Candidate> = self.entries.values().map(|r| r.candidate.clone()).collect();
        candidates.sort_by_key(|c| c.id);
        CandidateSnapshot { generation: self.current_generation.unwrap_or(CandidateGeneration::INITIAL), candidates }
    }

    /// Re-checks one candidate's freshness right before starting a new
    /// attempt against it — the intended call site is immediately before
    /// dialing, after whatever `start_delay` a race policy imposed, so a
    /// candidate that went stale while queued is never dialed fresh. This
    /// must **not** be used to decide whether to abort an attempt already in
    /// flight: a QUIC handshake making progress is itself stronger evidence
    /// of reachability than a discovery-result TTL, so an in-progress
    /// attempt runs to its own completion/timeout regardless of what this
    /// method would return by then.
    pub fn get_if_fresh(&mut self, id: CandidateId) -> Option<Candidate> {
        self.prune_expired();
        self.entries.values().find(|r| r.candidate.id == id).map(|r| r.candidate.clone())
    }

    pub fn prune_expired(&mut self) {
        let now = self.clock.now();
        self.entries.retain(|_, record| record.expires_at.is_none_or(|expires_at| now < expires_at));
    }
}

fn expires_at_for(observed_at: Instant, validity: CandidateValidity) -> Option<Instant> {
    match validity {
        CandidateValidity::Static => None,
        CandidateValidity::Lease { ttl } => Some(observed_at + ttl),
    }
}

/// Lower `rank` is preferred (`CandidatePriority`'s own docs) — merging two
/// observations of the same candidate keeps whichever origin claims the
/// better (lower) rank.
fn better_priority(a: CandidatePriority, b: CandidatePriority) -> CandidatePriority {
    if a.rank <= b.rank {
        a
    } else {
        b
    }
}

/// Adds `origin` to `origins` if not already present, then re-sorts so
/// iteration/telemetry order never depends on merge order (`Vec` chosen
/// over `BTreeSet`/`IndexSet` to avoid a new dependency — the invariant is
/// enforced here rather than relied upon implicitly).
fn merge_origin(origins: &mut Vec<CandidateOrigin>, origin: CandidateOrigin) {
    if !origins.contains(&origin) {
        origins.push(origin);
    }
    origins.sort();
    origins.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{CandidateOriginKind, CandidateRoute};
    use std::sync::Mutex;
    use std::time::Duration;

    struct FakeClock(Mutex<Instant>);

    impl FakeClock {
        fn new() -> Self {
            Self(Mutex::new(Instant::now()))
        }

        fn advance(&self, d: Duration) {
            let mut guard = self.0.lock().unwrap();
            *guard += d;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            *self.0.lock().unwrap()
        }
    }

    fn direct_draft(port: u16, priority_rank: u16, validity: CandidateValidity) -> CandidateDraft {
        CandidateDraft {
            route: CandidateRoute::StunP2p {
                cert_pin: crate::candidate::CertificatePinSha256::from_hex(&"ab".repeat(32)).unwrap(),
                peer_addr: format!("203.0.113.5:{port}").parse().unwrap(),
                stun_server: "203.0.113.9:3478".parse().unwrap(),
                server_name: crate::candidate::NormalizedServerName::new("isekai-helper").unwrap(),
            },
            origin: CandidateOrigin { source: CandidateOriginKind::LegacyIntent, provider_id: "legacy-intent".to_string() },
            priority: CandidatePriority { rank: priority_rank },
            validity,
        }
    }

    fn relay_draft(port: u16) -> CandidateDraft {
        CandidateDraft {
            route: CandidateRoute::Relay {
                cert_pin: crate::candidate::CertificatePinSha256::from_hex(&"ab".repeat(32)).unwrap(),
                helper_addr: format!("203.0.113.5:{port}").parse().unwrap(),
                server_name: crate::candidate::NormalizedServerName::new("isekai-helper").unwrap(),
            },
            origin: CandidateOrigin { source: CandidateOriginKind::LegacyIntent, provider_id: "legacy-intent".to_string() },
            priority: CandidatePriority { rank: 0 },
            validity: CandidateValidity::Static,
        }
    }

    #[test]
    fn replace_generation_yields_inserted_candidates() {
        let mut pool = CandidatePool::with_clock(FakeClock::new());
        let snapshot = pool
            .replace_generation(CandidateDraftBatch {
                generation: CandidateGeneration::INITIAL,
                candidates: vec![relay_draft(1)],
            })
            .unwrap();
        assert_eq!(snapshot.candidates.len(), 1);
        assert_eq!(snapshot.generation, CandidateGeneration::INITIAL);
    }

    #[test]
    fn same_key_twice_merges_instead_of_duplicating() {
        let mut pool = CandidatePool::with_clock(FakeClock::new());
        let mut draft_a = direct_draft(1, 5, CandidateValidity::Lease { ttl: Duration::from_secs(30) });
        draft_a.origin.provider_id = "provider-a".to_string();
        let mut draft_b = direct_draft(1, 2, CandidateValidity::Lease { ttl: Duration::from_secs(90) });
        draft_b.origin.provider_id = "provider-b".to_string();

        let snapshot = pool
            .replace_generation(CandidateDraftBatch { generation: CandidateGeneration::INITIAL, candidates: vec![draft_a, draft_b] })
            .unwrap();

        assert_eq!(snapshot.candidates.len(), 1, "same CandidateKey must merge into one candidate");
        let merged = &snapshot.candidates[0];
        assert_eq!(merged.origins.len(), 2, "origins should union rather than overwrite");
        assert_eq!(merged.priority.rank, 2, "lower rank (better priority) must win");
        assert_eq!(merged.validity, CandidateValidity::Lease { ttl: Duration::from_secs(90) }, "longer lease must win");
    }

    #[test]
    fn candidate_id_is_stable_across_merges() {
        let mut pool = CandidatePool::with_clock(FakeClock::new());
        let first = pool
            .replace_generation(CandidateDraftBatch { generation: CandidateGeneration::INITIAL, candidates: vec![relay_draft(1)] })
            .unwrap();
        let id_before = first.candidates[0].id;

        let second = pool
            .replace_generation(CandidateDraftBatch { generation: CandidateGeneration(1), candidates: vec![relay_draft(1)] })
            .unwrap();
        assert_eq!(second.candidates[0].id, id_before);
    }

    #[test]
    fn direct_and_relay_at_the_same_port_stay_distinct_candidates() {
        let mut pool = CandidatePool::with_clock(FakeClock::new());
        let snapshot = pool
            .replace_generation(CandidateDraftBatch {
                generation: CandidateGeneration::INITIAL,
                candidates: vec![direct_draft(1, 0, CandidateValidity::Static), relay_draft(1)],
            })
            .unwrap();
        assert_eq!(snapshot.candidates.len(), 2);
    }

    #[test]
    fn newer_generation_drops_candidates_missing_from_its_batch() {
        let mut pool = CandidatePool::with_clock(FakeClock::new());
        pool.replace_generation(CandidateDraftBatch { generation: CandidateGeneration::INITIAL, candidates: vec![relay_draft(1)] })
            .unwrap();
        let snapshot = pool
            .replace_generation(CandidateDraftBatch { generation: CandidateGeneration(1), candidates: vec![relay_draft(2)] })
            .unwrap();
        assert_eq!(snapshot.candidates.len(), 1);
        assert!(matches!(&snapshot.candidates[0].route, CandidateRoute::Relay { helper_addr, .. } if helper_addr.port() == 2));
    }

    #[test]
    fn stale_generation_is_rejected_without_mutating_the_pool() {
        let mut pool = CandidatePool::with_clock(FakeClock::new());
        pool.replace_generation(CandidateDraftBatch { generation: CandidateGeneration(5), candidates: vec![relay_draft(1)] })
            .unwrap();

        let err = pool
            .replace_generation(CandidateDraftBatch { generation: CandidateGeneration(3), candidates: vec![relay_draft(2)] })
            .unwrap_err();
        assert_eq!(err, StaleGeneration { current: CandidateGeneration(5), attempted: CandidateGeneration(3) });

        // Confirm nothing was mutated: still exactly the generation-5 candidate.
        let snapshot = pool.eligible_candidates();
        assert_eq!(snapshot.candidates.len(), 1);
        assert!(matches!(&snapshot.candidates[0].route, CandidateRoute::Relay { helper_addr, .. } if helper_addr.port() == 1));
    }

    #[test]
    fn expired_lease_candidates_are_pruned() {
        let clock = FakeClock::new();
        let mut pool = CandidatePool::with_clock(&clock);
        pool.replace_generation(CandidateDraftBatch {
            generation: CandidateGeneration::INITIAL,
            candidates: vec![direct_draft(1, 0, CandidateValidity::Lease { ttl: Duration::from_secs(1) })],
        })
        .unwrap();
        assert_eq!(pool.eligible_candidates().candidates.len(), 1);

        clock.advance(Duration::from_secs(2));
        assert_eq!(pool.eligible_candidates().candidates.len(), 0, "expired lease candidate must be pruned");
    }

    #[test]
    fn static_candidates_never_expire() {
        let clock = FakeClock::new();
        let mut pool = CandidatePool::with_clock(&clock);
        pool.replace_generation(CandidateDraftBatch { generation: CandidateGeneration::INITIAL, candidates: vec![relay_draft(1)] })
            .unwrap();
        clock.advance(Duration::from_secs(60 * 60 * 24));
        assert_eq!(pool.eligible_candidates().candidates.len(), 1);
    }

    #[test]
    fn get_if_fresh_returns_none_once_expired() {
        let clock = FakeClock::new();
        let mut pool = CandidatePool::with_clock(&clock);
        let snapshot = pool
            .replace_generation(CandidateDraftBatch {
                generation: CandidateGeneration::INITIAL,
                candidates: vec![direct_draft(1, 0, CandidateValidity::Lease { ttl: Duration::from_secs(1) })],
            })
            .unwrap();
        let id = snapshot.candidates[0].id;
        assert!(pool.get_if_fresh(id).is_some());

        clock.advance(Duration::from_secs(2));
        assert!(pool.get_if_fresh(id).is_none());
    }

    impl Clock for &FakeClock {
        fn now(&self) -> Instant {
            (**self).now()
        }
    }
}
