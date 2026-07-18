//! Wire format for `datagram_relay` (`#32`/`#33`) — deliberately **not**
//! ATTACH v2's channel-id capsule design sketched in task #35's design doc
//! §4 (that design binds a channel to an already-ATTACHed SSH session;
//! this module has no such session to bind to — see the parent module's
//! docs on why this stays isolated from `isekai-protocol::attach`). Two
//! framings:
//!
//! - **Control frames** ([`ControlFrame`]), sent over a dedicated reliable
//!   bidirectional stream: `ChannelOpen { id, target }` registers a channel
//!   id against a local UDP peer address; `ChannelClose { id }` retires it.
//!   Wire shape: `[u16 body_len][u8 frame_type][body]` — the length prefix
//!   lets a reader know exactly how many bytes to consume before decoding,
//!   since the underlying reliable stream itself has no message boundaries.
//! - **Datagram frames**: `[u32 channel_id][raw payload]`, sent as one QUIC
//!   datagram via `AnyMuxConnection::send_datagram`. Deliberately no length
//!   prefix on the payload — a QUIC datagram is already one discrete
//!   message, unlike the control stream.

use std::net::SocketAddr;

use bytes::{Buf, BufMut, Bytes, BytesMut};

/// Identifies one logical UDP-relay channel within a single quicmux
/// connection. Connection-local, not reused across connections — mirrors
/// task #35's design doc §4.3's identical convention for its own
/// (ATTACH-bound) channel ids, adapted here for this session-less protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelId(pub u32);

const DATAGRAM_CHANNEL_ID_LEN: usize = 4;

/// Prefixes `payload` with `channel_id` for sending as one QUIC datagram.
pub fn encode_datagram(channel_id: ChannelId, payload: &[u8]) -> Bytes {
    let mut buf = BytesMut::with_capacity(DATAGRAM_CHANNEL_ID_LEN + payload.len());
    buf.put_u32(channel_id.0);
    buf.extend_from_slice(payload);
    buf.freeze()
}

/// Splits a received QUIC datagram back into its channel id + payload.
/// `None` if the datagram is too short to even hold a channel id — dropped
/// by the caller (not treated as a fatal connection error: an unreliable
/// datagram is inherently allowed to be malformed/truncated in flight, and
/// per task #35's design doc §7, this module never retries/replays
/// datagrams anyway).
pub fn decode_datagram(datagram: &Bytes) -> Option<(ChannelId, Bytes)> {
    if datagram.len() < DATAGRAM_CHANNEL_ID_LEN {
        return None;
    }
    let mut buf = datagram.clone();
    let id = buf.get_u32();
    Some((ChannelId(id), buf))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlFrame {
    /// Registers `id` against `target`: the receiving side should bind a
    /// local UDP socket connected to `target` and relay datagrams tagged
    /// `id` to/from it (`datagram_relay::run_control_stream_reader`).
    ChannelOpen { id: ChannelId, target: SocketAddr },
    /// Retires `id` — the receiving side should tear down and stop relaying
    /// through whatever local UDP socket it registered for this id.
    ChannelClose { id: ChannelId },
}

const FRAME_TYPE_CHANNEL_OPEN: u8 = 1;
const FRAME_TYPE_CHANNEL_CLOSE: u8 = 2;

/// Maximum control-frame body length this module will ever encode/accept —
/// bounds the length-prefixed read in `datagram_relay::read_control_frame`
/// so a malformed or hostile peer can't make the reader allocate an
/// unbounded buffer from an attacker-controlled length prefix.
pub const MAX_CONTROL_FRAME_LEN: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameDecodeError {
    UnknownFrameType(u8),
    TooShort,
    FrameTooLarge { len: usize },
    InvalidTargetAddr,
}

impl std::fmt::Display for FrameDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownFrameType(t) => write!(f, "unknown control frame type {t}"),
            Self::TooShort => write!(f, "control frame body too short"),
            Self::FrameTooLarge { len } => write!(f, "control frame body too large: {len} bytes (max {MAX_CONTROL_FRAME_LEN})"),
            Self::InvalidTargetAddr => write!(f, "ChannelOpen target is not a valid socket address"),
        }
    }
}

impl std::error::Error for FrameDecodeError {}

/// Encodes one control frame as `[u16 body_len][body]` — see this module's
/// docs for why the length prefix exists.
pub fn encode_control_frame(frame: &ControlFrame) -> Bytes {
    let mut body = BytesMut::new();
    match frame {
        ControlFrame::ChannelOpen { id, target } => {
            body.put_u8(FRAME_TYPE_CHANNEL_OPEN);
            body.put_u32(id.0);
            let target_str = target.to_string();
            body.put_u16(target_str.len() as u16);
            body.extend_from_slice(target_str.as_bytes());
        }
        ControlFrame::ChannelClose { id } => {
            body.put_u8(FRAME_TYPE_CHANNEL_CLOSE);
            body.put_u32(id.0);
        }
    }
    let mut framed = BytesMut::with_capacity(2 + body.len());
    framed.put_u16(body.len() as u16);
    framed.extend_from_slice(&body);
    framed.freeze()
}

/// Decodes one control frame's *body* — the length prefix is consumed
/// separately by the caller's stream-reading loop
/// (`datagram_relay::read_control_frame`), which is what actually needs to
/// know the length before it can read the body off the stream at all.
pub fn decode_control_frame_body(body: &[u8]) -> Result<ControlFrame, FrameDecodeError> {
    let mut buf = body;
    if buf.is_empty() {
        return Err(FrameDecodeError::TooShort);
    }
    let frame_type = buf.get_u8();
    match frame_type {
        FRAME_TYPE_CHANNEL_OPEN => {
            if buf.remaining() < 4 + 2 {
                return Err(FrameDecodeError::TooShort);
            }
            let id = ChannelId(buf.get_u32());
            let target_len = buf.get_u16() as usize;
            if buf.remaining() < target_len {
                return Err(FrameDecodeError::TooShort);
            }
            let target_str = std::str::from_utf8(&buf[..target_len]).map_err(|_| FrameDecodeError::InvalidTargetAddr)?;
            let target = target_str.parse().map_err(|_| FrameDecodeError::InvalidTargetAddr)?;
            Ok(ControlFrame::ChannelOpen { id, target })
        }
        FRAME_TYPE_CHANNEL_CLOSE => {
            if buf.remaining() < 4 {
                return Err(FrameDecodeError::TooShort);
            }
            Ok(ControlFrame::ChannelClose { id: ChannelId(buf.get_u32()) })
        }
        other => Err(FrameDecodeError::UnknownFrameType(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagram_roundtrips_through_encode_decode() {
        let encoded = encode_datagram(ChannelId(42), b"hello mavlink");
        let (id, payload) = decode_datagram(&encoded).unwrap();
        assert_eq!(id, ChannelId(42));
        assert_eq!(&payload[..], b"hello mavlink");
    }

    #[test]
    fn decode_datagram_rejects_payload_shorter_than_channel_id() {
        let too_short = Bytes::from_static(&[0, 1, 2]);
        assert!(decode_datagram(&too_short).is_none());
    }

    #[test]
    fn decode_datagram_accepts_empty_payload() {
        let encoded = encode_datagram(ChannelId(7), b"");
        let (id, payload) = decode_datagram(&encoded).unwrap();
        assert_eq!(id, ChannelId(7));
        assert!(payload.is_empty());
    }

    #[test]
    fn channel_open_roundtrips_through_control_frame_encode_decode() {
        let frame = ControlFrame::ChannelOpen { id: ChannelId(3), target: "203.0.113.5:14550".parse().unwrap() };
        let encoded = encode_control_frame(&frame);
        // Simulates the stream reader: consume the length prefix, then decode the body.
        let body_len = u16::from_be_bytes([encoded[0], encoded[1]]) as usize;
        assert_eq!(body_len, encoded.len() - 2);
        let decoded = decode_control_frame_body(&encoded[2..]).unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn channel_close_roundtrips_through_control_frame_encode_decode() {
        let frame = ControlFrame::ChannelClose { id: ChannelId(9) };
        let encoded = encode_control_frame(&frame);
        let decoded = decode_control_frame_body(&encoded[2..]).unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn decode_control_frame_body_rejects_unknown_frame_type() {
        let err = decode_control_frame_body(&[0xff]).unwrap_err();
        assert_eq!(err, FrameDecodeError::UnknownFrameType(0xff));
    }

    #[test]
    fn decode_control_frame_body_rejects_empty_body() {
        let err = decode_control_frame_body(&[]).unwrap_err();
        assert_eq!(err, FrameDecodeError::TooShort);
    }

    #[test]
    fn decode_control_frame_body_rejects_truncated_channel_open() {
        // Frame type + channel id, but missing the target-length/target bytes.
        let truncated = [FRAME_TYPE_CHANNEL_OPEN, 0, 0, 0, 1];
        let err = decode_control_frame_body(&truncated).unwrap_err();
        assert_eq!(err, FrameDecodeError::TooShort);
    }

    #[test]
    fn decode_control_frame_body_rejects_invalid_target_addr() {
        let mut body = BytesMut::new();
        body.put_u8(FRAME_TYPE_CHANNEL_OPEN);
        body.put_u32(1);
        body.put_u16(11);
        body.extend_from_slice(b"not-an-addr");
        let err = decode_control_frame_body(&body).unwrap_err();
        assert_eq!(err, FrameDecodeError::InvalidTargetAddr);
    }
}
