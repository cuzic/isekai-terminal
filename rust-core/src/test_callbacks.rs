//! テスト専用: 複数ファイルの `#[cfg(test)]` モジュールが個別に同じ形の
//! `SessionCallback`/`OrchestratorCallback` テストダブルを再定義していた
//! 重複を解消するための共有置き場。`lib.rs`で`#[cfg(test)]`付きで宣言されて
//! いるため、非テストビルドには一切含まれない。

use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::Notify;

use crate::SessionCallback;

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
