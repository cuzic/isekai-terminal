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
    blackhole: Arc<AtomicBool>,
}

impl FaultInjector {
    pub(crate) fn new() -> Self {
        Self { cut: Arc::new(AtomicBool::new(false)), blackhole: Arc::new(AtomicBool::new(false)) }
    }

    /// 即座にネットワーク切断状態にする。以降の read は EOF、write は
    /// `ConnectionReset` を返すようになる。
    pub(crate) fn cut(&self) {
        self.cut.store(true, Ordering::Relaxed);
    }

    /// サイレント遮断にする(UDPの`FaultySender::poll_send`が「送ったふりをして破棄」する
    /// のと同じ故障の出方)。以降の read は永遠に`Pending`(EOFもエラーも返さない)、
    /// write は成功したふりをして破棄する。`cut()`(EOF/`ConnectionReset`=TCP RST相当)と
    /// 違い、上位層は相手からの応答が無いこと(keepalive等のタイムアウト)でしか死亡に
    /// 気付けない。復旧(restore)は提供しない(必要になったら足す)。`cut()`が優先される。
    pub(crate) fn blackhole(&self) {
        self.blackhole.store(true, Ordering::Relaxed);
    }

    fn is_cut(&self) -> bool {
        self.cut.load(Ordering::Relaxed)
    }

    fn is_blackholed(&self) -> bool {
        self.blackhole.load(Ordering::Relaxed)
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
        if self.injector.is_blackholed() {
            // wakerを登録しないのは意図的: blackholeは復旧しない(永久に無応答)ので起こす必要が無い。
            return Poll::Pending;
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
        if self.injector.is_blackholed() {
            return Poll::Ready(Ok(buf.len())); // 送ったふりをして破棄
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.injector.is_cut() {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::ConnectionReset)));
        }
        if self.injector.is_blackholed() {
            return Poll::Ready(Ok(()));
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

    #[tokio::test(start_paused = true)]
    async fn blackhole_swallows_writes_and_reads_never_complete() {
        let (mut client, server) = tokio::io::duplex(64);
        let injector = FaultInjector::new();
        let mut faulty = FaultyStream::new(server, injector.clone());

        injector.blackhole();

        // write は成功するが、相手には届かない。
        faulty.write_all(b"lost").await.expect("blackholed write pretends to succeed");
        faulty.flush().await.expect("blackholed flush pretends to succeed");
        let mut peer_buf = [0u8; 4];
        let peer_read = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client.read(&mut peer_buf),
        ).await;
        assert!(peer_read.is_err(), "blackholed write must not reach the peer");

        // 相手がデータを送っても、read は EOF もエラーも返さず永遠に完了しない。
        client.write_all(b"ignored").await.unwrap();
        let mut buf = [0u8; 8];
        let read = tokio::time::timeout(std::time::Duration::from_secs(3600), faulty.read(&mut buf)).await;
        assert!(read.is_err(), "blackholed read must stay pending (no EOF, no error)");
    }

    #[tokio::test]
    async fn cut_takes_precedence_over_blackhole() {
        let (_client, server) = tokio::io::duplex(64);
        let injector = FaultInjector::new();
        let mut faulty = FaultyStream::new(server, injector.clone());

        injector.blackhole();
        injector.cut();

        let mut buf = [0u8; 1];
        assert_eq!(faulty.read(&mut buf).await.unwrap(), 0, "cut wins: EOF");
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
