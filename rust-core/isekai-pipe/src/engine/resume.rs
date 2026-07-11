//! Phase 8-1/8-3: helper 側 output buffer と reattach（`RESUME`）処理。
//! 契約の詳細は `/HELPER_PROTOCOL.md` §7 を参照。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, Notify};

pub const CONTROL_HELLO: u8 = 0x10;
pub const CONTROL_ACK: u8 = 0x11;
pub const APP_ACK: u8 = 0x12;
// `RESUME`/`RESUME_ACK`/`REJECT_UNKNOWN_SESSION`/`REJECT_OFFSET_GONE` used
// to live here as isekai's own hand-rolled resume frame markers — replaced
// by `quicmux::resume`'s `FRAME_RESUME`/`FRAME_RESUME_ACK`/`ResumeRejectReason`
// (quicmux-server-resume Stage B). `CONTROL_HELLO`/`CONTROL_ACK`/`APP_ACK`
// remain: that control-stream sub-protocol is isekai's own and stays out of
// `quicmux::resume`'s scope (see that module's docs).

pub type SessionId = [u8; 16];

/// S→C 方向（helper → client）に送出したバイト列を保持するバウンデッドバッファ。
/// `start_offset` は `data` の先頭バイトの絶対オフセット、`end_offset` は
/// 送出済みバイト数の累計（= `archive/HELPER_PROTOCOL.md` の `helper_sent_offset`）。
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

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[allow(dead_code)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn remaining_capacity(&self) -> usize {
        self.capacity.saturating_sub(self.data.len())
    }

    pub fn is_full(&self) -> bool {
        self.remaining_capacity() == 0
    }

    /// 送出したバイト列をバッファに書き足す。上限を超える場合は何も書かず
    /// `false` を返す。呼び出し側は `remaining_capacity()` 以下だけ読み込む
    /// ことで、古い未確認データを失わず TCP backpressure をかける。
    pub fn append(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() > self.remaining_capacity() {
            return false;
        }
        self.data.extend(bytes.iter().copied());
        true
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

/// [`SessionTable::insert_existing`]'s result — see that method's docs for
/// why `InsertedAfterEvicting` carries the evicted `SessionId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    InsertedAfterEvicting(SessionId),
    Rejected,
}

impl InsertOutcome {
    #[allow(dead_code)]
    pub fn inserted(&self) -> bool {
        !matches!(self, InsertOutcome::Rejected)
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
    /// S→C output buffer に空きが戻ったことを relay loop へ伝える通知。
    pub output_space_available: Arc<Notify>,
}

impl Session {
    pub fn new(output_buffer_capacity: usize) -> Self {
        Self {
            output_buffer: OutputBuffer::new(output_buffer_capacity),
            helper_committed_offset: 0,
            parked_tcp: None,
            parked_since: None,
            output_space_available: Arc::new(Notify::new()),
        }
    }
}

/// 16 byte の `SessionId` を小文字16進文字列にする（ログ表示用）。
fn hex_lower(id: &SessionId) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// helper プロセス内でアクティブな resume 可能セッションのテーブル。
/// 各エントリは `Arc<Mutex<Session>>` で保持するため、呼び出し側は一度
/// `get()` した handle を複数の並行タスク（C→S / S→C / control ACK 受信）で
/// 使い回せる（テーブル自体のロックはエントリ取得時だけで済む）。
#[derive(Clone)]
pub struct SessionTable {
    inner: Arc<Mutex<HashMap<SessionId, Arc<Mutex<Session>>>>>,
    /// 同時に保持できるセッション数の上限（Phase S-4b）。悪意/異常な挙動の
    /// クライアントが `resume_window` 以内に大量の新規HELLOを送り続けても、
    /// `sweep_expired_parked` が効くまでの間にテーブルサイズ（≒メモリ使用量）が
    /// 無制限に増えないようにする DoS/リソース枯渇対策。
    max_sessions: usize,
}

impl SessionTable {
    /// 上限無し（実質 `usize::MAX`）のテーブルを作る。既存テスト・
    /// 上限を意識しない呼び出し元向けの後方互換コンストラクタ。
    /// 本番の起動経路（`main.rs`）は `with_max_sessions` を使うこと。
    pub fn new() -> Self {
        Self::with_max_sessions(usize::MAX)
    }

    pub fn with_max_sessions(max_sessions: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            max_sessions,
        }
    }

    /// production側では#18-4以降未使用(session_idはクライアントが
    /// `ATTACH_HELLO`で決める)——この crate 内のテストが任意のsession_idを
    /// 作るためだけに使う。
    #[allow(dead_code)]
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
    ///
    /// テーブルサイズが既に `max_sessions` に達している場合:
    /// - **parked**（`parked_tcp` が `Some`、つまり data stream が切れて resume 待ち）な
    ///   セッションのうち `parked_since` が最も古いものを1つ立ち退かせてから登録する。
    /// - 立ち退かせられるセッションが1つも無い（全セッションがアクティブ、
    ///   `parked_tcp` が `None`）場合は、新規登録自体を拒否する。アクティブな
    ///   セッションは進行中の中継そのものなので、決して立ち退き対象にしない。
    ///
    /// 戻り値は [`InsertOutcome`] — 立ち退きが発生した場合はどの `SessionId` を
    /// 立ち退かせたかを運ぶ。`sweep_expired_parked`のdocsと同じ理由で、この
    /// `SessionTable`単体では立ち退かせたsessionの`AttachArbiter`側fencing
    /// slotには触れられない(この型はそちらを知らない)ため、呼び出し元が
    /// `InsertOutcome::InsertedAfterEvicting`を見て`AttachRuntime::
    /// established_lease_for` → `relay_ended`を呼び、slotを解放する責務を持つ。
    /// これを怠ると、立ち退かせたsessionの`AttachArbiter`側の`Established`
    /// slotだけが残り続け、以後そのターゲットへの新規ATTACHが誰も使っていない
    /// セッションのせいで`BUSY_OTHER_SESSION`のまま永久に拒否される
    /// (`isekai-pipe serve`プロセスを再起動するまで回復しない) —
    /// `sweep_expired_parked`が実際に一度この不具合を起こしていたのと
    /// 全く同じ形。
    pub async fn insert_existing(&self, id: SessionId, handle: Arc<Mutex<Session>>) -> InsertOutcome {
        let mut inner = self.inner.lock().await;
        let mut evicted_id = None;
        if inner.len() >= self.max_sessions && !inner.contains_key(&id) {
            let mut oldest_parked: Option<(SessionId, std::time::Instant)> = None;
            for (candidate_id, candidate_handle) in inner.iter() {
                let candidate = candidate_handle.lock().await;
                if let Some(since) = candidate.parked_since {
                    let is_older = oldest_parked
                        .as_ref()
                        .map(|(_, oldest_since)| since < *oldest_since)
                        .unwrap_or(true);
                    if is_older {
                        oldest_parked = Some((*candidate_id, since));
                    }
                }
            }
            match oldest_parked {
                Some((evict_id, _)) => {
                    if let Some(evicted) = inner.remove(&evict_id) {
                        // parked_tcp を drop することで TCP 接続も close される。
                        drop(evicted.lock().await.parked_tcp.take());
                        log::warn!(
                            "session table full (max_sessions={}), evicted oldest parked session {} to make room for {}",
                            self.max_sessions,
                            hex_lower(&evict_id),
                            hex_lower(&id)
                        );
                        evicted_id = Some(evict_id);
                    }
                }
                None => {
                    log::warn!(
                        "session table full (max_sessions={}) and no parked session to evict (all sessions active), rejecting new session {}",
                        self.max_sessions,
                        hex_lower(&id)
                    );
                    return InsertOutcome::Rejected;
                }
            }
        }
        inner.insert(id, handle);
        match evicted_id {
            Some(evict_id) => InsertOutcome::InsertedAfterEvicting(evict_id),
            None => InsertOutcome::Inserted,
        }
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
    ///
    /// 破棄した `SessionId` を返す — この `SessionTable` だけでは
    /// `AttachArbiter`(`engine/attach_arbiter.rs`)の fencing slot には触れられない
    /// (この型はそちらを知らない、`engine/mod.rs`の呼び出し元だけが両方を持つ)ため、
    /// 呼び出し元がこの戻り値で `AttachRuntime::established_lease_for` →
    /// `relay_ended` を呼び、slot を解放する責務を持つ。これを怠ると、park
    /// 期限切れで `SessionTable` からは消えた session の `AttachArbiter` 側の
    /// `Established` slot だけが残り続け、以後そのターゲットへの新規ATTACHが
    /// 実際には誰も使っていないセッションのせいで`BUSY_OTHER_SESSION`のまま
    /// 永久に拒否される(`isekai-pipe serve`プロセスを再起動するまで回復しない)。
    pub async fn sweep_expired_parked(&self, max_parked: std::time::Duration) -> Vec<SessionId> {
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
        let mut discarded = Vec::with_capacity(expired.len());
        for id in expired {
            if let Some(handle) = self.remove(&id).await {
                // parked_tcp を drop することで TCP 接続も close される。
                drop(handle.lock().await.parked_tcp.take());
                log::info!("session {} expired while parked, discarded", hex_lower(&id));
                discarded.push(id);
            }
        }
        discarded
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
        assert!(buf.append(b"hello"));
        assert!(buf.append(b" world"));
        assert_eq!(buf.end_offset(), 11);
        assert_eq!(buf.replay_from(0).unwrap(), b"hello world");
        assert_eq!(buf.replay_from(5).unwrap(), b" world");
        assert_eq!(buf.replay_from(11).unwrap(), b"");
    }

    #[test]
    fn replay_from_beyond_end_is_none() {
        let mut buf = OutputBuffer::new(1024);
        assert!(buf.append(b"hi"));
        assert!(buf.replay_from(3).is_none());
    }

    #[test]
    fn advance_start_discards_confirmed_prefix() {
        let mut buf = OutputBuffer::new(1024);
        assert!(buf.append(b"0123456789"));
        buf.advance_start(4);
        assert_eq!(buf.start_offset(), 4);
        assert_eq!(buf.replay_from(4).unwrap(), b"456789");
        assert!(
            buf.replay_from(0).is_none(),
            "破棄済み範囲は None を返すべき"
        );
    }

    #[test]
    fn capacity_overflow_is_rejected_without_evicting_oldest_bytes() {
        let mut buf = OutputBuffer::new(4);
        assert!(buf.append(b"abcd"));
        assert!(!buf.append(b"e"));
        assert_eq!(buf.start_offset(), 0);
        assert_eq!(buf.end_offset(), 4);
        assert_eq!(buf.len(), 4);
        assert_eq!(buf.capacity(), 4);
        assert!(buf.is_full());
        assert_eq!(buf.remaining_capacity(), 0);
        assert_eq!(buf.replay_from(0).unwrap(), b"abcd");
    }

    #[test]
    fn advance_start_frees_capacity_for_later_appends() {
        let mut buf = OutputBuffer::new(10);
        assert!(buf.append(b"abcdefghij"));
        assert!(buf.is_full());
        buf.advance_start(6);
        assert_eq!(buf.start_offset(), 6);
        assert_eq!(buf.remaining_capacity(), 6);
        assert!(buf.append(b"klmnop"));
        assert_eq!(buf.end_offset(), 16);
        assert_eq!(buf.replay_from(6).unwrap(), b"ghijklmnop");
    }

    #[tokio::test]
    async fn session_table_insert_remove_roundtrip() {
        let table = SessionTable::new();
        let id = SessionTable::generate_session_id();
        assert!(!table.contains(&id).await);

        let mut session = Session::new(1024);
        assert!(session.output_buffer.append(b"payload"));
        session.helper_committed_offset = 42;
        table.insert(id, session).await;

        assert!(table.contains(&id).await);
        let handle_before_remove = table.get(&id).await.expect("session should be gettable");
        assert_eq!(
            handle_before_remove.lock().await.helper_committed_offset,
            42
        );

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
        expired_session.parked_since =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(60));
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

        assert!(
            !table.contains(&expired_id).await,
            "期限切れの park は破棄されるはず"
        );
        assert!(table.contains(&fresh_id).await, "期限内の park は残るはず");
        assert!(
            table.contains(&active_id).await,
            "アクティブな session には触れないはず"
        );
    }

    /// `max_sessions` に達した状態で新規登録すると、最も古い parked セッションが
    /// 1つ立ち退き、新規セッションが登録できることを確認する（Phase S-4b）。
    #[tokio::test]
    async fn insert_existing_evicts_oldest_parked_when_full() {
        let table = SessionTable::with_max_sessions(2);

        let older_id = SessionTable::generate_session_id();
        let mut older_session = Session::new(1024);
        older_session.parked_tcp = Some(dummy_parked_tcp().await);
        older_session.parked_since =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(10));
        table
            .insert_existing(older_id, Arc::new(Mutex::new(older_session)))
            .await;

        let newer_id = SessionTable::generate_session_id();
        let mut newer_session = Session::new(1024);
        newer_session.parked_tcp = Some(dummy_parked_tcp().await);
        newer_session.parked_since = Some(std::time::Instant::now());
        table
            .insert_existing(newer_id, Arc::new(Mutex::new(newer_session)))
            .await;

        // テーブルは既に上限(2)に達している。3つ目の登録は最も古い parked
        // セッション(older_id)を立ち退かせた上で成功するはず。
        let new_id = SessionTable::generate_session_id();
        let outcome = table
            .insert_existing(new_id, Arc::new(Mutex::new(Session::new(1024))))
            .await;

        assert_eq!(
            outcome,
            InsertOutcome::InsertedAfterEvicting(older_id),
            "立ち退き後に新規セッションは登録でき、立ち退かせたsession_idを呼び出し元へ返すはず \
             (呼び出し元がそのAttachArbiter leaseを解放できるように)"
        );
        assert!(
            !table.contains(&older_id).await,
            "最も古い parked セッションは立ち退くはず"
        );
        assert!(
            table.contains(&newer_id).await,
            "より新しい parked セッションは残るはず"
        );
        assert!(
            table.contains(&new_id).await,
            "新規セッションは登録されるはず"
        );
    }

    /// アクティブなセッション（`parked_tcp` が `None`）は決して立ち退き対象に
    /// ならないことを確認する。立ち退けるセッションが1つも無ければ、新規登録
    /// 自体を拒否する（Phase S-4b の設計判断）。
    #[tokio::test]
    async fn insert_existing_never_evicts_active_sessions_and_rejects_when_full() {
        let table = SessionTable::with_max_sessions(1);

        let active_id = SessionTable::generate_session_id();
        // parked_tcp/parked_since が None = 現在アクティブに中継中の session。
        table
            .insert_existing(active_id, Arc::new(Mutex::new(Session::new(1024))))
            .await;

        let new_id = SessionTable::generate_session_id();
        let outcome = table
            .insert_existing(new_id, Arc::new(Mutex::new(Session::new(1024))))
            .await;

        assert_eq!(
            outcome,
            InsertOutcome::Rejected,
            "立ち退けるparkedセッションが無ければ新規登録は拒否されるはず"
        );
        assert!(
            table.contains(&active_id).await,
            "アクティブなセッションは立ち退き対象にならないはず"
        );
        assert!(
            !table.contains(&new_id).await,
            "拒否された新規セッションはテーブルに存在しないはず"
        );
    }

    /// テーブルにアクティブなセッションと parked セッションが混在している場合、
    /// 立ち退き対象は必ず parked の方であり、アクティブな方には触れないことを確認する。
    #[tokio::test]
    async fn insert_existing_evicts_parked_not_active_when_mixed() {
        let table = SessionTable::with_max_sessions(2);

        let active_id = SessionTable::generate_session_id();
        table
            .insert_existing(active_id, Arc::new(Mutex::new(Session::new(1024))))
            .await;

        let parked_id = SessionTable::generate_session_id();
        let mut parked_session = Session::new(1024);
        parked_session.parked_tcp = Some(dummy_parked_tcp().await);
        parked_session.parked_since = Some(std::time::Instant::now());
        table
            .insert_existing(parked_id, Arc::new(Mutex::new(parked_session)))
            .await;

        let new_id = SessionTable::generate_session_id();
        let outcome = table
            .insert_existing(new_id, Arc::new(Mutex::new(Session::new(1024))))
            .await;

        assert_eq!(outcome, InsertOutcome::InsertedAfterEvicting(parked_id));
        assert!(
            table.contains(&active_id).await,
            "アクティブなセッションは残るはず"
        );
        assert!(
            !table.contains(&parked_id).await,
            "parked セッションが立ち退くはず"
        );
        assert!(table.contains(&new_id).await);
    }
}
