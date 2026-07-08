//! Pure value types for the Candidate model (`ISEKAI_PIPE_DESIGN.md`,
//! ChatGPT second-opinion consultations 2026-07-08). This module only
//! defines *what a candidate is* — nothing here touches I/O, `tokio`, or
//! `noq`. The runtime pieces (`CandidateProvider`, `CandidatePool`,
//! connection establishment) live in `isekai-transport`, which depends on
//! this crate rather than the other way around.
//!
//! # Design summary
//!
//! A `Candidate` is **an executable connection recipe, not a physical
//! route**: today's `StunP2p` route creates a fresh throwaway UDP socket per
//! attempt, so there is no persistent "local base" identity to model the way
//! ICE does. Two `Candidate`s are the "same" (dedup-equal, via
//! [`CandidateKey`]) only when they'd produce the same recipe — critically,
//! a direct route and a relay route to the same resolved address are
//! *different* candidates (different failure domains, different auth,
//! different latency/cost), and two STUN-P2P attempts against the same peer
//! but different STUN servers are also different recipes in this v1 model
//! (see `CandidateRoute::StunP2p`'s docs for why, and where a future
//! `RouteGroupKey` would live if that ever needs to change).
//!
//! Fencing/attach-safety fields (`attempt_id`, `fencing_token`,
//! `connection_generation`, `session_id`) deliberately do **not** live here.
//! A `Candidate` describes a route that *can* be tried; a `ConnectionAttempt`
//! (later task) describes one *actual* try of it — the same candidate can be
//! attempted multiple times (initial connect, race retry, resume,
//! post-network-switch reconnect), so folding a one-shot attempt id into the
//! reusable candidate would make it non-reusable.
//!
//! Likewise, session-level secrets (`session_secret` et al.) never appear on
//! `Candidate`/`CandidateKey` — the connection-establishment layer threads
//! those through separately (`ConnectContext`, a later task), since every
//! candidate drawn from one `ConnectionIntent`/`CandidatePool` shares the
//! same session secret today.

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use crate::{ConnectionIntent, IntentTransport};

/// A validated, canonical SHA-256 certificate fingerprint (32 bytes).
///
/// Kept as a distinct type — rather than passing `cert_sha256_hex: String`
/// straight into [`CandidateRoute`]/[`CandidateKey`] — because two
/// differently-cased or differently-delimited hex strings for the *same*
/// underlying bytes (`"AB12…"` vs `"ab12…"` vs `"ab:12:…"`) would otherwise
/// be misjudged as distinct candidates by a naive string-equality dedup.
/// Comparing decoded bytes makes that class of bug structurally impossible.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CertificatePinSha256([u8; 32]);

/// Matches `isekai_protocol::handshake`'s existing `cert_sha256` contract
/// (lowercase hex, exactly this many characters) — kept as a local literal
/// rather than a cross-crate dependency since `isekai-pipe-core` has no
/// other reason to depend on `isekai-protocol` yet.
const CERT_SHA256_HEX_LEN: usize = 64;

impl CertificatePinSha256 {
    /// Parses a lowercase hex-encoded SHA-256 digest. Rejects wrong length,
    /// uppercase, or non-hex characters — the same strictness
    /// `isekai_protocol::handshake::decode_handshake_json` already applies to
    /// `cert_sha256`, so a value that already passed handshake validation
    /// always parses here too.
    pub fn from_hex(hex: &str) -> Result<Self, CertificatePinError> {
        if hex.len() != CERT_SHA256_HEX_LEN {
            return Err(CertificatePinError::WrongLength { got: hex.len() });
        }
        if !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(CertificatePinError::NotLowercaseHex);
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
            let hi = hex_nibble(chunk[0]);
            let lo = hex_nibble(chunk[1]);
            bytes[i] = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            use fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}

impl fmt::Debug for CertificatePinSha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Not a secret (it's a public certificate fingerprint, already
        // exchanged in plaintext handshake JSON) — safe to print in full,
        // unlike session secrets.
        write!(f, "CertificatePinSha256({})", self.to_hex())
    }
}

fn hex_nibble(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => unreachable!("validated by from_hex's all-hex-digit check"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificatePinError {
    WrongLength { got: usize },
    NotLowercaseHex,
}

impl fmt::Display for CertificatePinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength { got } => {
                write!(f, "certificate pin must be {CERT_SHA256_HEX_LEN} hex characters, got {got}")
            }
            Self::NotLowercaseHex => write!(f, "certificate pin must be lowercase hex"),
        }
    }
}

impl std::error::Error for CertificatePinError {}

/// A normalized server name / SNI value: lowercased, without a trailing
/// root-zone dot, and never empty. IP-literal values (`"203.0.113.5"`) are
/// recognized and passed through as-is (lowercasing/dot-stripping would be
/// meaningless, and IP literals are never DNS names to begin with) rather
/// than treated as a hostname.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NormalizedServerName(String);

impl NormalizedServerName {
    pub fn new(raw: &str) -> Result<Self, ServerNameError> {
        if raw.is_empty() {
            return Err(ServerNameError::Empty);
        }
        if raw.parse::<std::net::IpAddr>().is_ok() {
            return Ok(Self(raw.to_string()));
        }
        let trimmed = raw.strip_suffix('.').unwrap_or(raw);
        if trimmed.is_empty() {
            return Err(ServerNameError::Empty);
        }
        Ok(Self(trimmed.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerNameError {
    Empty,
}

impl fmt::Display for ServerNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "server name must not be empty"),
        }
    }
}

impl std::error::Error for ServerNameError {}

/// Opaque candidate identifier. **Contract: pool-local only.** A
/// `CandidatePool` assigns these and keeps the same id across a re-insert of
/// an already-known [`CandidateKey`] (dedup merge). There is no promise of
/// stability across process restarts or across different pools — if a
/// cross-process-stable fingerprint is ever needed (e.g. for telemetry
/// correlation across a resume/reconnect that spans processes), add a
/// separate `candidate_key_fingerprint` field rather than overloading this
/// type's contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CandidateId(pub u64);

/// A candidate-collection round. Belongs to a *batch* of candidates
/// ([`CandidateDraftBatch`]/`CandidateSnapshot`), not to an individual
/// candidate — a `CandidatePool` uses this to reject stale results from a
/// superseded collection round, distinct from [`PunchGeneration`] (STUN
/// hole-punch retry counter) and any future `ConnectionGeneration` (attach
/// generation, `#18`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CandidateGeneration(pub u64);

impl CandidateGeneration {
    pub const INITIAL: CandidateGeneration = CandidateGeneration(0);

    pub fn next(self) -> CandidateGeneration {
        CandidateGeneration(self.0 + 1)
    }
}

/// Which broad class a [`CandidateRoute`] belongs to. `CandidatePriority`
/// comparisons are only meaningful *within* one class — see
/// `CandidateRoute::class`'s docs for why `Candidate` deliberately does not
/// implement `Ord` directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateClass {
    Direct,
    Relay,
}

/// How to actually dial one candidate. Each variant carries exactly the
/// fields that determine dedup identity ([`CandidateKey`] mirrors this
/// shape) — nothing here is a discovery detail (which STUN server told us
/// about a direct peer, which config source produced a relay endpoint); that
/// belongs on [`CandidateOrigin`] instead.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CandidateRoute {
    /// A direct (no-relay) attempt against a peer's own address — reached
    /// either via a STUN-observed address + hole-punch, or the
    /// direct-by-bootstrap-host candidate. `stun_server` is deliberately
    /// part of the dedup identity here (not just provenance): each attempt
    /// creates its own throwaway UDP socket and redoes its own
    /// STUN-query/hole-punch dance, so "same peer, different STUN server" is
    /// a genuinely different connection recipe in this v1 model, not a
    /// duplicate. A future `RouteGroupKey{cert_pin, peer_addr}` (not this
    /// type) would be the right place to group same-physical-route
    /// candidates discovered via different STUN servers, if `#11` ever needs
    /// that — this type itself should not be widened to do it.
    StunP2p {
        cert_pin: CertificatePinSha256,
        peer_addr: SocketAddr,
        stun_server: SocketAddr,
        /// QUIC SNI presented to the peer. `isekai-helper` ignores it today
        /// (see `RemoteSpec::server_name`'s docs in `isekai-transport`), but
        /// it's still required to actually dial — carried here rather than
        /// dropped, unlike an earlier draft of this type that omitted it by
        /// mistake.
        server_name: NormalizedServerName,
    },
    /// A relay-mediated attempt. Same resolved `helper_addr` reached through
    /// a different `server_name` (SNI/virtual host) is treated as a
    /// different candidate, since that can select a different endpoint
    /// behind the same IP:port.
    Relay {
        cert_pin: CertificatePinSha256,
        helper_addr: SocketAddr,
        server_name: NormalizedServerName,
    },
}

impl CandidateRoute {
    pub fn class(&self) -> CandidateClass {
        match self {
            CandidateRoute::StunP2p { .. } => CandidateClass::Direct,
            CandidateRoute::Relay { .. } => CandidateClass::Relay,
        }
    }

    /// The exact route-variant label telemetry logs as `candidate_kind`
    /// (`isekai-transport::telemetry`) — distinct from `class()`: this names
    /// the specific dialing strategy (`"stun-p2p"`), not the broader
    /// priority-comparison grouping (`Direct`) that a second, non-STUN direct
    /// strategy would eventually share.
    pub fn kind_label(&self) -> &'static str {
        match self {
            CandidateRoute::StunP2p { .. } => "stun-p2p",
            CandidateRoute::Relay { .. } => "relay",
        }
    }

    /// The dedup identity for this route. Currently identical in shape to
    /// `CandidateRoute` itself (no field has been added yet that affects how
    /// to dial but not whether two routes are "the same") — kept as a
    /// separate type anyway so a future route-only field (e.g. a local bind
    /// hint) can be added without silently changing dedup behavior.
    pub fn key(&self) -> CandidateKey {
        match self {
            CandidateRoute::StunP2p { cert_pin, peer_addr, stun_server, server_name } => CandidateKey::StunP2p {
                cert_pin: *cert_pin,
                peer_addr: *peer_addr,
                stun_server: *stun_server,
                server_name: server_name.clone(),
            },
            CandidateRoute::Relay { cert_pin, helper_addr, server_name } => {
                CandidateKey::Relay { cert_pin: *cert_pin, helper_addr: *helper_addr, server_name: server_name.clone() }
            }
        }
    }
}

/// Dedup identity for a [`CandidateRoute`] — see that type's docs and this
/// module's own docs for why address equality alone is not enough (direct vs
/// relay to the same resolved address are different candidates).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CandidateKey {
    StunP2p {
        cert_pin: CertificatePinSha256,
        peer_addr: SocketAddr,
        stun_server: SocketAddr,
        server_name: NormalizedServerName,
    },
    Relay { cert_pin: CertificatePinSha256, helper_addr: SocketAddr, server_name: NormalizedServerName },
}

/// Where a candidate was discovered — provenance, *not* dedup identity
/// (multiple origins can point at the same [`CandidateKey`]; see
/// `CandidateOrigin`'s own docs for the naming trap this avoids).
///
/// Deliberately `#[non_exhaustive]`-shaped in spirit — more will be added as
/// more providers land (`#11`'s multi-STUN, `#20`'s SSH-bootstrap-exchanged
/// candidates), each requiring a deliberate decision about how that origin
/// should be labeled rather than silently falling through a wildcard match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CandidateOriginKind {
    /// Converted directly from the legacy single-transport `ConnectionIntent`
    /// (`LegacyIntentProvider`).
    LegacyIntent,
    /// One of the alternate relay-assigned addresses listed in
    /// `ConnectionIntent::relay_endpoints` (`ConfigRelayProvider`, `#12`) —
    /// all pointing at the same isekai-helper instance (same cert pin,
    /// session secret, and server name) for relay-endpoint fallback.
    ConfigRelay,
}

impl CandidateOriginKind {
    /// The label telemetry logs as `candidate_source`
    /// (`isekai-transport::telemetry`).
    pub fn label(&self) -> &'static str {
        match self {
            CandidateOriginKind::LegacyIntent => "legacy-intent",
            CandidateOriginKind::ConfigRelay => "config-relay",
        }
    }
}

/// One "who discovered this candidate" record.
///
/// **Naming trap this avoids**: the `candidate_source` telemetry field
/// (`isekai-transport::telemetry`, predates this module) means "route kind"
/// (`"relay"` / `"stun-p2p"`) — that's [`CandidateClass`]/[`CandidateRoute`],
/// not this. This type's `source` means "which provider found it"
/// (`"legacy-intent"`, later `"trust-store"`/`"config"`/`"stun"`). Do not
/// call both concepts `CandidateSource`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CandidateOrigin {
    pub source: CandidateOriginKind,
    pub provider_id: String,
}

/// Relative priority *within one [`CandidateClass`]*. Comparing priorities
/// across classes (a `Direct` candidate's rank vs a `Relay` candidate's rank)
/// is meaningless — `Candidate` deliberately does not implement `Ord` for
/// this reason; a pool/scheduler that needs ordering must group by class
/// first. Independent from `start_delay`-style scheduling concerns (when to
/// begin an attempt), which belong to the race policy, not the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CandidatePriority {
    /// Lower ranks are preferred.
    pub rank: u16,
}

/// How long a candidate may be considered fresh enough to start a *new*
/// connection attempt against. Does not bound an already-in-flight
/// attempt's own connection lifetime — a `CandidatePool` (later task)
/// resolves this against a monotonic clock at insert/lookup time; this type
/// only carries the relative policy, not an absolute deadline (no
/// `Instant`/`SystemTime` here — this crate stays free of runtime-clock
/// concerns).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateValidity {
    /// Not subject to discovery-result staleness (e.g. static config).
    /// Still not a promise of permanent network reachability — just "not
    /// aged out by a TTL policy".
    Static,
    /// Reusable for this long after being observed (e.g. a STUN-observed
    /// address, whose underlying NAT mapping may expire — RFC 4787 notes
    /// mapping-timeout behavior varies widely across NAT implementations, so
    /// this is a local reuse policy, not a guarantee the mapping is still
    /// live).
    Lease { ttl: Duration },
}

impl CandidateValidity {
    /// Merge rule for two observations of the same [`CandidateKey`]:
    /// `Static` always wins (a statically-configured candidate's validity
    /// isn't diminished by also being independently discovered with a
    /// shorter-lived lease), and between two leases the longer-lived one
    /// wins (a fresh observation may reasonably extend how long the
    /// candidate is considered usable).
    pub fn merge(self, other: CandidateValidity) -> CandidateValidity {
        match (self, other) {
            (CandidateValidity::Static, _) | (_, CandidateValidity::Static) => CandidateValidity::Static,
            (CandidateValidity::Lease { ttl: a }, CandidateValidity::Lease { ttl: b }) => {
                CandidateValidity::Lease { ttl: a.max(b) }
            }
        }
    }
}

/// One provider's not-yet-deduplicated candidate output — no [`CandidateId`]
/// yet (a `CandidatePool` assigns one on insert, reusing an existing id if
/// this draft's route already matches a known [`CandidateKey`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateDraft {
    pub route: CandidateRoute,
    pub origin: CandidateOrigin,
    pub priority: CandidatePriority,
    pub validity: CandidateValidity,
}

/// A `CandidateProvider::gather` call's full result: the drafts *and* which
/// collection round they belong to, bundled together so a pool can never
/// apply a batch under the wrong generation (the generation living in a
/// separate argument invited exactly that mistake — see `#21`'s task notes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateDraftBatch {
    pub generation: CandidateGeneration,
    pub candidates: Vec<CandidateDraft>,
}

/// A pool-resident, deduplicated, id-assigned candidate. `origins` is plural
/// (unlike `CandidateDraft::origin`) because multiple discoveries can merge
/// into one candidate once their `CandidateRoute::key()`s match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: CandidateId,
    pub route: CandidateRoute,
    pub origins: Vec<CandidateOrigin>,
    pub priority: CandidatePriority,
    pub validity: CandidateValidity,
}

/// A pool's `eligible_candidates()`-style output: like
/// [`CandidateDraftBatch`], but post-dedup or `Candidate`s instead of
/// pre-dedup `CandidateDraft`s. Kept as a distinct type (rather than reusing
/// one generic `CandidateBatch<T>`) so it's never ambiguous from the type
/// alone whether a batch's contents have been deduplicated/id-assigned yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateSnapshot {
    pub generation: CandidateGeneration,
    pub candidates: Vec<Candidate>,
}

/// `provider_id` `LegacyIntentProvider` (`isekai-transport`) stamps onto the
/// single [`CandidateOrigin`] it ever produces — there is exactly one
/// instance of this provider kind, so a fixed literal is enough (contrast
/// with e.g. a future multi-STUN provider, where `provider_id` would need to
/// distinguish *which* configured STUN server produced a given candidate).
pub const LEGACY_INTENT_PROVIDER_ID: &str = "legacy-intent";

/// Converts the legacy single-transport `ConnectionIntent` (today's only
/// connection-intent shape — see `IntentTransport`'s own docs) into exactly
/// one [`CandidateDraft`]. Pure and infallible-in-practice except for
/// malformed data that should never occur in a `ConnectionIntent` a trust
/// store actually produced (kept fallible anyway rather than panicking,
/// since this crate must never trust its own serialized state
/// unconditionally).
///
/// Takes the whole `ConnectionIntent` (not just `IntentTransport`) because
/// the certificate pin lives on `ConnectionIntent::expected_server_identity`,
/// a sibling field of `transport` — the route and its expected identity are
/// only paired together at this level.
impl TryFrom<&ConnectionIntent> for CandidateDraft {
    type Error = CandidateConversionError;

    fn try_from(intent: &ConnectionIntent) -> Result<Self, Self::Error> {
        let cert_pin = CertificatePinSha256::from_hex(&intent.expected_server_identity.cert_sha256_hex)
            .map_err(CandidateConversionError::CertificatePin)?;

        let route = match &intent.transport {
            IntentTransport::Relay { helper_addr, server_name, .. } => CandidateRoute::Relay {
                cert_pin,
                helper_addr: parse_socket_addr(helper_addr, "helper_addr")?,
                server_name: NormalizedServerName::new(server_name).map_err(CandidateConversionError::ServerName)?,
            },
            IntentTransport::StunP2p { stun_server, peer_addr, server_name, .. } => CandidateRoute::StunP2p {
                cert_pin,
                peer_addr: parse_socket_addr(peer_addr, "peer_addr")?,
                stun_server: parse_socket_addr(stun_server, "stun_server")?,
                server_name: NormalizedServerName::new(server_name).map_err(CandidateConversionError::ServerName)?,
            },
        };

        Ok(CandidateDraft {
            route,
            origin: CandidateOrigin {
                source: CandidateOriginKind::LegacyIntent,
                provider_id: LEGACY_INTENT_PROVIDER_ID.to_string(),
            },
            // Only one candidate ever exists in this v1 conversion, so the
            // rank is arbitrary — `0` for "the only option".
            priority: CandidatePriority { rank: 0 },
            // Sourced directly from the trust store / explicit config, not a
            // discovery result with its own freshness window.
            validity: CandidateValidity::Static,
        })
    }
}

fn parse_socket_addr(raw: &str, field: &'static str) -> Result<SocketAddr, CandidateConversionError> {
    raw.parse().map_err(|_| CandidateConversionError::InvalidSocketAddr { field, value: raw.to_string() })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateConversionError {
    CertificatePin(CertificatePinError),
    ServerName(ServerNameError),
    InvalidSocketAddr { field: &'static str, value: String },
}

impl fmt::Display for CandidateConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CertificatePin(e) => write!(f, "invalid expected_server_identity.cert_sha256_hex: {e}"),
            Self::ServerName(e) => write!(f, "invalid server_name: {e}"),
            Self::InvalidSocketAddr { field, value } => write!(f, "invalid {field}: {value:?}"),
        }
    }
}

impl std::error::Error for CandidateConversionError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(byte: u8) -> CertificatePinSha256 {
        CertificatePinSha256::from_hex(&format!("{:02x}", byte).repeat(32)).unwrap()
    }

    fn helper_name() -> NormalizedServerName {
        NormalizedServerName::new("isekai-helper").unwrap()
    }

    #[test]
    fn certificate_pin_roundtrips_through_hex() {
        let hex = "ab".repeat(32);
        let parsed = CertificatePinSha256::from_hex(&hex).unwrap();
        assert_eq!(parsed.to_hex(), hex);
        assert_eq!(parsed.as_bytes(), &[0xabu8; 32]);
    }

    #[test]
    fn certificate_pin_rejects_wrong_length() {
        assert_eq!(
            CertificatePinSha256::from_hex("abcd").unwrap_err(),
            CertificatePinError::WrongLength { got: 4 }
        );
    }

    #[test]
    fn certificate_pin_rejects_uppercase() {
        let hex = "AB".repeat(32);
        assert_eq!(CertificatePinSha256::from_hex(&hex).unwrap_err(), CertificatePinError::NotLowercaseHex);
    }

    #[test]
    fn certificate_pin_equal_regardless_of_how_it_was_parsed() {
        // The whole point: two hex strings decoding to the same bytes must
        // compare equal, even though a naive `cert_sha256_hex: String`
        // comparison would already agree here (same string) — the real
        // regression this type prevents is a *different textual* encoding
        // of the same bytes (tested implicitly: there is exactly one valid
        // lowercase-hex encoding, so this type structurally cannot diverge).
        let a = pin(0xab);
        let b = CertificatePinSha256::from_hex(&"ab".repeat(32)).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn server_name_lowercases_and_strips_trailing_dot() {
        let name = NormalizedServerName::new("Relay.Example.COM.").unwrap();
        assert_eq!(name.as_str(), "relay.example.com");
    }

    #[test]
    fn server_name_rejects_empty() {
        assert_eq!(NormalizedServerName::new("").unwrap_err(), ServerNameError::Empty);
        assert_eq!(NormalizedServerName::new(".").unwrap_err(), ServerNameError::Empty);
    }

    #[test]
    fn server_name_passes_ip_literals_through_unchanged() {
        let name = NormalizedServerName::new("203.0.113.5").unwrap();
        assert_eq!(name.as_str(), "203.0.113.5");
    }

    #[test]
    fn candidate_key_distinguishes_direct_and_relay_at_the_same_address() {
        let addr: SocketAddr = "203.0.113.5:45231".parse().unwrap();
        let direct = CandidateRoute::StunP2p {
            cert_pin: pin(1),
            peer_addr: addr,
            stun_server: "203.0.113.9:3478".parse().unwrap(),
            server_name: helper_name(),
        };
        let relay = CandidateRoute::Relay {
            cert_pin: pin(1),
            helper_addr: addr,
            server_name: NormalizedServerName::new("isekai-helper").unwrap(),
        };
        assert_ne!(direct.key(), relay.key(), "direct and relay routes to the same address must not dedup together");
    }

    #[test]
    fn candidate_key_distinguishes_different_stun_servers_for_the_same_peer() {
        let peer: SocketAddr = "203.0.113.5:45231".parse().unwrap();
        let a = CandidateRoute::StunP2p {
            cert_pin: pin(1),
            peer_addr: peer,
            stun_server: "203.0.113.9:3478".parse().unwrap(),
            server_name: helper_name(),
        };
        let b = CandidateRoute::StunP2p {
            cert_pin: pin(1),
            peer_addr: peer,
            stun_server: "198.51.100.1:3478".parse().unwrap(),
            server_name: helper_name(),
        };
        assert_ne!(a.key(), b.key(), "different STUN servers must be distinct v1 candidates, even for the same peer");
    }

    #[test]
    fn candidate_key_distinguishes_different_server_names_for_the_same_relay_address() {
        let addr: SocketAddr = "203.0.113.5:443".parse().unwrap();
        let a = CandidateRoute::Relay { cert_pin: pin(1), helper_addr: addr, server_name: NormalizedServerName::new("relay-a.example").unwrap() };
        let b = CandidateRoute::Relay { cert_pin: pin(1), helper_addr: addr, server_name: NormalizedServerName::new("relay-b.example").unwrap() };
        assert_ne!(a.key(), b.key());
    }

    #[test]
    fn candidate_route_class_matches_its_kind() {
        let stun = CandidateRoute::StunP2p {
            cert_pin: pin(1),
            peer_addr: "203.0.113.5:1".parse().unwrap(),
            stun_server: "203.0.113.9:3478".parse().unwrap(),
            server_name: helper_name(),
        };
        let relay = CandidateRoute::Relay {
            cert_pin: pin(1),
            helper_addr: "203.0.113.5:1".parse().unwrap(),
            server_name: NormalizedServerName::new("isekai-helper").unwrap(),
        };
        assert_eq!(stun.class(), CandidateClass::Direct);
        assert_eq!(relay.class(), CandidateClass::Relay);
    }

    #[test]
    fn candidate_validity_merge_rules() {
        let short = CandidateValidity::Lease { ttl: Duration::from_secs(30) };
        let long = CandidateValidity::Lease { ttl: Duration::from_secs(90) };
        assert_eq!(CandidateValidity::Static.merge(CandidateValidity::Static), CandidateValidity::Static);
        assert_eq!(CandidateValidity::Static.merge(short), CandidateValidity::Static);
        assert_eq!(short.merge(CandidateValidity::Static), CandidateValidity::Static);
        assert_eq!(short.merge(long), long);
        assert_eq!(long.merge(short), long);
    }

    #[test]
    fn candidate_generation_increments() {
        let gen = CandidateGeneration::INITIAL;
        assert_eq!(gen.next(), CandidateGeneration(1));
        assert_eq!(gen.next().next(), CandidateGeneration(2));
    }

    fn sample_intent(transport: IntentTransport) -> ConnectionIntent {
        crate::ConnectionIntent::new(
            "production",
            "ssh",
            crate::ServerIdentity { cert_sha256_hex: "ab".repeat(32) },
            transport,
            crate::BootstrapProvenance::TrustStore { key: "production:22".to_string() },
        )
    }

    #[test]
    fn candidate_draft_from_relay_intent() {
        let intent = sample_intent(IntentTransport::Relay {
            helper_addr: "203.0.113.5:45231".to_string(),
            server_name: "isekai-helper".to_string(),
            session_secret_b64: "c2VjcmV0".to_string(),
        });
        let draft = CandidateDraft::try_from(&intent).unwrap();
        assert_eq!(
            draft.route,
            CandidateRoute::Relay {
                cert_pin: pin(0xab),
                helper_addr: "203.0.113.5:45231".parse().unwrap(),
                server_name: NormalizedServerName::new("isekai-helper").unwrap(),
            }
        );
        assert_eq!(draft.origin.source, CandidateOriginKind::LegacyIntent);
        assert_eq!(draft.origin.provider_id, LEGACY_INTENT_PROVIDER_ID);
        assert_eq!(draft.validity, CandidateValidity::Static);
    }

    #[test]
    fn candidate_draft_from_stun_p2p_intent() {
        let intent = sample_intent(IntentTransport::StunP2p {
            stun_server: "203.0.113.9:3478".to_string(),
            peer_addr: "203.0.113.5:45231".to_string(),
            server_name: "isekai-helper".to_string(),
            session_secret_b64: "c2VjcmV0".to_string(),
        });
        let draft = CandidateDraft::try_from(&intent).unwrap();
        assert_eq!(
            draft.route,
            CandidateRoute::StunP2p {
                cert_pin: pin(0xab),
                peer_addr: "203.0.113.5:45231".parse().unwrap(),
                stun_server: "203.0.113.9:3478".parse().unwrap(),
                server_name: helper_name(),
            }
        );
    }

    #[test]
    fn candidate_draft_rejects_invalid_helper_addr() {
        let intent = sample_intent(IntentTransport::Relay {
            helper_addr: "not-an-address".to_string(),
            server_name: "isekai-helper".to_string(),
            session_secret_b64: "c2VjcmV0".to_string(),
        });
        let err = CandidateDraft::try_from(&intent).unwrap_err();
        assert!(matches!(err, CandidateConversionError::InvalidSocketAddr { field: "helper_addr", .. }));
    }

    #[test]
    fn candidate_draft_never_carries_the_session_secret() {
        // The whole point of separating Candidate from session auth material
        // (`ConnectContext`, a later task): confirm the conversion has no
        // path that could smuggle `session_secret_b64` into the draft.
        let intent = sample_intent(IntentTransport::Relay {
            helper_addr: "203.0.113.5:45231".to_string(),
            server_name: "isekai-helper".to_string(),
            session_secret_b64: "top-secret-value".to_string(),
        });
        let draft = CandidateDraft::try_from(&intent).unwrap();
        let debug = format!("{draft:?}");
        assert!(!debug.contains("top-secret-value"));
    }
}
