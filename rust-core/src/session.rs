use std::collections::VecDeque;
use std::sync::Arc;

use log::{debug, info, warn};
use parking_lot::Mutex;
use timed_fsm::tokio_support::TokioTimerRuntime;
use timed_fsm::{TimerCommand, TimerRuntime};

use crate::{CellData, ClipboardMimeKind, ClipboardPayload, ScreenUpdate, ScrollbackSearchMatch, SessionCallback, RUNTIME};
use crate::session_state::{ProcessResult, SessionState, SideEffect};
use crate::terminal::{TermCell, Terminal};
use crate::theme::Theme;
use crate::transport::{SessionCmd, TransportCommand, TransportEvent};
use crate::trzsz::{TrzszMode, TrzszTimer};

const SCROLLBACK_LIMIT: usize = 1000;

// ── TermCell → 公開型変換（session 層の責務）────────────

fn to_cell_data(c: &TermCell) -> CellData {
    CellData {
        ch: c.ch.to_string(),
        fg: c.fg,
        bg: c.bg,
        bold: c.bold,
        dim: c.dim,
        italic: c.italic,
        underline: c.underline,
        strikethrough: c.strikethrough,
        blink: c.blink,
        invisible: c.invisible,
        link_id: c.link_id,
    }
}

fn make_screen_update(t: &Terminal) -> ScreenUpdate {
    ScreenUpdate {
        cols: t.cols() as u32,
        rows: t.rows() as u32,
        cells: t.screen_cells().iter().map(to_cell_data).collect(),
        cursor_row: t.cursor_row() as u32,
        // `Terminal::cursor_col()`は遅延折り返し(delayed wrap)状態を`cols`
        // (範囲外)で表す内部表現をそのまま返す(`terminal.rs`のCPR/EL/ED実装
        // 参照)。UniFFI越しにAndroid/iOSへ渡す`ScreenUpdate.cursor_col`は
        // 常に`0..cols`の描画可能な列を指すべきなので、可視上の最終列
        // (`cols - 1`)にクランプしてから公開する(Fableレビュー: タスク#56)。
        cursor_col: t.cursor_col().min(t.cols().saturating_sub(1)) as u32,
        title: t.title().map(str::to_owned),
        application_cursor_mode: t.application_cursor_mode(),
        application_keypad_mode: t.application_keypad_mode(),
        bracketed_paste_mode: t.bracketed_paste_mode(),
        mouse_reporting_mode: t.mouse_reporting_mode(),
        sgr_mouse_mode: t.sgr_mouse_mode(),
        cursor_visible: t.cursor_visible(),
        bell_generation: t.bell_generation(),
        cursor_shape: t.cursor_shape(),
        cursor_blink: t.cursor_blink(),
        link_table: t.link_table().to_vec(),
        images: t.images().to_vec(),
        kitty_keyboard_flags: t.kitty_keyboard_flags(),
    }
}

// ── SessionCore ──────────────────────────────────────────

pub(crate) struct SessionCore {
    handle_tx: Mutex<Option<tokio::sync::mpsc::Sender<TransportCommand>>>,
    session_tx: Mutex<Option<tokio::sync::mpsc::Sender<SessionCmd>>>,
    scrollback: Arc<Mutex<VecDeque<Vec<TermCell>>>>,
    screen_cols: Mutex<u32>,
    /// [resize]に直近渡された(cols, rows)。同一値の連続呼び出し(#62: IME開閉・回転・
    /// ピンチズームでKotlin/Swift側から不要に連発されるresize)をここで一元的に無視する
    /// ためだけに使う(rust-ssot: 判定ロジックをKotlin/Swift側にミラーしない)。
    last_resize_dims: Mutex<(u32, u32)>,
    /// Phase 12: per-session theme。このセッション(タブ)が現在使っているテーマの
    /// スナップショット。[scrollback_cells]の空白パディング色にもここから使う。
    current_theme: Mutex<Theme>,
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
            last_resize_dims: Mutex::new((80, 24)),
            current_theme: Mutex::new(Theme::default()),
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
        *self.last_resize_dims.lock() = (cols, rows);
        self.scrollback.lock().clear();
        // 接続(タブ作成)時点のグローバル既定テーマをスナップショットする。呼び出し側
        // (Kotlin)がプロファイル固有のテーマを使いたい場合は、この直後に[set_theme]を
        // 呼んで明示的に上書きする。
        let initial_theme = crate::theme::current();
        *self.current_theme.lock() = initial_theme;

        let callback: Arc<dyn SessionCallback> = Arc::from(callback);
        *self.callback.lock() = Some(Arc::clone(&callback));
        let scrollback = self.scrollback.clone();

        RUNTIME.spawn(async move {
            session_event_loop(event_rx, session_cmd_rx, cmd_tx, callback, scrollback, cols, rows, initial_theme).await;
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
        let blank = CellData {
            ch: " ".into(),
            fg: theme.default_fg,
            bg: theme.default_bg,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            strikethrough: false,
            blink: false,
            invisible: false,
            link_id: None,
        };
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

    /// scrollbackを対象にした部分一致検索(タスク#37)。`query`が空文字列なら
    /// (空文字列は「全セルにマッチ」という無意味な結果になるため)空Vecを返す。
    ///
    /// 各行を「全角文字のプレースホルダセルを除いた1セル=1文字単位」の列として
    /// 展開してから素朴な部分文字列探索を行う。プレースホルダを除くのは、
    /// プレースホルダの`ch`(常に半角スペース)が実在しない文字として検索文字列に
    /// 混入し、隣接セルをまたぐマッチを誤って分断/接続してしまうのを防ぐため。
    ///
    /// combining character(タスク#39)によりセルの`ch`が複数コードポイントを
    /// 持つ場合、そのセルの全コードポイントに同じ「セル通し番号」を振り、マッチの
    /// 開始/終了がその通し番号の境界(=セルの先頭/末尾)以外では成立しないよう
    /// 制約する。これにより例えば"e"+結合アクセントの1セルに対し、"e"や結合
    /// アクセント単体だけがそのセルの内部で部分マッチすることはない(Codexレビュー:
    /// 当初の実装はcharの列をそのまま探索しておりこの制約が無かった)。
    ///
    /// `case_sensitive=false`の大文字小文字比較はASCIIの範囲のみ
    /// (`char::to_ascii_lowercase`)。非ASCII文字のUnicodeケースフォールディングは
    /// 対象外(文字数が変わりうるcaseがあり、列インデックスとの対応が崩れるため)。
    ///
    /// スコープ外については[crate::ScrollbackSearchMatch]のドキュメントを参照。
    pub(crate) fn search_scrollback(&self, query: &str, case_sensitive: bool) -> Vec<ScrollbackSearchMatch> {
        let needle: Vec<char> = if case_sensitive {
            query.chars().collect()
        } else {
            query.chars().map(|c| c.to_ascii_lowercase()).collect()
        };
        if needle.is_empty() {
            return Vec::new();
        }

        let sb = self.scrollback.lock();
        let mut matches = Vec::new();
        for (row_idx, row) in sb.iter().enumerate() {
            // 行を「プレースホルダを除いたcharごとに、由来するセルの(列, 表示幅,
            // セル通し番号)」の列へ展開する。同一セル由来のcharは同じ
            // `cell_seq_of`値を持つ。
            let mut haystack: Vec<char> = Vec::with_capacity(row.len());
            let mut col_of: Vec<u32> = Vec::with_capacity(row.len());
            let mut width_of: Vec<u32> = Vec::with_capacity(row.len());
            let mut cell_seq_of: Vec<u32> = Vec::with_capacity(row.len());
            let mut cell_seq: u32 = 0;
            for (col, cell) in row.iter().enumerate() {
                if cell.is_wide_placeholder {
                    continue;
                }
                let width = if row.get(col + 1).is_some_and(|next| next.is_wide_placeholder) { 2 } else { 1 };
                for ch in cell.ch.chars() {
                    haystack.push(if case_sensitive { ch } else { ch.to_ascii_lowercase() });
                    col_of.push(col as u32);
                    width_of.push(width);
                    cell_seq_of.push(cell_seq);
                }
                cell_seq += 1;
            }

            if haystack.len() < needle.len() {
                continue;
            }
            for start in 0..=haystack.len() - needle.len() {
                let end = start + needle.len() - 1; // マッチ末尾のindex(含む)
                // マッチの開始/終了はどちらもセルの境界でなければならない
                // (combining characterセルの途中から始まる/終わるマッチは無効)。
                let starts_at_cell_boundary =
                    start == 0 || cell_seq_of[start - 1] != cell_seq_of[start];
                let ends_at_cell_boundary =
                    end + 1 == haystack.len() || cell_seq_of[end] != cell_seq_of[end + 1];
                if !starts_at_cell_boundary || !ends_at_cell_boundary {
                    continue;
                }
                if haystack[start..start + needle.len()] == needle[..] {
                    let col_start = col_of[start];
                    let col_end = col_of[end] + width_of[end];
                    matches.push(ScrollbackSearchMatch {
                        row: row_idx as u32,
                        col: col_start,
                        len: col_end - col_start,
                    });
                }
            }
        }
        matches
    }

    pub(crate) fn send(&self, data: Vec<u8>) {
        if let Some(tx) = self.handle_tx.lock().as_ref() {
            if tx.try_send(TransportCommand::WriteStdin(data)).is_err() {
                log::warn!("ssh: stdin channel full, keystroke dropped");
            }
        }
    }

    /// (cols, rows)が直前の呼び出しと同一なら何もしない(#62)。Android/iOS双方でIME
    /// キーボードの開閉・端末回転・ピンチズームのたびに同じサイズでresizeが連発され
    /// 得るが、その判定・抑止をここ1箇所(Rust側)に置くことで、Kotlin/Swift側に
    /// 同種のミラー判定を重複実装しなくて済む(rust-ssot原則)。
    ///
    /// dedupeキャッシュ(`last_resize_dims`)は、実際に`TransportCommand::Resize`の
    /// 送出に成功した場合にのみ更新する。送出前に更新してしまうと、チャネルが
    /// フル(`try_send`失敗)で今回のリサイズが実際には配送されなかった場合、次に
    /// 同じ(cols, rows)でリサイズ要求が来ても「前回と同一だから」という理由で
    /// 永久にスキップされてしまい、リモートPTYのサイズが実際の画面サイズと
    /// 食い違ったまま復旧できなくなる。
    pub(crate) fn resize(&self, cols: u32, rows: u32) {
        if *self.last_resize_dims.lock() == (cols, rows) {
            return;
        }
        *self.screen_cols.lock() = cols;
        let sent = match self.handle_tx.lock().as_ref() {
            Some(tx) => match tx.try_send(TransportCommand::Resize { cols, rows }) {
                Ok(()) => true,
                Err(_) => {
                    log::warn!("ssh: resize command dropped (channel full)");
                    false
                }
            },
            // まだ`start()`前(transportハンドル未確立)。実際には何も送っていないので
            // dedupeキャッシュは更新しない(`start()`が改めて初期サイズで初期化する)。
            None => false,
        };
        if sent {
            *self.last_resize_dims.lock() = (cols, rows);
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

    /// OSのフォーカス変化(タスク#60: タブ/split pane切替・アプリのbackground/foreground等)
    /// をそのまま`session_event_loop`へ転送する。フォーカスレポーティング(`?1004`)が
    /// 無効、または未接続/切断済みの場合は`session_state::notify_focus_change`/
    /// `send_session_cmd`がそれぞれ無音で無視する(rust-ssot: 有効/無効の判断は
    /// Rust側のみが持ち、Kotlin/Swift側は生イベントを転送するだけでよい)。
    pub(crate) fn notify_focus_change(&self, focused: bool) {
        self.send_session_cmd(SessionCmd::FocusChanged(focused));
    }
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
        SessionCmd::FocusChanged(focused) => state.notify_focus_change(focused),
    }
}

/// `isekai_protocol::ClipboardMime`(uniffiに依存しないpure crate側の型)と
/// `crate::ClipboardMimeKind`(UniFFI境界を越える側の型)は同じ3種を表す別々の型
/// (isekai-protocolはuniffiに依存できないため)なので、この2関数で変換する。
fn clipboard_mime_kind_from_protocol(mime: isekai_protocol::ClipboardMime) -> ClipboardMimeKind {
    match mime {
        isekai_protocol::ClipboardMime::TextPlain => ClipboardMimeKind::TextPlain,
        isekai_protocol::ClipboardMime::TextHtml => ClipboardMimeKind::TextHtml,
        isekai_protocol::ClipboardMime::ImagePng => ClipboardMimeKind::ImagePng,
    }
}

fn clipboard_mime_kind_to_protocol(mime: ClipboardMimeKind) -> isekai_protocol::ClipboardMime {
    match mime {
        ClipboardMimeKind::TextPlain => isekai_protocol::ClipboardMime::TextPlain,
        ClipboardMimeKind::TextHtml => isekai_protocol::ClipboardMime::TextHtml,
        ClipboardMimeKind::ImagePng => isekai_protocol::ClipboardMime::ImagePng,
    }
}

/// `CtlMessage::ClipboardPush`の`data_b64`をデコードする。base64が不正なら`warn!`ログを
/// 出して`None`を返す(既存の「不正な入力はドロップして継続する」opportunisticな方針を
/// 維持)。テキスト系mime(`TextPlain`/`TextHtml`)はさらにUTF-8として妥当かも検証する
/// (画像はUTF-8検証の対象外——任意バイト列をそのまま運ぶ)。`session_event_loop`の
/// select!アームから切り出したもの — base64/UTF-8デコードという純粋な部分だけを、
/// `spawn_blocking`によるコールバック分岐(非同期・I/O)から分離してユニットテスト
/// 可能にする。
fn decode_clipboard_push(mime: isekai_protocol::ClipboardMime, data_b64: &str) -> Option<ClipboardPayload> {
    let decoded = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64) {
        Ok(decoded) => decoded,
        Err(e) => {
            warn!("ctl-socket: clipboard push data_b64 was not valid base64: {e}");
            return None;
        }
    };
    let is_text = matches!(mime, isekai_protocol::ClipboardMime::TextPlain | isekai_protocol::ClipboardMime::TextHtml);
    if is_text {
        if let Err(e) = std::str::from_utf8(&decoded) {
            warn!("ctl-socket: clipboard push was not valid UTF-8: {e}");
            return None;
        }
    }
    Some(ClipboardPayload { mime: clipboard_mime_kind_from_protocol(mime), data: decoded })
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
    initial_theme: Theme,
) {
    info!("session: event loop start {}x{}", init_cols, init_rows);
    let mut state = SessionState::new(init_cols as usize, init_rows as usize, initial_theme);
    let mut timer_rt = TokioTimerRuntime::<TrzszTimer>::new();

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
                    callback.on_connected(); None
                }
                Some(TransportEvent::Stdout(bytes)) => {
                    callback.on_data(bytes.clone());
                    Some(state.on_stdout(bytes))
                }
                Some(TransportEvent::Resized { cols, rows }) => {
                    info!("session: terminal resize {}x{}", cols, rows);
                    Some(state.resize(cols as usize, rows as usize))
                }
                Some(TransportEvent::ForwardStateChanged { id, state }) => {
                    callback.on_forward_state_changed(id, state); None
                }
                Some(TransportEvent::CtlMessage(msg)) => {
                    match msg {
                        isekai_protocol::CtlMessage::SetTitle { value } => {
                            Some(state.set_title_from_ctl(value))
                        }
                        isekai_protocol::CtlMessage::ClipboardPush { mime, data_b64 } => {
                            if let Some(payload) = decode_clipboard_push(mime, &data_b64) {
                                let cb = Arc::clone(&callback);
                                tokio::task::spawn_blocking(move || cb.on_clipboard_write(payload));
                            }
                            None
                        }
                        // `ClipboardPullRequest`は`transport.rs`側で応答書き込みが
                        // 必要と判定され`TransportEvent::ClipboardPullRequestOverCtl`
                        // として別途届く(下記アーム参照)ので、ここには来ない。
                        // `ClipboardPullResponse`はdevice→hostの応答そのものであり、
                        // deviceがこれを受け取ることは無い。どちらも到達したら無視するだけ。
                        isekai_protocol::CtlMessage::ClipboardPullRequest {}
                        | isekai_protocol::CtlMessage::ClipboardPullResponse { .. } => None,
                    }
                }
                Some(TransportEvent::ClipboardPullRequestOverCtl(reply)) => {
                    // tmux迂回チャンネル経由のpull要求(`ISEKAI_PIPE_DESIGN.md` §8 Epic M
                    // follow-up)。Android`ClipboardManager`読み出しは同期I/Oなので
                    // `on_host_key`/`on_agent_sign_request`と同じ`spawn_blocking`パターンで
                    // 待つ。opt-in無効/クリップボード空(`None`)なら`reply`をdropするだけ
                    // (`transport.rs`側が応答無しでチャネルを閉じる)。
                    let cb = Arc::clone(&callback);
                    tokio::task::spawn_blocking(move || {
                        if let Some(payload) = cb.on_clipboard_pull_request() {
                            let mime = clipboard_mime_kind_to_protocol(payload.mime);
                            let data_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, payload.data);
                            let _ = reply.send(isekai_protocol::CtlMessage::ClipboardPullResponse { mime, data_b64 });
                        }
                    });
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
            timer_id = timer_rt.recv() => match timer_id {
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
                    let Some(payload) = cb.on_clipboard_pull_request() else { return };
                    // OSC 52はテキスト専用プロトコル。デバイスのクリップボードが画像
                    // だった場合、生バイト列をOSC 52応答として送っても端末シェル側で
                    // 意味を成さないため、テキスト以外は「何も返さない」(機能の有無を
                    // 教えない、既存のNone時と同じ扱い)。
                    if payload.mime != ClipboardMimeKind::TextPlain {
                        return;
                    }
                    let data_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, payload.data);
                    let response = format!("\x1b]52;c;{data_b64}\x07").into_bytes();
                    let _ = tx.blocking_send(TransportCommand::WriteStdin(response));
                });
            }
        }
    }
}

/// ProcessResult をすべて処理する（タイマー・scrollback・副作用・画面更新）
fn dispatch_result(
    r: ProcessResult,
    timer_rt: &mut TokioTimerRuntime<TrzszTimer>,
    transport_cmd_tx: &tokio::sync::mpsc::Sender<TransportCommand>,
    callback: &Arc<dyn SessionCallback>,
    terminal: &Terminal,
    scrollback: &Arc<Mutex<VecDeque<Vec<TermCell>>>>,
) {
    for cmd in r.timer_cmds {
        match cmd {
            TimerCommand::Set { id, duration } => timer_rt.set_timer(id, duration),
            TimerCommand::Kill { id }           => timer_rt.kill_timer(id),
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
        callback.on_clipboard_write(ClipboardPayload { mime: ClipboardMimeKind::TextPlain, data: text.into_bytes() });
    }
}

// ── Tests ────────────────────────────────────────────────

// `handle_session_cmd`自体はSessionState上のメソッドへのルーティングのみが責務で、
// Trzsz転送FSMの状態遷移そのものはtrzsz.rsに厚いテストがあるため、ここでは
// 「正しいメソッドへ配線されているか」を薄く確認するだけに留める。
#[cfg(test)]
mod handle_session_cmd_tests {
    use super::*;

    fn fresh_state() -> SessionState {
        SessionState::new(80, 24, Theme::default())
    }

    fn assert_is_noop(result: &ProcessResult) {
        assert!(result.side_effects.is_empty());
        assert!(result.timer_cmds.is_empty());
        assert!(!result.screen_dirty);
        assert!(result.pending_rows.is_empty());
        assert!(result.pending_clipboard_write.is_none());
        assert!(!result.clipboard_pull_requested);
    }

    #[test]
    fn set_theme_routes_to_session_state_set_theme() {
        let mut state = fresh_state();
        let custom = Theme { default_fg: 0x11223344, ..Theme::default() };
        assert_ne!(state.terminal().theme(), custom);

        let result = handle_session_cmd(&mut state, SessionCmd::SetTheme(custom));

        assert_eq!(state.terminal().theme(), custom);
        assert_is_noop(&result);
    }

    #[test]
    fn focus_changed_routes_to_session_state_notify_focus_change() {
        // タスク#60: `?1004`有効時のみCSI I/CSI OがSideEffect::SendStdinとして返る
        // ことを、`handle_session_cmd`経由で確認する(未有効時はno-op)。
        let mut state = fresh_state();
        let noop = handle_session_cmd(&mut state, SessionCmd::FocusChanged(true));
        assert_is_noop(&noop);

        state.on_stdout(b"\x1b[?1004h".to_vec());
        let result = handle_session_cmd(&mut state, SessionCmd::FocusChanged(true));
        assert_eq!(result.side_effects.len(), 1);
        match &result.side_effects[0] {
            SideEffect::SendStdin(bytes) => assert_eq!(bytes, b"\x1b[I"),
            other => panic!("expected SideEffect::SendStdin, got a different variant: {:?}", std::mem::discriminant(other)),
        }
    }

    #[test]
    fn trzsz_accept_upload_routes_to_on_kotlin_accept_upload() {
        let mut state = fresh_state();

        // FSMがWaitingKotlin状態でない(=リモートからのtrzsz開始マジックを未検出)ため
        // consume()のno-opになる — ここで確認したいのはhandle_session_cmdが
        // on_kotlin_accept_uploadへ正しく引数を渡して呼び出せることだけ。
        let result = handle_session_cmd(&mut state, SessionCmd::TrzszAcceptUpload {
            transfer_id: "t1".to_string(), file_name: "f.bin".to_string(), file_size: 42, mode: 0o644,
        });

        assert_is_noop(&result);
    }

    #[test]
    fn trzsz_chunk_routes_to_on_kotlin_chunk() {
        let mut state = fresh_state();

        let result = handle_session_cmd(&mut state, SessionCmd::TrzszChunk {
            transfer_id: "t1".to_string(), data: vec![1, 2, 3], is_last: true,
        });

        assert_is_noop(&result);
    }

    #[test]
    fn trzsz_accept_download_routes_to_on_kotlin_accept_download() {
        let mut state = fresh_state();

        let result =
            handle_session_cmd(&mut state, SessionCmd::TrzszAcceptDownload { transfer_id: "t1".to_string() });

        assert_is_noop(&result);
    }

    #[test]
    fn trzsz_cancel_routes_to_on_kotlin_cancel() {
        let mut state = fresh_state();

        let result = handle_session_cmd(&mut state, SessionCmd::TrzszCancel { transfer_id: "t1".to_string() });

        assert_is_noop(&result);
    }
}

// ctl-socket forward(`ISEKAI_PIPE_DESIGN.md` §8 Epic M)経由で届く
// `CtlMessage::ClipboardPush`のdata_b64デコード。base64/UTF-8双方の不正入力を
// dropして継続する(opportunisticな)方針、およびmime別の扱い(テキストのみUTF-8検証、
// 画像は任意バイト列をそのまま通す)をカバーする。
#[cfg(test)]
mod decode_clipboard_push_tests {
    use super::decode_clipboard_push;
    use crate::{ClipboardMimeKind, ClipboardPayload};
    use isekai_protocol::ClipboardMime;

    #[test]
    fn decodes_valid_base64_utf8_text() {
        // "hello" の標準base64
        assert_eq!(
            decode_clipboard_push(ClipboardMime::TextPlain, "aGVsbG8="),
            Some(ClipboardPayload { mime: ClipboardMimeKind::TextPlain, data: b"hello".to_vec() })
        );
    }

    #[test]
    fn returns_none_on_invalid_base64() {
        assert_eq!(decode_clipboard_push(ClipboardMime::TextPlain, "not valid base64!!"), None);
    }

    #[test]
    fn returns_none_on_valid_base64_text_that_is_not_utf8() {
        // 0xFF 0xFE は単独では不正なUTF-8シーケンス。base64エンコード済み("//4=")。
        assert_eq!(decode_clipboard_push(ClipboardMime::TextPlain, "//4="), None);
    }

    #[test]
    fn decodes_empty_string_to_empty_text() {
        assert_eq!(
            decode_clipboard_push(ClipboardMime::TextPlain, ""),
            Some(ClipboardPayload { mime: ClipboardMimeKind::TextPlain, data: Vec::new() })
        );
    }

    #[test]
    fn decodes_non_utf8_bytes_for_image_mime_without_utf8_validation() {
        // 0xFF 0xFE はテキストとしては不正だが、画像mimeならバイト列としてそのまま通す。
        assert_eq!(
            decode_clipboard_push(ClipboardMime::ImagePng, "//4="),
            Some(ClipboardPayload { mime: ClipboardMimeKind::ImagePng, data: vec![0xFF, 0xFE] })
        );
    }
}

// `SessionCore::scrollback_cells`(オフセット/行数からscrollbackを切り出す表示ロジック)
// と`dispatch_result`のscrollback上限トリミングは、実SSH/QUIC接続もTokioランタイムも
// 不要な純粋なデータ変換だが、`session.rs`にはテストが1つも無かった。
#[cfg(test)]
mod tests {
    use super::*;

    fn cell(label: char) -> TermCell {
        TermCell {
            ch: smol_str::SmolStr::new(label.to_string()),
            fg: 0xFF010101,
            bg: 0xFF020202,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            strikethrough: false,
            blink: false,
            invisible: false,
            is_wide_placeholder: false,
            link_id: None,
        }
    }

    fn row(label: char, len: usize) -> Vec<TermCell> {
        vec![cell(label); len]
    }

    /// 文字列`s`の各charを1セルずつに割り当てた行を組み立てる(検索テスト用)。
    fn text_row(s: &str) -> Vec<TermCell> {
        s.chars().map(cell).collect()
    }

    #[test]
    fn make_screen_update_link_table_stays_bounded_when_remote_floods_distinct_urls() {
        // タスク#70: `make_screen_update`は`Terminal::link_table()`を`to_vec()`で
        // 丸ごと複製して`ScreenUpdate`へ載せる。リモートが相異なるOSC8 URLを
        // 上限を超えて大量に流しても、UniFFI境界を越えて公開される
        // `ScreenUpdate.link_table`が`crate::terminal::MAX_LINK_TABLE`件で
        // 頭打ちになる(=毎フレームのFFIコピーコストも無界には悪化しない)ことを
        // 確認する。
        let mut t = Terminal::new(80, 24, Theme::default());
        let mut p = vte::Parser::new();
        let flood = crate::terminal::MAX_LINK_TABLE + 500;
        for i in 0..flood {
            let seq = format!("\x1b]8;;https://flood.example/{i}\x07");
            for &b in seq.as_bytes() { p.advance(&mut t, b); }
        }

        let upd = make_screen_update(&t);
        assert_eq!(
            upd.link_table.len(),
            crate::terminal::MAX_LINK_TABLE,
            "ScreenUpdate.link_tableは上限件数で頭打ちになり無界には増えない"
        );
    }

    #[test]
    fn make_screen_update_clamps_cursor_col_during_delayed_wrap() {
        // `Terminal::cursor_col()`は遅延折り返し(delayed wrap)中`cols`(範囲外)を
        // 返しうる内部表現をそのまま公開する。`ScreenUpdate.cursor_col`はUIが
        // 直接セルインデックスとして使う描画可能な列であるべきなので、UniFFI境界を
        // 越える前にここでクランプする(タスク#56)。
        let mut t = Terminal::new(10, 3, Theme::default());
        let mut p = vte::Parser::new();
        for &b in b"0123456789" { p.advance(&mut t, b); } // ちょうど10文字でwrap-pending
        assert_eq!(t.cursor_col(), 10, "precondition: terminal is in delayed-wrap state");

        let upd = make_screen_update(&t);
        assert_eq!(upd.cursor_col, 9, "ScreenUpdate.cursor_col must be clamped to the last visible column");
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

    // ── search_scrollback(タスク#37) ────────────────────────

    #[test]
    fn search_scrollback_returns_empty_for_empty_query() {
        let core = core_with_scrollback(20, vec![text_row("hello world")]);
        assert!(core.search_scrollback("", true).is_empty());
    }

    #[test]
    fn search_scrollback_finds_match_with_row_col_len() {
        // index0 = 最新行。"hello"はcol0開始・5セル分。
        let core = core_with_scrollback(20, vec![text_row("hello world")]);
        let matches = core.search_scrollback("hello", true);
        assert_eq!(matches, vec![ScrollbackSearchMatch { row: 0, col: 0, len: 5 }]);
    }

    #[test]
    fn search_scrollback_reports_row_using_newest_first_convention() {
        // scrollback_cellsと同じ規約(index0=最新)を`row`でもそのまま使う。
        let core = core_with_scrollback(20, vec![
            text_row("newest line"), // row 0
            text_row("older line"),  // row 1
        ]);
        let matches = core.search_scrollback("line", true);
        assert_eq!(matches.len(), 2);
        assert!(matches.contains(&ScrollbackSearchMatch { row: 0, col: 7, len: 4 }));
        assert!(matches.contains(&ScrollbackSearchMatch { row: 1, col: 6, len: 4 }));
    }

    #[test]
    fn search_scrollback_case_sensitive_true_requires_exact_case() {
        let core = core_with_scrollback(20, vec![text_row("Hello World")]);
        assert!(core.search_scrollback("hello", true).is_empty());
        assert_eq!(core.search_scrollback("Hello", true), vec![
            ScrollbackSearchMatch { row: 0, col: 0, len: 5 },
        ]);
    }

    #[test]
    fn search_scrollback_case_sensitive_false_ignores_ascii_case() {
        let core = core_with_scrollback(20, vec![text_row("Hello World")]);
        assert_eq!(core.search_scrollback("hello", false), vec![
            ScrollbackSearchMatch { row: 0, col: 0, len: 5 },
        ]);
        assert_eq!(core.search_scrollback("WORLD", false), vec![
            ScrollbackSearchMatch { row: 0, col: 6, len: 5 },
        ]);
    }

    #[test]
    fn search_scrollback_finds_multiple_matches_in_same_row() {
        let core = core_with_scrollback(20, vec![text_row("abcabc")]);
        let matches = core.search_scrollback("abc", true);
        assert_eq!(matches, vec![
            ScrollbackSearchMatch { row: 0, col: 0, len: 3 },
            ScrollbackSearchMatch { row: 0, col: 3, len: 3 },
        ]);
    }

    #[test]
    fn search_scrollback_returns_no_match_when_absent() {
        let core = core_with_scrollback(20, vec![text_row("hello world")]);
        assert!(core.search_scrollback("xyz", true).is_empty());
    }

    #[test]
    fn search_scrollback_match_spanning_wide_char_reports_display_width_as_len() {
        // col0='a', col1='あ'(表示幅2、col2はそのプレースホルダ), col3='b'。
        let mut wide = cell('あ');
        wide.is_wide_placeholder = false;
        let mut placeholder = cell(' ');
        placeholder.is_wide_placeholder = true;
        let row_cells = vec![cell('a'), wide, placeholder, cell('b')];
        let core = core_with_scrollback(20, vec![row_cells]);

        // プレースホルダは検索対象文字列から除外されるため、"あb"は連続した
        // 2文字としてマッチし、幅は全角文字の2セル分を含めて3になる。
        let matches = core.search_scrollback("あb", true);
        assert_eq!(matches, vec![ScrollbackSearchMatch { row: 0, col: 1, len: 3 }]);

        // 全角文字単体の検索では、その表示幅(2)がそのままlenになる。
        let matches = core.search_scrollback("あ", true);
        assert_eq!(matches, vec![ScrollbackSearchMatch { row: 0, col: 1, len: 2 }]);
    }

    #[test]
    fn search_scrollback_treats_combining_character_cell_as_one_unit() {
        // タスク#39: 結合文字は基底文字と同じセルの`ch`へ複数コードポイントとして
        // 格納される(例: "e" + COMBINING ACUTE ACCENT)。このセルの一部だけに
        // マッチが掛かることはない。
        let mut combined = cell('e');
        combined.ch = smol_str::SmolStr::new("e\u{0301}"); // "é"(結合文字表現)
        let row_cells = vec![cell('x'), combined, cell('y')];
        let core = core_with_scrollback(20, vec![row_cells]);

        // 結合文字の片方(基底文字のみ)を検索しても、そのセル全体にしかマッチしない
        // ("y"を含めた"e\u{0301}y"全体で検索すればマッチする)。
        let matches = core.search_scrollback("e\u{0301}y", true);
        assert_eq!(matches, vec![ScrollbackSearchMatch { row: 0, col: 1, len: 2 }]);
    }

    #[test]
    fn search_scrollback_does_not_match_inside_a_combining_character_cell() {
        // Codexレビュー(タスク#37): 結合文字セルの一部だけを検索語にした場合、
        // そのセルの内部で部分マッチしてはいけない(基底文字だけの"e"や結合
        // アクセント単体の検索が、"é"を表示しているセルにヒットしてはいけない)。
        let mut combined = cell('e');
        combined.ch = smol_str::SmolStr::new("e\u{0301}"); // "é"(結合文字表現)
        let row_cells = vec![cell('x'), combined, cell('y')];
        let core = core_with_scrollback(20, vec![row_cells]);

        assert!(core.search_scrollback("e", true).is_empty(), "base charだけの部分マッチは無効");
        assert!(core.search_scrollback("\u{0301}", true).is_empty(), "結合アクセント単体の部分マッチは無効");
        // セル全体("e\u{0301}")を丸ごと検索語にすればマッチする。
        assert_eq!(core.search_scrollback("e\u{0301}", true), vec![
            ScrollbackSearchMatch { row: 0, col: 1, len: 1 },
        ]);
    }

    // ── resize: 同一値dedupe(#62) ────────────────────────────

    /// [SessionCore::resize]が`TransportCommand::Resize`を実際に送るかどうかだけを
    /// 見るための最小セットアップ。`start()`はTokioランタイム上のevent loopを
    /// spawnしてしまう(このテストの対象外)ため使わず、`handle_tx`だけを直接張る。
    fn core_with_command_channel() -> (SessionCore, tokio::sync::mpsc::Receiver<TransportCommand>) {
        let core = SessionCore::new();
        let (tx, rx) = tokio::sync::mpsc::channel::<TransportCommand>(8);
        *core.handle_tx.lock() = Some(tx);
        (core, rx)
    }

    #[test]
    fn resize_sends_command_on_first_call() {
        let (core, mut rx) = core_with_command_channel();
        core.resize(100, 40);
        match rx.try_recv().expect("should have sent a resize command") {
            TransportCommand::Resize { cols, rows } => assert_eq!((cols, rows), (100, 40)),
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn resize_is_noop_when_dims_unchanged() {
        let (core, mut rx) = core_with_command_channel();
        core.resize(100, 40);
        rx.try_recv().expect("first call should send");
        core.resize(100, 40); // 同一値の連続呼び出し(#62: IME開閉・回転等を想定)
        assert!(rx.try_recv().is_err(), "duplicate resize with identical dims must not be sent");
    }

    #[test]
    fn resize_sends_again_when_dims_change() {
        let (core, mut rx) = core_with_command_channel();
        core.resize(100, 40);
        rx.try_recv().expect("first call should send");
        core.resize(100, 41); // rowsだけ変化
        match rx.try_recv().expect("changed dims should send again") {
            TransportCommand::Resize { cols, rows } => assert_eq!((cols, rows), (100, 41)),
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn resize_matching_initial_start_dims_is_noop() {
        // start()呼び出し直後に、Kotlin/Swift側がそのまま同じサイズでresize()を
        // 呼んでも(初期レイアウト確定直後の再通知等)、変化が無いので送るべきではない。
        let core = SessionCore::new();
        *core.last_resize_dims.lock() = (100, 40); // start()相当の初期化を模倣
        let (tx, mut rx) = tokio::sync::mpsc::channel::<TransportCommand>(8);
        *core.handle_tx.lock() = Some(tx);
        core.resize(100, 40);
        assert!(rx.try_recv().is_err(), "resize matching the dims set at start() must be a no-op");
    }

    #[test]
    fn resize_that_fails_to_send_does_not_poison_dedupe_cache() {
        // チャネルが埋まっていて`try_send`が失敗した場合、実際にはリモートへ何も
        // 配送されていないので、dedupeキャッシュを更新してはいけない。更新して
        // しまうと、次に同じ(cols, rows)でresizeが来たときに「前回と同一」と
        // 誤判定されて永久にスキップされ、リモートPTYのサイズが実画面と
        // 食い違ったまま復旧できなくなる。
        let core = SessionCore::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<TransportCommand>(1);
        tx.try_send(TransportCommand::WriteStdin(Vec::new())).expect("fill the only slot");
        *core.handle_tx.lock() = Some(tx);

        core.resize(100, 40); // channel full → 送出失敗、dedupeキャッシュは更新されないはず

        rx.try_recv().expect("drain the filler command"); // 空きを作る
        core.resize(100, 40); // 同一値だが、前回は届いていないので今度こそ送るべき
        match rx.try_recv().expect("resize must be retried once space is available") {
            TransportCommand::Resize { cols, rows } => assert_eq!((cols, rows), (100, 40)),
            _ => panic!("unexpected command variant"),
        }
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
        fn on_clipboard_write(&self, _payload: crate::ClipboardPayload) {}
        fn on_clipboard_pull_request(&self) -> Option<crate::ClipboardPayload> { None }
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
        let mut timer_rt = TokioTimerRuntime::<TrzszTimer>::new();
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
