//! Bootstrap request/report v2 wire types (`#20a`, ChatGPT second-opinion
//! consultation 2026-07-08, 3rd/4th rounds).
//!
//! The client's candidate list travels to `isekai-pipe serve` via
//! [`BootstrapRequestV2`] — written to a file over the SSH bootstrap exec's
//! stdin, length-prefixed alongside the existing `relay_jwt` payload (see
//! `isekai-bootstrap::openssh` for the transport side; this crate only
//! knows the wire shape, no SSH/file I/O). The server's response wraps the
//! existing [`crate::handshake::HandshakeJson`] in [`BootstrapReportV2`]
//! rather than adding fields to it directly — `BootstrapReportV2::v` (the
//! SSH-bootstrap-envelope schema version) and `HandshakeJson::v` (the
//! handshake-payload schema version) are two independent, non-conflicting
//! concepts precisely because of this nesting.
//!
//! Deliberately a **one-shot snapshot exchange**: no `end_of_candidates`
//! field, no update/withdraw/refresh frames. A `BootstrapReportV2` being
//! received successfully already means "candidate collection for this
//! bootstrap attempt is done" — continuous candidate exchange (network
//! change, expiry-driven refresh) is `#20c`'s job, deferred until real
//! telemetry (`#13b`) shows it's needed, and would use a QUIC control
//! stream rather than SSH as its transport (see `#20c`'s task notes) so it
//! is not a variant of this module at all, not even in spirit.
//!
//! [`BootstrapAttemptId`] identifies one SSH bootstrap *operation*, distinct
//! from three other identifiers this workspace already has, on purpose:
//! - [`crate::session_id::SessionId`]: the logical SSH session's own
//!   identity. The same session may be (re-)bootstrapped more than once
//!   (e.g. after a timeout), so a stale report from an earlier bootstrap
//!   attempt must be tellable apart from the current one even though both
//!   share the same `session_id`.
//! - `isekai_protocol::attach::AttemptId`/`ConnectionGeneration`: identify a
//!   single candidate's ATTACH try and its fencing generation — an entirely
//!   different layer (post-bootstrap connection establishment, not
//!   bootstrap itself).

use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::handshake::HandshakeJson;
use crate::session_id::{decode_session_id, SessionId, SESSION_ID_LEN};

pub const BOOTSTRAP_PROTOCOL_V2: u16 = 2;

pub const BOOTSTRAP_ATTEMPT_ID_LEN: usize = 16;

/// Generous cap for the whole `BootstrapRequestV2` JSON, mirroring
/// `handshake::MAX_HANDSHAKE_JSON_LEN`'s "reject a flood before it forces an
/// unbounded allocation" rationale.
pub const MAX_BOOTSTRAP_REQUEST_JSON_LEN: usize = 16 * 1024;
/// How many client candidates one request may carry.
pub const MAX_CLIENT_CANDIDATES: usize = 16;
/// Upper bound on a candidate's claimed remaining validity. A client
/// claiming a candidate is valid for, say, a full day is almost certainly
/// wrong (STUN-observed NAT mappings don't live that long) or hostile;
/// clamp rather than trust it verbatim.
pub const MAX_CANDIDATE_VALID_FOR_MS: u64 = 5 * 60 * 1000;

/// Identifies one SSH bootstrap operation. See module docs for why this is
/// a distinct type from `SessionId`/ATTACH's `AttemptId`/`ConnectionGeneration`.
/// `Debug`/`Display` show the same lowercase-hex form `SessionId` uses —
/// not a secret, just a correlation id.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BootstrapAttemptId([u8; BOOTSTRAP_ATTEMPT_ID_LEN]);

impl BootstrapAttemptId {
    pub fn from_bytes(bytes: [u8; BOOTSTRAP_ATTEMPT_ID_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; BOOTSTRAP_ATTEMPT_ID_LEN] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Debug for BootstrapAttemptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BootstrapAttemptId({})", self.to_hex())
    }
}

impl std::fmt::Display for BootstrapAttemptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

fn decode_hex_bytes<const N: usize>(field: &'static str, hex: &str) -> Result<[u8; N], ProtocolError> {
    if hex.len() != N * 2 {
        return Err(ProtocolError::HandshakeField {
            field,
            reason: format!("must be {} lowercase hex characters", N * 2),
        });
    }
    let mut out = [0u8; N];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let byte_str =
            std::str::from_utf8(chunk).map_err(|_| ProtocolError::HandshakeField { field, reason: "invalid hex".to_string() })?;
        let is_lowercase_hex = byte_str.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !is_lowercase_hex {
            return Err(ProtocolError::HandshakeField { field, reason: "must be lowercase hex".to_string() });
        }
        out[i] = u8::from_str_radix(byte_str, 16)
            .map_err(|_| ProtocolError::HandshakeField { field, reason: "invalid hex".to_string() })?;
    }
    Ok(out)
}

pub fn decode_bootstrap_attempt_id(hex: &str) -> Result<BootstrapAttemptId, ProtocolError> {
    Ok(BootstrapAttemptId(decode_hex_bytes::<BOOTSTRAP_ATTEMPT_ID_LEN>("bootstrap_attempt_id", hex)?))
}

fn decode_session_id_hex(hex: &str) -> Result<SessionId, ProtocolError> {
    let bytes = decode_hex_bytes::<SESSION_ID_LEN>("session_id", hex)?;
    decode_session_id(&bytes)
}

/// One candidate the client is offering the server as a hole-punch target
/// (`isekai-pipe serve`'s existing `--punch-peer` handling, generalized to
/// more than one address). Deliberately minimal — `route`/`endpoint` plus a
/// relative validity; no priority, no provenance metadata. This is *not*
/// the same type as `isekai-pipe-core::Candidate` (that type additionally
/// carries fencing-relevant identity like cert pins) — this is a bare wire
/// DTO converted into whatever domain representation the consuming crate
/// actually needs (`isekai-pipe`'s job, `#20a-3`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapCandidateV2 {
    /// e.g. `"stun-p2p"` — mirrors `HandshakeCandidate::kind`'s free-form
    /// string convention rather than a closed enum, so a future route kind
    /// doesn't require a schema version bump on this side either.
    pub route: String,
    /// `"ip:port"` the server should treat as a punch target.
    pub endpoint: String,
    /// How many milliseconds from *receipt* this candidate should be
    /// considered valid — relative, not absolute, since client and server
    /// clocks are not assumed to be synchronized. Clamped to
    /// `MAX_CANDIDATE_VALID_FOR_MS` by the receiver, never trusted verbatim.
    pub valid_for_ms: u64,
}

/// Client → server direction: the client's own candidates, delivered
/// alongside the existing `relay_jwt` over the bootstrap SSH exec's stdin
/// (`#20a-2`), never over argv (same rationale as `relay_jwt` already not
/// touching argv — other local users on the remote host can read process
/// argv via `ps aux`/`/proc/<pid>/cmdline`, and this may carry
/// network-topology-revealing addresses).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapRequestV2 {
    pub v: u16,
    /// Lowercase hex `SessionId`. See module docs for why this is a plain
    /// `String` at the wire layer (mirrors `HandshakeJson::session_secret`'s
    /// own "opaque string on the wire, typed accessor after validation"
    /// convention) rather than a custom serde impl on `SessionId` itself.
    pub session_id: String,
    /// Lowercase hex `BootstrapAttemptId`.
    pub bootstrap_attempt_id: String,
    #[serde(default)]
    pub client_candidates: Vec<BootstrapCandidateV2>,
}

impl BootstrapRequestV2 {
    pub fn session_id(&self) -> Result<SessionId, ProtocolError> {
        decode_session_id_hex(&self.session_id)
    }

    pub fn bootstrap_attempt_id(&self) -> Result<BootstrapAttemptId, ProtocolError> {
        decode_bootstrap_attempt_id(&self.bootstrap_attempt_id)
    }
}

/// Server → client direction: wraps the existing `HandshakeJson` rather
/// than adding fields to it (module docs). `isekai-pipe serve` (`#20a-4`)
/// writes this as one `write_all` call (content + trailing newline in a
/// single buffer) to the same inherited stdout the bare `HandshakeJson` line
/// already used — one write() syscall is enough to keep the calling shell's
/// `[ -s $tmpdir/handshake ]` poll loop from ever observing a partially
/// written line, so no separate temp-file/rename dance is needed here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapReportV2 {
    pub v: u16,
    pub session_id: String,
    pub bootstrap_attempt_id: String,
    pub handshake: HandshakeJson,
}

impl BootstrapReportV2 {
    pub fn session_id(&self) -> Result<SessionId, ProtocolError> {
        decode_session_id_hex(&self.session_id)
    }

    pub fn bootstrap_attempt_id(&self) -> Result<BootstrapAttemptId, ProtocolError> {
        decode_bootstrap_attempt_id(&self.bootstrap_attempt_id)
    }
}

/// Parses and validates a `BootstrapRequestV2`: size cap before handing the
/// bytes to `serde_json` (mirrors `decode_handshake_json`'s same
/// unbounded-allocation defense), version check, `session_id`/
/// `bootstrap_attempt_id` hex validity, candidate count/endpoint/validity
/// limits — **all-or-nothing**: a single malformed candidate rejects the
/// whole request rather than silently dropping just that entry.
pub fn decode_bootstrap_request_v2(bytes: &[u8]) -> Result<BootstrapRequestV2, ProtocolError> {
    if bytes.len() > MAX_BOOTSTRAP_REQUEST_JSON_LEN {
        return Err(ProtocolError::HandshakeTooLarge { got: bytes.len(), max: MAX_BOOTSTRAP_REQUEST_JSON_LEN });
    }
    let parsed: BootstrapRequestV2 =
        serde_json::from_slice(bytes).map_err(|e| ProtocolError::HandshakeJson(e.to_string()))?;
    validate_bootstrap_request_v2(&parsed)?;
    Ok(parsed)
}

pub fn validate_bootstrap_request_v2(request: &BootstrapRequestV2) -> Result<(), ProtocolError> {
    if request.v != BOOTSTRAP_PROTOCOL_V2 {
        return Err(ProtocolError::UnsupportedVersion {
            got: request.v as u32,
            min: BOOTSTRAP_PROTOCOL_V2 as u32,
            max: BOOTSTRAP_PROTOCOL_V2 as u32,
        });
    }
    request.session_id()?;
    request.bootstrap_attempt_id()?;

    if request.client_candidates.len() > MAX_CLIENT_CANDIDATES {
        return Err(ProtocolError::TooManyFeatures {
            declared: request.client_candidates.len(),
            max: MAX_CLIENT_CANDIDATES,
        });
    }
    for candidate in &request.client_candidates {
        if candidate.route.is_empty() {
            return Err(ProtocolError::HandshakeField {
                field: "client_candidates.route",
                reason: "must be non-empty".to_string(),
            });
        }
        if candidate.endpoint.is_empty() {
            return Err(ProtocolError::HandshakeField {
                field: "client_candidates.endpoint",
                reason: "must be non-empty".to_string(),
            });
        }
        let addr: std::net::SocketAddr = candidate.endpoint.parse().map_err(|_| ProtocolError::HandshakeField {
            field: "client_candidates.endpoint",
            reason: "must be a valid \"ip:port\" socket address".to_string(),
        })?;
        if addr.port() == 0 {
            return Err(ProtocolError::HandshakeField {
                field: "client_candidates.endpoint",
                reason: "port must be non-zero".to_string(),
            });
        }
        if addr.ip().is_multicast() {
            return Err(ProtocolError::HandshakeField {
                field: "client_candidates.endpoint",
                reason: "must not be a multicast address".to_string(),
            });
        }
        if addr.ip().is_unspecified() {
            return Err(ProtocolError::HandshakeField {
                field: "client_candidates.endpoint",
                reason: "must not be an unspecified address".to_string(),
            });
        }
    }
    Ok(())
}

/// Clamps a candidate's claimed `valid_for_ms` to `MAX_CANDIDATE_VALID_FOR_MS`
/// and converts it to a local deadline relative to `now` — the receiver's
/// own clock, never the client's claimed absolute time.
pub fn candidate_valid_until(candidate: &BootstrapCandidateV2, now: std::time::Instant) -> std::time::Instant {
    let clamped_ms = candidate.valid_for_ms.min(MAX_CANDIDATE_VALID_FOR_MS);
    now + std::time::Duration::from_millis(clamped_ms)
}

/// Generous cap for the whole `BootstrapReportV2` JSON — the envelope adds
/// only a handful of bytes over the inner `HandshakeJson`, so this mirrors
/// `handshake::MAX_HANDSHAKE_JSON_LEN` with headroom rather than defining an
/// independent budget.
pub const MAX_BOOTSTRAP_REPORT_JSON_LEN: usize = 8 * 1024;

/// Parses and validates a `BootstrapReportV2` (`#20a-4`): size cap, version
/// check, `session_id`/`bootstrap_attempt_id` hex validity, and the nested
/// `HandshakeJson` validated by its own existing
/// `handshake::validate_handshake` — same all-or-nothing contract as
/// [`decode_bootstrap_request_v2`].
pub fn decode_bootstrap_report_v2(bytes: &[u8]) -> Result<BootstrapReportV2, ProtocolError> {
    if bytes.len() > MAX_BOOTSTRAP_REPORT_JSON_LEN {
        return Err(ProtocolError::HandshakeTooLarge { got: bytes.len(), max: MAX_BOOTSTRAP_REPORT_JSON_LEN });
    }
    let parsed: BootstrapReportV2 =
        serde_json::from_slice(bytes).map_err(|e| ProtocolError::HandshakeJson(e.to_string()))?;
    validate_bootstrap_report_v2(&parsed)?;
    Ok(parsed)
}

pub fn validate_bootstrap_report_v2(report: &BootstrapReportV2) -> Result<(), ProtocolError> {
    if report.v != BOOTSTRAP_PROTOCOL_V2 {
        return Err(ProtocolError::UnsupportedVersion {
            got: report.v as u32,
            min: BOOTSTRAP_PROTOCOL_V2 as u32,
            max: BOOTSTRAP_PROTOCOL_V2 as u32,
        });
    }
    report.session_id()?;
    report.bootstrap_attempt_id()?;
    crate::handshake::validate_handshake(&report.handshake)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> BootstrapRequestV2 {
        BootstrapRequestV2 {
            v: BOOTSTRAP_PROTOCOL_V2,
            session_id: SessionId::from_bytes([0x11; SESSION_ID_LEN]).to_hex(),
            bootstrap_attempt_id: BootstrapAttemptId::from_bytes([0x22; BOOTSTRAP_ATTEMPT_ID_LEN]).to_hex(),
            client_candidates: vec![BootstrapCandidateV2 {
                route: "stun-p2p".to_string(),
                endpoint: "203.0.113.10:45000".to_string(),
                valid_for_ms: 30_000,
            }],
        }
    }

    #[test]
    fn request_roundtrips_through_json() {
        let request = sample_request();
        let bytes = serde_json::to_vec(&request).unwrap();
        let decoded = decode_bootstrap_request_v2(&bytes).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(decoded.session_id().unwrap(), SessionId::from_bytes([0x11; SESSION_ID_LEN]));
        assert_eq!(
            decoded.bootstrap_attempt_id().unwrap(),
            BootstrapAttemptId::from_bytes([0x22; BOOTSTRAP_ATTEMPT_ID_LEN])
        );
    }

    #[test]
    fn request_with_no_candidates_is_valid() {
        let mut request = sample_request();
        request.client_candidates.clear();
        let bytes = serde_json::to_vec(&request).unwrap();
        assert!(decode_bootstrap_request_v2(&bytes).is_ok());
    }

    #[test]
    fn rejects_oversized_request() {
        let mut bytes = serde_json::to_vec(&sample_request()).unwrap();
        bytes.extend(std::iter::repeat(b' ').take(MAX_BOOTSTRAP_REQUEST_JSON_LEN));
        let err = decode_bootstrap_request_v2(&bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeTooLarge { .. }));
    }

    #[test]
    fn rejects_wrong_version() {
        let mut request = sample_request();
        request.v = 1;
        let bytes = serde_json::to_vec(&request).unwrap();
        let err = decode_bootstrap_request_v2(&bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::UnsupportedVersion { .. }));
    }

    #[test]
    fn rejects_malformed_session_id() {
        let mut request = sample_request();
        request.session_id = "not-hex".to_string();
        let bytes = serde_json::to_vec(&request).unwrap();
        let err = decode_bootstrap_request_v2(&bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeField { field: "session_id", .. }));
    }

    #[test]
    fn rejects_too_many_candidates() {
        let mut request = sample_request();
        request.client_candidates =
            (0..MAX_CLIENT_CANDIDATES + 1).map(|i| BootstrapCandidateV2 {
                route: "stun-p2p".to_string(),
                endpoint: format!("203.0.113.10:{}", 40000 + i as u16),
                valid_for_ms: 1000,
            }).collect();
        let bytes = serde_json::to_vec(&request).unwrap();
        let err = decode_bootstrap_request_v2(&bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::TooManyFeatures { .. }));
    }

    #[test]
    fn rejects_zero_port_endpoint() {
        let mut request = sample_request();
        request.client_candidates[0].endpoint = "203.0.113.10:0".to_string();
        let bytes = serde_json::to_vec(&request).unwrap();
        let err = decode_bootstrap_request_v2(&bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeField { field: "client_candidates.endpoint", .. }));
    }

    #[test]
    fn rejects_unspecified_address() {
        let mut request = sample_request();
        request.client_candidates[0].endpoint = "0.0.0.0:1234".to_string();
        let bytes = serde_json::to_vec(&request).unwrap();
        let err = decode_bootstrap_request_v2(&bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeField { field: "client_candidates.endpoint", .. }));
    }

    #[test]
    fn rejects_multicast_address() {
        let mut request = sample_request();
        request.client_candidates[0].endpoint = "224.0.0.1:1234".to_string();
        let bytes = serde_json::to_vec(&request).unwrap();
        let err = decode_bootstrap_request_v2(&bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeField { field: "client_candidates.endpoint", .. }));
    }

    #[test]
    fn rejects_malformed_endpoint() {
        let mut request = sample_request();
        request.client_candidates[0].endpoint = "not-an-address".to_string();
        let bytes = serde_json::to_vec(&request).unwrap();
        let err = decode_bootstrap_request_v2(&bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeField { field: "client_candidates.endpoint", .. }));
    }

    #[test]
    fn bootstrap_attempt_id_debug_and_display_show_hex() {
        let id = BootstrapAttemptId::from_bytes([0xab; BOOTSTRAP_ATTEMPT_ID_LEN]);
        assert_eq!(id.to_hex(), "ab".repeat(BOOTSTRAP_ATTEMPT_ID_LEN));
        assert_eq!(format!("{id}"), id.to_hex());
        assert_eq!(format!("{id:?}"), format!("BootstrapAttemptId({})", id.to_hex()));
    }

    #[test]
    fn report_roundtrips_and_wraps_handshake_json_without_field_collision() {
        let handshake: HandshakeJson = serde_json::from_str(
            r#"{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}}}"#,
        )
        .unwrap();
        let report = BootstrapReportV2 {
            v: BOOTSTRAP_PROTOCOL_V2,
            session_id: SessionId::from_bytes([0x33; SESSION_ID_LEN]).to_hex(),
            bootstrap_attempt_id: BootstrapAttemptId::from_bytes([0x44; BOOTSTRAP_ATTEMPT_ID_LEN]).to_hex(),
            handshake: handshake.clone(),
        };
        let bytes = serde_json::to_vec(&report).unwrap();
        let decoded: BootstrapReportV2 = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, report);
        // The envelope's `v` (2) and the nested handshake's own `v` (1) are
        // independent fields at different JSON nesting levels — proves they
        // don't collide/overwrite each other during a real round-trip.
        assert_eq!(decoded.v, BOOTSTRAP_PROTOCOL_V2);
        assert_eq!(decoded.handshake.v, 1);
    }

    fn sample_report() -> BootstrapReportV2 {
        let handshake: HandshakeJson = serde_json::from_str(
            r#"{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}},"candidates":[{"kind":"direct-by-bootstrap-host","port":45231,"source":"bootstrap-ssh"}]}"#,
        )
        .unwrap();
        BootstrapReportV2 {
            v: BOOTSTRAP_PROTOCOL_V2,
            session_id: SessionId::from_bytes([0x33; SESSION_ID_LEN]).to_hex(),
            bootstrap_attempt_id: BootstrapAttemptId::from_bytes([0x44; BOOTSTRAP_ATTEMPT_ID_LEN]).to_hex(),
            handshake,
        }
    }

    #[test]
    fn decode_bootstrap_report_v2_accepts_a_valid_report() {
        let report = sample_report();
        let bytes = serde_json::to_vec(&report).unwrap();
        let decoded = decode_bootstrap_report_v2(&bytes).unwrap();
        assert_eq!(decoded, report);
        assert_eq!(decoded.session_id().unwrap(), SessionId::from_bytes([0x33; SESSION_ID_LEN]));
        assert_eq!(decoded.handshake.direct_by_bootstrap_host_port(), Some(45231));
    }

    #[test]
    fn decode_bootstrap_report_v2_rejects_oversized_report() {
        let mut bytes = serde_json::to_vec(&sample_report()).unwrap();
        bytes.extend(std::iter::repeat(b' ').take(MAX_BOOTSTRAP_REPORT_JSON_LEN));
        let err = decode_bootstrap_report_v2(&bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeTooLarge { .. }));
    }

    #[test]
    fn decode_bootstrap_report_v2_rejects_wrong_version() {
        let mut report = sample_report();
        report.v = 1;
        let bytes = serde_json::to_vec(&report).unwrap();
        let err = decode_bootstrap_report_v2(&bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::UnsupportedVersion { .. }));
    }

    #[test]
    fn decode_bootstrap_report_v2_rejects_malformed_session_id() {
        let mut report = sample_report();
        report.session_id = "not-hex".to_string();
        let bytes = serde_json::to_vec(&report).unwrap();
        let err = decode_bootstrap_report_v2(&bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeField { field: "session_id", .. }));
    }

    #[test]
    fn decode_bootstrap_report_v2_rejects_an_invalid_nested_handshake() {
        let mut report = sample_report();
        // `direct-by-bootstrap-host` requires a `port` — drop it so the
        // nested `HandshakeJson`'s own validation fails, proving
        // `validate_bootstrap_report_v2` actually delegates to it rather
        // than only checking the envelope fields.
        report.handshake.candidates[0].port = None;
        let bytes = serde_json::to_vec(&report).unwrap();
        let err = decode_bootstrap_report_v2(&bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeField { field: "candidates.port", .. }));
    }

    #[test]
    fn candidate_valid_until_clamps_to_the_maximum() {
        let now = std::time::Instant::now();
        let candidate = BootstrapCandidateV2 {
            route: "stun-p2p".to_string(),
            endpoint: "203.0.113.10:1".to_string(),
            valid_for_ms: MAX_CANDIDATE_VALID_FOR_MS * 100,
        };
        let deadline = candidate_valid_until(&candidate, now);
        assert_eq!(deadline, now + std::time::Duration::from_millis(MAX_CANDIDATE_VALID_FOR_MS));
    }
}
