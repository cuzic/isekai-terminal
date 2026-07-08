//! HELLO/proof/ACK types, extracted from
//! `rust-core/src/isekai_pipe_quic_transport.rs` (`archive/HELPER_PROTOCOL.md` §4). Only
//! the message shapes and validation live here; computing the proof itself
//! (HMAC over the QUIC exporter) stays in `isekai-terminal-core`/`isekai-transport`
//! since it needs the live QUIC connection.

use crate::error::ProtocolError;

pub const EXPORTER_LABEL: &[u8] = b"isekai-pipe-auth-v1";
pub const ALPN: &[u8] = b"isekai-pipe/1";

pub const FRAME_HELLO: u8 = 0x01;
pub const FRAME_ACK: u8 = 0x02;
pub const FRAME_REJECT_TARGET: u8 = 0xFC;
pub const FRAME_REJECT_UNSUPPORTED: u8 = 0xFD;
pub const FRAME_REJECT_DUPLICATE: u8 = 0xFE;
pub const FRAME_REJECT_AUTH: u8 = 0xFF;

pub const PROOF_LEN: usize = 32;
/// Size of the `requested_resume_grace_secs`/`effective_resume_grace_secs`
/// fields (`u32`, big-endian) added to `HELLO`/`ACK` for resume-grace
/// negotiation (`ISEKAI_PIPE_DESIGN.md` — client requests a grace period via
/// `HELLO`, the server clamps it to its own configured max and echoes the
/// effective value back in `ACK`; a request of `0` means "no preference, use
/// the server's own default/max").
pub const RESUME_GRACE_LEN: usize = 4;
pub const HELLO_FRAME_LEN: usize = 1 + PROOF_LEN + RESUME_GRACE_LEN;
/// `ACK`'s frame length when the response is `AckResponse::Ack` (the only
/// variant carrying a payload beyond the single type byte).
pub const ACK_FRAME_LEN: usize = 1 + RESUME_GRACE_LEN;

/// `proof = HMAC-SHA256(session_secret, exporter)` (`archive/HELPER_PROTOCOL.md` §4).
/// Deliberately opaque: `Debug` does not print the bytes so a stray `{:?}` in
/// a log line can't leak it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Proof([u8; PROOF_LEN]);

impl Proof {
    pub fn new(bytes: [u8; PROOF_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; PROOF_LEN] {
        &self.0
    }

    /// Constant-time comparison, per the timing-attack mitigation mandated by
    /// `archive/HELPER_PROTOCOL.md` §4 ("proof の比較は constant-time equality で行う").
    pub fn ct_eq(&self, other: &Proof) -> bool {
        let mut diff = 0u8;
        for (a, b) in self.0.iter().zip(other.0.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

impl std::fmt::Debug for Proof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Proof(..)")
    }
}

/// A decoded `HELLO` frame: the auth proof plus the client's requested
/// resume-grace period (`0` = no preference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelloFrame {
    pub proof: Proof,
    pub requested_resume_grace_secs: u32,
}

pub fn encode_hello(proof: &Proof, requested_resume_grace_secs: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HELLO_FRAME_LEN);
    buf.push(FRAME_HELLO);
    buf.extend_from_slice(proof.as_bytes());
    buf.extend_from_slice(&requested_resume_grace_secs.to_be_bytes());
    buf
}

pub fn decode_hello(buf: &[u8]) -> Result<HelloFrame, ProtocolError> {
    if buf.len() != HELLO_FRAME_LEN {
        return Err(ProtocolError::FrameLengthMismatch { got: buf.len(), expected: HELLO_FRAME_LEN });
    }
    if buf[0] != FRAME_HELLO {
        return Err(ProtocolError::UnknownFrameType(buf[0]));
    }
    let mut p = [0u8; PROOF_LEN];
    p.copy_from_slice(&buf[1..1 + PROOF_LEN]);
    let mut g = [0u8; RESUME_GRACE_LEN];
    g.copy_from_slice(&buf[1 + PROOF_LEN..]);
    Ok(HelloFrame { proof: Proof::new(p), requested_resume_grace_secs: u32::from_be_bytes(g) })
}

/// The helper's response to `HELLO` (`archive/HELPER_PROTOCOL.md` §4). `Ack` carries the
/// negotiated `effective_resume_grace_secs` (`min(requested, server's own
/// configured max)`, or the server's own default when the client requested
/// `0`); every reject variant stays a bare one-byte frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckResponse {
    Ack { effective_resume_grace_secs: u32 },
    RejectAuth,
    RejectDuplicate,
    RejectTarget,
    RejectUnsupported,
}

pub fn encode_ack_response(resp: AckResponse) -> Vec<u8> {
    match resp {
        AckResponse::Ack { effective_resume_grace_secs } => {
            let mut buf = Vec::with_capacity(ACK_FRAME_LEN);
            buf.push(FRAME_ACK);
            buf.extend_from_slice(&effective_resume_grace_secs.to_be_bytes());
            buf
        }
        AckResponse::RejectAuth => vec![FRAME_REJECT_AUTH],
        AckResponse::RejectDuplicate => vec![FRAME_REJECT_DUPLICATE],
        AckResponse::RejectTarget => vec![FRAME_REJECT_TARGET],
        AckResponse::RejectUnsupported => vec![FRAME_REJECT_UNSUPPORTED],
    }
}

/// Decodes a full `ACK` frame buffer: `buf[0]` is always the type byte;
/// `Ack` additionally requires exactly `RESUME_GRACE_LEN` trailing bytes
/// (reject variants must be exactly 1 byte, no trailing bytes). Callers read
/// off the wire in two steps — the type byte first, then (only for
/// `FRAME_ACK`) `RESUME_GRACE_LEN` more bytes — since the frame length
/// depends on the type byte itself.
pub fn decode_ack_response(buf: &[u8]) -> Result<AckResponse, ProtocolError> {
    let Some(&type_byte) = buf.first() else {
        return Err(ProtocolError::FrameLengthMismatch { got: 0, expected: 1 });
    };
    match type_byte {
        FRAME_ACK => {
            if buf.len() != ACK_FRAME_LEN {
                return Err(ProtocolError::FrameLengthMismatch { got: buf.len(), expected: ACK_FRAME_LEN });
            }
            let mut g = [0u8; RESUME_GRACE_LEN];
            g.copy_from_slice(&buf[1..]);
            Ok(AckResponse::Ack { effective_resume_grace_secs: u32::from_be_bytes(g) })
        }
        FRAME_REJECT_AUTH | FRAME_REJECT_DUPLICATE | FRAME_REJECT_TARGET | FRAME_REJECT_UNSUPPORTED => {
            if buf.len() != 1 {
                return Err(ProtocolError::FrameLengthMismatch { got: buf.len(), expected: 1 });
            }
            Ok(match type_byte {
                FRAME_REJECT_AUTH => AckResponse::RejectAuth,
                FRAME_REJECT_DUPLICATE => AckResponse::RejectDuplicate,
                FRAME_REJECT_TARGET => AckResponse::RejectTarget,
                _ => AckResponse::RejectUnsupported,
            })
        }
        other => Err(ProtocolError::UnknownFrameType(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrips() {
        let proof = Proof::new([9u8; PROOF_LEN]);
        let encoded = encode_hello(&proof, 180);
        assert_eq!(encoded.len(), HELLO_FRAME_LEN);
        let decoded = decode_hello(&encoded).unwrap();
        assert!(decoded.proof.ct_eq(&proof));
        assert_eq!(decoded.requested_resume_grace_secs, 180);
    }

    #[test]
    fn hello_roundtrips_with_zero_meaning_no_preference() {
        let proof = Proof::new([3u8; PROOF_LEN]);
        let encoded = encode_hello(&proof, 0);
        let decoded = decode_hello(&encoded).unwrap();
        assert_eq!(decoded.requested_resume_grace_secs, 0);
    }

    #[test]
    fn decode_hello_rejects_wrong_length() {
        let err = decode_hello(&[FRAME_HELLO; 10]).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: 10, expected: HELLO_FRAME_LEN });
    }

    #[test]
    fn decode_hello_rejects_wrong_type_byte() {
        let mut buf = vec![0x99u8];
        buf.extend_from_slice(&[0u8; PROOF_LEN + RESUME_GRACE_LEN]);
        let err = decode_hello(&buf).unwrap_err();
        assert_eq!(err, ProtocolError::UnknownFrameType(0x99));
    }

    #[test]
    fn ack_response_roundtrips_for_all_known_values() {
        for resp in [
            AckResponse::Ack { effective_resume_grace_secs: 90 },
            AckResponse::RejectAuth,
            AckResponse::RejectDuplicate,
            AckResponse::RejectTarget,
            AckResponse::RejectUnsupported,
        ] {
            let buf = encode_ack_response(resp);
            assert_eq!(decode_ack_response(&buf).unwrap(), resp);
        }
    }

    #[test]
    fn decode_ack_response_rejects_unknown_frame_type() {
        let err = decode_ack_response(&[0x42]).unwrap_err();
        assert_eq!(err, ProtocolError::UnknownFrameType(0x42));
    }

    #[test]
    fn decode_ack_response_rejects_empty_buffer() {
        let err = decode_ack_response(&[]).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: 0, expected: 1 });
    }

    #[test]
    fn decode_ack_response_rejects_wrong_ack_length() {
        let err = decode_ack_response(&[FRAME_ACK, 0, 0]).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: 3, expected: ACK_FRAME_LEN });
    }

    #[test]
    fn decode_ack_response_rejects_trailing_bytes_on_reject() {
        let err = decode_ack_response(&[FRAME_REJECT_AUTH, 0]).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: 2, expected: 1 });
    }

    #[test]
    fn proof_ct_eq_detects_mismatch() {
        let a = Proof::new([1u8; PROOF_LEN]);
        let mut bytes = [1u8; PROOF_LEN];
        bytes[31] = 2;
        let b = Proof::new(bytes);
        assert!(!a.ct_eq(&b));
    }

    #[test]
    fn proof_debug_does_not_leak_bytes() {
        let secret = Proof::new([0xABu8; PROOF_LEN]);
        assert_eq!(format!("{secret:?}"), "Proof(..)");
    }
}
