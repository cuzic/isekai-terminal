//! `--mode stun`'s simple, non-resumable one-shot relay: two independent
//! `tokio::spawn`ed copy tasks (`ISEKAI_SSH_DESIGN.md`: not
//! `tokio::io::copy_bidirectional`, since stdin/stdout are two separate
//! handles, not one duplex object). See `super::relay_pump` for `--mode
//! relay`'s resumable counterpart and why the two do not share an
//! implementation: `relay_stdio` here needs no shared, lockable replay
//! buffer/offset counters and no ability to swap the underlying stream out
//! from under a running pump after a reconnect, so a plain pair of spawned
//! tasks — simpler than `relay_pump`'s single-task `tokio::select!` — is
//! genuinely enough for it. `isekai_transport::resume` is only wired up for
//! `RelayTarget` today, so this module has no resume support to speak of.

use anyhow::{Context, Result};
use isekai_transport::ByteStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Runs the two independent copy tasks. A clean EOF on one side does **not**
/// abort the other: e.g. `ssh` closing its write end of our stdin (C2H EOF)
/// is routine well before the server has said everything it's going to say,
/// and prematurely cutting H2C off there would truncate output the user was
/// still supposed to see. Both directions are allowed to run to their own
/// completion; only a genuine error (or task panic) on either side aborts
/// the other and returns early, since at that point continuing the survivor
/// alone serves no purpose. Once both sides have finished, this process
/// exits and closes its stdout, which is how `ssh` (reading our stdout)
/// learns the pass-through has ended.
pub async fn relay_stdio(stream: Box<dyn ByteStream>) -> Result<()> {
    let (quic_read, quic_write) = stream.split();

    let mut c2h = tokio::spawn(pump_stdin_to_quic(quic_write));
    let mut h2c = tokio::spawn(pump_quic_to_stdout(quic_read));
    let (mut c2h_done, mut h2c_done) = (false, false);

    while !c2h_done || !h2c_done {
        tokio::select! {
            res = &mut c2h, if !c2h_done => {
                c2h_done = true;
                if let Err(err) = join_result("isekai-ssh: stdin->QUIC relay task panicked", res) {
                    h2c.abort();
                    return Err(err);
                }
            }
            res = &mut h2c, if !h2c_done => {
                h2c_done = true;
                if let Err(err) = join_result("isekai-ssh: QUIC->stdout relay task panicked", res) {
                    c2h.abort();
                    return Err(err);
                }
            }
        }
    }
    Ok(())
}

/// Flattens a `tokio::spawn` result (`Result<Result<()>, JoinError>`) into a
/// single `Result<()>`, attaching `panic_ctx` if the task itself panicked
/// rather than returning an error.
fn join_result(panic_ctx: &str, res: std::result::Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    res.map_err(|e| anyhow::Error::new(e).context(panic_ctx.to_string()))?
}

/// C2H direction: `ssh` (our stdin) -> isekai-helper (the QUIC stream's send
/// side). On stdin EOF, finishes (shuts down) the QUIC send side so
/// isekai-helper sees a clean half-close rather than a reset.
async fn pump_stdin_to_quic(mut quic_write: Box<dyn isekai_transport::ByteStreamWriteHalf>) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = stdin.read(&mut buf).await.context("isekai-ssh: reading from stdin failed")?;
        if n == 0 {
            break;
        }
        quic_write.write_all(&buf[..n]).await.context("isekai-ssh: writing to isekai-helper failed")?;
    }
    // Best-effort: isekai-helper is free to have already gone away.
    let _ = quic_write.shutdown().await;
    Ok(())
}

/// H2C direction: isekai-helper (the QUIC stream's receive side) -> `ssh`
/// (our stdout). Every successful chunk is flushed immediately — `ssh`
/// expects to see SSH protocol bytes promptly, not batched.
async fn pump_quic_to_stdout(mut quic_read: Box<dyn isekai_transport::ByteStreamReadHalf>) -> Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = quic_read.read(&mut buf).await.context("isekai-ssh: reading from isekai-helper failed")?;
        if n == 0 {
            break;
        }
        stdout.write_all(&buf[..n]).await.context("isekai-ssh: writing to stdout failed")?;
        stdout.flush().await.context("isekai-ssh: flushing stdout failed")?;
    }
    Ok(())
}
