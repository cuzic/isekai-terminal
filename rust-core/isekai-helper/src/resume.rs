//! Phase 8-1/8-3: helper 側 output buffer と reattach（`RESUME`）処理。
//! 契約の詳細は `/HELPER_PROTOCOL.md` §7 を参照。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;

pub const CONTROL_HELLO: u8 = 0x10;
pub const CONTROL_ACK: u8 = 0x11;
pub const APP_ACK: u8 = 0x12;
pub const RESUME: u8 = 0x03;
pub const RESUME_ACK: u8 = 0x13;
pub const REJECT_UNKNOWN_SESSION: u8 = 0xF9;
pub const REJECT_OFFSET_GONE: u8 = 0xF8;

pub type SessionId = [u8; 16];

/// S→C 方向（helper → client）に送出したバイト列を保持するバウンデッドバッファ。
/// `start_offset` は `data` の先頭バイトの絶対オフセット、`end_offset` は
/// 送出済みバイト数の累計（= `HELPER_PROTOCOL.md` の `helper_sent_offset`）。
pub struct OutputBuffer {
    data: VecDeque<u8>,
    start_offset: u64,
    capacity: usize,
}

impl OutputBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(capacity.min(1 << 20)),
            start_offset: 0,
            capacity,
        }
    }

    /// 送出したバイト列をバッファに書き足す。上限を超えた古いバイトは
    /// 即座に破棄する（`advance_start` を待たない — 上限超過は
    /// `REJECT_OFFSET_GONE` を招くが、無制限のメモリ増加よりは安全という判断）。
    pub fn append(&mut self, bytes: &[u8]) {
        self.data.extend(bytes.iter().copied());
        while self.data.len() > self.capacity {
            self.data.pop_front();
            self.start_offset += 1;
        }
    }

    /// 相手（client）が `client_delivered_offset` として確認した位置まで、
    /// 安全に破棄してよいバイトを実際に破棄する。
    pub fn advance_start(&mut self, confirmed_offset: u64) {
        while self.start_offset < confirmed_offset && !self.data.is_empty() {
            self.data.pop_front();
            self.start_offset += 1;
        }
        if confirmed_offset > self.start_offset {
            // データが既に無い（confirmed が end_offset を超えている等）場合は
            // start_offset だけ進める必要はない（end_offset 側で整合性が取れる）。
        }
    }

    #[allow(dead_code)]
    pub fn start_offset(&self) -> u64 {
        self.start_offset
    }

    pub fn end_offset(&self) -> u64 {
        self.start_offset + self.data.len() as u64
    }

    /// `from` 以降、現在の `end_offset` までのバイト列を返す。
    /// `from` が既に破棄済みの範囲（`start_offset` より前）なら `None`
    /// （呼び出し側は `REJECT_OFFSET_GONE` を返すべき）。
    /// `from` が `end_offset` を超える不正な値の場合も `None`。
    pub fn replay_from(&self, from: u64) -> Option<Vec<u8>> {
        if from < self.start_offset || from > self.end_offset() {
            return None;
        }
        let skip = (from - self.start_offset) as usize;
        Some(self.data.iter().skip(skip).copied().collect())
    }
}

/// resume 可能な 1 セッション分の状態。
pub struct Session {
    pub output_buffer: OutputBuffer,
    /// C→S 方向（client → helper → target）で target への書き込みに成功した累計バイト数。
    pub helper_committed_offset: u64,
    /// data stream が切れている間、target への TCP 接続を「park」しておく置き場。
    /// `RESUME` で reattach が成功したら取り出して中継を再開する。`None` の間は
    /// アクティブな data stream が使用中、または TCP 自体が既に切れている
    /// （その場合は session ごと破棄される）。
    pub parked_tcp: Option<(OwnedReadHalf, OwnedWriteHalf)>,
    /// `parked_tcp` に入れられた時刻。resume が一定時間来なければ
    /// `SessionTable::sweep_expired_parked` で破棄する（HELPER_PROTOCOL.md §7.5）。
    pub parked_since: Option<std::time::Instant>,
}

impl Session {
    pub fn new(output_buffer_capacity: usize) -> Self {
        Self {
            output_buffer: OutputBuffer::new(output_buffer_capacity),
            helper_committed_offset: 0,
            parked_tcp: None,
            parked_since: None,
        }
    }
}

/// helper プロセス内でアクティブな resume 可能セッションのテーブル。
/// 各エントリは `Arc<Mutex<Session>>` で保持するため、呼び出し側は一度
/// `get()` した handle を複数の並行タスク（C→S / S→C / control ACK 受信）で
/// 使い回せる（テーブル自体のロックはエントリ取得時だけで済む）。
#[derive(Clone)]
pub struct SessionTable {
    inner: Arc<Mutex<HashMap<SessionId, Arc<Mutex<Session>>>>>,
}

impl SessionTable {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn generate_session_id() -> SessionId {
        use rand::RngCore;
        let mut id = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut id);
        id
    }

    #[allow(dead_code)]
    pub async fn insert(&self, id: SessionId, session: Session) -> Arc<Mutex<Session>> {
        let handle = Arc::new(Mutex::new(session));
        self.inner.lock().await.insert(id, handle.clone());
        handle
    }

    /// 既に中継が使い始めている `Arc<Mutex<Session>>` handle をそのまま登録する。
    /// `relay_with_resume` は control stream の accept を待たずに中継を開始する
    /// ため、`Session` は先に作成済みで、control stream 確立時にこれで登録する。
    pub async fn insert_existing(&self, id: SessionId, handle: Arc<Mutex<Session>>) {
        self.inner.lock().await.insert(id, handle);
    }

    pub async fn get(&self, id: &SessionId) -> Option<Arc<Mutex<Session>>> {
        self.inner.lock().await.get(id).cloned()
    }

    pub async fn remove(&self, id: &SessionId) -> Option<Arc<Mutex<Session>>> {
        self.inner.lock().await.remove(id)
    }

    #[allow(dead_code)]
    pub async fn contains(&self, id: &SessionId) -> bool {
        self.inner.lock().await.contains_key(id)
    }

    /// `parked_tcp` に入れられてから `max_parked` 以上経過したセッションを
    /// 破棄する（TCP 接続を close して session_id をテーブルから除く）。
    /// アクティブなセッション（`parked_tcp` が `None`）には触れない。
    /// HELPER_PROTOCOL.md §7.5「一定時間 resume が来なければ破棄する」の実装。
    pub async fn sweep_expired_parked(&self, max_parked: std::time::Duration) {
        let expired: Vec<SessionId> = {
            let inner = self.inner.lock().await;
            let mut expired = Vec::new();
            for (id, handle) in inner.iter() {
                let session = handle.lock().await;
                if let Some(since) = session.parked_since {
                    if since.elapsed() >= max_parked {
                        expired.push(*id);
                    }
                }
            }
            expired
        };
        for id in expired {
            if let Some(handle) = self.remove(&id).await {
                // parked_tcp を drop することで TCP 接続も close される。
                drop(handle.lock().await.parked_tcp.take());
                log::info!("session {} expired while parked, discarded", id.iter().map(|b| format!("{b:02x}")).collect::<String>());
            }
        }
    }
}

impl Default for SessionTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_replay_full_range() {
        let mut buf = OutputBuffer::new(1024);
        buf.append(b"hello");
        buf.append(b" world");
        assert_eq!(buf.end_offset(), 11);
        assert_eq!(buf.replay_from(0).unwrap(), b"hello world");
        assert_eq!(buf.replay_from(5).unwrap(), b" world");
        assert_eq!(buf.replay_from(11).unwrap(), b"");
    }

    #[test]
    fn replay_from_beyond_end_is_none() {
        let mut buf = OutputBuffer::new(1024);
        buf.append(b"hi");
        assert!(buf.replay_from(3).is_none());
    }

    #[test]
    fn advance_start_discards_confirmed_prefix() {
        let mut buf = OutputBuffer::new(1024);
        buf.append(b"0123456789");
        buf.advance_start(4);
        assert_eq!(buf.start_offset(), 4);
        assert_eq!(buf.replay_from(4).unwrap(), b"456789");
        assert!(buf.replay_from(0).is_none(), "破棄済み範囲は None を返すべき");
    }

    #[test]
    fn capacity_overflow_evicts_oldest_bytes() {
        let mut buf = OutputBuffer::new(4);
        buf.append(b"abcdefgh"); // 8 bytes into a 4-byte buffer
        assert_eq!(buf.start_offset(), 4);
        assert_eq!(buf.end_offset(), 8);
        assert_eq!(buf.replay_from(4).unwrap(), b"efgh");
        assert!(
            buf.replay_from(0).is_none(),
            "capacity 超過で古いバイトは破棄済みのはず"
        );
    }

    #[test]
    fn capacity_overflow_across_multiple_appends() {
        let mut buf = OutputBuffer::new(10);
        for _ in 0..5 {
            buf.append(b"abcd"); // 20 bytes total into a 10-byte buffer
        }
        assert_eq!(buf.end_offset(), 20);
        assert_eq!(buf.start_offset(), 10);
        assert_eq!(buf.replay_from(10).unwrap().len(), 10);
    }

    #[tokio::test]
    async fn session_table_insert_remove_roundtrip() {
        let table = SessionTable::new();
        let id = SessionTable::generate_session_id();
        assert!(!table.contains(&id).await);

        let mut session = Session::new(1024);
        session.output_buffer.append(b"payload");
        session.helper_committed_offset = 42;
        table.insert(id, session).await;

        assert!(table.contains(&id).await);
        let handle_before_remove = table.get(&id).await.expect("session should be gettable");
        assert_eq!(handle_before_remove.lock().await.helper_committed_offset, 42);

        let removed = table.remove(&id).await.expect("session should exist");
        let removed = removed.lock().await;
        assert_eq!(removed.helper_committed_offset, 42);
        assert_eq!(removed.output_buffer.replay_from(0).unwrap(), b"payload");
        drop(removed);
        assert!(!table.contains(&id).await);
    }

    #[test]
    fn generated_session_ids_are_not_all_zero_and_differ() {
        let a = SessionTable::generate_session_id();
        let b = SessionTable::generate_session_id();
        assert_ne!(a, [0u8; 16]);
        assert_ne!(a, b);
    }

    async fn dummy_parked_tcp() -> (OwnedReadHalf, OwnedWriteHalf) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        drop(client);
        server.into_split()
    }

    /// `sweep_expired_parked` は max_parked を超えて park された session だけを
    /// 破棄し、期限内の park や、そもそも park されていない（アクティブな）
    /// session には触れないことを確認する。
    #[tokio::test]
    async fn sweep_expired_parked_removes_only_stale_entries() {
        let table = SessionTable::new();
        let max_parked = std::time::Duration::from_secs(30);

        let expired_id = SessionTable::generate_session_id();
        let mut expired_session = Session::new(1024);
        expired_session.parked_tcp = Some(dummy_parked_tcp().await);
        expired_session.parked_since = Some(std::time::Instant::now() - std::time::Duration::from_secs(60));
        table.insert(expired_id, expired_session).await;

        let fresh_id = SessionTable::generate_session_id();
        let mut fresh_session = Session::new(1024);
        fresh_session.parked_tcp = Some(dummy_parked_tcp().await);
        fresh_session.parked_since = Some(std::time::Instant::now());
        table.insert(fresh_id, fresh_session).await;

        let active_id = SessionTable::generate_session_id();
        // parked_tcp/parked_since が None = 現在アクティブに中継中の session。
        table.insert(active_id, Session::new(1024)).await;

        table.sweep_expired_parked(max_parked).await;

        assert!(!table.contains(&expired_id).await, "期限切れの park は破棄されるはず");
        assert!(table.contains(&fresh_id).await, "期限内の park は残るはず");
        assert!(table.contains(&active_id).await, "アクティブな session には触れないはず");
    }
}
