//! HELLO/RESUME proof computation, extracted from
//! `isekai_pipe_quic_transport.rs::compute_proof` and generalized to work
//! behind `quicmux::AnyMuxConnection` instead of a concrete
//! `noq::Connection` (`archive/ISEKAI_SSH_DESIGN.md` "実装方針").

use hmac::{Hmac, Mac};
use isekai_protocol::hello::{Proof, EXPORTER_LABEL, PROOF_LEN};
use sha2::Sha256;

use crate::error::TransportError;

type HmacSha256 = Hmac<Sha256>;

/// `proof = HMAC-SHA256(session_secret, exporter [|| extra])`
/// (`archive/HELPER_PROTOCOL.md` §4, `isekai_pipe_quic_transport.rs::compute_proof`).
///
/// `extra` is empty for the initial HELLO. A non-empty `extra` (the
/// `session_id` bytes) is how a future `RESUME` frame's proof would be
/// computed — not implemented by this crate yet (`archive/ISEKAI_SSH_DESIGN.md`
/// phase S-4a), but this function already supports it so that phase doesn't
/// need to duplicate the HMAC logic.
pub async fn compute_proof(
    conn: &quicmux::AnyMuxConnection,
    session_secret: &[u8],
    extra: &[u8],
) -> Result<Proof, TransportError> {
    let exporter = conn.export_keying_material(EXPORTER_LABEL, b"").await?;
    let mut mac = HmacSha256::new_from_slice(session_secret).expect("HMAC accepts any key length");
    mac.update(&exporter);
    if !extra.is_empty() {
        mac.update(extra);
    }
    let bytes: [u8; PROOF_LEN] = mac.finalize().into_bytes().into();
    Ok(Proof::new(bytes))
}
