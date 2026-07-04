//! HELLO/proof/ACK types, extracted from
//! `rust-core/src/helper_quic_transport.rs` (`HELPER_PROTOCOL.md` §4). Only
//! the message shapes and validation live here; computing the proof itself
//! (HMAC over the QUIC exporter) stays in `tssh-core`/`isekai-transport`
//! since it needs the live QUIC connection.

use crate::error::ProtocolError;

pub const EXPORTER_LABEL: &[u8] = b"isekai-helper-auth-v1";
pub const ALPN: &[u8] = b"isekai-helper/1";

pub const FRAME_HELLO: u8 = 0x01;
pub const FRAME_ACK: u8 = 0x02;
pub const FRAME_REJECT_TARGET: u8 = 0xFC;
pub const FRAME_REJECT_UNSUPPORTED: u8 = 0xFD;
pub const FRAME_REJECT_DUPLICATE: u8 = 0xFE;
pub const FRAME_REJECT_AUTH: u8 = 0xFF;

pub const PROOF_LEN: usize = 32;
pub const HELLO_FRAME_LEN: usize = 1 + PROOF_LEN;

/// `proof = HMAC-SHA256(session_secret, exporter)` (`HELPER_PROTOCOL.md` §4).
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
    /// `HELPER_PROTOCOL.md` §4 ("proof の比較は constant-time equality で行う").
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

pub fn encode_hello(proof: &Proof) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HELLO_FRAME_LEN);
    buf.push(FRAME_HELLO);
    buf.extend_from_slice(proof.as_bytes());
    buf
}

pub fn decode_hello(buf: &[u8]) -> Result<Proof, ProtocolError> {
    if buf.len() != HELLO_FRAME_LEN {
        return Err(ProtocolError::FrameLengthMismatch { got: buf.len(), expected: HELLO_FRAME_LEN });
    }
    if buf[0] != FRAME_HELLO {
        return Err(ProtocolError::UnknownFrameType(buf[0]));
    }
    let mut p = [0u8; PROOF_LEN];
    p.copy_from_slice(&buf[1..]);
    Ok(Proof::new(p))
}

/// The helper's one-byte response to `HELLO` (`HELPER_PROTOCOL.md` §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckResponse {
    Ack,
    RejectAuth,
    RejectDuplicate,
    RejectTarget,
    RejectUnsupported,
}

pub fn encode_ack_response(resp: AckResponse) -> u8 {
    match resp {
        AckResponse::Ack => FRAME_ACK,
        AckResponse::RejectAuth => FRAME_REJECT_AUTH,
        AckResponse::RejectDuplicate => FRAME_REJECT_DUPLICATE,
        AckResponse::RejectTarget => FRAME_REJECT_TARGET,
        AckResponse::RejectUnsupported => FRAME_REJECT_UNSUPPORTED,
    }
}

pub fn decode_ack_response(byte: u8) -> Result<AckResponse, ProtocolError> {
    match byte {
        FRAME_ACK => Ok(AckResponse::Ack),
        FRAME_REJECT_AUTH => Ok(AckResponse::RejectAuth),
        FRAME_REJECT_DUPLICATE => Ok(AckResponse::RejectDuplicate),
        FRAME_REJECT_TARGET => Ok(AckResponse::RejectTarget),
        FRAME_REJECT_UNSUPPORTED => Ok(AckResponse::RejectUnsupported),
        other => Err(ProtocolError::UnknownFrameType(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrips() {
        let proof = Proof::new([9u8; PROOF_LEN]);
        let encoded = encode_hello(&proof);
        assert_eq!(encoded.len(), HELLO_FRAME_LEN);
        let decoded = decode_hello(&encoded).unwrap();
        assert!(decoded.ct_eq(&proof));
    }

    #[test]
    fn decode_hello_rejects_wrong_length() {
        let err = decode_hello(&[FRAME_HELLO; 10]).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: 10, expected: HELLO_FRAME_LEN });
    }

    #[test]
    fn decode_hello_rejects_wrong_type_byte() {
        let mut buf = vec![0x99u8];
        buf.extend_from_slice(&[0u8; PROOF_LEN]);
        let err = decode_hello(&buf).unwrap_err();
        assert_eq!(err, ProtocolError::UnknownFrameType(0x99));
    }

    #[test]
    fn ack_response_roundtrips_for_all_known_values() {
        for resp in [
            AckResponse::Ack,
            AckResponse::RejectAuth,
            AckResponse::RejectDuplicate,
            AckResponse::RejectTarget,
            AckResponse::RejectUnsupported,
        ] {
            let byte = encode_ack_response(resp);
            assert_eq!(decode_ack_response(byte).unwrap(), resp);
        }
    }

    #[test]
    fn decode_ack_response_rejects_unknown_frame_type() {
        let err = decode_ack_response(0x42).unwrap_err();
        assert_eq!(err, ProtocolError::UnknownFrameType(0x42));
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
