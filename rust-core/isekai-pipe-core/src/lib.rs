//! Core value types and local runtime storage for `isekai-pipe`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::RngCore;
use serde::{Deserialize, Serialize};

pub use isekai_pipe_protocol::{LogicalHost, ServiceName};

mod candidate;
pub use candidate::{
    validate_endpoint_identity, Candidate, CandidateClass, CandidateConversionError, CandidateDraft,
    CandidateDraftBatch, CandidateGeneration, CandidateId, CandidateKey, CandidateOrigin, CandidateOriginKind,
    CandidatePriority, CandidateRoute, CandidateSnapshot, CandidateValidity, CertificatePinError,
    CertificatePinSha256, NormalizedServerName, ServerNameError, LEGACY_INTENT_PROVIDER_ID,
};

mod profile;
pub use profile::{
    default_profiles_dir, load_persistent_profile, migrate_trust_store, update_persistent_profile,
    write_persistent_profile, LegacyRelayTransport, PathHint, PersistentProfile, PERSISTENT_PROFILE_SCHEMA_VERSION,
};

mod outcome;
pub use outcome::{
    claim_connect_outcome, write_connect_outcome, ConnectOutcome, ConnectOutcomeClass, CONNECT_OUTCOME_SCHEMA_VERSION,
};

mod ctl_gc;
pub use ctl_gc::sweep_stale_sockets;

mod port_range;
pub use port_range::parse_port_range;

pub const CONNECTION_INTENT_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_INTENT_TTL: Duration = Duration::from_secs(120);
pub const DEFAULT_CANDIDATE_RACE_DELAY_MS: u64 = 150;
pub const DEFAULT_RELAY_DELAY_MS: u64 = 750;
/// Requested resume-grace period sent in `HELLO` (`isekai_protocol::hello`),
/// absent an explicit `#@isekai resume-grace` override — the server clamps
/// this to its own configured max and echoes back the effective value in
/// `ACK` (`ISEKAI_PIPE_DESIGN.md`).
pub const DEFAULT_RESUME_GRACE_SECS: u64 = 120;

/// STUN hole-punch retry counter, distinct from `CandidateGeneration`
/// (candidate-collection round) and any future connection-attach generation
/// (`#18`) — newtyped specifically to prevent those three counters from
/// being confused with one another (`candidate.rs`'s module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PunchGeneration(pub u64);

/// A remote service exposed by `isekai-pipe serve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    name: ServiceName,
    target: String,
}

impl ServiceSpec {
    pub fn new(name: ServiceName, target: impl Into<String>) -> Result<Self, ServiceSpecError> {
        let target = target.into();
        if name.as_str().is_empty() {
            return Err(ServiceSpecError::EmptyName);
        }
        if target.is_empty() {
            return Err(ServiceSpecError::EmptyTarget);
        }
        Ok(Self { name, target })
    }

    pub fn parse(input: &str) -> Result<Self, ServiceSpecError> {
        let Some((name, target)) = input.split_once('=') else {
            return Err(ServiceSpecError::MissingEquals);
        };
        Self::new(ServiceName::new(name), target)
    }

    pub fn ssh_target(target: impl Into<String>) -> Result<Self, ServiceSpecError> {
        Self::new(ServiceName::new("ssh"), target)
    }

    pub fn name(&self) -> &ServiceName {
        &self.name
    }

    pub fn target(&self) -> &str {
        &self.target
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceSpecError {
    MissingEquals,
    EmptyName,
    EmptyTarget,
}

impl std::fmt::Display for ServiceSpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingEquals => write!(f, "service must be in name=target form"),
            Self::EmptyName => write!(f, "service name must not be empty"),
            Self::EmptyTarget => write!(f, "service target must not be empty"),
        }
    }
}

impl std::error::Error for ServiceSpecError {}

/// High-level role of an `isekai-pipe` process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeRole {
    /// Local side: stdio/TCP listen to logical session.
    Connect,
    /// Remote side: logical session to service target.
    Serve,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionIntent {
    pub schema_version: u32,
    pub intent_id: String,
    pub profile: String,
    pub peer_id: String,
    pub expected_server_identity: ServerIdentity,
    pub service: String,
    #[serde(default)]
    pub link_endpoints: Vec<String>,
    #[serde(default)]
    pub rendezvous: Vec<String>,
    #[serde(default)]
    pub stun_servers: Vec<String>,
    #[serde(default)]
    pub relay_endpoints: Vec<String>,
    pub transport: IntentTransport,
    /// `ISEKAI_PIPE_DESIGN.md` §8 Epic I's `I-route-scheduler`: an alternate
    /// transport *family* to try, in order, if `transport` fails entirely —
    /// not a same-family fallback (that's what `stun_servers`/`relay_endpoints`
    /// already express) and not racing (deliberately out of scope, see this
    /// crate's docs on `IntentTransport`). `None` — today's default — means
    /// no cross-family fallback exists for this intent; a caller (e.g.
    /// `isekai-ssh/src/wrapper.rs::select_transport`, which already computes
    /// the "the other family also had a usable transport" fact but
    /// previously discarded it) may set this when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cross_family_fallback: Option<IntentTransport>,
    pub relay_policy: RelayPolicy,
    #[serde(default = "default_candidate_race_delay_ms")]
    pub candidate_race_delay_ms: u64,
    #[serde(default = "default_relay_delay_ms")]
    pub relay_delay_ms: u64,
    /// Requested resume-grace period, in seconds, sent to the server as part
    /// of `HELLO` — the *request*, not the negotiated value (the server
    /// always has final say via its `ACK`'s effective value). `0` means "no
    /// preference, use the server's own default/max".
    #[serde(default = "default_resume_grace_secs")]
    pub resume_grace_secs: u64,
    /// `None` binds this connection's local QUIC socket to an OS-assigned
    /// ephemeral port (the default); `Some((start, end))` narrows it to
    /// that inclusive range instead — for a caller behind a restrictive
    /// local firewall/NAT that only permits outbound UDP within a known
    /// range (`#@isekai local-bind-port-range`). The client-side
    /// counterpart of `isekai-helper --bind-port-range` on the remote side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_bind_port_range: Option<(u16, u16)>,
    pub punch_generation: PunchGeneration,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub bootstrap_provenance: BootstrapProvenance,
}

impl ConnectionIntent {
    pub fn new(
        profile: impl Into<String>,
        service: impl Into<String>,
        expected_server_identity: ServerIdentity,
        transport: IntentTransport,
        bootstrap_provenance: BootstrapProvenance,
    ) -> Self {
        let profile = profile.into();
        let now = unix_ms(SystemTime::now());
        let expires = now + DEFAULT_INTENT_TTL.as_millis() as u64;
        Self {
            schema_version: CONNECTION_INTENT_SCHEMA_VERSION,
            intent_id: new_intent_id(),
            peer_id: profile.clone(),
            profile,
            expected_server_identity,
            service: service.into(),
            link_endpoints: Vec::new(),
            rendezvous: Vec::new(),
            stun_servers: Vec::new(),
            relay_endpoints: Vec::new(),
            transport,
            cross_family_fallback: None,
            relay_policy: RelayPolicy::RelayAllowed,
            candidate_race_delay_ms: DEFAULT_CANDIDATE_RACE_DELAY_MS,
            relay_delay_ms: DEFAULT_RELAY_DELAY_MS,
            resume_grace_secs: DEFAULT_RESUME_GRACE_SECS,
            local_bind_port_range: None,
            punch_generation: PunchGeneration(0),
            created_at_unix_ms: now,
            expires_at_unix_ms: expires,
            bootstrap_provenance,
        }
    }

    pub fn validate_for_use(&self, now: SystemTime) -> Result<(), IntentError> {
        if self.schema_version != CONNECTION_INTENT_SCHEMA_VERSION {
            return Err(IntentError::UnsupportedSchema(self.schema_version));
        }
        if unix_ms(now) > self.expires_at_unix_ms {
            return Err(IntentError::Expired);
        }
        Ok(())
    }
}

fn default_candidate_race_delay_ms() -> u64 {
    DEFAULT_CANDIDATE_RACE_DELAY_MS
}

fn default_relay_delay_ms() -> u64 {
    DEFAULT_RELAY_DELAY_MS
}

fn default_resume_grace_secs() -> u64 {
    DEFAULT_RESUME_GRACE_SECS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerIdentity {
    pub cert_sha256_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum IntentTransport {
    Relay {
        helper_addr: String,
        server_name: String,
        session_secret_b64: String,
    },
    StunP2p {
        stun_server: String,
        peer_addr: String,
        server_name: String,
        session_secret_b64: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RelayPolicy {
    RelayAllowed,
    RelayRequired,
    DirectOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum BootstrapProvenance {
    TrustStore { key: String },
    ExplicitProfile,
}

#[derive(Debug)]
pub enum IntentError {
    Io(io::Error),
    Json(serde_json::Error),
    InvalidIntentId,
    Missing,
    Expired,
    UnsupportedSchema(u32),
}

impl std::fmt::Display for IntentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Json(e) => write!(f, "{e}"),
            Self::InvalidIntentId => write!(f, "intent id contains invalid characters"),
            Self::Missing => write!(f, "connection intent was not found"),
            Self::Expired => write!(f, "connection intent has expired"),
            Self::UnsupportedSchema(version) => {
                write!(f, "unsupported connection intent schema version {version}")
            }
        }
    }
}

impl std::error::Error for IntentError {}

impl From<io::Error> for IntentError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for IntentError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub fn default_runtime_dir() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("ISEKAI_PIPE_RUNTIME_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(path).join("isekai"));
    }
    Ok(std::env::temp_dir().join(format!("isekai-{}", current_uid())))
}

pub fn write_connection_intent(
    runtime_dir: &Path,
    intent: &ConnectionIntent,
) -> Result<PathBuf, IntentError> {
    validate_intent_id(&intent.intent_id)?;
    let intents = runtime_dir.join("intents");
    create_private_dir(runtime_dir)?;
    create_private_dir(&intents)?;
    let path = intents.join(format!("{}.json", intent.intent_id));
    let tmp = intents.join(format!("{}.{}.tmp", intent.intent_id, std::process::id()));
    let bytes = serde_json::to_vec_pretty(intent)?;
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

pub fn claim_connection_intent(
    runtime_dir: &Path,
    intent_id: &str,
) -> Result<ConnectionIntent, IntentError> {
    validate_intent_id(intent_id)?;
    let src = runtime_dir
        .join("intents")
        .join(format!("{intent_id}.json"));
    let claimed_dir = runtime_dir.join("claimed");
    create_private_dir(runtime_dir)?;
    create_private_dir(&claimed_dir)?;
    let dst = claimed_dir.join(format!("{intent_id}.{}.json", std::process::id()));
    match fs::rename(&src, &dst) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(IntentError::Missing),
        Err(e) => return Err(IntentError::Io(e)),
    }
    let bytes = fs::read(&dst)?;
    let intent: ConnectionIntent = serde_json::from_slice(&bytes)?;
    intent.validate_for_use(SystemTime::now())?;
    Ok(intent)
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn validate_intent_id(intent_id: &str) -> Result<(), IntentError> {
    let valid = !intent_id.is_empty()
        && intent_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if valid {
        Ok(())
    } else {
        Err(IntentError::InvalidIntentId)
    }
}

fn new_intent_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        libc_getuid()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

#[cfg(unix)]
fn libc_getuid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_distinct() {
        assert_ne!(PipeRole::Connect, PipeRole::Serve);
    }

    #[test]
    fn parses_named_service_spec() {
        let spec = ServiceSpec::parse("ssh=127.0.0.1:22").unwrap();
        assert_eq!(spec.name().as_str(), "ssh");
        assert_eq!(spec.target(), "127.0.0.1:22");
    }

    #[test]
    fn maps_legacy_target_to_ssh_service() {
        let spec = ServiceSpec::ssh_target("127.0.0.1:22").unwrap();
        assert_eq!(spec.name().as_str(), "ssh");
        assert_eq!(spec.target(), "127.0.0.1:22");
    }

    #[test]
    fn rejects_malformed_service_specs() {
        assert_eq!(
            ServiceSpec::parse("ssh").unwrap_err(),
            ServiceSpecError::MissingEquals
        );
        assert_eq!(
            ServiceSpec::parse("=127.0.0.1:22").unwrap_err(),
            ServiceSpecError::EmptyName
        );
        assert_eq!(
            ServiceSpec::parse("ssh=").unwrap_err(),
            ServiceSpecError::EmptyTarget
        );
    }

    #[test]
    fn writes_and_claims_connection_intent_once() {
        let root =
            std::env::temp_dir().join(format!("isekai-pipe-intent-test-{}", new_intent_id()));
        let intent = ConnectionIntent::new(
            "production",
            "ssh",
            ServerIdentity {
                cert_sha256_hex: "ab".repeat(32),
            },
            IntentTransport::Relay {
                helper_addr: "127.0.0.1:1234".to_string(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: "c2VjcmV0".to_string(),
            },
            BootstrapProvenance::TrustStore {
                key: "production:22".to_string(),
            },
        );
        assert_eq!(intent.link_endpoints, Vec::<String>::new());
        assert_eq!(
            intent.candidate_race_delay_ms,
            DEFAULT_CANDIDATE_RACE_DELAY_MS
        );
        assert_eq!(intent.relay_delay_ms, DEFAULT_RELAY_DELAY_MS);

        write_connection_intent(&root, &intent).unwrap();
        let claimed = claim_connection_intent(&root, &intent.intent_id).unwrap();
        assert_eq!(claimed, intent);
        assert!(matches!(
            claim_connection_intent(&root, &intent.intent_id).unwrap_err(),
            IntentError::Missing
        ));
        let _ = fs::remove_dir_all(root);
    }
}
