use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info};
use parking_lot::Mutex;
use timed_fsm::TimerCommand;

use crate::{CellData, ScreenUpdate, SessionCallback, DEFAULT_BG, DEFAULT_FG, RUNTIME};
use crate::session_state::{ProcessResult, SessionState, SideEffect};
use crate::terminal::{TermCell, Terminal};
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
}

impl SessionCore {
    pub(crate) fn new() -> Self {
        SessionCore {
            handle_tx: Mutex::new(None),
            session_tx: Mutex::new(None),
            scrollback: Arc::new(Mutex::new(VecDeque::new())),
            screen_cols: Mutex::new(80),
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

        let callback: Arc<dyn SessionCallback> = Arc::from(callback);
        let scrollback = self.scrollback.clone();

        RUNTIME.spawn(async move {
            session_event_loop(event_rx, session_cmd_rx, cmd_tx, callback, scrollback, cols, rows).await;
        });

        (cmd_rx, event_tx)
    }

    pub(crate) fn scrollback_len(&self) -> u32 {
        self.scrollback.lock().len() as u32
    }

    pub(crate) fn scrollback_cells(&self, offset: u32, rows: u32) -> Vec<CellData> {
        let cols = *self.screen_cols.lock() as usize;
        let sb = self.scrollback.lock();
        let blank = CellData { ch: " ".into(), fg: DEFAULT_FG, bg: DEFAULT_BG, bold: false };
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
        if let Some(tx) = self.session_tx.lock().as_ref() {
            let _ = tx.try_send(SessionCmd::TrzszAcceptUpload { transfer_id, file_name, file_size, mode });
        }
    }

    pub(crate) fn trzsz_send_chunk(&self, transfer_id: String, data: Vec<u8>, is_last: bool) {
        if let Some(tx) = self.session_tx.lock().as_ref() {
            let _ = tx.try_send(SessionCmd::TrzszChunk { transfer_id, data, is_last });
        }
    }

    pub(crate) fn trzsz_accept_download(&self, transfer_id: String) {
        if let Some(tx) = self.session_tx.lock().as_ref() {
            let _ = tx.try_send(SessionCmd::TrzszAcceptDownload { transfer_id });
        }
    }

    pub(crate) fn trzsz_cancel(&self, transfer_id: String) {
        if let Some(tx) = self.session_tx.lock().as_ref() {
            let _ = tx.try_send(SessionCmd::TrzszCancel { transfer_id });
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
    init_cols: u32,
    init_rows: u32,
) {
    info!("session: event loop start {}x{}", init_cols, init_rows);
    let mut state = SessionState::new(init_cols as usize, init_rows as usize);
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
                Some(TransportEvent::Connected) => {
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
                Some(TransportEvent::Disconnected { reason }) => {
                    info!("session: disconnected reason={:?}", reason);
                    callback.on_disconnected(reason); break 'outer;
                }
                Some(TransportEvent::NoViablePath) => {
                    info!("session: no viable path (all paths unhealthy)");
                    callback.on_no_viable_path(); None
                }
                None => {
                    info!("session: event channel closed");
                    callback.on_disconnected(None); break 'outer;
                }
            },
            timer_id = timeout_rx.recv() => match timer_id {
                Some(id) => Some(state.on_timeout(id)),
                None => None,
            },
            cmd = session_cmd_rx.recv() => match cmd {
                Some(c) => Some(match c {
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
                }),
                None => None,
            },
        };

        if let Some(r) = result {
            dispatch_result(r, &mut timer_rt, &transport_cmd_tx, &callback,
                            state.terminal(), &scrollback);
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
}
