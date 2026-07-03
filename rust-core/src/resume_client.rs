//! Phase 8-2/8-3: client 側 input replay buffer（C→S 方向の resume 用）と、
//! 実際の reattach（`RESUME` フレーム送受信・裏での再接続）を行う
//! `ReattachableStream`。契約の詳細は `/HELPER_PROTOCOL.md` §7 を参照。
//!
//! `rust-core/isekai-helper/src/resume.rs` の `OutputBuffer` と対になる、
//! 逆方向（C→S）のバッファ。データ構造は同じ設計（バウンデッド・確認済み
//! offset 破棄・指定 offset からの再送）だが、別クレート（`isekai-helper` は
//! 独立したバイナリクレート）のため共有せず重複実装している。

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub(crate) const CONTROL_HELLO: u8 = 0x10;
pub(crate) const CONTROL_ACK: u8 = 0x11;
pub(crate) const APP_ACK: u8 = 0x12;
pub(crate) const RESUME: u8 = 0x03;
pub(crate) const RESUME_ACK: u8 = 0x13;
#[allow(dead_code)]
pub(crate) const REJECT_UNKNOWN_SESSION: u8 = 0xF9;
#[allow(dead_code)]
pub(crate) const REJECT_OFFSET_GONE: u8 = 0xF8;

pub(crate) type SessionId = [u8; 16];

/// C→S 方向（client → helper）に送出したバイト列を保持するバウンデッドバッファ。
/// helper 側 `OutputBuffer`（isekai-helper/src/resume.rs）と同じ設計。
pub(crate) struct ReplayBuffer {
    data: VecDeque<u8>,
    start_offset: u64,
    capacity: usize,
}

impl ReplayBuffer {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(capacity.min(1 << 20)),
            start_offset: 0,
            capacity,
        }
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) {
        self.data.extend(bytes.iter().copied());
        while self.data.len() > self.capacity {
            self.data.pop_front();
            self.start_offset += 1;
        }
    }

    /// helper が `helper_committed_offset` として確認した位置まで破棄する。
    pub(crate) fn advance_start(&mut self, confirmed_offset: u64) {
        while self.start_offset < confirmed_offset && !self.data.is_empty() {
            self.data.pop_front();
            self.start_offset += 1;
        }
    }

    #[allow(dead_code)]
    pub(crate) fn start_offset(&self) -> u64 {
        self.start_offset
    }

    pub(crate) fn end_offset(&self) -> u64 {
        self.start_offset + self.data.len() as u64
    }

    /// Phase 8-3（reattach ハンドシェイク）で使用する。8-2 の時点では未配線。
    #[allow(dead_code)]
    pub(crate) fn replay_from(&self, from: u64) -> Option<Vec<u8>> {
        if from < self.start_offset || from > self.end_offset() {
            return None;
        }
        let skip = (from - self.start_offset) as usize;
        Some(self.data.iter().skip(skip).copied().collect())
    }
}

/// resume 用に client 側が保持する状態。C→S は `replay_buffer` に tee、
/// S→C は `client_delivered_offset` を進めるだけ（helper 側が output buffer を
/// 持つため client 側は受信データを保持し直す必要が無い）。
pub(crate) struct ClientResumeState {
    pub(crate) replay_buffer: ReplayBuffer,
    pub(crate) client_delivered_offset: u64,
    /// control stream 確立時に helper が発行した session_id。
    /// Phase 8-3（reattach ハンドシェイク）で `RESUME` フレームに使う。
    #[allow(dead_code)]
    pub(crate) session_id: Option<SessionId>,
}

impl ClientResumeState {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            replay_buffer: ReplayBuffer::new(capacity),
            client_delivered_offset: 0,
            session_id: None,
        }
    }
}

/// data stream (`S: AsyncRead + AsyncWrite`) を包み、読み書きしたバイト数を
/// `ClientResumeState` に tee する。control stream が使えない（旧 helper 等）
/// 場合でもこの wrapper 自体は素通しとして機能するため、呼び出し側は常に
/// これで包んでよい。
///
/// Phase 8-3 で `ReattachableStream` が導入されてからは、本番コードは
/// こちらではなくそちらを使う（QUIC connection 消失時に russh へエラーを
/// 見せずに reattach できるのは `ReattachableStream` のみ）。この型は
/// 「offset を tee するだけ」の最小構成としてテスト・将来の用途に残してある。
#[cfg(test)]
pub(crate) struct ResumeAwareStream<S> {
    inner: S,
    state: Arc<Mutex<ClientResumeState>>,
}

#[cfg(test)]
impl<S> ResumeAwareStream<S> {
    pub(crate) fn new(inner: S, state: Arc<Mutex<ClientResumeState>>) -> Self {
        Self { inner, state }
    }
}

#[cfg(test)]
impl<S: AsyncRead + Unpin> AsyncRead for ResumeAwareStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            let n = buf.filled().len() - before;
            if n > 0 {
                this.state.lock().unwrap().client_delivered_offset += n as u64;
            }
        }
        poll
    }
}

#[cfg(test)]
impl<S: AsyncWrite + Unpin> AsyncWrite for ResumeAwareStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &poll {
            if *n > 0 {
                this.state.lock().unwrap().replay_buffer.append(&buf[..*n]);
            }
        }
        poll
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

// ── Phase 8-3: reattach 対応ストリーム ──────────────────────────────

/// reattach（新しい QUIC connection への `RESUME` 送信）が成功した結果。
pub(crate) struct ReattachResult {
    pub(crate) send: quinn::SendStream,
    pub(crate) recv: quinn::RecvStream,
    /// helper が確認した C→S オフセット。これより前の replay_buffer は破棄してよい。
    pub(crate) helper_committed_offset: u64,
}

/// 1回の reattach 試行を行う関数の型。呼び出し元（`helper_quic_transport.rs`）が
/// quinn/rustls の具体的な接続手順を実装し、`ReattachableStream` はこれを
/// 抽象的に呼び出すだけにする（層を分離する）。
pub(crate) type ReattachFn = Arc<
    dyn Fn(SessionId, u64, u64) -> Pin<Box<dyn Future<Output = Result<ReattachResult, String>> + Send>>
        + Send
        + Sync,
>;

enum StreamSlot {
    Connected(quinn::RecvStream, quinn::SendStream),
    /// 背後で reattach 試行中。完了したら `Connected` か `Failed` に遷移する。
    Reattaching,
    /// リトライ上限に達し、諦めた。以降の poll は実際の I/O エラーを返す
    /// （呼び出し元＝ russh がここで初めて「セッションが本当に切れた」と認識する）。
    Failed(String),
}

struct ReattachInner {
    slot: Mutex<StreamSlot>,
    wakers: Mutex<Vec<Waker>>,
    /// `helper_quic_transport.rs` の control stream タスク（APP_ACK 送受信）とも
    /// 共有するため、外部で作られた `Arc<Mutex<_>>` をそのまま保持する
    /// （このモジュール自身は所有しない）。
    resume: Arc<Mutex<ClientResumeState>>,
    reattach_fn: ReattachFn,
    /// 二重に reattach タスクを起動しないためのガード。
    reattaching_started: std::sync::atomic::AtomicBool,
}

impl ReattachInner {
    fn register_waker(&self, cx: &Context<'_>) {
        self.wakers.lock().unwrap().push(cx.waker().clone());
    }

    fn wake_all(&self) {
        for w in self.wakers.lock().unwrap().drain(..) {
            w.wake();
        }
    }
}

/// data stream を包み、QUIC connection が失われても（`RESUME` による reattach が
/// 成功する限り）呼び出し元（russh）に I/O エラーを見せない。
///
/// 設計上の要点: russh の `client::connect_stream` は渡された stream 上で
/// SSH プロトコル状態（鍵・MAC シーケンス番号等）を保持し続ける。stream が
/// 一度でも I/O エラーを返すと russh はそのセッションを終了とみなすため、
/// 「同じ SSH セッションを継続する」という Phase 8 の目的を達成するには、
/// **stream オブジェクト自身が背後で新しい QUIC connection に張り替わり、
/// 呼び出し元には何も気づかせない**必要がある。エラーを見た poll_read/
/// poll_write は即座にエラーを返さず、reattach タスクを起動して
/// `Poll::Pending` を返し、reattach 完了時に waker で起こす。
/// リトライ上限に達した場合のみ、最終的に実際のエラーを返す。
#[derive(Clone)]
pub(crate) struct ReattachableStream {
    inner: Arc<ReattachInner>,
}

/// リトライ回数・間隔（固定値。指数バックオフ）。
const REATTACH_MAX_RETRIES: u32 = 5;
const REATTACH_BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

impl ReattachableStream {
    /// data stream の read/write が I/O エラーを返した際の共通処理。
    /// エラーを呼び出し元（russh）にはまだ見せず、reattach を起動して
    /// waker を登録するところまでを行う（`Poll::Pending` を返すのは呼び出し側）。
    fn begin_reattach_after_io_error(&self, cx: &Context<'_>, e: impl std::fmt::Display, direction: &str) {
        log::warn!("reattach: data stream {direction} failed ({e}), triggering reattach");
        self.trigger_reattach();
        self.inner.register_waker(cx);
    }

    pub(crate) fn new(
        send: quinn::SendStream,
        recv: quinn::RecvStream,
        resume_state: Arc<Mutex<ClientResumeState>>,
        reattach_fn: ReattachFn,
    ) -> Self {
        Self {
            inner: Arc::new(ReattachInner {
                slot: Mutex::new(StreamSlot::Connected(recv, send)),
                wakers: Mutex::new(Vec::new()),
                resume: resume_state,
                reattach_fn,
                reattaching_started: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    fn trigger_reattach(&self) {
        use std::sync::atomic::Ordering;
        if self
            .inner
            .reattaching_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            // 既に別の poll がトリガー済み。
            return;
        }
        *self.inner.slot.lock().unwrap() = StreamSlot::Reattaching;

        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                let (session_id, client_sent_offset, client_delivered_offset) = {
                    let resume = inner.resume.lock().unwrap();
                    let Some(id) = resume.session_id else {
                        // control stream が一度も確立していない = resume 不可能。
                        let mut slot = inner.slot.lock().unwrap();
                        *slot = StreamSlot::Failed("no session_id, resume not supported for this connection".into());
                        drop(slot);
                        inner.wake_all();
                        return;
                    };
                    (id, resume.replay_buffer.end_offset(), resume.client_delivered_offset)
                };

                log::info!("reattach: attempt {attempt}/{REATTACH_MAX_RETRIES}");
                match (inner.reattach_fn)(session_id, client_sent_offset, client_delivered_offset).await {
                    Ok(mut result) => {
                        // helper がまだ受け取っていない C→S バイト列を、通常の
                        // relay に戻す前に replay しておく。
                        let to_replay = {
                            let resume = inner.resume.lock().unwrap();
                            resume.replay_buffer.replay_from(result.helper_committed_offset)
                        };
                        if let Some(bytes) = to_replay {
                            if !bytes.is_empty() {
                                if let Err(e) = result.send.write_all(&bytes).await {
                                    log::warn!("reattach: failed to replay C->S bytes: {e}");
                                    if attempt >= REATTACH_MAX_RETRIES {
                                        *inner.slot.lock().unwrap() = StreamSlot::Failed(e.to_string());
                                        inner.wake_all();
                                        return;
                                    }
                                    tokio::time::sleep(REATTACH_BASE_DELAY * 2u32.pow(attempt - 1)).await;
                                    continue;
                                }
                            }
                        }
                        {
                            let mut resume = inner.resume.lock().unwrap();
                            resume.replay_buffer.advance_start(result.helper_committed_offset);
                        }
                        *inner.slot.lock().unwrap() = StreamSlot::Connected(result.recv, result.send);
                        inner.reattaching_started.store(false, Ordering::SeqCst);
                        log::info!("reattach: succeeded on attempt {attempt}");
                        inner.wake_all();
                        return;
                    }
                    Err(e) => {
                        log::warn!("reattach: attempt {attempt} failed: {e}");
                        if attempt >= REATTACH_MAX_RETRIES {
                            *inner.slot.lock().unwrap() = StreamSlot::Failed(e);
                            inner.wake_all();
                            return;
                        }
                        let delay = REATTACH_BASE_DELAY * 2u32.pow(attempt - 1);
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        });
    }
}

impl AsyncRead for ReattachableStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let mut slot = this.inner.slot.lock().unwrap();
        match &mut *slot {
            StreamSlot::Connected(recv, _send) => {
                let before = buf.filled().len();
                match Pin::new(recv).poll_read(cx, buf) {
                    Poll::Ready(Ok(())) => {
                        let n = buf.filled().len() - before;
                        drop(slot);
                        if n > 0 {
                            this.inner.resume.lock().unwrap().client_delivered_offset += n as u64;
                        }
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(Err(e)) => {
                        drop(slot);
                        this.begin_reattach_after_io_error(cx, e, "read");
                        Poll::Pending
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            StreamSlot::Reattaching => {
                drop(slot);
                this.inner.register_waker(cx);
                Poll::Pending
            }
            StreamSlot::Failed(msg) => {
                let err = io::Error::new(io::ErrorKind::NotConnected, msg.clone());
                Poll::Ready(Err(err))
            }
        }
    }
}

impl AsyncWrite for ReattachableStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let mut slot = this.inner.slot.lock().unwrap();
        match &mut *slot {
            StreamSlot::Connected(_recv, send) => match Pin::new(send).poll_write(cx, buf) {
                Poll::Ready(Ok(n)) => {
                    drop(slot);
                    if n > 0 {
                        this.inner.resume.lock().unwrap().replay_buffer.append(&buf[..n]);
                    }
                    Poll::Ready(Ok(n))
                }
                Poll::Ready(Err(e)) => {
                    drop(slot);
                    this.begin_reattach_after_io_error(cx, e, "write");
                    Poll::Pending
                }
                Poll::Pending => Poll::Pending,
            },
            StreamSlot::Reattaching => {
                drop(slot);
                this.inner.register_waker(cx);
                Poll::Pending
            }
            StreamSlot::Failed(msg) => {
                let err = io::Error::new(io::ErrorKind::NotConnected, msg.clone());
                Poll::Ready(Err(err))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let mut slot = this.inner.slot.lock().unwrap();
        match &mut *slot {
            StreamSlot::Connected(_recv, send) => Pin::new(send).poll_flush(cx),
            StreamSlot::Reattaching => {
                drop(slot);
                this.inner.register_waker(cx);
                Poll::Pending
            }
            StreamSlot::Failed(msg) => Poll::Ready(Err(io::Error::new(io::ErrorKind::NotConnected, msg.clone()))),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let mut slot = this.inner.slot.lock().unwrap();
        match &mut *slot {
            StreamSlot::Connected(_recv, send) => Pin::new(send).poll_shutdown(cx),
            // shutdown 中に reattach が要る状態なら、素直に成功扱いにする
            // （呼び出し元は既にセッションを終わらせようとしている）。
            StreamSlot::Reattaching | StreamSlot::Failed(_) => Poll::Ready(Ok(())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_replay_full_range() {
        let mut buf = ReplayBuffer::new(1024);
        buf.append(b"hello");
        buf.append(b" world");
        assert_eq!(buf.end_offset(), 11);
        assert_eq!(buf.replay_from(0).unwrap(), b"hello world");
        assert_eq!(buf.replay_from(5).unwrap(), b" world");
    }

    #[test]
    fn advance_start_discards_confirmed_prefix() {
        let mut buf = ReplayBuffer::new(1024);
        buf.append(b"0123456789");
        buf.advance_start(4);
        assert_eq!(buf.start_offset(), 4);
        assert!(buf.replay_from(0).is_none());
        assert_eq!(buf.replay_from(4).unwrap(), b"456789");
    }

    #[test]
    fn capacity_overflow_evicts_oldest_bytes() {
        let mut buf = ReplayBuffer::new(4);
        buf.append(b"abcdefgh");
        assert_eq!(buf.start_offset(), 4);
        assert_eq!(buf.replay_from(4).unwrap(), b"efgh");
    }

    #[tokio::test]
    async fn resume_aware_stream_tracks_offsets_through_duplex() {
        let (client_side, server_side) = tokio::io::duplex(64);
        let state = Arc::new(Mutex::new(ClientResumeState::new(1024)));
        let mut wrapped = ResumeAwareStream::new(client_side, state.clone());

        let mut server_side = server_side;
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 5];
            server_side.read_exact(&mut buf).await.unwrap();
            server_side.write_all(b"reply").await.unwrap();
        });

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        wrapped.write_all(b"hello").await.unwrap();
        let mut resp = [0u8; 5];
        wrapped.read_exact(&mut resp).await.unwrap();
        assert_eq!(&resp, b"reply");

        // std::sync::Mutex 経由で poll_write/poll_read 内から同期的に更新するため、
        // write_all/read_exact の完了時点で即座に反映されているはず。
        let s = state.lock().unwrap();
        assert_eq!(s.replay_buffer.end_offset(), 5, "C->S 5 bytes 送信のはず");
        assert_eq!(s.client_delivered_offset, 5, "S->C 5 bytes 受信のはず");
        assert_eq!(s.replay_buffer.replay_from(0).unwrap(), b"hello");
    }

    /// Phase 8-4: reattach が `REATTACH_MAX_RETRIES` 回すべて失敗し続けた場合、
    /// `ReattachableStream` は無限に `Pending` を返し続けるのではなく、最終的に
    /// 呼び出し元（russh）へ実際の `io::Error` を返すことを確認する。
    /// 指数バックオフの累計待ち時間（1+2+4+8=15秒）は仮想時間で進める。
    #[tokio::test(start_paused = true)]
    async fn reattach_gives_up_after_max_retries_and_surfaces_error() {
        let resume_state = Arc::new(Mutex::new(ClientResumeState {
            replay_buffer: ReplayBuffer::new(1024),
            client_delivered_offset: 0,
            session_id: Some([7u8; 16]),
        }));
        let reattach_fn: ReattachFn =
            Arc::new(|_id, _sent, _delivered| Box::pin(async { Err("mock: helper unreachable".to_string()) }));
        let inner = Arc::new(ReattachInner {
            // 初期値は trigger_reattach が即座に上書きするので何でもよい。
            slot: Mutex::new(StreamSlot::Reattaching),
            wakers: Mutex::new(Vec::new()),
            resume: resume_state,
            reattach_fn,
            reattaching_started: std::sync::atomic::AtomicBool::new(false),
        });
        let stream = ReattachableStream { inner };

        stream.trigger_reattach();

        // バックオフの累計(15秒)を確実に超えるまで、少しずつ仮想時間を進めながら
        // 都度スケジューラに制御を返す（一度に大きく進めるとタイマーの発火順が
        // 保証されないため）。
        for _ in 0..20 {
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }

        let mut buf = [0u8; 1];
        let mut read_buf = ReadBuf::new(&mut buf);
        let err = std::future::poll_fn(|cx| Pin::new(&mut stream.clone()).poll_read(cx, &mut read_buf))
            .await
            .expect_err("5回リトライを使い切ったら poll_read は実エラーを返すはず");
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        assert!(err.to_string().contains("mock: helper unreachable"));
    }
}
