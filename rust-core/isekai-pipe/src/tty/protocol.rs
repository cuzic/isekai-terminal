//! The frame protocol carried over the Unix domain socket between
//! `isekai-pipe tty daemon` and `isekai-pipe tty attach` (see [`super`]'s
//! module docs for the whole feature's design). Deliberately a fresh,
//! independent implementation rather than sharing code with
//! `isekai-ssh`'s `native/mux/protocol.rs` (which plays the same
//! length-prefixed-frame role for the Windows-native local mux): different
//! process boundary, different trust model (that one crosses a same-OS-user
//! named pipe on Windows; this one crosses a same-OS-user Unix domain socket
//! on the always-Linux remote host, authenticated via `SO_PEERCRED` rather
//! than a token), and this module has no version-negotiation needs (both
//! ends are always the same `isekai-pipe` binary, started together by the
//! same deploy).
//!
//! Wire format, one direction of concern per side but symmetric on the
//! wire:
//!
//! ```text
//! [u32 frame_len (big-endian)][u8 tag][payload ... (frame_len - 1 bytes)]
//! ```

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest payload any single frame may carry. `Stdin`/`Stdout` chunks are a
/// few KiB in practice; this is generous headroom while still bounding a
/// hostile/buggy peer's forced allocation before the length prefix is even
/// fully validated, matching `native/mux/protocol.rs`'s identical
/// `MAX_FRAME_PAYLOAD` rationale.
pub(crate) const MAX_FRAME_PAYLOAD: usize = 1024 * 1024;
const MAX_FRAME_LEN: usize = 1 + MAX_FRAME_PAYLOAD;

/// Cap on `Hello`'s `term` string — a real `$TERM` value is short; this only
/// exists so a peer can't smuggle a large allocation through a
/// variable-length `Hello` field.
const MAX_TERM_LEN: usize = 256;

const TAG_HELLO: u8 = 0x01;
const TAG_HELLO_ACK: u8 = 0x02;
const TAG_STDIN: u8 = 0x10;
const TAG_RESIZE: u8 = 0x11;
const TAG_STDOUT: u8 = 0x20;
const TAG_EXIT: u8 = 0x21;
const TAG_PREEMPTED: u8 = 0x22;

/// One decoded protocol message. Client (`tty attach`) → daemon:
/// [`Frame::Hello`], [`Frame::Stdin`], [`Frame::Resize`]. Daemon → client:
/// [`Frame::HelloAck`] (immediately followed by zero or more
/// [`Frame::Stdout`] frames replaying the daemon's ring buffer),
/// [`Frame::Stdout`], [`Frame::Exit`], [`Frame::Preempted`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Frame {
    /// First frame the client sends. `cols`/`rows`/`term` let the daemon set
    /// the pty's initial window size and `$TERM`-dependent behavior (a fresh
    /// pty is only sized at `openpty` time — see `pty.rs` — so this must
    /// arrive before that call, not applied as a later resize).
    Hello { term: String, cols: u16, rows: u16 },
    /// Daemon's acknowledgement that this client is now the attached
    /// occupant. The client may now stream `Stdin`/`Resize`.
    HelloAck,
    /// Client → daemon terminal input bytes.
    Stdin(Vec<u8>),
    /// Client → daemon terminal resize, applied via `TIOCSWINSZ`.
    Resize { cols: u16, rows: u16 },
    /// Daemon → client pty output bytes.
    Stdout(Vec<u8>),
    /// Daemon → client: the pty's child process exited with this code. The
    /// daemon closes the connection (and the whole daemon process, per this
    /// feature's "daemon lifetime = shell lifetime" design) right after.
    Exit(u8),
    /// Daemon → client: a newer `attach` took over this daemon's single
    /// occupant slot. The client should print a one-line notice and exit
    /// non-zero, not hang or treat this as an ordinary disconnect.
    Preempted,
}

impl Frame {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Frame::Hello { term, cols, rows } => {
                out.push(TAG_HELLO);
                out.extend_from_slice(&cols.to_be_bytes());
                out.extend_from_slice(&rows.to_be_bytes());
                let term_bytes = term.as_bytes();
                out.extend_from_slice(&(term_bytes.len() as u16).to_be_bytes());
                out.extend_from_slice(term_bytes);
            }
            Frame::HelloAck => out.push(TAG_HELLO_ACK),
            Frame::Stdin(data) => {
                out.push(TAG_STDIN);
                out.extend_from_slice(data);
            }
            Frame::Resize { cols, rows } => {
                out.push(TAG_RESIZE);
                out.extend_from_slice(&cols.to_be_bytes());
                out.extend_from_slice(&rows.to_be_bytes());
            }
            Frame::Stdout(data) => {
                out.push(TAG_STDOUT);
                out.extend_from_slice(data);
            }
            Frame::Exit(code) => {
                out.push(TAG_EXIT);
                out.push(*code);
            }
            Frame::Preempted => out.push(TAG_PREEMPTED),
        }
        out
    }

    fn decode(tag: u8, payload: &[u8]) -> io::Result<Frame> {
        match tag {
            TAG_HELLO => {
                let cols = read_u16(payload, 0)?;
                let rows = read_u16(payload, 2)?;
                let term_len = read_u16(payload, 4)? as usize;
                if term_len > MAX_TERM_LEN {
                    return Err(malformed("Hello term string exceeds the cap"));
                }
                let term_start = 6usize;
                let term_end = term_start.checked_add(term_len).ok_or_else(|| malformed("Hello term length overflow"))?;
                if payload.len() != term_end {
                    return Err(malformed("Hello frame has trailing or truncated bytes"));
                }
                let term = std::str::from_utf8(&payload[term_start..term_end])
                    .map_err(|_| malformed("Hello term string is not valid UTF-8"))?
                    .to_string();
                Ok(Frame::Hello { term, cols, rows })
            }
            TAG_HELLO_ACK => Ok(Frame::HelloAck),
            TAG_STDIN => Ok(Frame::Stdin(payload.to_vec())),
            TAG_RESIZE => Ok(Frame::Resize { cols: read_u16(payload, 0)?, rows: read_u16(payload, 2)? }),
            TAG_STDOUT => Ok(Frame::Stdout(payload.to_vec())),
            TAG_EXIT => {
                let code = *payload.first().ok_or_else(|| malformed("Exit frame missing its status byte"))?;
                if payload.len() != 1 {
                    return Err(malformed("Exit frame has trailing bytes"));
                }
                Ok(Frame::Exit(code))
            }
            TAG_PREEMPTED => Ok(Frame::Preempted),
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
    io::Error::new(io::ErrorKind::InvalidData, format!("isekai-pipe tty frame: {msg}"))
}

/// Writes one frame with its `u32` big-endian length header, then flushes.
pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> io::Result<()> {
    let body = frame.encode();
    if body.len() > MAX_FRAME_LEN {
        return Err(malformed("outgoing frame exceeds the size cap"));
    }
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Reads the next frame. Returns `Ok(None)` on a clean end of stream (the
/// peer closed between frames), and an error on a truncated frame, an
/// oversized length header (rejected before allocating), or a malformed
/// body.
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut len_buf = [0u8; 4];
    match r.read(&mut len_buf[..1]).await? {
        0 => return Ok(None),
        1 => {}
        _ => unreachable!("read into a 1-byte slice returns 0 or 1"),
    }
    r.read_exact(&mut len_buf[1..]).await?;
    let frame_len = u32::from_be_bytes(len_buf) as usize;
    if !(1..=MAX_FRAME_LEN).contains(&frame_len) {
        return Err(malformed(&format!("declared frame length {frame_len} is out of range (1..={MAX_FRAME_LEN})")));
    }
    let mut body = vec![0u8; frame_len];
    r.read_exact(&mut body).await?;
    Frame::decode(body[0], &body[1..]).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    fn sample_frames() -> Vec<Frame> {
        vec![
            Frame::Hello { term: "xterm-256color".to_string(), cols: 120, rows: 40 },
            Frame::HelloAck,
            Frame::Stdin(b"ls -la\n".to_vec()),
            Frame::Resize { cols: 80, rows: 24 },
            Frame::Stdout(b"total 0\n".to_vec()),
            Frame::Exit(0),
            Frame::Exit(137),
            Frame::Preempted,
        ]
    }

    #[tokio::test]
    async fn every_frame_variant_round_trips() {
        for frame in sample_frames() {
            let (mut w, mut r) = duplex(64 * 1024);
            write_frame(&mut w, &frame).await.unwrap();
            drop(w);
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
}
