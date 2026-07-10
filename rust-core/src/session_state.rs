use vte::Parser;
use timed_fsm::{TimedStateMachine, TimerCommand, Response};
use crate::terminal::{Terminal, TermCell};
use crate::theme::Theme;
use crate::trzsz::{TrzszTransferFsm, TrzszEffect, TrzszEvent, TrzszMode, TrzszTimer};

// ── 出力型 ───────────────────────────────────────────────

/// async 層が実行すべき副作用。コールバック・チャネル・タイマーは含まない。
pub(crate) enum SideEffect {
    SendStdin(Vec<u8>),
    TrzszRequest {
        transfer_id: String,
        mode: TrzszMode,
        suggested_name: Option<String>,
        expected_size: Option<u64>,
    },
    DownloadChunk { transfer_id: String, data: Vec<u8>, is_last: bool },
    Progress { transfer_id: String, transferred: u64, total: Option<u64> },
    Finished { transfer_id: String, success: bool, message: Option<String> },
}

pub(crate) struct ProcessResult {
    pub(crate) timer_cmds: Vec<TimerCommand<TrzszTimer>>,
    pub(crate) side_effects: Vec<SideEffect>,
    /// Terminal からスクロールアウトした行（async 層が shared Arc に書き込む）
    pub(crate) pending_rows: Vec<Vec<TermCell>>,
    pub(crate) screen_dirty: bool,
    /// このバッチでリモートが OSC 52 クリップボード書き込みを要求していれば、その
    /// (デコード済み)テキスト(`ISEKAI_PIPE_DESIGN.md` §8 Epic M)。
    pub(crate) pending_clipboard_write: Option<String>,
    /// このバッチでリモートが OSC 52 query(クリップボード読み出し)を要求したか。
    /// 実際の応答送出には非同期のKotlin往復が要るため、ここではフラグを立てるだけで
    /// `session.rs`のevent loopが処理する(`dispatch_result`は同期関数のまま)。
    pub(crate) clipboard_pull_requested: bool,
}

// ── SessionState ─────────────────────────────────────────

/// 同期的なセッション状態機械。
/// チャネル・コールバック・Tokio に一切依存せず、単体テストから直接呼べる。
pub(crate) struct SessionState {
    terminal: Terminal,
    parser: Parser,
    fsm: TrzszTransferFsm,
}

impl SessionState {
    pub(crate) fn new(cols: usize, rows: usize, theme: Theme) -> Self {
        SessionState {
            terminal: Terminal::new(cols, rows, theme),
            parser: Parser::new(),
            fsm: TrzszTransferFsm::new(),
        }
    }

    pub(crate) fn terminal(&self) -> &Terminal { &self.terminal }

    /// このセッションのテーマを差し替える。以降にパースされるSGRの色解決にのみ反映される。
    pub(crate) fn set_theme(&mut self, theme: Theme) {
        self.terminal.set_theme(theme);
    }

    /// tmux 迂回 control-plane(`ISEKAI_PIPE_DESIGN.md` §8 Epic M)経由でリモートから
    /// 届いたタイトルを、OSC 0/2 のパースを経由せず直接反映する。次の`ScreenUpdate`に
    /// 乗せて`onScreenUpdate`まで届くよう`screen_dirty`を立てる。
    pub(crate) fn set_title_from_ctl(&mut self, title: String) -> ProcessResult {
        self.terminal.set_title(title);
        ProcessResult {
            timer_cmds: Vec::new(),
            side_effects: Vec::new(),
            pending_rows: Vec::new(),
            screen_dirty: true,
            pending_clipboard_write: None,
            clipboard_pull_requested: false,
        }
    }

    /// リサイズ時にターミナル・パーサーをリセットする。現在のテーマは引き継ぐ
    /// (リサイズのついでにテーマがグローバル既定へ戻ってしまわないようにする)。
    pub(crate) fn reset_for_resize(&mut self, cols: usize, rows: usize) {
        let theme = self.terminal.theme();
        self.terminal.take_scrollback();  // 旧サイズの pending 行を破棄
        self.terminal = Terminal::new(cols, rows, theme);
        self.parser = Parser::new();
    }

    pub(crate) fn on_stdout(&mut self, bytes: Vec<u8>) -> ProcessResult {
        let resp = self.fsm.on_event(TrzszEvent::StdoutBytes(bytes));
        self.apply(resp)
    }

    pub(crate) fn on_timeout(&mut self, id: TrzszTimer) -> ProcessResult {
        let resp = self.fsm.on_timeout(id);
        self.apply(resp)
    }

    pub(crate) fn on_kotlin_accept_upload(
        &mut self, transfer_id: String, file_name: String, file_size: u64, mode: u32,
    ) -> ProcessResult {
        let resp = self.fsm.on_event(TrzszEvent::KotlinAcceptUpload { transfer_id, file_name, file_size, mode });
        self.apply(resp)
    }

    pub(crate) fn on_kotlin_chunk(
        &mut self, transfer_id: String, data: Vec<u8>, is_last: bool,
    ) -> ProcessResult {
        let resp = self.fsm.on_event(TrzszEvent::KotlinChunk { transfer_id, data, is_last });
        self.apply(resp)
    }

    pub(crate) fn on_kotlin_accept_download(&mut self, transfer_id: String) -> ProcessResult {
        let resp = self.fsm.on_event(TrzszEvent::KotlinAcceptDownload { transfer_id });
        self.apply(resp)
    }

    pub(crate) fn on_kotlin_cancel(&mut self, transfer_id: String) -> ProcessResult {
        let resp = self.fsm.on_event(TrzszEvent::KotlinCancel { transfer_id });
        self.apply(resp)
    }

    fn apply(&mut self, resp: Response<TrzszEffect, TrzszTimer>) -> ProcessResult {
        let timer_cmds = resp.timers;
        let mut side_effects = Vec::new();
        let mut screen_dirty = false;

        for effect in resp.actions {
            match effect {
                TrzszEffect::FlushVte(bytes) => {
                    for byte in &bytes { self.parser.advance(&mut self.terminal, *byte); }
                    screen_dirty = true;
                }
                TrzszEffect::SendStdin(bytes) => {
                    side_effects.push(SideEffect::SendStdin(bytes));
                }
                TrzszEffect::OnTrzszRequest { transfer_id, mode, suggested_name, expected_size } => {
                    side_effects.push(SideEffect::TrzszRequest { transfer_id, mode, suggested_name, expected_size });
                }
                TrzszEffect::OnDownloadChunk { transfer_id, data, is_last } => {
                    side_effects.push(SideEffect::DownloadChunk { transfer_id, data, is_last });
                }
                TrzszEffect::OnProgress { transfer_id, transferred, total } => {
                    side_effects.push(SideEffect::Progress { transfer_id, transferred, total });
                }
                TrzszEffect::OnFinished { transfer_id, success, message } => {
                    side_effects.push(SideEffect::Finished { transfer_id, success, message });
                }
            }
        }

        let pending_rows = self.terminal.take_scrollback();
        let pending_clipboard_write = self.terminal.take_pending_clipboard_write();
        let clipboard_pull_requested = self.terminal.take_pending_clipboard_pull_request();
        ProcessResult {
            timer_cmds,
            side_effects,
            pending_rows,
            screen_dirty,
            pending_clipboard_write,
            clipboard_pull_requested,
        }
    }
}

// ── Tests ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_ascii_passthrough_to_vte() {
        let mut state = SessionState::new(80, 24, Theme::default());
        let r = state.on_stdout(b"hello".to_vec());
        assert!(r.screen_dirty);
        assert!(r.side_effects.is_empty());
        assert!(r.pending_rows.is_empty());
        assert_eq!(state.terminal().screen_cells()[0].ch.as_str(), "h");
        assert_eq!(state.terminal().cursor_col(), 5);
    }

    #[test]
    fn test_pending_rows_returned_on_scroll() {
        let mut state = SessionState::new(10, 3, Theme::default());
        let mut all_rows: Vec<Vec<TermCell>> = Vec::new();
        for i in 0..5u8 {
            let r = state.on_stdout(format!("line{}\r\n", i).into_bytes());
            all_rows.extend(r.pending_rows);
        }
        // 5 行を 3 行端末に流すと \n が scroll_bottom で 3 回発火 → 3 行スクロールアウト
        assert_eq!(all_rows.len(), 3);
    }

    #[test]
    fn set_title_from_ctl_reflects_in_terminal_title_and_marks_screen_dirty() {
        let mut state = SessionState::new(80, 24, Theme::default());
        assert_eq!(state.terminal().title(), None);

        let r = state.set_title_from_ctl("remote title".to_string());

        assert_eq!(state.terminal().title(), Some("remote title"));
        assert!(r.screen_dirty);
        assert!(r.side_effects.is_empty());
        assert!(r.timer_cmds.is_empty());
        assert!(r.pending_rows.is_empty());
    }

    #[test]
    fn test_timer_cmds_forwarded() {
        // タイマー命令が ProcessResult に含まれることを確認
        let mut state = SessionState::new(80, 24, Theme::default());
        let r = state.on_stdout(b"normal text".to_vec());
        // 通常テキストはタイマー命令を生まない
        assert!(r.timer_cmds.is_empty());
    }

    #[test]
    fn test_resize_clears_terminal() {
        let mut state = SessionState::new(80, 24, Theme::default());
        let _ = state.on_stdout(b"hello".to_vec());
        assert_eq!(state.terminal().screen_cells()[0].ch.as_str(), "h");
        state.reset_for_resize(40, 12);
        assert_eq!(state.terminal().cols(), 40);
        assert_eq!(state.terminal().rows(), 12);
        assert_eq!(state.terminal().screen_cells()[0].ch.as_str(), " ");
    }

    // ── Proptest: 不変量 ────────────────────────────────

    proptest! {
        /// 任意 stdout でパニックしない・ターミナル不変量が保たれる
        #[test]
        fn prop_no_panic_and_invariants(
            bytes in proptest::collection::vec(any::<u8>(), 0..512)
        ) {
            let mut state = SessionState::new(80, 24, Theme::default());
            let r = state.on_stdout(bytes);
            // screen_cells の長さは常に cols × rows
            let t = state.terminal();
            prop_assert_eq!(t.screen_cells().len(), t.cols() * t.rows());
            // カーソルは常に画面内
            prop_assert!(t.cursor_row() < t.rows());
            prop_assert!(t.cursor_col() <= t.cols());
            // pending_rows の各行幅は cols と一致
            for row in &r.pending_rows {
                prop_assert_eq!(row.len(), t.cols());
            }
        }

        /// 複数ラウンドの stdout でも不変量が崩れない
        #[test]
        fn prop_multi_round_invariants(
            rounds in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 0..128),
                1..8,
            )
        ) {
            let mut state = SessionState::new(40, 12, Theme::default());
            for bytes in rounds {
                let _ = state.on_stdout(bytes);
            }
            let t = state.terminal();
            prop_assert_eq!(t.screen_cells().len(), t.cols() * t.rows());
            prop_assert!(t.cursor_row() < t.rows());
        }

        /// reset_for_resize 後もサイズ不変量が成立する
        #[test]
        fn prop_resize_then_invariants(
            before in proptest::collection::vec(any::<u8>(), 0..256),
            cols in 10usize..120,
            rows in 4usize..40,
            after in proptest::collection::vec(any::<u8>(), 0..256),
        ) {
            let mut state = SessionState::new(80, 24, Theme::default());
            let _ = state.on_stdout(before);
            state.reset_for_resize(cols, rows);
            let _ = state.on_stdout(after);
            let t = state.terminal();
            prop_assert_eq!(t.cols(), cols);
            prop_assert_eq!(t.rows(), rows);
            prop_assert_eq!(t.screen_cells().len(), cols * rows);
        }
    }
}
