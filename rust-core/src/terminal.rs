use vte::Perform;
use crate::theme::Theme;

/// `38;5;n` / `48;5;n`（indexed color）を ARGB へ解決する。
/// `0..=15` は現在のテーマの ANSI 16色テーブルを参照する（それ以外は固定の 216色/グレースケール）。
fn ansi256_to_argb(theme: &Theme, n: u8) -> u32 {
    match n {
        0..=15  => theme.ansi16[n as usize],
        16..=231 => {
            let n = n as u32 - 16;
            let r = (n / 36) * 51;
            let g = ((n % 36) / 6) * 51;
            let b = (n % 6) * 51;
            0xFF000000 | (r << 16) | (g << 8) | b
        }
        232..=255 => {
            let v = 8 + (n as u32 - 232) * 10;
            0xFF000000 | (v << 16) | (v << 8) | v
        }
    }
}

#[derive(Clone)]
pub(crate) struct TermCell {
    pub(crate) ch: smol_str::SmolStr,
    pub(crate) fg: u32,
    pub(crate) bg: u32,
    pub(crate) bold: bool,
}

/// 純粋な VTE 端末状態機械。外部の Arc/Mutex を一切持たない。
/// スクロールアウトした行は `pending_scrollback` に積み、
/// 呼び出し元が `take_scrollback()` でフラッシュする。
pub(crate) struct Terminal {
    /// このセッション(タブ)固有のテーマスナップショット。Phase 12: per-session theme。
    /// `set_theme()`で明示的に更新されるまで変わらない(グローバルの`theme::current()`を
    /// 都度読みには行かない)。
    theme: Theme,
    cols: usize,
    rows: usize,
    main_cells: Vec<TermCell>,
    alt_cells: Vec<TermCell>,
    alt_active: bool,
    saved_cursor_main: Option<(usize, usize, u32, u32, bool)>,
    saved_cursor_alt: Option<(usize, usize, u32, u32, bool)>,
    cursor_row: usize,
    cursor_col: usize,
    cur_fg: u32,
    cur_bg: u32,
    cur_bold: bool,
    scroll_top: usize,
    scroll_bottom: usize,
    title: Option<String>,
    /// リモートが OSC 52 (`ESC]52;c;<base64>BEL`) でクリップボードへの書き込みを要求した
    /// 場合、次に`take_pending_clipboard_write()`が呼ばれるまで保持する
    /// (`ISEKAI_PIPE_DESIGN.md` §8 Epic M: tmuxが`set-titles`/`allow-passthrough`を
    /// 適切に設定していれば、control-plane機構を使わずこの標準OSC経路だけで動く)。
    pending_clipboard_write: Option<String>,
    /// リモートが OSC 52 query(`ESC]52;c;?BEL`)でクリップボードの読み出しを要求した
    /// 場合に立つ。実際の応答(device→hostへの base64 OSC 52 フレーム送信)は
    /// このモジュールの外(`session_state.rs`/`session.rs`)が担う——
    /// Android のクリップボード内容を取得するには非同期の Kotlin 往復が要るため、
    /// この同期的な VTE コールバックの中では完結できない。
    pending_clipboard_pull_request: bool,
    pending_scrollback: Vec<Vec<TermCell>>,
    application_cursor_mode: bool,
    bracketed_paste_mode: bool,
}

impl Terminal {
    /// `theme`はこのセッション(タブ)が使う配色のスナップショット。呼び出し元
    /// (`SessionState`/`SessionCore`)が「グローバル既定を使うか、プロファイル/タブ固有の
    /// 上書きを使うか」を解決した結果をそのまま渡す。
    pub(crate) fn new(cols: usize, rows: usize, theme: Theme) -> Self {
        let blank = TermCell { ch: smol_str::SmolStr::new_inline(" "), fg: theme.default_fg, bg: theme.default_bg, bold: false };
        let cells = vec![blank.clone(); cols * rows];
        Terminal {
            theme,
            cols, rows,
            main_cells: cells.clone(),
            alt_cells: cells,
            alt_active: false,
            saved_cursor_main: None,
            saved_cursor_alt: None,
            cursor_row: 0, cursor_col: 0,
            cur_fg: theme.default_fg, cur_bg: theme.default_bg, cur_bold: false,
            scroll_top: 0, scroll_bottom: rows - 1,
            title: None,
            pending_clipboard_write: None,
            pending_clipboard_pull_request: false,
            pending_scrollback: Vec::new(),
            application_cursor_mode: false,
            bracketed_paste_mode: false,
        }
    }

    /// このセッションのテーマを差し替える。以降に解決される SGR にのみ反映され、
    /// 既に解決済みのセル(画面上・scrollback上とも)は遡って再着色されない。
    pub(crate) fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// 現在のテーマのスナップショット([resize_preserving_state]後も変わらない)。
    pub(crate) fn theme(&self) -> Theme {
        self.theme
    }

    /// スクロールアウトした行を取り出す。呼び出し後はバッファが空になる。
    pub(crate) fn take_scrollback(&mut self) -> Vec<Vec<TermCell>> {
        std::mem::take(&mut self.pending_scrollback)
    }

    /// 保留中の OSC 52 クリップボード書き込みを取り出す。呼び出し後は空になる
    /// (`take_scrollback`と同じ「1バッチ分をここでフラッシュする」パターン)。
    pub(crate) fn take_pending_clipboard_write(&mut self) -> Option<String> {
        self.pending_clipboard_write.take()
    }

    /// 保留中の OSC 52 クリップボード読み出し要求を取り出す(1回きり、trueだった場合は
    /// falseにリセットされる)。
    pub(crate) fn take_pending_clipboard_pull_request(&mut self) -> bool {
        std::mem::take(&mut self.pending_clipboard_pull_request)
    }

    pub(crate) fn cols(&self) -> usize { self.cols }
    pub(crate) fn rows(&self) -> usize { self.rows }
    pub(crate) fn cursor_row(&self) -> usize { self.cursor_row }
    pub(crate) fn cursor_col(&self) -> usize { self.cursor_col }
    pub(crate) fn title(&self) -> Option<&str> { self.title.as_deref() }

    /// OSC 0/2 のパース経由ではなく、外部(tmux迂回control-plane、`ISEKAI_PIPE_DESIGN.md`
    /// §8 Epic M)から直接タイトルを設定する。
    pub(crate) fn set_title(&mut self, title: String) {
        self.title = Some(title);
    }
    pub(crate) fn application_cursor_mode(&self) -> bool { self.application_cursor_mode }
    pub(crate) fn bracketed_paste_mode(&self) -> bool { self.bracketed_paste_mode }
    pub(crate) fn screen_cells(&self) -> &[TermCell] { self.cells() }

    fn reset_all(&mut self) {
        let theme = self.theme;
        let blank = TermCell { ch: smol_str::SmolStr::new_inline(" "), fg: theme.default_fg, bg: theme.default_bg, bold: false };
        let cells = vec![blank; self.cols * self.rows];
        self.main_cells = cells.clone();
        self.alt_cells = cells;
        self.alt_active = false;
        self.saved_cursor_main = None;
        self.saved_cursor_alt = None;
        self.cursor_row = 0; self.cursor_col = 0;
        self.cur_fg = theme.default_fg; self.cur_bg = theme.default_bg; self.cur_bold = false;
        self.scroll_top = 0; self.scroll_bottom = self.rows - 1;
        self.title = None;
        self.pending_clipboard_write = None;
        self.pending_clipboard_pull_request = false;
        self.application_cursor_mode = false;
        self.bracketed_paste_mode = false;
    }

    fn cells(&self) -> &Vec<TermCell> {
        if self.alt_active { &self.alt_cells } else { &self.main_cells }
    }

    fn cells_mut(&mut self) -> &mut Vec<TermCell> {
        if self.alt_active { &mut self.alt_cells } else { &mut self.main_cells }
    }

    fn blank(&self) -> TermCell {
        TermCell { ch: smol_str::SmolStr::new_inline(" "), fg: self.cur_fg, bg: self.cur_bg, bold: false }
    }

    fn cell_mut(&mut self, row: usize, col: usize) -> &mut TermCell {
        let cols = self.cols;
        &mut self.cells_mut()[row * cols + col]
    }

    /// リサイズ時に画面内容(main/alt screen とも)・カーソル位置・保存カーソル・現在の
    /// SGR属性・scroll region・application cursor mode・bracketed paste mode・title・
    /// 保留中のクリップボード状態を保持しつつ、新しい `new_cols`×`new_rows` にリサイズ
    /// する(tty の通常の resize は画面内容を消去すべきイベントではない)。
    ///
    /// - 列(cols)方向は reflow しない: `TermCell` は「その行が直前行から折り返された
    ///   結果か、独立した論理行か」を記録していないため、安全に re-wrap できない。
    ///   縮む場合は各行の右側をクリップし、広がる場合は右側を現在の空白色でパディングする。
    /// - 行(rows)方向、縮む場合: 各画面(main/alt)は「その画面のカーソル行がちょうど
    ///   新しい最終行に収まる分だけ」上端から行を取り除く(xterm がウィンドウを縦に
    ///   縮めた時、カーソルの行を可視範囲に保ったまま内容を上へ押し出す挙動と同じ)。
    ///   カーソルが画面の上の方にあり新サイズにそもそも収まるなら、上端からは何も
    ///   取り除かない(単純に古い内容を先頭から`new_rows`行だけ残す — top-left
    ///   アンカー)。取り除いた分だけでは`new_rows`に届かない場合、余った下端の行は
    ///   (カーソルより下の空白であることが通常なので) scrollback を経由せず単に破棄
    ///   する。上端から取り除いた行のうち main screen(`main_cells`)分だけは(xterm
    ///   挙動に合わせ) `pending_scrollback` へ push する(呼び出し元が
    ///   `take_scrollback()` で回収する)。これは現在 alt screen を表示中
    ///   (`alt_active`)でも行う — main screen 自体は裏で保持され続けており、alt から
    ///   抜けた時に見えるべき履歴を失わないようにするため。alt screen(`alt_cells`)
    ///   自体は実端末同様 scrollback を持たないため、alt 側で取り除いた行は単に破棄
    ///   する。広がる場合は下端を空白行でパディングする(カーソル位置は不変)。
    ///
    /// VTE パーサー(`vte::Parser`)の状態はこのメソッドの外(呼び出し元)で保持される —
    /// 通常の tty resize はエスケープシーケンスの読み取り途中を打ち切るべきイベントでは
    /// ないため、`Parser` を作り直さないこと。
    pub(crate) fn resize_preserving_state(&mut self, new_cols: usize, new_rows: usize) {
        // 呼び出し元(Android/iOS)は現状もっと大きい下限(例: 10x5)を強制しているが、
        // Terminal 自身の不変量(`cursor_row < rows`等)を呼び出し元の実装に依存させない
        // よう、ここでも最低 1x1 を保証する(0を渡されると `self.rows - 1` 等で
        // underflow する)。
        let new_cols = new_cols.max(1);
        let new_rows = new_rows.max(1);

        if new_cols == self.cols && new_rows == self.rows {
            return;
        }

        let old_cols = self.cols;
        let old_rows = self.rows;
        let total_removed = old_rows.saturating_sub(new_rows);
        let blank = self.blank();
        // 全画面がデフォルトのscroll region(`0..old_rows-1`)だった場合、リサイズ後も
        // 全画面region(`0..new_rows-1`)であるべき(単純に`min(max_row)`すると、
        // 行が増えた時に新しく増えた下端がscroll regionの外のままになるバグを生む)。
        let had_full_scroll_region = self.scroll_top == 0 && self.scroll_bottom == old_rows.saturating_sub(1);

        // 各画面(main/alt)につき、「その画面のカーソルがいた行」を基準に、上端から
        // 何行取り除けばカーソル行が新しい可視範囲に収まるかを個別に計算する。
        // 非アクティブ側の画面のカーソルは、直近の切り替え時に保存された
        // saved_cursor_{main,alt} を参照する(無ければ 0 行目とみなす)。
        let top_removed_for = |reference_row: usize| -> usize {
            (reference_row + 1).saturating_sub(new_rows).min(total_removed)
        };
        let main_reference_row = if self.alt_active {
            self.saved_cursor_main.map(|c| c.0).unwrap_or(0)
        } else {
            self.cursor_row
        };
        let alt_reference_row = if self.alt_active {
            self.cursor_row
        } else {
            self.saved_cursor_alt.map(|c| c.0).unwrap_or(0)
        };
        let main_removed = top_removed_for(main_reference_row);
        let alt_removed = top_removed_for(alt_reference_row);

        self.main_cells = Self::resize_grid(
            &self.main_cells, old_cols, old_rows, new_cols, new_rows, main_removed, &blank,
            Some(&mut self.pending_scrollback),
        );
        self.alt_cells = Self::resize_grid(
            &self.alt_cells, old_cols, old_rows, new_cols, new_rows, alt_removed, &blank,
            None,
        );

        let max_row = new_rows.saturating_sub(1);
        let shift_row = |row: usize, removed: usize| -> usize {
            if row < removed { 0 } else { (row - removed).min(max_row) }
        };
        let active_removed = if self.alt_active { alt_removed } else { main_removed };
        self.cursor_row = shift_row(self.cursor_row, active_removed);
        // cursor_col の有効範囲は 0..=cols (== cols は「次の print() で折り返す」
        // 保留状態を表す。`print()`/`prop_cursor_in_bounds` 参照)。単純に
        // `min(new_cols)` すると、折り返し待ちでない通常の位置(例: 旧80列中の70列目)
        // が新しい `new_cols`(例: 40) を超えていても「40 = ちょうど右端で折り返し待ち」
        // に化けてしまう。折り返し待ちだった場合(`col == old_cols`)のみ新しい
        // `new_cols` に対応する折り返し待ちへ写し、それ以外は `new_cols - 1`
        // (見えている最後の列)にクランプする。
        let clamp_col = |col: usize| -> usize {
            if col >= old_cols {
                new_cols
            } else {
                col.min(new_cols.saturating_sub(1))
            }
        };
        self.cursor_col = clamp_col(self.cursor_col);

        if let Some((row, col, fg, bg, bold)) = self.saved_cursor_main.take() {
            self.saved_cursor_main = Some((shift_row(row, main_removed), clamp_col(col), fg, bg, bold));
        }
        if let Some((row, col, fg, bg, bold)) = self.saved_cursor_alt.take() {
            self.saved_cursor_alt = Some((shift_row(row, alt_removed), clamp_col(col), fg, bg, bold));
        }

        if had_full_scroll_region {
            self.scroll_top = 0;
            self.scroll_bottom = max_row;
        } else {
            self.scroll_top = self.scroll_top.min(max_row);
            self.scroll_bottom = self.scroll_bottom.min(max_row);
            if self.scroll_bottom <= self.scroll_top {
                self.scroll_top = 0;
                self.scroll_bottom = max_row;
            }
        }

        self.cols = new_cols;
        self.rows = new_rows;
    }

    /// [resize_preserving_state] のグリッド(cols×rows のセル配列)1つ分のリサイズを行う。
    /// 行(rows)が縮む場合、まず上端`top_removed`行を取り除き(`overflow_sink`が`Some`
    /// ならそこへ積む。`None`なら単に破棄 — alt screen 用)、それでも`new_rows`に
    /// 収まりきらない残りは下端から破棄する(scrollbackには積まない)。
    fn resize_grid(
        old_cells: &[TermCell],
        old_cols: usize,
        old_rows: usize,
        new_cols: usize,
        new_rows: usize,
        top_removed: usize,
        blank: &TermCell,
        overflow_sink: Option<&mut Vec<Vec<TermCell>>>,
    ) -> Vec<TermCell> {
        let mut rows: Vec<Vec<TermCell>> = (0..old_rows)
            .map(|r| old_cells[r * old_cols..(r + 1) * old_cols].to_vec())
            .collect();

        if new_rows < old_rows {
            let removed: Vec<Vec<TermCell>> = rows.drain(0..top_removed).collect();
            if let Some(sink) = overflow_sink {
                sink.extend(removed);
            }
            rows.truncate(new_rows);
        } else if new_rows > old_rows {
            for _ in 0..(new_rows - old_rows) {
                rows.push(vec![blank.clone(); old_cols]);
            }
        }

        let mut new_cells = Vec::with_capacity(new_cols * new_rows);
        for mut row in rows {
            row.resize(new_cols, blank.clone());
            new_cells.extend(row);
        }
        new_cells
    }

    fn switch_to_alt(&mut self, save_cursor: bool) {
        if save_cursor {
            self.saved_cursor_main = Some((
                self.cursor_row, self.cursor_col,
                self.cur_fg, self.cur_bg, self.cur_bold,
            ));
        }
        let theme = self.theme;
        self.main_cells = self.cells().clone();
        let blank = TermCell { ch: smol_str::SmolStr::new_inline(" "), fg: theme.default_fg, bg: theme.default_bg, bold: false };
        self.alt_cells = vec![blank; self.cols * self.rows];
        self.alt_active = true;
        if save_cursor {
            self.cursor_row = 0;
            self.cursor_col = 0;
            self.cur_fg = theme.default_fg;
            self.cur_bg = theme.default_bg;
            self.cur_bold = false;
        }
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
    }

    fn switch_to_main(&mut self, restore_cursor: bool) {
        if !self.alt_active { return; }
        self.alt_active = false;
        if restore_cursor {
            if let Some((row, col, fg, bg, bold)) = self.saved_cursor_main.take() {
                self.cursor_row = row;
                self.cursor_col = col;
                self.cur_fg = fg;
                self.cur_bg = bg;
                self.cur_bold = bold;
            }
        }
    }

    fn scroll_up_region(&mut self, n: usize) {
        let top = self.scroll_top;
        let bot = self.scroll_bottom;
        let n = n.min(bot - top + 1);
        let cols = self.cols;

        if top == 0 && !self.alt_active {
            for i in 0..n {
                let row_start = i * cols;
                let row = self.main_cells[row_start..row_start + cols].to_vec();
                self.pending_scrollback.push(row);
            }
        }

        for row in top..=(bot - n) {
            for col in 0..cols {
                let src = self.cells_mut()[(row + n) * cols + col].clone();
                self.cells_mut()[row * cols + col] = src;
            }
        }
        for row in (bot - n + 1)..=bot {
            let blank = self.blank();
            for col in 0..cols {
                self.cells_mut()[row * cols + col] = blank.clone();
            }
        }
    }

    fn newline(&mut self) {
        if self.cursor_row == self.scroll_bottom {
            self.scroll_up_region(1);
        } else if self.cursor_row < self.rows - 1 {
            self.cursor_row += 1;
        }
    }

    fn erase_cells(&mut self, start: usize, end: usize) {
        let blank = self.blank();
        let len = self.cells_mut().len();
        for i in start..end.min(len) {
            self.cells_mut()[i] = blank.clone();
        }
    }

    fn handle_sgr(&mut self, ps: &[u16]) {
        // SGR 解決に使うテーブルはこの呼び出し時点のグローバルテーマから取得する
        // （`set_terminal_theme` で差し替え可能。以前に解決済みのセルは遡って再着色されない）。
        let theme = self.theme;
        if ps.is_empty() {
            self.cur_fg = theme.default_fg;
            self.cur_bg = theme.default_bg;
            self.cur_bold = false;
            return;
        }
        let mut i = 0;
        while i < ps.len() {
            match ps[i] {
                0  => { self.cur_fg = theme.default_fg; self.cur_bg = theme.default_bg; self.cur_bold = false; }
                1  => { self.cur_bold = true; }
                22 => { self.cur_bold = false; }
                30..=37 => { self.cur_fg = theme.ansi16[(ps[i] - 30) as usize]; }
                38 => {
                    if let Some((color, advance)) = parse_extended_color(&theme, ps, i) {
                        self.cur_fg = color;
                        i += advance;
                    }
                }
                39 => { self.cur_fg = theme.default_fg; }
                40..=47 => { self.cur_bg = theme.ansi16[(ps[i] - 40) as usize]; }
                48 => {
                    if let Some((color, advance)) = parse_extended_color(&theme, ps, i) {
                        self.cur_bg = color;
                        i += advance;
                    }
                }
                49  => { self.cur_bg = theme.default_bg; }
                90..=97  => { self.cur_fg = theme.ansi16[8 + (ps[i] - 90) as usize]; }
                100..=107 => { self.cur_bg = theme.ansi16[8 + (ps[i] - 100) as usize]; }
                _ => {}
            }
            i += 1;
        }
    }
}

/// SGR `38`(前景色)/`48`(背景色)ケースが共通で使う拡張色パース。
/// 256色パレット(`5;n`)とtrue color(`2;r;g;b`)の2形式に対応する。
/// 戻り値は`(解決した色, psを追加で消費した数)`。パースできなければ`None`
/// (呼び出し側は色を変更せず、通常通り`i`を1つ進めるだけでよい)。
fn parse_extended_color(theme: &Theme, ps: &[u16], i: usize) -> Option<(u32, usize)> {
    if ps.get(i + 1) == Some(&5) {
        let n = *ps.get(i + 2)?;
        return Some((ansi256_to_argb(theme, n as u8), 2));
    }
    if ps.get(i + 1) == Some(&2) && i + 4 < ps.len() {
        let (r, g, b) = (ps[i + 2] as u32, ps[i + 3] as u32, ps[i + 4] as u32);
        return Some((0xFF000000 | (r << 16) | (g << 8) | b, 4));
    }
    None
}

impl Perform for Terminal {
    fn print(&mut self, c: char) {
        use unicode_width::UnicodeWidthChar;
        let width = c.width().unwrap_or(1);

        if self.cursor_col >= self.cols {
            self.cursor_col = 0;
            self.newline();
        }
        if self.cursor_row < self.rows {
            *self.cell_mut(self.cursor_row, self.cursor_col) = TermCell {
                ch: smol_str::SmolStr::new(c.encode_utf8(&mut [0u8; 4])),
                fg: self.cur_fg,
                bg: self.cur_bg,
                bold: self.cur_bold,
            };
            self.cursor_col += 1;
            if width == 2 && self.cursor_col < self.cols {
                *self.cell_mut(self.cursor_row, self.cursor_col) = TermCell {
                    ch: smol_str::SmolStr::new_inline(" "),
                    fg: self.cur_fg,
                    bg: self.cur_bg,
                    bold: false,
                };
                self.cursor_col += 1;
            }
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x0D => { self.cursor_col = 0; }
            0x0A | 0x0B | 0x0C => { self.newline(); }
            0x08 => { if self.cursor_col > 0 { self.cursor_col -= 1; } }
            0x09 => {
                self.cursor_col = ((self.cursor_col / 8) + 1) * 8;
                if self.cursor_col >= self.cols { self.cursor_col = self.cols - 1; }
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _ignore: bool, action: char) {
        let is_dec = intermediates.first() == Some(&b'?');
        let ps: Vec<u16> = params.iter().map(|sub| sub[0]).collect();
        let p0 = ps.get(0).copied().unwrap_or(0);
        let p1 = ps.get(1).copied().unwrap_or(0);

        if is_dec {
            match (action, p0) {
                ('h', 47) | ('h', 1047) => { self.switch_to_alt(false); }
                ('h', 1049) => { self.switch_to_alt(true); }
                ('l', 47) | ('l', 1047) => { self.switch_to_main(false); }
                ('l', 1049) => { self.switch_to_main(true); }
                ('h', 25) | ('l', 25) => {}
                ('h', 1) => { self.application_cursor_mode = true; }
                ('l', 1) => { self.application_cursor_mode = false; }
                ('h', 2004) => { self.bracketed_paste_mode = true; }
                ('l', 2004) => { self.bracketed_paste_mode = false; }
                _ => {}
            }
            return;
        }

        match action {
            'A' => { let n = p0.max(1) as usize; self.cursor_row = self.cursor_row.saturating_sub(n); }
            'B' => { let n = p0.max(1) as usize; self.cursor_row = (self.cursor_row + n).min(self.rows - 1); }
            'C' => { let n = p0.max(1) as usize; self.cursor_col = (self.cursor_col + n).min(self.cols - 1); }
            'D' => { let n = p0.max(1) as usize; self.cursor_col = self.cursor_col.saturating_sub(n); }
            'E' => { let n = p0.max(1) as usize; self.cursor_row = (self.cursor_row + n).min(self.rows - 1); self.cursor_col = 0; }
            'F' => { let n = p0.max(1) as usize; self.cursor_row = self.cursor_row.saturating_sub(n); self.cursor_col = 0; }
            'G' => { self.cursor_col = (p0.max(1) as usize - 1).min(self.cols - 1); }
            'H' | 'f' => {
                self.cursor_row = (p0.max(1) as usize - 1).min(self.rows - 1);
                self.cursor_col = (p1.max(1) as usize - 1).min(self.cols - 1);
            }
            'J' => match p0 {
                0 => { let s = self.cursor_row * self.cols + self.cursor_col; self.erase_cells(s, self.cols * self.rows); }
                1 => { let e = self.cursor_row * self.cols + self.cursor_col + 1; self.erase_cells(0, e); }
                2 | 3 => { self.erase_cells(0, self.cols * self.rows); self.cursor_row = 0; self.cursor_col = 0; }
                _ => {}
            },
            'K' => {
                let row = self.cursor_row;
                let col = self.cursor_col;
                match p0 {
                    0 => { let s = row * self.cols + col; let e = (row + 1) * self.cols; self.erase_cells(s, e); }
                    1 => { let s = row * self.cols; let e = row * self.cols + col + 1; self.erase_cells(s, e); }
                    2 => { let s = row * self.cols; let e = (row + 1) * self.cols; self.erase_cells(s, e); }
                    _ => {}
                }
            }
            'S' => { self.scroll_up_region(p0.max(1) as usize); }
            'd' => { self.cursor_row = (p0.max(1) as usize - 1).min(self.rows - 1); }
            'm' => { self.handle_sgr(&ps); }
            'r' => {
                let top = (p0.max(1) as usize - 1).min(self.rows - 1);
                let bot = (p1.max(1) as usize - 1).min(self.rows - 1);
                if top < bot { self.scroll_top = top; self.scroll_bottom = bot; }
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        match (params.get(0), params.get(1)) {
            (Some(&b"0"), Some(title)) | (Some(&b"2"), Some(title)) => {
                if let Ok(s) = std::str::from_utf8(title) {
                    self.title = Some(s.to_string());
                }
            }
            // OSC 52 (`ESC]52;<selection>;<base64|?>BEL`): clipboard set.
            // `<selection>` (params[1], conventionally `c`/`p`/...) is not
            // distinguished — this app only has one clipboard. `Pd == "?"`
            // is a *query* (device→host read), not handled here (see the
            // `pending_clipboard_write` field doc comment).
            (Some(&b"52"), _) => {
                if let Some(&payload) = params.get(2) {
                    if payload == b"?" {
                        // Query (device→host read). The actual reply (an OSC 52
                        // response written back to the remote's stdin) needs an
                        // async round trip to Kotlin for the current clipboard
                        // text, which this synchronous VTE callback can't do —
                        // it only flags the request; `session_state.rs`/`session.rs`
                        // drain it and perform the round trip.
                        self.pending_clipboard_pull_request = true;
                    } else if let Ok(decoded) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, payload) {
                        if let Ok(text) = String::from_utf8(decoded) {
                            self.pending_clipboard_write = Some(text);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn hook(&mut self, _params: &vte::Params, _ints: &[u8], _ignore: bool, _c: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
    fn esc_dispatch(&mut self, _ints: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'M' => {
                if self.cursor_row == self.scroll_top {
                    let top = self.scroll_top;
                    let bot = self.scroll_bottom;
                    let cols = self.cols;
                    for row in (top + 1..=bot).rev() {
                        for col in 0..cols {
                            let src = self.cells_mut()[(row - 1) * cols + col].clone();
                            self.cells_mut()[row * cols + col] = src;
                        }
                    }
                    let blank = self.blank();
                    for col in 0..cols {
                        self.cells_mut()[top * cols + col] = blank.clone();
                    }
                } else if self.cursor_row > 0 {
                    self.cursor_row -= 1;
                }
            }
            b'c' => { self.reset_all(); }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vte::Parser;
    use proptest::prelude::*;

    fn feed(t: &mut Terminal, bytes: &[u8]) {
        let mut p = Parser::new();
        for &b in bytes { p.advance(t, b); }
    }

    fn cell(t: &Terminal, row: usize, col: usize) -> &str {
        t.screen_cells()[row * t.cols() + col].ch.as_str()
    }

    #[test]
    fn test_print_ascii() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"hello");
        assert_eq!(cell(&t, 0, 0), "h");
        assert_eq!(cell(&t, 0, 4), "o");
        assert_eq!(t.cursor_col(), 5);
        assert_eq!(t.cursor_row(), 0);
    }

    #[test]
    fn test_cr_lf() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"hello\r\nworld");
        assert_eq!(cell(&t, 0, 0), "h");
        assert_eq!(cell(&t, 1, 0), "w");
        assert_eq!(t.cursor_row(), 1);
        assert_eq!(t.cursor_col(), 5);
    }

    #[test]
    fn test_scroll_pushes_pending() {
        let mut t = Terminal::new(10, 3, Theme::default());
        // 4 行流すと 1 行スクロールアウト
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3");
        let pending = t.take_scrollback();
        assert!(!pending.is_empty());
        assert!(t.take_scrollback().is_empty());  // 2 回目は空
    }

    #[test]
    fn test_cursor_address_csi_h() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[6;11H");  // row=6, col=11（1-indexed）
        assert_eq!(t.cursor_row(), 5);
        assert_eq!(t.cursor_col(), 10);
    }

    #[test]
    fn test_erase_display_j2() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"hello\x1b[2J");
        assert_eq!(cell(&t, 0, 0), " ");
        assert_eq!(t.cursor_row(), 0);
        assert_eq!(t.cursor_col(), 0);
    }

    #[test]
    fn test_sgr_ansi_color() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[31mA");  // red fg
        let c = &t.screen_cells()[0];
        assert_eq!(c.ch.as_str(), "A");
        assert_eq!(c.fg, Theme::default().ansi16[1]);  // ANSI red
    }

    #[test]
    fn test_sgr_reset() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[31m\x1b[0mB");
        assert_eq!(t.screen_cells()[0].fg, Theme::default().default_fg);
    }

    #[test]
    fn test_custom_theme_passed_at_construction_changes_sgr_resolution() {
        // Phase 12: per-session theme。Terminal::new()に明示的に渡したテーマが
        // 初期デフォルト色にもSGR解決にも使われる(グローバルは一切参照しない)。
        let mut custom = Theme::default();
        custom.ansi16[1] = 0xFF123456;   // 赤(index 1)を差し替え
        custom.default_fg = 0xFF111111;
        custom.default_bg = 0xFF222222;

        let mut t = Terminal::new(80, 24, custom);
        assert_eq!(t.screen_cells()[0].fg, 0xFF111111);
        assert_eq!(t.screen_cells()[0].bg, 0xFF222222);

        feed(&mut t, b"\x1b[31mA");
        assert_eq!(t.screen_cells()[0].fg, 0xFF123456);

        // 256色パレットの 0..=15 部分も同テーブルを参照する
        feed(&mut t, b"\r\x1b[38;5;1mB");
        assert_eq!(t.screen_cells()[0].fg, 0xFF123456);
    }

    #[test]
    fn test_set_theme_affects_only_future_sgr_resolution() {
        // Phase 12: per-session theme。set_theme()は「以降にパースされるSGR」にのみ
        // 反映され、既に解決済みのセルは遡って再着色されない(既存の仕様を維持)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[31mA");
        let original_red = t.screen_cells()[0].fg;

        let mut custom = Theme::default();
        custom.ansi16[1] = 0xFF123456;
        t.set_theme(custom);

        assert_eq!(t.screen_cells()[0].fg, original_red);

        feed(&mut t, b"\r\x1b[31mB");
        assert_eq!(t.screen_cells()[0].fg, 0xFF123456);
    }

    #[test]
    fn test_alt_screen_switch() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"main");
        feed(&mut t, b"\x1b[?1049h");   // alt に切り替え（カーソル保存）
        assert_eq!(cell(&t, 0, 0), " "); // alt は空白
        feed(&mut t, b"\x1b[?1049l");   // main に戻る
        assert_eq!(cell(&t, 0, 0), "m"); // main が復元
    }

    #[test]
    fn test_title_osc() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]0;My Title\x07");
        assert_eq!(t.title(), Some("My Title"));
    }

    #[test]
    fn test_clipboard_write_osc_52() {
        let mut t = Terminal::new(80, 24, Theme::default());
        // "hello" base64-encoded, selection "c" (clipboard).
        feed(&mut t, b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(t.take_pending_clipboard_write(), Some("hello".to_string()));
        // Consumed once — a second take returns None until the next OSC 52.
        assert_eq!(t.take_pending_clipboard_write(), None);
    }

    #[test]
    fn test_clipboard_query_is_not_treated_as_a_write() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]52;c;?\x07");
        assert_eq!(t.take_pending_clipboard_write(), None);
    }

    #[test]
    fn test_clipboard_query_sets_pending_pull_request() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]52;c;?\x07");
        assert!(t.take_pending_clipboard_pull_request());
        // Consumed once — a second take returns false until the next query.
        assert!(!t.take_pending_clipboard_pull_request());
    }

    #[test]
    fn test_clipboard_write_does_not_set_pending_pull_request() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]52;c;aGVsbG8=\x07");
        assert!(!t.take_pending_clipboard_pull_request());
    }

    #[test]
    fn test_reset_clears_pending_clipboard_pull_request() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]52;c;?\x07");
        feed(&mut t, b"\x1bc"); // RIS (full reset)
        assert!(!t.take_pending_clipboard_pull_request());
    }

    #[test]
    fn test_clipboard_write_ignores_invalid_base64() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]52;c;not-valid-base64!!\x07");
        assert_eq!(t.take_pending_clipboard_write(), None);
    }

    #[test]
    fn test_reset_clears_pending_clipboard_write() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]52;c;aGVsbG8=\x07");
        feed(&mut t, b"\x1bc"); // RIS (full reset)
        assert_eq!(t.take_pending_clipboard_write(), None);
    }

    #[test]
    fn test_backspace() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"ab\x08c");  // a b BS c → "ac" at col 0,1
        assert_eq!(cell(&t, 0, 0), "a");
        assert_eq!(cell(&t, 0, 1), "c");
        assert_eq!(t.cursor_col(), 2);
    }

    #[test]
    fn test_cursor_up_down_csi() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[5B");  // cursor down 5
        assert_eq!(t.cursor_row(), 5);
        feed(&mut t, b"\x1b[2A");  // cursor up 2
        assert_eq!(t.cursor_row(), 3);
    }

    #[test]
    fn test_erase_line_k0() {
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"hello");
        feed(&mut t, b"\x1b[1G\x1b[2K");  // col=1 then erase whole line
        for i in 0..10 {
            assert_eq!(t.screen_cells()[i].ch.as_str(), " ", "col {}", i);
        }
    }

    // ── resize_preserving_state ─────────────────────────

    #[test]
    fn test_resize_preserving_state_updates_dimensions() {
        let mut t = Terminal::new(80, 24, Theme::default());
        t.resize_preserving_state(40, 12);
        assert_eq!(t.cols(), 40);
        assert_eq!(t.rows(), 12);
        assert_eq!(t.screen_cells().len(), 40 * 12);
    }

    #[test]
    fn test_resize_preserving_state_keeps_existing_content() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"hello");
        t.resize_preserving_state(40, 12);
        assert_eq!(cell(&t, 0, 0), "h");
        assert_eq!(cell(&t, 0, 4), "o");
    }

    #[test]
    fn test_resize_preserving_state_growing_cols_pads_with_blank() {
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"hi");
        t.resize_preserving_state(20, 3);
        assert_eq!(cell(&t, 0, 0), "h");
        assert_eq!(cell(&t, 0, 1), "i");
        for col in 2..20 {
            assert_eq!(cell(&t, 0, col), " ", "col {}", col);
        }
    }

    #[test]
    fn test_resize_preserving_state_shrinking_cols_clips_content() {
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"0123456789");
        t.resize_preserving_state(5, 3);
        assert_eq!(cell(&t, 0, 0), "0");
        assert_eq!(cell(&t, 0, 4), "4");
        assert_eq!(t.screen_cells().len(), 5 * 3);
    }

    #[test]
    fn test_resize_preserving_state_growing_rows_pads_bottom() {
        let mut t = Terminal::new(10, 2, Theme::default());
        feed(&mut t, b"top");
        t.resize_preserving_state(10, 5);
        assert_eq!(cell(&t, 0, 0), "t");
        for col in 0..10 {
            assert_eq!(cell(&t, 4, col), " ");
        }
    }

    #[test]
    fn test_resize_preserving_state_shrinking_rows_pushes_top_overflow_to_scrollback() {
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        t.resize_preserving_state(10, 2);
        let pending = t.take_scrollback();
        // 5行→2行なので上端3行がscrollbackへ押し出される
        assert_eq!(pending.len(), 3);
        assert_eq!(pending[0][0].ch.as_str(), "r"); // row0 が最も古い(先頭)
    }

    #[test]
    fn test_resize_preserving_state_only_pushes_main_screen_overflow_to_scrollback() {
        // main/alt 両方のグリッドが同時にリサイズされる(alt表示中でもmain_cellsは裏で
        // 保持されている)が、scrollbackに積まれるのはmain screenの内容のみ。altの
        // 内容は実端末同様破棄され、混入しない。
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"m0\r\nm1\r\nm2\r\nm3\r\nm4"); // 主画面に5行(ちょうど収まる)
        feed(&mut t, b"\x1b[?1049h"); // altへ切り替え
        feed(&mut t, b"a0\r\na1\r\na2\r\na3\r\na4"); // altにも5行

        t.resize_preserving_state(10, 2);
        let pending = t.take_scrollback();

        assert_eq!(pending.len(), 3);
        for (i, row) in pending.iter().enumerate() {
            let text: String = row.iter().map(|c| c.ch.as_str()).collect();
            assert!(text.starts_with(&format!("m{}", i)), "row {} = {:?}", i, text);
        }

        feed(&mut t, b"\x1b[?1049l"); // main に戻る
        assert_eq!(cell(&t, 0, 0), "m");
        assert_eq!(cell(&t, 0, 1), "3"); // あふれた3行分、row0はm3の内容になる
    }

    #[test]
    fn test_resize_preserving_state_preserves_sgr_and_cursor_within_bounds() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[31mA"); // 赤, cursor_col=1
        t.resize_preserving_state(40, 12);
        feed(&mut t, b"B");
        assert_eq!(cell(&t, 0, 1), "B");
        assert_eq!(t.screen_cells()[1].fg, Theme::default().ansi16[1]); // 赤が引き継がれた
    }

    #[test]
    fn test_resize_preserving_state_clips_cursor_when_shrinking() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[20;70H"); // row=20,col=70(0-indexed 19,69)
        t.resize_preserving_state(40, 10);
        assert!(t.cursor_row() < 10);
        assert!(t.cursor_col() <= 40);
    }

    #[test]
    fn test_resize_preserving_state_preserves_title_and_modes() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]0;My Title\x07");
        feed(&mut t, b"\x1b[?1h");    // application cursor mode on
        feed(&mut t, b"\x1b[?2004h"); // bracketed paste on
        t.resize_preserving_state(40, 12);
        assert_eq!(t.title(), Some("My Title"));
        assert!(t.application_cursor_mode());
        assert!(t.bracketed_paste_mode());
    }

    #[test]
    fn test_resize_preserving_state_noop_when_size_unchanged() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"hello");
        t.resize_preserving_state(80, 24);
        assert_eq!(cell(&t, 0, 0), "h");
        assert_eq!(t.cols(), 80);
        assert_eq!(t.rows(), 24);
    }

    #[test]
    fn test_resize_preserving_state_default_scroll_region_grows_with_screen() {
        // Codexレビュー(#18)で発見されたバグの回帰テスト: 全画面がデフォルトの
        // scroll region(0..old_rows-1)だった場合、行が増えるリサイズ後も
        // scroll regionが全画面(0..new_rows-1)を覆っていなければならない。
        // 単純にmin(max_row)するだけだと、増えた下端がscroll regionの外に
        // 取り残され、newlineでのスクロールが画面の上半分だけで起きてしまう。
        let mut t = Terminal::new(80, 24, Theme::default());
        t.resize_preserving_state(80, 40);
        // 24行目(0-indexed)より下までnewlineでスクロールできることを確認する:
        // scroll regionが0..23のまま壊れていれば、この時点でcursor_rowは23で
        // 頭打ちになる。newlineを新しい行数分(40回)より多く送り、最終的に
        // 新しい最終行(39)まで到達することを確認する。
        for _ in 0..45 {
            feed(&mut t, b"x\r\n");
        }
        assert_eq!(t.cursor_row(), 39, "scroll region did not grow to cover the new rows");
    }

    #[test]
    fn test_resize_preserving_state_explicit_scroll_region_is_clamped_not_reset() {
        // 全画面でない明示的なscroll region(DECSTBM)が設定されていた場合は、
        // (全画面だった場合と違って)新サイズにclampするだけで、勝手に全画面へは
        // リセットしない。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[3;10r"); // scroll region = rows 3..10 (1-indexed) = 2..9 (0-indexed)
        t.resize_preserving_state(80, 12);
        // scroll_top/bottomの直接getterは無いため、scroll region下端(0-indexed 9)を
        // 超えてnewlineしてもcursor_rowが10へ進まない(regionの外に出ない)ことで
        // 間接的に検証する。
        feed(&mut t, b"\x1b[10;1H"); // カーソルをregion下端(0-indexed row9)へ
        feed(&mut t, b"\r\nA");
        assert_eq!(t.cursor_row(), 9, "explicit scroll region should not be reset to full-screen");
    }

    #[test]
    fn test_resize_preserving_state_shrinking_cols_does_not_create_spurious_wrap_pending() {
        // Codexレビュー(#18)で発見されたバグの回帰テスト: 折り返し待ち状態でない
        // 通常のカーソル位置(旧cols内の途中の列)が、単純に`min(new_cols)`されると
        // 「ちょうど新しいnew_colsで折り返し待ち」に化けてしまい、次に出力した文字が
        // 同じ行の右端ではなく次行の先頭に出てしまうバグがあった。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[1;70H"); // row=1,col=70(1-indexed) → cursor_col=69(0-indexed)
        t.resize_preserving_state(40, 24); // cols: 80→40, 69 は新しい幅を超える
        feed(&mut t, b"Z");
        // 折り返し待ちに化けていれば "Z" は次の行(row 1)の先頭に出てしまう。
        // 正しくは同じ行(row 0)の右端(col 39)に出る。
        assert_eq!(t.cursor_row(), 0, "cursor should not have wrapped to the next row");
        assert_eq!(cell(&t, 0, 39), "Z");
    }

    #[test]
    fn test_resize_preserving_state_preserves_wrap_pending_state() {
        // 折り返し待ち状態(cursor_col == cols)だった場合は、リサイズ後も
        // 折り返し待ちのまま(新しいcolsの値)引き継がれる。
        let mut t = Terminal::new(10, 24, Theme::default());
        feed(&mut t, b"0123456789"); // ちょうど10文字 → cursor_col=10(==cols, 折り返し待ち)
        assert_eq!(t.cursor_col(), 10);
        t.resize_preserving_state(20, 24);
        assert_eq!(t.cursor_col(), 20);
    }

    #[test]
    fn test_resize_preserving_state_clamps_zero_size_to_minimum() {
        // Terminal自身の不変量(cursor_row < rows等)を、呼び出し元がどんな値を渡しても
        // 保つため、0を渡されても最低1x1にclampする(Codexレビュー(#18)指摘のP2)。
        let mut t = Terminal::new(80, 24, Theme::default());
        t.resize_preserving_state(0, 0);
        assert_eq!(t.cols(), 1);
        assert_eq!(t.rows(), 1);
        assert_eq!(t.screen_cells().len(), 1);
        assert!(t.cursor_row() < t.rows());
        assert!(t.cursor_col() <= t.cols());
    }

    // ── Proptest: 不変量 ────────────────────────────────

    proptest! {
        /// 任意バイト列でパニックしない
        #[test]
        fn prop_no_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let mut t = Terminal::new(80, 24, Theme::default());
            feed(&mut t, &bytes);
        }

        /// カーソルは常に画面内に収まる
        #[test]
        fn prop_cursor_in_bounds(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let mut t = Terminal::new(80, 24, Theme::default());
            feed(&mut t, &bytes);
            prop_assert!(t.cursor_row() < t.rows(),
                "cursor_row={} >= rows={}", t.cursor_row(), t.rows());
            prop_assert!(t.cursor_col() <= t.cols(),
                "cursor_col={} > cols={}", t.cursor_col(), t.cols());
        }

        /// screen_cells の長さは常に cols × rows
        #[test]
        fn prop_screen_size_invariant(
            cols in 10usize..120,
            rows in 4usize..40,
            bytes in proptest::collection::vec(any::<u8>(), 0..512),
        ) {
            let mut t = Terminal::new(cols, rows, Theme::default());
            feed(&mut t, &bytes);
            prop_assert_eq!(t.screen_cells().len(), cols * rows);
        }

        /// スクロールアウト行の幅は cols と一致する
        #[test]
        fn prop_scrollback_row_width(
            cols in 10usize..80,
            rows in 3usize..10,
            bytes in proptest::collection::vec(any::<u8>(), 0..512),
        ) {
            let mut t = Terminal::new(cols, rows, Theme::default());
            feed(&mut t, &bytes);
            for row in t.take_scrollback() {
                prop_assert_eq!(row.len(), cols);
            }
        }

        /// resize_preserving_state 前後でサイズ・カーソル不変量が保たれる
        #[test]
        fn prop_resize_preserving_state_invariants(
            before in proptest::collection::vec(any::<u8>(), 0..256),
            new_cols in 10usize..120,
            new_rows in 4usize..40,
            after in proptest::collection::vec(any::<u8>(), 0..256),
        ) {
            let mut t = Terminal::new(80, 24, Theme::default());
            feed(&mut t, &before);
            t.resize_preserving_state(new_cols, new_rows);
            prop_assert_eq!(t.cols(), new_cols);
            prop_assert_eq!(t.rows(), new_rows);
            prop_assert_eq!(t.screen_cells().len(), new_cols * new_rows);
            prop_assert!(t.cursor_row() < t.rows());
            prop_assert!(t.cursor_col() <= t.cols());
            feed(&mut t, &after);
            prop_assert_eq!(t.screen_cells().len(), new_cols * new_rows);
            prop_assert!(t.cursor_row() < t.rows());
            prop_assert!(t.cursor_col() <= t.cols());
        }
    }
}
