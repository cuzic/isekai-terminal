//! isekai-helper's startup handshake JSON (`HELPER_PROTOCOL.md` §2), mirrored
//! from `rust-core/src/helper_bootstrap.rs::HelperHandshake`. This module
//! adds the field-level validation that the original `#[derive(Deserialize)]`
//! left to callers (length/format of `cert_sha256`/`session_secret`, a size
//! cap on the JSON itself) since bootstrap code must treat this line as
//! untrusted input coming back over an SSH exec channel.

use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

/// Generous cap for the one-line handshake JSON. The real payload is well
/// under 300 bytes; this only exists to reject a hostile/broken helper that
/// floods stdout instead of emitting the expected single line.
pub const MAX_HANDSHAKE_JSON_LEN: usize = 4096;

pub const CERT_SHA256_HEX_LEN: usize = 64;
pub const SESSION_SECRET_DECODED_LEN: usize = 32;
pub const SUPPORTED_HANDSHAKE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeJson {
    pub v: u32,
    pub listen_port: u16,
    pub cert_sha256: String,
    pub session_secret: String,
    #[serde(default)]
    pub stun_observed_addr: Option<String>,
    #[serde(default)]
    pub relay_public_addr: Option<String>,
}

/// Parses and validates one line of handshake JSON. Rejects oversized input
/// before handing it to `serde_json` so a hostile/broken helper can't force
/// an unbounded allocation.
pub fn decode_handshake_json(bytes: &[u8]) -> Result<HandshakeJson, ProtocolError> {
    if bytes.len() > MAX_HANDSHAKE_JSON_LEN {
        return Err(ProtocolError::HandshakeTooLarge { got: bytes.len(), max: MAX_HANDSHAKE_JSON_LEN });
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

    let is_lowercase_hex64 = h.cert_sha256.len() == CERT_SHA256_HEX_LEN
        && h.cert_sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !is_lowercase_hex64 {
        return Err(ProtocolError::HandshakeField {
            field: "cert_sha256",
            reason: format!("must be {CERT_SHA256_HEX_LEN} lowercase hex characters"),
        });
    }

    let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &h.session_secret)
        .map_err(|e| ProtocolError::HandshakeField { field: "session_secret", reason: e.to_string() })?;
    if decoded.len() != SESSION_SECRET_DECODED_LEN {
        return Err(ProtocolError::HandshakeField {
            field: "session_secret",
            reason: format!("decodes to {} bytes, expected {}", decoded.len(), SESSION_SECRET_DECODED_LEN),
        });
    }

    if h.listen_port == 0 {
        return Err(ProtocolError::HandshakeField { field: "listen_port", reason: "must be non-zero".to_string() });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_json() -> Vec<u8> {
        br#"{"v":1,"listen_port":45231,"cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb","session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=","stun_observed_addr":"203.0.113.5:45231","relay_public_addr":null}"#.to_vec()
    }

    #[test]
    fn decodes_valid_handshake() {
        let h = decode_handshake_json(&valid_json()).unwrap();
        assert_eq!(h.v, 1);
        assert_eq!(h.listen_port, 45231);
        assert_eq!(h.stun_observed_addr.as_deref(), Some("203.0.113.5:45231"));
        assert_eq!(h.relay_public_addr, None);
    }

    #[test]
    fn optional_fields_default_to_none_when_absent() {
        let json = br#"{"v":1,"listen_port":1,"cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb","session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE="}"#;
        let h = decode_handshake_json(json).unwrap();
        assert_eq!(h.stun_observed_addr, None);
        assert_eq!(h.relay_public_addr, None);
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
        let json = br#"{"v":99,"listen_port":1,"cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb","session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE="}"#;
        let err = decode_handshake_json(json).unwrap_err();
        assert_eq!(
            err,
            ProtocolError::UnsupportedVersion { got: 99, min: SUPPORTED_HANDSHAKE_VERSION, max: SUPPORTED_HANDSHAKE_VERSION }
        );
    }

    #[test]
    fn rejects_bad_cert_sha256_length() {
        let json = br#"{"v":1,"listen_port":1,"cert_sha256":"deadbeef","session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE="}"#;
        let err = decode_handshake_json(json).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeField { field: "cert_sha256", .. }));
    }

    #[test]
    fn rejects_uppercase_cert_sha256() {
        let json = br#"{"v":1,"listen_port":1,"cert_sha256":"3A7F000000000000000000000000000000000000000000000000000000AA","session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE="}"#;
        let err = decode_handshake_json(json).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeField { field: "cert_sha256", .. }));
    }

    #[test]
    fn rejects_bad_session_secret_encoding() {
        let json = br#"{"v":1,"listen_port":1,"cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb","session_secret":"not-base64!!"}"#;
        let err = decode_handshake_json(json).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeField { field: "session_secret", .. }));
    }

    #[test]
    fn rejects_session_secret_of_wrong_decoded_length() {
        let json = br#"{"v":1,"listen_port":1,"cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb","session_secret":"YWJj"}"#;
        let err = decode_handshake_json(json).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeField { field: "session_secret", .. }));
    }

    #[test]
    fn rejects_zero_listen_port() {
        let json = br#"{"v":1,"listen_port":0,"cert_sha256":"3a7f00000000000000000000000000000000000000000000000000000000aabb","session_secret":"MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE="}"#;
        let err = decode_handshake_json(json).unwrap_err();
        assert!(matches!(err, ProtocolError::HandshakeField { field: "listen_port", .. }));
    }
}
