//! Phase 8-1/8-3: helper 側 output buffer と reattach（`RESUME`）処理。
//! 契約の詳細は `/HELPER_PROTOCOL.md` §7 を参照。

use std::sync::Arc;

use quicmux::ReplayBuffer;
use tokio::sync::Notify;

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
/// `start_offset` は先頭バイトの絶対オフセット、`end_offset` は送出済みバイト数の
/// 累計（= `archive/HELPER_PROTOCOL.md` の `helper_sent_offset`）。
///
/// 実体は[`quicmux::ReplayBuffer`]。以前はこのファイル(`OutputBuffer`)と
/// `resume_loop.rs`(`C2hReplayBuffer`)とquicmuxに、ほぼ同一の
/// `VecDeque<u8>`+`start_offset`+`capacity`実装が3つ並存していた。しかも
/// `advance_start`の範囲外挙動だけが静かに食い違っており、片方
/// (`C2hReplayBuffer`のjump-ahead)はこのサーバー側の使い方では
/// offsetを壊す(`quicmux::ReplayBuffer::advance_start`のdocs参照 —
/// このサーバーはack読み取りタスクと中継ループが別タスクで同じsession lockを
/// 奪い合うため、「peerへ送出済みだがappend前」の窓が実際に開く)。
/// 正しい方(clamp)へ一本化した。
pub type OutputBuffer = ReplayBuffer;

/// resume 可能な 1 セッション分の、中継ホットパスが触るデータ(出力バッファ等)。
///
/// docs/adr/0019-functional-core-effects.md Step 2a 以降、park状態(`parked_tcp`/`parked_since`)・
/// 交渉済みgrace・`preempt`/`reparked`の`Notify`はここには無い: park/unpark/破棄の判断は
/// 純粋reducer(`serve_fsm::ServeAggregate`)が、ソケットと`Notify`はそれと同じロックの
/// 下にある`attach_runtime::SessionIo`が持つ。このstructは`SessionIo`から
/// `Arc<Mutex<Session>>`として参照され、ロック自体は集約ロックとは別(ネストしない)。
pub struct Session {
    pub output_buffer: OutputBuffer,
    /// C→S 方向（client → helper → target）で target への書き込みに成功した累計バイト数。
    pub helper_committed_offset: u64,
    /// S→C output buffer に空きが戻ったことを relay loop へ伝える通知。
    pub output_space_available: Arc<Notify>,
}

impl Session {
    pub fn new(output_buffer_capacity: usize) -> Self {
        Self {
            output_buffer: OutputBuffer::new(output_buffer_capacity),
            helper_committed_offset: 0,
            output_space_available: Arc::new(Notify::new()),
        }
    }
}

// `SessionTable`(session_id → `Arc<Mutex<Session>>`の独立したロック付きテーブル)は
// Step 2aで撤去した。容量ベースのLRU立ち退き(`insert_existing`)・admission用の
// 立ち退き(`claim_oldest_parked`)・park期限切れの掃除(`sweep_expired_parked`)は
// `serve_fsm::ServeAggregate`の`Activated`/`EvictOldestParked`/`Sweep`遷移になり、
// どれもfencing slotの解放と同じapplyで行われる(呼び出し元が`release_slot_for`を
// 覚えておく必要が無くなった、`.claude/rules/always-connects.md`)。旧テーブルの
// 単体テストは`serve_fsm.rs`へMillisベースで移植してある。
