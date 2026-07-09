//! ATTACH v2: race-safe attach/fencing wire types (`#18`, ChatGPTセカンドオピニオン
//! 2026-07-08 2ラウンド目で確定した設計). Replaces the single `active: AtomicBool`
//! compare-exchange isekai-pipe serve currently uses to reject a second HELLO
//! with a granular, generation-based winner-determination scheme.
//!
//! Three identifiers are deliberately kept separate rather than folded into a
//! single "attempt_id":
//! - [`SessionId`] (existing type, reused as-is): the logical SSH session.
//!   Shared by every candidate in every round, and by resume.
//! - [`ConnectionGeneration`]: the fencing token itself. Belongs to a *round*
//!   of candidates, not to an individual candidate — only the client-side
//!   orchestrator (`isekai-transport`'s future `GenerationCoordinator`, `#18-5`)
//!   advances it, and only when an ambiguous post-attach failure requires
//!   safely superseding a possibly-still-committed earlier round.
//! - [`AttemptId`]: identifies one candidate's HELLO within its generation.
//!   Has no ordering — it cannot be used for fencing by itself, only for
//!   telling two same-generation attempts apart (`ALREADY_ATTACHED` between
//!   them).
//!
//! Winner rule (not simple first-write-wins nor last-write-wins):
//! **within one generation, the first authenticated ATTACH_HELLO the server
//! accepts wins; a strictly larger generation may supersede an
//! earlier generation that has not yet reached `Established`.** This is what
//! lets `#19`'s concurrent direct/relay race (same generation, first commit
//! wins) and `#25`'s post-`AmbiguousAfterAttach` continuation (new generation
//! safely fences the old one) share one mechanism.
//!
//! Proof computation itself (HMAC over the live QUIC connection's exporter,
//! mirroring `hello::Proof`/`resume::ResumeProof`) stays out of this I/O-free
//! crate; [`attach_hello_proof_transcript`]/[`cancel_attach_proof_transcript`]
//! only build the pure "extra" bytes that `isekai-transport::proof::compute_proof`
//! feeds into the HMAC alongside the exporter, so that altering the frame
//! type, `session_id`, `generation`, `attempt_id`, or options invalidates the
//! proof (domain separation prevents a captured HELLO proof from being
//! replayed as a valid CANCEL proof for the same identifiers, and vice
//! versa).

use crate::error::ProtocolError;
use crate::session_id::{decode_session_id, SessionId, SESSION_ID_LEN};
use crate::version::CURRENT_PROTOCOL_VERSION;

pub const ATTEMPT_ID_LEN: usize = 16;
pub const ATTACH_TOKEN_LEN: usize = 16;
pub const ATTACH_PROOF_LEN: usize = 32;
pub const GENERATION_LEN: usize = 8;

pub const FRAME_ATTACH_HELLO: u8 = 0x30;
pub const FRAME_ATTACH_READY: u8 = 0x31;
pub const FRAME_ATTACH_ACTIVATE: u8 = 0x32;
pub const FRAME_ATTACH_CANCEL: u8 = 0x33;

/// Reuses `hello::FRAME_REJECT_AUTH`/`FRAME_REJECT_TARGET`/`FRAME_REJECT_UNSUPPORTED`
/// for the reasons that carry over unchanged from v1 HELLO/ACK; only the
/// fencing-specific reject reasons below are new to ATTACH v2.
pub const FRAME_REJECT_ALREADY_ATTACHED: u8 = 0xF0;
pub const FRAME_REJECT_STALE_GENERATION: u8 = 0xF1;
pub const FRAME_REJECT_BUSY_OTHER_SESSION: u8 = 0xF2;
pub const FRAME_REJECT_ATTACH_ALREADY_ESTABLISHED: u8 = 0xF3;

/// Domain-separation strings folded into each proof's transcript so a proof
/// computed for one frame kind can never validate a different one, even when
/// `session_id`/`generation`/`attempt_id` are all identical.
pub const ATTACH_HELLO_PROOF_DOMAIN: &[u8] = b"isekai-pipe/attach/v2/hello";
pub const CANCEL_ATTACH_PROOF_DOMAIN: &[u8] = b"isekai-pipe/attach/v2/cancel";

/// `1` (type) + `session_id` + `generation` + `attempt_id` +
/// `requested_resume_grace_secs` (`u32`) + `proof`.
pub const ATTACH_HELLO_FRAME_LEN: usize = 1 + SESSION_ID_LEN + GENERATION_LEN + ATTEMPT_ID_LEN + 4 + ATTACH_PROOF_LEN;
/// `1` (type) + `session_id` + `generation` + `attempt_id` +
/// `negotiated_resume_grace_secs` (`u32`) + `attach_token`.
pub const ATTACH_READY_FRAME_LEN: usize = 1 + SESSION_ID_LEN + GENERATION_LEN + ATTEMPT_ID_LEN + 4 + ATTACH_TOKEN_LEN;
/// `1` (type) + `session_id` + `generation` + `attempt_id` + `attach_token`.
pub const ATTACH_ACTIVATE_FRAME_LEN: usize = 1 + SESSION_ID_LEN + GENERATION_LEN + ATTEMPT_ID_LEN + ATTACH_TOKEN_LEN;
/// `1` (type) + `session_id` + `generation` + `attempt_id` + `proof`.
pub const CANCEL_ATTACH_FRAME_LEN: usize = 1 + SESSION_ID_LEN + GENERATION_LEN + ATTEMPT_ID_LEN + ATTACH_PROOF_LEN;
/// `STALE_GENERATION` is the only reject reason with a payload beyond the
/// type byte: the server's `current_generation`, so the client knows how far
/// ahead to jump rather than incrementing by one and potentially colliding
/// again.
pub const STALE_GENERATION_REJECT_FRAME_LEN: usize = 1 + GENERATION_LEN;

/// Identifies one candidate's HELLO within a [`ConnectionGeneration`]. Purely
/// a correlation identifier — unlike `generation`, it has no ordering and
/// cannot by itself be used to decide a winner (`ALREADY_ATTACHED` is what
/// tells two same-generation attempts apart).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttemptId([u8; ATTEMPT_ID_LEN]);

impl AttemptId {
    pub fn from_bytes(bytes: [u8; ATTEMPT_ID_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; ATTEMPT_ID_LEN] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Debug for AttemptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AttemptId({})", self.to_hex())
    }
}

impl std::fmt::Display for AttemptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

fn decode_fixed<const N: usize>(buf: &[u8]) -> Result<[u8; N], ProtocolError> {
    buf.try_into().map_err(|_| ProtocolError::FrameLengthMismatch { got: buf.len(), expected: N })
}

pub fn decode_attempt_id(buf: &[u8]) -> Result<AttemptId, ProtocolError> {
    Ok(AttemptId(decode_fixed(buf)?))
}

/// The fencing token itself. Belongs to a *round* of candidates (assigned by
/// the client-side orchestrator, `#18-5`), never to an individual candidate —
/// see the module docs for the winner rule this makes possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionGeneration(u64);

impl ConnectionGeneration {
    pub const INITIAL: Self = Self(0);

    pub fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub fn get(self) -> u64 {
        self.0
    }

    /// Advances to the next generation, rejecting the `u64` overflow a
    /// pathologically long-lived session could otherwise trigger — mirrors
    /// `offset::checked_advance`'s overflow handling for the same reason
    /// (never silently wrap a value a security decision depends on).
    pub fn checked_next(self) -> Result<Self, ProtocolError> {
        self.0.checked_add(1).map(Self).ok_or(ProtocolError::GenerationOverflow)
    }

    pub fn to_be_bytes(self) -> [u8; GENERATION_LEN] {
        self.0.to_be_bytes()
    }

    pub fn from_be_bytes(bytes: [u8; GENERATION_LEN]) -> Self {
        Self(u64::from_be_bytes(bytes))
    }
}

impl std::fmt::Display for ConnectionGeneration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn decode_generation(buf: &[u8]) -> Result<ConnectionGeneration, ProtocolError> {
    Ok(ConnectionGeneration::from_be_bytes(decode_fixed(buf)?))
}

/// Server-generated nonce that binds [`AttachActivate`] to the specific
/// attempt the server accepted (`AttachReadyV2`'s payload). Not itself the
/// authentication mechanism for the initial attach — that is `AttachProof` —
/// but compared constant-time on activation since it is presented back to
/// the server as a capability-like value. `Debug` hides the bytes for the
/// same reason `hello::Proof` does.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AttachToken([u8; ATTACH_TOKEN_LEN]);

impl AttachToken {
    pub fn new(bytes: [u8; ATTACH_TOKEN_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; ATTACH_TOKEN_LEN] {
        &self.0
    }

    pub fn ct_eq(&self, other: &AttachToken) -> bool {
        let mut diff = 0u8;
        for (a, b) in self.0.iter().zip(other.0.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

impl std::fmt::Debug for AttachToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AttachToken(..)")
    }
}

fn decode_attach_token(buf: &[u8]) -> Result<AttachToken, ProtocolError> {
    Ok(AttachToken(decode_fixed(buf)?))
}

/// `HMAC-SHA256(session_secret, exporter || attach_hello_proof_transcript(..))`
/// or `.. || cancel_attach_proof_transcript(..)` depending on frame kind —
/// computed by `isekai-transport` (needs the live QUIC connection's
/// exporter), never by this crate. Opaque and `Debug`-hidden like
/// `hello::Proof`/`resume::ResumeProof`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AttachProof([u8; ATTACH_PROOF_LEN]);

impl AttachProof {
    pub fn new(bytes: [u8; ATTACH_PROOF_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; ATTACH_PROOF_LEN] {
        &self.0
    }

    pub fn ct_eq(&self, other: &AttachProof) -> bool {
        let mut diff = 0u8;
        for (a, b) in self.0.iter().zip(other.0.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

impl std::fmt::Debug for AttachProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AttachProof(..)")
    }
}

fn decode_attach_proof(buf: &[u8]) -> Result<AttachProof, ProtocolError> {
    Ok(AttachProof(decode_fixed(buf)?))
}

/// Identifies exactly one candidate attempt: which logical session, which
/// fencing generation, which attempt within that generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttachKey {
    pub session_id: SessionId,
    pub generation: ConnectionGeneration,
    pub attempt_id: AttemptId,
}

/// Builds the pure transcript bytes fed to HMAC (alongside the QUIC
/// exporter) for an [`AttachHello`]'s proof. Domain-separated from
/// [`cancel_attach_proof_transcript`] so a captured HELLO proof for a given
/// `(session_id, generation, attempt_id)` can never be replayed as that same
/// tuple's CANCEL proof.
pub fn attach_hello_proof_transcript(
    session_id: &SessionId,
    generation: ConnectionGeneration,
    attempt_id: &AttemptId,
    requested_resume_grace_secs: u32,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        ATTACH_HELLO_PROOF_DOMAIN.len() + 2 + 1 + SESSION_ID_LEN + GENERATION_LEN + ATTEMPT_ID_LEN + 4,
    );
    buf.extend_from_slice(ATTACH_HELLO_PROOF_DOMAIN);
    buf.extend_from_slice(&CURRENT_PROTOCOL_VERSION.to_be_bytes());
    buf.push(FRAME_ATTACH_HELLO);
    buf.extend_from_slice(session_id.as_bytes());
    buf.extend_from_slice(&generation.to_be_bytes());
    buf.extend_from_slice(attempt_id.as_bytes());
    buf.extend_from_slice(&requested_resume_grace_secs.to_be_bytes());
    buf
}

/// Same purpose as [`attach_hello_proof_transcript`] but for
/// [`CancelAttach`] — deliberately a different domain string and omits
/// `requested_resume_grace_secs` (CANCEL has no such option), so the two
/// transcripts can never collide even for identical identifiers.
pub fn cancel_attach_proof_transcript(
    session_id: &SessionId,
    generation: ConnectionGeneration,
    attempt_id: &AttemptId,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        CANCEL_ATTACH_PROOF_DOMAIN.len() + 2 + 1 + SESSION_ID_LEN + GENERATION_LEN + ATTEMPT_ID_LEN,
    );
    buf.extend_from_slice(CANCEL_ATTACH_PROOF_DOMAIN);
    buf.extend_from_slice(&CURRENT_PROTOCOL_VERSION.to_be_bytes());
    buf.push(FRAME_ATTACH_CANCEL);
    buf.extend_from_slice(session_id.as_bytes());
    buf.extend_from_slice(&generation.to_be_bytes());
    buf.extend_from_slice(attempt_id.as_bytes());
    buf
}

/// `ATTACH_HELLO` (client → server, replaces v1 `HELLO` for candidates that
/// participate in fencing). `#18-6` will wire this into
/// `isekai-transport::relay::connect_and_handshake`'s `commit_attach` half;
/// until then this is a pure value type with no caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachHello {
    pub session_id: SessionId,
    pub generation: ConnectionGeneration,
    pub attempt_id: AttemptId,
    pub requested_resume_grace_secs: u32,
    pub proof: AttachProof,
}

pub fn encode_attach_hello(frame: &AttachHello) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ATTACH_HELLO_FRAME_LEN);
    buf.push(FRAME_ATTACH_HELLO);
    buf.extend_from_slice(frame.session_id.as_bytes());
    buf.extend_from_slice(&frame.generation.to_be_bytes());
    buf.extend_from_slice(frame.attempt_id.as_bytes());
    buf.extend_from_slice(&frame.requested_resume_grace_secs.to_be_bytes());
    buf.extend_from_slice(frame.proof.as_bytes());
    debug_assert_eq!(buf.len(), ATTACH_HELLO_FRAME_LEN);
    buf
}

pub fn decode_attach_hello(buf: &[u8]) -> Result<AttachHello, ProtocolError> {
    if buf.len() != ATTACH_HELLO_FRAME_LEN {
        return Err(ProtocolError::FrameLengthMismatch { got: buf.len(), expected: ATTACH_HELLO_FRAME_LEN });
    }
    if buf[0] != FRAME_ATTACH_HELLO {
        return Err(ProtocolError::UnknownFrameType(buf[0]));
    }
    let mut pos = 1;
    let session_id = decode_session_id(&buf[pos..pos + SESSION_ID_LEN])?;
    pos += SESSION_ID_LEN;
    let generation = decode_generation(&buf[pos..pos + GENERATION_LEN])?;
    pos += GENERATION_LEN;
    let attempt_id = decode_attempt_id(&buf[pos..pos + ATTEMPT_ID_LEN])?;
    pos += ATTEMPT_ID_LEN;
    let requested_resume_grace_secs = u32::from_be_bytes(decode_fixed(&buf[pos..pos + 4])?);
    pos += 4;
    let proof = decode_attach_proof(&buf[pos..pos + ATTACH_PROOF_LEN])?;
    pos += ATTACH_PROOF_LEN;
    debug_assert_eq!(pos, ATTACH_HELLO_FRAME_LEN);
    Ok(AttachHello { session_id, generation, attempt_id, requested_resume_grace_secs, proof })
}

/// Why the server rejected an `ATTACH_HELLO` or `CANCEL_ATTACH`. `Auth`/
/// `Target`/`Unsupported` carry over unchanged from v1 HELLO/ACK; the rest
/// are new to fencing (module docs' winner-rule table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachRejectReason {
    Auth,
    Target,
    Unsupported,
    /// Same `(session_id, generation)`, but a different `attempt_id` already
    /// won this generation. Not a terminal failure for the *session* — just
    /// this one candidate lost a race (`#19`) or arrived after a
    /// pre-attach fallback already picked a winner.
    AlreadyAttached,
    /// `generation` is behind the server's current generation for this
    /// `session_id`. Carries the server's `current_generation` so the client
    /// knows exactly how far to jump instead of guessing.
    StaleGeneration { current_generation: ConnectionGeneration },
    /// A *different* `session_id` is currently active; this server instance
    /// only ever serves one logical session at a time.
    BusyOtherSession,
    /// This `session_id` has already completed its initial attach and moved
    /// to `Established` — a new round shouldn't be started for it, `RESUME`
    /// should be used instead.
    AttachAlreadyEstablished,
}

pub fn encode_attach_reject(reason: AttachRejectReason) -> Vec<u8> {
    match reason {
        AttachRejectReason::Auth => vec![crate::hello::FRAME_REJECT_AUTH],
        AttachRejectReason::Target => vec![crate::hello::FRAME_REJECT_TARGET],
        AttachRejectReason::Unsupported => vec![crate::hello::FRAME_REJECT_UNSUPPORTED],
        AttachRejectReason::AlreadyAttached => vec![FRAME_REJECT_ALREADY_ATTACHED],
        AttachRejectReason::StaleGeneration { current_generation } => {
            let mut buf = Vec::with_capacity(STALE_GENERATION_REJECT_FRAME_LEN);
            buf.push(FRAME_REJECT_STALE_GENERATION);
            buf.extend_from_slice(&current_generation.to_be_bytes());
            buf
        }
        AttachRejectReason::BusyOtherSession => vec![FRAME_REJECT_BUSY_OTHER_SESSION],
        AttachRejectReason::AttachAlreadyEstablished => vec![FRAME_REJECT_ATTACH_ALREADY_ESTABLISHED],
    }
}

pub fn decode_attach_reject(buf: &[u8]) -> Result<AttachRejectReason, ProtocolError> {
    let Some(&type_byte) = buf.first() else {
        return Err(ProtocolError::FrameLengthMismatch { got: 0, expected: 1 });
    };
    match type_byte {
        crate::hello::FRAME_REJECT_AUTH
        | crate::hello::FRAME_REJECT_TARGET
        | crate::hello::FRAME_REJECT_UNSUPPORTED
        | FRAME_REJECT_ALREADY_ATTACHED
        | FRAME_REJECT_BUSY_OTHER_SESSION
        | FRAME_REJECT_ATTACH_ALREADY_ESTABLISHED => {
            if buf.len() != 1 {
                return Err(ProtocolError::FrameLengthMismatch { got: buf.len(), expected: 1 });
            }
            Ok(match type_byte {
                crate::hello::FRAME_REJECT_AUTH => AttachRejectReason::Auth,
                crate::hello::FRAME_REJECT_TARGET => AttachRejectReason::Target,
                crate::hello::FRAME_REJECT_UNSUPPORTED => AttachRejectReason::Unsupported,
                FRAME_REJECT_ALREADY_ATTACHED => AttachRejectReason::AlreadyAttached,
                FRAME_REJECT_BUSY_OTHER_SESSION => AttachRejectReason::BusyOtherSession,
                _ => AttachRejectReason::AttachAlreadyEstablished,
            })
        }
        FRAME_REJECT_STALE_GENERATION => {
            if buf.len() != STALE_GENERATION_REJECT_FRAME_LEN {
                return Err(ProtocolError::FrameLengthMismatch {
                    got: buf.len(),
                    expected: STALE_GENERATION_REJECT_FRAME_LEN,
                });
            }
            let current_generation = decode_generation(&buf[1..])?;
            Ok(AttachRejectReason::StaleGeneration { current_generation })
        }
        other => Err(ProtocolError::UnknownFrameType(other)),
    }
}

/// The server's response to `ATTACH_HELLO`: either `Ready` (success, echoes
/// every identifier plus a fresh [`AttachToken`] and the negotiated
/// resume-grace) or a rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachResponse {
    Ready {
        session_id: SessionId,
        generation: ConnectionGeneration,
        attempt_id: AttemptId,
        negotiated_resume_grace_secs: u32,
        attach_token: AttachToken,
    },
    Reject(AttachRejectReason),
}

pub fn encode_attach_response(resp: &AttachResponse) -> Vec<u8> {
    match resp {
        AttachResponse::Ready { session_id, generation, attempt_id, negotiated_resume_grace_secs, attach_token } => {
            let mut buf = Vec::with_capacity(ATTACH_READY_FRAME_LEN);
            buf.push(FRAME_ATTACH_READY);
            buf.extend_from_slice(session_id.as_bytes());
            buf.extend_from_slice(&generation.to_be_bytes());
            buf.extend_from_slice(attempt_id.as_bytes());
            buf.extend_from_slice(&negotiated_resume_grace_secs.to_be_bytes());
            buf.extend_from_slice(attach_token.as_bytes());
            debug_assert_eq!(buf.len(), ATTACH_READY_FRAME_LEN);
            buf
        }
        AttachResponse::Reject(reason) => encode_attach_reject(*reason),
    }
}

/// Reads a full `AttachResponse`: `buf[0]` is always the type byte;
/// `FRAME_ATTACH_READY` additionally requires exactly `ATTACH_READY_FRAME_LEN`
/// total bytes, `FRAME_REJECT_STALE_GENERATION` requires
/// `STALE_GENERATION_REJECT_FRAME_LEN`, every other known reject byte
/// requires exactly `1`. Mirrors `hello::decode_ack_response`'s
/// "type byte first, then depending on type" split so a caller reading off
/// the wire can size its second read the same way (`#18-6`).
pub fn decode_attach_response(buf: &[u8]) -> Result<AttachResponse, ProtocolError> {
    let Some(&type_byte) = buf.first() else {
        return Err(ProtocolError::FrameLengthMismatch { got: 0, expected: 1 });
    };
    if type_byte == FRAME_ATTACH_READY {
        if buf.len() != ATTACH_READY_FRAME_LEN {
            return Err(ProtocolError::FrameLengthMismatch { got: buf.len(), expected: ATTACH_READY_FRAME_LEN });
        }
        let mut pos = 1;
        let session_id = decode_session_id(&buf[pos..pos + SESSION_ID_LEN])?;
        pos += SESSION_ID_LEN;
        let generation = decode_generation(&buf[pos..pos + GENERATION_LEN])?;
        pos += GENERATION_LEN;
        let attempt_id = decode_attempt_id(&buf[pos..pos + ATTEMPT_ID_LEN])?;
        pos += ATTEMPT_ID_LEN;
        let negotiated_resume_grace_secs = u32::from_be_bytes(decode_fixed(&buf[pos..pos + 4])?);
        pos += 4;
        let attach_token = decode_attach_token(&buf[pos..pos + ATTACH_TOKEN_LEN])?;
        pos += ATTACH_TOKEN_LEN;
        debug_assert_eq!(pos, ATTACH_READY_FRAME_LEN);
        return Ok(AttachResponse::Ready {
            session_id,
            generation,
            attempt_id,
            negotiated_resume_grace_secs,
            attach_token,
        });
    }
    Ok(AttachResponse::Reject(decode_attach_reject(buf)?))
}

/// `ATTACH_ACTIVATE` (client → server, control stream): confirms the client
/// actually received `AttachReadyV2` and moves the server's
/// `PendingActivation` state to `Established` (`#18-2`/`#18-3`). Deliberately
/// carries no separate proof — `attach_token` is unguessable server-random
/// data the client can only have obtained by already completing the
/// authenticated `ATTACH_HELLO`/`AttachReadyV2` exchange for this exact
/// `(session_id, generation, attempt_id)`, so re-proving here would be
/// redundant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachActivate {
    pub session_id: SessionId,
    pub generation: ConnectionGeneration,
    pub attempt_id: AttemptId,
    pub attach_token: AttachToken,
}

pub fn encode_attach_activate(frame: &AttachActivate) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ATTACH_ACTIVATE_FRAME_LEN);
    buf.push(FRAME_ATTACH_ACTIVATE);
    buf.extend_from_slice(frame.session_id.as_bytes());
    buf.extend_from_slice(&frame.generation.to_be_bytes());
    buf.extend_from_slice(frame.attempt_id.as_bytes());
    buf.extend_from_slice(frame.attach_token.as_bytes());
    debug_assert_eq!(buf.len(), ATTACH_ACTIVATE_FRAME_LEN);
    buf
}

pub fn decode_attach_activate(buf: &[u8]) -> Result<AttachActivate, ProtocolError> {
    if buf.len() != ATTACH_ACTIVATE_FRAME_LEN {
        return Err(ProtocolError::FrameLengthMismatch { got: buf.len(), expected: ATTACH_ACTIVATE_FRAME_LEN });
    }
    if buf[0] != FRAME_ATTACH_ACTIVATE {
        return Err(ProtocolError::UnknownFrameType(buf[0]));
    }
    let mut pos = 1;
    let session_id = decode_session_id(&buf[pos..pos + SESSION_ID_LEN])?;
    pos += SESSION_ID_LEN;
    let generation = decode_generation(&buf[pos..pos + GENERATION_LEN])?;
    pos += GENERATION_LEN;
    let attempt_id = decode_attempt_id(&buf[pos..pos + ATTEMPT_ID_LEN])?;
    pos += ATTEMPT_ID_LEN;
    let attach_token = decode_attach_token(&buf[pos..pos + ATTACH_TOKEN_LEN])?;
    pos += ATTACH_TOKEN_LEN;
    debug_assert_eq!(pos, ATTACH_ACTIVATE_FRAME_LEN);
    Ok(AttachActivate { session_id, generation, attempt_id, attach_token })
}

/// `CANCEL_ATTACH` (client → server, best-effort): asks the server to give up
/// a specific `(session_id, generation, attempt_id)` early. Unlike
/// `AttachActivate`, this carries its own proof — it may arrive on a fresh
/// connection rather than the one that performed the original attach (the
/// original may be exactly the dead connection being cancelled) — and the
/// server applies it only on an *exact* match, never as a session-wide or
/// generation-wide wildcard (module docs: safety must never depend on this
/// frame arriving at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancelAttach {
    pub session_id: SessionId,
    pub generation: ConnectionGeneration,
    pub attempt_id: AttemptId,
    pub proof: AttachProof,
}

pub fn encode_cancel_attach(frame: &CancelAttach) -> Vec<u8> {
    let mut buf = Vec::with_capacity(CANCEL_ATTACH_FRAME_LEN);
    buf.push(FRAME_ATTACH_CANCEL);
    buf.extend_from_slice(frame.session_id.as_bytes());
    buf.extend_from_slice(&frame.generation.to_be_bytes());
    buf.extend_from_slice(frame.attempt_id.as_bytes());
    buf.extend_from_slice(frame.proof.as_bytes());
    debug_assert_eq!(buf.len(), CANCEL_ATTACH_FRAME_LEN);
    buf
}

pub fn decode_cancel_attach(buf: &[u8]) -> Result<CancelAttach, ProtocolError> {
    if buf.len() != CANCEL_ATTACH_FRAME_LEN {
        return Err(ProtocolError::FrameLengthMismatch { got: buf.len(), expected: CANCEL_ATTACH_FRAME_LEN });
    }
    if buf[0] != FRAME_ATTACH_CANCEL {
        return Err(ProtocolError::UnknownFrameType(buf[0]));
    }
    let mut pos = 1;
    let session_id = decode_session_id(&buf[pos..pos + SESSION_ID_LEN])?;
    pos += SESSION_ID_LEN;
    let generation = decode_generation(&buf[pos..pos + GENERATION_LEN])?;
    pos += GENERATION_LEN;
    let attempt_id = decode_attempt_id(&buf[pos..pos + ATTEMPT_ID_LEN])?;
    pos += ATTEMPT_ID_LEN;
    let proof = decode_attach_proof(&buf[pos..pos + ATTACH_PROOF_LEN])?;
    pos += ATTACH_PROOF_LEN;
    debug_assert_eq!(pos, CANCEL_ATTACH_FRAME_LEN);
    Ok(CancelAttach { session_id, generation, attempt_id, proof })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(b: u8) -> SessionId {
        SessionId::from_bytes([b; SESSION_ID_LEN])
    }

    fn aid(b: u8) -> AttemptId {
        AttemptId::from_bytes([b; ATTEMPT_ID_LEN])
    }

    #[test]
    fn attach_hello_roundtrips() {
        let frame = AttachHello {
            session_id: sid(1),
            generation: ConnectionGeneration::new(5),
            attempt_id: aid(2),
            requested_resume_grace_secs: 90,
            proof: AttachProof::new([7u8; ATTACH_PROOF_LEN]),
        };
        let encoded = encode_attach_hello(&frame);
        assert_eq!(encoded.len(), ATTACH_HELLO_FRAME_LEN);
        assert_eq!(decode_attach_hello(&encoded).unwrap(), frame);
    }

    #[test]
    fn decode_attach_hello_rejects_wrong_length() {
        let err = decode_attach_hello(&[FRAME_ATTACH_HELLO; 10]).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: 10, expected: ATTACH_HELLO_FRAME_LEN });
    }

    #[test]
    fn decode_attach_hello_rejects_wrong_type_byte() {
        let mut buf = vec![0x99u8];
        buf.extend_from_slice(&[0u8; ATTACH_HELLO_FRAME_LEN - 1]);
        let err = decode_attach_hello(&buf).unwrap_err();
        assert_eq!(err, ProtocolError::UnknownFrameType(0x99));
    }

    #[test]
    fn attach_ready_roundtrips() {
        let resp = AttachResponse::Ready {
            session_id: sid(3),
            generation: ConnectionGeneration::new(9),
            attempt_id: aid(4),
            negotiated_resume_grace_secs: 120,
            attach_token: AttachToken::new([8u8; ATTACH_TOKEN_LEN]),
        };
        let encoded = encode_attach_response(&resp);
        assert_eq!(encoded.len(), ATTACH_READY_FRAME_LEN);
        assert_eq!(decode_attach_response(&encoded).unwrap(), resp);
    }

    #[test]
    fn attach_response_reject_roundtrips_for_all_known_values() {
        for reason in [
            AttachRejectReason::Auth,
            AttachRejectReason::Target,
            AttachRejectReason::Unsupported,
            AttachRejectReason::AlreadyAttached,
            AttachRejectReason::BusyOtherSession,
            AttachRejectReason::AttachAlreadyEstablished,
            AttachRejectReason::StaleGeneration { current_generation: ConnectionGeneration::new(42) },
        ] {
            let resp = AttachResponse::Reject(reason);
            let encoded = encode_attach_response(&resp);
            assert_eq!(decode_attach_response(&encoded).unwrap(), resp);
        }
    }

    #[test]
    fn decode_attach_response_rejects_empty_buffer() {
        let err = decode_attach_response(&[]).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: 0, expected: 1 });
    }

    #[test]
    fn decode_attach_response_rejects_wrong_ready_length() {
        let err = decode_attach_response(&[FRAME_ATTACH_READY, 0, 0]).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: 3, expected: ATTACH_READY_FRAME_LEN });
    }

    #[test]
    fn decode_attach_response_rejects_trailing_bytes_on_simple_reject() {
        let err = decode_attach_response(&[FRAME_REJECT_ALREADY_ATTACHED, 0]).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: 2, expected: 1 });
    }

    #[test]
    fn decode_attach_response_rejects_wrong_stale_generation_length() {
        let err = decode_attach_response(&[FRAME_REJECT_STALE_GENERATION, 0, 0]).unwrap_err();
        assert_eq!(
            err,
            ProtocolError::FrameLengthMismatch { got: 3, expected: STALE_GENERATION_REJECT_FRAME_LEN }
        );
    }

    #[test]
    fn decode_attach_response_rejects_unknown_type() {
        let err = decode_attach_response(&[0x42]).unwrap_err();
        assert_eq!(err, ProtocolError::UnknownFrameType(0x42));
    }

    #[test]
    fn attach_activate_roundtrips() {
        let frame = AttachActivate {
            session_id: sid(5),
            generation: ConnectionGeneration::new(1),
            attempt_id: aid(6),
            attach_token: AttachToken::new([1u8; ATTACH_TOKEN_LEN]),
        };
        let encoded = encode_attach_activate(&frame);
        assert_eq!(encoded.len(), ATTACH_ACTIVATE_FRAME_LEN);
        assert_eq!(decode_attach_activate(&encoded).unwrap(), frame);
    }

    #[test]
    fn decode_attach_activate_rejects_wrong_type_byte() {
        let mut buf = vec![0x99u8];
        buf.extend_from_slice(&[0u8; ATTACH_ACTIVATE_FRAME_LEN - 1]);
        let err = decode_attach_activate(&buf).unwrap_err();
        assert_eq!(err, ProtocolError::UnknownFrameType(0x99));
    }

    #[test]
    fn cancel_attach_roundtrips() {
        let frame = CancelAttach {
            session_id: sid(7),
            generation: ConnectionGeneration::new(2),
            attempt_id: aid(8),
            proof: AttachProof::new([3u8; ATTACH_PROOF_LEN]),
        };
        let encoded = encode_cancel_attach(&frame);
        assert_eq!(encoded.len(), CANCEL_ATTACH_FRAME_LEN);
        assert_eq!(decode_cancel_attach(&encoded).unwrap(), frame);
    }

    #[test]
    fn decode_cancel_attach_rejects_wrong_length() {
        let err = decode_cancel_attach(&[FRAME_ATTACH_CANCEL; 5]).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: 5, expected: CANCEL_ATTACH_FRAME_LEN });
    }

    #[test]
    fn connection_generation_checked_next_advances() {
        let g = ConnectionGeneration::new(5);
        assert_eq!(g.checked_next().unwrap(), ConnectionGeneration::new(6));
    }

    #[test]
    fn connection_generation_checked_next_rejects_overflow() {
        let g = ConnectionGeneration::new(u64::MAX);
        assert_eq!(g.checked_next(), Err(ProtocolError::GenerationOverflow));
    }

    #[test]
    fn connection_generation_ordering_reflects_fencing() {
        assert!(ConnectionGeneration::new(5) < ConnectionGeneration::new(6));
    }

    #[test]
    fn connection_generation_round_trips_be_bytes() {
        let g = ConnectionGeneration::new(0x0102030405060708);
        assert_eq!(ConnectionGeneration::from_be_bytes(g.to_be_bytes()), g);
    }

    #[test]
    fn attach_token_ct_eq_detects_mismatch() {
        let a = AttachToken::new([1u8; ATTACH_TOKEN_LEN]);
        let mut bytes = [1u8; ATTACH_TOKEN_LEN];
        bytes[15] = 2;
        let b = AttachToken::new(bytes);
        assert!(!a.ct_eq(&b));
    }

    #[test]
    fn attach_token_debug_does_not_leak_bytes() {
        let token = AttachToken::new([0xABu8; ATTACH_TOKEN_LEN]);
        assert_eq!(format!("{token:?}"), "AttachToken(..)");
    }

    #[test]
    fn attach_proof_ct_eq_detects_mismatch() {
        let a = AttachProof::new([1u8; ATTACH_PROOF_LEN]);
        let mut bytes = [1u8; ATTACH_PROOF_LEN];
        bytes[31] = 2;
        let b = AttachProof::new(bytes);
        assert!(!a.ct_eq(&b));
    }

    #[test]
    fn attach_proof_debug_does_not_leak_bytes() {
        let proof = AttachProof::new([0xABu8; ATTACH_PROOF_LEN]);
        assert_eq!(format!("{proof:?}"), "AttachProof(..)");
    }

    #[test]
    fn attempt_id_debug_and_display_show_hex() {
        let id = aid(0x0a);
        assert_eq!(id.to_hex(), "0a".repeat(ATTEMPT_ID_LEN));
        assert_eq!(format!("{id}"), id.to_hex());
    }

    // --- Proof transcript uniqueness (module docs' "1bit変更で検証失敗" /
    // "frame type差し替え不可" requirement): since the real proof is
    // `HMAC(session_secret, exporter || transcript)` and HMAC is a PRF, any
    // transcript byte difference below necessarily changes the resulting
    // proof — this crate cannot compute the HMAC itself (no live QUIC
    // exporter), so it proves the transcript-uniqueness precondition instead.
    // The end-to-end HMAC property is exercised in `isekai-transport` (#18-6).

    #[test]
    fn hello_transcript_changes_with_generation() {
        let a = attach_hello_proof_transcript(&sid(1), ConnectionGeneration::new(1), &aid(1), 0);
        let b = attach_hello_proof_transcript(&sid(1), ConnectionGeneration::new(2), &aid(1), 0);
        assert_ne!(a, b);
    }

    #[test]
    fn hello_transcript_changes_with_attempt_id() {
        let a = attach_hello_proof_transcript(&sid(1), ConnectionGeneration::new(1), &aid(1), 0);
        let b = attach_hello_proof_transcript(&sid(1), ConnectionGeneration::new(1), &aid(2), 0);
        assert_ne!(a, b);
    }

    #[test]
    fn hello_transcript_changes_with_session_id() {
        let a = attach_hello_proof_transcript(&sid(1), ConnectionGeneration::new(1), &aid(1), 0);
        let b = attach_hello_proof_transcript(&sid(2), ConnectionGeneration::new(1), &aid(1), 0);
        assert_ne!(a, b);
    }

    #[test]
    fn hello_transcript_changes_with_options() {
        let a = attach_hello_proof_transcript(&sid(1), ConnectionGeneration::new(1), &aid(1), 0);
        let b = attach_hello_proof_transcript(&sid(1), ConnectionGeneration::new(1), &aid(1), 90);
        assert_ne!(a, b);
    }

    /// The core anti-replay property: a HELLO transcript and a CANCEL
    /// transcript for the *exact same* `(session_id, generation, attempt_id)`
    /// must never collide, or a captured CANCEL proof could be replayed as a
    /// HELLO proof (or vice versa).
    #[test]
    fn hello_and_cancel_transcripts_never_collide_for_same_identifiers() {
        let hello = attach_hello_proof_transcript(&sid(1), ConnectionGeneration::new(1), &aid(1), 0);
        let cancel = cancel_attach_proof_transcript(&sid(1), ConnectionGeneration::new(1), &aid(1));
        assert_ne!(hello, cancel);
    }

    #[test]
    fn cancel_transcript_changes_with_generation() {
        let a = cancel_attach_proof_transcript(&sid(1), ConnectionGeneration::new(1), &aid(1));
        let b = cancel_attach_proof_transcript(&sid(1), ConnectionGeneration::new(2), &aid(1));
        assert_ne!(a, b);
    }
}
