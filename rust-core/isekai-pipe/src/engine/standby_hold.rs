//! Server side of the warm-standby hold (`isekai_protocol::standby`).
//!
//! `handle_connection` closes a connection whose first stream doesn't deliver
//! a frame type within `HELLO_TIMEOUT`. A client-side `WarmStandby`
//! (`isekai-transport/src/warm_standby.rs`) instead sends
//! `FRAME_STANDBY_HOLD` + an HMAC proof as that first frame; this module
//! verifies it, then keeps the connection open — answering liveness pings on
//! the hold stream — until the client opens a second stream carrying the
//! actual `quicmux::FRAME_RESUME` (promotion), which is handed back to
//! `handle_connection` to be processed exactly like a RESUME arriving on a
//! freshly dialed connection.

use anyhow::{anyhow, Context, Result};
use isekai_protocol::standby::{
    decode_standby_hold, FRAME_STANDBY_HOLD, FRAME_STANDBY_PING, FRAME_STANDBY_PONG, FRAME_STANDBY_READY,
    STANDBY_HOLD_FRAME_LEN, STANDBY_HOLD_PROOF_DOMAIN,
};
use quicmux::{AnyByteStreamReadHalf, AnyByteStreamWriteHalf, AnyMuxConnection};
use subtle::ConstantTimeEq;

use super::{proof_from_exporter, read_exact, reject, EXPORTER_LABEL, FRAME_REJECT_UNSUPPORTED, HELLO_TIMEOUT};

/// `hello::FRAME_REJECT_AUTH` — the same byte every other proof mismatch on
/// this server answers with.
const FRAME_REJECT_AUTH: u8 = isekai_protocol::hello::FRAME_REJECT_AUTH;

/// Verifies the hold frame's proof, acknowledges it, and then serves the
/// hold until the client opens its promotion stream. Returns that stream's
/// halves with the `FRAME_RESUME` type byte already consumed — i.e. exactly
/// the state `handle_resume_stream` expects.
///
/// `rest` is the hold frame minus its type byte (`handle_connection` has
/// already read `STANDBY_HOLD_FRAME_LEN - 1` bytes for it).
pub(super) async fn hold_until_resume_stream(
    conn: &AnyMuxConnection,
    mut send: AnyByteStreamWriteHalf,
    mut recv: AnyByteStreamReadHalf,
    rest: &[u8],
    session_secret: &[u8; 32],
) -> Result<(AnyByteStreamWriteHalf, AnyByteStreamReadHalf)> {
    let mut frame = [0u8; STANDBY_HOLD_FRAME_LEN];
    frame[0] = FRAME_STANDBY_HOLD;
    if rest.len() != STANDBY_HOLD_FRAME_LEN - 1 {
        return Err(anyhow!("standby hold frame has unexpected length {}", rest.len() + 1));
    }
    frame[1..].copy_from_slice(rest);
    let proof = decode_standby_hold(&frame).context("failed to decode STANDBY_HOLD")?;
    let exporter = conn
        .export_keying_material(EXPORTER_LABEL, b"")
        .await
        .map_err(|e| anyhow!("export_keying_material failed: {e:?}"))?;
    let expected = proof_from_exporter(session_secret, &exporter, STANDBY_HOLD_PROOF_DOMAIN);
    if proof[..].ct_eq(&expected[..]).unwrap_u8() != 1 {
        reject(&mut send, &[FRAME_REJECT_AUTH]).await;
        return Err(anyhow!("standby hold proof mismatch, rejecting"));
    }
    send.write_all(&[FRAME_STANDBY_READY]).await.context("failed to acknowledge STANDBY_HOLD")?;

    // Once the client finishes (or resets) the hold stream it no longer
    // pings; from then on only a promotion stream can keep this connection
    // alive, bounded by the ordinary HELLO_TIMEOUT.
    let mut hold_stream_open = true;
    loop {
        let mut byte = [0u8; 1];
        let next = tokio::select! {
            // Prefer a promotion stream over a ping that happens to be ready
            // at the same instant — promotion is what the hold exists for.
            biased;
            stream = conn.accept_bi() => Some(stream.context("standby connection closed while holding")?),
            read = async { recv.read(&mut byte).await.map(|n| (n, byte[0])) }, if hold_stream_open => {
                match read {
                    Ok((0, _)) | Err(_) => hold_stream_open = false,
                    Ok((_, FRAME_STANDBY_PING)) => {
                        send.write_all(&[FRAME_STANDBY_PONG]).await.context("failed to answer standby ping")?;
                    }
                    Ok((_, other)) => return Err(anyhow!("unexpected byte {other:#x} on standby hold stream")),
                }
                None
            }
            _ = tokio::time::sleep(HELLO_TIMEOUT), if !hold_stream_open => {
                return Err(anyhow!("standby hold stream ended without a promotion stream"));
            }
        };
        let Some(stream) = next else { continue };

        let (mut resume_recv, mut resume_send) = stream.split();
        let mut type_byte = [0u8; 1];
        tokio::time::timeout(HELLO_TIMEOUT, read_exact(&mut resume_recv, &mut type_byte))
            .await
            .context("standby promotion frame timeout")?
            .context("failed to read standby promotion frame type")?;
        if type_byte[0] != quicmux::FRAME_RESUME {
            reject(&mut resume_send, &[FRAME_REJECT_UNSUPPORTED]).await;
            return Err(anyhow!("unexpected frame type {:#x} on standby promotion stream", type_byte[0]));
        }
        return Ok((resume_send, resume_recv));
    }
}
