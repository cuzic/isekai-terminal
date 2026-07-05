//! `RESUME`/`RESUME_ACK` frames (`HELPER_PROTOCOL.md` §7.3), byte-for-byte
//! compatible with the wire format already implemented (as `pub(crate)`,
//! non-shareable outside `isekai-terminal-core`) by `rust-core/src/resume_client.rs`
//! and the `reattach_fn` closures in `helper_quic_transport.rs` /
//! `isekai_link_relay_transport.rs` / `isekai_stun_p2p_transport.rs`. This
//! module reimplements only the byte layout as a new, shareable type —
//! `rust-core/src/` itself is intentionally left untouched (`ISEKAI_SSH_DESIGN.md`
//! Phase S-4a scope: `isekai-terminal-core` migrating onto these types is future work).
//!
//! Per `ISEKAI_SSH_DESIGN.md` "resume を ProxyCommand の背後に隠す" /
//! "session_id は識別子であって認証情報ではない", the `RESUME` frame carries
//! a `resume_proof` alongside `session_id` specifically to prevent session
//! hijacking by anyone who merely guesses/observes a `session_id`. That
//! `resume_proof` field already exists in the current wire format (see
//! `helper_quic_transport.rs`'s `reattach_fn`, which builds
//! `[RESUME] || session_id || resume_proof || client_sent_offset ||
//! client_delivered_offset`) — this module does not need to invent a new
//! field for it, only a type. `resume_proof` computation itself
//! (`HMAC-SHA256(session_secret, exporter || session_id)`) needs a live QUIC
//! connection's exporter and stays out of this I/O-free crate, in
//! `isekai-transport`/`isekai-terminal-core`; here `ResumeProof` is deliberately just an
//! opaque 32-byte value, the same treatment `hello::Proof` gets for the
//! initial HELLO proof.

use crate::error::ProtocolError;
use crate::hello::FRAME_REJECT_AUTH;
use crate::offset::{C2hHelperCommittedOffset, C2hSentOffset, H2cClientDeliveredOffset, H2cSentOffset};
use crate::session_id::{decode_session_id, SessionId, SESSION_ID_LEN};

/// `HELPER_PROTOCOL.md` §4 reserves this value for `RESUME` ahead of time; it
/// is not adjacent to the HELLO/ACK frame bytes (`0x01`/`0x02`).
pub const FRAME_RESUME: u8 = 0x03;
pub const FRAME_RESUME_ACK: u8 = 0x13;

/// `RESUME` rejection reasons (`HELPER_PROTOCOL.md` §7.3 "RESUME の拒否応答").
/// `Auth` deliberately reuses `hello::FRAME_REJECT_AUTH` (`0xFF`) rather than
/// minting a new byte — the spec explicitly re-purposes the existing
/// HELLO/ACK reject vocabulary for resume's proof check, since both mean
/// "the presented proof did not match".
pub const FRAME_RESUME_REJECT_AUTH: u8 = FRAME_REJECT_AUTH;
pub const FRAME_RESUME_REJECT_UNKNOWN_SESSION: u8 = 0xF9;
pub const FRAME_RESUME_REJECT_OFFSET_GONE: u8 = 0xF8;

pub const RESUME_PROOF_LEN: usize = 32;

/// `1` (type byte) + `session_id` + `resume_proof` + two `u64` offsets.
pub const RESUME_FRAME_LEN: usize = 1 + SESSION_ID_LEN + RESUME_PROOF_LEN + 8 + 8;
/// `1` (type byte) + two `u64` offsets.
pub const RESUME_ACK_FRAME_LEN: usize = 1 + 8 + 8;

/// `resume_proof = HMAC-SHA256(session_secret, exporter || session_id)`
/// (`HELPER_PROTOCOL.md` §7.3). Computing this needs the live QUIC
/// connection's `export_keying_material`, which this I/O-free crate has no
/// access to — so, like `hello::Proof`, it is modeled here only as an opaque
/// 32-byte value. `Debug` hides the bytes so a stray `{:?}` in a log line
/// can't leak it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ResumeProof([u8; RESUME_PROOF_LEN]);

impl ResumeProof {
    pub fn new(bytes: [u8; RESUME_PROOF_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; RESUME_PROOF_LEN] {
        &self.0
    }

    /// Constant-time comparison, mirroring `hello::Proof::ct_eq` — resume
    /// proof checks are exactly as security-sensitive as the initial HELLO
    /// proof check (`HELPER_PROTOCOL.md` §4 constant-time mandate applies
    /// equally here per §7.3).
    pub fn ct_eq(&self, other: &ResumeProof) -> bool {
        let mut diff = 0u8;
        for (a, b) in self.0.iter().zip(other.0.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

impl std::fmt::Debug for ResumeProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResumeProof(..)")
    }
}

/// `RESUME` request (client → helper, new QUIC connection's control stream
/// head; `HELPER_PROTOCOL.md` §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeFrame {
    pub session_id: SessionId,
    pub resume_proof: ResumeProof,
    pub client_sent_offset: C2hSentOffset,
    pub client_delivered_offset: H2cClientDeliveredOffset,
}

/// Encodes `[[FRAME_RESUME] || session_id || resume_proof ||
/// client_sent_offset || client_delivered_offset]`, matching
/// `helper_quic_transport.rs`'s `reattach_fn` byte-for-byte.
pub fn encode_resume(frame: &ResumeFrame) -> Vec<u8> {
    let mut buf = Vec::with_capacity(RESUME_FRAME_LEN);
    buf.push(FRAME_RESUME);
    buf.extend_from_slice(frame.session_id.as_bytes());
    buf.extend_from_slice(frame.resume_proof.as_bytes());
    buf.extend_from_slice(&frame.client_sent_offset.to_be_bytes());
    buf.extend_from_slice(&frame.client_delivered_offset.to_be_bytes());
    debug_assert_eq!(buf.len(), RESUME_FRAME_LEN);
    buf
}

/// Defensive decode: rejects any length other than exactly
/// `RESUME_FRAME_LEN` and any type byte other than `FRAME_RESUME`.
pub fn decode_resume(buf: &[u8]) -> Result<ResumeFrame, ProtocolError> {
    if buf.len() != RESUME_FRAME_LEN {
        return Err(ProtocolError::FrameLengthMismatch { got: buf.len(), expected: RESUME_FRAME_LEN });
    }
    if buf[0] != FRAME_RESUME {
        return Err(ProtocolError::UnknownFrameType(buf[0]));
    }

    let mut pos = 1;
    let session_id = decode_session_id(&buf[pos..pos + SESSION_ID_LEN])?;
    pos += SESSION_ID_LEN;

    let mut proof_bytes = [0u8; RESUME_PROOF_LEN];
    proof_bytes.copy_from_slice(&buf[pos..pos + RESUME_PROOF_LEN]);
    pos += RESUME_PROOF_LEN;

    let client_sent_offset = C2hSentOffset::from_be_bytes(buf[pos..pos + 8].try_into().unwrap());
    pos += 8;
    let client_delivered_offset = H2cClientDeliveredOffset::from_be_bytes(buf[pos..pos + 8].try_into().unwrap());
    pos += 8;
    debug_assert_eq!(pos, RESUME_FRAME_LEN);

    Ok(ResumeFrame { session_id, resume_proof: ResumeProof::new(proof_bytes), client_sent_offset, client_delivered_offset })
}

/// `RESUME_ACK` response (helper → client; `HELPER_PROTOCOL.md` §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeAckFrame {
    pub helper_committed_offset: C2hHelperCommittedOffset,
    pub helper_sent_offset: H2cSentOffset,
}

/// Encodes `[[FRAME_RESUME_ACK] || helper_committed_offset ||
/// helper_sent_offset]`, matching the response body
/// `helper_quic_transport.rs`'s `reattach_fn` reads byte-for-byte.
pub fn encode_resume_ack(frame: &ResumeAckFrame) -> Vec<u8> {
    let mut buf = Vec::with_capacity(RESUME_ACK_FRAME_LEN);
    buf.push(FRAME_RESUME_ACK);
    buf.extend_from_slice(&frame.helper_committed_offset.to_be_bytes());
    buf.extend_from_slice(&frame.helper_sent_offset.to_be_bytes());
    buf
}

/// Defensive decode: rejects any length other than exactly
/// `RESUME_ACK_FRAME_LEN` and any type byte other than `FRAME_RESUME_ACK`.
pub fn decode_resume_ack(buf: &[u8]) -> Result<ResumeAckFrame, ProtocolError> {
    if buf.len() != RESUME_ACK_FRAME_LEN {
        return Err(ProtocolError::FrameLengthMismatch { got: buf.len(), expected: RESUME_ACK_FRAME_LEN });
    }
    if buf[0] != FRAME_RESUME_ACK {
        return Err(ProtocolError::UnknownFrameType(buf[0]));
    }
    let helper_committed_offset = C2hHelperCommittedOffset::from_be_bytes(buf[1..9].try_into().unwrap());
    let helper_sent_offset = H2cSentOffset::from_be_bytes(buf[9..17].try_into().unwrap());
    Ok(ResumeAckFrame { helper_committed_offset, helper_sent_offset })
}

/// Why a `RESUME` request was rejected (`HELPER_PROTOCOL.md` §7.3). Each
/// variant is a single-byte response with no body, unlike `RESUME_ACK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeRejectReason {
    /// `resume_proof` が不正 (`0xFF`, reused from HELLO/ACK's `REJECT_AUTH`).
    Auth,
    /// `session_id` が存在しない — helper 再起動・タイムアウト等 (`0xF9`).
    UnknownSession,
    /// 要求された offset が既に helper 側バッファの範囲外 (`0xF8`).
    OffsetGone,
}

pub fn encode_resume_reject(reason: ResumeRejectReason) -> u8 {
    match reason {
        ResumeRejectReason::Auth => FRAME_RESUME_REJECT_AUTH,
        ResumeRejectReason::UnknownSession => FRAME_RESUME_REJECT_UNKNOWN_SESSION,
        ResumeRejectReason::OffsetGone => FRAME_RESUME_REJECT_OFFSET_GONE,
    }
}

pub fn decode_resume_reject(byte: u8) -> Result<ResumeRejectReason, ProtocolError> {
    match byte {
        FRAME_RESUME_REJECT_AUTH => Ok(ResumeRejectReason::Auth),
        FRAME_RESUME_REJECT_UNKNOWN_SESSION => Ok(ResumeRejectReason::UnknownSession),
        FRAME_RESUME_REJECT_OFFSET_GONE => Ok(ResumeRejectReason::OffsetGone),
        other => Err(ProtocolError::UnknownFrameType(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_frame() -> ResumeFrame {
        ResumeFrame {
            session_id: SessionId::from_bytes([0x42u8; SESSION_ID_LEN]),
            resume_proof: ResumeProof::new([0x99u8; RESUME_PROOF_LEN]),
            client_sent_offset: C2hSentOffset::new(123_456),
            client_delivered_offset: H2cClientDeliveredOffset::new(654_321),
        }
    }

    #[test]
    fn resume_roundtrips() {
        let frame = sample_frame();
        let encoded = encode_resume(&frame);
        assert_eq!(encoded.len(), RESUME_FRAME_LEN);
        assert_eq!(decode_resume(&encoded).unwrap(), frame);
    }

    #[test]
    fn resume_ack_roundtrips() {
        let frame = ResumeAckFrame {
            helper_committed_offset: C2hHelperCommittedOffset::new(111),
            helper_sent_offset: H2cSentOffset::new(222),
        };
        let encoded = encode_resume_ack(&frame);
        assert_eq!(encoded.len(), RESUME_ACK_FRAME_LEN);
        assert_eq!(decode_resume_ack(&encoded).unwrap(), frame);
    }

    #[test]
    fn resume_reject_roundtrips_for_all_known_values() {
        for reason in [ResumeRejectReason::Auth, ResumeRejectReason::UnknownSession, ResumeRejectReason::OffsetGone] {
            let byte = encode_resume_reject(reason);
            assert_eq!(decode_resume_reject(byte).unwrap(), reason);
        }
    }

    #[test]
    fn resume_reject_auth_matches_hello_reject_auth_byte() {
        // HELPER_PROTOCOL.md §7.3 explicitly reuses the existing HELLO/ACK
        // reject vocabulary for RESUME's proof check rather than minting a
        // new byte; this pins that down against accidental drift.
        assert_eq!(encode_resume_reject(ResumeRejectReason::Auth), crate::hello::FRAME_REJECT_AUTH);
    }

    #[test]
    fn decode_resume_rejects_wrong_length() {
        let err = decode_resume(&[FRAME_RESUME; 10]).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: 10, expected: RESUME_FRAME_LEN });
    }

    #[test]
    fn decode_resume_rejects_oversized_buffer() {
        // Frames are fixed-length, not length-prefixed, so an oversized
        // buffer is simply a length mismatch rather than a distinct
        // "declared length exceeds limit" case — still must not panic or
        // silently truncate.
        let huge = vec![FRAME_RESUME; RESUME_FRAME_LEN * 100];
        let err = decode_resume(&huge).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: huge.len(), expected: RESUME_FRAME_LEN });
    }

    #[test]
    fn decode_resume_rejects_wrong_type_byte() {
        let mut buf = vec![0x99u8];
        buf.extend_from_slice(&[0u8; RESUME_FRAME_LEN - 1]);
        let err = decode_resume(&buf).unwrap_err();
        assert_eq!(err, ProtocolError::UnknownFrameType(0x99));
    }

    #[test]
    fn decode_resume_ack_rejects_wrong_length() {
        let err = decode_resume_ack(&[FRAME_RESUME_ACK; 5]).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: 5, expected: RESUME_ACK_FRAME_LEN });
    }

    #[test]
    fn decode_resume_ack_rejects_wrong_type_byte() {
        let mut buf = vec![0x77u8];
        buf.extend_from_slice(&[0u8; RESUME_ACK_FRAME_LEN - 1]);
        let err = decode_resume_ack(&buf).unwrap_err();
        assert_eq!(err, ProtocolError::UnknownFrameType(0x77));
    }

    #[test]
    fn decode_resume_reject_rejects_unknown_byte() {
        let err = decode_resume_reject(0x01).unwrap_err();
        assert_eq!(err, ProtocolError::UnknownFrameType(0x01));
    }

    #[test]
    fn resume_proof_debug_does_not_leak_bytes() {
        let proof = ResumeProof::new([0xABu8; RESUME_PROOF_LEN]);
        assert_eq!(format!("{proof:?}"), "ResumeProof(..)");
    }

    #[test]
    fn resume_proof_ct_eq_detects_mismatch() {
        let a = ResumeProof::new([1u8; RESUME_PROOF_LEN]);
        let mut bytes = [1u8; RESUME_PROOF_LEN];
        bytes[31] = 2;
        let b = ResumeProof::new(bytes);
        assert!(!a.ct_eq(&b));
    }

    /// Existing-wire-format compatibility check (`ISEKAI_SSH_DESIGN.md` Phase
    /// S-4a acceptance criteria): builds the exact byte sequence that
    /// `rust-core/src/helper_quic_transport.rs`'s `reattach_fn` closure
    /// produces —
    /// `let mut frame = vec![resume_client::RESUME];`
    /// `frame.extend_from_slice(&session_id);`
    /// `frame.extend_from_slice(&resume_proof);`
    /// `frame.extend_from_slice(&client_sent_offset.to_be_bytes());`
    /// `frame.extend_from_slice(&client_delivered_offset.to_be_bytes());`
    /// — using the same constant marker byte values
    /// (`resume_client::RESUME == 0x03`, matching `FRAME_RESUME` here) and
    /// confirms this crate's decoder parses it correctly. `resume_client.rs`
    /// is `pub(crate)` inside `isekai-terminal-core` and out of scope to modify/import
    /// from here (Phase S-4a instructions), so the bytes are reconstructed
    /// from the documented constants instead of calling into that module.
    #[test]
    fn decodes_existing_resume_client_wire_format() {
        let session_id_bytes = [0x11u8; SESSION_ID_LEN];
        let resume_proof_bytes = [0x22u8; RESUME_PROOF_LEN];
        let client_sent_offset: u64 = 9_000;
        let client_delivered_offset: u64 = 8_500;

        let mut wire = vec![0x03u8]; // resume_client::RESUME
        wire.extend_from_slice(&session_id_bytes);
        wire.extend_from_slice(&resume_proof_bytes);
        wire.extend_from_slice(&client_sent_offset.to_be_bytes());
        wire.extend_from_slice(&client_delivered_offset.to_be_bytes());
        assert_eq!(wire.len(), 65, "HELPER_PROTOCOL.md §7.3: RESUME is a fixed 65-byte frame");

        let decoded = decode_resume(&wire).unwrap();
        assert_eq!(decoded.session_id, SessionId::from_bytes(session_id_bytes));
        assert_eq!(decoded.resume_proof.as_bytes(), &resume_proof_bytes);
        assert_eq!(decoded.client_sent_offset.get(), client_sent_offset);
        assert_eq!(decoded.client_delivered_offset.get(), client_delivered_offset);
    }

    /// Same as above for `RESUME_ACK`, mirroring the read side of
    /// `helper_quic_transport.rs`'s `reattach_fn`:
    /// `let mut resp = [0u8; 1]; recv.read_exact(&mut resp)...`
    /// `if resp[0] != resume_client::RESUME_ACK { ... }`
    /// `let mut rest = [0u8; 16]; recv.read_exact(&mut rest)...`
    /// `let helper_committed_offset = u64::from_be_bytes(rest[0..8]...)`
    #[test]
    fn decodes_existing_resume_ack_wire_format() {
        let helper_committed_offset: u64 = 7_777;
        let helper_sent_offset: u64 = 6_666;

        let mut wire = vec![0x13u8]; // resume_client::RESUME_ACK
        wire.extend_from_slice(&helper_committed_offset.to_be_bytes());
        wire.extend_from_slice(&helper_sent_offset.to_be_bytes());
        assert_eq!(wire.len(), 17, "HELPER_PROTOCOL.md §7.3: RESUME_ACK is a fixed 17-byte frame");

        let decoded = decode_resume_ack(&wire).unwrap();
        assert_eq!(decoded.helper_committed_offset.get(), helper_committed_offset);
        assert_eq!(decoded.helper_sent_offset.get(), helper_sent_offset);
    }
}
