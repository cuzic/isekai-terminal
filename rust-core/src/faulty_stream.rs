//! テスト専用: 任意の `AsyncRead + AsyncWrite` を包み、ネットワーク完全切断を
//! シミュレートする。サンドボックスに `tc`/`netem` が無い(CAP_NET_ADMIN が
//! 実効的に付与されていない)ため、OS レベルではなくソケットラッパーとして
//! アプリケーション層でこの障害を再現する。
//!
//! TCP 経路(`run_russh_transport`)のテストが `cut()` による強制切断だけを
//! 必要としているため、遅延・パケットロスのモデリングはここでは持たない
//! (同種のポリシーが必要な場合は `faulty_udp_socket.rs` の
//! `UdpFaultInjector` を参照)。

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// `FaultyStream` を操作するハンドル。クローンして保持すれば、ストリームが
/// 使用中でも `cut()` による強制切断をテスト側から動的に行える。
#[derive(Clone)]
pub(crate) struct FaultInjector {
    cut: Arc<AtomicBool>,
}

impl FaultInjector {
    pub(crate) fn new() -> Self {
        Self { cut: Arc::new(AtomicBool::new(false)) }
    }

    /// 即座にネットワーク切断状態にする。以降の read は EOF、write は
    /// `ConnectionReset` を返すようになる。
    pub(crate) fn cut(&self) {
        self.cut.store(true, Ordering::Relaxed);
    }

    fn is_cut(&self) -> bool {
        self.cut.load(Ordering::Relaxed)
    }
}

/// `S` を包み、`FaultInjector::cut()` された後は read/write を切断状態として
/// 振る舞わせる。`client::connect_stream` や `tokio::io::join` が受け取る
/// 箇所にそのまま差し込める。
pub(crate) struct FaultyStream<S> {
    inner: S,
    injector: FaultInjector,
}

impl<S> FaultyStream<S> {
    pub(crate) fn new(inner: S, injector: FaultInjector) -> Self {
        Self { inner, injector }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for FaultyStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.injector.is_cut() {
            return Poll::Ready(Ok(())); // EOF
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for FaultyStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.injector.is_cut() {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::ConnectionReset)));
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.injector.is_cut() {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::ConnectionReset)));
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn passes_data_through_unmodified_by_default() {
        let (mut client, server) = tokio::io::duplex(64);
        let mut faulty = FaultyStream::new(server, FaultInjector::new());

        client.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        faulty.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn cut_causes_read_eof_and_write_error() {
        let (_client, server) = tokio::io::duplex(64);
        let injector = FaultInjector::new();
        let mut faulty = FaultyStream::new(server, injector.clone());

        injector.cut();

        let mut buf = [0u8; 1];
        let n = faulty.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "cut 後の read は EOF を返す");

        let err = faulty.write_all(b"x").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
    }

    #[tokio::test]
    async fn cut_mid_session_terminates_in_flight_transfer() {
        let (mut client, server) = tokio::io::duplex(64);
        let injector = FaultInjector::new();
        let mut faulty = FaultyStream::new(server, injector.clone());

        client.write_all(b"ok").await.unwrap();
        let mut buf = [0u8; 2];
        faulty.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ok");

        injector.cut();
        client.write_all(b"lost").await.unwrap();
        let n = faulty.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "切断後は inner にデータが残っていても EOF を返す");
    }
}
