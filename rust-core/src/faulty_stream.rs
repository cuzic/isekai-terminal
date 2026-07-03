//! テスト専用: 任意の `AsyncRead + AsyncWrite` を包み、ネットワーク断・遅延・
//! パケットロスをシミュレートする。サンドボックスに `tc`/`netem` が無い
//! (CAP_NET_ADMIN が実効的に付与されていない) ため、OS レベルではなく
//! ソケットラッパーとしてアプリケーション層でこれらの障害を再現する。
//!
//! TCP 経路 (`run_russh_transport`) も QUIC 経路 (`quic_transport`,
//! `helper_quic_transport`) も、russh には `AsyncRead + AsyncWrite` の
//! ストリームとして渡っているだけなので、このラッパーを間に挟めば両方の
//! テストで使い回せる。
//!
//! パケットロスは「バイトを破損させる」のではなく「TCP の再送に相当する
//! 遅延を発生させる」形でモデル化している。実際のパケットロスは TCP に
//! よって透過的に再送されるため、アプリケーション層から見える影響は
//! バイト欠落ではなく遅延・ジッタである、という前提に合わせている。

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use rand::Rng;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Sleep;

const DEFAULT_RETRANSMIT_PENALTY: Duration = Duration::from_millis(500);

struct FaultState {
    latency_ms: AtomicU64,
    loss_permille: AtomicU64,
    retransmit_penalty_ms: AtomicU64,
    cut: AtomicBool,
}

/// `FaultyStream` を操作するハンドル。クローンして保持すれば、ストリームが
/// 使用中でも遅延・ロス率の変更や `cut()` による強制切断をテスト側から
/// 動的に行える。
#[derive(Clone)]
pub(crate) struct FaultInjector {
    state: Arc<FaultState>,
}

impl FaultInjector {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(FaultState {
                latency_ms: AtomicU64::new(0),
                loss_permille: AtomicU64::new(0),
                retransmit_penalty_ms: AtomicU64::new(DEFAULT_RETRANSMIT_PENALTY.as_millis() as u64),
                cut: AtomicBool::new(false),
            }),
        }
    }

    /// すべての read/write に加える固定遅延。
    pub(crate) fn set_latency(&self, latency: Duration) {
        self.state.latency_ms.store(latency.as_millis() as u64, Ordering::Relaxed);
    }

    /// 0.0〜1.0 のパケットロス率。ロスと判定された read/write には
    /// `retransmit_penalty` 分の追加遅延が発生する。
    pub(crate) fn set_loss_rate(&self, rate: f64) {
        let permille = (rate.clamp(0.0, 1.0) * 1000.0).round() as u64;
        self.state.loss_permille.store(permille, Ordering::Relaxed);
    }

    pub(crate) fn set_retransmit_penalty(&self, penalty: Duration) {
        self.state.retransmit_penalty_ms.store(penalty.as_millis() as u64, Ordering::Relaxed);
    }

    /// 即座にネットワーク切断状態にする。以降の read は EOF、write は
    /// `ConnectionReset` を返すようになる。
    pub(crate) fn cut(&self) {
        self.state.cut.store(true, Ordering::Relaxed);
    }

    fn is_cut(&self) -> bool {
        self.state.cut.load(Ordering::Relaxed)
    }

    fn sample_delay(&self) -> Duration {
        let base = Duration::from_millis(self.state.latency_ms.load(Ordering::Relaxed));
        let permille = self.state.loss_permille.load(Ordering::Relaxed);
        let lost = permille > 0 && rand::thread_rng().gen_range(0..1000) < permille;
        if lost {
            let penalty = Duration::from_millis(self.state.retransmit_penalty_ms.load(Ordering::Relaxed));
            base + penalty
        } else {
            base
        }
    }
}

/// `S` を包み、`FaultInjector` で設定された遅延・パケットロス・切断を
/// 各 read/write に適用する。`client::connect_stream` や `tokio::io::join`
/// が受け取る箇所にそのまま差し込める。
pub(crate) struct FaultyStream<S> {
    inner: S,
    injector: FaultInjector,
    delay: Option<Pin<Box<Sleep>>>,
}

impl<S> FaultyStream<S> {
    pub(crate) fn new(inner: S, injector: FaultInjector) -> Self {
        Self { inner, injector, delay: None }
    }

    /// 遅延ゲートを通過するまで Pending を返す。通過したら Ready(()) を返し、
    /// 呼び出し側は inner の poll_read/poll_write に進んでよい。
    fn poll_delay(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            if let Some(sleep) = self.delay.as_mut() {
                match sleep.as_mut().poll(cx) {
                    Poll::Ready(()) => {
                        self.delay = None;
                        return Poll::Ready(());
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
            let dur = self.injector.sample_delay();
            if dur.is_zero() {
                return Poll::Ready(());
            }
            self.delay = Some(Box::pin(tokio::time::sleep(dur)));
        }
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
        match self.poll_delay(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(()) => {}
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
        match self.poll_delay(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(()) => {}
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
    use std::time::Instant;
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
    async fn applies_fixed_latency_to_reads() {
        let (mut client, server) = tokio::io::duplex(64);
        let injector = FaultInjector::new();
        injector.set_latency(Duration::from_millis(200));
        let mut faulty = FaultyStream::new(server, injector);

        client.write_all(b"hi").await.unwrap();
        let start = Instant::now();
        let mut buf = [0u8; 2];
        faulty.read_exact(&mut buf).await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(200));
    }

    #[tokio::test]
    async fn loss_rate_adds_retransmit_penalty_without_dropping_bytes() {
        let (mut client, server) = tokio::io::duplex(64);
        let injector = FaultInjector::new();
        injector.set_loss_rate(1.0); // 常にロス扱い
        injector.set_retransmit_penalty(Duration::from_millis(150));
        let mut faulty = FaultyStream::new(server, injector);

        client.write_all(b"data").await.unwrap();
        let start = Instant::now();
        let mut buf = [0u8; 4];
        faulty.read_exact(&mut buf).await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(150));
        assert_eq!(&buf, b"data"); // バイトは欠落しない
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
