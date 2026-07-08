//! isekai-helper's startup handshake JSON (`archive/HELPER_PROTOCOL.md` §2), mirrored
//! from `rust-core/src/helper_bootstrap.rs::IsekaiPipeHandshake`. This module
//! adds the field-level validation that the original `#[derive(Deserialize)]`
//! left to callers (length/format of `cert_sha256`/`session_secret`, a size
//! cap on the JSON itself) since bootstrap code must treat this line as
//! untrusted input coming back over an SSH exec channel.
//!
//! Single schema, no legacy/flat duplicate fields: identity lives under
//! `peer.server_identity`, and every reachability fact (the direct QUIC
//! port, a STUN-observed address, a relay-assigned address) is a
//! `candidates` entry rather than its own top-level field. There is exactly
//! one representation of each fact to keep in sync (`archive/ISEKAI_PIPE_MIGRATION.md`
//! P5 "旧名整理").

use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

/// Generous cap for the one-line handshake JSON. The real payload is well
/// under 300 bytes; this only exists to reject a hostile/broken helper that
/// floods stdout instead of emitting the expected single line.
pub const MAX_HANDSHAKE_JSON_LEN: usize = 4096;

pub const CERT_SHA256_HEX_LEN: usize = 64;
pub const SESSION_SECRET_DECODED_LEN: usize = 32;
pub const SUPPORTED_HANDSHAKE_VERSION: u32 = 1;
pub const CANDIDATE_DIRECT_BY_BOOTSTRAP_HOST: &str = "direct-by-bootstrap-host";
pub const CANDIDATE_SERVER_REFLEXIVE: &str = "server-reflexive";
pub const CANDIDATE_RELAYED: &str = "relayed";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeJson {
    pub v: u32,
    pub session_secret: String,
    pub protocol: HandshakeProtocol,
    pub peer: HandshakePeer,
    #[serde(default)]
    pub services: Vec<HandshakeService>,
    #[serde(default)]
    pub candidates: Vec<HandshakeCandidate>,
}

/// Logical wire protocol served by this process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeProtocol {
    pub name: String,
    pub alpn: String,
}

/// Peer identity introduced by the bootstrap channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakePeer {
    #[serde(default)]
    pub peer_id: Option<String>,
    pub server_identity: ServerIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerIdentity {
    pub kind: String,
    pub cert_sha256: String,
}

/// A named service exposed by serve-side policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeService {
    pub name: String,
    pub target: String,
}

/// A runtime reachability candidate advertised by the serve side.
///
/// `direct-by-bootstrap-host` intentionally carries only a port: the client
/// already knows the SSH bootstrap host, while the helper/serve process does
/// not know which address the client used to reach it through ProxyJump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeCandidate {
    pub kind: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub source: Option<String>,
}

impl HandshakeJson {
    /// `peer.server_identity.cert_sha256`, the sole source of the QUIC
    /// certificate fingerprint clients pin against.
    pub fn cert_sha256(&self) -> &str {
        &self.peer.server_identity.cert_sha256
    }

    /// Port for the `direct-by-bootstrap-host` candidate, if advertised.
    pub fn direct_by_bootstrap_host_port(&self) -> Option<u16> {
        self.candidate_port(CANDIDATE_DIRECT_BY_BOOTSTRAP_HOST)
    }

    /// Endpoint for the `server-reflexive` (STUN-observed) candidate, if
    /// advertised.
    pub fn stun_observed_addr(&self) -> Option<&str> {
        self.candidate_endpoint(CANDIDATE_SERVER_REFLEXIVE)
    }

    /// Endpoint for the `relayed` candidate, if advertised.
    pub fn relay_public_addr(&self) -> Option<&str> {
        self.candidate_endpoint(CANDIDATE_RELAYED)
    }

    fn candidate_port(&self, kind: &str) -> Option<u16> {
        self.candidates
            .iter()
            .find(|candidate| candidate.kind == kind)
            .and_then(|candidate| candidate.port)
    }

    fn candidate_endpoint(&self, kind: &str) -> Option<&str> {
        self.candidates
            .iter()
            .find(|candidate| candidate.kind == kind)
            .and_then(|candidate| candidate.endpoint.as_deref())
    }
}

/// Parses and validates one line of handshake JSON. Rejects oversized input
/// before handing it to `serde_json` so a hostile/broken helper can't force
/// an unbounded allocation.
pub fn decode_handshake_json(bytes: &[u8]) -> Result<HandshakeJson, ProtocolError> {
    if bytes.len() > MAX_HANDSHAKE_JSON_LEN {
        return Err(ProtocolError::HandshakeTooLarge {
            got: bytes.len(),
            max: MAX_HANDSHAKE_JSON_LEN,
        });
    }
    let parsed: HandshakeJson =
        serde_json::from_slice(bytes).map_err(|e| ProtocolError::HandshakeJson(e.to_string()))?;
    validate_handshake(&parsed)?;
    Ok(parsed)
}

pub fn validate_handshake(h: &HandshakeJson) -> Result<(), ProtocolError> {
    if h.v != SUPPORTED_HANDSHAKE_VERSION {
        return Err(ProtocolError::UnsupportedVersion {
            got: h.v,
            min: SUPPORTED_HANDSHAKE_VERSION,
            max: SUPPORTED_HANDSHAKE_VERSION,
        });
    }

    validate_non_empty("protocol.name", &h.protocol.name)?;
    validate_non_empty("protocol.alpn", &h.protocol.alpn)?;

    if let Some(peer_id) = &h.peer.peer_id {
        validate_non_empty("peer.peer_id", peer_id)?;
    }
    validate_non_empty("peer.server_identity.kind", &h.peer.server_identity.kind)?;

    let cert_sha256 = &h.peer.server_identity.cert_sha256;
    let is_lowercase_hex64 = cert_sha256.len() == CERT_SHA256_HEX_LEN
        && cert_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !is_lowercase_hex64 {
        return Err(ProtocolError::HandshakeField {
            field: "peer.server_identity.cert_sha256",
            reason: format!("must be {CERT_SHA256_HEX_LEN} lowercase hex characters"),
        });
    }

    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        &h.session_secret,
    )
    .map_err(|e| ProtocolError::HandshakeField {
        field: "session_secret",
        reason: e.to_string(),
    })?;
    if decoded.len() != SESSION_SECRET_DECODED_LEN {
        return Err(ProtocolError::HandshakeField {
            field: "session_secret",
            reason: format!(
                "decodes to {} bytes, expected {}",
                decoded.len(),
                SESSION_SECRET_DECODED_LEN
            ),
        });
    }

    for service in &h.services {
        validate_non_empty("services.name", &service.name)?;
        validate_non_empty("services.target", &service.target)?;
    }

    for candidate in &h.candidates {
        validate_non_empty("candidates.kind", &candidate.kind)?;
        if let Some(endpoint) = &candidate.endpoint {
            validate_non_empty("candidates.endpoint", endpoint)?;
        }
        if let Some(port) = candidate.port {
            if port == 0 {
                return Err(ProtocolError::HandshakeField {
                    field: "candidates.port",
                    reason: "must be non-zero".to_string(),
                });
            }
        }
        match candidate.kind.as_str() {
            CANDIDATE_DIRECT_BY_BOOTSTRAP_HOST => {
                if candidate.port.is_none() {
                    return Err(ProtocolError::HandshakeField {
                        field: "candidates.port",
                        reason: "direct-by-bootstrap-host requires port".to_string(),
                    });
                }
            }
            CANDIDATE_SERVER_REFLEXIVE | CANDIDATE_RELAYED => {
                if candidate.endpoint.is_none() {
                    return Err(ProtocolError::HandshakeField {
                        field: "candidates.endpoint",
                        reason: format!("{} requires endpoint", candidate.kind),
                    });
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn validate_non_empty(field: &'static str, value: &str) -> Result<(), ProtocolError> {
    if value.is_empty() {
        return Err(ProtocolError::HandshakeField {
            field,
            reason: "must be non-empty".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_json() -> Vec<u8> {
        br#"{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}},"candidates":[{"kind":"direct-by-bootstrap-host","port":45231,"source":"bootstrap-ssh"},{"kind":"server-reflexive","endpoint":"203.0.113.5:45231","source":"stun"}]}"#.to_vec()
    }

    #[test]
    fn decodes_valid_handshake() {
        let h = decode_handshake_json(&valid_json()).unwrap();
        assert_eq!(h.v, 1);
        assert_eq!(h.direct_by_bootstrap_host_port(), Some(45231));
        assert_eq!(h.stun_observed_addr(), Some("203.0.113.5:45231"));
        assert_eq!(h.relay_public_addr(), None);
        assert_eq!(
            h.cert_sha256(),
            "3a7f00000000000000000000000000000000000000000000000000000000aabb"
        );
    }

    #[test]
    fn optional_fields_default_to_empty_when_absent() {
        let json = br#"{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}}}"#;
        let h = decode_handshake_json(json).unwrap();
        assert_eq!(h.direct_by_bootstrap_host_port(), None);
        assert_eq!(h.stun_observed_addr(), None);
        assert_eq!(h.relay_public_addr(), None);
        assert!(h.services.is_empty());
        assert!(h.candidates.is_empty());
    }

    #[test]
    fn decodes_relayed_candidate() {
        let json = br#"{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}},"services":[{"name":"ssh","target":"127.0.0.1:22"}],"candidates":[{"kind":"relayed","endpoint":"203.0.113.9:45900","source":"isekai-link-relay"}]}"#;
        let h = decode_handshake_json(json).unwrap();
        assert_eq!(h.services[0].name, "ssh");
        assert_eq!(h.relay_public_addr(), Some("203.0.113.9:45900"));
    }

    #[test]
    fn rejects_direct_by_bootstrap_host_candidate_without_port() {
        let json = br#"{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}},"candidates":[{"kind":"direct-by-bootstrap-host","source":"bootstrap-ssh"}]}"#;
        let err = decode_handshake_json(json).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::HandshakeField {
                field: "candidates.port",
                ..
            }
        ));
    }

    #[test]
    fn rejects_relayed_candidate_without_endpoint() {
        let json = br#"{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}},"candidates":[{"kind":"relayed","source":"isekai-link-relay"}]}"#;
        let err = decode_handshake_json(json).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::HandshakeField {
                field: "candidates.endpoint",
                ..
            }
        ));
    }

    #[test]
    fn rejects_oversized_json() {
        let mut json = valid_json();
        json.extend(std::iter::repeat(b' ').take(MAX_HANDSHAKE_JSON_LEN));
        let err = decode_handshake_json(&json).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeTooLarge { .. }));
    }

    #[test]
    fn rejects_malformed_json() {
        let err = decode_handshake_json(b"not json").unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeJson(_)));
    }

    #[test]
    fn rejects_unsupported_version() {
        let json = br#"{"v":99,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}}}"#;
        let err = decode_handshake_json(json).unwrap_err();
        assert_eq!(
            err,
            ProtocolError::UnsupportedVersion {
                got: 99,
                min: SUPPORTED_HANDSHAKE_VERSION,
                max: SUPPORTED_HANDSHAKE_VERSION
            }
        );
    }

    #[test]
    fn rejects_bad_cert_sha256_length() {
        let json = br#"{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"deadbeef"}}}"#;
        let err = decode_handshake_json(json).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::HandshakeField {
                field: "peer.server_identity.cert_sha256",
                ..
            }
        ));
    }

    #[test]
    fn rejects_uppercase_cert_sha256() {
        let json = br#"{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3A7F000000000000000000000000000000000000000000000000000000AA"}}}"#;
        let err = decode_handshake_json(json).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::HandshakeField {
                field: "peer.server_identity.cert_sha256",
                ..
            }
        ));
    }

    #[test]
    fn rejects_bad_session_secret_encoding() {
        let json = br#"{"v":1,"session_secret":"not-base64!!","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}}}"#;
        let err = decode_handshake_json(json).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::HandshakeField {
                field: "session_secret",
                ..
            }
        ));
    }

    #[test]
    fn rejects_session_secret_of_wrong_decoded_length() {
        let json = br#"{"v":1,"session_secret":"YWJj","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}}}"#;
        let err = decode_handshake_json(json).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::HandshakeField {
                field: "session_secret",
                ..
            }
        ));
    }

    #[test]
    fn rejects_zero_candidate_port() {
        let json = br#"{"v":1,"session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","protocol":{"name":"isekai-pipe","alpn":"isekai-pipe/1"},"peer":{"server_identity":{"kind":"quic-cert-sha256","cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb"}},"candidates":[{"kind":"direct-by-bootstrap-host","port":0,"source":"bootstrap-ssh"}]}"#;
        let err = decode_handshake_json(json).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::HandshakeField {
                field: "candidates.port",
                ..
            }
        ));
    }
}
