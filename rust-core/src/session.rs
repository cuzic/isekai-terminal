use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use parking_lot::Mutex;
use timed_fsm::TimerCommand;

use crate::{CellData, ScreenUpdate, SessionCallback, RUNTIME};
use crate::session_state::{ProcessResult, SessionState, SideEffect};
use crate::terminal::{TermCell, Terminal};
use crate::theme::Theme;
use crate::transport::{SessionCmd, TransportCommand, TransportEvent};
use crate::trzsz::{TrzszMode, TrzszTimer};

const SCROLLBACK_LIMIT: usize = 1000;

// ── TermCell → 公開型変換（session 層の責務）────────────

fn to_cell_data(c: &TermCell) -> CellData {
    CellData { ch: c.ch.to_string(), fg: c.fg, bg: c.bg, bold: c.bold }
}

fn make_screen_update(t: &Terminal) -> ScreenUpdate {
    ScreenUpdate {
        cols: t.cols() as u32,
        rows: t.rows() as u32,
        cells: t.screen_cells().iter().map(to_cell_data).collect(),
        cursor_row: t.cursor_row() as u32,
        cursor_col: t.cursor_col() as u32,
        title: t.title().map(str::to_owned),
        application_cursor_mode: t.application_cursor_mode(),
        bracketed_paste_mode: t.bracketed_paste_mode(),
    }
}

// ── TokioTimerRuntime ────────────────────────────────────

struct TokioTimerRuntime {
    handle: Option<tokio::task::JoinHandle<()>>,
    timeout_tx: tokio::sync::mpsc::Sender<TrzszTimer>,
}

impl TokioTimerRuntime {
    fn new(timeout_tx: tokio::sync::mpsc::Sender<TrzszTimer>) -> Self {
        TokioTimerRuntime { handle: None, timeout_tx }
    }

    fn set(&mut self, id: TrzszTimer, dur: Duration) {
        if self.handle.is_some() {
            debug!("trzsz timer: replace {:?} dur={:?}", id, dur);
        } else {
            debug!("trzsz timer: set {:?} dur={:?}", id, dur);
        }
        self.kill(id);
        let tx = self.timeout_tx.clone();
        self.handle = Some(tokio::spawn(async move {
            tokio::time::sleep(dur).await;
            debug!("trzsz timer: fired {:?}", id);
            let _ = tx.send(id).await;
        }));
    }

    fn kill(&mut self, id: TrzszTimer) {
        if let Some(h) = self.handle.take() {
            debug!("trzsz timer: killed {:?}", id);
            h.abort();
        }
    }
}

// ── SessionCore ──────────────────────────────────────────

pub(crate) struct SessionCore {
    handle_tx: Mutex<Option<tokio::sync::mpsc::Sender<TransportCommand>>>,
    session_tx: Mutex<Option<tokio::sync::mpsc::Sender<SessionCmd>>>,
    scrollback: Arc<Mutex<VecDeque<Vec<TermCell>>>>,
    screen_cols: Mutex<u32>,
    /// Phase 12: per-session theme。このセッション(タブ)が現在使っているテーマの
    /// スナップショット。[scrollback_cells]の空白パディング色にもここから使う。
    current_theme: Mutex<Theme>,
    /// Phase 1C(#26): [notify_network_lost]が「ハンドシェイク中か接続済みか」を
    /// 判断するために見る。`start()`でfalseにリセットし、`TransportEvent::Connected`
    /// を受け取った時点で`session_event_loop`がtrueにする(Android版
    /// `SessionOrchestrator`の`ConnPhase`と同種の情報だが、iOSは`SessionOrchestrator`
    /// を経由しない低レベルセッションを直接使うため、ここ`SessionCore`に持たせる
    /// 必要がある)。
    connected: Arc<AtomicBool>,
    /// [start] に渡された callback の複製。ブートストラップ用 SSH 接続
    /// （`isekai_pipe_quic_transport::bootstrap_helper_via_ssh` 等、isekai-helper を
    /// 起動するための踏み台 SSH）が発火する `TransportEvent::HostKey` を、本セッションの
    /// `on_host_key`（Kotlin 側の `KnownHostRepository` を参照する既存の TOFU/変更検知
    /// ロジック）へそのまま転送するために保持する。このセッションで発生し得る唯一の
    /// HostKey イベントはこのブートストラップ SSH 由来であり（QUIC データプレーン自体は
    /// ホスト鍵という概念を持たず cert_sha256 ピン留めのみ)、同一の意思決定ロジックを
    /// 流用してよい（`[callback]` getter 参照）。
    callback: Mutex<Option<Arc<dyn SessionCallback>>>,
}

impl SessionCore {
    pub(crate) fn new() -> Self {
        SessionCore {
            handle_tx: Mutex::new(None),
            session_tx: Mutex::new(None),
            scrollback: Arc::new(Mutex::new(VecDeque::new())),
            screen_cols: Mutex::new(80),
            current_theme: Mutex::new(Theme::default()),
            connected: Arc::new(AtomicBool::new(false)),
            callback: Mutex::new(None),
        }
    }

    pub(crate) fn start(
        &self,
        cols: u32,
        rows: u32,
        callback: Box<dyn SessionCallback>,
    ) -> (tokio::sync::mpsc::Receiver<TransportCommand>, tokio::sync::mpsc::Sender<TransportEvent>) {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<TransportCommand>(64);
        let (event_tx, event_rx) = tokio::sync::mpsc::channel::<TransportEvent>(256);
        let (session_cmd_tx, session_cmd_rx) = tokio::sync::mpsc::channel::<SessionCmd>(64);

        *self.handle_tx.lock() = Some(cmd_tx.clone());
        *self.session_tx.lock() = Some(session_cmd_tx);
        *self.screen_cols.lock() = cols;
        self.scrollback.lock().clear();
        self.connected.store(false, Ordering::SeqCst);
        // 接続(タブ作成)時点のグローバル既定テーマをスナップショットする。呼び出し側
        // (Kotlin)がプロファイル固有のテーマを使いたい場合は、この直後に[set_theme]を
        // 呼んで明示的に上書きする。
        let initial_theme = crate::theme::current();
        *self.current_theme.lock() = initial_theme;

        let callback: Arc<dyn SessionCallback> = Arc::from(callback);
        *self.callback.lock() = Some(Arc::clone(&callback));
        let scrollback = self.scrollback.clone();
        let connected = self.connected.clone();

        RUNTIME.spawn(async move {
            session_event_loop(event_rx, session_cmd_rx, cmd_tx, callback, scrollback, connected, cols, rows, initial_theme).await;
        });

        (cmd_rx, event_tx)
    }

    /// [start] に渡された callback の複製を返す(`start` 呼び出し後のみ `Some`)。
    /// ブートストラップ SSH のホスト鍵検証をこのセッション本来の callback に
    /// 委譲したい呼び出し元（`isekai_pipe_quic_transport` 等）向け。
    pub(crate) fn callback(&self) -> Option<Arc<dyn SessionCallback>> {
        self.callback.lock().clone()
    }

    /// このセッション(タブ)のテーマを差し替える。[start]の前後どちらで呼んでも安全
    /// (start前に呼んだ場合は次のstart時に上書きされてしまうため、通常はstartの直後に
    /// 呼ぶこと)。
    pub(crate) fn set_theme(&self, theme: Theme) {
        *self.current_theme.lock() = theme;
        self.send_session_cmd(SessionCmd::SetTheme(theme));
    }

    /// [session_tx]が張られていれば(=`start`後かつ`disconnect`前なら)`cmd`を投げる。
    /// 未接続/切断済みなら黙って無視する(呼び出し側は都度存在確認しなくてよい)。
    fn send_session_cmd(&self, cmd: SessionCmd) {
        if let Some(tx) = self.session_tx.lock().as_ref() {
            let _ = tx.try_send(cmd);
        }
    }

    /// transport コマンド送信端を複製して返す。connect() 直後に
    /// 初期ポートフォワード(config.forwards)を投入するために使う。
    pub(crate) fn command_sender(&self) -> Option<tokio::sync::mpsc::Sender<TransportCommand>> {
        self.handle_tx.lock().clone()
    }

    pub(crate) fn scrollback_len(&self) -> u32 {
        self.scrollback.lock().len() as u32
    }

    pub(crate) fn scrollback_cells(&self, offset: u32, rows: u32) -> Vec<CellData> {
        let theme = *self.current_theme.lock();
        let cols = *self.screen_cols.lock() as usize;
        let sb = self.scrollback.lock();
        let blank = CellData { ch: " ".into(), fg: theme.default_fg, bg: theme.default_bg, bold: false };
        let mut result = vec![blank; rows as usize * cols];
        for r in 0..rows as usize {
            let sb_idx = offset as usize + (rows as usize - 1 - r);
            if let Some(row) = sb.get(sb_idx) {
                let copy_cols = row.len().min(cols);
                for (i, cell) in row[..copy_cols].iter().enumerate() {
                    result[r * cols + i] = to_cell_data(cell);
                }
            }
        }
        result
    }

    pub(crate) fn send(&self, data: Vec<u8>) {
        if let Some(tx) = self.handle_tx.lock().as_ref() {
            if tx.try_send(TransportCommand::WriteStdin(data)).is_err() {
                log::warn!("ssh: stdin channel full, keystroke dropped");
            }
        }
    }

    pub(crate) fn resize(&self, cols: u32, rows: u32) {
        *self.screen_cols.lock() = cols;
        if let Some(tx) = self.handle_tx.lock().as_ref() {
            if tx.try_send(TransportCommand::Resize { cols, rows }).is_err() {
                log::warn!("ssh: resize command dropped (channel full)");
            }
        }
    }

    pub(crate) fn disconnect(&self) {
        if let Some(tx) = self.handle_tx.lock().as_ref() {
            if tx.try_send(TransportCommand::Disconnect).is_err() {
                let tx = tx.clone();
                crate::RUNTIME.spawn(async move {
                    let _ = tx.send(TransportCommand::Disconnect).await;
                });
            }
        }
        *self.session_tx.lock() = None;
    }

    pub(crate) fn trzsz_accept_upload(&self, transfer_id: String, file_name: String, file_size: u64, mode: u32) {
        self.send_session_cmd(SessionCmd::TrzszAcceptUpload { transfer_id, file_name, file_size, mode });
    }

    pub(crate) fn trzsz_send_chunk(&self, transfer_id: String, data: Vec<u8>, is_last: bool) {
        self.send_session_cmd(SessionCmd::TrzszChunk { transfer_id, data, is_last });
    }

    pub(crate) fn trzsz_accept_download(&self, transfer_id: String) {
        self.send_session_cmd(SessionCmd::TrzszAcceptDownload { transfer_id });
    }

    pub(crate) fn trzsz_cancel(&self, transfer_id: String) {
        self.send_session_cmd(SessionCmd::TrzszCancel { transfer_id });
    }

    /// Phase 1C(#26): OSからネットワーク断(Wi-Fi/セルラー消失等)を通知された時の対応を
    /// 決める。`SessionOrchestrator::notify_network_lost`(Android版が使う)と同じ方針
    /// (QUIC接続はパス変更に自前で耐えられるため無視し、ハンドシェイク中やプレーンTCPは
    /// 切断扱いにする)を、iOSが直接使う低レベルセッション(`SshSession`/
    /// `IsekaiPipeQuicSession`等)側でも成立させる。呼び出し側(Swift)はOSの生イベントを
    /// そのまま転送するだけで、判断はこの関数(Rust SSOT)が行う。
    pub(crate) fn notify_network_lost(&self, is_quic: bool) {
        let has_session = self.handle_tx.lock().is_some();
        let connected = self.connected.load(Ordering::SeqCst);
        if should_abort_on_network_lost(has_session, connected, is_quic) {
            log::warn!(
                "session: network lost — aborting (is_quic={is_quic}, connected={connected})"
            );
            self.disconnect();
        } else {
            log::info!(
                "session: network lost — ignoring (has_session={has_session}, is_quic={is_quic}, connected={connected})"
            );
        }
    }
}

/// [SessionCore::notify_network_lost]の判断ロジック本体。実チャネル/AtomicBoolから
/// 切り離した純粋関数にすることで、tokioタスクを起動せずに全パターンを単体テストできる。
fn should_abort_on_network_lost(has_session: bool, connected: bool, is_quic: bool) -> bool {
    if !has_session {
        // 維持すべき接続がそもそも無い(Idle)。
        return false;
    }
    if connected && is_quic {
        // 接続済みQUICはtransport自身のtransparent resumeを信頼し、何もしない。
        return false;
    }
    // ハンドシェイク中(connected==false)、または接続済み非QUIC(プレーンSSH等)。
    true
}

/// Kotlin/Swift側から送られてきた`SessionCmd`を`SessionState`に適用する。
/// `session_event_loop`の`select!`アームから切り出したもの
/// (select → match(cmd) → match(c)という三重ネストを避けるため)。
fn handle_session_cmd(state: &mut SessionState, c: SessionCmd) -> ProcessResult {
    match c {
        SessionCmd::TrzszAcceptUpload { transfer_id, file_name, file_size, mode } => {
            info!("session: TrzszAcceptUpload id={} file={} size={}", transfer_id, file_name, file_size);
            state.on_kotlin_accept_upload(transfer_id, file_name, file_size, mode)
        }
        SessionCmd::TrzszChunk { transfer_id, data, is_last } => {
            info!("session: TrzszChunk id={} size={} is_last={}", transfer_id, data.len(), is_last);
            state.on_kotlin_chunk(transfer_id, data, is_last)
        }
        SessionCmd::TrzszAcceptDownload { transfer_id } =>
            state.on_kotlin_accept_download(transfer_id),
        SessionCmd::TrzszCancel { transfer_id } =>
            state.on_kotlin_cancel(transfer_id),
        SessionCmd::SetTheme(theme) => {
            state.set_theme(theme);
            ProcessResult {
                timer_cmds: Vec::new(),
                side_effects: Vec::new(),
                pending_rows: Vec::new(),
                screen_dirty: false,
                pending_clipboard_write: None,
                clipboard_pull_requested: false,
            }
        }
    }
}

// ── session event loop（薄い async ラッパー）──────────────

pub(crate) async fn session_event_loop(
    mut event_rx: tokio::sync::mpsc::Receiver<TransportEvent>,
    mut session_cmd_rx: tokio::sync::mpsc::Receiver<SessionCmd>,
    transport_cmd_tx: tokio::sync::mpsc::Sender<TransportCommand>,
    callback: Arc<dyn SessionCallback>,
    scrollback: Arc<Mutex<VecDeque<Vec<TermCell>>>>,
    connected: Arc<AtomicBool>,
    init_cols: u32,
    init_rows: u32,
    initial_theme: Theme,
) {
    info!("session: event loop start {}x{}", init_cols, init_rows);
    let mut state = SessionState::new(init_cols as usize, init_rows as usize, initial_theme);
    let (timeout_tx, mut timeout_rx) = tokio::sync::mpsc::channel::<TrzszTimer>(16);
    let mut timer_rt = TokioTimerRuntime::new(timeout_tx);

    'outer: loop {
        let result: Option<ProcessResult> = tokio::select! {
            event = event_rx.recv() => match event {
                Some(TransportEvent::HostKey(fp, reply_tx)) => {
                    let cb = Arc::clone(&callback);
                    tokio::task::spawn_blocking(move || {
                        let accepted = cb.on_host_key(fp);
                        let _ = reply_tx.send(accepted);
                    });
                    None
                }
                Some(TransportEvent::AgentSignRequest { key_fingerprint, reply }) => {
                    let cb = Arc::clone(&callback);
                    tokio::task::spawn_blocking(move || {
                        let approved = cb.on_agent_sign_request(key_fingerprint);
                        let _ = reply.send(approved);
                    });
                    None
                }
                Some(TransportEvent::Connected) => {
                    connected.store(true, Ordering::SeqCst);
                    callback.on_connected(); None
                }
                Some(TransportEvent::Stdout(bytes)) => {
                    callback.on_data(bytes.clone());
                    Some(state.on_stdout(bytes))
                }
                Some(TransportEvent::Resized { cols, rows }) => {
                    info!("session: terminal resize {}x{}, scrollback cleared", cols, rows);
                    state.reset_for_resize(cols as usize, rows as usize);
                    scrollback.lock().clear();
                    None
                }
                Some(TransportEvent::ForwardStateChanged { id, state }) => {
                    callback.on_forward_state_changed(id, state); None
                }
                Some(TransportEvent::CtlMessage(msg)) => {
                    match msg {
                        isekai_protocol::CtlMessage::SetTitle { value } => {
                            Some(state.set_title_from_ctl(value))
                        }
                        isekai_protocol::CtlMessage::ClipboardPush { data_b64, .. } => {
                            match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &data_b64) {
                                Ok(decoded) => match String::from_utf8(decoded) {
                                    Ok(text) => {
                                        let cb = Arc::clone(&callback);
                                        tokio::task::spawn_blocking(move || cb.on_clipboard_write(text));
                                    }
                                    Err(e) => warn!("ctl-socket: clipboard push was not valid UTF-8: {e}"),
                                },
                                Err(e) => warn!("ctl-socket: clipboard push data_b64 was not valid base64: {e}"),
                            }
                            None
                        }
                        // device→host のクリップボード読み出し(pull)は、この新しい
                        // チャネルでの応答書き込みが未実装(`ISEKAI_PIPE_DESIGN.md` §8
                        // Epic M follow-up、タスク#84の既知の残作業)。OSC 52経由の
                        // pull(`ClipboardPullRequest`をterminal.rsが検出するパス)は
                        // 別に実装済み — こちらは無視するだけ。
                        isekai_protocol::CtlMessage::ClipboardPullRequest {} => {
                            debug!("ctl-socket: clipboard pull over ctl-socket is not yet implemented, ignoring");
                            None
                        }
                        isekai_protocol::CtlMessage::ClipboardPullResponse { .. } => None,
                    }
                }
                Some(TransportEvent::Disconnected { reason }) => {
                    info!("session: disconnected reason={:?}", reason);
                    connected.store(false, Ordering::SeqCst);
                    callback.on_disconnected(reason); break 'outer;
                }
                Some(TransportEvent::NoViablePath) => {
                    info!("session: no viable path (all paths unhealthy)");
                    callback.on_no_viable_path(); None
                }
                None => {
                    info!("session: event channel closed");
                    connected.store(false, Ordering::SeqCst);
                    callback.on_disconnected(None); break 'outer;
                }
            },
            timer_id = timeout_rx.recv() => match timer_id {
                Some(id) => Some(state.on_timeout(id)),
                None => None,
            },
            cmd = session_cmd_rx.recv() => match cmd {
                Some(c) => Some(handle_session_cmd(&mut state, c)),
                None => None,
            },
        };

        if let Some(r) = result {
            let clipboard_pull_requested = r.clipboard_pull_requested;
            dispatch_result(r, &mut timer_rt, &transport_cmd_tx, &callback,
                            state.terminal(), &scrollback);
            if clipboard_pull_requested {
                // Fetching the current Android clipboard text needs a Kotlin
                // round trip (`on_host_key`/`on_agent_sign_request`'s same
                // `spawn_blocking` pattern) that `dispatch_result` — a plain
                // sync fn — can't perform. `None` means "opt-in disabled or
                // no clipboard content": send nothing back rather than an
                // explicit empty reply (`ISEKAI_PIPE_DESIGN.md` §8 Epic M).
                let cb = Arc::clone(&callback);
                let tx = transport_cmd_tx.clone();
                tokio::task::spawn_blocking(move || {
                    if let Some(text) = cb.on_clipboard_pull_request() {
                        let data_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, text);
                        let response = format!("\x1b]52;c;{data_b64}\x07").into_bytes();
                        let _ = tx.blocking_send(TransportCommand::WriteStdin(response));
                    }
                });
            }
        }
    }
}

/// ProcessResult をすべて処理する（タイマー・scrollback・副作用・画面更新）
fn dispatch_result(
    r: ProcessResult,
    timer_rt: &mut TokioTimerRuntime,
    transport_cmd_tx: &tokio::sync::mpsc::Sender<TransportCommand>,
    callback: &Arc<dyn SessionCallback>,
    terminal: &Terminal,
    scrollback: &Arc<Mutex<VecDeque<Vec<TermCell>>>>,
) {
    for cmd in r.timer_cmds {
        match cmd {
            TimerCommand::Set { id, duration } => timer_rt.set(id, duration),
            TimerCommand::Kill { id }           => timer_rt.kill(id),
        }
    }

    if !r.pending_rows.is_empty() {
        let mut sb = scrollback.lock();
        for row in r.pending_rows { sb.push_front(row); }
        let overflow = sb.len().saturating_sub(SCROLLBACK_LIMIT);
        if overflow > 0 {
            for _ in 0..overflow { sb.pop_back(); }
            debug!("scrollback: dropped {} row(s), total={}", overflow, sb.len());
        }
    }

    for effect in r.side_effects {
        match effect {
            SideEffect::SendStdin(bytes) => {
                let len = bytes.len();
                if let Err(e) = transport_cmd_tx.try_send(TransportCommand::WriteStdin(bytes)) {
                    log::error!("trzsz: FATAL try_send WriteStdin({} bytes) failed: {}", len, e);
                }
            }
            SideEffect::TrzszRequest { transfer_id, mode, suggested_name, expected_size } => {
                let mode_str = match mode {
                    TrzszMode::Upload   => "upload",
                    TrzszMode::Download => "download",
                    TrzszMode::Dir      => "dir",
                }.to_string();
                info!("trzsz: request {} mode={} name={:?} size={:?}",
                    transfer_id, mode_str, suggested_name, expected_size);
                callback.on_trzsz_request(transfer_id, mode_str, suggested_name, expected_size);
            }
            SideEffect::DownloadChunk { transfer_id, data, is_last } => {
                debug!("trzsz: download chunk {} bytes is_last={}", data.len(), is_last);
                callback.on_trzsz_download_chunk(transfer_id, data, is_last);
            }
            SideEffect::Progress { transfer_id, transferred, total } => {
                debug!("trzsz: progress {} {}/{:?}", transfer_id, transferred, total);
                callback.on_trzsz_progress(transfer_id, transferred, total);
            }
            SideEffect::Finished { transfer_id, success, message } => {
                info!("trzsz: finished {} success={} msg={:?}", transfer_id, success, message);
                callback.on_trzsz_finished(transfer_id, success, message);
            }
        }
    }

    if r.screen_dirty {
        let upd = make_screen_update(terminal);
        debug!("screen: update {}x{} cursor=({},{})",
            upd.cols, upd.rows, upd.cursor_col, upd.cursor_row);
        callback.on_screen_update(upd);
    }

    // OSC 52 クリップボード書き込み(`ISEKAI_PIPE_DESIGN.md` §8 Epic M)。opt-inかどうかの
    // 判断はKotlin側(`TerminalSession`)に委ねる——ここは「リモートがこう要求した」という
    // 事実をそのまま伝えるだけで、適用するかどうかの分岐はRust側に持ち込まない
    // (`.claude/rules/rust-ssot.md`が対象にしているのはセッション/プロトコル状態であり、
    // これは単なるイベント通知)。
    if let Some(text) = r.pending_clipboard_write {
        callback.on_clipboard_write(text);
    }
}

// ── Tests ────────────────────────────────────────────────

#[cfg(test)]
mod should_abort_on_network_lost_tests {
    use super::should_abort_on_network_lost;

    #[test]
    fn idle_is_always_ignored() {
        assert!(!should_abort_on_network_lost(false, false, false));
        assert!(!should_abort_on_network_lost(false, false, true));
        // has_session=falseならconnectedは実質意味を持たないが念のため網羅する。
        assert!(!should_abort_on_network_lost(false, true, false));
        assert!(!should_abort_on_network_lost(false, true, true));
    }

    #[test]
    fn handshake_in_progress_is_always_aborted_regardless_of_transport() {
        assert!(should_abort_on_network_lost(true, false, false));
        assert!(should_abort_on_network_lost(true, false, true));
    }

    #[test]
    fn connected_quic_is_ignored_trusting_transparent_resume() {
        assert!(!should_abort_on_network_lost(true, true, true));
    }

    #[test]
    fn connected_non_quic_is_aborted() {
        assert!(should_abort_on_network_lost(true, true, false));
    }
}

// `SessionCore::scrollback_cells`(オフセット/行数からscrollbackを切り出す表示ロジック)
// と`dispatch_result`のscrollback上限トリミングは、実SSH/QUIC接続もTokioランタイムも
// 不要な純粋なデータ変換だが、`session.rs`にはテストが1つも無かった。
#[cfg(test)]
mod tests {
    use super::*;

    fn cell(label: char) -> TermCell {
        TermCell { ch: smol_str::SmolStr::new(label.to_string()), fg: 0xFF010101, bg: 0xFF020202, bold: false }
    }

    fn row(label: char, len: usize) -> Vec<TermCell> {
        vec![cell(label); len]
    }

    fn core_with_scrollback(cols: u32, rows_by_index: Vec<Vec<TermCell>>) -> SessionCore {
        let core = SessionCore::new();
        *core.screen_cols.lock() = cols;
        *core.scrollback.lock() = VecDeque::from(rows_by_index);
        core
    }

    // index0 = newest(直近scrollしたばかりの行、ライブ画面に一番近い)、
    // 添字が大きいほど古い行 — dispatch_resultがpush_frontで積む順に合わせている。

    #[test]
    fn scrollback_cells_orders_oldest_to_newest_top_to_bottom() {
        let core = core_with_scrollback(3, vec![
            row('0', 3), row('1', 3), row('2', 3), row('3', 3), row('4', 3),
        ]);
        let cells = core.scrollback_cells(0, 3);
        assert_eq!(cells[0 * 3].ch, "2"); // viewport最上段 = idx2(このoffsetでの最古)
        assert_eq!(cells[1 * 3].ch, "1");
        assert_eq!(cells[2 * 3].ch, "0"); // viewport最下段 = idx0(ライブ画面に一番近い)
    }

    #[test]
    fn scrollback_cells_offset_shifts_further_into_history() {
        let core = core_with_scrollback(3, vec![
            row('0', 3), row('1', 3), row('2', 3), row('3', 3), row('4', 3),
        ]);
        let cells = core.scrollback_cells(2, 3);
        assert_eq!(cells[0 * 3].ch, "4");
        assert_eq!(cells[1 * 3].ch, "3");
        assert_eq!(cells[2 * 3].ch, "2");
    }

    #[test]
    fn scrollback_cells_pads_with_blank_theme_color_past_available_history() {
        let core = core_with_scrollback(2, vec![row('0', 2)]);
        let cells = core.scrollback_cells(5, 3); // 唯一ある行よりずっと過去を要求
        let theme = Theme::default();
        for c in &cells {
            assert_eq!(c.ch, " ");
            assert_eq!(c.fg, theme.default_fg);
            assert_eq!(c.bg, theme.default_bg);
        }
    }

    #[test]
    fn scrollback_cells_truncates_rows_longer_than_screen_cols() {
        let core = core_with_scrollback(2, vec![row('x', 5)]); // cols=2より広い行
        let cells = core.scrollback_cells(0, 1);
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].ch, "x");
        assert_eq!(cells[1].ch, "x");
    }

    #[test]
    fn scrollback_cells_pads_rows_shorter_than_screen_cols_with_blanks() {
        let core = core_with_scrollback(5, vec![row('y', 2)]); // cols=5より狭い行
        let cells = core.scrollback_cells(0, 1);
        assert_eq!(cells.len(), 5);
        assert_eq!(cells[0].ch, "y");
        assert_eq!(cells[1].ch, "y");
        assert_eq!(cells[2].ch, " ");
        assert_eq!(cells[3].ch, " ");
        assert_eq!(cells[4].ch, " ");
    }

    // ── dispatch_result: scrollback上限トリミング ────────────

    struct NoopSessionCallback;
    impl SessionCallback for NoopSessionCallback {
        fn on_data(&self, _data: Vec<u8>) {}
        fn on_host_key(&self, _fingerprint: String) -> bool { true }
        fn on_connected(&self) {}
        fn on_disconnected(&self, _reason: Option<String>) {}
        fn on_screen_update(&self, _update: ScreenUpdate) {}
        fn on_trzsz_request(&self, _transfer_id: String, _mode: String, _suggested_name: Option<String>, _expected_size: Option<u64>) {}
        fn on_trzsz_download_chunk(&self, _transfer_id: String, _data: Vec<u8>, _is_last: bool) {}
        fn on_trzsz_progress(&self, _transfer_id: String, _transferred: u64, _total: Option<u64>) {}
        fn on_trzsz_finished(&self, _transfer_id: String, _success: bool, _message: Option<String>) {}
        fn on_no_viable_path(&self) {}
        fn on_forward_state_changed(&self, _id: String, _state: crate::ForwardState) {}
        fn on_agent_sign_request(&self, _key_fingerprint: String) -> bool { true }
        fn on_clipboard_write(&self, _text: String) {}
        fn on_clipboard_pull_request(&self) -> Option<String> { None }
    }

    #[test]
    fn dispatch_result_trims_scrollback_to_limit_by_dropping_the_oldest() {
        // SCROLLBACK_LIMIT - 1 件を予め積んでおき、末尾(=最古)に目印の行を置く。
        let mut initial: VecDeque<Vec<TermCell>> = (0..SCROLLBACK_LIMIT - 1)
            .map(|_| row('a', 1))
            .collect();
        *initial.back_mut().unwrap() = row('Z', 1); // 最古 = 一番最初に落ちるべき行
        let scrollback = Arc::new(Mutex::new(initial));

        let (transport_cmd_tx, _transport_cmd_rx) = tokio::sync::mpsc::channel(8);
        let (timeout_tx, _timeout_rx) = tokio::sync::mpsc::channel(1);
        let mut timer_rt = TokioTimerRuntime::new(timeout_tx);
        let callback: Arc<dyn SessionCallback> = Arc::new(NoopSessionCallback);
        let terminal = Terminal::new(80, 24, Theme::default());

        let result = ProcessResult {
            timer_cmds: Vec::new(),
            side_effects: Vec::new(),
            pending_rows: vec![row('N', 1), row('N', 1), row('N', 1)], // 3行新規追加
            screen_dirty: false,
            pending_clipboard_write: None,
            clipboard_pull_requested: false,
        };
        dispatch_result(result, &mut timer_rt, &transport_cmd_tx, &callback, &terminal, &scrollback);

        let sb = scrollback.lock();
        assert_eq!(sb.len(), SCROLLBACK_LIMIT, "should be capped at SCROLLBACK_LIMIT, not left at +3 over");
        assert!(
            sb.iter().all(|r| r[0].ch != "Z"),
            "the oldest row (back of the deque) must be the one evicted, not an arbitrary one"
        );
    }
}
