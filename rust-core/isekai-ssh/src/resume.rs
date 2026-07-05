//! C2H replay buffer with backpressure (`ISEKAI_SSH_DESIGN.md` Phase S-4c
//! task 2 "C2H replay buffer"). This is the `isekai-ssh`-side counterpart to
//! `isekai-transport::resume`'s control-stream/`RESUME` wire logic: it owns
//! the actual bytes that may need replaying after a reconnect, plus the
//! backpressure policy that keeps it bounded.
//!
//! Deliberately **not** a port of `rust-core/src/resume_client.rs::ReplayBuffer`
//! byte-for-byte: that buffer silently evicts its oldest bytes once full
//! (`ReplayBuffer::append`'s `while self.data.len() > self.capacity { pop_front(); }`).
//! For isekai-ssh that would be a correctness bug, not just a resume-window
//! size tradeoff — the evicted bytes might not yet be
//! `c2h_helper_committed_offset`-confirmed, so evicting them would mean a
//! `RESUME` after that point could never correctly replay the SSH byte
//! stream (the bytes are just gone). `ISEKAI_SSH_DESIGN.md`'s task
//! description explicitly calls for backpressure instead: "上限バイト数に
//!達したら、stdinからの読み取りを一時停止し...". This buffer therefore
//! refuses to grow past `capacity` at all (`is_full`/`remaining_capacity`);
//! the caller (`connect.rs`'s `pump_c2h`) is responsible for pausing its own
//! stdin reads when told to, which is what actually backpressures the parent
//! `ssh` process (`ISEKAI_SSH_DESIGN.md`: "読み取りを呼ばなければパイプが
//! 埋まってssh側の書き込みがブロックされる、という単純な仕組みで十分").

use std::collections::VecDeque;

/// C2H (client→helper) bytes isekai-ssh has already written to the QUIC data
/// stream but isekai-helper has not yet confirmed committing to its
/// `--target` TCP socket (`ISEKAI_SSH_DESIGN.md`'s
/// `c2h_helper_committed_offset` "source of truth"). Kept around so a
/// `RESUME` after a disconnect can replay exactly the bytes isekai-helper
/// says it never received.
pub struct C2hReplayBuffer {
    data: VecDeque<u8>,
    start_offset: u64,
    capacity: usize,
}

impl C2hReplayBuffer {
    pub fn new(capacity: usize) -> Self {
        Self { data: VecDeque::with_capacity(capacity.min(1 << 20)), start_offset: 0, capacity }
    }

    /// `true` once the buffer holds `capacity` unconfirmed bytes — the pump
    /// loop must stop reading from stdin until this goes back to `false`
    /// (via `advance_start`, driven by isekai-helper's `APP_ACK`/`RESUME_ACK`
    /// progress reports).
    pub fn is_full(&self) -> bool {
        self.data.len() >= self.capacity
    }

    /// How many more bytes may be `append`ed before `is_full()` — used to
    /// cap a single stdin read so `append` never has to reject a call.
    pub fn remaining_capacity(&self) -> usize {
        self.capacity.saturating_sub(self.data.len())
    }

    /// Appends bytes already written to the data stream. Panics (via a
    /// `debug_assert!`) if the caller ignored `remaining_capacity` and tried
    /// to push past `capacity` — that would indicate a bug in the pump loop's
    /// backpressure check, not a condition callers should need to handle at
    /// runtime.
    pub fn append(&mut self, bytes: &[u8]) {
        debug_assert!(
            self.data.len() + bytes.len() <= self.capacity,
            "C2hReplayBuffer::append called past capacity — caller must respect remaining_capacity()"
        );
        self.data.extend(bytes.iter().copied());
    }

    /// Discards bytes isekai-helper has confirmed (`c2h_helper_committed_offset`,
    /// from an `APP_ACK` or a `RESUME_ACK`), freeing room for more stdin reads.
    pub fn advance_start(&mut self, confirmed_offset: u64) {
        let wanted = confirmed_offset.saturating_sub(self.start_offset) as usize;
        let drop_count = wanted.min(self.data.len());
        self.data.drain(..drop_count);
        self.start_offset += drop_count as u64;

        if confirmed_offset > self.start_offset {
            // isekai-helper confirmed further than we have data for (e.g. we
            // already discarded up to some earlier confirmed offset and this
            // is a stale/duplicate report) — advancing the marker without
            // more data to drop is harmless and keeps `start_offset`/
            // `end_offset` consistent for future `replay_from` bounds checks.
            self.start_offset = confirmed_offset;
        }
    }

    /// Not used by production code today (`connect.rs` only ever needs
    /// `end_offset`/`replay_from`) but kept `pub`, matching
    /// `resume_client.rs::ReplayBuffer::start_offset`'s own
    /// `#[allow(dead_code)]`-annotated exposure, since it's a natural part of
    /// this type's public surface and several tests rely on it.
    #[allow(dead_code)]
    pub fn start_offset(&self) -> u64 {
        self.start_offset
    }

    pub fn end_offset(&self) -> u64 {
        self.start_offset + self.data.len() as u64
    }

    /// Bytes from `from` (a `RESUME_ACK`'s `helper_committed_offset`) through
    /// `end_offset()`, to replay onto a freshly resumed data stream. `None`
    /// if `from` is outside the currently buffered range.
    pub fn replay_from(&self, from: u64) -> Option<Vec<u8>> {
        if from < self.start_offset || from > self.end_offset() {
            return None;
        }
        let skip = (from - self.start_offset) as usize;
        Some(self.data.iter().skip(skip).copied().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_replay_round_trip() {
        let mut buf = C2hReplayBuffer::new(1024);
        buf.append(b"hello");
        buf.append(b" world");
        assert_eq!(buf.end_offset(), 11);
        assert_eq!(buf.replay_from(0).unwrap(), b"hello world");
        assert_eq!(buf.replay_from(5).unwrap(), b" world");
    }

    #[test]
    fn advance_start_discards_confirmed_prefix_and_frees_capacity() {
        let mut buf = C2hReplayBuffer::new(4);
        buf.append(b"abcd");
        assert!(buf.is_full());
        buf.advance_start(2);
        assert!(!buf.is_full());
        assert_eq!(buf.remaining_capacity(), 2);
        assert_eq!(buf.replay_from(2).unwrap(), b"cd");
        assert!(buf.replay_from(0).is_none(), "bytes before start_offset must not be replayable");
    }

    /// The core backpressure contract this module exists for
    /// (`ISEKAI_SSH_DESIGN.md`): once the buffer reaches its capacity, the
    /// caller (`pump_c2h`) must stop reading further stdin bytes rather than
    /// silently dropping or evicting unconfirmed C2H data. This test proves
    /// the buffer's own state machine enforces that boundary correctly:
    /// `is_full`/`remaining_capacity` accurately reflect "no room" at
    /// capacity, and only actually confirming bytes (`advance_start`) frees
    /// room again — never eviction.
    #[test]
    fn is_full_and_remaining_capacity_enforce_backpressure_boundary() {
        let mut buf = C2hReplayBuffer::new(8);
        assert!(!buf.is_full());
        assert_eq!(buf.remaining_capacity(), 8);

        buf.append(b"1234");
        assert!(!buf.is_full());
        assert_eq!(buf.remaining_capacity(), 4);

        buf.append(b"5678");
        assert!(buf.is_full(), "buffer must report full at exactly capacity");
        assert_eq!(buf.remaining_capacity(), 0);

        // Confirming only part of the buffer frees exactly that much room —
        // it must not silently drop the still-unconfirmed remainder.
        buf.advance_start(3);
        assert!(!buf.is_full());
        assert_eq!(buf.remaining_capacity(), 3);
        assert_eq!(buf.end_offset(), 8, "unconfirmed bytes 3..8 must still be present, not evicted");
        assert_eq!(buf.replay_from(3).unwrap(), b"45678");
    }

    #[test]
    #[should_panic(expected = "past capacity")]
    fn append_past_capacity_is_a_caller_bug_not_silent_eviction() {
        // Unlike `resume_client.rs::ReplayBuffer` (Android side), which
        // silently evicts old bytes on overflow, this buffer must never
        // silently lose unconfirmed data — a caller that ignores
        // `remaining_capacity()`/`is_full()` and appends anyway has a bug,
        // and should find out loudly (in debug builds) rather than getting a
        // quietly-corrupted replay buffer.
        let mut buf = C2hReplayBuffer::new(4);
        buf.append(b"abcd");
        buf.append(b"e");
    }

    #[test]
    fn advance_start_past_end_offset_is_handled_without_panicking() {
        let mut buf = C2hReplayBuffer::new(8);
        buf.append(b"ab");
        // A stray/duplicate confirmation past what's actually buffered must
        // not panic or leave `start_offset > end_offset` in a way that later
        // confuses `replay_from`.
        buf.advance_start(100);
        assert_eq!(buf.start_offset(), 100);
        assert_eq!(buf.end_offset(), 100);
        assert!(buf.replay_from(100).unwrap().is_empty());
    }
}
