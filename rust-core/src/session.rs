use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use parking_lot::Mutex;
use timed_fsm::TimerCommand;

use crate::{CellData, ClipboardMimeKind, ClipboardPayload, ScreenUpdate, ScrollbackSearchMatch, SessionCallback, RUNTIME};
use crate::session_state::{ProcessResult, SessionState, SideEffect};
use crate::terminal::TermCell;
use crate::theme::Theme;
use crate::transport::{SessionCmd, TransportCommand, TransportEvent};
use crate::trzsz::{TrzszMode, TrzszTimer};

const SCROLLBACK_LIMIT: usize = 1000;

/// DEC Synchronized Output(`?2026`)のsafety-netタイムアウト。リモートが
/// `CSI ?2026h`を送ったまま`CSI ?2026l`を送らずハングした場合、これだけ経過
/// したら強制的に同期状態を解除して直近の画面内容をflushする(さもないと画面が
/// 永久に固まって見える)。値はモバイル/ネットワーク越しの体感を踏まえた
/// 端末機能としての経験則(codexとの設計相談での合意値)。
const SYNC_OUTPUT_SAFETY_TIMEOUT: Duration = Duration::from_millis(250);

/// safety-netタイマーの発火通知(`fired_generation`)が、現在armされている
/// タイマーそのものからのものかどうかを判定する(codexレビュー指摘のstale通知
/// race対策——`session_event_loop`内の`sync_output_timeout_rx`アームdocコメント
/// 参照)。ロジックをここに切り出すのは、tokio spawnを伴わずに単体テストできる
/// ようにするため。
fn sync_output_timeout_is_current(fired_generation: u64, armed_generation: Option<u64>) -> bool {
    armed_generation == Some(fired_generation)
}

// ── RepaintThrottle ──────────────────────────────────────

/// `make_screen_update`計算 + `on_screen_update`コールバック発火の頻度を、
/// ディスプレイの実効フレームレート程度に間引くための状態機械
/// (kittyの`repaint_delay`相当。参照: PLAN.md 該当Phase)。VTEパース自体
/// (`state.on_stdout`)は間引かない——端末状態は常に最新でなければならず、
/// 間引いてよいのは「画面をUI/UniFFI越しに反映する頻度」だけ。
///
/// リーディングエッジ(アイドル直後の1発は即時発行)+トレーリングエッジ
/// (連続dirty中は最後の1回だけ`min_interval`後に発行)方式。これにより通常の
/// タイピングのようなスパースな入力では追加レイテンシがゼロのまま、flood時
/// だけ間引きが効く。
struct RepaintThrottle {
    min_interval: Duration,
    last_emit: Option<Instant>,
    armed_deadline: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
enum RepaintDecision {
    /// アイドル状態からのdirty。即座に`make_screen_update`+コールバックしてよい。
    EmitNow,
    /// `min_interval`内の2回目以降のdirty。指定deadlineでタイマーをarmすること。
    Arm(Instant),
    /// 既にタイマーがarmされている。何もしなくてよい(発火を待つ)。
    AlreadyArmed,
}

const REPAINT_MIN_INTERVAL: Duration = Duration::from_millis(16);

impl RepaintThrottle {
    fn new(min_interval: Duration) -> Self {
        RepaintThrottle { min_interval, last_emit: None, armed_deadline: None }
    }

    /// `screen_dirty`なバッチを処理した直後に呼ぶ。
    fn on_dirty(&mut self, now: Instant) -> RepaintDecision {
        if self.armed_deadline.is_some() {
            return RepaintDecision::AlreadyArmed;
        }
        match self.last_emit {
            Some(t) if now < t + self.min_interval => {
                let deadline = t + self.min_interval;
                self.armed_deadline = Some(deadline);
                RepaintDecision::Arm(deadline)
            }
            _ => RepaintDecision::EmitNow,
        }
    }

    /// 実際に`on_screen_update`を発行した直後に呼ぶ(`EmitNow`経路・タイマー
    /// 発火経路の両方から)。
    fn note_emitted(&mut self, now: Instant) {
        self.last_emit = Some(now);
        self.armed_deadline = None;
    }

    fn timer_armed(&self) -> bool {
        self.armed_deadline.is_some()
    }
}

impl Default for RepaintThrottle {
    fn default() -> Self {
        RepaintThrottle::new(REPAINT_MIN_INTERVAL)
    }
}

#[cfg(test)]
mod repaint_throttle_tests {
    use super::*;

    #[test]
    fn idle_dirty_emits_immediately() {
        let mut t = RepaintThrottle::new(Duration::from_millis(16));
        let now = Instant::now();
        assert_eq!(t.on_dirty(now), RepaintDecision::EmitNow);
    }

    #[test]
    fn second_dirty_within_min_interval_arms_remaining_time() {
        let mut t = RepaintThrottle::new(Duration::from_millis(16));
        let t0 = Instant::now();
        assert_eq!(t.on_dirty(t0), RepaintDecision::EmitNow);
        t.note_emitted(t0);

        let t1 = t0 + Duration::from_millis(5);
        let deadline = t0 + Duration::from_millis(16);
        assert_eq!(t.on_dirty(t1), RepaintDecision::Arm(deadline));
    }

    #[test]
    fn dirty_while_already_armed_is_a_noop_decision() {
        let mut t = RepaintThrottle::new(Duration::from_millis(16));
        let t0 = Instant::now();
        t.on_dirty(t0);
        t.note_emitted(t0);

        let t1 = t0 + Duration::from_millis(3);
        t.on_dirty(t1); // arms
        assert!(t.timer_armed());

        let t2 = t0 + Duration::from_millis(9);
        assert_eq!(t.on_dirty(t2), RepaintDecision::AlreadyArmed);
    }

    #[test]
    fn note_emitted_disarms_and_next_dirty_after_interval_emits_immediately() {
        let mut t = RepaintThrottle::new(Duration::from_millis(16));
        let t0 = Instant::now();
        t.on_dirty(t0);
        t.note_emitted(t0);

        let t1 = t0 + Duration::from_millis(5);
        t.on_dirty(t1); // arms
        assert!(t.timer_armed());

        let fire_at = t0 + Duration::from_millis(16);
        t.note_emitted(fire_at);
        assert!(!t.timer_armed());

        let t2 = fire_at + Duration::from_millis(17);
        assert_eq!(t.on_dirty(t2), RepaintDecision::EmitNow);
    }
}

// ── TermCell → 公開型変換（session 層の責務）────────────

pub(crate) fn to_cell_data(c: &TermCell) -> CellData {
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

    /// タスク#17: `TransportCommand::FilePreviewExec`を1本キューイングする。
    /// `run_ssh_channel_loop`は全トランスポート共通(SSH直結・tsshd QUIC・
    /// isekai-pipe QUIC系いずれもこのループ経由でSSHチャネルを持つ、
    /// `transport/ssh_handler.rs`のモジュールdoc参照)なので、`add_local_forward`と
    /// 違いトランスポートごとの対応可否分岐は不要——`command_sender()`が生きて
    /// いれば常に対応できる。未接続/切断済みで`command_sender()`が無い場合のみ
    /// `false`を返す。
    pub(crate) fn file_preview_exec(&self, request_id: String, command_line: String) -> bool {
        if let Some(tx) = self.command_sender() {
            tx.try_send(TransportCommand::FilePreviewExec { request_id, command_line }).is_ok()
        } else {
            false
        }
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

    /// OSC 133(タスク#13)「前のプロンプトへジャンプ」。`from_scroll_offset`/
    /// `from_showing_scrollback`はKotlin側の現在の表示位置(既存の検索ジャンプ・
    /// タスク#79と同じ規約)。判断ロジックは`session_event_loop`側
    /// (`Terminal::prompt_jump_target`)に一元化されており、結果は
    /// `OrchestratorCallback::on_prompt_jump`で非同期に返る。
    pub(crate) fn jump_to_previous_prompt(&self, from_scroll_offset: u32, from_showing_scrollback: bool) {
        self.send_session_cmd(SessionCmd::PromptJumpPrevious { from_scroll_offset, from_showing_scrollback });
    }

    /// [jump_to_previous_prompt]の「次」版。
    pub(crate) fn jump_to_next_prompt(&self, from_scroll_offset: u32, from_showing_scrollback: bool) {
        self.send_session_cmd(SessionCmd::PromptJumpNext { from_scroll_offset, from_showing_scrollback });
    }

    /// OSC 133(タスク#13): タップされたセル(画面座標、0-indexed)が現在アクティブな
    /// 入力行上であれば、そこへカーソルを移動する矢印キー相当のバイト列を送る
    /// (Ghostty`cl=line`相当)。対象外なら無音でno-op。
    pub(crate) fn click_to_prompt_cursor(&self, row: u32, col: u32) {
        self.send_session_cmd(SessionCmd::ClickToPromptCursor { row, col });
    }

    /// OSC 133(タスク#13)「直前コマンドの出力だけをコピー」。結果は
    /// `OrchestratorCallback::on_prompt_output_copy_ready`で非同期に返る
    /// (該当コマンドがまだ無ければ`None`)。
    pub(crate) fn copy_last_command_output(&self) {
        self.send_session_cmd(SessionCmd::CopyLastCommandOutput);
    }
}

/// Kotlin/Swift側から送られてきた`SessionCmd`を`SessionState`に適用する。
/// `session_event_loop`の`select!`アームから切り出したもの
/// (select → match(cmd) → match(c)という三重ネストを避けるため)。
///
/// `scrollback_len`はOSC 133(タスク#13)の「前/次のプロンプトへジャンプ」だけが
/// 使う(`Terminal`自身は`SessionCore`側でトリミングされた後の実際のscrollback長を
/// 知らないため、呼び出し元[`session_event_loop`]が`scrollback`ロックから読んで
/// 渡す)。他のコマンドは無視する——呼び出しごとに軽いロックが1回増えるだけなので、
/// コマンド種別で分岐して省略するより毎回渡す方が単純。
fn handle_session_cmd(state: &mut SessionState, c: SessionCmd, scrollback_len: u32) -> ProcessResult {
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
            ProcessResult::default()
        }
        SessionCmd::FocusChanged(focused) => state.notify_focus_change(focused),
        SessionCmd::PromptJumpPrevious { from_scroll_offset, from_showing_scrollback } =>
            state.jump_to_prompt(true, from_scroll_offset, from_showing_scrollback, scrollback_len),
        SessionCmd::PromptJumpNext { from_scroll_offset, from_showing_scrollback } =>
            state.jump_to_prompt(false, from_scroll_offset, from_showing_scrollback, scrollback_len),
        SessionCmd::ClickToPromptCursor { row, col } =>
            state.click_to_prompt_cursor(row, col),
        SessionCmd::CopyLastCommandOutput =>
            state.copy_last_command_output(),
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

/// `dispatch_transport_event`の戻り値。`TransportEvent::Disconnected`/
/// event_rx正常close相当のケースを`break 'outer`できるよう、通常の
/// `Option<ProcessResult>`とは別に`Break`を持つ。
enum EventOutcome {
    Continue(Option<ProcessResult>),
    Break,
}

/// 1件の`TransportEvent`を処理する(`session_event_loop`の`select!`の
/// `event_rx.recv()`アームから切り出したもの)。
///
/// `TransportEvent::Stdout`の場合は、`event_rx`に既に届いている後続の連続
/// `Stdout`をノンブロッキングの`try_recv`で吸い出し1本のバイト列に連結してから
/// `state.on_stdout`/`callback.on_data`を1回だけ呼ぶ(kittyの`input_delay`相当の
/// バッチ化——このプロジェクトは既にイベント化済みのためタイマーは使わず、
/// 既にキューに積まれている分だけを遅延ゼロで束ねる)。drain中に非`Stdout`を
/// 引いてしまった場合は`pending_event`へ退避し、呼び出し元が次のループ先頭で
/// `select!`より先に処理することで元の到着順序([Stdout, Resized, Stdout]等)を
/// 厳密に保つ。
fn dispatch_transport_event(
    event: TransportEvent,
    event_rx: &mut tokio::sync::mpsc::Receiver<TransportEvent>,
    pending_event: &mut Option<TransportEvent>,
    state: &mut SessionState,
    callback: &Arc<dyn SessionCallback>,
) -> EventOutcome {
    match event {
        TransportEvent::HostKey(fp, reply_tx) => {
            let cb = Arc::clone(callback);
            tokio::task::spawn_blocking(move || {
                let accepted = cb.on_host_key(fp);
                let _ = reply_tx.send(accepted);
            });
            EventOutcome::Continue(None)
        }
        TransportEvent::AgentSignRequest { key_fingerprint, reply } => {
            let cb = Arc::clone(callback);
            tokio::task::spawn_blocking(move || {
                let approved = cb.on_agent_sign_request(key_fingerprint);
                let _ = reply.send(approved);
            });
            EventOutcome::Continue(None)
        }
        TransportEvent::Connected => {
            callback.on_connected();
            EventOutcome::Continue(None)
        }
        TransportEvent::Stdout(bytes) => {
            let mut combined = bytes;
            loop {
                match event_rx.try_recv() {
                    Ok(TransportEvent::Stdout(more)) => combined.extend_from_slice(&more),
                    Ok(other) => {
                        *pending_event = Some(other);
                        break;
                    }
                    Err(_) => break, // Empty または Disconnected — どちらもdrain終了でよい
                }
            }
            callback.on_data(combined.clone());
            EventOutcome::Continue(Some(state.on_stdout(combined)))
        }
        TransportEvent::Resized { cols, rows } => {
            info!("session: terminal resize {}x{}", cols, rows);
            EventOutcome::Continue(Some(state.resize(cols as usize, rows as usize)))
        }
        TransportEvent::ForwardStateChanged { id, state: fwd_state } => {
            callback.on_forward_state_changed(id, fwd_state);
            EventOutcome::Continue(None)
        }
        TransportEvent::CtlMessage(msg) => match msg {
            isekai_protocol::CtlMessage::SetTitle { value } => {
                EventOutcome::Continue(Some(state.set_title_from_ctl(value)))
            }
            isekai_protocol::CtlMessage::ClipboardPush { mime, data_b64 } => {
                if let Some(payload) = decode_clipboard_push(mime, &data_b64) {
                    let cb = Arc::clone(callback);
                    tokio::task::spawn_blocking(move || cb.on_clipboard_write(payload));
                }
                EventOutcome::Continue(None)
            }
            // `ClipboardPullRequest`は`transport.rs`側で応答書き込みが必要と判定され
            // `TransportEvent::ClipboardPullRequestOverCtl`として別途届く(下のアーム参照)
            // ので、ここには来ない。`ClipboardPullResponse`はdevice→hostの応答そのもの
            // であり、deviceがこれを受け取ることは無い。どちらも到達したら無視するだけ。
            //
            // `SetVar`/`GetVarRequest`(task #16)も同様にここには来ない設計:
            // `ssh_handler::run_ssh_channel_loop`のctl_rx消費タスクがKotlin側の
            // 非同期往復を要さず(メモリ上の`CtlVarStore`参照のみ)その場で処理し切る
            // ため、`TransportEvent::CtlMessage`としてこの`session_event_loop`まで
            // 転送されることが無い。`GetVarResponse`はdevice→hostの応答そのもので
            // あり、`ClipboardPullResponse`同様deviceが受け取ることは無い。
            //
            // `BuildRequest`/`BuildOutputChunk`/`BuildFinished`(Epic P、
            // リモート発ビルドトリガー)はAndroid/iOS本体アプリでは意図的に
            // 未サポート — スマホ上にビルドツールチェーンは無く、Phase 1の
            // スコープは`isekai-ssh`(デスクトップCLIラッパー)のみ
            // (`ISEKAI_PIPE_DESIGN.md` §8 Epic P)。
            //
            // すべて到達したら無視するだけの防御的なアーム。
            isekai_protocol::CtlMessage::ClipboardPullRequest {}
            | isekai_protocol::CtlMessage::ClipboardPullResponse { .. }
            | isekai_protocol::CtlMessage::SetVar { .. }
            | isekai_protocol::CtlMessage::GetVarRequest { .. }
            | isekai_protocol::CtlMessage::GetVarResponse { .. }
            | isekai_protocol::CtlMessage::BuildRequest { .. }
            | isekai_protocol::CtlMessage::BuildOutputChunk { .. }
            | isekai_protocol::CtlMessage::BuildFinished { .. } => EventOutcome::Continue(None),
        },
        TransportEvent::ClipboardPullRequestOverCtl(reply) => {
            // tmux迂回チャンネル経由のpull要求(`ISEKAI_PIPE_DESIGN.md` §8 Epic M
            // follow-up)。Android`ClipboardManager`読み出しは同期I/Oなので
            // `on_host_key`/`on_agent_sign_request`と同じ`spawn_blocking`パターンで待つ。
            // opt-in無効/クリップボード空(`None`)なら`reply`をdropするだけ
            // (`transport.rs`側が応答無しでチャネルを閉じる)。
            let cb = Arc::clone(callback);
            tokio::task::spawn_blocking(move || {
                if let Some(payload) = cb.on_clipboard_pull_request() {
                    let mime = clipboard_mime_kind_to_protocol(payload.mime);
                    let data_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, payload.data);
                    let _ = reply.send(isekai_protocol::CtlMessage::ClipboardPullResponse { mime, data_b64 });
                }
            });
            EventOutcome::Continue(None)
        }
        TransportEvent::Disconnected { reason } => {
            info!("session: disconnected reason={:?}", reason);
            callback.on_disconnected(reason);
            EventOutcome::Break
        }
        TransportEvent::NoViablePath => {
            info!("session: no viable path (all paths unhealthy)");
            callback.on_no_viable_path();
            EventOutcome::Continue(None)
        }
        TransportEvent::FilePreviewExecResult { request_id, stdout, exit_status } => {
            callback.on_file_preview_exec_result(request_id, stdout, exit_status);
            EventOutcome::Continue(None)
        }
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
    let (timeout_tx, mut timeout_rx) = tokio::sync::mpsc::channel::<TrzszTimer>(16);
    let mut timer_rt = TokioTimerRuntime::new(timeout_tx);
    // DEC Synchronized Output(`?2026`)のsafety-netタイマー。`TokioTimerRuntime`は
    // trzsz FSM専用の単一スロット実装なので使い回さず、同じ形の別スロットを持つ
    // (codexとの設計相談: 複数タイマー種別を汎化するのはタイマー種別が増える見込みが
    // 出てから)。`sync_output_timer_handle`は「現在armされているsafety-netタイマー」
    // そのもの——ループ末尾でTerminal側の実際の状態と突き合わせてarm/disarmする。
    //
    // タイマー発火通知に`()`ではなくgeneration番号(`u64`)を積むのは、以下のstale
    // 通知raceを防ぐため(2周目のcodexレビューで指摘): safety-netタイマーが発火して
    // `sync_timeout_tx`へ送信した直後(まだ`sync_timeout_rx`側で受信していない時点)に
    // `CSI ?2026l`がstdoutから届いて処理されると、そちらのイテレーションで
    // `sync_output_timer_handle.abort()`しても、channel容量1に既に積まれてしまった
    // 発火通知そのものは取り消せない。generationを見ずに素朴に
    // `force_end_synchronized_output`を呼ぶと、その直後に始まった**別の**
    // `?2026h`区間まで誤って強制終了させてしまう。`sync_output_armed_generation`が
    // 現在armされているタイマーのgenerationを保持し、[sync_output_timeout_is_current]
    // が一致するものだけを本物の発火として扱う。
    let (sync_timeout_tx, mut sync_timeout_rx) = tokio::sync::mpsc::channel::<u64>(1);
    let mut sync_output_timer_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut sync_output_timer_generation: u64 = 0;
    let mut sync_output_armed_generation: Option<u64> = None;

    // 画面反映(`make_screen_update`+`on_screen_update`)の間引き(kittyの
    // `repaint_delay`相当、[RepaintThrottle]参照)。armされたら`repaint`を
    // deadlineへ`reset`し、`select!`のrepaintアームが発火したときだけ
    // 実際に画面を発行する。`sleep(0)`で初期化(未armなので`timer_armed()`
    // ガードにより最初のポーリングでは発火しない)。
    let mut repaint_throttle = RepaintThrottle::default();
    let repaint_timer = tokio::time::sleep(Duration::from_secs(0));
    tokio::pin!(repaint_timer);

    // `dispatch_transport_event`が連続`Stdout`をdrainする際に引いてしまった
    // 非`Stdout`イベントの退避スロット(1件のみ)。次のループ先頭で
    // `select!`より先に消費し、元の到着順序を保つ。
    let mut pending_event: Option<TransportEvent> = None;

    'outer: loop {
        let result: Option<ProcessResult> = if let Some(ev) = pending_event.take() {
            match dispatch_transport_event(ev, &mut event_rx, &mut pending_event, &mut state, &callback) {
                EventOutcome::Continue(r) => r,
                EventOutcome::Break => break 'outer,
            }
        } else {
            tokio::select! {
            event = event_rx.recv() => match event {
                Some(ev) => match dispatch_transport_event(ev, &mut event_rx, &mut pending_event, &mut state, &callback) {
                    EventOutcome::Continue(r) => r,
                    EventOutcome::Break => break 'outer,
                },
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
                // OSC 133(タスク#13)の「前/次のプロンプトへジャンプ」だけが実際に
                // 使う値だが、`handle_session_cmd`の外(このループ)でしか
                // `scrollback`ロックを取れないため、コマンド種別を問わず毎回渡す
                // (`handle_session_cmd`のdocコメント参照)。
                Some(c) => Some(handle_session_cmd(&mut state, c, scrollback.lock().len() as u32)),
                None => None,
            },
            fired = sync_timeout_rx.recv() => match fired {
                Some(gen) if sync_output_timeout_is_current(gen, sync_output_armed_generation) => {
                    warn!("session: ?2026 (synchronized output) safety-net timeout fired, forcing flush");
                    Some(state.force_end_synchronized_output())
                }
                // stale通知(既に?2026l/RIS/直前のforce_endでこのgenerationは
                // disarm済み)。無視する——`sync_output_timeout_is_current`のdocコメント
                // 参照。
                Some(_) => None,
                None => None,
            },
            () = &mut repaint_timer, if repaint_throttle.timer_armed() => {
                emit_screen_update(&mut state, &callback, &mut repaint_throttle, Instant::now());
                None
            }
            }
        };

        // DEC Synchronized Output(`?2026`)のsafety-netタイマーのarm/disarm。上の
        // `select!`のどのアームが発火したかに関わらず、`Terminal`の実際の状態
        // (vteが`?2026h`/`?2026l`を処理した結果、または直前のforce_end)と
        // 突き合わせて一元的に判断する——`state.on_stdout`/`handle_session_cmd`等
        // 個々の呼び出し側に判断を分散させない。
        let sync_active = state.terminal().synchronized_output_active();
        if sync_active {
            if sync_output_timer_handle.is_none() {
                sync_output_timer_generation += 1;
                let gen = sync_output_timer_generation;
                sync_output_armed_generation = Some(gen);
                let tx = sync_timeout_tx.clone();
                sync_output_timer_handle = Some(tokio::spawn(async move {
                    tokio::time::sleep(SYNC_OUTPUT_SAFETY_TIMEOUT).await;
                    let _ = tx.send(gen).await;
                }));
            }
        } else {
            if let Some(h) = sync_output_timer_handle.take() {
                h.abort();
            }
            sync_output_armed_generation = None;
        }

        if let Some(r) = result {
            let clipboard_pull_requested = r.clipboard_pull_requested;
            // 画面反映(`make_screen_update`+`on_screen_update`)は
            // `RepaintThrottle`(kittyの`repaint_delay`相当)で間引く——`state`が
            // `screen_dirty`を返した全バッチが即座に発行されるわけではない。
            // `screen_dirty`でないバッチではスナップショットを更新しない
            // (画面が変化していない=前回スナップショットが有効なため)。
            // タイマー・scrollback・side effects・clipboard(`dispatch_result`が
            // 処理するもの)はこの間引きの対象外で、従来どおり即時処理する。
            if r.screen_dirty {
                match repaint_throttle.on_dirty(Instant::now()) {
                    RepaintDecision::EmitNow => {
                        emit_screen_update(&mut state, &callback, &mut repaint_throttle, Instant::now());
                    }
                    RepaintDecision::Arm(deadline) => {
                        repaint_timer.as_mut().reset(tokio::time::Instant::from_std(deadline));
                    }
                    RepaintDecision::AlreadyArmed => {}
                }
            }
            dispatch_result(r, &mut timer_rt, &transport_cmd_tx, &callback, &scrollback);
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

/// `make_screen_update`を計算し`on_screen_update`コールバックへ渡す。
/// `RepaintThrottle`が`EmitNow`を返した経路(`session_event_loop`本体)と、
/// armされたタイマーが発火した経路(`select!`内)の両方から呼ばれる——画面発行は
/// 必ずこの関数を通り、`throttle.note_emitted`で発行時刻を記録する。
fn emit_screen_update(
    state: &mut SessionState,
    callback: &Arc<dyn SessionCallback>,
    throttle: &mut RepaintThrottle,
    now: Instant,
) {
    let upd = state.make_screen_update();
    debug!("screen: update {}x{} cursor=({},{}) dirty_rows={:?}",
        upd.cols, upd.rows, upd.cursor_col, upd.cursor_row,
        upd.dirty_rows.as_ref().map(Vec::len));
    callback.on_screen_update(upd);
    throttle.note_emitted(now);
}

/// ProcessResult をすべて処理する（タイマー・scrollback・副作用）。画面更新の
/// 発行は`RepaintThrottle`により間引かれるため、ここでは扱わない
/// (`emit_screen_update`を参照、呼び出し元の`session_event_loop`が別途呼ぶ)。
fn dispatch_result(
    r: ProcessResult,
    timer_rt: &mut TokioTimerRuntime,
    transport_cmd_tx: &tokio::sync::mpsc::Sender<TransportCommand>,
    callback: &Arc<dyn SessionCallback>,
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

    // OSC 52 クリップボード書き込み(`ISEKAI_PIPE_DESIGN.md` §8 Epic M)。opt-inかどうかの
    // 判断はKotlin側(`TerminalSession`)に委ねる——ここは「リモートがこう要求した」という
    // 事実をそのまま伝えるだけで、適用するかどうかの分岐はRust側に持ち込まない
    // (`.claude/rules/rust-ssot.md`が対象にしているのはセッション/プロトコル状態であり、
    // これは単なるイベント通知)。
    if let Some(text) = r.pending_clipboard_write {
        callback.on_clipboard_write(ClipboardPayload { mime: ClipboardMimeKind::TextPlain, data: text.into_bytes() });
    }

    // OSC 133(タスク#13)。どちらも「要求されたバッチでだけコールバックを呼ぶ」
    // (`prompt_jump_target`/`prompt_output_copy_text`自体は「ジャンプ先/直前出力が
    // 無かった」場合`None`もあり得るため、専用のrequestedフラグで区別する
    // ——`ProcessResult`の同名フィールドdocコメント参照)。
    if r.prompt_jump_requested {
        callback.on_prompt_jump(r.prompt_jump_target);
    }
    if r.prompt_output_copy_requested {
        callback.on_prompt_output_copy_ready(r.prompt_output_copy_text);
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
        assert!(!result.prompt_jump_requested);
        assert!(result.prompt_jump_target.is_none());
        assert!(!result.prompt_output_copy_requested);
        assert!(result.prompt_output_copy_text.is_none());
    }

    #[test]
    fn set_theme_routes_to_session_state_set_theme() {
        let mut state = fresh_state();
        let custom = Theme { default_fg: 0x11223344, ..Theme::default() };
        assert_ne!(state.terminal().theme(), custom);

        let result = handle_session_cmd(&mut state, SessionCmd::SetTheme(custom), 0);

        assert_eq!(state.terminal().theme(), custom);
        assert_is_noop(&result);
    }

    #[test]
    fn focus_changed_routes_to_session_state_notify_focus_change() {
        // タスク#60: `?1004`有効時のみCSI I/CSI OがSideEffect::SendStdinとして返る
        // ことを、`handle_session_cmd`経由で確認する(未有効時はno-op)。
        let mut state = fresh_state();
        let noop = handle_session_cmd(&mut state, SessionCmd::FocusChanged(true), 0);
        assert_is_noop(&noop);

        state.on_stdout(b"\x1b[?1004h".to_vec());
        let result = handle_session_cmd(&mut state, SessionCmd::FocusChanged(true), 0);
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
        }, 0);

        assert_is_noop(&result);
    }

    #[test]
    fn trzsz_chunk_routes_to_on_kotlin_chunk() {
        let mut state = fresh_state();

        let result = handle_session_cmd(&mut state, SessionCmd::TrzszChunk {
            transfer_id: "t1".to_string(), data: vec![1, 2, 3], is_last: true,
        }, 0);

        assert_is_noop(&result);
    }

    #[test]
    fn trzsz_accept_download_routes_to_on_kotlin_accept_download() {
        let mut state = fresh_state();

        let result =
            handle_session_cmd(&mut state, SessionCmd::TrzszAcceptDownload { transfer_id: "t1".to_string() }, 0);

        assert_is_noop(&result);
    }

    #[test]
    fn trzsz_cancel_routes_to_on_kotlin_cancel() {
        let mut state = fresh_state();

        let result = handle_session_cmd(&mut state, SessionCmd::TrzszCancel { transfer_id: "t1".to_string() }, 0);

        assert_is_noop(&result);
    }

    // ── OSC 133(タスク#13) ──────────────────────────────────

    #[test]
    fn prompt_jump_previous_routes_to_session_state_and_reports_requested() {
        let mut state = fresh_state();

        let result = handle_session_cmd(
            &mut state,
            SessionCmd::PromptJumpPrevious { from_scroll_offset: 0, from_showing_scrollback: false },
            0,
        );

        // まだプロンプトマークが1つも無いので見つからないが、「要求はあった」
        // ことは呼び出し元(`dispatch_result`)がコールバックを呼べるよう伝わる。
        assert!(result.prompt_jump_requested);
        assert!(result.prompt_jump_target.is_none());
    }

    #[test]
    fn prompt_jump_next_routes_to_session_state_and_reports_requested() {
        let mut state = fresh_state();

        let result = handle_session_cmd(
            &mut state,
            SessionCmd::PromptJumpNext { from_scroll_offset: 0, from_showing_scrollback: false },
            0,
        );

        assert!(result.prompt_jump_requested);
        assert!(result.prompt_jump_target.is_none());
    }

    #[test]
    fn click_to_prompt_cursor_routes_to_session_state_and_is_noop_when_no_active_input_line() {
        let mut state = fresh_state();

        let result = handle_session_cmd(&mut state, SessionCmd::ClickToPromptCursor { row: 0, col: 5 }, 0);

        assert_is_noop(&result);
    }

    #[test]
    fn copy_last_command_output_routes_to_session_state_and_reports_requested() {
        let mut state = fresh_state();

        let result = handle_session_cmd(&mut state, SessionCmd::CopyLastCommandOutput, 0);

        assert!(result.prompt_output_copy_requested);
        assert!(result.prompt_output_copy_text.is_none());
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
    use crate::LineDamage;

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
    fn sync_output_timeout_is_current_only_matches_currently_armed_generation() {
        assert!(sync_output_timeout_is_current(1, Some(1)));
        assert!(
            !sync_output_timeout_is_current(1, Some(2)),
            "古いgenerationの発火通知は、既に新しいgenerationがarmされていれば無視すべき"
        );
        assert!(
            !sync_output_timeout_is_current(1, None),
            "既にdisarm済み(?2026l/RIS/直前のforce_end)なら無視すべき"
        );
    }

    #[test]
    fn make_screen_update_link_table_stays_bounded_when_remote_floods_distinct_urls() {
        // タスク#70: `make_screen_update`は`Terminal::link_table()`を`to_vec()`で
        // 丸ごと複製して`ScreenUpdate`へ載せる。リモートが相異なるOSC8 URLを
        // 上限を超えて大量に流しても、UniFFI境界を越えて公開される
        // `ScreenUpdate.link_table`が`crate::terminal::MAX_LINK_TABLE`件で
        // 頭打ちになる(=毎フレームのFFIコピーコストも無界には悪化しない)ことを
        // 確認する。
        let mut state = SessionState::new(80, 24, Theme::default());
        let flood = crate::terminal::MAX_LINK_TABLE + 500;
        let mut bytes = Vec::new();
        for i in 0..flood {
            bytes.extend_from_slice(format!("\x1b]8;;https://flood.example/{i}\x07").as_bytes());
        }
        state.on_stdout(bytes);

        let upd = state.make_screen_update();
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
        let mut state = SessionState::new(10, 3, Theme::default());
        state.on_stdout(b"0123456789".to_vec()); // ちょうど10文字でwrap-pending
        assert_eq!(state.terminal().cursor_col(), 10, "precondition: terminal is in delayed-wrap state");

        let upd = state.make_screen_update();
        assert_eq!(upd.cursor_col, 9, "ScreenUpdate.cursor_col must be clamped to the last visible column");
    }

    // ── 行単位 dirty diff(タスク#92-95, #101)────────────────

    #[test]
    fn dirty_rows_is_none_on_first_emit() {
        // 初回発行は前回スナップショットが無いので全画面dirty(=None)。
        let mut state = SessionState::new(80, 24, Theme::default());
        let upd = state.make_screen_update();
        assert!(upd.dirty_rows.is_none(), "初回は全画面dirty(None)であるべき");
    }

    #[test]
    fn dirty_rows_is_empty_when_screen_and_cursor_unchanged() {
        // 2回連続で完全に同一の画面(カーソルも静止)を発行したら、2回目は損傷行が空。
        let mut state = SessionState::new(80, 24, Theme::default());
        let _ = state.make_screen_update(); // スナップショット確定(None)
        let upd = state.make_screen_update();
        assert_eq!(upd.dirty_rows, Some(vec![]), "無変化フレームは空のdirty_rows");
    }

    #[test]
    fn update_seq_increments_monotonically_across_emits() {
        // 配信チャネルがconflateされた場合(Android Channel.CONFLATED等)にUI層が
        // 読み飛ばしを検知できるよう、発行のたびに単調増加する(セルフレビューで
        // 発覚したconflated-channel問題の修正)。
        let mut state = SessionState::new(80, 24, Theme::default());
        let u0 = state.make_screen_update();
        let u1 = state.make_screen_update();
        let u2 = state.make_screen_update();
        assert_eq!(u0.update_seq, 1, "0始まりでwrapping_add(1)するので初回発行はseq=1");
        assert_eq!(u1.update_seq, 2);
        assert_eq!(u2.update_seq, 3);
    }

    #[test]
    fn dirty_rows_single_cell_change_is_one_tight_range() {
        // row5 col0 に1文字だけ書き、カーソルは元の位置(home)へ戻す。損傷は
        // その1セル(left==right)のみで、カーソル移動由来の余計な行は付かない。
        let mut state = SessionState::new(80, 24, Theme::default());
        let _ = state.make_screen_update();
        state.on_stdout(b"\x1b[6;1HX\x1b[1;1H".to_vec()); // row5 col0 に'X'、その後 home へ復帰
        let upd = state.make_screen_update();
        assert_eq!(
            upd.dirty_rows,
            Some(vec![LineDamage { line: 5, left: 0, right: 0 }]),
            "変化した1セルだけがtightな損傷レンジになる"
        );
    }

    #[test]
    fn dirty_rows_multi_row_change() {
        // row3 と row7 に3文字ずつ書き、カーソルは home へ戻す。損傷は2行、各 [0,2]。
        let mut state = SessionState::new(80, 24, Theme::default());
        let _ = state.make_screen_update();
        state.on_stdout(b"\x1b[4;1HAAA\x1b[8;1HBBB\x1b[1;1H".to_vec());
        let upd = state.make_screen_update();
        assert_eq!(
            upd.dirty_rows,
            Some(vec![
                LineDamage { line: 3, left: 0, right: 2 },
                LineDamage { line: 7, left: 0, right: 2 },
            ]),
            "変化した2行だけが行番号昇順で損傷レンジになる"
        );
    }

    #[test]
    fn dirty_rows_cursor_only_move_marks_prev_and_new_rows() {
        // セル内容は一切変えず、カーソルだけ (0,0) → (row4,col2) へ動かす。iOS向けに
        // 「離れた行(row0)」と「乗った行(row4)」の両方が損傷行に載る(タスク#94)。
        let mut state = SessionState::new(80, 24, Theme::default());
        let _ = state.make_screen_update(); // スナップショット確定(cursor (0,0))
        state.on_stdout(b"\x1b[5;3H".to_vec()); // CUP: row4 col2 へ(印字なし=セル不変)
        let upd = state.make_screen_update();
        assert_eq!(
            upd.dirty_rows,
            Some(vec![
                LineDamage { line: 0, left: 0, right: 0 }, // 前回カーソル行(消す)
                LineDamage { line: 4, left: 2, right: 2 }, // 今回カーソル行(描く)
            ]),
            "カーソルのみ移動でも前回位置と今回位置の両行が損傷になる"
        );
    }

    #[test]
    fn dirty_rows_cursor_visibility_only_toggle_marks_cursor_row() {
        // カーソル位置は不変のまま、DECTCEM(`CSI ?25l`)で可視性だけを切り替える。
        // 下地セルは不変なのでコンテンツ差分では検出できないが、カーソル行は
        // 強制dirty化されなければならない(セルフレビューで検出したギャップの回帰テスト:
        // 元実装は位置のみ比較していたため、可視性だけの変化を見落としていた)。
        let mut state = SessionState::new(80, 24, Theme::default());
        let _ = state.make_screen_update(); // スナップショット確定(cursor (0,0), visible)
        state.on_stdout(b"\x1b[?25l".to_vec()); // カーソル非表示、位置は不変
        let upd = state.make_screen_update();
        assert_eq!(
            upd.dirty_rows,
            Some(vec![LineDamage { line: 0, left: 0, right: 0 }]),
            "位置が不変でも可視性トグルだけでカーソル行がdirtyになるべき"
        );
    }

    #[test]
    fn dirty_rows_is_none_on_resize() {
        // リサイズは寸法が変わる(かつ full_damage_pending も立つ)ので全画面dirty。
        let mut state = SessionState::new(80, 24, Theme::default());
        let _ = state.make_screen_update();
        let _ = state.resize(100, 30);
        let upd = state.make_screen_update();
        assert!(upd.dirty_rows.is_none(), "リサイズは全画面dirty(None)");
    }

    #[test]
    fn dirty_rows_is_none_on_scroll_up_region() {
        // SU(`CSI S`)は行座標をずらすので、内容差分ではなく明示フラグで全画面dirty
        // (タスク#93)。空画面でも(全行 blank→blank で内容は不変でも)Noneになるのが
        // 「内容ベースでなく構造イベントベース」であることの確認。
        let mut state = SessionState::new(80, 24, Theme::default());
        let _ = state.make_screen_update();
        state.on_stdout(b"\x1b[S".to_vec());
        let upd = state.make_screen_update();
        assert!(upd.dirty_rows.is_none(), "SUは全画面dirty(None)");
    }

    #[test]
    fn dirty_rows_is_none_on_insert_lines() {
        let mut state = SessionState::new(80, 24, Theme::default());
        let _ = state.make_screen_update();
        state.on_stdout(b"\x1b[L".to_vec()); // IL
        let upd = state.make_screen_update();
        assert!(upd.dirty_rows.is_none(), "ILは全画面dirty(None)");
    }

    #[test]
    fn dirty_rows_is_none_on_delete_lines() {
        let mut state = SessionState::new(80, 24, Theme::default());
        let _ = state.make_screen_update();
        state.on_stdout(b"\x1b[M".to_vec()); // DL
        let upd = state.make_screen_update();
        assert!(upd.dirty_rows.is_none(), "DLは全画面dirty(None)");
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
        let (timeout_tx, _timeout_rx) = tokio::sync::mpsc::channel(1);
        let mut timer_rt = TokioTimerRuntime::new(timeout_tx);
        let callback: Arc<dyn SessionCallback> = Arc::new(NoopSessionCallback);

        let result = ProcessResult {
            pending_rows: vec![row('N', 1), row('N', 1), row('N', 1)], // 3行新規追加
            ..Default::default()
        };
        // dispatch_resultはscrollback/side effectsのみを扱う(画面発行は
        // emit_screen_update側の責務なのでここでは検証不要)。
        dispatch_result(result, &mut timer_rt, &transport_cmd_tx, &callback, &scrollback);

        let sb = scrollback.lock();
        assert_eq!(sb.len(), SCROLLBACK_LIMIT, "should be capped at SCROLLBACK_LIMIT, not left at +3 over");
        assert!(
            sb.iter().all(|r| r[0].ch != "Z"),
            "the oldest row (back of the deque) must be the one evicted, not an arbitrary one"
        );
    }
}

/// `dispatch_transport_event`のStdout drain連結(kittyの`input_delay`相当)の
/// ユニットテスト。実async loopは不要——`tokio::sync::mpsc`の`try_send`/
/// `try_recv`は同期メソッドなので、関数を直接同期的に呼んで検証できる。
#[cfg(test)]
mod dispatch_transport_event_tests {
    use super::*;

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

    fn fresh_state() -> SessionState {
        SessionState::new(80, 24, Theme::default())
    }

    fn visible_text(upd: &ScreenUpdate, len: usize) -> String {
        upd.cells[0..len].iter().map(|c| c.ch.as_str()).collect()
    }

    #[test]
    fn consecutive_queued_stdout_is_concatenated_into_a_single_on_stdout_call() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<TransportEvent>(8);
        // "ab"は直接dispatchする引数、"cd"/"ef"は既にキューに積まれている想定。
        tx.try_send(TransportEvent::Stdout(b"cd".to_vec())).unwrap();
        tx.try_send(TransportEvent::Stdout(b"ef".to_vec())).unwrap();

        let mut state = fresh_state();
        let callback: Arc<dyn SessionCallback> = Arc::new(NoopSessionCallback);
        let mut pending: Option<TransportEvent> = None;

        let outcome = dispatch_transport_event(
            TransportEvent::Stdout(b"ab".to_vec()), &mut rx, &mut pending, &mut state, &callback,
        );

        assert!(matches!(outcome, EventOutcome::Continue(Some(_))), "Stdout must produce a ProcessResult");
        assert!(pending.is_none(), "no non-Stdout event was queued, so nothing should be stashed");
        assert!(rx.try_recv().is_err(), "the queued Stdout events must be fully drained, not left behind");

        let upd = state.make_screen_update();
        assert_eq!(
            visible_text(&upd, 6), "abcdef",
            "three queued Stdout chunks must be applied in order as if concatenated into one on_stdout call"
        );
    }

    #[test]
    fn non_stdout_event_encountered_while_draining_is_stashed_and_stops_the_drain() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<TransportEvent>(8);
        // [Stdout("before"), Resized, Stdout("after")]という到着順を想定。
        tx.try_send(TransportEvent::Resized { cols: 100, rows: 40 }).unwrap();
        tx.try_send(TransportEvent::Stdout(b"after".to_vec())).unwrap();

        let mut state = fresh_state();
        let callback: Arc<dyn SessionCallback> = Arc::new(NoopSessionCallback);
        let mut pending: Option<TransportEvent> = None;

        let outcome = dispatch_transport_event(
            TransportEvent::Stdout(b"before".to_vec()), &mut rx, &mut pending, &mut state, &callback,
        );
        assert!(matches!(outcome, EventOutcome::Continue(Some(_))));
        assert!(
            matches!(pending, Some(TransportEvent::Resized { cols: 100, rows: 40 })),
            "Resized must be stashed into pending_event rather than silently dropped or merged into the Stdout batch"
        );
        // Resizedより後ろの2件目のStdoutは、Resizedを飛び越えてdrainされてはならない
        // (順序保全)。まだキューに残っているはず。
        match rx.try_recv() {
            Ok(TransportEvent::Stdout(b)) => assert_eq!(b, b"after"),
            Ok(_) => panic!("expected the second Stdout to remain queued, got a different TransportEvent variant"),
            Err(e) => panic!("expected the second Stdout to remain queued, got error: {e:?}"),
        }

        // 呼び出し元(session_event_loop本番コード)は次にpending_eventを最優先で処理する。
        let resize_event = pending.take().expect("pending_event was asserted Some above");
        let outcome2 = dispatch_transport_event(resize_event, &mut rx, &mut pending, &mut state, &callback);
        assert!(matches!(outcome2, EventOutcome::Continue(Some(_))));
        assert!(pending.is_none(), "Resized does not itself drain further events");

        assert_eq!(state.terminal().cols(), 100, "resize must be applied before the later Stdout, preserving order");
        assert_eq!(state.terminal().rows(), 40);
    }
}

/// `session_event_loop`本体への`RepaintThrottle`統合(kittyの`repaint_delay`
/// 相当)の決定的な統合テスト。`tokio::time::pause`で仮想時間を制御し、
/// 「アイドル時は即時発行(タイピングレイテンシ回帰なし)」「flood時は間引かれる」
/// 「発行された`update_seq`は単調増加で欠けが無い」を検証する。
#[cfg(test)]
mod session_event_loop_tests {
    use super::*;

    struct RecordingSessionCallback {
        updates: Mutex<Vec<ScreenUpdate>>,
    }

    impl RecordingSessionCallback {
        fn new() -> Self {
            RecordingSessionCallback { updates: Mutex::new(Vec::new()) }
        }
    }

    impl SessionCallback for RecordingSessionCallback {
        fn on_data(&self, _data: Vec<u8>) {}
        fn on_host_key(&self, _fingerprint: String) -> bool { true }
        fn on_connected(&self) {}
        fn on_disconnected(&self, _reason: Option<String>) {}
        fn on_screen_update(&self, update: ScreenUpdate) { self.updates.lock().push(update); }
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

    /// アイドル状態(直前の発行から`REPAINT_MIN_INTERVAL`以上経過)でのstdoutは、
    /// リーディングエッジとして即座に1回`on_screen_update`が発行されること
    /// (タイピング直後のエコー表示にタイマー分の追加レイテンシが乗らないことの
    /// 回帰防止)。
    #[tokio::test(start_paused = true)]
    async fn idle_stdout_emits_screen_update_immediately() {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel::<TransportEvent>(64);
        let (_session_cmd_tx, session_cmd_rx) = tokio::sync::mpsc::channel(1);
        let (transport_cmd_tx, _transport_cmd_rx) = tokio::sync::mpsc::channel(8);
        let callback = Arc::new(RecordingSessionCallback::new());
        let scrollback = Arc::new(Mutex::new(VecDeque::new()));

        let cb_for_loop: Arc<dyn SessionCallback> = callback.clone();
        let handle = tokio::spawn(session_event_loop(
            event_rx, session_cmd_rx, transport_cmd_tx, cb_for_loop, scrollback,
            80, 24, Theme::default(),
        ));

        event_tx.send(TransportEvent::Stdout(b"hello".to_vec())).await.unwrap();
        // イベントループが1周してemit_screen_update(EmitNow経路)を実行するのを待つ。
        // 仮想時間は進めない(=アイドル即時発行がタイマー待ちに頼っていないことの確認)。
        for _ in 0..100 {
            tokio::task::yield_now().await;
            if !callback.updates.lock().is_empty() {
                break;
            }
        }

        assert_eq!(
            callback.updates.lock().len(), 1,
            "idle stdout must emit immediately without waiting for the repaint timer"
        );

        drop(event_tx);
        let _ = handle.await;
    }

    /// `REPAINT_MIN_INTERVAL`より短い間隔で大量のstdoutが届くflood
    /// (catで巨大ファイルを吐き出すシナリオ相当)では、`on_screen_update`の
    /// 発行回数が投入イベント数を大きく下回ること、かつ発行された
    /// `update_seq`が単調増加で欠けが無いことを検証する。
    #[tokio::test(start_paused = true)]
    async fn flood_within_min_interval_coalesces_to_far_fewer_emits() {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel::<TransportEvent>(4096);
        let (_session_cmd_tx, session_cmd_rx) = tokio::sync::mpsc::channel(1);
        let (transport_cmd_tx, _transport_cmd_rx) = tokio::sync::mpsc::channel(8);
        let callback = Arc::new(RecordingSessionCallback::new());
        let scrollback = Arc::new(Mutex::new(VecDeque::new()));

        let cb_for_loop: Arc<dyn SessionCallback> = callback.clone();
        let handle = tokio::spawn(session_event_loop(
            event_rx, session_cmd_rx, transport_cmd_tx, cb_for_loop, scrollback,
            80, 24, Theme::default(),
        ));

        // 最初の1発はアイドルからのリーディングエッジ発行でbaselineを作ってから、
        // min_interval内に収まる密度で大量に投入する。
        event_tx.send(TransportEvent::Stdout(b"start\r\n".to_vec())).await.unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;

        let n: usize = 500;
        for i in 0..n {
            event_tx.send(TransportEvent::Stdout(format!("line{i}\r\n").into_bytes())).await.unwrap();
        }
        // repaint_delay(REPAINT_MIN_INTERVAL)を跨いで、armされたトレーリング
        // エッジの発行が起きるまで仮想時間を進める。
        tokio::time::advance(REPAINT_MIN_INTERVAL * 3).await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }

        let updates = callback.updates.lock();
        assert!(
            updates.len() < n,
            "expected far fewer on_screen_update calls ({}) than flooded events ({n})",
            updates.len(),
        );
        assert!(
            updates.len() >= 2,
            "expected at least the leading-edge emit plus a trailing coalesced emit, got {}",
            updates.len(),
        );

        let seqs: Vec<u32> = updates.iter().map(|u| u.update_seq).collect();
        for w in seqs.windows(2) {
            assert!(
                w[1] == w[0].wrapping_add(1),
                "update_seq must increase by exactly 1 per emitted update (no gaps among emitted frames): {seqs:?}"
            );
        }
        drop(updates);

        drop(event_tx);
        let _ = handle.await;
    }
}
