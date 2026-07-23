//! Pure, dependency-light protocol types shared by isekai-terminal
//! (`isekai-terminal-core`), isekai-helper, and the future `isekai-ssh` CLI
//! (`archive/ISEKAI_SSH_DESIGN.md` "実装方針"). This crate must never depend on
//! tokio/quinn/noq/russh/uniffi or any Android-specific type — it only knows
//! about byte layouts, JSON schemas, and the value types built on top of
//! them. I/O, QUIC, and SSH live in `isekai-transport`/`isekai-terminal-core` instead.
//!
//! `resume` holds `ResumeRejectReason` — the resume wire frames themselves
//! now live in `quicmux::resume` (`quicmux-server-resume` Stage B), see that
//! module's docs and `resume.rs`'s own module doc here for why.

pub mod attach;
pub mod bootstrap;
pub mod bootstrap_request;
pub mod ctl;
pub mod ctl_vars;
pub mod error;
pub mod handshake;
pub mod hello;
pub mod offset;
pub mod resume;
pub mod session_id;
pub mod version;

pub use attach::{
    AttachActivate, AttachHello, AttachKey, AttachProof, AttachRejectReason, AttachResponse, AttachToken, AttemptId,
    CancelAttach, ConnectionGeneration,
};
pub use ctl::{
    decode_ctl_message, validate_ctl_message, BuildOutputStream, ClipboardMime, CtlMessage, VarScope,
    MAX_BUILD_CHUNK_DECODED_LEN, MAX_BUILD_PROFILE_NAME_LEN, MAX_BUILD_RESULT_PATHS, MAX_BUILD_RESULT_PATH_LEN,
    MAX_CLIPBOARD_IMAGE_DECODED_LEN, MAX_CLIPBOARD_TEXT_DECODED_LEN, MAX_CTL_MESSAGE_LINE_LEN, MAX_VAR_KEY_LEN,
    MAX_VAR_VALUE_LEN,
};
pub use ctl_vars::CtlVarStore;
pub use bootstrap_request::{
    BootstrapAttemptId, BootstrapCandidateV2, BootstrapRequestV2, BootstrapReportV2, BOOTSTRAP_PROTOCOL_V2,
};
pub use error::ProtocolError;
pub use handshake::HandshakeJson;
pub use hello::{AckResponse, Proof};
pub use offset::{C2hHelperCommittedOffset, C2hSentOffset, H2cClientDeliveredOffset, H2cSentOffset};
pub use resume::ResumeRejectReason;
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
}

pub fn identify_frame_type(byte: u8) -> Result<FrameType, ProtocolError> {
    match byte {
        hello::FRAME_HELLO => Ok(FrameType::Hello),
        hello::FRAME_ACK => Ok(FrameType::Ack),
        hello::FRAME_REJECT_AUTH => Ok(FrameType::RejectAuth),
        hello::FRAME_REJECT_DUPLICATE => Ok(FrameType::RejectDuplicate),
        hello::FRAME_REJECT_TARGET => Ok(FrameType::RejectTarget),
        hello::FRAME_REJECT_UNSUPPORTED => Ok(FrameType::RejectUnsupported),
        version::FRAME_VERSION_NEGOTIATION => Ok(FrameType::VersionNegotiation),
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
    }

    #[test]
    fn rejects_unknown_frame_type() {
        // 0x05 is not assigned to any known frame family.
        let err = identify_frame_type(0x05).unwrap_err();
        assert_eq!(err, ProtocolError::UnknownFrameType(0x05));
    }
}
