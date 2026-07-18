//! The SSH-specific frame protocol carried over one
//! [`local_ipc_mux::ExclusiveChannel`] connection between the mux *owner*
//! (holds the shared authenticated `russh` `client::Handle`) and one *client*
//! process. Each connection multiplexes exactly one remote shell session; the
//! owner opens an independent SSH channel per client (see the module docs in
//! [`super`]).
//!
//! Wire format — a length-prefixed frame stream, one direction of concern per
//! side but symmetric on the wire:
//!
//! ```text
//! [u32 frame_len (big-endian)][u8 tag][payload ... (frame_len - 1 bytes)]
//! ```
//!
//! `frame_len` counts the tag byte plus the payload. Design points called out
//! by the M4 plan's Codex review, all enforced here:
//!
//! * **Size cap** ([`MAX_FRAME_PAYLOAD`]): a malformed or hostile peer can't
//!   force an unbounded allocation — [`read_frame`] rejects any header whose
//!   declared length exceeds the cap *before* allocating, exactly as
//!   `isekai-protocol`'s `HandshakeJson`/`CtlMessage` readers do.
//! * **Version field**: the first client→owner frame is a [`Frame::Hello`]
//!   carrying [`MUX_PROTOCOL_VERSION`]. The owner rejects a mismatch loudly
//!   (a [`Frame::Rejected`] then a close) rather than silently misparsing a
//!   future incompatible layout — see [`super::owner`].
//! * **Auth token**: `Hello` also carries an opaque token the owner compares
//!   (constant-time, [`token_eq`]) against a secret only the owning OS user
//!   could read. Defense-in-depth beneath the named pipe's own same-user ACL.
//!
//! Ordering and backpressure are properties of *how the relay drives this
//! protocol*, not of the encoding, and are documented on [`super::owner`] and
//! [`super::client`]: both directions are pumped by a single sequential loop,
//! so a `Resize` that a client sent before some `Stdin` bytes is applied to
//! the remote PTY before those bytes (never reordered), and a slow client
//! back-pressures the owner (which stops draining the SSH channel) because
//! each `Stdout` frame write is awaited before the next remote read.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bumped only on an *incompatible* change to this frame protocol. The owner
/// refuses a client whose `Hello` carries a different version (loud failure,
/// not silent misbehavior — an M4 review requirement).
pub const MUX_PROTOCOL_VERSION: u16 = 1;

/// Largest payload any single frame may carry. Terminal stdin/stdout chunks
/// are a few KiB in practice (the relay reads into an 8 KiB buffer); this cap
/// is generous headroom while still bounding a hostile peer's forced
/// allocation, matching `isekai-protocol`'s "reject a flood before it forces
/// an allocation" convention.
pub const MAX_FRAME_PAYLOAD: usize = 1024 * 1024;

/// Upper bound on the on-wire `frame_len` header (tag byte + payload).
const MAX_FRAME_LEN: usize = 1 + MAX_FRAME_PAYLOAD;

/// Cap on the `TERM` string carried in [`Frame::Hello`] — a real terminal
/// type name is short; this only exists so a peer can't smuggle a large
/// allocation in through the `Hello` frame's variable-length field.
const MAX_TERM_LEN: usize = 256;

// Frame type tags. Grouped by direction for readability, but the reader
// accepts any tag on either side — role enforcement (a client must not send
// `Stdout`, etc.) is the relay's job, not the codec's.
const TAG_HELLO: u8 = 0x01;
const TAG_HELLO_ACK: u8 = 0x02;
const TAG_REJECTED: u8 = 0x03;
const TAG_STDIN: u8 = 0x10;
const TAG_RESIZE: u8 = 0x11;
const TAG_SHUTDOWN: u8 = 0x12;
const TAG_STDOUT: u8 = 0x20;
const TAG_STDERR: u8 = 0x21;
const TAG_EXIT: u8 = 0x22;

/// One decoded protocol message. Client→owner: [`Frame::Hello`],
/// [`Frame::Stdin`], [`Frame::Resize`], [`Frame::Shutdown`]. Owner→client:
/// [`Frame::HelloAck`], [`Frame::Rejected`], [`Frame::Stdout`],
/// [`Frame::Stderr`], [`Frame::Exit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// First frame a client sends. `cols`/`rows`/`term` let the owner open
    /// the per-client shell channel with the right initial PTY geometry.
    Hello { version: u16, token: Vec<u8>, term: String, cols: u16, rows: u16 },
    /// Owner's acknowledgement that the version matched and the token was
    /// accepted; the client may now stream stdin/resize.
    HelloAck { version: u16 },
    /// Owner's refusal (version mismatch or bad token). Carries a short
    /// human-readable reason for the client to print before it exits.
    Rejected { reason: String },
    /// Client→owner terminal input bytes.
    Stdin(Vec<u8>),
    /// Client→owner terminal resize. Applied to the remote PTY in receive
    /// order relative to `Stdin` (see the module docs on ordering).
    Resize { cols: u16, rows: u16 },
    /// Client→owner "I'm closing this session" (the local terminal reached
    /// EOF / the client is exiting). The owner sends EOF to the remote shell.
    Shutdown,
    /// Owner→client remote stdout bytes.
    Stdout(Vec<u8>),
    /// Owner→client remote stderr bytes (kept separate from `Stdout`, exactly
    /// as the single-process path keeps remote `ExtendedData` off local
    /// stdout).
    Stderr(Vec<u8>),
    /// Owner→client remote exit status; the session has ended cleanly.
    Exit(u8),
}

impl Frame {
    /// The tag byte plus the encoded payload (everything after the `u32`
    /// length header). [`write_frame`] prepends the length.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Frame::Hello { version, token, term, cols, rows } => {
                out.push(TAG_HELLO);
                out.extend_from_slice(&version.to_be_bytes());
                out.extend_from_slice(&cols.to_be_bytes());
                out.extend_from_slice(&rows.to_be_bytes());
                let term_bytes = term.as_bytes();
                out.extend_from_slice(&(term_bytes.len() as u16).to_be_bytes());
                out.extend_from_slice(term_bytes);
                out.extend_from_slice(token);
            }
            Frame::HelloAck { version } => {
                out.push(TAG_HELLO_ACK);
                out.extend_from_slice(&version.to_be_bytes());
            }
            Frame::Rejected { reason } => {
                out.push(TAG_REJECTED);
                out.extend_from_slice(reason.as_bytes());
            }
            Frame::Stdin(data) => {
                out.push(TAG_STDIN);
                out.extend_from_slice(data);
            }
            Frame::Resize { cols, rows } => {
                out.push(TAG_RESIZE);
                out.extend_from_slice(&cols.to_be_bytes());
                out.extend_from_slice(&rows.to_be_bytes());
            }
            Frame::Shutdown => out.push(TAG_SHUTDOWN),
            Frame::Stdout(data) => {
                out.push(TAG_STDOUT);
                out.extend_from_slice(data);
            }
            Frame::Stderr(data) => {
                out.push(TAG_STDERR);
                out.extend_from_slice(data);
            }
            Frame::Exit(code) => {
                out.push(TAG_EXIT);
                out.push(*code);
            }
        }
        out
    }

    /// Decodes a frame body (tag byte already split off as `tag`, `payload`
    /// is everything after it). Returns a `InvalidData` error on any
    /// malformed field so a truncated or hostile frame fails loudly rather
    /// than being silently misinterpreted.
    fn decode(tag: u8, payload: &[u8]) -> io::Result<Frame> {
        match tag {
            TAG_HELLO => {
                // version(2) + cols(2) + rows(2) + term_len(2) + term + token(rest)
                let version = read_u16(payload, 0)?;
                let cols = read_u16(payload, 2)?;
                let rows = read_u16(payload, 4)?;
                let term_len = read_u16(payload, 6)? as usize;
                if term_len > MAX_TERM_LEN {
                    return Err(malformed("Hello term string exceeds the cap"));
                }
                let term_start: usize = 8;
                let term_end = term_start.checked_add(term_len).ok_or_else(|| malformed("Hello term length overflow"))?;
                if payload.len() < term_end {
                    return Err(malformed("Hello frame truncated before end of term string"));
                }
                let term = std::str::from_utf8(&payload[term_start..term_end])
                    .map_err(|_| malformed("Hello term string is not valid UTF-8"))?
                    .to_string();
                let token = payload[term_end..].to_vec();
                Ok(Frame::Hello { version, token, term, cols, rows })
            }
            TAG_HELLO_ACK => Ok(Frame::HelloAck { version: read_u16(payload, 0)? }),
            TAG_REJECTED => {
                let reason = String::from_utf8_lossy(payload).into_owned();
                Ok(Frame::Rejected { reason })
            }
            TAG_STDIN => Ok(Frame::Stdin(payload.to_vec())),
            TAG_RESIZE => Ok(Frame::Resize { cols: read_u16(payload, 0)?, rows: read_u16(payload, 2)? }),
            TAG_SHUTDOWN => Ok(Frame::Shutdown),
            TAG_STDOUT => Ok(Frame::Stdout(payload.to_vec())),
            TAG_STDERR => Ok(Frame::Stderr(payload.to_vec())),
            TAG_EXIT => {
                let code = *payload.first().ok_or_else(|| malformed("Exit frame missing its status byte"))?;
                Ok(Frame::Exit(code))
            }
            other => Err(malformed(&format!("unknown frame tag {other:#04x}"))),
        }
    }
}

fn read_u16(payload: &[u8], offset: usize) -> io::Result<u16> {
    let end = offset.checked_add(2).ok_or_else(|| malformed("u16 field offset overflow"))?;
    let slice = payload.get(offset..end).ok_or_else(|| malformed("frame truncated before a u16 field"))?;
    Ok(u16::from_be_bytes([slice[0], slice[1]]))
}

fn malformed(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("isekai-ssh mux frame: {msg}"))
}

/// Writes one frame with its `u32` big-endian length header, then flushes.
/// Returns an error if the payload exceeds [`MAX_FRAME_PAYLOAD`] (so a bug on
/// the *sending* side surfaces here rather than tripping the peer's reader).
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> io::Result<()> {
    let body = frame.encode();
    if body.len() > MAX_FRAME_LEN {
        return Err(malformed("outgoing frame exceeds the size cap"));
    }
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Reads the next frame. Returns `Ok(None)` on a *clean* end of stream (the
/// peer closed between frames), and an error on a truncated frame, an
/// oversized length header (before allocating), or a malformed body.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut len_buf = [0u8; 4];
    // Read the first header byte on its own so a peer closing cleanly between
    // frames (0 bytes here) is `Ok(None)`, not an `UnexpectedEof` error.
    match r.read(&mut len_buf[..1]).await? {
        0 => return Ok(None),
        1 => {}
        _ => unreachable!("read into a 1-byte slice returns 0 or 1"),
    }
    // Any EOF *now* is mid-header truncation — a genuine error.
    r.read_exact(&mut len_buf[1..]).await?;
    let frame_len = u32::from_be_bytes(len_buf) as usize;
    if frame_len < 1 || frame_len > MAX_FRAME_LEN {
        return Err(malformed(&format!("declared frame length {frame_len} is out of range (1..={MAX_FRAME_LEN})")));
    }
    let mut body = vec![0u8; frame_len];
    r.read_exact(&mut body).await?;
    Frame::decode(body[0], &body[1..]).map(Some)
}

/// Constant-time equality for the auth token — compares every byte regardless
/// of where the first difference is, so the comparison's duration doesn't leak
/// how many leading bytes a guess got right. (The named-pipe ACL already
/// restricts the token file to the same OS user; this is defense-in-depth.)
pub fn token_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    fn sample_frames() -> Vec<Frame> {
        vec![
            Frame::Hello { version: MUX_PROTOCOL_VERSION, token: vec![9u8; 32], term: "xterm-256color".to_string(), cols: 120, rows: 40 },
            Frame::HelloAck { version: MUX_PROTOCOL_VERSION },
            Frame::Rejected { reason: "protocol version mismatch".to_string() },
            Frame::Stdin(b"ls -la\n".to_vec()),
            Frame::Resize { cols: 80, rows: 24 },
            Frame::Shutdown,
            Frame::Stdout(b"total 0\n".to_vec()),
            Frame::Stderr(b"bash: nope: command not found\n".to_vec()),
            Frame::Exit(42),
        ]
    }

    #[tokio::test]
    async fn every_frame_variant_round_trips() {
        for frame in sample_frames() {
            let (mut w, mut r) = duplex(64 * 1024);
            write_frame(&mut w, &frame).await.unwrap();
            drop(w); // clean EOF after the single frame
            let decoded = read_frame(&mut r).await.unwrap().expect("a frame was written");
            assert_eq!(decoded, frame, "frame did not survive an encode/decode round-trip");
        }
    }

    #[tokio::test]
    async fn multiple_frames_stream_in_order() {
        let (mut w, mut r) = duplex(64 * 1024);
        let frames = sample_frames();
        for f in &frames {
            write_frame(&mut w, f).await.unwrap();
        }
        drop(w);
        for expected in &frames {
            let got = read_frame(&mut r).await.unwrap().unwrap();
            assert_eq!(&got, expected);
        }
        assert_eq!(read_frame(&mut r).await.unwrap(), None, "stream must end cleanly after the last frame");
    }

    #[tokio::test]
    async fn clean_eof_between_frames_is_none_not_error() {
        let (w, mut r) = duplex(1024);
        drop(w);
        assert_eq!(read_frame(&mut r).await.unwrap(), None, "a peer that closes between frames is Ok(None)");
    }

    #[tokio::test]
    async fn an_oversized_length_header_is_rejected_before_allocating() {
        let (mut w, mut r) = duplex(1024);
        // A length header claiming far more than the cap must be refused up
        // front, without the reader trying to allocate a buffer that size.
        let bogus_len = (MAX_FRAME_LEN as u32) + 1;
        w.write_all(&bogus_len.to_be_bytes()).await.unwrap();
        w.flush().await.unwrap();
        let err = read_frame(&mut r).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "an oversized frame length must be an InvalidData error");
    }

    #[tokio::test]
    async fn a_zero_length_frame_header_is_rejected() {
        let (mut w, mut r) = duplex(1024);
        w.write_all(&0u32.to_be_bytes()).await.unwrap();
        w.flush().await.unwrap();
        let err = read_frame(&mut r).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn a_truncated_body_is_an_error_not_a_clean_eof() {
        let (mut w, mut r) = duplex(1024);
        // Claim a 10-byte body but send only 3, then close.
        w.write_all(&10u32.to_be_bytes()).await.unwrap();
        w.write_all(&[TAG_STDIN, 1, 2]).await.unwrap();
        w.flush().await.unwrap();
        drop(w);
        let err = read_frame(&mut r).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "a truncated body must not be mistaken for a clean stream end");
    }

    #[tokio::test]
    async fn an_unknown_tag_is_rejected() {
        let (mut w, mut r) = duplex(1024);
        w.write_all(&1u32.to_be_bytes()).await.unwrap();
        w.write_all(&[0xEE]).await.unwrap();
        w.flush().await.unwrap();
        let err = read_frame(&mut r).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn writing_an_oversized_payload_fails_on_the_sender() {
        let (mut w, _r) = duplex(1024);
        let too_big = Frame::Stdin(vec![0u8; MAX_FRAME_PAYLOAD + 1]);
        let err = write_frame(&mut w, &too_big).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "a payload past the cap must be refused on the sending side");
    }

    #[test]
    fn token_eq_is_length_and_content_sensitive() {
        assert!(token_eq(b"same-token", b"same-token"));
        assert!(!token_eq(b"same-token", b"same-toker"));
        assert!(!token_eq(b"short", b"shorter"));
        assert!(token_eq(b"", b""));
    }
}
