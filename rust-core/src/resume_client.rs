//! Phase 8-2/8-3: client 側 input replay buffer（C→S 方向の resume 用）と、
//! 実際の reattach（`RESUME` フレーム送受信・裏での再接続）を行う
//! `ReattachableStream`。契約の詳細は `/HELPER_PROTOCOL.md` §7 を参照。
//!
//! `rust-core/isekai-helper/src/resume.rs` の `OutputBuffer` と対になる、
//! 逆方向（C→S）のバッファ。データ構造は同じ設計（バウンデッド・確認済み
//! offset 破棄・指定 offset からの再送）だが、別クレート（`isekai-helper` は
//! 独立したバイナリクレート）のため共有せず重複実装している。
//!
//! Phase 1d(isekai-terminal-core/isekai-transport crate共有化):
//! `ReattachableStream`は生の`noq::SendStream`/`RecvStream`ではなく
//! `quicmux::{AnyByteStreamReadHalf, AnyByteStreamWriteHalf}`の上に
//! 実装されている。これらの`async fn`ベースのメソッドは、poll方式の
//! `AsyncRead`/`AsyncWrite`へ1回のpollごとに橋渡しするのが難しい
//! （呼び出し元が渡す`buf`の生存期間がpollごとに変わるため、future自体を
//! poll間で保持できない）。そのため`tokio::io::duplex`を挟んだ
//! バックグラウンドpumpタスク方式にした: 呼び出し元(russh)が触るのは常に
//! 素通しの`DuplexStream`側であり、reattach・replay・offset管理は全て
//! pumpタスク側の実`.await`ベースのループで行う。pollベースの手書き
//! waker管理が不要になり、read/write両方向の失敗を1つのタスクの
//! `tokio::select!`で自然に直列化できる（同時に2つのreattachが走らない）。

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

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

    /// 本体コードからは呼ばれず、このファイル末尾のテストからのみ使われる。
    #[allow(dead_code)]
    pub(crate) fn start_offset(&self) -> u64 {
        self.start_offset
    }

    pub(crate) fn end_offset(&self) -> u64 {
        self.start_offset + self.data.len() as u64
    }

    /// Phase 8-3（reattach ハンドシェイク）で使用する（`trigger_reattach`参照）。
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

// ── Phase 8-3 / Phase 1d: reattach 対応ストリーム ──────────────────────

/// このモジュールのpump/reattachロジックが必要とする最小限の非同期read/write
/// インタフェース — 具体型に対して実装する(`dyn`化しない)ことで、本番の
/// `quicmux::{AnyByteStreamReadHalf, AnyByteStreamWriteHalf}`とこのモジュール
/// 専用のtest mockの両方が同じジェネリック関数群(`ReattachableStream::new`/
/// `run_pump`/`attempt_reattach`等)を満たせるようにする。`quicmux::AnyByteStream`
/// 系のenumはbackendの種類分しかvariantを持たない設計(そのドキュメント参照)
/// なので、test mock用の3番目のvariantを増やす余地は無い — そのためこの
/// モジュール独自の最小限のシームが必要。
pub(crate) trait ByteHalfRead: Send {
    fn read(&mut self, buf: &mut [u8]) -> impl std::future::Future<Output = Result<usize, String>> + Send;
}

pub(crate) trait ByteHalfWrite: Send {
    fn write_all(&mut self, buf: &[u8]) -> impl std::future::Future<Output = Result<(), String>> + Send;
    fn shutdown(&mut self) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

impl ByteHalfRead for quicmux::AnyByteStreamReadHalf {
    fn read(&mut self, buf: &mut [u8]) -> impl std::future::Future<Output = Result<usize, String>> + Send {
        async move { quicmux::AnyByteStreamReadHalf::read(self, buf).await.map_err(|e| e.to_string()) }
    }
}

impl ByteHalfWrite for quicmux::AnyByteStreamWriteHalf {
    fn write_all(&mut self, buf: &[u8]) -> impl std::future::Future<Output = Result<(), String>> + Send {
        async move { quicmux::AnyByteStreamWriteHalf::write_all(self, buf).await.map_err(|e| e.to_string()) }
    }
    fn shutdown(&mut self) -> impl std::future::Future<Output = Result<(), String>> + Send {
        async move { quicmux::AnyByteStreamWriteHalf::shutdown(self).await.map_err(|e| e.to_string()) }
    }
}

/// reattach（新しい QUIC connection への `RESUME` 送信）が成功した結果。
pub(crate) struct ReattachResult<R, W> {
    pub(crate) read: R,
    pub(crate) write: W,
    /// helper が確認した C→S オフセット。これより前の replay_buffer は破棄してよい。
    pub(crate) helper_committed_offset: u64,
}

/// 1回の reattach 試行を行う関数の型。呼び出し元（`isekai_pipe_quic_transport.rs`等）が
/// noq/rustls の具体的な接続手順を実装し、`ReattachableStream` はこれを
/// 抽象的に呼び出すだけにする（層を分離する）。
pub(crate) type ReattachFn<R, W> = Arc<
    dyn Fn(SessionId, u64, u64) -> Pin<Box<dyn std::future::Future<Output = Result<ReattachResult<R, W>, String>> + Send>>
        + Send
        + Sync,
>;

/// リトライ回数・間隔（固定値。指数バックオフ）。
const REATTACH_MAX_RETRIES: u32 = 5;
const REATTACH_BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// 呼び出し元(russh)とpumpタスクの間を橋渡しするバッファサイズ。
/// reattach中もある程度は素通しに書き込み続けられる猶予として、
/// 単発のSSHパケットが十分収まるサイズにしてある。
const DUPLEX_BUFFER_SIZE: usize = 64 * 1024;
const RECV_CHUNK_SIZE: usize = 16 * 1024;

/// data stream を包み、QUIC connection が失われても（`RESUME` による reattach が
/// 成功する限り）呼び出し元（russh）に I/O エラーを見せない。
///
/// 設計上の要点: russh の `client::connect_stream` は渡された stream 上で
/// SSH プロトコル状態（鍵・MAC シーケンス番号等）を保持し続ける。stream が
/// 一度でも I/O エラーを返すと russh はそのセッションを終了とみなすため、
/// 「同じ SSH セッションを継続する」という Phase 8 の目的を達成するには、
/// **stream オブジェクト自身が背後で新しい QUIC connection に張り替わり、
/// 呼び出し元には何も気づかせない**必要がある。
///
/// 呼び出し元が実際に読み書きするのは `tokio::io::duplex` の片側
/// (`caller_side`) だけで、もう片側 (`pump_side`) をバックグラウンドタスクが
/// 握って `ByteStreamReadHalf`/`WriteHalf` との実データ授受・reattach・
/// replay・offset管理を行う。呼び出し元から見た「エラーを見せない」性質は、
/// duplexの内部バッファによる自然なバックプレッシャー（reattach中は
/// pumpタスクが busy なのでバッファが埋まるまで書き込みが単純にブロックする）
/// と、リトライ上限到達時のみ立つ`terminal_error`フラグの2つで実現する。
pub(crate) struct ReattachableStream {
    duplex: tokio::io::DuplexStream,
    terminal_error: Arc<Mutex<Option<String>>>,
}

impl ReattachableStream {
    /// `R`/`W` はcallerが持つ具体的な read/write half の型(本番では
    /// `quicmux::{AnyByteStreamReadHalf, AnyByteStreamWriteHalf}`)から型推論
    /// されるので、呼び出し側で明示的に書く必要はない — `ReattachableStream`
    /// 自身は非ジェネリックのまま(`R`/`W`は`run_pump`へspawnした時点で型消去
    /// される)なので、この関数の戻り値型は3つの呼び出し元ファイル全てで
    /// 共通の`resume_client::ReattachableStream`のまま変わらない。
    pub(crate) fn new<R: ByteHalfRead + 'static, W: ByteHalfWrite + 'static>(
        read: R,
        write: W,
        resume_state: Arc<Mutex<ClientResumeState>>,
        reattach_fn: ReattachFn<R, W>,
    ) -> Self {
        let (caller_side, pump_side) = tokio::io::duplex(DUPLEX_BUFFER_SIZE);
        let terminal_error = Arc::new(Mutex::new(None));
        tokio::spawn(run_pump(pump_side, read, write, resume_state, reattach_fn, terminal_error.clone()));
        Self { duplex: caller_side, terminal_error }
    }

    fn check_terminal_error(&self) -> Option<io::Error> {
        self.terminal_error.lock().unwrap().clone().map(|msg| io::Error::new(io::ErrorKind::NotConnected, msg))
    }
}

impl AsyncRead for ReattachableStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(err) = this.check_terminal_error() {
            return Poll::Ready(Err(err));
        }
        Pin::new(&mut this.duplex).poll_read(cx, buf)
    }
}

impl AsyncWrite for ReattachableStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Some(err) = this.check_terminal_error() {
            return Poll::Ready(Err(err));
        }
        Pin::new(&mut this.duplex).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(err) = this.check_terminal_error() {
            return Poll::Ready(Err(err));
        }
        Pin::new(&mut this.duplex).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // shutdown 中に既に failed 状態でも、呼び出し元は既にセッションを
        // 終わらせようとしているので成功扱いにする(旧pollベース実装と同じ判断)。
        let this = self.get_mut();
        Pin::new(&mut this.duplex).poll_shutdown(cx)
    }
}

/// `run_pump`が実データ授受のループから抜けて reattach を試みる理由。
enum PumpEvent {
    /// 呼び出し元(duplex)からバイト列を読んだ。helperへ転送する。
    FromCaller(Vec<u8>),
    /// 呼び出し元が書き込み側を閉じた(EOF)。もう送るデータは無い。
    FromCallerClosed,
    /// helperからバイト列を読んだ。呼び出し元(duplex)へ転送する。
    FromHelper(Vec<u8>),
    /// helperが読み込み方向をclean EOFで閉じた。
    FromHelperClosed,
    /// helperとの読み書きいずれかが失敗した。reattachが必要。
    Failed { direction: &'static str, message: String },
}

async fn next_pump_event(
    pump_read: &mut (impl AsyncRead + Unpin),
    read: &mut impl ByteHalfRead,
    send_buf: &mut [u8],
    recv_buf: &mut [u8],
    helper_read_done: bool,
) -> PumpEvent {
    tokio::select! {
        result = pump_read.read(send_buf) => {
            match result {
                Ok(0) => PumpEvent::FromCallerClosed,
                Ok(n) => PumpEvent::FromCaller(send_buf[..n].to_vec()),
                Err(_) => PumpEvent::FromCallerClosed,
            }
        }
        result = read.read(recv_buf), if !helper_read_done => {
            match result {
                Ok(0) => PumpEvent::FromHelperClosed,
                Ok(n) => PumpEvent::FromHelper(recv_buf[..n].to_vec()),
                Err(e) => PumpEvent::Failed { direction: "read", message: e.to_string() },
            }
        }
    }
}

/// `reattach_fn`を呼び、成功したら`helper_committed_offset`より前を
/// `replay_buffer`から破棄し、そこから先を新しいwrite側へ再送する。
/// 再送自体が失敗した場合もreattach全体の失敗として扱い、同じ試行回数の
/// 予算内で`reattach_fn`をもう一度呼び直す(旧pollベース実装の
/// `trigger_reattach`と同じ「reattach+replayを1回の試行として数える」設計)。
async fn attempt_reattach<R: ByteHalfRead, W: ByteHalfWrite>(
    resume_state: &Arc<Mutex<ClientResumeState>>,
    reattach_fn: &ReattachFn<R, W>,
) -> Result<(R, W), String> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let (session_id, client_sent_offset, client_delivered_offset) = {
            let resume = resume_state.lock().unwrap();
            let Some(id) = resume.session_id else {
                return Err("no session_id, resume not supported for this connection".to_string());
            };
            (id, resume.replay_buffer.end_offset(), resume.client_delivered_offset)
        };

        log::info!("reattach: attempt {attempt}/{REATTACH_MAX_RETRIES}");
        let outcome = match reattach_fn(session_id, client_sent_offset, client_delivered_offset).await {
            Ok(ReattachResult { read, mut write, helper_committed_offset }) => {
                let to_replay = {
                    let mut resume = resume_state.lock().unwrap();
                    resume.replay_buffer.advance_start(helper_committed_offset);
                    resume.replay_buffer.replay_from(helper_committed_offset)
                };
                match to_replay {
                    Some(bytes) if !bytes.is_empty() => match write.write_all(&bytes).await {
                        Ok(()) => Ok((read, write)),
                        Err(e) => Err(format!("failed to replay C->S bytes: {e}")),
                    },
                    _ => Ok((read, write)),
                }
            }
            Err(e) => Err(e),
        };

        match outcome {
            Ok(halves) => {
                log::info!("reattach: succeeded on attempt {attempt}");
                return Ok(halves);
            }
            Err(e) => {
                log::warn!("reattach: attempt {attempt} failed: {e}");
                if attempt >= REATTACH_MAX_RETRIES {
                    return Err(e);
                }
                tokio::time::sleep(REATTACH_BASE_DELAY * 2u32.pow(attempt - 1)).await;
            }
        }
    }
}

/// `chunk`をhelperへ書き込む。失敗したら成功するまで(または諦めるまで)
/// `attempt_reattach`を挟みながらリトライする — トップレベルの`write.write_all`
/// が1回失敗しただけで即座にreattachへ移る(壊れたかもしれない同じ接続への
/// 無条件リトライはしない、旧pollベース実装と同じ判断)。
async fn write_with_reattach<R: ByteHalfRead, W: ByteHalfWrite>(
    chunk: Vec<u8>,
    read: &mut R,
    write: &mut W,
    resume_state: &Arc<Mutex<ClientResumeState>>,
    reattach_fn: &ReattachFn<R, W>,
    helper_read_done: &mut bool,
) -> Result<(), String> {
    loop {
        match write.write_all(&chunk).await {
            Ok(()) => {
                resume_state.lock().unwrap().replay_buffer.append(&chunk);
                return Ok(());
            }
            Err(e) => {
                log::warn!("reattach: data stream write failed ({e}), triggering reattach");
                let (new_read, new_write) = attempt_reattach(resume_state, reattach_fn).await?;
                *read = new_read;
                *write = new_write;
                *helper_read_done = false;
                // ループの先頭に戻り、同じchunkを新しい接続へ再送する。
            }
        }
    }
}

/// `ReattachableStream`のバックグラウンドpumpタスク本体。呼び出し元(duplex)と
/// helper(`ByteStreamReadHalf`/`WriteHalf`)の間で双方向にバイトを転送し続け、
/// 片方向が失敗したら`attempt_reattach`でreattachしてから転送を再開する。
/// 両方向を同じタスク内の`tokio::select!`で扱うことで、reattachの起動が
/// 自然に直列化される(2つのreattachが同時に走ることはない)。
async fn run_pump<R: ByteHalfRead, W: ByteHalfWrite>(
    pump_side: tokio::io::DuplexStream,
    mut read: R,
    mut write: W,
    resume_state: Arc<Mutex<ClientResumeState>>,
    reattach_fn: ReattachFn<R, W>,
    terminal_error: Arc<Mutex<Option<String>>>,
) {
    let (mut pump_read, mut pump_write) = tokio::io::split(pump_side);
    let mut send_buf = vec![0u8; RECV_CHUNK_SIZE];
    let mut recv_buf = vec![0u8; RECV_CHUNK_SIZE];
    let mut helper_read_done = false;

    loop {
        match next_pump_event(&mut pump_read, &mut read, &mut send_buf, &mut recv_buf, helper_read_done).await {
            PumpEvent::FromCaller(chunk) => {
                if let Err(final_err) =
                    write_with_reattach(chunk, &mut read, &mut write, &resume_state, &reattach_fn, &mut helper_read_done)
                        .await
                {
                    *terminal_error.lock().unwrap() = Some(final_err);
                    return;
                }
            }
            PumpEvent::FromCallerClosed => return,
            PumpEvent::FromHelper(chunk) => {
                if pump_write.write_all(&chunk).await.is_err() {
                    return; // 呼び出し元(duplex)側が閉じられた。
                }
                resume_state.lock().unwrap().client_delivered_offset += chunk.len() as u64;
            }
            PumpEvent::FromHelperClosed => {
                // helper側がread方向をclean EOFで閉じた。これはエラーではない
                // ので reattach しない。以後この方向は select 対象から外し
                // （`helper_read_done`ガード）、busy-loopを避ける。
                let _ = pump_write.shutdown().await;
                helper_read_done = true;
            }
            PumpEvent::Failed { direction, message } => {
                log::warn!("reattach: data stream {direction} failed ({message}), triggering reattach");
                match attempt_reattach(&resume_state, &reattach_fn).await {
                    Ok((new_read, new_write)) => {
                        read = new_read;
                        write = new_write;
                        helper_read_done = false;
                    }
                    Err(final_err) => {
                        *terminal_error.lock().unwrap() = Some(final_err);
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use tokio::sync::mpsc;

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

    // ── ReattachableStream(mock ByteStream経由) ─────────────────────

    /// テスト専用のchannelベースmock。`rx`から読んだ`Err`はread失敗を、
    /// `fail_write_once`がtrueの間の`write_all`呼び出しは1回だけ失敗を
    /// 模擬する。
    struct MockReadHalf {
        rx: mpsc::UnboundedReceiver<Result<Vec<u8>, String>>,
    }

    impl ByteHalfRead for MockReadHalf {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
            match self.rx.recv().await {
                Some(Ok(bytes)) => {
                    let n = bytes.len().min(buf.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                    Ok(n)
                }
                Some(Err(msg)) => Err(msg),
                None => Ok(0), // channel closed -> EOF
            }
        }
    }

    struct MockWriteHalf {
        tx: mpsc::UnboundedSender<Vec<u8>>,
        fail_write_once: Arc<AtomicBool>,
    }

    impl ByteHalfWrite for MockWriteHalf {
        async fn write_all(&mut self, buf: &[u8]) -> Result<(), String> {
            if self.fail_write_once.swap(false, Ordering::SeqCst) {
                return Err("mock write failure".to_string());
            }
            let _ = self.tx.send(buf.to_vec());
            Ok(())
        }

        async fn shutdown(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    /// (read half, write half, helperへの書き込みを観測するreceiver,
    /// helperからの読み取りを注入するsender, 次のwrite_all呼び出しを
    /// 1回だけ失敗させるフラグ) を組で作る。
    #[allow(clippy::type_complexity)]
    fn mock_pair() -> (
        MockReadHalf,
        MockWriteHalf,
        mpsc::UnboundedReceiver<Vec<u8>>,
        mpsc::UnboundedSender<Result<Vec<u8>, String>>,
        Arc<AtomicBool>,
    ) {
        let (read_tx, read_rx) = mpsc::unbounded_channel::<Result<Vec<u8>, String>>();
        let (write_tx, write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let fail_write_once = Arc::new(AtomicBool::new(false));
        let read = MockReadHalf { rx: read_rx };
        let write = MockWriteHalf { tx: write_tx, fail_write_once: fail_write_once.clone() };
        (read, write, write_rx, read_tx, fail_write_once)
    }

    fn resume_state_with_session() -> Arc<Mutex<ClientResumeState>> {
        Arc::new(Mutex::new(ClientResumeState {
            replay_buffer: ReplayBuffer::new(1 << 20),
            client_delivered_offset: 0,
            session_id: Some([7u8; 16]),
        }))
    }

    #[tokio::test]
    async fn pass_through_read_and_write_both_directions() {
        let (read, write, mut helper_write_rx, helper_read_tx, _fail) = mock_pair();
        let resume_state = resume_state_with_session();
        let reattach_fn: ReattachFn<MockReadHalf, MockWriteHalf> = Arc::new(|_id, _sent, _delivered| {
            Box::pin(async {
                Err::<ReattachResult<MockReadHalf, MockWriteHalf>, String>("reattach should not be called in this test".to_string())
            })
        });
        let mut stream = ReattachableStream::new(read, write, resume_state.clone(), reattach_fn);

        stream.write_all(b"hello helper").await.unwrap();
        let received = helper_write_rx.recv().await.unwrap();
        assert_eq!(received, b"hello helper");

        helper_read_tx.send(Ok(b"hello client".to_vec())).unwrap();
        let mut buf = [0u8; 32];
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello client");

        assert_eq!(resume_state.lock().unwrap().client_delivered_offset, 12);
    }

    #[tokio::test]
    async fn write_failure_triggers_transparent_reattach_and_replays_the_failed_chunk() {
        let (read1, write1, _write_rx1, _read_tx1, fail_write_once) = mock_pair();
        fail_write_once.store(true, Ordering::SeqCst);

        let (read2, write2, mut helper_write_rx2, _read_tx2, _fail2) = mock_pair();
        let reattach_calls = Arc::new(AtomicUsize::new(0));

        let reattach_calls_for_closure = reattach_calls.clone();
        let read2 = Arc::new(Mutex::new(Some(read2)));
        let write2 = Arc::new(Mutex::new(Some(write2)));
        let reattach_fn: ReattachFn<MockReadHalf, MockWriteHalf> = Arc::new(move |_id, _sent, _delivered| {
            reattach_calls_for_closure.fetch_add(1, Ordering::SeqCst);
            let read2 = read2.clone();
            let write2 = write2.clone();
            Box::pin(async move {
                let read = read2.lock().unwrap().take().expect("reattach_fn called more than once in this test");
                let write = write2.lock().unwrap().take().unwrap();
                Ok(ReattachResult { read, write, helper_committed_offset: 0 })
            })
        });

        let resume_state = resume_state_with_session();
        let mut stream = ReattachableStream::new(read1, write1, resume_state, reattach_fn);

        // caller視点ではエラーは一切見えない: write_allは(duplexへのbuffer完了として)
        // 成功し、実際の再送は裏で起きる。
        stream.write_all(b"world").await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_secs(5), helper_write_rx2.recv())
            .await
            .expect("timed out waiting for the replayed chunk on the new connection")
            .expect("channel closed unexpectedly");
        assert_eq!(received, b"world");
        assert_eq!(reattach_calls.load(Ordering::SeqCst), 1);
    }

    /// Phase 8-4: reattach が `REATTACH_MAX_RETRIES` 回すべて失敗し続けた場合、
    /// `ReattachableStream` は呼び出し元(russh)へ実際の `io::Error` を返す。
    /// 指数バックオフの累計待ち時間（1+2+4+8=15秒）は仮想時間で進める。
    #[tokio::test(start_paused = true)]
    async fn reattach_gives_up_after_max_retries_and_surfaces_error() {
        let (read, write, _write_rx, read_tx, _fail) = mock_pair();
        let reattach_fn: ReattachFn<MockReadHalf, MockWriteHalf> =
            Arc::new(|_id, _sent, _delivered| Box::pin(async { Err("mock: helper unreachable".to_string()) }));
        let resume_state = resume_state_with_session();
        let mut stream = ReattachableStream::new(read, write, resume_state, reattach_fn);

        // helper側の読み取りを失敗させ、reattachループを起動する。
        read_tx.send(Err("mock: connection lost".to_string())).unwrap();

        // バックオフの累計(15秒)を確実に超えるまで、少しずつ仮想時間を進めながら
        // 都度スケジューラに制御を返す（一度に大きく進めるとタイマーの発火順が
        // 保証されないため）。
        for _ in 0..20 {
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }

        let mut buf = [0u8; 1];
        let err = stream
            .read(&mut buf)
            .await
            .expect_err("5回リトライを使い切ったら read は実エラーを返すはず");
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        assert!(err.to_string().contains("mock: helper unreachable"));
    }
}
