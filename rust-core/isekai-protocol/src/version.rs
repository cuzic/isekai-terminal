//! Version negotiation, new for the isekai-ssh work (not present in the
//! current `HELPER_PROTOCOL.md` wire format). `ISEKAI_SSH_DESIGN.md`
//! "実装方針" calls for `protocol_version`/`min_supported_version`/`features`
//! to exist from the start so Android/CLI/isekai-helper/e2e tests can all
//! speak the same vocabulary once `isekai-transport` wires this up.

use crate::error::ProtocolError;

pub const CURRENT_PROTOCOL_VERSION: u16 = 1;
pub const MIN_SUPPORTED_PROTOCOL_VERSION: u16 = 1;

pub const FRAME_VERSION_NEGOTIATION: u8 = 0x20;

pub const MAX_FEATURES: usize = 32;
pub const MAX_FEATURE_NAME_LEN: usize = 64;
/// Upper bound on the whole encoded frame (type byte + two version fields +
/// feature count + every feature's length-prefixed name at its own cap).
pub const MAX_VERSION_FRAME_LEN: usize = 1 + 2 + 2 + 2 + MAX_FEATURES * (2 + MAX_FEATURE_NAME_LEN);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    pub protocol_version: u16,
    pub min_supported_version: u16,
    pub features: Vec<String>,
}

impl VersionInfo {
    pub fn current(features: Vec<String>) -> Self {
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            min_supported_version: MIN_SUPPORTED_PROTOCOL_VERSION,
            features,
        }
    }

    /// Two peers can talk if their supported-version ranges overlap.
    pub fn is_compatible_with(&self, other: &VersionInfo) -> bool {
        self.protocol_version >= other.min_supported_version
            && other.protocol_version >= self.min_supported_version
    }

    pub fn supports(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }
}

pub fn encode_version_info(v: &VersionInfo) -> Result<Vec<u8>, ProtocolError> {
    if v.features.len() > MAX_FEATURES {
        return Err(ProtocolError::TooManyFeatures { declared: v.features.len(), max: MAX_FEATURES });
    }
    let mut payload = Vec::new();
    payload.extend_from_slice(&v.protocol_version.to_be_bytes());
    payload.extend_from_slice(&v.min_supported_version.to_be_bytes());
    payload.extend_from_slice(&(v.features.len() as u16).to_be_bytes());
    for f in &v.features {
        if f.len() > MAX_FEATURE_NAME_LEN {
            return Err(ProtocolError::FrameTooLarge { declared: f.len(), max: MAX_FEATURE_NAME_LEN });
        }
        payload.extend_from_slice(&(f.len() as u16).to_be_bytes());
        payload.extend_from_slice(f.as_bytes());
    }
    let mut buf = Vec::with_capacity(1 + payload.len());
    buf.push(FRAME_VERSION_NEGOTIATION);
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Defensive decode: rejects frames that are too short/long overall, an
/// unknown type byte, a feature count over `MAX_FEATURES`, any single
/// feature name over `MAX_FEATURE_NAME_LEN`, and truncated buffers where a
/// declared length runs past the data actually available. All length
/// bookkeeping uses `checked_add` so a crafted buffer can't make the cursor
/// wrap around instead of erroring.
pub fn decode_version_info(buf: &[u8]) -> Result<VersionInfo, ProtocolError> {
    if buf.len() > MAX_VERSION_FRAME_LEN {
        return Err(ProtocolError::FrameTooLarge { declared: buf.len(), max: MAX_VERSION_FRAME_LEN });
    }
    const HEADER_LEN: usize = 7;
    if buf.len() < HEADER_LEN {
        return Err(ProtocolError::FrameIncomplete { declared: HEADER_LEN, available: buf.len() });
    }
    if buf[0] != FRAME_VERSION_NEGOTIATION {
        return Err(ProtocolError::UnknownFrameType(buf[0]));
    }
    let protocol_version = u16::from_be_bytes([buf[1], buf[2]]);
    let min_supported_version = u16::from_be_bytes([buf[3], buf[4]]);
    let feature_count = u16::from_be_bytes([buf[5], buf[6]]) as usize;
    if feature_count > MAX_FEATURES {
        return Err(ProtocolError::TooManyFeatures { declared: feature_count, max: MAX_FEATURES });
    }

    let mut pos = HEADER_LEN;
    let mut features = Vec::with_capacity(feature_count);
    for _ in 0..feature_count {
        let len_end = pos
            .checked_add(2)
            .ok_or(ProtocolError::FrameTooLarge { declared: usize::MAX, max: MAX_VERSION_FRAME_LEN })?;
        if len_end > buf.len() {
            return Err(ProtocolError::FrameIncomplete { declared: len_end, available: buf.len() });
        }
        let flen = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
        if flen > MAX_FEATURE_NAME_LEN {
            return Err(ProtocolError::FrameTooLarge { declared: flen, max: MAX_FEATURE_NAME_LEN });
        }
        let str_end = len_end
            .checked_add(flen)
            .ok_or(ProtocolError::FrameTooLarge { declared: usize::MAX, max: MAX_VERSION_FRAME_LEN })?;
        if str_end > buf.len() {
            return Err(ProtocolError::FrameIncomplete { declared: str_end, available: buf.len() });
        }
        let s = std::str::from_utf8(&buf[len_end..str_end])
            .map_err(|_| ProtocolError::HandshakeField { field: "features", reason: "invalid utf-8".to_string() })?;
        features.push(s.to_string());
        pos = str_end;
    }

    if pos != buf.len() {
        return Err(ProtocolError::FrameLengthMismatch { got: buf.len(), expected: pos });
    }

    Ok(VersionInfo { protocol_version, min_supported_version, features })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_with_features() {
        let v = VersionInfo::current(vec!["resume".to_string(), "multipath".to_string()]);
        let encoded = encode_version_info(&v).unwrap();
        assert_eq!(decode_version_info(&encoded).unwrap(), v);
    }

    #[test]
    fn roundtrips_with_no_features() {
        let v = VersionInfo::current(vec![]);
        let encoded = encode_version_info(&v).unwrap();
        assert_eq!(decode_version_info(&encoded).unwrap(), v);
    }

    #[test]
    fn decode_rejects_unknown_frame_type() {
        let mut buf = vec![0x99u8];
        buf.extend_from_slice(&[0u8; 6]);
        let err = decode_version_info(&buf).unwrap_err();
        assert_eq!(err, ProtocolError::UnknownFrameType(0x99));
    }

    #[test]
    fn decode_rejects_truncated_header() {
        let buf = vec![FRAME_VERSION_NEGOTIATION, 0, 1];
        let err = decode_version_info(&buf).unwrap_err();
        assert_eq!(err, ProtocolError::FrameIncomplete { declared: 7, available: 3 });
    }

    #[test]
    fn decode_rejects_truncated_feature_body() {
        // Header claims one feature of length 10, but the buffer ends early.
        let mut buf = vec![FRAME_VERSION_NEGOTIATION];
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&10u16.to_be_bytes());
        buf.extend_from_slice(b"short");
        let err = decode_version_info(&buf).unwrap_err();
        assert!(matches!(err, ProtocolError::FrameIncomplete { .. }));
    }

    #[test]
    fn decode_rejects_too_many_features() {
        let mut buf = vec![FRAME_VERSION_NEGOTIATION];
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&((MAX_FEATURES + 1) as u16).to_be_bytes());
        let err = decode_version_info(&buf).unwrap_err();
        assert_eq!(err, ProtocolError::TooManyFeatures { declared: MAX_FEATURES + 1, max: MAX_FEATURES });
    }

    #[test]
    fn decode_rejects_oversized_feature_name() {
        let mut buf = vec![FRAME_VERSION_NEGOTIATION];
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&1u16.to_be_bytes());
        // Declares a feature name far longer than MAX_FEATURE_NAME_LEN, and
        // far longer than the buffer itself — must be rejected before any
        // attempt to read that many bytes.
        buf.extend_from_slice(&u16::MAX.to_be_bytes());
        let err = decode_version_info(&buf).unwrap_err();
        assert_eq!(err, ProtocolError::FrameTooLarge { declared: u16::MAX as usize, max: MAX_FEATURE_NAME_LEN });
    }

    #[test]
    fn decode_rejects_huge_overall_frame() {
        let huge = vec![FRAME_VERSION_NEGOTIATION; MAX_VERSION_FRAME_LEN + 1];
        let err = decode_version_info(&huge).unwrap_err();
        assert_eq!(err, ProtocolError::FrameTooLarge { declared: MAX_VERSION_FRAME_LEN + 1, max: MAX_VERSION_FRAME_LEN });
    }

    #[test]
    fn encode_rejects_too_many_features() {
        let v = VersionInfo::current((0..MAX_FEATURES + 1).map(|i| i.to_string()).collect());
        let err = encode_version_info(&v).unwrap_err();
        assert_eq!(err, ProtocolError::TooManyFeatures { declared: MAX_FEATURES + 1, max: MAX_FEATURES });
    }

    #[test]
    fn compatibility_requires_overlapping_ranges() {
        let a = VersionInfo { protocol_version: 3, min_supported_version: 2, features: vec![] };
        let b = VersionInfo { protocol_version: 2, min_supported_version: 2, features: vec![] };
        assert!(a.is_compatible_with(&b));

        let c = VersionInfo { protocol_version: 1, min_supported_version: 1, features: vec![] };
        assert!(!a.is_compatible_with(&c));
    }
}
