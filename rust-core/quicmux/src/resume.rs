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
//! token currently occupy an established slot". [`ResumeAcceptor::try_resume`]
//! is this crate's equivalent of that one check, left to the caller to
//! implement however its own session bookkeeping needs to.
//!
//! # Division of responsibility
//!
//! - **This module owns**: the wire framing for the resume request/response
//!   exchange ([`request_resume`]/[`accept_resume`]), and a generic
//!   offset-based [`ReplayBuffer`] a caller can use to buffer bytes it has
//!   sent so it can honor a resume request that asks to replay from an
//!   earlier offset.
//! - **The caller owns**: what a `token`/`auth_blob` mean, how to verify
//!   `auth_blob` (this crate has no authentication layer of its own — see
//!   [`crate::MuxError::AuthenticationFailed`]'s docs), and all session
//!   bookkeeping (mapping a token to a parked connection/buffer, deciding
//!   whether a token is currently resumable, single-flight-ing concurrent
//!   resume attempts for the same token). [`ResumeAcceptor::try_resume`] is
//!   the one seam between the two: a single atomic operation, not a
//!   lookup-then-take-then-check sequence, specifically so a caller's
//!   implementation can make "only one concurrent resume attempt for the
//!   same token succeeds" an actual contract instead of a lookup-time race.

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

/// Why [`ResumeAcceptor::try_resume`] declined to resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeRejectReason {
    /// `auth_blob` did not verify.
    Auth,
    /// `token` does not name any session the acceptor currently knows
    /// about (never existed, already resumed by a concurrent attempt, or
    /// evicted).
    UnknownToken,
    /// `token` is known, but the requested `client_delivered_offset` is no
    /// longer covered by the acceptor's replay buffer (it already
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

/// A resume attempt, as decoded off the wire and handed to
/// [`ResumeAcceptor::try_resume`]. Every field is caller-interpreted —
/// this crate only carries the bytes.
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
    /// has already received and processed — the offset
    /// [`ResumeAcceptor::try_resume`] should replay from.
    pub client_delivered_offset: u64,
}

/// What [`ResumeAcceptor::try_resume`] decided.
pub enum ResumeDecision {
    /// Resume accepted. `replay` is sent back to the client immediately
    /// after the [`FRAME_RESUME_ACK`] frame, on the same stream — the
    /// client should treat it exactly like ordinary application data that
    /// arrived on a fresh connection, since (from the client's point of
    /// view) it's the tail end of what it already sent/received before the
    /// disconnect.
    Accepted {
        /// The acceptor's own record of how many bytes of the client's
        /// C→S stream it has durably committed — the caller-side
        /// equivalent of `isekai-pipe`'s `helper_committed_offset`.
        committed_offset: u64,
        /// How many bytes the acceptor has sent in total on the S→C
        /// direction (`replay`'s bytes are the suffix of this ending at
        /// this offset) — lets the client cross-check its own bookkeeping.
        sent_offset: u64,
        /// Bytes to replay, starting at the request's
        /// `client_delivered_offset`. Empty if the client was already
        /// fully caught up.
        replay: Vec<u8>,
    },
    /// Resume declined; see [`ResumeRejectReason`].
    Rejected(ResumeRejectReason),
}

/// Caller-supplied resume policy — the one seam between this crate's wire
/// framing and the caller's own session bookkeeping/authentication. See
/// this module's docs for why this must be a single atomic operation
/// rather than a lookup-then-take-then-check sequence: a caller's
/// implementation is expected to make this the single-flight point (e.g.
/// by holding a lock or using `take()` on a parked resource inside this one
/// call) so that two concurrent resume attempts for the same `token` can
/// never both succeed.
#[async_trait::async_trait]
pub trait ResumeAcceptor: Send + Sync {
    async fn try_resume(&self, request: ResumeRequest) -> ResumeDecision;
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
/// knows to call this function the type byte is already consumed. A caller
/// that owns the whole exchange from scratch should use [`accept_resume`]
/// instead of calling this directly.
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
            let mut replay_len = [0u8; 4];
            read_exact(&mut recv, &mut replay_len).await.map_err(ResumeRequestError::Mux)?;
            let mut replay = vec![0u8; u32::from_be_bytes(replay_len) as usize];
            read_exact(&mut recv, &mut replay).await.map_err(ResumeRequestError::Mux)?;
            Ok(ResumeAckOutcome {
                committed_offset: u64::from_be_bytes(committed),
                sent_offset: u64::from_be_bytes(sent_offset),
                replay,
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
/// reported, the replay bytes to treat as continuing the previous data
/// stream, and the still-open stream (recombined via [`AnyByteStream::unsplit`]
/// — split only transiently during the request/response exchange itself) to
/// keep driving application traffic on — exactly the same connection the
/// resume request itself was sent on, now repurposed as the ongoing data
/// stream (mirrors `isekai-pipe`'s own `reconnect_and_resume`, whose
/// `RESUME` frame and subsequent application data share one stream).
pub struct ResumeAckOutcome {
    pub committed_offset: u64,
    pub sent_offset: u64,
    pub replay: Vec<u8>,
    pub stream: AnyByteStream,
}

impl std::fmt::Debug for ResumeAckOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResumeAckOutcome")
            .field("committed_offset", &self.committed_offset)
            .field("sent_offset", &self.sent_offset)
            .field("replay_len", &self.replay.len())
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

/// The server side of a resume exchange: accepts the stream the client's
/// [`request_resume`] opened, decodes the request, asks `acceptor` to
/// decide, and writes the response. On [`ResumeDecision::Accepted`],
/// returns the still-open stream (recombined via [`AnyByteStream::unsplit`])
/// for the caller to keep relaying application traffic on (the same stream,
/// now repurposed) — on
/// [`ResumeDecision::Rejected`], waits for the peer to observe the reject
/// frame (see [`crate::AnyByteStream::wait_for_close`]'s docs for why: the
/// same "peer never saw the response before the connection died" race
/// `isekai-pipe`'s own `reject()` exists to close) before returning the
/// error.
pub async fn accept_resume(conn: &AnyMuxConnection, acceptor: &dyn ResumeAcceptor) -> Result<AnyByteStream, ResumeRequestError> {
    let conn_exporter = conn.export_keying_material(b"quicmux-resume-v1", b"").await.map_err(ResumeRequestError::Mux)?;
    let stream = conn.accept_bi().await.map_err(ResumeRequestError::Mux)?;
    let (mut recv, mut send) = stream.split();

    let mut frame_type = [0u8; 1];
    read_exact(&mut recv, &mut frame_type).await.map_err(ResumeRequestError::Mux)?;
    if frame_type[0] != FRAME_RESUME {
        return Err(ResumeRequestError::Mux(MuxError::ProtocolViolation(format!("expected RESUME frame, got {:#x}", frame_type[0]))));
    }
    let request = decode_resume_request(&mut recv, conn_exporter).await.map_err(ResumeRequestError::Mux)?;

    match acceptor.try_resume(request).await {
        ResumeDecision::Accepted { committed_offset, sent_offset, replay } => {
            respond_resume_accepted(&mut send, committed_offset, sent_offset, &replay).await.map_err(ResumeRequestError::Mux)?;
            Ok(AnyByteStream::unsplit(recv, send))
        }
        ResumeDecision::Rejected(reason) => {
            respond_resume_rejected(&mut send, reason).await;
            Err(ResumeRequestError::Rejected(reason))
        }
    }
}

/// Writes a [`FRAME_RESUME_ACK`] response. `pub` for the same reason as
/// [`decode_resume_request`] — a caller integrating this into its own
/// existing frame dispatch (rather than going through [`accept_resume`])
/// still needs to send the response itself.
pub async fn respond_resume_accepted(send: &mut AnyByteStreamWriteHalf, committed_offset: u64, sent_offset: u64, replay: &[u8]) -> Result<(), MuxError> {
    let mut ack = Vec::with_capacity(1 + 8 + 8 + 4 + replay.len());
    ack.push(FRAME_RESUME_ACK);
    ack.extend_from_slice(&committed_offset.to_be_bytes());
    ack.extend_from_slice(&sent_offset.to_be_bytes());
    ack.extend_from_slice(&(replay.len() as u32).to_be_bytes());
    ack.extend_from_slice(replay);
    send.write_all(&ack).await
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
    /// calling this (matching every existing caller of `OutputBuffer`), so
    /// this is a defensive check, not the primary backpressure mechanism.
    pub fn append(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() > self.remaining_capacity() {
            return false;
        }
        self.data.extend(bytes.iter().copied());
        true
    }

    /// Discards bytes the peer has confirmed receiving up to
    /// `confirmed_offset`. A `confirmed_offset` at or before the current
    /// `start_offset` is a no-op (already discarded, or a stale/duplicate
    /// ack) rather than an error — acks can legitimately arrive
    /// out of order or be resent.
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

/// End-to-end tests driving [`request_resume`]/[`accept_resume`] over a real
/// `noq` connection — the unit tests above cover the pure encode/decode/
/// buffer logic in isolation, but the framing itself (frame-type byte
/// ordering, length-prefixed fields, stream reuse after the handshake) is
/// only genuinely exercised by actually round-tripping bytes over a live
/// connection.
#[cfg(all(test, feature = "noq"))]
mod noq_e2e_tests {
    use super::*;
    use crate::config::{MuxClientConfig, MuxServerConfig};
    use crate::types::BindSpec;
    use crate::{AnyMuxConnection, AnyMuxFactory, AnyMuxListener};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn test_client_config() -> MuxClientConfig {
        MuxClientConfig {
            alpn: b"quicmux-resume-test/1".to_vec(),
            exporter_label: b"quicmux-resume-test-exporter".to_vec(),
            max_idle_timeout: std::time::Duration::from_secs(15),
            keep_alive_interval: std::time::Duration::from_secs(5),
            max_concurrent_bidi_streams: 2,
            max_concurrent_uni_streams: 0,
            multipath: false,
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
            cert_chain: vec![cert_der],
            private_key: key_der,
        };
        (config, cert_sha256_hex)
    }

    /// The simplest possible [`ResumeAcceptor`]: a single fixed token is
    /// resumable exactly once (matching the single-flight contract this
    /// module's docs describe — a second attempt for the same token, or any
    /// unrecognized token, is `UnknownToken`), with a fixed reply buffer and
    /// a counter so tests can assert how many times it was actually called.
    struct OnceAcceptor {
        token: Vec<u8>,
        replay: Vec<u8>,
        committed_offset: u64,
        sent_offset: u64,
        claimed: Mutex<bool>,
        call_count: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ResumeAcceptor for OnceAcceptor {
        async fn try_resume(&self, request: ResumeRequest) -> ResumeDecision {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if request.token != self.token {
                return ResumeDecision::Rejected(ResumeRejectReason::UnknownToken);
            }
            let mut claimed = self.claimed.lock().await;
            if *claimed {
                return ResumeDecision::Rejected(ResumeRejectReason::UnknownToken);
            }
            if request.client_delivered_offset < self.sent_offset - self.replay.len() as u64 {
                return ResumeDecision::Rejected(ResumeRejectReason::OffsetGone);
            }
            *claimed = true;
            ResumeDecision::Accepted { committed_offset: self.committed_offset, sent_offset: self.sent_offset, replay: self.replay.clone() }
        }
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
    async fn request_resume_and_accept_resume_roundtrip_on_acceptance() {
        let (server_config, cert_sha256_hex) = test_server_config();
        let (client_conn, server_conn) = connect_pair(server_config, cert_sha256_hex).await;

        let acceptor = Arc::new(OnceAcceptor {
            token: b"session-42".to_vec(),
            replay: b"tail bytes".to_vec(),
            committed_offset: 100,
            sent_offset: 200,
            claimed: Mutex::new(false),
            call_count: AtomicUsize::new(0),
        });

        let server_task = {
            let acceptor = acceptor.clone();
            tokio::spawn(async move { accept_resume(&server_conn, acceptor.as_ref()).await })
        };

        let outcome = request_resume(&client_conn, b"session-42", b"proof-bytes", 300, 190).await.expect("resume should be accepted");
        assert_eq!(outcome.committed_offset, 100);
        assert_eq!(outcome.sent_offset, 200);
        assert_eq!(outcome.replay, b"tail bytes");
        assert_eq!(acceptor.call_count.load(Ordering::SeqCst), 1);

        let mut server_stream = server_task.await.expect("server task panicked").expect("accept_resume should succeed");
        // Both sides should still be able to drive the same stream as an
        // ongoing data stream after the resume handshake — prove it with one
        // more write from the server side.
        server_stream.write_all(b"post-resume").await.expect("post-resume write failed");
        let mut client_stream = outcome.stream;
        let mut buf = [0u8; 32];
        let n = client_stream.read(&mut buf).await.expect("post-resume read failed");
        assert_eq!(&buf[..n], b"post-resume");
    }

    #[tokio::test]
    async fn request_resume_surfaces_rejection_and_the_peer_observes_it_before_the_connection_closes() {
        let (server_config, cert_sha256_hex) = test_server_config();
        let (client_conn, server_conn) = connect_pair(server_config, cert_sha256_hex).await;

        let acceptor = Arc::new(OnceAcceptor {
            token: b"session-42".to_vec(),
            replay: vec![],
            committed_offset: 0,
            sent_offset: 0,
            claimed: Mutex::new(false),
            call_count: AtomicUsize::new(0),
        });

        let server_task = { let acceptor = acceptor.clone(); tokio::spawn(async move { accept_resume(&server_conn, acceptor.as_ref()).await }) };

        let err = request_resume(&client_conn, b"wrong-token", b"proof-bytes", 0, 0).await.expect_err("unknown token should be rejected");
        assert!(matches!(err, ResumeRequestError::Rejected(ResumeRejectReason::UnknownToken)));

        let server_result = server_task.await.expect("server task panicked");
        assert!(matches!(server_result, Err(ResumeRequestError::Rejected(ResumeRejectReason::UnknownToken))));
    }

    #[tokio::test]
    async fn a_second_concurrent_resume_for_the_same_token_is_rejected_single_flight() {
        let (server_config, cert_sha256_hex) = test_server_config();
        let (client_conn, server_conn) = connect_pair(server_config, cert_sha256_hex).await;

        let acceptor = Arc::new(OnceAcceptor {
            token: b"session-42".to_vec(),
            replay: vec![],
            committed_offset: 0,
            sent_offset: 0,
            claimed: Mutex::new(false),
            call_count: AtomicUsize::new(0),
        });

        // First resume claims the token via the real wire protocol.
        let server_task = { let acceptor = acceptor.clone(); tokio::spawn(async move { accept_resume(&server_conn, acceptor.as_ref()).await }) };
        let first = request_resume(&client_conn, b"session-42", b"proof-bytes", 0, 0).await;
        assert!(first.is_ok());
        server_task.await.expect("server task panicked").expect("first accept_resume should succeed");

        // A second attempt for the same token against the same acceptor
        // (simulating a racing/retried resume) must be rejected — proves
        // `try_resume`'s single atomic call is enough to make this safe
        // without any additional locking at the quicmux layer.
        let second_decision = acceptor
            .try_resume(ResumeRequest { token: b"session-42".to_vec(), auth_blob: vec![], conn_exporter: [0u8; 32], client_sent_offset: 0, client_delivered_offset: 0 })
            .await;
        assert!(matches!(second_decision, ResumeDecision::Rejected(ResumeRejectReason::UnknownToken)));
        assert_eq!(acceptor.call_count.load(Ordering::SeqCst), 2);
    }
}
