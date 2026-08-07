//! A bounded, drop-oldest byte ring buffer for the daemon's dtach-style
//! "replay recent output on attach" behavior.
//!
//! Deliberately **not** `isekai-pipe/src/engine/resume.rs`'s existing
//! `OutputBuffer` (a design review flagged this): that type is built for the
//! QUIC resume protocol's ACK-driven trimming — `append` *rejects* new data
//! once full rather than evicting old data, because trimming there only
//! happens when the client's `APP_ACK` confirms delivery of a specific
//! offset. There is no ACK channel here (a `tty attach` client that never
//! attaches yet must not stall the shell), so the only sane policy is the
//! opposite one: always accept new output, silently drop the oldest bytes
//! once the buffer is full. This is a completely different eviction policy,
//! not a parameterization of the same type, hence a fresh ~small
//! implementation rather than a shared one.

use std::collections::VecDeque;

/// A prefix of an escape sequence can survive at the front of the buffer
/// after eviction (e.g. a color-setting `ESC [ 3 1 m` split mid-sequence),
/// which would render as garbage/misinterpreted control codes if replayed
/// as-is. [`RingBuffer::replay`] prefixes the replay with this soft-reset
/// sequence (`ESC c`, RIS — full terminal reset) so a client always starts
/// from a clean, well-defined state regardless of what's at the buffer's
/// current head.
const SOFT_RESET: &[u8] = b"\x1bc";

pub(crate) struct RingBuffer {
    buf: VecDeque<u8>,
    capacity: usize,
}

impl RingBuffer {
    pub(crate) fn new(capacity: usize) -> Self {
        Self { buf: VecDeque::with_capacity(capacity.min(64 * 1024)), capacity }
    }

    /// Appends `data`, evicting the oldest bytes first if this would exceed
    /// `capacity`. Never rejects — a hostile or just chatty pty is bounded
    /// by memory use, never by blocking or losing the ability to accept
    /// further output.
    pub(crate) fn append(&mut self, data: &[u8]) {
        if data.len() >= self.capacity {
            // The new chunk alone already fills (or exceeds) the buffer —
            // only its own tail is ever going to be visible in the replay,
            // so skip evicting one byte at a time and just keep that tail.
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.capacity..]);
            return;
        }
        let overflow = (self.buf.len() + data.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            self.buf.drain(..overflow);
        }
        self.buf.extend(data);
    }

    /// The soft-reset sequence followed by everything currently buffered,
    /// ready to send to a newly-attaching client in one shot.
    pub(crate) fn replay(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SOFT_RESET.len() + self.buf.len());
        out.extend_from_slice(SOFT_RESET);
        out.extend(self.buf.iter().copied());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_within_capacity_keeps_everything() {
        let mut rb = RingBuffer::new(16);
        rb.append(b"hello ");
        rb.append(b"world");
        assert_eq!(rb.replay(), [SOFT_RESET, b"hello world"].concat());
    }

    #[test]
    fn append_past_capacity_drops_the_oldest_bytes() {
        let mut rb = RingBuffer::new(5);
        rb.append(b"abc");
        rb.append(b"defgh"); // total 8 bytes written, capacity 5 -> keep last 5: "defgh"
        assert_eq!(rb.replay(), [SOFT_RESET, b"defgh".as_slice()].concat());
    }

    #[test]
    fn a_single_chunk_larger_than_capacity_keeps_only_its_tail() {
        let mut rb = RingBuffer::new(4);
        rb.append(b"0123456789");
        assert_eq!(rb.replay(), [SOFT_RESET, b"6789".as_slice()].concat());
    }

    #[test]
    fn never_rejects_regardless_of_how_much_is_written() {
        let mut rb = RingBuffer::new(8);
        for _ in 0..1000 {
            rb.append(b"x");
        }
        assert_eq!(rb.replay(), [SOFT_RESET, b"xxxxxxxx".as_slice()].concat());
    }

    #[test]
    fn empty_buffer_replays_just_the_soft_reset() {
        let rb = RingBuffer::new(16);
        assert_eq!(rb.replay(), SOFT_RESET);
    }
}
