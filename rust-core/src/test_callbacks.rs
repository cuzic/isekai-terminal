//! テスト専用: 複数ファイルの `#[cfg(test)]` モジュールが個別に同じ形の
//! `SessionCallback`/`OrchestratorCallback` テストダブルを再定義していた
//! 重複を解消するための共有置き場。`lib.rs`で`#[cfg(test)]`付きで宣言されて
//! いるため、非テストビルドには一切含まれない。

use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;

use crate::file_preview::FilePreviewOutcome;
use crate::{ConnectionPublicState, ForwardState, OrchestratorCallback, SessionCallback};

/// 受信データを`buf`に溜め、到着のたびに`notify`を起こす`SessionCallback`
/// 実装。`isekai_stun_p2p_transport.rs`と`isekai_pipe_quic_transport.rs`の
/// e2eテストが同一の定義をそれぞれ持っていたため共通化した。
pub(crate) struct BufferingSessionCallback {
    pub(crate) buf: Arc<StdMutex<Vec<u8>>>,
    pub(crate) notify: Arc<Notify>,
}

impl SessionCallback for BufferingSessionCallback {
    fn on_data(&self, data: Vec<u8>) {
        self.buf.lock().unwrap().extend_from_slice(&data);
        self.notify.notify_one();
    }
    fn on_host_key(&self, _fingerprint: String) -> bool { true }
    fn on_connected(&self) {}
    fn on_disconnected(&self, reason: Option<String>) {
        eprintln!("test: disconnected: {reason:?}");
    }
    fn on_screen_update(&self, _update: crate::ScreenUpdate) {}
    fn on_trzsz_request(&self, _t: String, _m: String, _n: Option<String>, _s: Option<u64>) {}
    fn on_trzsz_download_chunk(&self, _t: String, _d: Vec<u8>, _l: bool) {}
    fn on_trzsz_progress(&self, _t: String, _tr: u64, _to: Option<u64>) {}
    fn on_trzsz_finished(&self, _t: String, _s: bool, _m: Option<String>) {}
    fn on_no_viable_path(&self) {}
    fn on_forward_state_changed(&self, _id: String, _state: crate::ForwardState) {}
    fn on_agent_sign_request(&self, _key_fingerprint: String) -> bool { true }
    fn on_clipboard_write(&self, _payload: crate::ClipboardPayload) {}
    fn on_clipboard_pull_request(&self) -> Option<crate::ClipboardPayload> { None }
}

/// `transport/ssh_handler.rs`・`transport/forward.rs`・`orchestrator.rs`が
/// それぞれ独自に定義していた「`OrchestratorCallback`のうち接続状態/データ/
/// forward状態/file-preview結果だけを`mpsc`チャネルへ転送し、残りは全部
/// no-opにする」テストダブルの共有版。3ファイルの版は転送するイベントの
/// 種類(部分集合)が違うだけで、他の実装は完全に同一だった。イベント種類を
/// 全ファイル分の合併(union)にしたぶん、各テストが受け取るイベントは元より
/// 増えるが、既存のポーリングループは全て`match { .. => continue }`の
/// ワイルドカード節を持つため、関心が無いイベント種別が届いても無視される
/// だけで安全。
#[allow(dead_code)]
pub(crate) enum OrchestratorTestEvent {
    Connection(ConnectionPublicState),
    Data(Vec<u8>),
    Forward(String, ForwardState),
    FilePreview(String, FilePreviewOutcome),
}

pub(crate) struct ForwardingOrchestratorCallback {
    pub(crate) tx: UnboundedSender<OrchestratorTestEvent>,
}

impl OrchestratorCallback for ForwardingOrchestratorCallback {
    fn on_connection_state_changed(&self, state: ConnectionPublicState) {
        let _ = self.tx.send(OrchestratorTestEvent::Connection(state));
    }
    fn on_screen_update(&self, _update: crate::ScreenUpdate) {}
    fn on_host_key(&self, _host: String, _port: u16, _fingerprint: String) -> bool { true }
    fn on_data(&self, data: Vec<u8>) {
        let _ = self.tx.send(OrchestratorTestEvent::Data(data));
    }
    fn on_trzsz_state_changed(&self, _state: crate::TrzszPublicState) {}
    fn on_download_complete(&self, _file_name: Option<String>, _data: Vec<u8>) {}
    fn on_no_viable_path(&self) {}
    fn on_forward_state_changed(&self, id: String, state: ForwardState) {
        let _ = self.tx.send(OrchestratorTestEvent::Forward(id, state));
    }
    fn on_agent_sign_request(&self, _key_fingerprint: String) -> bool { true }
    fn on_clipboard_write(&self, _payload: crate::ClipboardPayload) {}
    fn on_clipboard_pull_request(&self) -> Option<crate::ClipboardPayload> { None }
    fn on_request_wifi_fd(&self) -> Option<crate::PlatformFd> { None }
    fn on_request_cellular_fd(&self) -> Option<crate::PlatformFd> { None }
    fn on_rebind_state_changed(&self, _state: crate::rebind_manager::RebindPublicState) {}
    fn on_notify(&self, _kind: crate::NotifyKind) {}
    fn on_prompt_jump(&self, _target: Option<crate::PromptJumpTarget>) {}
    fn on_prompt_output_copy_ready(&self, _text: Option<String>) {}
    fn on_file_preview_result(&self, request_id: String, outcome: FilePreviewOutcome) {
        let _ = self.tx.send(OrchestratorTestEvent::FilePreview(request_id, outcome));
    }
}
