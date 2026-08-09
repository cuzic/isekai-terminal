//! A generic, protocol-agnostic session-resume primitive: reconnect an
//! [`AnyMuxConnection`] after transport loss and continue a byte stream from
//! a checkpoint offset, without this crate ever knowing what the caller's
//! own attach/authentication protocol looks like.
//!
//! # Scope: RESUME only, not ATTACH
//!
//! This module deliberately does **not** attempt to genericize `isekai`'s
//! own ATTACH v2 handshake (`isekai-protocol::attach`, `isekai-pipe`'s
//! `AttachArbiter`/`AttachRuntime`) — that protocol's complexity (multi-
//! candidate racing across direct/relay/STUN paths, generation-based
//! fencing, the `PendingActivation` ambiguous-window fix) exists to solve a
//! problem specific to isekai's own multi-path connection-establishment
//! story, not to session resume in general. `isekai-pipe`'s own
//! `resume.rs` module docs record the reason this module's scope is safe to
//! keep narrow: "同一sessionへのresumeはfencing衝突になり得ない" (resuming
//! the same session is never a fencing conflict) — `RESUME` there already
//! bypasses `AttachArbiter::hello`'s fencing entirely and only checks
//! `AttachRuntime::established_lease_for(session_id)`, i.e. "does this
//! token currently occupy an established slot" — that one check is left to
//! the caller to implement however its own session bookkeeping needs to.
//!
//! # Division of responsibility
//!
//! - **This module owns**: the wire framing for the resume request/response
//!   exchange ([`request_resume`] on the client side; [`decode_resume_request`]
//!   plus [`respond_resume_accepted`]/[`respond_resume_rejected`] on the
//!   server side), and a generic offset-based [`ReplayBuffer`] a caller uses
//!   to buffer bytes it has sent so it can honor a resume request that asks
//!   to replay from an earlier offset.
//! - **The caller owns**: what a `token`/`auth_blob` mean, how to verify
//!   `auth_blob` (this crate has no authentication layer of its own — see
//!   [`crate::MuxError::AuthenticationFailed`]'s docs), and all session
//!   bookkeeping (mapping a token to a parked connection/buffer, deciding
//!   whether a token is currently resumable, single-flight-ing concurrent
//!   resume attempts for the same token).
//!
//! # Why there is no server-side `accept_resume`/`ResumeAcceptor`
//!
//! This module used to also offer an `accept_resume(conn, &dyn ResumeAcceptor)`
//! that performed its own `accept_bi()` and dispatched into a caller-supplied
//! trait object. The only real server in this workspace —
//! `isekai-pipe serve`'s `handle_connection` — structurally cannot use that
//! shape: it already reads the first frame-type byte off a single stream to
//! choose between its own `ATTACH_HELLO`/`CancelAttach` frames and this
//! module's [`FRAME_RESUME`], so by the time it knows the frame is a resume,
//! both the stream and its type byte are already consumed. It therefore
//! called [`decode_resume_request`]/[`respond_resume_accepted`]/
//! [`respond_resume_rejected`] directly, and the trait-object API ended up
//! with no callers outside this module's own tests.
//!
//! It was deleted rather than kept "for future generality": a second,
//! never-exercised-in-production code path through the same resume wire
//! format is exactly the kind of drift this workspace's reconnect code
//! cannot afford (`.claude/rules/always-connects.md`), and it made the
//! tests below assert against a parallel implementation instead of the one
//! that actually ships. Those tests now drive the same direct API the real
//! server uses.

use std::collections::VecDeque;

use crate::error::MuxError;
use crate::mux::{AnyByteStream, AnyByteStreamReadHalf, AnyByteStreamWriteHalf, AnyMuxConnection};

/// This module's own frame markers — deliberately distinct from any
/// caller's own protocol frame bytes (e.g. isekai's `RESUME`=`0x03`,
/// `RESUME_ACK`=`0x13`) so the two can never be confused if a caller
/// migrates from its own hand-rolled resume protocol to this one on a
/// connection that also carries other framing. Version byte first, in case
/// this wire format ever needs a breaking change.
pub const FRAME_RESUME: u8 = 0x01;
pub const FRAME_RESUME_ACK: u8 = 0x02;
pub const FRAME_RESUME_REJECT: u8 = 0x03;

/// Why the server declined to resume — the wire vocabulary of
/// [`respond_resume_rejected`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeRejectReason {
    /// `auth_blob` did not verify.
    Auth,
    /// `token` does not name any session the server currently knows
    /// about (never existed, already resumed by a concurrent attempt, or
    /// evicted).
    UnknownToken,
    /// `token` is known, but the requested `client_delivered_offset` is no
    /// longer covered by the server's replay buffer (it already
    /// discarded that range).
    OffsetGone,
}

impl ResumeRejectReason {
    fn to_wire(self) -> u8 {
        match self {
            Self::Auth => 0,
            Self::UnknownToken => 1,
            Self::OffsetGone => 2,
        }
    }

    fn from_wire(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Auth),
            1 => Some(Self::UnknownToken),
            2 => Some(Self::OffsetGone),
            _ => None,
        }
    }
}

/// A resume attempt, as decoded off the wire by [`decode_resume_request`].
/// Every field is caller-interpreted — this crate only carries the bytes.
pub struct ResumeRequest {
    /// Opaque session identifier. This crate never interprets its bytes;
    /// the caller's own session bookkeeping (e.g. a `HashMap<Vec<u8>, _>`
    /// keyed on this) gives it meaning.
    pub token: Vec<u8>,
    /// Opaque authentication material. Verifying this against
    /// `conn_exporter` (and whatever secret the caller's own protocol
    /// keeps) is entirely the caller's responsibility — this crate has no
    /// authentication layer of its own, matching [`crate::MuxError::AuthenticationFailed`]'s
    /// docs.
    pub auth_blob: Vec<u8>,
    /// The *new* connection's TLS exporter — the caller's `auth_blob`
    /// verification almost always needs to bind the proof to this specific
    /// connection (e.g. `HMAC(secret, exporter || token)`), the same way
    /// `isekai-pipe`'s own resume proof does, to stop a captured
    /// `auth_blob` from being replayed against a different connection.
    pub conn_exporter: [u8; 32],
    /// How many bytes the client has sent on this logical session so far
    /// (caller-defined units — typically bytes on the caller's own
    /// application byte stream, not this frame's own wire bytes).
    pub client_sent_offset: u64,
    /// How many bytes of the caller's *previous* replay buffer the client
    /// has already received and processed — the offset the server should
    /// replay from.
    pub client_delivered_offset: u64,
}

fn encode_resume_request(req: &ResumeRequestToSend<'_>) -> Result<Vec<u8>, MuxError> {
    if req.token.len() > u16::MAX as usize {
        return Err(MuxError::ProtocolViolation("resume token too large to encode (max 65535 bytes)".to_string()));
    }
    if req.auth_blob.len() > u16::MAX as usize {
        return Err(MuxError::ProtocolViolation("resume auth_blob too large to encode (max 65535 bytes)".to_string()));
    }
    let mut buf = Vec::with_capacity(1 + 2 + req.token.len() + 2 + req.auth_blob.len() + 8 + 8);
    buf.push(FRAME_RESUME);
    buf.extend_from_slice(&(req.token.len() as u16).to_be_bytes());
    buf.extend_from_slice(req.token);
    buf.extend_from_slice(&(req.auth_blob.len() as u16).to_be_bytes());
    buf.extend_from_slice(req.auth_blob);
    buf.extend_from_slice(&req.client_sent_offset.to_be_bytes());
    buf.extend_from_slice(&req.client_delivered_offset.to_be_bytes());
    Ok(buf)
}

struct ResumeRequestToSend<'a> {
    token: &'a [u8],
    auth_blob: &'a [u8],
    client_sent_offset: u64,
    client_delivered_offset: u64,
}

/// Reads exactly `buf.len()` bytes, treating a clean EOF before that as an
/// error — [`AnyByteStreamReadHalf::read`]'s "at most `buf.len()`, possibly
/// fewer, `0` on EOF" contract is weaker than fixed-size frame decoding
/// needs (mirrors every other crate in this workspace's own private
/// `read_exact` helper — deliberately duplicated per this project's
/// convention rather than shared, see `isekai-transport::relay`'s module
/// docs).
async fn read_exact(recv: &mut AnyByteStreamReadHalf, buf: &mut [u8]) -> Result<(), MuxError> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = recv.read(&mut buf[filled..]).await?;
        if n == 0 {
            return Err(MuxError::StreamIo(format!(
                "stream ended before {} bytes were read (got {filled})",
                buf.len()
            )));
        }
        filled += n;
    }
    Ok(())
}

/// Decodes a [`ResumeRequest`]'s body from `recv` — **not including** the
/// leading [`FRAME_RESUME`] type byte itself. `pub` (unlike this module's
/// other frame-internals) for a caller whose own connection-dispatch
/// already reads the first frame-type byte itself before it knows which
/// kind of frame this is — e.g. `isekai-pipe serve`'s `handle_connection`,
/// which reads one byte to choose between its own `ATTACH_HELLO`/
/// `CancelAttach` frames and this module's `FRAME_RESUME`, so by the time it
/// knows to call this function the type byte is already consumed — which is
/// why this, and not a `accept_resume(conn, &dyn ResumeAcceptor)` wrapper
/// that does its own `accept_bi()`, is this module's server-side entry point
/// (see the module docs).
pub async fn decode_resume_request(recv: &mut AnyByteStreamReadHalf, conn_exporter: [u8; 32]) -> Result<ResumeRequest, MuxError> {
    let mut token_len = [0u8; 2];
    read_exact(recv, &mut token_len).await?;
    let mut token = vec![0u8; u16::from_be_bytes(token_len) as usize];
    read_exact(recv, &mut token).await?;

    let mut auth_len = [0u8; 2];
    read_exact(recv, &mut auth_len).await?;
    let mut auth_blob = vec![0u8; u16::from_be_bytes(auth_len) as usize];
    read_exact(recv, &mut auth_blob).await?;

    let mut sent = [0u8; 8];
    read_exact(recv, &mut sent).await?;
    let mut delivered = [0u8; 8];
    read_exact(recv, &mut delivered).await?;

    Ok(ResumeRequest {
        token,
        auth_blob,
        conn_exporter,
        client_sent_offset: u64::from_be_bytes(sent),
        client_delivered_offset: u64::from_be_bytes(delivered),
    })
}

/// The client side of a resume exchange: dials nothing itself (the caller
/// must already have a fresh [`AnyMuxConnection`] — e.g. via
/// [`crate::AnyMuxEndpoint::connect`]), opens a stream, sends the resume
/// request, and awaits the response.
pub async fn request_resume(
    conn: &AnyMuxConnection,
    token: &[u8],
    auth_blob: &[u8],
    client_sent_offset: u64,
    client_delivered_offset: u64,
) -> Result<ResumeAckOutcome, ResumeRequestError> {
    let stream = conn.open_bi().await.map_err(ResumeRequestError::Mux)?;
    let (mut recv, mut send) = stream.split();

    let frame = encode_resume_request(&ResumeRequestToSend { token, auth_blob, client_sent_offset, client_delivered_offset })
        .map_err(ResumeRequestError::Mux)?;
    send.write_all(&frame).await.map_err(ResumeRequestError::Mux)?;

    let mut frame_type = [0u8; 1];
    read_exact(&mut recv, &mut frame_type).await.map_err(ResumeRequestError::Mux)?;
    match frame_type[0] {
        FRAME_RESUME_ACK => {
            let mut committed = [0u8; 8];
            read_exact(&mut recv, &mut committed).await.map_err(ResumeRequestError::Mux)?;
            let mut sent_offset = [0u8; 8];
            read_exact(&mut recv, &mut sent_offset).await.map_err(ResumeRequestError::Mux)?;
            // Replay bytes are *not* part of this frame — see
            // `respond_resume_accepted`'s docs for why: they follow as plain,
            // unframed continuation of this same stream, so a caller reading
            // `ResumeAckOutcome::stream` normally afterward just sees them as
            // ordinary application data, exactly as if the connection had
            // never dropped.
            Ok(ResumeAckOutcome {
                committed_offset: u64::from_be_bytes(committed),
                sent_offset: u64::from_be_bytes(sent_offset),
                stream: AnyByteStream::unsplit(recv, send),
            })
        }
        FRAME_RESUME_REJECT => {
            let mut reason_byte = [0u8; 1];
            read_exact(&mut recv, &mut reason_byte).await.map_err(ResumeRequestError::Mux)?;
            let reason = ResumeRejectReason::from_wire(reason_byte[0])
                .ok_or_else(|| ResumeRequestError::Mux(MuxError::ProtocolViolation(format!("unknown resume reject reason byte {:#x}", reason_byte[0]))))?;
            Err(ResumeRequestError::Rejected(reason))
        }
        other => Err(ResumeRequestError::Mux(MuxError::ProtocolViolation(format!("unexpected resume response frame type {other:#x}")))),
    }
}

/// The result of a successful [`request_resume`]: the offsets the acceptor
/// reported, and the still-open stream (recombined via [`AnyByteStream::unsplit`]
/// — split only transiently during the request/response exchange itself) to
/// keep driving application traffic on — exactly the same connection the
/// resume request itself was sent on, now repurposed as the ongoing data
/// stream (mirrors `isekai-pipe`'s own `reconnect_and_resume`, whose
/// `RESUME` frame and subsequent application data share one stream).
///
/// Deliberately has **no** `replay: Vec<u8>` field: the acceptor's replay
/// bytes are *not* part of the `RESUME_ACK` frame this type is decoded
/// from — see [`respond_resume_accepted`]'s docs for why — they instead
/// arrive as ordinary, unframed data on `stream` itself, indistinguishable
/// to the caller from any other bytes the peer sends. A caller reading
/// `stream` normally after a successful resume sees them automatically;
/// there is nothing separate to consume.
pub struct ResumeAckOutcome {
    pub committed_offset: u64,
    pub sent_offset: u64,
    pub stream: AnyByteStream,
}

impl std::fmt::Debug for ResumeAckOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResumeAckOutcome")
            .field("committed_offset", &self.committed_offset)
            .field("sent_offset", &self.sent_offset)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResumeRequestError {
    #[error(transparent)]
    Mux(#[from] MuxError),
    #[error("resume rejected: {0:?}")]
    Rejected(ResumeRejectReason),
}

/// Writes a [`FRAME_RESUME_ACK`] response, followed by `replay` as plain,
/// unframed continuation of the same stream — deliberately **not** a
/// length-prefixed field inside the ACK frame itself. A caller resuming a
/// raw pass-through relay (every real caller in this workspace) just keeps
/// reading the stream after a successful resume exactly like it would any
/// other connection; from its point of view there is no distinction between
/// "replayed" and "newly arriving" bytes, only a running byte offset — so
/// framing replay as a separate structured field would force every such
/// caller to un-frame it again before feeding it back into the same plain
/// byte-stream abstraction it already uses everywhere else. `pub` for the
/// same reason as [`decode_resume_request`]: a caller integrating this into
/// its own existing frame dispatch needs to send the response itself.
pub async fn respond_resume_accepted(send: &mut AnyByteStreamWriteHalf, committed_offset: u64, sent_offset: u64, replay: &[u8]) -> Result<(), MuxError> {
    let mut ack = Vec::with_capacity(1 + 8 + 8);
    ack.push(FRAME_RESUME_ACK);
    ack.extend_from_slice(&committed_offset.to_be_bytes());
    ack.extend_from_slice(&sent_offset.to_be_bytes());
    send.write_all(&ack).await?;
    if !replay.is_empty() {
        send.write_all(replay).await?;
    }
    Ok(())
}

/// Writes a [`FRAME_RESUME_REJECT`] response and waits for the peer to
/// observe it (see [`crate::AnyByteStream::wait_for_close`]'s docs for why:
/// the same "peer never saw the response before the connection died" race
/// `isekai-pipe`'s own `reject()` exists to close) before returning.
/// Best-effort — a failure to write or observe close is not surfaced since
/// the caller is already on its way to reporting [`ResumeRejectReason`] as
/// the operative error; a secondary I/O failure while trying to tell the
/// peer about it isn't more actionable than that.
pub async fn respond_resume_rejected(send: &mut AnyByteStreamWriteHalf, reason: ResumeRejectReason) {
    let frame = [FRAME_RESUME_REJECT, reason.to_wire()];
    if send.write_all(&frame).await.is_ok() {
        let _ = send.shutdown().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), send.wait_for_close()).await;
    }
}

/// A generic, direction-agnostic bounded byte buffer keyed by absolute
/// offset — a caller uses one per direction it needs replay for (typically
/// just its own send direction; the peer's send direction is that peer's
/// own [`ReplayBuffer`]). Ported from `isekai-pipe`'s `OutputBuffer`, which
/// was already fully generic (no TCP/noq-specific type anywhere in it) —
/// moved here verbatim in spirit, with explicit overflow handling on the
/// offset arithmetic added (flagged in review as the class of bug this
/// project has hit before in adjacent offset-tracking code).
///
/// This is the **single** replay-buffer implementation in the workspace.
/// `isekai-pipe` previously carried two near-identical copies of it
/// (`engine::resume::OutputBuffer` on the server side, `resume_loop::
/// C2hReplayBuffer` on the client side) which had silently drifted apart on
/// [`Self::advance_start`]'s out-of-range behavior — see that method's docs
/// for which behavior is correct and why the other one would corrupt
/// offsets. Both now use this type.
///
/// # Overflow policy: reject, never evict
///
/// [`Self::append`] refuses to write anything that would exceed `capacity`
/// and reports that as `false`; it never evicts the oldest bytes to make
/// room. That is the only policy that makes sense for a *replay* buffer:
/// the oldest bytes are precisely the ones a resuming peer is most likely
/// to still need, so dropping them to accept newer ones converts a
/// recoverable "slow down" into an unrecoverable
/// [`ResumeRejectReason::OffsetGone`]. Callers are expected to apply real
/// backpressure by never reading more than [`Self::remaining_capacity`]
/// bytes from their source in the first place, which makes a `false` return
/// a should-never-happen defensive signal rather than routine flow control.
///
/// (`isekai-pipe`'s `tty::ring_buffer` deliberately does the opposite —
/// evicting oldest-first — because it is a scrollback buffer, where losing
/// the oldest lines is the *intended* behavior and there is no peer whose
/// offsets could desync. It is not a member of this family and must not be
/// folded into it.)
pub struct ReplayBuffer {
    data: VecDeque<u8>,
    start_offset: u64,
    capacity: usize,
}

impl ReplayBuffer {
    pub fn new(capacity: usize) -> Self {
        Self { data: VecDeque::with_capacity(capacity.min(1 << 20)), start_offset: 0, capacity }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn remaining_capacity(&self) -> usize {
        self.capacity.saturating_sub(self.data.len())
    }

    pub fn is_full(&self) -> bool {
        self.remaining_capacity() == 0
    }

    pub fn start_offset(&self) -> u64 {
        self.start_offset
    }

    /// `start_offset + data.len()`, saturating rather than panicking on
    /// overflow — at `capacity` bytes/append this would take billions of
    /// years to actually reach `u64::MAX` in practice, but a caller that
    /// somehow got here should see a stuck-at-max offset rather than a
    /// panic taking down the whole relay task.
    pub fn end_offset(&self) -> u64 {
        self.start_offset.saturating_add(self.data.len() as u64)
    }

    /// Appends `bytes`. Returns `false` (writing nothing) if `bytes` would
    /// exceed `remaining_capacity()` — the caller is expected to only ever
    /// read up to `remaining_capacity()` bytes from its own source before
    /// calling this, so this is a defensive check, not the primary
    /// backpressure mechanism. See the type's docs for why the answer to
    /// overflow is "refuse" and never "evict the oldest bytes".
    pub fn append(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() > self.remaining_capacity() {
            return false;
        }
        self.data.extend(bytes.iter().copied());
        true
    }

    /// Discards bytes the peer has confirmed receiving up to
    /// `confirmed_offset`.
    ///
    /// A `confirmed_offset` at or before the current `start_offset` is a
    /// no-op (already discarded, or a stale/duplicate ack) rather than an
    /// error — acks can legitimately arrive out of order or be resent.
    ///
    /// A `confirmed_offset` beyond `end_offset()` **clamps**: the buffer is
    /// drained and `start_offset` stops at the old `end_offset()`. It
    /// deliberately does *not* jump `start_offset` ahead to
    /// `confirmed_offset`, even though that would look like the tidier
    /// postcondition, because that is unsound in the presence of the
    /// send-then-append ordering every caller here uses:
    ///
    /// 1. The caller writes `n` bytes to the peer.
    /// 2. The caller appends those same `n` bytes to this buffer.
    ///
    /// Between (1) and (2) the peer can legitimately receive, process, and
    /// ack those bytes, so an ack naming an offset past the current
    /// `end_offset()` is a *normal race*, not a protocol violation —
    /// `isekai-pipe serve` hits exactly this window, since its ack reader
    /// runs in a task spawned separately from its relay loop and the two
    /// contend for the same session lock. Jumping `start_offset` to
    /// `confirmed_offset` there would mislabel the bytes that step (2) is
    /// about to append: they belong at the *old* `end_offset()`, so the
    /// buffer would then hand a resuming peer already-delivered bytes under
    /// a higher offset, silently duplicating data and desyncing both sides'
    /// offset accounting. Clamping keeps step (2)'s bytes correctly
    /// labelled.
    ///
    /// (`isekai-pipe`'s client-side copy of this buffer used to jump ahead
    /// instead. It was safe only by accident of its own structure — there,
    /// append and `advance_start` run sequentially in one task with append
    /// first, so the window never opens — and it was never reachable anyway,
    /// because `replay_and_advance` rejects an out-of-range offset via
    /// `replay_from` before calling this. Consolidating on the clamping
    /// behavior loses nothing and removes the trap.)
    pub fn advance_start(&mut self, confirmed_offset: u64) {
        while self.start_offset < confirmed_offset && !self.data.is_empty() {
            self.data.pop_front();
            self.start_offset += 1;
        }
    }

    /// Bytes from `from` (inclusive) to `end_offset()`. `None` if `from` is
    /// before `start_offset` (already discarded — the caller should treat
    /// this as [`ResumeRejectReason::OffsetGone`]) or after `end_offset()`
    /// (the peer is claiming to have received bytes that were never sent —
    /// a protocol violation, not a normal condition).
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
    fn append_and_replay_full_range() {
        let mut buf = ReplayBuffer::new(1024);
        assert!(buf.append(b"hello"));
        assert!(buf.append(b" world"));
        assert_eq!(buf.end_offset(), 11);
        assert_eq!(buf.replay_from(0).unwrap(), b"hello world");
        assert_eq!(buf.replay_from(5).unwrap(), b" world");
        assert_eq!(buf.replay_from(11).unwrap(), b"");
    }

    #[test]
    fn replay_from_beyond_end_is_none() {
        let mut buf = ReplayBuffer::new(1024);
        assert!(buf.append(b"hi"));
        assert!(buf.replay_from(3).is_none());
    }

    #[test]
    fn replay_from_before_start_is_none_after_advance() {
        let mut buf = ReplayBuffer::new(1024);
        assert!(buf.append(b"0123456789"));
        buf.advance_start(4);
        assert_eq!(buf.start_offset(), 4);
        assert_eq!(buf.replay_from(4).unwrap(), b"456789");
        assert!(buf.replay_from(0).is_none(), "discarded range should be None");
    }

    #[test]
    fn advance_start_is_a_no_op_for_a_stale_or_duplicate_ack() {
        let mut buf = ReplayBuffer::new(1024);
        assert!(buf.append(b"0123456789"));
        buf.advance_start(6);
        buf.advance_start(6); // duplicate ack
        buf.advance_start(2); // stale ack (older than current start_offset)
        assert_eq!(buf.start_offset(), 6);
        assert_eq!(buf.replay_from(6).unwrap(), b"6789");
    }

    #[test]
    fn capacity_overflow_is_rejected_without_evicting_oldest_bytes() {
        let mut buf = ReplayBuffer::new(4);
        assert!(buf.append(b"abcd"));
        assert!(!buf.append(b"e"));
        assert_eq!(buf.start_offset(), 0);
        assert_eq!(buf.end_offset(), 4);
        assert_eq!(buf.len(), 4);
        assert!(buf.is_full());
        assert_eq!(buf.remaining_capacity(), 0);
        assert_eq!(buf.replay_from(0).unwrap(), b"abcd");
    }

    #[test]
    fn advance_start_frees_capacity_for_later_appends() {
        let mut buf = ReplayBuffer::new(10);
        assert!(buf.append(b"abcdefghij"));
        assert!(buf.is_full());
        buf.advance_start(6);
        assert_eq!(buf.remaining_capacity(), 6);
        assert!(buf.append(b"klmnop"));
        assert_eq!(buf.end_offset(), 16);
        assert_eq!(buf.replay_from(6).unwrap(), b"ghijklmnop");
    }

    /// Regression guard for the send-then-append race described in
    /// `advance_start`'s docs: an ack that names bytes already written to
    /// the peer but not yet appended here must clamp, so that the append
    /// which follows still lands at the offset those bytes actually have.
    /// The "jump `start_offset` to `confirmed_offset`" variant this
    /// consolidation removed would leave `start_offset == 110` here and then
    /// mislabel `"0123456789"` as offsets 110..120.
    #[test]
    fn advance_start_past_end_clamps_so_a_racing_append_stays_correctly_labelled() {
        let mut buf = ReplayBuffer::new(64);
        assert!(buf.append(b"0123456789"));
        buf.advance_start(10);
        assert_eq!(buf.start_offset(), 10);
        assert!(buf.is_empty());

        // The caller has just written offsets 10..20 to the peer but has not
        // reached its own `append` yet; the peer acks all 20 bytes first.
        buf.advance_start(20);
        assert_eq!(buf.start_offset(), 10, "must clamp at end_offset, not jump to 20");
        assert_eq!(buf.end_offset(), 10);

        // Now the append lands. Those bytes are offsets 10..20 and must be
        // replayable as such.
        assert!(buf.append(b"abcdefghij"));
        assert_eq!(buf.start_offset(), 10);
        assert_eq!(buf.end_offset(), 20);
        assert_eq!(buf.replay_from(10).unwrap(), b"abcdefghij");
        assert_eq!(buf.replay_from(20).unwrap(), b"");
        // The ack is re-delivered (the peer resends it); still consistent.
        buf.advance_start(20);
        assert_eq!(buf.start_offset(), 20);
        assert!(buf.is_empty());
    }

    #[test]
    fn end_offset_saturates_instead_of_panicking_near_u64_max() {
        let mut buf = ReplayBuffer::new(4);
        buf.start_offset = u64::MAX - 1;
        assert!(buf.append(b"ab"));
        assert_eq!(buf.end_offset(), u64::MAX, "end_offset should saturate, not panic, once start_offset is near u64::MAX");
    }

    #[test]
    fn resume_reject_reason_wire_roundtrip() {
        for reason in [ResumeRejectReason::Auth, ResumeRejectReason::UnknownToken, ResumeRejectReason::OffsetGone] {
            assert_eq!(ResumeRejectReason::from_wire(reason.to_wire()), Some(reason));
        }
        assert_eq!(ResumeRejectReason::from_wire(0xEF), None);
    }

    #[test]
    fn encode_resume_request_rejects_oversized_token() {
        let token = vec![0u8; u16::MAX as usize + 1];
        let req = ResumeRequestToSend { token: &token, auth_blob: b"", client_sent_offset: 0, client_delivered_offset: 0 };
        assert!(matches!(encode_resume_request(&req), Err(MuxError::ProtocolViolation(_))));
    }
}

/// End-to-end tests driving [`request_resume`] against the same direct
/// server-side API the real server (`isekai-pipe serve`'s
/// `handle_connection`) uses — [`decode_resume_request`] +
/// [`respond_resume_accepted`]/[`respond_resume_rejected`] — over a real
/// `noq` connection. The unit tests above cover the pure encode/decode/
/// buffer logic in isolation, but the framing itself (frame-type byte
/// ordering, length-prefixed fields, stream reuse after the handshake) is
/// only genuinely exercised by actually round-tripping bytes over a live
/// connection.
///
/// These deliberately do *not* go through a test-only acceptor abstraction:
/// the removed `accept_resume`/`ResumeAcceptor` pair made these tests assert
/// against a code path no shipped binary ever executed (see the module docs).
#[cfg(all(test, feature = "noq"))]
mod noq_e2e_tests {
    use super::*;
    use crate::config::{MuxClientConfig, MuxServerConfig};
    use crate::types::BindSpec;
    use crate::{AnyMuxConnection, AnyMuxFactory, AnyMuxListener};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn test_client_config() -> MuxClientConfig {
        MuxClientConfig {
            alpn: b"quicmux-resume-test/1".to_vec(),
            exporter_label: b"quicmux-resume-test-exporter".to_vec(),
            max_idle_timeout: std::time::Duration::from_secs(15),
            keep_alive_interval: std::time::Duration::from_secs(5),
            max_concurrent_bidi_streams: 2,
            max_concurrent_uni_streams: 0,
            multipath: false,
            datagram_send_buffer_size: None,
        }
    }

    fn test_server_config() -> (MuxServerConfig, String) {
        let cert = rcgen::generate_simple_self_signed(vec!["quicmux-resume-test.local".to_string()]).unwrap();
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().clone());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
        let cert_sha256_hex = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(cert_der.as_ref());
            hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let config = MuxServerConfig {
            alpn: test_client_config().alpn,
            exporter_label: test_client_config().exporter_label,
            max_idle_timeout: std::time::Duration::from_secs(15),
            keep_alive_interval: std::time::Duration::from_secs(5),
            max_concurrent_bidi_streams: 2,
            max_concurrent_uni_streams: 0,
            multipath: false,
            datagram_send_buffer_size: None,
            cert_chain: vec![cert_der],
            private_key: key_der,
        };
        (config, cert_sha256_hex)
    }

    /// Reproduces `isekai-pipe serve`'s `handle_connection` server-side
    /// preamble exactly: accept the stream, read the single frame-type byte
    /// its own dispatch reads before it knows this is a resume at all, then
    /// hand the *rest* of the body to [`decode_resume_request`]. Returning
    /// the split halves lets each test decide the response itself, which is
    /// the whole point — the decision is the caller's, not this crate's.
    async fn accept_resume_request(
        conn: &AnyMuxConnection,
    ) -> (ResumeRequest, AnyByteStreamReadHalf, AnyByteStreamWriteHalf) {
        let conn_exporter = conn
            .export_keying_material(b"quicmux-resume-test-v1", b"")
            .await
            .expect("export_keying_material failed");
        let stream = conn.accept_bi().await.expect("accept_bi failed");
        let (mut recv, send) = stream.split();
        let mut frame_type = [0u8; 1];
        read_exact(&mut recv, &mut frame_type).await.expect("frame type read failed");
        assert_eq!(frame_type[0], FRAME_RESUME, "client should have sent a RESUME frame");
        let request = decode_resume_request(&mut recv, conn_exporter).await.expect("decode_resume_request failed");
        (request, recv, send)
    }

    async fn connect_pair(server_config: MuxServerConfig, cert_sha256_hex: String) -> (AnyMuxConnection, AnyMuxConnection) {
        let listener = AnyMuxListener::bind_noq(server_config, BindSpec::any_ipv4()).await.expect("listener bind failed");
        let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listener.local_addr().unwrap().port());

        let server_conn_task = tokio::spawn(async move {
            let incoming = listener.accept().await.expect("no incoming connection");
            incoming.accept().await.expect("server handshake failed")
        });

        let factory = AnyMuxFactory::noq(test_client_config());
        let endpoint = factory.create_endpoint(BindSpec::any_ipv4()).await.expect("create_endpoint failed");
        let client_conn = endpoint
            .connect(crate::types::RemoteSpec { addr: server_addr, server_name: "quicmux-resume-test.local".to_string(), cert_sha256_hex })
            .await
            .expect("client connect failed");
        let server_conn = server_conn_task.await.expect("server task panicked");
        (client_conn, server_conn)
    }

    #[tokio::test]
    async fn request_resume_roundtrips_against_the_direct_server_api_on_acceptance() {
        let (server_config, cert_sha256_hex) = test_server_config();
        let (client_conn, server_conn) = connect_pair(server_config, cert_sha256_hex).await;

        let server_task = tokio::spawn(async move {
            let (request, recv, mut send) = accept_resume_request(&server_conn).await;
            // Every field the client passed must survive the wire exactly —
            // this is the only place the length-prefixed encoding is checked
            // against a real decode rather than a same-process unit test.
            assert_eq!(request.token, b"session-42");
            assert_eq!(request.auth_blob, b"proof-bytes");
            assert_eq!(request.client_sent_offset, 300);
            assert_eq!(request.client_delivered_offset, 190);
            respond_resume_accepted(&mut send, 100, 200, b"tail bytes")
                .await
                .expect("respond_resume_accepted failed");
            AnyByteStream::unsplit(recv, send)
        });

        let outcome = request_resume(&client_conn, b"session-42", b"proof-bytes", 300, 190).await.expect("resume should be accepted");
        assert_eq!(outcome.committed_offset, 100);
        assert_eq!(outcome.sent_offset, 200);

        let mut server_stream = server_task.await.expect("server task panicked");
        let mut client_stream = outcome.stream;

        // `replay` isn't a separate field on `ResumeAckOutcome` — it arrives
        // as plain, unframed data on `stream` itself (see
        // `respond_resume_accepted`'s docs). Read it back exactly like any
        // other application data to prove that.
        let mut buf = [0u8; 32];
        let n = client_stream.read(&mut buf).await.expect("replay read failed");
        assert_eq!(&buf[..n], b"tail bytes", "replay bytes should arrive as ordinary stream data");

        // Both sides should still be able to drive the same stream as an
        // ongoing data stream after the resume handshake — prove it with one
        // more write from the server side.
        server_stream.write_all(b"post-resume").await.expect("post-resume write failed");
        let n = client_stream.read(&mut buf).await.expect("post-resume read failed");
        assert_eq!(&buf[..n], b"post-resume");
    }

    #[tokio::test]
    async fn request_resume_surfaces_rejection_and_the_peer_observes_it_before_the_connection_closes() {
        let (server_config, cert_sha256_hex) = test_server_config();
        let (client_conn, server_conn) = connect_pair(server_config, cert_sha256_hex).await;

        let server_task = tokio::spawn(async move {
            let (request, _recv, mut send) = accept_resume_request(&server_conn).await;
            assert_eq!(request.token, b"wrong-token");
            respond_resume_rejected(&mut send, ResumeRejectReason::UnknownToken).await;
        });

        let err = request_resume(&client_conn, b"wrong-token", b"proof-bytes", 0, 0).await.expect_err("unknown token should be rejected");
        assert!(matches!(err, ResumeRequestError::Rejected(ResumeRejectReason::UnknownToken)));

        server_task.await.expect("server task panicked");
    }

    /// Every [`ResumeRejectReason`] must survive the real wire, not just the
    /// in-process `to_wire`/`from_wire` round-trip the unit tests cover: a
    /// reason byte that decodes to something else (or to a
    /// `ProtocolViolation`) on the client would turn a recoverable, retryable
    /// rejection into an opaque failure, which is precisely the class of
    /// "never automatically recovers" bug `.claude/rules/always-connects.md`
    /// exists to prevent.
    #[tokio::test]
    async fn every_reject_reason_survives_the_real_wire() {
        for reason in [ResumeRejectReason::Auth, ResumeRejectReason::UnknownToken, ResumeRejectReason::OffsetGone] {
            let (server_config, cert_sha256_hex) = test_server_config();
            let (client_conn, server_conn) = connect_pair(server_config, cert_sha256_hex).await;

            let server_task = tokio::spawn(async move {
                let (_request, _recv, mut send) = accept_resume_request(&server_conn).await;
                respond_resume_rejected(&mut send, reason).await;
            });

            let err = request_resume(&client_conn, b"session-42", b"proof-bytes", 0, 0)
                .await
                .expect_err("the server rejected, so the client must see an error");
            match err {
                ResumeRequestError::Rejected(observed) => assert_eq!(observed, reason),
                other => panic!("expected Rejected({reason:?}), got {other:?}"),
            }
            server_task.await.expect("server task panicked");
        }
    }
}
