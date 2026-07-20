use vte::Parser;
use timed_fsm::{TimedStateMachine, TimerCommand, Response};
use crate::kitty_graphics::{ApcInterceptor, ApcStep};
use crate::{CellData, CursorShape, LineDamage, ScreenUpdate};
use crate::session::to_cell_data;
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

/// タスク#13でフィールドが4つ増え、各コンストラクタサイトで毎回全フィールドを
/// 書き下すのが煩雑になったため`Default`を導出する(全フィールドが空/falseを
/// 自然な既定値として持つ)。各メソッドは`ProcessResult { pending_rows, screen_dirty:
/// true, ..Default::default() }`のように必要なフィールドだけ明示すればよい。
#[derive(Default)]
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
    /// OSC 133(タスク#13)「前/次のプロンプトへジャンプ」が要求されたか。
    /// `pending_clipboard_write`と違い結果が`None`(ジャンプ先なし)でも「要求は
    /// あった」ことを呼び出し元(`dispatch_result`)へ伝える必要があるため、
    /// 専用のbooleanで分離する(`clipboard_pull_requested`と同型のパターン)。
    pub(crate) prompt_jump_requested: bool,
    /// [prompt_jump_requested]が`true`の場合の実際のジャンプ先。ジャンプ対象が
    /// 見つからなければ`None`。
    pub(crate) prompt_jump_target: Option<crate::PromptJumpTarget>,
    /// OSC 133(タスク#13)「直前コマンドの出力だけをコピー」が要求されたか。
    /// [prompt_jump_requested]と同じ理由で専用のbooleanにする。
    pub(crate) prompt_output_copy_requested: bool,
    /// [prompt_output_copy_requested]が`true`の場合の実際のテキスト。まだ完了した
    /// コマンドが無ければ`None`。
    pub(crate) prompt_output_copy_text: Option<String>,
}

/// カーソルが乗る行(`row`)の損傷レンジに、少なくともカーソル列(`col`)を含める
/// (タスク#94)。その行が既に損傷リストにあればレンジを広げ、無ければ1点だけの
/// エントリを追加する。`row`が画面範囲外(縮小直後の旧カーソル位置等)、または
/// `cols == 0`なら何もしない。
fn force_cursor_row_dirty(damages: &mut Vec<LineDamage>, row: usize, col: usize, cols: usize, rows: usize) {
    if cols == 0 || row >= rows { return; }
    let col = col.min(cols - 1) as u16;
    let line = row as u16;
    if let Some(d) = damages.iter_mut().find(|d| d.line == line) {
        d.left = d.left.min(col);
        d.right = d.right.max(col);
    } else {
        damages.push(LineDamage { line, left: col, right: col });
    }
}

// ── SessionState ─────────────────────────────────────────

/// 同期的なセッション状態機械。
/// チャネル・コールバック・Tokio に一切依存せず、単体テストから直接呼べる。
pub(crate) struct SessionState {
    terminal: Terminal,
    parser: Parser,
    /// Kitty graphics(#53)のAPC文字列を、vteへ渡す前にバイトストリームから切り出す
    /// 前段。vteはAPCを配送しないため必要(`kitty_graphics.rs`モジュールdoc参照)。
    apc: ApcInterceptor,
    fsm: TrzszTransferFsm,
    /// 前回発行した`ScreenUpdate`のグリッドセル・寸法・カーソル位置のスナップショット
    /// (タスク#92、行単位のdamage tracking用)。次に`make_screen_update`が呼ばれたとき、
    /// 現在のグリッドとこれを行ごとに比較して変化した列レンジ(`LineDamage`)を求める。
    /// `None`は「まだ一度も発行していない」状態を表し、初回は全画面dirty扱いになる。
    last_emitted_cells: Option<Vec<TermCell>>,
    last_emitted_cols: usize,
    last_emitted_rows: usize,
    /// 前回発行時のカーソル位置(タスク#94のカーソル行強制dirty化用)。カーソルが
    /// 離れた行を消せるよう、前回位置も今回位置と併せて損傷行に含める。`cursor_col`は
    /// [make_screen_update]が公開するのと同じくクランプ済みの値を保持する。
    last_cursor_row: usize,
    last_cursor_col: usize,
    /// 前回発行時のカーソル可視性(DECTCEM `CSI ?25h`/`l`)。位置が不変でも可視性だけ
    /// 切り替わった場合、下地セルは不変なのでコンテンツ差分では検出できない——
    /// カーソルを同じ描画パスでセルと一緒に描くiOS(タスク#98/#99)がカーソルの
    /// 消し忘れ/描き忘れを起こさないよう、位置と同様にこの値も比較してカーソル行を
    /// 強制dirty化する。
    last_cursor_visible: bool,
    /// 前回発行時のカーソル形状(DECSCUSR)。可視性と同じ理由(位置不変でも下地セルが
    /// 不変なコンテンツ差分では検出できない)でカーソル行の強制dirty化条件に含める。
    last_cursor_shape: CursorShape,
    /// 前回発行時のカーソル点滅モード(`CSI ?12h`/`l`・DECSCUSR)。可視性・形状と同じ
    /// 理由(Opusレビュー指摘: `CSI ?12l`/`CSI 2 q`で位置・形状不変のまま点滅だけ
    /// 切り替わるケースが可視性・形状の強制dirty化だけでは漏れていた)でカーソル行の
    /// 強制dirty化条件に含める。
    last_cursor_blink: bool,
    /// 発行するたびに単調増加する`ScreenUpdate`の連番(タスク#97/#99のCodexならぬ
    /// セルフレビューで発覚: UI層への配信チャネルが`Channel.CONFLATED`(Android)
    /// 等でconflateされる場合、中間の発行が読み飛ばされうる。`dirty_rows`は
    /// 「直前に発行したScreenUpdateとの差分」なので、発行N+1が読み飛ばされて
    /// UI側がN→N+2しか見なければ、N→N+1間の変化がdirty_rowsに載らず表示が
    /// 化ける。UI側はこの`update_seq`が前回受信値+1でなければ(=読み飛ばしが
    /// あれば)`dirty_rows`を信用せず全画面再描画にフォールバックする)。
    update_seq: u32,
}

impl SessionState {
    pub(crate) fn new(cols: usize, rows: usize, theme: Theme) -> Self {
        SessionState {
            terminal: Terminal::new(cols, rows, theme),
            parser: Parser::new(),
            apc: ApcInterceptor::new(),
            fsm: TrzszTransferFsm::new(),
            last_emitted_cells: None,
            last_emitted_cols: 0,
            last_emitted_rows: 0,
            last_cursor_row: 0,
            last_cursor_col: 0,
            last_cursor_visible: true,
            last_cursor_shape: CursorShape::Block,
            last_cursor_blink: true,
            update_seq: 0,
        }
    }

    /// 前回発行した`ScreenUpdate`との行単位差分(damage tracking)を計算し、`dirty_rows`を
    /// 添えた最新の`ScreenUpdate`を生成する(タスク#92)。呼び出し元(`session.rs`の
    /// イベントループ)は`screen_dirty`なバッチでのみ呼ぶ——画面が変化していないバッチ
    /// ではスナップショットを更新しない(前回スナップショットが有効なまま)。
    ///
    /// `dirty_rows`の意味:
    /// - `None` = 全画面損傷(初回発行・寸法変更・[Terminal::take_full_damage_pending]が
    ///   立っている構造的変更[タスク#93])。UI層はグリッド全体を再描画する。
    /// - `Some(vec)` = `vec`の各行の`[left,right]`レンジのみ再描画すればよい。損傷のない
    ///   行は含まれない。カーソル行(前回位置・今回位置)は下地セルが不変でも含める
    ///   (タスク#94)。
    pub(crate) fn make_screen_update(&mut self) -> ScreenUpdate {
        // `full_damage_pending`はワンショット——読み取ってクリアする(後続の不変借用より前に)。
        let full_damage_flag = self.terminal.take_full_damage_pending();

        let cols = self.terminal.cols();
        let rows = self.terminal.rows();
        let cursor_row = self.terminal.cursor_row();
        // `cursor_col()`は遅延折り返し中に`cols`(範囲外)を返しうる——描画可能な最終列へ
        // クランプしてから公開・損傷計算に使う(Fableレビュー: タスク#56)。
        let cursor_col = self.terminal.cursor_col().min(cols.saturating_sub(1));
        let cursor_visible = self.terminal.cursor_visible();
        let cursor_shape = self.terminal.cursor_shape();
        let cursor_blink = self.terminal.cursor_blink();

        // 現在のグリッドを1度だけ所有スナップショットへ複製する。行差分の比較コストは
        // O(rows×cols)のセル等価判定で、下の`to_cell_data`変換が毎フレーム払うコストと
        // 同オーダーなので許容する(この複製自体も同オーダー)。
        let current_cells: Vec<TermCell> = self.terminal.screen_cells().to_vec();
        let cells: Vec<CellData> = current_cells.iter().map(to_cell_data).collect();

        let dims_match = self.last_emitted_cols == cols
            && self.last_emitted_rows == rows
            && self.last_emitted_cells.as_ref().map_or(false, |c| c.len() == cols * rows);

        let dirty_rows: Option<Vec<LineDamage>> = if full_damage_flag || !dims_match {
            // 全画面損傷: 初回発行(last_emitted_cells==None → dims_match==false)・
            // 寸法変更・構造的変更(#93)。行差分は取らない。
            None
        } else {
            let prev = self.last_emitted_cells.as_ref().expect("dims_match implies Some");
            let mut damages: Vec<LineDamage> = Vec::new();
            for row in 0..rows {
                let base = row * cols;
                let mut left: Option<usize> = None;
                let mut right = 0usize;
                for col in 0..cols {
                    if current_cells[base + col] != prev[base + col] {
                        if left.is_none() { left = Some(col); }
                        right = col;
                    }
                }
                if let Some(l) = left {
                    damages.push(LineDamage { line: row as u16, left: l as u16, right: right as u16 });
                }
            }
            // カーソル行の強制dirty化(タスク#94)。iOSはカーソルをセル内容と同じ描画
            // パスで描くため、カーソルが動いたら下地セルが不変でも「離れた行(前回位置、
            // 古いカーソルを消す)」と「乗った行(今回位置、新しいカーソルを描く)」を
            // 再描画させる。位置に加えて可視性(DECTCEM)・形状(DECSCUSR)・点滅
            // (`CSI ?12h`/`l`)も比較する——`CSI ?25l`/`?25h`・形状変更・点滅切替は
            // いずれも位置が不変のままカーソルの見た目だけが切り替わるケースで、下地
            // セルも不変なのでコンテンツ差分では検出できない。これらを見落とすとiOS側
            // で消し忘れ/描き忘れが起きる(可視性・形状はセルフレビュー、点滅はOpus
            // レビューで検出)。動いても見た目が何も変わっていなければ前回フレームで
            // 既に正しく描かれているので何も足さない——これにより「画面が完全に同一の
            // 連続フレーム」は空の`dirty_rows`になる(タスク#101)。
            if (cursor_row, cursor_col, cursor_visible, cursor_shape, cursor_blink)
                != (
                    self.last_cursor_row,
                    self.last_cursor_col,
                    self.last_cursor_visible,
                    self.last_cursor_shape,
                    self.last_cursor_blink,
                )
            {
                force_cursor_row_dirty(&mut damages, self.last_cursor_row, self.last_cursor_col, cols, rows);
                force_cursor_row_dirty(&mut damages, cursor_row, cursor_col, cols, rows);
            }
            // 損傷行を行番号昇順にそろえる(消費側の決定性・テスト容易性のため)。
            damages.sort_by_key(|d| d.line);
            Some(damages)
        };

        // スナップショットは全画面損傷時も含め毎回無条件に更新する——次回の差分が
        // 正しく取れるように。
        self.last_emitted_cells = Some(current_cells);
        self.last_emitted_cols = cols;
        self.last_emitted_rows = rows;
        self.last_cursor_row = cursor_row;
        self.last_cursor_col = cursor_col;
        self.last_cursor_visible = cursor_visible;
        self.last_cursor_shape = cursor_shape;
        self.last_cursor_blink = cursor_blink;
        self.update_seq = self.update_seq.wrapping_add(1);

        let t = &self.terminal;
        ScreenUpdate {
            update_seq: self.update_seq,
            cols: cols as u32,
            rows: rows as u32,
            cells,
            cursor_row: cursor_row as u32,
            cursor_col: cursor_col as u32,
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
            dirty_rows,
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
        ProcessResult { screen_dirty: true, ..Default::default() }
    }

    /// リサイズ時に画面内容・scrollback・カーソル位置・SGR属性・scroll region等の
    /// ターミナル状態を保持したまま新しいサイズへ合わせる
    /// (`Terminal::resize_preserving_state`参照)。通常の tty resize はエスケープ
    /// シーケンスの読み取り途中を打ち切るべきイベントではないため、`self.parser`
    /// (`vte::Parser`)も作り直さず、そのまま使い続ける。
    ///
    /// rows が縮んで画面上端からはみ出た行は `pending_rows` として返す(呼び出し元が
    /// 既存の stdout scrollback フラッシュ経路にそのまま乗せられる)。`screen_dirty:
    /// true` を返すため、呼び出し元は次の stdout 受信を待たずに新サイズ・保持内容を
    /// 反映した `ScreenUpdate` を発行できる(#44)。
    pub(crate) fn resize(&mut self, cols: usize, rows: usize) -> ProcessResult {
        self.terminal.resize_preserving_state(cols, rows);
        let pending_rows = self.terminal.take_scrollback();
        ProcessResult { pending_rows, screen_dirty: true, ..Default::default() }
    }

    /// OSのフォーカス変化(タスク#60: タブ/split pane切替・アプリのbackground/foreground等)
    /// をそのまま受け取り、フォーカスレポーティング(`?1004`)が有効な場合のみ
    /// `SideEffect::SendStdin`としてエンコード済みのシーケンスを返す
    /// ([Terminal::encode_focus_event]参照)。無効時は`ProcessResult`が空のまま返る
    /// (画面には影響しないため`screen_dirty`も立てない)。
    /// DEC Synchronized Output(`?2026`)のsafety-netタイムアウト(`session.rs`)専用。
    /// `CSI ?2026l`が来ないままリモートがハングした場合、Rust側から強制的に同期
    /// 状態を解除し、直近の(部分的な)画面内容を1回flushする(`Terminal::
    /// force_end_synchronized_output`のdocコメント参照)。
    pub(crate) fn force_end_synchronized_output(&mut self) -> ProcessResult {
        self.terminal.force_end_synchronized_output();
        ProcessResult { screen_dirty: true, ..Default::default() }
    }

    pub(crate) fn notify_focus_change(&mut self, focused: bool) -> ProcessResult {
        let mut side_effects = Vec::new();
        if let Some(bytes) = self.terminal.encode_focus_event(focused) {
            side_effects.push(SideEffect::SendStdin(bytes));
        }
        ProcessResult { side_effects, ..Default::default() }
    }

    /// OSC 133(タスク#13)「前/次のプロンプトへジャンプ」。`want_previous`は
    /// true=前・false=次。`from_scroll_offset`/`from_showing_scrollback`は
    /// Kotlin側が現在表示している位置(既存の検索ジャンプ・タスク#79と同じ規約)、
    /// `scrollback_len`は呼び出し元(`session.rs`)が`scrollback`ロックから読んだ
    /// 現在のscrollback長([Terminal]自身は`SessionCore`側のトリミング後の長さを
    /// 知らないため、呼び出し元が渡す)。判断ロジック自体は
    /// [Terminal::prompt_jump_target]に一元化されている。
    pub(crate) fn jump_to_prompt(
        &self,
        want_previous: bool,
        from_scroll_offset: u32,
        from_showing_scrollback: bool,
        scrollback_len: u32,
    ) -> ProcessResult {
        let target = self.terminal.prompt_jump_target(
            want_previous, from_scroll_offset, from_showing_scrollback, scrollback_len,
        );
        ProcessResult { prompt_jump_requested: true, prompt_jump_target: target, ..Default::default() }
    }

    /// OSC 133(タスク#13): タップされたセルが現在アクティブな入力行上であれば、
    /// そこへカーソルを移動する矢印キー相当のバイト列を送る(Ghostty`cl=line`相当)。
    /// 判断ロジックは[Terminal::cursor_move_bytes_for_click]に一元化されている。
    pub(crate) fn click_to_prompt_cursor(&self, row: u32, col: u32) -> ProcessResult {
        let mut side_effects = Vec::new();
        if let Some(bytes) = self.terminal.cursor_move_bytes_for_click(row, col) {
            side_effects.push(SideEffect::SendStdin(bytes));
        }
        ProcessResult { side_effects, ..Default::default() }
    }

    /// OSC 133(タスク#13)「直前コマンドの出力だけをコピー」。判断ロジックは
    /// [Terminal::last_command_output_text]に一元化されている。
    pub(crate) fn copy_last_command_output(&self) -> ProcessResult {
        let text = self.terminal.last_command_output_text();
        ProcessResult { prompt_output_copy_requested: true, prompt_output_copy_text: text, ..Default::default() }
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
                    // Kitty graphics(#53)のAPCだけを`ApcInterceptor`で抜き取り、それ以外の
                    // バイトはバイト等価でvteへ渡す(vteはAPCを配送しないため必要)。
                    for byte in &bytes {
                        match self.apc.feed(*byte) {
                            ApcStep::Pass(b) => self.parser.advance(&mut self.terminal, b),
                            ApcStep::PassTwo(a, b) => {
                                self.parser.advance(&mut self.terminal, a);
                                self.parser.advance(&mut self.terminal, b);
                            }
                            ApcStep::Consume => {}
                            ApcStep::Apc(payload) => self.terminal.dispatch_kitty_apc(&payload),
                        }
                    }
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

        // DEC Synchronized Output(`?2026`)がアクティブな間は、実際にvteが処理した
        // 内容とは無関係に`onScreenUpdate`のpushを抑制する(scrollback/side effects
        // は通常通り流す——表示のpushだけを止める)。`?2026l`自体を処理したこの
        // `apply`呼び出しでは`synchronized_output_active`が既にfalseに戻っている
        // ため、蓄積されていた変更がここで1回にまとめてflushされる
        // (Codexレビュー: safety-netタイムアウトは別途`session.rs`が持つ)。
        let screen_dirty = screen_dirty && !self.terminal.synchronized_output_active();

        let pending_rows = self.terminal.take_scrollback();
        let pending_clipboard_write = self.terminal.take_pending_clipboard_write();
        let clipboard_pull_requested = self.terminal.take_pending_clipboard_pull_request();
        // DA/DSR/CPR応答(タスク#38)。新しいtransport経路は追加せず、既存の
        // `SideEffect::SendStdin`にそのまま乗せる(`terminal.rs`の
        // `pending_terminal_responses`フィールドdoc comment参照)。
        for resp in self.terminal.take_pending_terminal_responses() {
            side_effects.push(SideEffect::SendStdin(resp));
        }
        ProcessResult {
            timer_cmds,
            side_effects,
            pending_rows,
            screen_dirty,
            pending_clipboard_write,
            clipboard_pull_requested,
            ..Default::default()
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
    fn test_synchronized_output_suppresses_screen_dirty_while_active() {
        // `CSI ?2026h`の直後(まだ`?2026l`が来ていない)チャンクでは、実際に画面が
        // 変化していても`screen_dirty`は立たない(onScreenUpdateのpushを止める)。
        let mut state = SessionState::new(80, 24, Theme::default());
        let r = state.on_stdout(b"\x1b[?2026hhello".to_vec());
        assert!(!r.screen_dirty, "sync中はscreen_dirtyが抑制されているべき");
        // Terminalの中身自体は普通に更新されている(表示pushだけを止めている)。
        assert_eq!(state.terminal().screen_cells()[0].ch.as_str(), "h");
    }

    #[test]
    fn test_synchronized_output_flushes_once_on_end() {
        // `?2026h`〜`?2026l`が別々のon_stdout呼び出し(=別々のsocket readチャンク)に
        // 分かれていても、`?2026l`を含むチャンクで蓄積された変更が1回のflushとして
        // 反映されることを確認する。
        let mut state = SessionState::new(80, 24, Theme::default());
        let r1 = state.on_stdout(b"\x1b[?2026hhello".to_vec());
        assert!(!r1.screen_dirty);
        let r2 = state.on_stdout(b"\x1b[?2026l".to_vec());
        assert!(r2.screen_dirty, "?2026lで蓄積分がまとめてflushされるべき");
        assert_eq!(state.terminal().screen_cells()[0].ch.as_str(), "h");
    }

    #[test]
    fn test_synchronized_output_same_chunk_h_and_l_still_flushes() {
        // 同一チャンクに`h`と`l`が両方入っている場合(十分小さい再描画)、最終的に
        // inactiveへ戻るので通常通り1回flushされる(抑制されっぱなしにならない)。
        let mut state = SessionState::new(80, 24, Theme::default());
        let r = state.on_stdout(b"\x1b[?2026hhello\x1b[?2026l".to_vec());
        assert!(r.screen_dirty);
    }

    #[test]
    fn test_force_end_synchronized_output_forces_flush() {
        // safety-netタイムアウト(`session.rs`)がこのメソッドを呼ぶ経路。sync中で
        // 抑制されていた画面を強制的に1回flushし、以降は通常通りdirtyが伝播する。
        let mut state = SessionState::new(80, 24, Theme::default());
        let r1 = state.on_stdout(b"\x1b[?2026hhello".to_vec());
        assert!(!r1.screen_dirty);

        let r2 = state.force_end_synchronized_output();
        assert!(r2.screen_dirty);
        assert!(!state.terminal().synchronized_output_active());

        // 強制解除後は普通にdirtyが伝播する(抑制されたまま固まらない)。
        let r3 = state.on_stdout(b"world".to_vec());
        assert!(r3.screen_dirty);
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
    fn test_cpr_request_produces_send_stdin_side_effect() {
        // タスク#38: CSI 6n(CPR)が SessionState::on_stdout 経由で
        // SideEffect::SendStdin に変換され、既存の応答送信経路にそのまま乗ることを
        // 確認する(新しいtransport経路を追加しない設計)。
        let mut state = SessionState::new(80, 24, Theme::default());
        let r = state.on_stdout(b"\x1b[6n".to_vec());
        assert_eq!(r.side_effects.len(), 1);
        match &r.side_effects[0] {
            SideEffect::SendStdin(bytes) => assert_eq!(bytes, b"\x1b[1;1R"),
            other => panic!("expected SideEffect::SendStdin, got a different variant: {:?}", std::mem::discriminant(other)),
        }
    }

    #[test]
    fn test_notify_focus_change_noop_when_mode_off() {
        // タスク#60: `?1004`が有効化されていなければ何も送らない。
        let mut state = SessionState::new(80, 24, Theme::default());
        let r = state.notify_focus_change(true);
        assert!(r.side_effects.is_empty());
        assert!(!r.screen_dirty);
    }

    #[test]
    fn test_notify_focus_change_produces_send_stdin_when_mode_on() {
        // タスク#60: `?1004`有効時、OS由来のフォーカス変化がCSI I/CSI Oとして
        // 既存のSideEffect::SendStdin経路にそのまま乗る。
        let mut state = SessionState::new(80, 24, Theme::default());
        state.on_stdout(b"\x1b[?1004h".to_vec());
        let gained = state.notify_focus_change(true);
        assert_eq!(gained.side_effects.len(), 1);
        match &gained.side_effects[0] {
            SideEffect::SendStdin(bytes) => assert_eq!(bytes, b"\x1b[I"),
            other => panic!("expected SideEffect::SendStdin, got a different variant: {:?}", std::mem::discriminant(other)),
        }
        let lost = state.notify_focus_change(false);
        assert_eq!(lost.side_effects.len(), 1);
        match &lost.side_effects[0] {
            SideEffect::SendStdin(bytes) => assert_eq!(bytes, b"\x1b[O"),
            other => panic!("expected SideEffect::SendStdin, got a different variant: {:?}", std::mem::discriminant(other)),
        }
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
    fn test_resize_preserves_screen_content() {
        let mut state = SessionState::new(80, 24, Theme::default());
        let _ = state.on_stdout(b"hello".to_vec());
        assert_eq!(state.terminal().screen_cells()[0].ch.as_str(), "h");
        state.resize(40, 12);
        assert_eq!(state.terminal().cols(), 40);
        assert_eq!(state.terminal().rows(), 12);
        // リサイズ後も画面内容は保持される("消去"されない)。
        assert_eq!(state.terminal().screen_cells()[0].ch.as_str(), "h");
    }

    #[test]
    fn test_resize_returns_screen_dirty_process_result() {
        // #44: resize直後にScreenUpdateを発行できるよう、resize()自体が
        // screen_dirty: true な ProcessResult を返す。
        let mut state = SessionState::new(80, 24, Theme::default());
        let r = state.resize(40, 12);
        assert!(r.screen_dirty);
        assert!(r.side_effects.is_empty());
        assert!(r.timer_cmds.is_empty());
    }

    #[test]
    fn test_resize_shrinking_rows_pushes_overflow_to_pending_rows() {
        // rows が縮んで画面上端からはみ出た行は pending_rows(→呼び出し元がscrollbackへ
        // 積む)として返る(xterm挙動)。
        let mut state = SessionState::new(10, 5, Theme::default());
        for i in 0..5u8 {
            let _ = state.on_stdout(format!("line{}\r\n", i).into_bytes());
        }
        let r = state.resize(10, 2);
        assert!(!r.pending_rows.is_empty());
    }

    #[test]
    fn test_resize_preserves_sgr_attributes() {
        let mut state = SessionState::new(80, 24, Theme::default());
        let _ = state.on_stdout(b"\x1b[31mred".to_vec());
        let red_fg = state.terminal().screen_cells()[0].fg;
        state.resize(40, 12);
        let _ = state.on_stdout(b"\rmore".to_vec());
        // resize前に設定したSGR(赤字)が、resize後の新規出力にも引き継がれている。
        assert_eq!(state.terminal().screen_cells()[0].fg, red_fg);
    }

    #[test]
    fn test_resize_does_not_reset_in_progress_escape_sequence() {
        // VTEパーサーの状態(パーサーを作り直していないこと)の検証: エスケープ
        // シーケンスを2つのバッチに分割して送り、間でresizeを挟んでも正しく解釈される。
        let mut state = SessionState::new(80, 24, Theme::default());
        let _ = state.on_stdout(b"\x1b[3".to_vec()); // "\x1b[31m" の途中で中断
        state.resize(40, 12);
        let _ = state.on_stdout(b"1mA".to_vec()); // 残りを送る
        let c = &state.terminal().screen_cells()[0];
        assert_eq!(c.ch.as_str(), "A");
        assert_eq!(c.fg, Theme::default().ansi16[1]); // 赤 = SGR 31 が正しく解釈された
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

        /// resize 後もサイズ・カーソル不変量が成立する(画面内容・パーサー状態を
        /// 保持しつつリサイズする新仕様)
        #[test]
        fn prop_resize_then_invariants(
            before in proptest::collection::vec(any::<u8>(), 0..256),
            cols in 10usize..120,
            rows in 4usize..40,
            after in proptest::collection::vec(any::<u8>(), 0..256),
        ) {
            let mut state = SessionState::new(80, 24, Theme::default());
            let _ = state.on_stdout(before);
            let r = state.resize(cols, rows);
            prop_assert!(r.screen_dirty);
            let _ = state.on_stdout(after);
            let t = state.terminal();
            prop_assert_eq!(t.cols(), cols);
            prop_assert_eq!(t.rows(), rows);
            prop_assert_eq!(t.screen_cells().len(), cols * rows);
            prop_assert!(t.cursor_row() < t.rows());
            prop_assert!(t.cursor_col() <= t.cols());
            // resize由来のpending_rowsは旧cols幅のまま(scrollback_cellsが
            // row.len().min(cols)でclip+padして吸収する設計 — session.rs参照)なので、
            // ここでは新colsとの一致は assert しない。
        }
    }
}
