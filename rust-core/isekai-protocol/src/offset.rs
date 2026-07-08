//! C2H/H2C stream-position offsets (`archive/ISEKAI_SSH_DESIGN.md` "resume を
//! ProxyCommand の背後に隠す"節). Naming and direction semantics follow that
//! section exactly; these are plain value types with overflow-checked
//! arithmetic. The resume frames that carry them on the wire (`RESUME`/
//! `RESUME_ACK`) live in the `resume` module (Phase S-4a).

use crate::error::ProtocolError;

macro_rules! offset_type {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(u64);

        impl $name {
            pub const ZERO: Self = Self(0);

            pub fn new(raw: u64) -> Self {
                Self(raw)
            }

            pub fn get(self) -> u64 {
                self.0
            }

            /// Advances the offset by `delta` bytes, rejecting the u64 overflow
            /// that a malicious or corrupted peer could otherwise trigger.
            pub fn checked_advance(self, delta: u64) -> Result<Self, ProtocolError> {
                self.0.checked_add(delta).map(Self).ok_or(ProtocolError::OffsetOverflow)
            }

            pub fn to_be_bytes(self) -> [u8; 8] {
                self.0.to_be_bytes()
            }

            pub fn from_be_bytes(bytes: [u8; 8]) -> Self {
                Self(u64::from_be_bytes(bytes))
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

offset_type!(
    C2hSentOffset,
    "isekai-ssh が送信済みの相手先端点オフセット（client→helper 方向）。"
);
offset_type!(
    C2hHelperCommittedOffset,
    "helper が sshd への TCP write に成功した地点（client→helper 方向の source of truth）。"
);
offset_type!(
    H2cSentOffset,
    "helper が送信済みのオフセット（helper→client 方向）。"
);
offset_type!(
    H2cClientDeliveredOffset,
    "isekai-ssh が自身の stdout への write_all に成功した地点（helper→client 方向の source of truth）。"
);

/// Decodes a fixed 8-byte big-endian offset field out of a wire buffer.
/// Used by higher layers (`isekai-transport`, `isekai-helper`) that frame
/// these offsets on the wire (e.g. an ack/progress message); this crate only
/// validates the byte layout, not the frame's meaning.
pub fn decode_offset_field(buf: &[u8]) -> Result<u64, ProtocolError> {
    let arr: [u8; 8] =
        buf.try_into().map_err(|_| ProtocolError::FrameLengthMismatch { got: buf.len(), expected: 8 })?;
    Ok(u64::from_be_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_advance_accumulates() {
        let a = C2hSentOffset::new(10);
        let b = a.checked_advance(5).unwrap();
        assert_eq!(b.get(), 15);
    }

    #[test]
    fn checked_advance_rejects_u64_overflow() {
        let near_max = H2cClientDeliveredOffset::new(u64::MAX - 3);
        let err = near_max.checked_advance(10).unwrap_err();
        assert_eq!(err, ProtocolError::OffsetOverflow);
    }

    #[test]
    fn decode_then_advance_rejects_overflow() {
        // A peer claiming to have already sent u64::MAX bytes, followed by any
        // further progress, must be rejected rather than silently wrapping.
        let raw = u64::MAX.to_be_bytes();
        let decoded = decode_offset_field(&raw).unwrap();
        let offset = C2hHelperCommittedOffset::new(decoded);
        assert_eq!(offset.checked_advance(1), Err(ProtocolError::OffsetOverflow));
    }

    #[test]
    fn decode_offset_field_rejects_wrong_length() {
        let err = decode_offset_field(&[0u8; 7]).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: 7, expected: 8 });
    }

    #[test]
    fn round_trips_be_bytes() {
        let o = H2cSentOffset::new(0x0102030405060708);
        assert_eq!(H2cSentOffset::from_be_bytes(o.to_be_bytes()), o);
    }

    #[test]
    fn ordering_reflects_progress() {
        assert!(C2hSentOffset::new(5) < C2hSentOffset::new(6));
    }
}
