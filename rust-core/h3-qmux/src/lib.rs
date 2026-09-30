//! QUIC transport implementation for h3 based on `qmux` (draft-ietf-quic-qmux
//! — a polyfill of QUIC's stream/datagram API over a reliable byte stream,
//! e.g. TLS-over-TCP). Mirrors `h3-noq`'s structure (`Connection`,
//! `OpenStreams`, `BidiStream`, `RecvStream`, `SendStream`), since
//! `qmux::Session` exposes roughly the same open/accept-stream shape as
//! `noq::Connection` — the notable differences accounted for here:
//!
//! - `qmux`'s `Session::{open_bi,accept_bi,open_uni,accept_uni}` and
//!   `SendStream::write_all` / `RecvStream::read_chunk` are all `async fn`s
//!   with no poll-based equivalent (unlike noq, which exposes `poll_write`
//!   directly on `noq::SendStream`), so both directions need
//!   `ReusableBoxFuture`-based bridging into h3's poll-based
//!   `quic::SendStream`/`quic::RecvStream` traits, not just the receive
//!   side (h3-noq only needs it for receive).
//! - `qmux::{SendStream,RecvStream}` expose no stream-id accessor (unlike
//!   `noq::{SendStream,RecvStream}::id()`), even though `qmux` streams *do*
//!   carry a real RFC 9000-style id on the wire internally (it reuses QUIC's
//!   STREAM frame encoding verbatim) — that id is just not part of the
//!   crate's public API. h3-datagram's quarter-stream-id routing needs this
//!   real, peer-correlated id (not merely a locally-unique one — an earlier
//!   version of this adapter used a single flat counter and broke datagram
//!   routing because the client's and server's counters diverged), so
//!   `StreamIdAllocator` reconstructs it by applying RFC 9000's own
//!   deterministic numbering rule locally on both ends: `index << 2 |
//!   direction_bit | initiator_bit`, counting "the Nth stream I opened" and
//!   "the Nth stream I accepted" separately per (direction, initiator)
//!   category. For streams *we* open this is exact (our own open order is
//!   the id order). For *peer*-opened streams it is not guaranteed:
//!   `qmux::Session` delivers them to `accept_*` in first-frame arrival
//!   order, so a later stream can overtake an earlier one. Peer-opened uni
//!   stream ids are never used for routing (h3 control/QPACK streams), so
//!   that's harmless; for peer-opened *bidi* streams (request streams, whose
//!   id is the datagram routing key) only the first one is accepted — see
//!   `poll_accept_bidi`.
#![deny(missing_docs)]

use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{self, ready, Poll},
};

use bytes::{Buf, Bytes};
use futures_util::{
    stream::{self},
    Stream, StreamExt,
};
use tokio_util::sync::ReusableBoxFuture;
use web_transport_trait::{RecvStream as _, SendStream as _, Session as _};

use h3::{
    error::Code,
    quic::{self, ConnectionErrorIncoming, StreamErrorIncoming, WriteBuf},
};

#[cfg(feature = "tracing")]
use tracing::instrument;

#[cfg(feature = "datagram")]
pub mod datagram;

/// BoxStream with Sync trait
type BoxStreamSync<'a, T> = Pin<Box<dyn Stream<Item = T> + Sync + Send + 'a>>;

/// Reconstructs real, peer-correlated RFC 9000 stream ids locally on both
/// ends of a `qmux::Session` — see the module docs for why this is necessary
/// and why it's safe (both peers observe their own opened/accepted streams
/// in the same relative order per category).
struct StreamIdAllocator {
    is_server: bool,
    local_bidi: AtomicU64,
    local_uni: AtomicU64,
    remote_bidi: AtomicU64,
    remote_uni: AtomicU64,
}

impl StreamIdAllocator {
    fn new(is_server: bool) -> Self {
        Self {
            is_server,
            local_bidi: AtomicU64::new(0),
            local_uni: AtomicU64::new(0),
            remote_bidi: AtomicU64::new(0),
            remote_uni: AtomicU64::new(0),
        }
    }

    /// A stream we're about to `open_bi` on.
    fn next_local_bidi(&self) -> quic::StreamId {
        Self::encode(self.local_bidi.fetch_add(1, Ordering::Relaxed), false, self.is_server)
    }

    /// A stream we're about to `open_uni` on.
    fn next_local_uni(&self) -> quic::StreamId {
        Self::encode(self.local_uni.fetch_add(1, Ordering::Relaxed), true, self.is_server)
    }

    /// A stream we just `accept_bi`'d — opened by the peer, so the
    /// initiator bit reflects the peer's role, not ours.
    fn next_remote_bidi(&self) -> quic::StreamId {
        Self::encode(self.remote_bidi.fetch_add(1, Ordering::Relaxed), false, !self.is_server)
    }

    /// A stream we just `accept_uni`'d.
    fn next_remote_uni(&self) -> quic::StreamId {
        Self::encode(self.remote_uni.fetch_add(1, Ordering::Relaxed), true, !self.is_server)
    }

    /// RFC 9000 §2.1's own bit layout — matches `qmux::StreamId::new`'s
    /// internal formula exactly (verified against `moq-dev/web-transport`'s
    /// `qmux::stream::StreamId::new`), which is why reconstructing it here
    /// produces the same id `qmux::Session` assigned internally.
    fn encode(index: u64, is_uni: bool, opener_is_server: bool) -> quic::StreamId {
        let mut id = index << 2;
        if is_uni {
            id |= 0x02;
        }
        if opener_is_server {
            id |= 0x01;
        }
        id.try_into().expect("stream index should stay within h3's StreamId range")
    }
}

/// A QUIC connection backed by qmux
///
/// Implements a [`quic::Connection`] backed by a [`qmux::Session`].
pub struct Connection {
    session: qmux::Session,
    ids: Arc<StreamIdAllocator>,
    incoming_bi: BoxStreamSync<'static, Result<(qmux::SendStream, qmux::RecvStream), qmux::Error>>,
    opening_bi: Option<BoxStreamSync<'static, Result<(qmux::SendStream, qmux::RecvStream), qmux::Error>>>,
    incoming_uni: BoxStreamSync<'static, Result<qmux::RecvStream, qmux::Error>>,
    opening_uni: Option<BoxStreamSync<'static, Result<qmux::SendStream, qmux::Error>>>,
}

impl Connection {
    /// Create a [`Connection`] from a [`qmux::Session`]. `is_server` must
    /// reflect which side of the handshake produced this session
    /// (`qmux::Session::accept` → `true`, `qmux::Session::connect` →
    /// `false`) — see `StreamIdAllocator`'s docs for why this is needed.
    pub fn new(session: qmux::Session, is_server: bool) -> Self {
        Self {
            session: session.clone(),
            ids: Arc::new(StreamIdAllocator::new(is_server)),
            incoming_bi: Box::pin(stream::unfold(session.clone(), |session| async {
                Some((session.accept_bi().await, session))
            })),
            opening_bi: None,
            incoming_uni: Box::pin(stream::unfold(session.clone(), |session| async {
                Some((session.accept_uni().await, session))
            })),
            opening_uni: None,
        }
    }
}

impl<B> quic::Connection<B> for Connection
where
    B: Buf + Send + 'static,
{
    type RecvStream = RecvStream;
    type OpenStreams = OpenStreams;

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_accept_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, ConnectionErrorIncoming>> {
        let (send, recv) = ready!(self.incoming_bi.poll_next_unpin(cx))
            .expect("self.incoming_bi BoxStream never returns None")
            .map_err(convert_connection_error)?;
        // `qmux` delivers peer-initiated streams in *first-frame arrival*
        // order (a later stream whose first frame overtakes an earlier one's
        // is delivered first — no implicit in-order opening), so the
        // locally reconstructed id of the second and later peer-initiated
        // bidi stream may not be its real wire id, and h3-datagram's
        // quarter-stream-id routing would silently mis-route. Only the first
        // one is guaranteed (it is always index 0). Fail closed rather than
        // hand out a possibly wrong id — this crate's own use is client-only
        // (the peer never opens bidi streams) and a single-request server.
        if self.ids.remote_bidi.load(Ordering::Relaxed) >= 1 {
            return Poll::Ready(Err(ConnectionErrorIncoming::InternalError(
                "h3-qmux: more than one peer-initiated bidi stream is unsupported (qmux exposes no stream ids, \
                 so a correct id cannot be reconstructed)"
                    .to_string(),
            )));
        }
        // One bidi stream = one id, shared by both halves.
        let id = self.ids.next_remote_bidi();
        Poll::Ready(Ok(Self::BidiStream {
            send: Self::SendStream::new(send, id),
            recv: Self::RecvStream::new(recv, id),
        }))
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_accept_recv(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::RecvStream, ConnectionErrorIncoming>> {
        let recv = ready!(self.incoming_uni.poll_next_unpin(cx))
            .expect("self.incoming_uni BoxStream never returns None")
            .map_err(convert_connection_error)?;
        Poll::Ready(Ok(Self::RecvStream::new(recv, self.ids.next_remote_uni())))
    }

    fn opener(&self) -> Self::OpenStreams {
        OpenStreams {
            session: self.session.clone(),
            ids: self.ids.clone(),
            opening_bi: None,
            opening_uni: None,
        }
    }
}

impl<B> quic::OpenStreams<B> for Connection
where
    B: Buf + Send + 'static,
{
    type SendStream = SendStream<B>;
    type BidiStream = BidiStream<B>;

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_open_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        let bi = self.opening_bi.get_or_insert_with(|| {
            Box::pin(stream::unfold(self.session.clone(), |session| async {
                Some((session.open_bi().await, session))
            }))
        });
        let (send, recv) = ready!(bi.poll_next_unpin(cx))
            .expect("BoxStream does not return None")
            .map_err(convert_stream_error)?;
        let id = self.ids.next_local_bidi();
        Poll::Ready(Ok(Self::BidiStream {
            send: Self::SendStream::new(send, id),
            recv: RecvStream::new(recv, id),
        }))
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_open_send(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        let uni = self.opening_uni.get_or_insert_with(|| {
            Box::pin(stream::unfold(self.session.clone(), |session| async {
                Some((session.open_uni().await, session))
            }))
        });

        let send = ready!(uni.poll_next_unpin(cx))
            .expect("BoxStream does not return None")
            .map_err(convert_stream_error)?;
        Poll::Ready(Ok(Self::SendStream::new(send, self.ids.next_local_uni())))
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn close(&mut self, code: Code, reason: &[u8]) {
        self.session.close(code_to_u32(code.value()), &String::from_utf8_lossy(reason));
    }
}

/// An h3 error code (a QUIC varint, up to 2^62) narrowed to qmux's `u32`
/// codes. A value that doesn't fit used to be silently truncated (`as u32`)
/// into an unrelated code; it now maps to `H3_INTERNAL_ERROR`.
fn code_to_u32(code: u64) -> u32 {
    u32::try_from(code).unwrap_or(Code::H3_INTERNAL_ERROR.value() as u32)
}

fn convert_connection_error(e: qmux::Error) -> h3::quic::ConnectionErrorIncoming {
    match e {
        // Keep the peer's close code (it used to be replaced by 0, i.e.
        // H3_NO_ERROR-ish, hiding why the peer closed).
        qmux::Error::ConnectionClosed { code, .. } => ConnectionErrorIncoming::ApplicationClose { error_code: code.into() },
        qmux::Error::IdleTimeout | qmux::Error::HandshakeTimeout => ConnectionErrorIncoming::Timeout,
        other => ConnectionErrorIncoming::Undefined(Arc::new(other)),
    }
}

fn convert_stream_error(e: qmux::Error) -> StreamErrorIncoming {
    match e {
        qmux::Error::StreamReset(code) => StreamErrorIncoming::StreamTerminated { error_code: code.into() },
        qmux::Error::StreamStop(code) => StreamErrorIncoming::StreamTerminated { error_code: code.into() },
        other @ (qmux::Error::ConnectionClosed { .. } | qmux::Error::IdleTimeout | qmux::Error::HandshakeTimeout) => {
            StreamErrorIncoming::ConnectionErrorIncoming { connection_error: convert_connection_error(other) }
        }
        other => StreamErrorIncoming::Unknown(Box::new(other)),
    }
}

/// Stream opener backed by a qmux session
///
/// Implements [`quic::OpenStreams`] using [`qmux::Session`].
pub struct OpenStreams {
    session: qmux::Session,
    ids: Arc<StreamIdAllocator>,
    opening_bi: Option<BoxStreamSync<'static, Result<(qmux::SendStream, qmux::RecvStream), qmux::Error>>>,
    opening_uni: Option<BoxStreamSync<'static, Result<qmux::SendStream, qmux::Error>>>,
}

impl<B> quic::OpenStreams<B> for OpenStreams
where
    B: Buf + Send + 'static,
{
    type SendStream = SendStream<B>;
    type BidiStream = BidiStream<B>;

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_open_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        let bi = self.opening_bi.get_or_insert_with(|| {
            Box::pin(stream::unfold(self.session.clone(), |session| async {
                Some((session.open_bi().await, session))
            }))
        });
        let (send, recv) = ready!(bi.poll_next_unpin(cx))
            .expect("BoxStream does not return None")
            .map_err(convert_stream_error)?;
        let id = self.ids.next_local_bidi();
        Poll::Ready(Ok(Self::BidiStream {
            send: Self::SendStream::new(send, id),
            recv: RecvStream::new(recv, id),
        }))
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_open_send(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        let uni = self.opening_uni.get_or_insert_with(|| {
            Box::pin(stream::unfold(self.session.clone(), |session| async {
                Some((session.open_uni().await, session))
            }))
        });

        let send = ready!(uni.poll_next_unpin(cx))
            .expect("BoxStream does not return None")
            .map_err(convert_stream_error)?;
        Poll::Ready(Ok(Self::SendStream::new(send, self.ids.next_local_uni())))
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn close(&mut self, code: Code, reason: &[u8]) {
        self.session.close(code_to_u32(code.value()), &String::from_utf8_lossy(reason));
    }
}

impl Clone for OpenStreams {
    fn clone(&self) -> Self {
        Self {
            session: self.session.clone(),
            ids: self.ids.clone(),
            opening_bi: None,
            opening_uni: None,
        }
    }
}

/// qmux-backed bidirectional stream
///
/// Implements [`quic::BidiStream`] which allows the stream to be split
/// into two structs each implementing one direction.
pub struct BidiStream<B>
where
    B: Buf + Send + 'static,
{
    send: SendStream<B>,
    recv: RecvStream,
}

impl<B> quic::BidiStream<B> for BidiStream<B>
where
    B: Buf + Send + 'static,
{
    type SendStream = SendStream<B>;
    type RecvStream = RecvStream;

    fn split(self) -> (Self::SendStream, Self::RecvStream) {
        (self.send, self.recv)
    }
}

impl<B: Buf + Send + 'static> quic::RecvStream for BidiStream<B> {
    type Buf = Bytes;

    fn poll_data(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, StreamErrorIncoming>> {
        self.recv.poll_data(cx)
    }

    fn stop_sending(&mut self, error_code: u64) {
        self.recv.stop_sending(error_code)
    }

    fn recv_id(&self) -> quic::StreamId {
        self.recv.recv_id()
    }
}

impl<B> quic::SendStream<B> for BidiStream<B>
where
    B: Buf + Send + 'static,
{
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.send.poll_ready(cx)
    }

    fn poll_finish(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.send.poll_finish(cx)
    }

    fn reset(&mut self, reset_code: u64) {
        self.send.reset(reset_code)
    }

    fn send_data<D: Into<WriteBuf<B>>>(&mut self, data: D) -> Result<(), StreamErrorIncoming> {
        self.send.send_data(data)
    }

    fn send_id(&self) -> quic::StreamId {
        self.send.send_id()
    }
}

impl<B> quic::SendStreamUnframed<B> for BidiStream<B>
where
    B: Buf + Send + 'static,
{
    fn poll_send<D: Buf>(
        &mut self,
        cx: &mut task::Context<'_>,
        buf: &mut D,
    ) -> Poll<Result<usize, StreamErrorIncoming>> {
        self.send.poll_send(cx, buf)
    }
}

impl<B> quic::Is0rtt for BidiStream<B>
where
    B: Buf + Send + 'static,
{
    fn is_0rtt(&self) -> bool {
        self.recv.is_0rtt()
    }
}

/// qmux-backed receive stream
///
/// Implements a [`quic::RecvStream`] backed by a [`qmux::RecvStream`].
pub struct RecvStream {
    stream: Option<qmux::RecvStream>,
    stream_id: quic::StreamId,
    read_chunk_fut: ReadChunkFuture,
    pending_stop: Option<u32>,
}

type ReadChunkFuture =
    ReusableBoxFuture<'static, (qmux::RecvStream, Result<Option<Bytes>, qmux::Error>)>;

impl RecvStream {
    fn new(stream: qmux::RecvStream, stream_id: quic::StreamId) -> Self {
        Self {
            stream: Some(stream),
            stream_id,
            read_chunk_fut: ReusableBoxFuture::new(async { unreachable!() }),
            pending_stop: None,
        }
    }
}

impl quic::RecvStream for RecvStream {
    type Buf = Bytes;

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_data(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, StreamErrorIncoming>> {
        if let Some(mut stream) = self.stream.take() {
            self.read_chunk_fut.set(async move {
                let chunk = stream.read_chunk(usize::MAX).await;
                (stream, chunk)
            })
        };

        let (mut stream, chunk) = ready!(self.read_chunk_fut.poll(cx));
        if let Some(error_code) = self.pending_stop.take() {
            stream.stop(error_code);
        }
        self.stream = Some(stream);
        Poll::Ready(chunk.map_err(convert_stream_error))
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn stop_sending(&mut self, error_code: u64) {
        let error_code = code_to_u32(error_code);
        if let Some(stream) = self.stream.as_mut() {
            stream.stop(error_code);
        } else {
            self.pending_stop = Some(error_code);
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn recv_id(&self) -> quic::StreamId {
        self.stream_id
    }
}

impl quic::Is0rtt for RecvStream {
    /// `qmux` doesn't expose per-stream 0-RTT state (unlike `noq::RecvStream::is_0rtt`),
    /// so this always reports `false` (never treat a stream's data as 0-RTT-replayable) —
    /// the TLS config used to establish the underlying `qmux::Session` should disable
    /// 0-RTT resumption entirely so this is never inaccurate in the unsafe direction.
    fn is_0rtt(&self) -> bool {
        false
    }
}

/// qmux-backed send stream
///
/// Implements a [`quic::SendStream`] backed by a [`qmux::SendStream`].
pub struct SendStream<B: Buf + Send + 'static> {
    stream_id: quic::StreamId,
    state: SendStreamState<B>,
}

enum SendStreamState<B: Buf + Send + 'static> {
    Idle(qmux::SendStream),
    Writing(WriteFuture),
    Finishing(qmux::SendStream),
    /// A write failed, or the stream was reset while a write was in flight:
    /// every later operation reports this error instead of the old
    /// behavior of pretending to succeed (`Ok` from `poll_ready`/
    /// `poll_finish` with nothing actually sent).
    Failed(String),
    /// Transient state only observed re-entrantly inside `poll_ready`/etc.
    Invalid,
    _Marker(std::marker::PhantomData<B>),
}

type WriteFuture = ReusableBoxFuture<'static, (qmux::SendStream, Result<(), qmux::Error>)>;

impl<B> SendStream<B>
where
    B: Buf + Send + 'static,
{
    fn new(stream: qmux::SendStream, stream_id: quic::StreamId) -> SendStream<B> {
        Self { stream_id, state: SendStreamState::Idle(stream) }
    }

    /// Settles a finished write: back to `Idle` on success, `Failed` (with
    /// the error returned now *and* remembered for every later call) on
    /// failure.
    fn complete_write(&mut self, stream: qmux::SendStream, result: Result<(), qmux::Error>) -> Result<(), StreamErrorIncoming> {
        match result {
            Ok(()) => {
                self.state = SendStreamState::Idle(stream);
                Ok(())
            }
            Err(e) => {
                self.state = SendStreamState::Failed(e.to_string());
                Err(convert_stream_error(e))
            }
        }
    }
}

/// The error to report for an operation attempted in a state that can't
/// perform it (`Failed`, or the should-never-be-observed `Invalid`).
fn failed_error<B: Buf + Send + 'static>(state: &SendStreamState<B>) -> StreamErrorIncoming {
    let reason = match state {
        SendStreamState::Failed(reason) => reason.clone(),
        _ => "send stream is in an invalid state".to_string(),
    };
    StreamErrorIncoming::Unknown(Box::new(std::io::Error::other(format!("h3-qmux: {reason}"))))
}

impl<B> quic::SendStream<B> for SendStream<B>
where
    B: Buf + Send + 'static,
{
    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        match std::mem::replace(&mut self.state, SendStreamState::Invalid) {
            SendStreamState::Idle(stream) => {
                self.state = SendStreamState::Idle(stream);
                Poll::Ready(Ok(()))
            }
            SendStreamState::Writing(mut fut) => {
                // Must restore `self.state` to `Writing(fut)` on the Pending
                // path before returning — `ready!` would otherwise return
                // early with `fut` still owned by this match arm's local
                // binding, dropping the in-flight write (and losing any
                // bytes already queued in `stream`'s internal buffer/task)
                // the moment this function returns.
                match fut.poll(cx) {
                    Poll::Ready((stream, result)) => Poll::Ready(self.complete_write(stream, result)),
                    Poll::Pending => {
                        self.state = SendStreamState::Writing(fut);
                        Poll::Pending
                    }
                }
            }
            SendStreamState::Finishing(stream) => {
                self.state = SendStreamState::Finishing(stream);
                Poll::Ready(Ok(()))
            }
            other => {
                let err = failed_error(&other);
                self.state = other;
                Poll::Ready(Err(err))
            }
        }
    }

    /// Finishes (FIN) the stream — after completing any write still in
    /// flight. It used to report `Ok` while a write was pending without ever
    /// sending FIN, and `Ok` after a failed write.
    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_finish(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        match std::mem::replace(&mut self.state, SendStreamState::Invalid) {
            SendStreamState::Idle(mut stream) => {
                let result = stream.finish().map_err(convert_stream_error);
                self.state = SendStreamState::Finishing(stream);
                Poll::Ready(result)
            }
            SendStreamState::Writing(mut fut) => match fut.poll(cx) {
                Poll::Ready((stream, result)) => {
                    if let Err(e) = self.complete_write(stream, result) {
                        return Poll::Ready(Err(e));
                    }
                    self.poll_finish(cx)
                }
                Poll::Pending => {
                    self.state = SendStreamState::Writing(fut);
                    Poll::Pending
                }
            },
            SendStreamState::Finishing(stream) => {
                self.state = SendStreamState::Finishing(stream);
                Poll::Ready(Ok(()))
            }
            other => {
                let err = failed_error(&other);
                self.state = other;
                Poll::Ready(Err(err))
            }
        }
    }

    /// Resets the stream. With a write in flight, the write future (which
    /// owns the `qmux::SendStream`) is dropped, which makes qmux reset the
    /// stream itself — with code `0`, since qmux offers no way to reach a
    /// stream owned by an in-flight write to pass the requested code. It
    /// used to do nothing at all in that state.
    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn reset(&mut self, reset_code: u64) {
        match std::mem::replace(&mut self.state, SendStreamState::Invalid) {
            SendStreamState::Idle(mut stream) | SendStreamState::Finishing(mut stream) => {
                stream.reset(code_to_u32(reset_code));
                self.state = SendStreamState::Idle(stream);
            }
            SendStreamState::Writing(fut) => {
                drop(fut);
                self.state = SendStreamState::Failed(format!("stream reset (code {reset_code}) while a write was in flight"));
            }
            other => self.state = other,
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn send_data<D: Into<WriteBuf<B>>>(&mut self, data: D) -> Result<(), StreamErrorIncoming> {
        let SendStreamState::Idle(mut stream) = std::mem::replace(&mut self.state, SendStreamState::Invalid) else {
            #[cfg(feature = "tracing")]
            tracing::error!("send_data called while send stream is not ready");
            return Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::InternalError(
                    "internal error in the http stack".to_string(),
                ),
            });
        };

        let mut buf: WriteBuf<B> = data.into();
        self.state = SendStreamState::Writing(ReusableBoxFuture::new(async move {
            let mut result = Ok(());
            while buf.has_remaining() {
                match stream.write(buf.chunk()).await {
                    Ok(written) => buf.advance(written),
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            (stream, result)
        }));
        Ok(())
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn send_id(&self) -> quic::StreamId {
        self.stream_id
    }
}

impl<B> quic::SendStreamUnframed<B> for SendStream<B>
where
    B: Buf + Send + 'static,
{
    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_send<D: Buf>(
        &mut self,
        cx: &mut task::Context<'_>,
        buf: &mut D,
    ) -> Poll<Result<usize, StreamErrorIncoming>> {
        match std::mem::replace(&mut self.state, SendStreamState::Invalid) {
            SendStreamState::Idle(mut stream) => {
                let chunk = Bytes::copy_from_slice(buf.chunk());
                let len = chunk.len();
                self.state = SendStreamState::Writing(ReusableBoxFuture::new(async move {
                    let result = stream.write(&chunk).await.map(|_| ());
                    (stream, result)
                }));
                buf.advance(len);
                Poll::Ready(Ok(len))
            }
            SendStreamState::Writing(mut fut) => {
                // Same Pending-path state-loss hazard as `poll_ready` above.
                match fut.poll(cx) {
                    Poll::Ready((stream, result)) => {
                        self.complete_write(stream, result)?;
                        Poll::Ready(Ok(0))
                    }
                    Poll::Pending => {
                        self.state = SendStreamState::Writing(fut);
                        Poll::Pending
                    }
                }
            }
            other => {
                let err = failed_error(&other);
                self.state = other;
                Poll::Ready(Err(err))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_that_do_not_fit_in_u32_map_to_internal_error_instead_of_truncating() {
        assert_eq!(code_to_u32(0x100), 0x100);
        assert_eq!(code_to_u32(u64::from(u32::MAX)), u32::MAX);
        assert_eq!(code_to_u32(u64::from(u32::MAX) + 0x100), Code::H3_INTERNAL_ERROR.value() as u32);
    }
}
