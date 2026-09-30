//! Warm-standby hold: wire types for keeping a pre-established QUIC
//! connection to `isekai-pipe serve` open *without* it counting as an idle
//! "opened a connection but never sent its first frame" connection.
//!
//! `isekai-pipe serve`'s `handle_connection` closes any connection whose
//! first stream doesn't deliver its frame type within `HELLO_TIMEOUT` (5s) —
//! a deliberate guard against a peer that just completes the QUIC handshake
//! and then sits on it. `isekai-transport`'s `WarmStandby` needs exactly that
//! "sit on it" shape (a standby connection kept warm for minutes, promoted
//! with a `RESUME` only once the primary dies), so before this frame existed
//! the server silently closed every standby ~5s after it was dialed and
//! almost every promotion fell back to a full resume anyway.
//!
//! The protocol, all on the connection's **first** bidirectional stream (the
//! "hold stream"):
//!
//! 1. client → server: [`FRAME_STANDBY_HOLD`] followed by a 32-byte proof
//!    `HMAC-SHA256(session_secret, exporter || STANDBY_HOLD_PROOF_DOMAIN)`
//!    (authenticated so that only a holder of the server's `session_secret`
//!    can pin a connection open indefinitely; an unauthenticated peer still
//!    gets the ordinary `HELLO_TIMEOUT` treatment).
//! 2. server → client: [`FRAME_STANDBY_READY`] (or `hello::FRAME_REJECT_AUTH`
//!    on a proof mismatch; an older server that predates this frame answers
//!    `hello::FRAME_REJECT_UNSUPPORTED` and closes — the client treats that
//!    as "this server can't hold a standby" and stops dialing one).
//! 3. liveness: client writes [`FRAME_STANDBY_PING`], server answers
//!    [`FRAME_STANDBY_PONG`] — an application-level round trip, so a
//!    connection the server has already given up on can't pass the probe.
//! 4. promotion: the client opens a **second** bidirectional stream and sends
//!    an ordinary `quicmux::FRAME_RESUME` request on it. The server only
//!    accepts `FRAME_RESUME` as that second stream's frame type.
//!
//! Proof computation needs the live connection's exporter, so it stays out of
//! this I/O-free crate (see `attach.rs`'s module docs for the same split).

use crate::error::ProtocolError;

pub const FRAME_STANDBY_HOLD: u8 = 0x34;
pub const FRAME_STANDBY_READY: u8 = 0x35;
pub const FRAME_STANDBY_PING: u8 = 0x36;
pub const FRAME_STANDBY_PONG: u8 = 0x37;

pub const STANDBY_HOLD_PROOF_LEN: usize = 32;
/// `1` (type) + proof.
pub const STANDBY_HOLD_FRAME_LEN: usize = 1 + STANDBY_HOLD_PROOF_LEN;

/// Domain-separation string fed to the HMAC after the exporter, so a
/// standby-hold proof can never be replayed as any other frame's proof (and
/// vice versa — every other proof in this protocol either carries its own
/// domain string or a `session_id`).
pub const STANDBY_HOLD_PROOF_DOMAIN: &[u8] = b"isekai-pipe/standby/v1/hold";

pub fn encode_standby_hold(proof: &[u8; STANDBY_HOLD_PROOF_LEN]) -> [u8; STANDBY_HOLD_FRAME_LEN] {
    let mut buf = [0u8; STANDBY_HOLD_FRAME_LEN];
    buf[0] = FRAME_STANDBY_HOLD;
    buf[1..].copy_from_slice(proof);
    buf
}

/// Decodes the proof from a full [`FRAME_STANDBY_HOLD`] frame (type byte
/// included).
pub fn decode_standby_hold(buf: &[u8]) -> Result<[u8; STANDBY_HOLD_PROOF_LEN], ProtocolError> {
    if buf.len() != STANDBY_HOLD_FRAME_LEN {
        return Err(ProtocolError::FrameLengthMismatch { got: buf.len(), expected: STANDBY_HOLD_FRAME_LEN });
    }
    if buf[0] != FRAME_STANDBY_HOLD {
        return Err(ProtocolError::UnknownFrameType(buf[0]));
    }
    let mut proof = [0u8; STANDBY_HOLD_PROOF_LEN];
    proof.copy_from_slice(&buf[1..]);
    Ok(proof)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standby_hold_roundtrips() {
        let proof = [7u8; STANDBY_HOLD_PROOF_LEN];
        let frame = encode_standby_hold(&proof);
        assert_eq!(frame[0], FRAME_STANDBY_HOLD);
        assert_eq!(decode_standby_hold(&frame).unwrap(), proof);
    }

    #[test]
    fn decode_standby_hold_rejects_wrong_length_and_type() {
        assert!(matches!(decode_standby_hold(&[FRAME_STANDBY_HOLD]), Err(ProtocolError::FrameLengthMismatch { .. })));
        let mut frame = encode_standby_hold(&[0u8; STANDBY_HOLD_PROOF_LEN]);
        frame[0] = 0x30;
        assert_eq!(decode_standby_hold(&frame), Err(ProtocolError::UnknownFrameType(0x30)));
    }

    #[test]
    fn standby_frame_types_do_not_collide_with_first_stream_frame_types() {
        // The server dispatches on the first stream's first byte; these must
        // stay distinct from ATTACH_HELLO/ATTACH_CANCEL and quicmux's RESUME
        // (0x01).
        for b in [FRAME_STANDBY_HOLD, FRAME_STANDBY_READY, FRAME_STANDBY_PING, FRAME_STANDBY_PONG] {
            assert_ne!(b, crate::attach::FRAME_ATTACH_HELLO);
            assert_ne!(b, crate::attach::FRAME_ATTACH_CANCEL);
            assert_ne!(b, 0x01);
        }
    }
}
