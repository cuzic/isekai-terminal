//! `ResumeRejectReason` — why a resume attempt was rejected, shared between
//! `isekai-transport` and `quicmux::resume` (`quicmux-server-resume` Stage
//! B's `map_reject_reason`, see `isekai-transport/src/resume.rs`).
//!
//! The `RESUME`/`RESUME_ACK` wire frames this module used to define
//! (`ResumeFrame`/`ResumeAckFrame`/`ResumeProof`, `FRAME_RESUME` = `0x03`)
//! were isekai's own bespoke resume wire format, byte-for-byte compatible
//! with `rust-core/src/resume_client.rs`'s original hand-rolled
//! implementation. `quicmux-server-resume` Stage B replaced that wire format
//! end-to-end with a new, protocol-agnostic one owned by `quicmux::resume`
//! (`FRAME_RESUME` = `0x01`, a different byte value — the two are not
//! compatible) — `isekai-pipe serve` and `isekai-transport`'s client-side
//! resume both moved onto it as their actual production wire protocol, and
//! nothing in this workspace decodes the old frame shape anymore. The old
//! frame types/codecs were deleted with them; only this reason enum survived,
//! since it is still a meaningful semantic value independent of either wire
//! format.

/// Why a resume attempt was rejected, independent of which wire format
/// (the old bespoke `RESUME`/`RESUME_ACK` frames, or `quicmux::resume`'s
/// replacement) carried the rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeRejectReason {
    /// The presented resume proof did not match.
    Auth,
    /// `session_id` が存在しない — helper 再起動・タイムアウト等。
    UnknownSession,
    /// 要求された offset が既に helper 側バッファの範囲外。
    OffsetGone,
}
