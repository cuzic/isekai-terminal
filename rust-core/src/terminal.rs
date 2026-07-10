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
    /// query(`Pd == "?"`)は未対応(device→hostのクリップボード読み出しは
    /// Android/iOS本体アプリではまだ実装していない、タスク#82参照)。
    pending_clipboard_write: Option<String>,
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

    /// 現在のテーマのスナップショット([SessionState::reset_for_resize]がリサイズ時に
    /// 新しい`Terminal`へ引き継ぐために使う)。
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

    pub(crate) fn cols(&self) -> usize { self.cols }
    pub(crate) fn rows(&self) -> usize { self.rows }
    pub(crate) fn cursor_row(&self) -> usize { self.cursor_row }
    pub(crate) fn cursor_col(&self) -> usize { self.cursor_col }
    pub(crate) fn title(&self) -> Option<&str> { self.title.as_deref() }
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
                    if ps.get(i + 1) == Some(&5) {
                        if let Some(&n) = ps.get(i + 2) { self.cur_fg = ansi256_to_argb(&theme, n as u8); i += 2; }
                    } else if ps.get(i + 1) == Some(&2) && i + 4 < ps.len() {
                        let (r, g, b) = (ps[i+2] as u32, ps[i+3] as u32, ps[i+4] as u32);
                        self.cur_fg = 0xFF000000 | (r << 16) | (g << 8) | b;
                        i += 4;
                    }
                }
                39 => { self.cur_fg = theme.default_fg; }
                40..=47 => { self.cur_bg = theme.ansi16[(ps[i] - 40) as usize]; }
                48 => {
                    if ps.get(i + 1) == Some(&5) {
                        if let Some(&n) = ps.get(i + 2) { self.cur_bg = ansi256_to_argb(&theme, n as u8); i += 2; }
                    } else if ps.get(i + 1) == Some(&2) && i + 4 < ps.len() {
                        let (r, g, b) = (ps[i+2] as u32, ps[i+3] as u32, ps[i+4] as u32);
                        self.cur_bg = 0xFF000000 | (r << 16) | (g << 8) | b;
                        i += 4;
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
                    if payload != b"?" {
                        if let Ok(decoded) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, payload) {
                            if let Ok(text) = String::from_utf8(decoded) {
                                self.pending_clipboard_write = Some(text);
                            }
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
    }
}
