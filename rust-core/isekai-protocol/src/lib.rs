//! Pure, dependency-light protocol types shared by isekai-terminal
//! (`tssh-core`), isekai-helper, and the future `isekai-ssh` CLI
//! (`ISEKAI_SSH_DESIGN.md` "実装方針"). This crate must never depend on
//! tokio/quinn/noq/russh/uniffi or any Android-specific type — it only knows
//! about byte layouts, JSON schemas, and the value types built on top of
//! them. I/O, QUIC, and SSH live in `isekai-transport`/`tssh-core` instead.
//!
//! Resume-specific frames (`RESUME`/`RESUME_ACK`, `HELPER_PROTOCOL.md` §7.3)
//! live in the `resume` module (Phase S-4a), byte-compatible with the
//! existing `pub(crate)` implementation in `rust-core/src/resume_client.rs`.

pub mod error;
pub mod handshake;
pub mod hello;
pub mod offset;
pub mod resume;
pub mod session_id;
pub mod version;

pub use error::ProtocolError;
pub use handshake::HandshakeJson;
pub use hello::{AckResponse, Proof};
pub use offset::{C2hHelperCommittedOffset, C2hSentOffset, H2cClientDeliveredOffset, H2cSentOffset};
pub use resume::{ResumeAckFrame, ResumeFrame, ResumeProof, ResumeRejectReason};
pub use session_id::SessionId;
pub use version::VersionInfo;

/// The frame types this crate currently knows how to identify by their
/// leading discriminant byte. Used to give a single, crate-wide answer to
/// "is this frame type byte recognized at all", independent of which
/// per-type decoder eventually parses the rest of the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Hello,
    Ack,
    RejectAuth,
    RejectDuplicate,
    RejectTarget,
    RejectUnsupported,
    VersionNegotiation,
    Resume,
    ResumeAck,
    ResumeRejectUnknownSession,
    ResumeRejectOffsetGone,
}

pub fn identify_frame_type(byte: u8) -> Result<FrameType, ProtocolError> {
    match byte {
        hello::FRAME_HELLO => Ok(FrameType::Hello),
        hello::FRAME_ACK => Ok(FrameType::Ack),
        // `resume::FRAME_RESUME_REJECT_AUTH` is defined as this same byte
        // (`HELPER_PROTOCOL.md` §7.3 reuses HELLO/ACK's reject vocabulary for
        // RESUME's proof check), so this one match arm covers both frame
        // families and intentionally reports `RejectAuth` either way.
        hello::FRAME_REJECT_AUTH => Ok(FrameType::RejectAuth),
        hello::FRAME_REJECT_DUPLICATE => Ok(FrameType::RejectDuplicate),
        hello::FRAME_REJECT_TARGET => Ok(FrameType::RejectTarget),
        hello::FRAME_REJECT_UNSUPPORTED => Ok(FrameType::RejectUnsupported),
        version::FRAME_VERSION_NEGOTIATION => Ok(FrameType::VersionNegotiation),
        resume::FRAME_RESUME => Ok(FrameType::Resume),
        resume::FRAME_RESUME_ACK => Ok(FrameType::ResumeAck),
        resume::FRAME_RESUME_REJECT_UNKNOWN_SESSION => Ok(FrameType::ResumeRejectUnknownSession),
        resume::FRAME_RESUME_REJECT_OFFSET_GONE => Ok(FrameType::ResumeRejectOffsetGone),
        other => Err(ProtocolError::UnknownFrameType(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_all_known_frame_types() {
        assert_eq!(identify_frame_type(hello::FRAME_HELLO).unwrap(), FrameType::Hello);
        assert_eq!(identify_frame_type(hello::FRAME_ACK).unwrap(), FrameType::Ack);
        assert_eq!(identify_frame_type(hello::FRAME_REJECT_AUTH).unwrap(), FrameType::RejectAuth);
        assert_eq!(identify_frame_type(hello::FRAME_REJECT_DUPLICATE).unwrap(), FrameType::RejectDuplicate);
        assert_eq!(identify_frame_type(hello::FRAME_REJECT_TARGET).unwrap(), FrameType::RejectTarget);
        assert_eq!(identify_frame_type(hello::FRAME_REJECT_UNSUPPORTED).unwrap(), FrameType::RejectUnsupported);
        assert_eq!(identify_frame_type(version::FRAME_VERSION_NEGOTIATION).unwrap(), FrameType::VersionNegotiation);
        assert_eq!(identify_frame_type(resume::FRAME_RESUME).unwrap(), FrameType::Resume);
        assert_eq!(identify_frame_type(resume::FRAME_RESUME_ACK).unwrap(), FrameType::ResumeAck);
        assert_eq!(
            identify_frame_type(resume::FRAME_RESUME_REJECT_UNKNOWN_SESSION).unwrap(),
            FrameType::ResumeRejectUnknownSession
        );
        assert_eq!(
            identify_frame_type(resume::FRAME_RESUME_REJECT_OFFSET_GONE).unwrap(),
            FrameType::ResumeRejectOffsetGone
        );
    }

    #[test]
    fn resume_reject_auth_byte_is_shared_with_hello_reject_auth() {
        // HELPER_PROTOCOL.md §7.3 explicitly reuses hello::FRAME_REJECT_AUTH
        // for RESUME's proof check rather than minting a new byte, so this
        // one byte resolves to `RejectAuth` regardless of which frame family
        // is being decoded — that's intentional, not an omission.
        assert_eq!(resume::FRAME_RESUME_REJECT_AUTH, hello::FRAME_REJECT_AUTH);
        assert_eq!(identify_frame_type(hello::FRAME_REJECT_AUTH).unwrap(), FrameType::RejectAuth);
    }

    #[test]
    fn rejects_unknown_frame_type() {
        // 0x05 is not assigned to any known frame family.
        let err = identify_frame_type(0x05).unwrap_err();
        assert_eq!(err, ProtocolError::UnknownFrameType(0x05));
    }
}
