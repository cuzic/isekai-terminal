//! Generic length-prefixed message framing over any bidirectional stream, and
//! the owner-side [`serve`] loop that dispatches accepted connections to a
//! per-connection handler.
//!
//! The wire format is a `u32` big-endian length prefix followed by exactly
//! that many payload bytes — matching this workspace's own framing convention
//! (`isekai-protocol`'s length-prefixed frames are big-endian throughout; see
//! `isekai-protocol::version`/`attach`) and its practice of capping a declared
//! length *before* allocating for it (see `isekai-protocol::ctl`'s
//! `MAX_CTL_MESSAGE_LINE_LEN` check). This module is deliberately
//! byte-agnostic: it has no opinion on what a frame's payload means — that's
//! the caller's protocol (e.g. `isekai-ssh`'s SSH-specific multiplexer).

use std::future::Future;
use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::ExclusiveChannel;

/// Writes `payload` as one length-prefixed frame: a `u32` big-endian length
/// followed by the payload bytes, then flushes. Fails with
/// [`io::ErrorKind::InvalidInput`] if `payload` is larger than `u32::MAX`
/// (unrepresentable in the length prefix) — callers should keep frames well
/// under any such bound anyway.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame payload of {} bytes exceeds the u32 length-prefix limit", payload.len()),
        )
    })?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(payload).await?;
    w.flush().await?;
    Ok(())
}

/// Reads one length-prefixed frame written by [`write_frame`]. Rejects a
/// declared length exceeding `max_len` with [`io::ErrorKind::InvalidData`]
/// *before* allocating or reading the payload, so a malformed or hostile peer
/// can't force an unbounded allocation (the actual security property). A
/// clean EOF at the frame boundary surfaces as
/// [`io::ErrorKind::UnexpectedEof`] via `read_exact`.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, max_len: usize) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > max_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("declared frame length {len} exceeds the {max_len}-byte cap"),
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok(payload)
}

/// Owner-side relay loop: repeatedly [`accept`](ExclusiveChannel::accept)s the
/// next client connection and spawns a task running `handler` on it, so many
/// clients are served concurrently over the owner's lifetime. Returns only
/// when `accept` itself fails (a genuine failure of the underlying channel);
/// per-connection errors are the handler's concern, not this loop's.
///
/// `handler` is cloned per connection and must produce a `Send` future, so it
/// composes directly with `tokio::spawn`. It is intentionally opaque to this
/// crate: it owns whatever framing/protocol runs over the connection.
pub async fn serve<C, H, Fut>(mut owner: C, handler: H) -> io::Result<()>
where
    C: ExclusiveChannel,
    H: Fn(C::Connection) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    loop {
        let conn = owner.accept().await?;
        let handler = handler.clone();
        tokio::spawn(async move {
            handler(conn).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryChannel;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn frame_round_trips_over_a_duplex_pair() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let payload = b"the quick brown fox".to_vec();

        write_frame(&mut a, &payload).await.unwrap();
        let read = read_frame(&mut b, 1024).await.unwrap();
        assert_eq!(read, payload);
    }

    #[tokio::test]
    async fn an_empty_frame_round_trips() {
        let (mut a, mut b) = tokio::io::duplex(64);
        write_frame(&mut a, b"").await.unwrap();
        let read = read_frame(&mut b, 16).await.unwrap();
        assert!(read.is_empty());
    }

    #[tokio::test]
    async fn multiple_frames_read_back_in_order() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        write_frame(&mut a, b"first").await.unwrap();
        write_frame(&mut a, b"second").await.unwrap();

        assert_eq!(read_frame(&mut b, 64).await.unwrap(), b"first");
        assert_eq!(read_frame(&mut b, 64).await.unwrap(), b"second");
    }

    #[tokio::test]
    async fn a_length_prefix_over_the_cap_is_rejected_without_reading_the_payload() {
        // Write only a 4-byte length prefix declaring a huge payload, and no
        // payload bytes at all. read_frame must reject at the cap check
        // before it ever tries to read (and thus block on) the payload.
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let declared: u32 = 10 * 1024 * 1024;
        writer.write_all(&declared.to_be_bytes()).await.unwrap();
        writer.flush().await.unwrap();

        let err = read_frame(&mut reader, 1024).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn a_frame_exactly_at_the_cap_is_accepted() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let payload = vec![7u8; 256];
        write_frame(&mut a, &payload).await.unwrap();
        let read = read_frame(&mut b, 256).await.unwrap();
        assert_eq!(read, payload);
    }

    #[tokio::test]
    async fn serve_dispatches_each_accepted_connection_to_the_handler() {
        let name = "test-framing-serve-dispatch";
        let owner = InMemoryChannel::try_claim(name).await.unwrap();
        let seen = Arc::new(AtomicUsize::new(0));

        let seen_in_handler = Arc::clone(&seen);
        let handler = move |mut conn: tokio::io::DuplexStream| {
            let seen = Arc::clone(&seen_in_handler);
            async move {
                // Count this connection *before* echoing, so that once the
                // client observes the echo below the increment has already
                // happened (avoids racing the final assert).
                let msg = read_frame(&mut conn, 1024).await.unwrap();
                seen.fetch_add(1, Ordering::SeqCst);
                write_frame(&mut conn, &msg).await.unwrap();
            }
        };
        let server = tokio::spawn(serve(owner, handler));

        for i in 0..3u8 {
            let mut client = InMemoryChannel::connect(name).await.unwrap();
            write_frame(&mut client, &[i]).await.unwrap();
            let echoed = read_frame(&mut client, 1024).await.unwrap();
            assert_eq!(echoed, vec![i]);
        }

        // All three handlers ran to completion (each incremented after its
        // echo, which we've already observed above).
        assert_eq!(seen.load(Ordering::SeqCst), 3);
        server.abort();
    }
}
