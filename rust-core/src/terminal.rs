use std::collections::{HashMap, VecDeque};
use vte::Perform;
use crate::sixel::{SixelDecoder, SixelImage};
use crate::kitty_graphics::{KittyCommand, KittyGraphics};
use crate::theme::Theme;
use crate::{CursorShape, ImagePlacement, MouseButton, MouseEventKind, MouseReportingMode, TerminalKeyModifiers};

/// Sixel(タスク#42)の名目セルサイズ(ピクセル)。実フォントのピクセルサイズは
/// このRustコアには分からない(Android/iOSの実描画レイヤーのみが知っている)ため、
/// VT340由来の名目値で近似する——アスペクト比(1:2)は一般的なmonospaceフォントの
/// セル比に近く、`ImagePlacement::rows_span`/`cols_span`の見た目上の破綻を避ける
/// 目的で選んだ固定値であり、実ピクセル座標としては使わない(`ImagePlacement`の
/// docコメント参照)。
const SIXEL_CELL_WIDTH_PX: usize = 10;
const SIXEL_CELL_HEIGHT_PX: usize = 20;
/// 同時にライブな(まだ寿命が尽きていない)Sixel画像の上限。スクロール等が起きず
/// 同じ場所へ画像を上書きし続けるだけのストリーム(dedupeせず`images`へpushし
/// 続ける)でメモリが無制限に増えるのを防ぐ——上限に達したら最も古いものを
/// 1つ捨てる(`SCROLLBACK_LIMIT`と同種の素朴なキャップ、`session.rs`参照)。
///
/// 個数だけのキャップであり総バイト数は見ていないため、単体上限
/// (`sixel::MAX_SIXEL_AREA` = 4M pixel = RGBA8888で16MiB相当)いっぱいの画像を
/// `MAX_LIVE_IMAGES`回連続で送りつけられると理論上16MiB×32=512MiBまで増え得る
/// (タスク#90、`ScreenUpdate::images`として毎フレーム`to_vec()`で複製される分の
/// FFIコピー負荷も同様)。`MAX_TOTAL_IMAGE_RGBA_BYTES`が総バイト数側のキャップを
/// 別途掛ける。
const MAX_LIVE_IMAGES: usize = 32;
/// ライブなSixel画像の`rgba`バイト列の合計サイズ上限(バイト)。`MAX_LIVE_IMAGES`
/// (個数の上限)だけでは単体最大サイズ(16MiB)の画像を32枚溜め込まれた場合の
/// 理論上限(512MiB)を防げない(タスク#90)ため、こちらは総バイト数を直接キャップ
/// する。単体上限(16MiB)の数枚分は正当な用途(進捗表示等で連続する数枚のSixel)
/// として許容しつつ、512MiBという理論上限を大幅に下げる値として単体上限の4倍
/// (64MiB)を選ぶ。`place_sixel_image`は新しい画像を追加した後、この合計または
/// `MAX_LIVE_IMAGES`を超えている間、最も古い画像から順に捨てる。
const MAX_TOTAL_IMAGE_RGBA_BYTES: usize = 64 * 1024 * 1024;
/// OSC 8(タスク#40)のURL intern表(`Terminal::link_table`)に登録できるURLの
/// 上限件数。`link_table`は(scrollback中の過去セルの`link_id`がindexとして
/// 指し続けるため)RISでもクリアせず単調増加する設計(`link_table`フィールドの
/// docコメント参照)なので、上限が無いと相異なるOSC8 URLを大量に流すリモートに
/// よってメモリが際限なく増え、かつ`make_screen_update`が毎フレーム`to_vec()`で
/// 表全体を複製するFFIコピーコストも線形に悪化する(タスク#70)。上限到達後の
/// 新規URLは`intern_link`がインターンせず`None`を返し、呼び出し元
/// (`handle_osc8_hyperlink`)はリンク無し(`active_link_id = None`)にフォールバック
/// する——既存id(=既存セルの`link_id`参照)はscrollback保護のため削除・再利用
/// しない。
pub(crate) const MAX_LINK_TABLE: usize = 4096;
/// OSC 133(タスク#13、セマンティックプロンプト)のマーク履歴上限。500プロンプト
/// サイクル分(A/B/C/Dの4マーク)を目安に選んだ値——`link_table`(タスク#40)と同じ
/// 理由(相異なるURLを大量に流されてもメモリが際限なく増えないようにする)で、
/// 上限到達後は最も古いマークから捨てる(`Terminal::prompt_marks`フィールド参照)。
const MAX_PROMPT_MARKS: usize = 2000;

/// OSC 133(`ESC]133;<Ps>...ST`、タスク#13)の1マークが表す区切りの種類。
/// <https://gitlab.freedesktop.org/Per_Bothner/specifications/blob/master/proposals/semantic-prompts.md>
/// のA/B/C/Dにそれぞれ対応する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptMarkKind {
    /// `ESC]133;A`: プロンプト描画の開始。
    PromptStart,
    /// `ESC]133;B`: プロンプト終了・コマンド入力開始。
    CommandStart,
    /// `ESC]133;C`: コマンド実行開始(以降の出力がそのコマンドの出力)。
    CommandExecuted,
    /// `ESC]133;D[;<exit_code>]`: コマンド終了。`exit_code`は数値としてパースできた
    /// 場合のみ`Some`(シェルによってはexit codeを送らない、または`aid=`等の
    /// 数値以外のパラメータを送ることがあるため、パース失敗は単に`None`として扱う)。
    CommandFinished { exit_code: Option<i32> },
}

/// OSC 133の1マーク(`PromptMarkKind`+発生した絶対行番号)。`row`は
/// `Terminal::total_scrolled_lines`基準の単調増加座標——scrollbackが後から
/// トリミングされてもマーク自身の値は変わらず、参照する側(`prompt_jump_target`等)が
/// 呼び出し時点の`total_scrolled_lines`/scrollback長と突き合わせて現在の
/// 表示位置へ変換する(`ScrollbackSearchMatch`の「offsetはスナップショット時点限定」
/// という制約と違い、こちらは絶対座標なので長期保持しても意味が変わらない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PromptMark {
    pub(crate) kind: PromptMarkKind,
    pub(crate) row: u64,
}
/// 1セルへ結合文字(combining character、幅0文字)として追記できるUTF-8バイト数の
/// 上限(タスク#78)。`print()`のwidth==0分岐(タスク#39)は結合文字を直前セルの
/// `cell.ch`へ無条件で連結する実装だったため、`a` + `U+0301`(2バイト)を大量に
/// 送り続けるリモートに対して1セルの文字列長がNに比例して際限なく増え続ける
/// (`make_screen_update`が毎フレーム複製する分のコストも線形に悪化する)。
/// 正当な結合文字列(例: ベトナム語・デーヴァナーガリー文字等の複数記号併用)は
/// 実用上せいぜい数個程度で収まるため、32バイト程度では既存の正当な用途を
/// 誤って切り詰めかねない一方(codexレビュー指摘)、上限自体は必須
/// ——base文字(最大4バイト)+ 結合文字数十個分の余裕を見て128バイトとする。
/// 上限に達した以降の結合文字は黙って捨てる(`MAX_LINK_TABLE`と同種の素朴な
/// キャップ)。
const MAX_COMBINING_BYTES_PER_CELL: usize = 128;

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

/// セルが表示する文字の見た目上の幅(1 または 2)。`is_wide_placeholder` セル自体は
/// 常に `" "`(幅1)を保持するため、このヘルパーは「そのセルの `ch` 自体が全角文字の
/// 本体か」を判定するのに使う([Terminal::sanitize_wide_row] 参照)。
fn cell_display_width(cell: &TermCell) -> usize {
    use unicode_width::UnicodeWidthChar;
    cell.ch.chars().next().and_then(|c| c.width()).unwrap_or(1)
}

/// 行内で全角(wide)文字の「本体セル(表示幅==2) + 直後の`is_wide_placeholder`セル」
/// という対応関係が崩れている箇所を修復する共通ロジック(タスク#47/#85)。
/// [`Terminal::sanitize_wide_row`](挿入/削除経路)に加え、リサイズによる列数縮小時の
/// 右端クリップ(`Terminal::resize_grid`)・DECAWM off時に全角文字を最終列へ書き込んだ
/// 場合(`Terminal::print_mapped`)でも、placeholderを伴わない幅2本体セルが孤立して
/// 残り得るため、それらから共通で呼び出す。
///
/// - `is_wide_placeholder`セルの左隣が幅2本体でなければ、浮いたplaceholderフラグを
///   解除する。
/// - 表示幅2の本体セルの右隣が`is_wide_placeholder`でなければ(行末で切り落とされた・
///   placeholderを置く列が無かった等)、片割れを失った本体を空白へ変換する。色・装飾等
///   の他の属性は(壊れた復旧時の見た目として無難なので)そのまま残す。
fn sanitize_wide_cells(row: &mut [TermCell]) {
    let cols = row.len();
    for c in 0..cols {
        if row[c].is_wide_placeholder {
            let left_is_wide_head = c > 0 && cell_display_width(&row[c - 1]) == 2;
            if !left_is_wide_head {
                row[c].is_wide_placeholder = false;
            }
        } else if cell_display_width(&row[c]) == 2 {
            let right_is_placeholder = c + 1 < cols && row[c + 1].is_wide_placeholder;
            if !right_is_placeholder {
                row[c].ch = smol_str::SmolStr::new_inline(" ");
            }
        }
    }
}

/// G0/G1文字セット指定(`ESC ( <final>`/`ESC ) <final>`、タスク#41)。ASCII以外は
/// DEC Special Graphics(罫線・記号セット、最終バイト`0`)のみ対応する — UK(`A`)等の
/// 他の国別セットはグラフィック文字の写像を持たない(ASCIIとほぼ同一の文字集合)ため
/// 区別せずASCIIとして扱う(未知の最終バイトは`esc_dispatch`側でASCII指定として
/// フォールバックする——codexレビュー指摘: 「区別せずASCIIとして扱う」というこの
/// コメント自体の意図と、以前の実装が未知の最終バイトを単に無視していた挙動が
/// 食い違っていたため、意図通りASCIIへ倒すよう修正した)。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Charset {
    Ascii,
    DecSpecialGraphics,
}

/// DEC Special Graphics and Line Drawing Set(`ESC ( 0`/`ESC ) 0`)における、ASCII
/// `_`(0x5f)・`` ` ``(0x60)〜`~`(0x7e)のUnicode写像。非UTF-8ロケールのncurses/dialog/mc等が
/// 罫線描画にこのモードを使うため、翻訳しないとレンダラー側に生ASCII(`q`/`x`等)が
/// 渡り「lqqqk」のように文字化けする(タスク#41、Fableレビュー2次で実害を指摘)。
/// `_`(0x5f)はVT100仕様上blank(空白)に写像される(VT100 User Guide Table 3-9、
/// codexレビュー指摘: 当初0x60〜0x7eのみ扱っており0x5fが未対応だった)。
/// この範囲外の文字(0x5f未満・0x7f以上)はASCIIと同一のためそのまま返す。
/// マッピングは xterm/alacritty 等主要実装が使う標準VT100テーブルに準拠する。
fn dec_special_graphics(c: char) -> char {
    match c {
        '_' => ' ',
        '`' => '◆',
        'a' => '▒',
        'b' => '\u{2409}', // SYMBOL FOR HORIZONTAL TABULATION
        'c' => '\u{240c}', // SYMBOL FOR FORM FEED
        'd' => '\u{240d}', // SYMBOL FOR CARRIAGE RETURN
        'e' => '\u{240a}', // SYMBOL FOR LINE FEED
        'f' => '°',
        'g' => '±',
        'h' => '\u{2424}', // SYMBOL FOR NEWLINE
        'i' => '\u{240b}', // SYMBOL FOR VERTICAL TABULATION
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        other => other,
    }
}

#[derive(Clone, PartialEq)]
pub(crate) struct TermCell {
    pub(crate) ch: smol_str::SmolStr,
    pub(crate) fg: u32,
    pub(crate) bg: u32,
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    pub(crate) italic: bool,
    pub(crate) underline: bool,
    pub(crate) strikethrough: bool,
    pub(crate) blink: bool,
    pub(crate) invisible: bool,
    /// 全角(wide)文字が占める2セル目(プレースホルダ)であることを示す内部専用フラグ。
    /// `CellData`(UniFFI公開型)には出さない——`session.rs::to_cell_data`が変換時に
    /// 落とす。幅0の結合文字([Terminal::print]参照)を、プレースホルダではなく
    /// 全角文字自身の本体セルへ付加するために使う(Fableレビュー: タスク#39)。
    pub(crate) is_wide_placeholder: bool,
    /// OSC 8(`ESC]8;params;URIST`、タスク#40)で開かれているハイパーリンクの
    /// intern id。`Some`ならこのセルは`Terminal::link_table()`の同indexのURL文字列を
    /// 指す。セル自体には`String`を持たせない——`CellData`は`ScreenUpdate`として
    /// 毎フレーム全セル分FFIコピーされるため(Fableレビュー2次: `Option<String>`を
    /// セルへ直接持たせるとコストが跳ね上がる)。かわりに`ScreenUpdate`に
    /// id→URLの`link_table`を1つ持たせ、セルは軽量な`Option<u32>`のみ持つ
    /// intern方式にする。
    pub(crate) link_id: Option<u32>,
}

/// 現在のカーソル位置に適用されている SGR 属性一式(色は「論理色」——`reverse`が
/// 立っている場合でも fg/bg 自体はスワップしない)。SGR 27 で reverse を解除した
/// 時に元の色へ戻せるようにするため。実際にセルへ書き込む時([TermAttrs::to_cell])
/// にのみ reverse を適用した実効色を計算する——このコードベースは色を SGR パース時
/// に ARGB へ解決し、以後遡って再着色しない方針(`ansi256_to_argb`・テーマ切り替えの
/// 既存テスト参照)に一貫させるため、セルに書き込む瞬間が「解決するタイミング」となる。
#[derive(Clone, Copy)]
pub(crate) struct TermAttrs {
    pub(crate) fg: u32,
    pub(crate) bg: u32,
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    pub(crate) italic: bool,
    pub(crate) underline: bool,
    pub(crate) strikethrough: bool,
    pub(crate) blink: bool,
    pub(crate) invisible: bool,
    pub(crate) reverse: bool,
}

impl TermAttrs {
    fn default_for(theme: &Theme) -> Self {
        TermAttrs {
            fg: theme.default_fg,
            bg: theme.default_bg,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            strikethrough: false,
            blink: false,
            invisible: false,
            reverse: false,
        }
    }

    /// `reverse` を適用した実効 (fg, bg)。`reverse` が立っていなければ論理色そのまま。
    fn effective_colors(&self) -> (u32, u32) {
        if self.reverse { (self.bg, self.fg) } else { (self.fg, self.bg) }
    }

    /// `link_id`は含めない——ハイパーリンク状態(OSC 8)はSGR属性(この構造体)とは
    /// 独立で、`ESC[0m`のようなSGRリセットでは閉じない(呼び出し元([Terminal::print]
    /// 等)が`Terminal::active_link_id`を別途セルへ付加する)。
    fn to_cell(&self, ch: smol_str::SmolStr) -> TermCell {
        let (fg, bg) = self.effective_colors();
        TermCell {
            ch,
            fg,
            bg,
            bold: self.bold,
            dim: self.dim,
            italic: self.italic,
            underline: self.underline,
            strikethrough: self.strikethrough,
            blink: self.blink,
            invisible: self.invisible,
            is_wide_placeholder: false,
            link_id: None,
        }
    }
}

/// 画面全体をリセットする時(初期化・`RIS`・alt screen切り替え時の新画面)に使う、
/// SGR属性を一切持たない空白セル。カーソル位置の現在SGR属性を引き継ぐ通常の
/// erase/blank ([Terminal::blank]) とは意図的に区別する。
fn blank_cell_for_theme(theme: &Theme) -> TermCell {
    TermCell {
        ch: smol_str::SmolStr::new_inline(" "),
        fg: theme.default_fg,
        bg: theme.default_bg,
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

/// DECSC(`ESC 7`)・CSI `s`・DECSET `?1047`/`?1049`が保存するカーソル状態一式。
/// ECMA-48/DECSC仕様上の保存対象(カーソル位置・SGR属性・文字セット状態)を1つに
/// まとめた struct(タスク#41で`(usize, usize, TermAttrs)`タプルから拡張。
/// [Terminal]の`saved_cursor_main`フィールドdocコメント参照)。
#[derive(Clone, Copy)]
struct SavedCursor {
    row: usize,
    col: usize,
    attrs: TermAttrs,
    g0: Charset,
    g1: Charset,
    /// SI/SOによるGL(現在印字に使われる文字集合)の選択状態。`false`=G0、`true`=G1。
    gl_is_g1: bool,
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
    /// カーソル位置 + SGR属性一式の保存スロット(それぞれの画面ごとに1つ)。
    /// 2つの独立した経路から書き込まれる:
    /// 1. DECSET/DECRST `?1047`/`?1049`(`switch_to_alt`/`switch_to_main`)—— alt画面への
    ///    切り替え時に暗黙的にmain側のカーソルを保存/復元する(仕様上`?1049`は
    ///    「DECSCとして保存 → alt切替 → 画面消去」「main復帰 → DECRCとして復元」を兼ねる
    ///    ため、下記2.のDECSC/DECRCと同じスロットを共有するのが正しい——実際、
    ///    アプリがalt画面へ入る前に明示`ESC 7`していた場合、`?1049h`の暗黙保存が
    ///    それを上書きするのが仕様通りの挙動)。
    /// 2. DECSC/DECRC(`ESC 7`/`ESC 8`)・CSI `s`/`u`(ANSI.SYS方言、タスク#57)——
    ///    `save_cursor_decsc`/`restore_cursor_decrc`が、その時点でアクティブな画面
    ///    (`alt_active`)に応じてどちらのスロットを使うか選ぶ。alt画面上で明示的に
    ///    `ESC 7`/`ESC 8`する場合はこちらが専ら`saved_cursor_alt`を使う経路になる。
    ///
    /// 文字セット状態(G0/G1、タスク#41)も[SavedCursor]の一部として保存/復元される
    /// (DECSCは仕様上カーソル位置・SGR属性・文字セット状態の3つを保存対象とする)。
    saved_cursor_main: Option<SavedCursor>,
    saved_cursor_alt: Option<SavedCursor>,
    cursor_row: usize,
    cursor_col: usize,
    cur_attrs: TermAttrs,
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
    /// DA(`CSI c`/`CSI > c`)・DSR/CPR(`CSI 5n`/`CSI 6n`)への応答として、次に
    /// `take_pending_terminal_responses()`が呼ばれるまで蓄積される生バイト列のキュー
    /// (`pending_clipboard_write`と同型のパターン。タスク#38)。`SessionState::apply()`が
    /// これを既存の`SideEffect::SendStdin`に変換して送り返す——新しいtransport経路は
    /// 追加しない(`session_state.rs:10`の`SideEffect::SendStdin`→
    /// `transport/ssh_handler.rs`の`TransportCommand::WriteStdin`が既存の応答送信経路)。
    pending_terminal_responses: Vec<Vec<u8>>,
    pending_scrollback: Vec<Vec<TermCell>>,
    application_cursor_mode: bool,
    /// DECKPAM/DECKPNM(`ESC =`/`ESC >`、タスク#43)によるアプリケーションキーパッド
    /// モード。既定は`false`(numeric keypad mode)。`application_cursor_mode`
    /// (DECCKM)とは独立したモードで、テンキー(0-9・`,`・`-`・`.`・Enter・`+`・`*`・
    /// `/`・`=`)がSS3形式(`ESC O <letter>`)を送るかリテラル文字を送るかを切り替える
    /// (実際のキーイベントのエンコードはUI層のキーエンコーダーが担う——
    /// `application_cursor_mode`と同じ役割分担、`ScreenUpdate`経由でUI層へ公開する)。
    application_keypad_mode: bool,
    bracketed_paste_mode: bool,
    /// DEC Synchronized Output(`CSI ?2026h`/`CSI ?2026l`)。有効な間、リモートは
    /// 「これから送る一連の書き込みは1フレームとして扱ってほしい」と宣言している。
    /// `Terminal`自身は「今アクティブか」だけを保持し、実際の画面更新push抑制は
    /// `session_state.rs::SessionState::apply`(`screen_dirty`のgating)・safety-net
    /// タイムアウトは`session.rs`(tokioタイマー)が担う——`Terminal`はtokioに依存
    /// しない同期層のままにするため、ここに抑制ロジックそのものは置かない。
    synchronized_output_active: bool,
    /// 直近の`ScreenUpdate`発行以降に「行単位の差分では表現できない」構造的変更
    /// (スクロール・IL/DL・SU/SD・リサイズ・alt画面切替・RIS)が起きたことを表す
    /// ワンショットフラグ(タスク#93)。`session_state.rs::SessionState::make_screen_update`
    /// が発行時に取り出して(`take_full_damage_pending`)クリアし、立っていれば行差分の
    /// 計算をスキップして全画面dirty(`dirty_rows = None`)を報告する。座標がずれる
    /// 変更を通して差分を取ろうとしないという、Alacritty/iTerm2/xterm.jsが共通して
    /// 採る設計に倣う。`pending_scrollback`/`pending_rows`の有無からスクロールを
    /// 推測しない理由は、それらがtop/main regionのスクロール時にしか積まれず、
    /// alt画面・非ゼロDECSTBM・SU/SD・IL/DLが座標をずらしても積まれないため。
    full_damage_pending: bool,
    /// フォーカスレポーティング(`CSI ?1004h`/`l`、タスク#60)。vim/tmuxが`FocusGained`/
    /// `FocusLost` autocmdの発火に使う。有効時のみ[encode_focus_event]がOS由来の
    /// フォーカス変化イベントを`CSI I`/`CSI O`へエンコードする(無効時は`None`を返し
    /// 何も送らない)。既定はoff——`bracketed_paste_mode`/`mouse_reporting_mode`と同じ
    /// DECモード保持基盤(#36)のパターンに倣う。
    focus_reporting_mode: bool,
    /// DECTCEM(`CSI ?25h`/`CSI ?25l`)によるカーソル表示/非表示状態。既定は表示(`true`)。
    /// vim/lessなどがカーソルを隠す指示を送るケースに対応するため、`ScreenUpdate`へ
    /// そのまま伝播する(`session.rs::make_screen_update`参照)。
    cursor_visible: bool,
    /// BEL(0x07)を受信するたびに単調増加するカウンタ。`ScreenUpdate::bell_generation`
    /// としてそのまま公開する——bool ではなく世代カウンタにすることで、conflated
    /// チャネル越しに複数回の BEL が1つの `ScreenUpdate` にまとめられても呼び出し側が
    /// 「前回より値が進んだか」で取りこぼしを検知でき、同一 `ScreenUpdate` の再適用
    /// (例: 画面回転後の再描画)で二重にフィードバックが鳴るのも防げる。
    /// `reset_all`(RIS)では**意図的にリセットしない**——単調増加を維持する
    /// (Fableレビュー: タスク#24)。OSC終端の BEL(`ESC]0;title BEL`)は vte が
    /// ターミネータとして消費し `execute` には渡らないため、ここではカウントされない
    /// (この仕様はテストで明記する)。
    bell_generation: u64,
    /// DECSCUSR(`CSI Ps SP q`)で選択されたカーソル形状。既定は`Block`。
    cursor_shape: CursorShape,
    /// カーソルが点滅すべきか。DECSCUSRのパラメータ(奇数=blink/偶数=steady、
    /// 0はblinking blockと同義)、およびDECSET/DECRST `?12`(`CSI ?12h`/`CSI ?12l`、
    /// タスク#55、形状は変えず点滅の有無だけを切り替える)の両方がこのフィールドを
    /// 更新する(`CursorShape`とは独立)。
    cursor_blink: bool,
    /// DECAWM(`CSI ?7h`/`CSI ?7l`)。行右端に到達した際に自動折り返しするかどうか。
    /// 既定はxterm同様`true`(on)。offの場合、右端到達後の`print()`は次行へ
    /// 折り返さず、右端の最終列を上書きし続ける(タスク#56)。
    autowrap_mode: bool,
    /// DECOM(`CSI ?6h`/`CSI ?6l`、origin mode)。既定は`false`(off)。onの間、
    /// 絶対カーソル位置指定(CUP/HVP `H`/`f`、VPA `d`)とCPR(`CSI 6n`)応答の行座標、
    /// および相対カーソル移動(CUU/CUD/CNL/CPL `A`/`B`/`E`/`F`)の可動範囲は、画面全体
    /// ではなく現在のscroll region([scroll_top, scroll_bottom])基準になる(タスク#59)。
    /// 左右マージン(DECLRMM)はこのコードベースに未実装なので列方向は影響を受けない
    /// (`CSI s`のコメント参照)。モード切り替え自体(`h`/`l`どちらでも)でカーソルを
    /// home位置(on: `scroll_top`行、off: 0行目。いずれも列0)へ移動する——実端末
    /// (xterm含む)の挙動に倣う。
    origin_mode: bool,
    /// REP(`CSI Ps b`)が繰り返す対象——`print()`が実際にセルへ書き込んだ最後の
    /// graphic文字と、その時点の(reverse適用前の論理)SGR属性のペア(結合文字・幅0の
    /// 文字は対象外、[Perform::print]の幅0分岐を参照)。`None`は「直前にgraphic文字が
    /// 一度も書かれていない」状態(初期化直後・RIS直後)を表し、その状態でのREPは
    /// no-opにする(ECMA-48の「直前のgraphic文字を繰り返す」という定義上、対象が
    /// 存在しない場合の自然なフォールバックとして採用。タスク#48)。
    ///
    /// 属性も一緒に凍結して保持する(現在の`cur_attrs`ではなく、記録時点のものを
    /// REP実行時に使う)——元の文字を書いた後にSGRが変わっていても、REPは
    /// 「その文字が実際に画面に描かれた見た目」をそのまま繰り返すべきであり、
    /// REPを実行した時点で偶然有効なSGRに化けてはいけないため(タスク#48要件:
    /// 「直前に描画した文字・属性」を保持する)。
    ///
    /// 改行・カーソル移動・SGR等の制御機能を挟んでもクリアしない — xterm/VTE系実装の
    /// 一般的な挙動(REPは「最後に画面へ書かれたgraphic文字」を覚え続け、CR/LF等の
    /// 制御機能はそれを消さない)に合わせる。この値を書き込むのは`print()`の
    /// 非結合文字分岐のみ。
    ///
    /// 3つ目の要素(`Option<u32>`)は記録時点の`active_link_id`(タスク#40)。SGR属性と
    /// 同様、REPは「その文字が実際に画面に描かれた見た目」を再現すべきなので、
    /// REP実行時点で偶然開いている/閉じているハイパーリンク状態ではなく記録時点の
    /// ものを使う。
    last_graphic_cell: Option<(char, TermAttrs, Option<u32>)>,
    /// `ESC ( <final>`で指定されたG0文字セット。既定はASCII(タスク#41)。
    g0_charset: Charset,
    /// `ESC ) <final>`で指定されたG1文字セット。既定はASCII(タスク#41)。
    g1_charset: Charset,
    /// SI(0x0F)/SO(0x0E)によるGL(印字時に実際に使われる文字集合)の選択状態。
    /// `false`=G0(既定、SI相当)、`true`=G1(SO相当)。`print()`はこのフラグで
    /// `g0_charset`/`g1_charset`のどちらを適用するか決める(タスク#41)。
    gl_is_g1: bool,
    /// DECSET/DECRST `?1000`/`?1002`/`?1003`(タスク#36)。既定は`Off`。
    /// `csi_dispatch`の`is_dec`分岐で更新され、`ScreenUpdate::mouse_reporting_mode`
    /// としてそのまま公開する(rust-ssot: どのタッチ/ジェスチャイベントを
    /// マウスレポートとして送るべきかの判断材料はRust側が保持する)。
    mouse_reporting_mode: MouseReportingMode,
    /// DECSET/DECRST `?1006`(SGR拡張マウスレポーティング、タスク#36)。既定は`false`
    /// (レガシーX10形式)。`encode_pointer_event`がこの値でエンコード形式を切り替える。
    sgr_mouse_mode: bool,
    /// DECSET/DECRST `?1007`(Alternate Scroll): alt screen でホイールを矢印キーに
    /// 変換する。既定は`false`。
    alternate_scroll: bool,
    /// DECSET/DECRST `?1015`(URXVTマウスエンコーディング): マウスレポートを
    /// `CSI Cb ; Cx ; Cy M`形式でエンコードする。`?1006`(SGR)と排他ではない。
    /// 既定は`false`。
    urxvt_mouse_mode: bool,
    /// OSC 8(`ESC]8;params;URIST`、タスク#40)で現在開いているハイパーリンクの
    /// intern id(`link_table`の index)。`None`はリンクなし。`print()`が書き込む
    /// 全セルへそのまま付与する——SGR属性(`cur_attrs`)とは独立な状態であり、
    /// `ESC[0m`等のSGRリセットでは閉じない。閉じるのは(1) 空URIのOSC 8
    /// (`ESC]8;;ST`)、(2) `reset_all`(RIS)、(3) alt screenへの切替/復帰
    /// (`switch_to_alt`/`switch_to_main`)のいずれか。
    active_link_id: Option<u32>,
    /// OSC 8で見たURL文字列のintern表。index(=`u32`)が`TermCell::link_id`/
    /// `active_link_id`の値そのもの。同じURLは`intern_link`で重複排除して
    /// 同じidを再利用する。`reset_all`(RIS)でもクリアしない——scrollbackへ
    /// 既に流れた過去のセルの`link_id`がこの表のindexを指し続けているため、
    /// RIS後にindexを再利用してしまうと過去セルが別URLを指す破損が起きる
    /// (Fableレビュー2次の設計方針: アクティブリンク状態のみクリアし、表自体は
    /// セッション生存期間中インデックスの安定性を保つ)。
    link_table: Vec<String>,
    /// `link_table`への逆引き(URL文字列→既存id)。`intern_link`の重複排除にのみ
    /// 使う内部キャッシュで、`link_table`と要素数・内容が常に対応する。
    link_ids: HashMap<String, u32>,
    /// HTS(`ESC H`)/TBC(`CSI g`)による可変タブストップ(タスク#61)。
    /// `tab_stops[col] == true`はその列がタブストップであることを示す。長さは
    /// 常に`cols`と一致する。既定は8列おきの固定タブ(`col % 8 == 0`、以前の
    /// HT/CHT/CBTの固定8桁計算——`(col/8+1)*8`——と等価なパターン。alacritty等の
    /// 実装同様、列0も既定でタブストップに含める。CBTが列0まで戻れる/HTが列0を
    /// 飛び越さないという既存挙動には影響しない)。HT(0x09)/CHT(`CSI Ps I`)/
    /// CBT(`CSI Ps Z`)は`next_tab_stop`/`prev_tab_stop`経由でこの状態を参照する。
    tab_stops: Vec<bool>,
    /// Sixel(`DCS Pa;Pb;Ph q ... ST`、タスク#42)を`hook`〜`unhook`の間デコード中の
    /// 状態。`hook`で`c == 'q'`の時のみ`Some`になり、`unhook`で`take()`されて
    /// 消費される(sixel以外のDCS、例えば将来のReGIS等は無視するため`None`のまま)。
    sixel_decoder: Option<SixelDecoder>,
    /// 現在アクティブな画像配置。`ScreenUpdate::images`としてそのまま公開する
    /// (`session.rs::make_screen_update`参照)。寿命管理の詳細は
    /// [Terminal::clear_images]・[Terminal::place_sixel_image]参照。
    images: Vec<ImagePlacement>,
    /// 次に発行する画像id。RIS(`reset_all`)でも**リセットしない**——`link_table`と
    /// 同じ理由で、過去に呼び出し側へ渡したidを再利用すると別画像との衝突が
    /// 起こりうるため単調増加を保つ。
    next_image_id: u64,
    /// Kitty graphics protocol(#53)のAPCデコード状態(チャンク分割転送の途中組み立て
    /// を保持する)。実際のAPC切り出しは`session_state.rs`の`ApcInterceptor`が行い、
    /// 完成したペイロードを`dispatch_kitty_apc`経由でここへ渡す。
    kitty: KittyGraphics,
    /// `images`内のKitty画像について、内部placement id → Kitty client image id(`i=`)の
    /// 対応表。`i=`を伴わないKitty画像はここに載らない(client idが無いため)。
    /// `d=i`/`d=I`(id指定削除)の対象特定にのみ使う——`d=a`(全削除)は代わりに
    /// [Self::kitty_placement_ids] を使う(Opusレビュー指摘: このmapのkeysだけを
    /// 全削除対象にすると、`i=`を送らないクライアント[chafa等]が置いた画像が
    /// 永久に消せなくなる)。
    kitty_id_by_placement: HashMap<u64, u64>,
    /// `images`内の、Kitty経由(`i=`の有無を問わず)で置かれた全placement idの集合。
    /// `d=a`(全削除)の対象特定に使う。sixel画像はここに載らない(=Kittyの全削除が
    /// sixelを巻き込まない)。
    kitty_placement_ids: std::collections::HashSet<u64>,
    /// Kitty keyboard protocol(タスク#54、
    /// <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>)の progressive
    /// enhancement flags スタック(main画面用)。各要素はその時点でpushされた
    /// flagsのビットマスク(bit0=disambiguate escape codes、bit1=report event
    /// types、bit2=report alternate keys、bit3=report all keys as escape
    /// codes、bit4=report associated text)。「現在有効なflags」はスタック最上段
    /// (空なら0=legacy mode)。このコアが担うのは、リモートが送ってくる
    /// push/pop/set/queryシーケンス(`csi_dispatch`の`u`分岐参照)を解釈して
    /// 現在のnegotiated flagsを`ScreenUpdate::kitty_keyboard_flags`として公開する
    /// ところまで(rust-ssot: 「どのflagsが有効か」の判断・保持はRust側に一元化し、
    /// Kotlin/Swift側にミラー状態は作らない)。bit0(disambiguate escape codes)の
    /// Escapeキーについては、この値を[crate::terminal_special_key_bytes]へそのまま
    /// 渡すことで実際のCSI `u`エンコードもRust側のSSOT関数が行う(#29の修飾キーCSI
    /// エンコードと同じ役割分担——タスク#54実装当初はこの引数配線が抜けていた
    /// バグをタスク#72で修正、詳細は`ScreenUpdate::kitty_keyboard_flags`のdocコメント
    /// 参照)。
    ///
    /// mainとaltで**独立したスタック**を持つ(仕様: "The main and alternate
    /// screens in the terminal emulator must maintain their own, independent,
    /// keyboard mode stacks"、Fable 2次レビュー指摘)。`switch_to_alt`/
    /// `switch_to_main`は`cells()`/`cells_mut()`と同様`alt_active`で参照先を
    /// 切り替えるだけでよく、切替自体で中身を保存/復元する必要はない
    /// (`main_cells`/`alt_cells`と同型のパターン)。
    main_kitty_flags_stack: Vec<u16>,
    /// [Terminal::main_kitty_flags_stack]のalt画面版。
    alt_kitty_flags_stack: Vec<u16>,
    /// OSC 133(タスク#13)の絶対行番号カウンタ。`Terminal`の生存期間を通じて、
    /// scrollbackへ実際に押し出された行数(`pending_scrollback`へpushした回数と
    /// 常に一致する——`scroll_up_region`/`resize_preserving_state`の該当箇所で
    /// 同時にインクリメントする)を単調増加でカウントする。ある時点のカーソル行の
    /// 「絶対行番号」は`total_scrolled_lines + cursor_row`で求まり、後から
    /// scrollbackが`SCROLLBACK_LIMIT`でトリミングされても(`total_scrolled_lines`
    /// 自体はトリミングの影響を受けないため)位置を再計算できる。RIS(`reset_all`)
    /// でも`link_table`と同じ理由でリセットしない——scrollbackへ既に流れた過去の
    /// プロンプトマークの`row`がこのカウンタを基準にしているため。
    total_scrolled_lines: u64,
    /// OSC 133(タスク#13)のマーク履歴。`MAX_PROMPT_MARKS`件を超えたら最も古い
    /// ものから捨てる(`link_table`と異なりRIS(`reset_all`)ではクリアしない——
    /// `total_scrolled_lines`と同じ理由)。「前/次のプロンプトへジャンプ」
    /// (`prompt_jump_target`)・「直前コマンド出力コピー」の判定に使う。
    prompt_marks: VecDeque<PromptMark>,
    /// 現在「コマンド入力中」(OSC 133;Bは来たが;Cがまだ来ていない区間)かどうか。
    /// タップでのカーソル移動(`cursor_move_bytes_for_click`、タスク#13)が
    /// 対象とする行を絞り込むために使う——タップされた行が現在のカーソル行と
    /// 一致し、かつこのフラグが立っている場合のみ有効なタップとみなす(入力行の
    /// 絶対行番号を別途保持する必要はない——現在のカーソル行が常に「今アクティブな
    /// 入力行」を指すため)。
    input_line_active: bool,
    /// OSC 133;C(コマンド実行開始)〜;D(コマンド終了)の間、`true`の間だけ
    /// `print()`で書かれた文字を[current_output_line]/[current_output_lines]へ
    /// そのまま貯める(タスク#13「直前コマンドの出力だけをコピー」)。
    ///
    /// スコープ外(意図的な制約): カーソル移動を伴う書き換え(`\r`によるプログレス
    /// バー上書き・`top`等のフルスクリーンTUI)はそのまま連結されてしまい、見た目の
    /// 最終状態を再現しない。グリッド上の実際の表示内容を都度読み直す設計にすれば
    /// 正確になるが、scrollbackと現在画面をまたぐ座標変換が必要になり複雑化するため、
    /// タスク#13ではストリームをそのまま捕捉する単純な実装に留めた(単純な行指向の
    /// 出力——`ls`/`cat`/`git log`等の大半のユースケース——では正しく動く)。
    capturing_command_output: bool,
    /// [capturing_command_output]中に貯めている、まだ改行(LF/VT/FF)が来ていない
    /// 現在行のテキスト。
    current_output_line: String,
    /// [capturing_command_output]中に確定した行の列。
    current_output_lines: Vec<String>,
    /// 直近に完了した1コマンド分の出力(タスク#13)。新しいコマンドが
    /// `OSC 133;C`で始まると、次の`;D`到達時にこの値が上書きされる——「直前の」
    /// 1件のみを保持する設計(複数コマンド分の履歴は持たない)。
    last_command_output: Option<Vec<String>>,
}

/// Kitty keyboard protocolのflagsスタック(タスク#54)の最大深さ。仕様は
/// 具体的な上限値を規定していない("Terminals should limit the size of the
/// stack as appropriate")ため、他の主要実装が採用する保守的な値に倣う。
/// 上限を超えるpushは最も古いエントリ(index 0)を追い出す(仕様: "If a push
/// request is received and the stack is full, the oldest entry from the
/// stack must be evicted")。
const KITTY_KEYBOARD_STACK_MAX: usize = 8;

/// 既定のタブストップパターン(8列おき、列0を含む)を`cols`幅分生成する。
/// `Terminal::new`・`reset_all`(RIS)・`resize_preserving_state`(新しく増えた列分)
/// で使う。
fn default_tab_stops(cols: usize) -> Vec<bool> {
    (0..cols).map(|c| c % 8 == 0).collect()
}

/// UI層(#50/#51)からRustへ渡す、座標付きの生ポインタイベント(rust-ssot:
/// 「今どのマウスモードか」「このイベントを報告すべきか」の判断はUI層に持たせず、
/// Rust側の[Terminal::encode_pointer_event]が[MouseReportingMode]/SGRモードを
/// 見て一元的に行う)。`row`/`col`は0-basedのセル座標(画面外の値は
/// `encode_pointer_event`側でクランプする)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PointerEvent {
    pub(crate) row: usize,
    pub(crate) col: usize,
    pub(crate) kind: MouseEventKind,
    /// `Motion`かつボタンを押していない単純な移動の場合のみ`None`。
    pub(crate) button: Option<MouseButton>,
    pub(crate) modifiers: TerminalKeyModifiers,
}

/// [MouseButton]をxterm mouse protocolの「ボタン番号」フィールド(0〜3、または
/// wheel用の64/65)へ変換する。`None`(ボタン無しの移動、またはレガシー形式での
/// release)は`3`(xterm仕様上「no button」を表す予約値)。
fn mouse_button_base_code(button: Option<MouseButton>) -> u8 {
    match button {
        Some(MouseButton::Left) => 0,
        Some(MouseButton::Middle) => 1,
        Some(MouseButton::Right) => 2,
        None => 3,
        Some(MouseButton::WheelUp) => 64,
        Some(MouseButton::WheelDown) => 65,
        Some(MouseButton::WheelLeft) => 66,
        Some(MouseButton::WheelRight) => 67,
    }
}

/// xterm mouse protocolの修飾子ビット: Shift(4) / Meta(8) / Ctrl(16)。
/// `TerminalKeyModifiers::meta`(Windows/Cmdキー)はxterm mouse protocolに
/// 対応するビットが無いため使わない——`alt`をxterm用語の"Meta"ビットに割り当てる
/// (xterm自身の実装がAltキーをこのビットに使っているのに倣う)。
fn mouse_modifier_bits(m: TerminalKeyModifiers) -> u8 {
    (if m.shift { 4 } else { 0 }) | (if m.alt { 8 } else { 0 }) | (if m.ctrl { 16 } else { 0 })
}

/// [PointerEvent]を、`cols`/`rows`(端末の実サイズ)・`mode`([MouseReportingMode])・
/// `sgr`(`?1006`有効か)に従ってターミナルへ送るべきバイト列にエンコードする
/// (タスク#36)。[Terminal::encode_pointer_event]から`self`の対応フィールドを渡して
/// 呼ばれるほか、`lib.rs::terminal_pointer_event_bytes`(タスク#51、UI層が
/// `ScreenUpdate`から読んだ`mouse_reporting_mode`/`sgr_mouse_mode`/`cols`/`rows`を
/// そのまま渡す`#[uniffi::export]`関数)からも直接呼ばれる——`Terminal`インスタンス
/// (実行中セッション)を経由せずにこのエンコードロジックだけをUI層から使いたい
/// (rust-ssot: 「今どのマウスモードか」の判断はここに一元化し、UI層にミラーしない)
/// ため、状態を持たない自由関数として切り出してある。報告すべきでないイベント
/// (モードがOff、またはモードが対象外のイベント種別)は`None`を返す——
/// 呼び出し元はこれを「何も送らない」の合図として扱えばよい。
///
/// # モードごとの報告対象(xterm互換)
/// - `Off`: 何も報告しない。
/// - `Normal`(`?1000`): press/releaseのみ(移動は報告しない)。
/// - `ButtonEvent`(`?1002`): 上記に加え、ボタンを押したままの移動(drag、
///   `button.is_some()`)のみ報告する。ボタン無しの単純な移動は無視する。
/// - `AnyEvent`(`?1003`): ボタン状態に関係なく全ての移動を報告する。
///
/// ホイール(`WheelUp`/`WheelDown`)は常に`Press`種別として渡される前提
/// (releaseの概念が無いため)で、`Normal`を含む全モードで報告される
/// (press/release扱いの分岐に乗るため)。
///
/// # エンコード形式
/// - `?1006`(SGR)有効時: `ESC [ < Cb ; Cx ; Cy M`(press/drag)または
///   `ESC [ < Cb ; Cx ; Cy m`(release)。座標は1-based・10進数で桁数の
///   制限が無い。releaseでもどのボタンが離されたかを`Cb`にそのまま残せる
///   (`M`/`m`の違いだけでpress/releaseを区別する)。
/// - `?1006`無効時(レガシーX10形式): `ESC [ M Cb Cx Cy`(3バイトとも
///   `値+32`の単一バイト)。仕様上1バイトにしかエンコードできないため、
///   座標は`223`(`255 - 32`)で頭打ちにクランプする——`1000`だけ有効で
///   `1006`を送らないアプリ(古いtmux等)向けの互換性を意図的に実装する
///   判断(Fableレビュー指摘: 割り切って未実装にするのではなくクランプして
///   実装する)。また、レガシー形式は「どのボタンが離されたか」を表現できず
///   仕様上常に`3`(no button)を報告する(SGRとの意図的な差)。
pub(crate) fn encode_pointer_event_bytes(
    event: PointerEvent,
    cols: usize,
    rows: usize,
    mode: MouseReportingMode,
    sgr: bool,
    urxvt: bool,
) -> Option<Vec<u8>> {
    let reportable = match event.kind {
        MouseEventKind::Press | MouseEventKind::Release => mode != MouseReportingMode::Off,
        MouseEventKind::Motion => match mode {
            MouseReportingMode::Off | MouseReportingMode::Normal => false,
            MouseReportingMode::ButtonEvent => event.button.is_some(),
            MouseReportingMode::AnyEvent => true,
        },
    };
    if !reportable {
        return None;
    }

    let base = mouse_button_base_code(event.button);
    let modifier_bits = mouse_modifier_bits(event.modifiers);
    let motion_bit = if event.kind == MouseEventKind::Motion { 0x20 } else { 0 };
    let col = event.col.min(cols.saturating_sub(1));
    let row = event.row.min(rows.saturating_sub(1));

    if urxvt && !sgr {
        // URXVT encoding: `CSI Cb ; Cx ; Cy M` (press) / `CSI Cb ; Cx ; Cy m` (release).
        // Cb is button+modifiers+motion, Cx/Cy are 1-based decimal coordinates.
        let cb = base as u32 + modifier_bits as u32 + motion_bit as u32;
        let terminator = if event.kind == MouseEventKind::Release { 'm' } else { 'M' };
        Some(format!("\x1b[{};{};{}{}", cb, col + 1, row + 1, terminator).into_bytes())
    } else if sgr {
        let cb = base as u32 + modifier_bits as u32 + motion_bit as u32;
        let terminator = if event.kind == MouseEventKind::Release { 'm' } else { 'M' };
        Some(format!("\x1b[<{};{};{}{}", cb, col + 1, row + 1, terminator).into_bytes())
    } else {
        // レガシーX10形式: releaseはどのボタンだったか表現できないため
        // 仕様上常に`3`(no button)を使う。
        let legacy_base = if event.kind == MouseEventKind::Release { 3 } else { base };
        let cb = (legacy_base as u32 + modifier_bits as u32 + motion_bit as u32).min(255 - 32) as u8;
        let clamp_coord = |v: usize| -> u8 { (v.min(223 - 1) as u8) + 1 + 32 };
        Some(vec![0x1B, b'[', b'M', 32 + cb, clamp_coord(col), clamp_coord(row)])
    }
}

impl Terminal {
    /// `theme`はこのセッション(タブ)が使う配色のスナップショット。呼び出し元
    /// (`SessionState`/`SessionCore`)が「グローバル既定を使うか、プロファイル/タブ固有の
    /// 上書きを使うか」を解決した結果をそのまま渡す。
    pub(crate) fn new(cols: usize, rows: usize, theme: Theme) -> Self {
        let blank = blank_cell_for_theme(&theme);
        let cells = vec![blank.clone(); cols * rows];
        let tab_stops = default_tab_stops(cols);
        Terminal {
            theme,
            cols, rows,
            main_cells: cells.clone(),
            alt_cells: cells,
            alt_active: false,
            saved_cursor_main: None,
            saved_cursor_alt: None,
            cursor_row: 0, cursor_col: 0,
            cur_attrs: TermAttrs::default_for(&theme),
            scroll_top: 0, scroll_bottom: rows - 1,
            title: None,
            pending_clipboard_write: None,
            pending_clipboard_pull_request: false,
            pending_terminal_responses: Vec::new(),
            pending_scrollback: Vec::new(),
            application_cursor_mode: false,
            application_keypad_mode: false,
            bracketed_paste_mode: false,
            synchronized_output_active: false,
            full_damage_pending: false,
            focus_reporting_mode: false,
            cursor_visible: true,
            bell_generation: 0,
            cursor_shape: CursorShape::Block,
            cursor_blink: true,
            autowrap_mode: true,
            origin_mode: false,
            last_graphic_cell: None,
            g0_charset: Charset::Ascii,
            g1_charset: Charset::Ascii,
            gl_is_g1: false,
            mouse_reporting_mode: MouseReportingMode::Off,
            sgr_mouse_mode: false,
            alternate_scroll: false,
            urxvt_mouse_mode: false,
            active_link_id: None,
            link_table: Vec::new(),
            link_ids: HashMap::new(),
            tab_stops,
            sixel_decoder: None,
            images: Vec::new(),
            next_image_id: 0,
            kitty: KittyGraphics::new(),
            kitty_id_by_placement: HashMap::new(),
            kitty_placement_ids: std::collections::HashSet::new(),
            main_kitty_flags_stack: Vec::new(),
            alt_kitty_flags_stack: Vec::new(),
            total_scrolled_lines: 0,
            prompt_marks: VecDeque::new(),
            input_line_active: false,
            capturing_command_output: false,
            current_output_line: String::new(),
            current_output_lines: Vec::new(),
            last_command_output: None,
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

    /// 保留中の端末応答(DA/DSR/CPR等)を取り出す。呼び出し後は空になる
    /// (`take_scrollback`/`take_pending_clipboard_write`と同じ「1バッチ分をここで
    /// フラッシュする」パターン)。
    pub(crate) fn take_pending_terminal_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending_terminal_responses)
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
    pub(crate) fn application_keypad_mode(&self) -> bool { self.application_keypad_mode }
    pub(crate) fn bracketed_paste_mode(&self) -> bool { self.bracketed_paste_mode }

    /// DEC Synchronized Output(`?2026`)が現在アクティブ(`CSI ?2026h`済み・
    /// `CSI ?2026l`未受信)かどうか。`session_state.rs::SessionState::apply`が
    /// `screen_dirty`のgatingに使う。
    pub(crate) fn synchronized_output_active(&self) -> bool { self.synchronized_output_active }

    /// safety-netタイムアウト(`session.rs`、`CSI ?2026l`が来ないままハングした
    /// 場合の強制解除)専用。通常の解除は`?2026l`自体がDECSET/DECRSTループを
    /// 経由して行う——ここは「リモートを信用せず強制的に戻す」経路であることを
    /// 名前で明示するため、通常のsetterと分けている。
    pub(crate) fn force_end_synchronized_output(&mut self) {
        self.synchronized_output_active = false;
    }

    /// 構造的変更(スクロール・IL/DL・リサイズ・alt画面切替・RIS)で立った全画面
    /// dirtyフラグを取り出してクリアする(タスク#93)。`SessionState::make_screen_update`
    /// が発行のたびに1回だけ呼ぶ——[full_damage_pending]フィールドのdocコメント参照。
    pub(crate) fn take_full_damage_pending(&mut self) -> bool {
        std::mem::take(&mut self.full_damage_pending)
    }
    /// フォーカスレポーティング(`?1004`、タスク#60)の現在値。テストから参照する
    /// (`ScreenUpdate`へは公開しない——`encode_focus_event`がRust側で完結して判断する
    /// ため、UI層がこの値自体を必要としない)。
    pub(crate) fn focus_reporting_mode(&self) -> bool { self.focus_reporting_mode }
    pub(crate) fn mouse_reporting_mode(&self) -> MouseReportingMode { self.mouse_reporting_mode }
    pub(crate) fn sgr_mouse_mode(&self) -> bool { self.sgr_mouse_mode }
    pub(crate) fn alternate_scroll(&self) -> bool { self.alternate_scroll }
    pub(crate) fn urxvt_mouse_mode(&self) -> bool { self.urxvt_mouse_mode }
    pub(crate) fn cursor_visible(&self) -> bool { self.cursor_visible }
    pub(crate) fn bell_generation(&self) -> u64 { self.bell_generation }
    pub(crate) fn cursor_shape(&self) -> CursorShape { self.cursor_shape }
    pub(crate) fn cursor_blink(&self) -> bool { self.cursor_blink }
    /// DECAWM(`CSI ?7h`/`CSI ?7l`)の現在値。テスト・`print()`から参照する。
    pub(crate) fn autowrap_mode(&self) -> bool { self.autowrap_mode }
    /// DECOM(`CSI ?6h`/`CSI ?6l`)の現在値。テストから参照する。
    pub(crate) fn origin_mode(&self) -> bool { self.origin_mode }
    /// OSC 8(タスク#40)のURL intern表。`TermCell::link_id`/`Terminal::active_link_id`
    /// の値はこのスライスのindex。`session.rs::make_screen_update`が
    /// `ScreenUpdate::link_table`として丸ごと公開する。
    pub(crate) fn link_table(&self) -> &[String] { &self.link_table }
    /// Sixel(タスク#42)で現在アクティブな画像配置の一覧。`session.rs::make_screen_update`
    /// が`ScreenUpdate::images`としてそのまま公開する。
    pub(crate) fn images(&self) -> &[ImagePlacement] { &self.images }

    /// 現在アクティブな画面(main/alt)のKitty keyboard flagsスタック(タスク#54)。
    /// `cells()`/`cells_mut()`と同じ「`alt_active`でどちらのスロットを使うか選ぶ」
    /// パターン。
    fn kitty_flags_stack(&self) -> &Vec<u16> {
        if self.alt_active { &self.alt_kitty_flags_stack } else { &self.main_kitty_flags_stack }
    }

    fn kitty_flags_stack_mut(&mut self) -> &mut Vec<u16> {
        if self.alt_active { &mut self.alt_kitty_flags_stack } else { &mut self.main_kitty_flags_stack }
    }

    /// Kitty keyboard protocol(タスク#54)の現在有効なnegotiated flags(スタック
    /// 最上段、空なら0=legacy mode)。`session.rs::make_screen_update`が
    /// `ScreenUpdate::kitty_keyboard_flags`としてそのまま公開する——実際のキー
    /// エンコード判断はUI層(Kotlin/Swift)がこの値を見て行う([Terminal]の
    /// `main_kitty_flags_stack`フィールドdocコメント参照)。
    pub(crate) fn kitty_keyboard_flags(&self) -> u16 {
        self.kitty_flags_stack().last().copied().unwrap_or(0)
    }

    /// 現在アクティブな画像配置を全て消去する。呼び出し元は「画面内容が大きく
    /// 変わり、既存の画像配置がもはや正しい位置を指しているとは言えなくなる
    /// 操作」の直後に呼ぶ(RIS・スクロール・IL/DL・リサイズ・alt画面切替。
    /// `ImagePlacement`のdocコメントの「スコープ外」参照——誤った位置に画像が
    /// 取り残されるより消える方が安全側という判断)。
    fn clear_images(&mut self) {
        self.images.clear();
        self.kitty_id_by_placement.clear();
        self.kitty_placement_ids.clear();
    }

    /// 現在ライブな全Sixel画像の`rgba`バイト列の合計サイズ(バイト)。
    /// `place_sixel_image`の`MAX_TOTAL_IMAGE_RGBA_BYTES`判定に使う(タスク#90)。
    fn total_image_rgba_bytes(&self) -> usize {
        self.images.iter().map(|img| img.rgba.len()).sum()
    }

    /// デコードが完了したSixelビットマップを、現在のカーソル位置を左上として
    /// グリッドへ配置する(タスク#42)。行・列方向とも画面の残りサイズへクランプする
    /// (`ImagePlacement`のdocコメント「実装範囲」参照——画像による自動スクロールは
    /// 発生させず、画面下端でクリップする)。
    fn place_sixel_image(&mut self, img: SixelImage) {
        self.push_image(img.width, img.height, img.rgba);
    }

    /// デコード済みビットマップ(sixel/Kitty共通)をカーソル位置を左上としてグリッドへ
    /// 配置し、発行したplacement idを返す(タスク#42/#53)。行・列方向とも画面の残り
    /// サイズへクランプする(`ImagePlacement`のdocコメント「実装範囲」参照——画像による
    /// 自動スクロールは発生させず、画面下端でクリップする)。
    fn push_image(&mut self, width: usize, height: usize, rgba: Vec<u8>) -> u64 {
        let col = self.cursor_col.min(self.cols.saturating_sub(1));
        let row = self.cursor_row.min(self.rows.saturating_sub(1));
        let cols_span = (width + SIXEL_CELL_WIDTH_PX - 1) / SIXEL_CELL_WIDTH_PX;
        let rows_span = (height + SIXEL_CELL_HEIGHT_PX - 1) / SIXEL_CELL_HEIGHT_PX;
        let cols_span = cols_span.max(1).min(self.cols - col);
        let rows_span = rows_span.max(1).min(self.rows - row);

        let id = self.next_image_id;
        self.next_image_id = self.next_image_id.wrapping_add(1);
        self.images.push(ImagePlacement {
            id,
            row: row as u32,
            col: col as u32,
            rows_span: rows_span as u32,
            cols_span: cols_span as u32,
            width_px: width as u32,
            height_px: height as u32,
            rgba,
        });
        // 個数(`MAX_LIVE_IMAGES`)と合計バイト数(`MAX_TOTAL_IMAGE_RGBA_BYTES`)の
        // 両方を満たすまで、最も古い画像から順に捨てる(タスク#90)。新しく
        // push した画像自体は単体で`MAX_TOTAL_IMAGE_RGBA_BYTES`を超えることは
        // ない(`sixel::MAX_SIXEL_AREA`による単体上限が総バイト数上限より
        // 十分小さい)ため、このループが全画像を捨て切ることはない。
        while self.images.len() > MAX_LIVE_IMAGES
            || self.total_image_rgba_bytes() > MAX_TOTAL_IMAGE_RGBA_BYTES
        {
            let removed = self.images.remove(0);
            self.kitty_id_by_placement.remove(&removed.id);
            self.kitty_placement_ids.remove(&removed.id);
        }

        // カーソルは画像の左下(次行の先頭)へ移動する(近似——実端末実装は分かれる
        // が、xterm等広く使われる実装の挙動に合わせる)。
        self.cursor_row = (row + rows_span).min(self.rows.saturating_sub(1));
        self.cursor_col = 0;
        id
    }

    /// [crate::kitty_graphics::ApcInterceptor]が切り出した完成APCペイロード1件を処理
    /// する(#53)。デコード・削除の判断は`KittyGraphics`側で完結しており、ここでは
    /// その結果(配置/削除)を`images`へ反映するだけ(rust-ssot)。
    pub(crate) fn dispatch_kitty_apc(&mut self, payload: &[u8]) {
        match self.kitty.dispatch(payload) {
            KittyCommand::Place(img) => {
                let id = self.push_image(img.width, img.height, img.rgba);
                // `i=`(client image id)の有無によらず、Kitty経由の全placementを
                // ここへ記録する(`d=a`全削除の対象特定用。Opusレビュー指摘:
                // 以前はkitty_idが無い画像がここに載らず、d=aで永久に消せなかった)。
                self.kitty_placement_ids.insert(id);
                if let Some(kid) = img.kitty_id {
                    self.kitty_id_by_placement.insert(id, kid);
                }
            }
            KittyCommand::DeleteAll => {
                let ids = std::mem::take(&mut self.kitty_placement_ids);
                self.images.retain(|img| !ids.contains(&img.id));
                self.kitty_id_by_placement.clear();
            }
            KittyCommand::DeleteId(kid) => {
                let placements: std::collections::HashSet<u64> = self
                    .kitty_id_by_placement
                    .iter()
                    .filter(|(_, v)| **v == kid)
                    .map(|(k, _)| *k)
                    .collect();
                self.images.retain(|img| !placements.contains(&img.id));
                self.kitty_id_by_placement.retain(|k, _| !placements.contains(k));
                self.kitty_placement_ids.retain(|k| !placements.contains(k));
            }
            KittyCommand::None => {}
        }
    }

    /// HT(0x09)/CHT(`CSI Ps I`、タスク#61)共通: `col`より右にある最も近いタブストップを
    /// 返す。存在しなければ最終列(`cols - 1`)。`col`が`cols`(delayed wrapによる
    /// 画面外の折り返し待ち位置)でも、探索範囲`(col+1)..cols`が空になり自然に
    /// `cols - 1`へ正規化される——HT/CHTの旧固定8桁実装が持っていた「計算してから
    /// クランプ」の順序と同じ挙動になる。
    fn next_tab_stop(&self, col: usize) -> usize {
        ((col + 1)..self.cols)
            .find(|&c| self.tab_stops[c])
            .unwrap_or_else(|| self.cols.saturating_sub(1))
    }

    /// CBT(`CSI Ps Z`、タスク#61)用: `col`より左にある最も近いタブストップを返す。
    /// 存在しなければ列0。
    fn prev_tab_stop(&self, col: usize) -> usize {
        (0..col).rev().find(|&c| self.tab_stops[c]).unwrap_or(0)
    }

    /// origin modeが有効な間、絶対/相対カーソル移動の座標基準として使う行範囲
    /// `[top, bottom]`(0-indexed、画面全体の座標系)。offの場合は画面全体
    /// (`0..=rows-1`)。
    fn origin_row_bounds(&self) -> (usize, usize) {
        if self.origin_mode {
            (self.scroll_top, self.scroll_bottom)
        } else {
            (0, self.rows.saturating_sub(1))
        }
    }
    pub(crate) fn screen_cells(&self) -> &[TermCell] { self.cells() }

    fn reset_all(&mut self) {
        let theme = self.theme;
        let blank = blank_cell_for_theme(&theme);
        let cells = vec![blank; self.cols * self.rows];
        self.main_cells = cells.clone();
        self.alt_cells = cells;
        self.alt_active = false;
        self.saved_cursor_main = None;
        self.saved_cursor_alt = None;
        self.cursor_row = 0; self.cursor_col = 0;
        self.cur_attrs = TermAttrs::default_for(&theme);
        self.scroll_top = 0; self.scroll_bottom = self.rows - 1;
        self.title = None;
        self.pending_clipboard_write = None;
        self.pending_clipboard_pull_request = false;
        self.pending_terminal_responses.clear();
        self.application_cursor_mode = false;
        self.application_keypad_mode = false;
        self.bracketed_paste_mode = false;
        self.synchronized_output_active = false;
        // RISは画面全体を初期化する——行差分では追随できないので全画面dirty(#93)。
        self.full_damage_pending = true;
        self.focus_reporting_mode = false;
        self.cursor_visible = true;
        self.cursor_shape = CursorShape::Block;
        self.cursor_blink = true;
        self.autowrap_mode = true;
        self.origin_mode = false;
        self.last_graphic_cell = None;
        self.g0_charset = Charset::Ascii;
        self.g1_charset = Charset::Ascii;
        self.gl_is_g1 = false;
        self.mouse_reporting_mode = MouseReportingMode::Off;
        self.sgr_mouse_mode = false;
        self.alternate_scroll = false;
        self.urxvt_mouse_mode = false;
        // アクティブなハイパーリンク状態のみクリアする。`link_table`自体は
        // クリアしない([Terminal]の`link_table`フィールドdocコメント参照)。
        self.active_link_id = None;
        // RIS(`ESC c`)はタブストップも既定(8列おき)へ戻す(実端末の挙動、タスク#61)。
        self.tab_stops = default_tab_stops(self.cols);
        // Sixel(タスク#42): RISは画面内容自体を全消去するため、画像配置も道連れに
        // 消す(`next_image_id`自体はリセットしない、[Terminal]のdocコメント参照)。
        self.clear_images();
        // Kitty keyboard protocol(タスク#54): RISはmain/alt両方のflagsスタックを
        // 空(legacy mode)へ戻す。他のDECモード([mouse_reporting_mode]等)と同じく
        // RISはセッション全体の状態を初期化する操作なので、main/alt独立という
        // 仕様上の制約とは矛盾しない(両方を初期化するだけ)。
        self.main_kitty_flags_stack.clear();
        self.alt_kitty_flags_stack.clear();
        // OSC 133(タスク#13): `total_scrolled_lines`/`prompt_marks`/`last_command_output`
        // は`link_table`と同じ理由でRISでもクリアしない(過去マークの絶対行番号の
        // 意味が変わってしまうため)。「今」の一時的な状態のみリセットする。
        self.input_line_active = false;
        self.capturing_command_output = false;
        self.current_output_line.clear();
        self.current_output_lines.clear();
    }

    fn cells(&self) -> &Vec<TermCell> {
        if self.alt_active { &self.alt_cells } else { &self.main_cells }
    }

    fn cells_mut(&mut self) -> &mut Vec<TermCell> {
        if self.alt_active { &mut self.alt_cells } else { &mut self.main_cells }
    }

    /// erase/scroll/リサイズパディング用の空白セル。現在の SGR 属性(色・reverse等)
    /// を引き継ぐ — `blank_cell_for_theme` (画面全体リセット用、属性なし)とは別物。
    fn blank(&self) -> TermCell {
        self.cur_attrs.to_cell(smol_str::SmolStr::new_inline(" "))
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
        // リサイズは全セルの座標が変わる——行差分では追随できないので全画面dirty(#93)。
        // (`make_screen_update`側でも寸法不一致を全画面dirty扱いにするが、意図を明示
        // するため構造的変更の一つとしてここでも立てる。)
        self.full_damage_pending = true;

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
            self.saved_cursor_main.map(|c| c.row).unwrap_or(0)
        } else {
            self.cursor_row
        };
        let alt_reference_row = if self.alt_active {
            self.cursor_row
        } else {
            self.saved_cursor_alt.map(|c| c.row).unwrap_or(0)
        };
        let main_removed = top_removed_for(main_reference_row);
        let alt_removed = top_removed_for(alt_reference_row);

        // OSC 133(タスク#13): `resize_grid`はmain側の押し出し行を`pending_scrollback`へ
        // 直接pushする(内部で何行押し出すか決まる)ため、呼び出し前後の長さ差分で
        // `total_scrolled_lines`を増やす([scroll_up_region]と違い、ここでは押し出す
        // 行数を呼び出し元が事前に知らないため差分方式にする)。
        let scrollback_before = self.pending_scrollback.len();
        self.main_cells = Self::resize_grid(
            &self.main_cells, old_cols, old_rows, new_cols, new_rows, main_removed, &blank,
            Some(&mut self.pending_scrollback),
        );
        self.alt_cells = Self::resize_grid(
            &self.alt_cells, old_cols, old_rows, new_cols, new_rows, alt_removed, &blank,
            None,
        );
        self.total_scrolled_lines += (self.pending_scrollback.len() - scrollback_before) as u64;

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

        if let Some(saved) = self.saved_cursor_main.take() {
            self.saved_cursor_main = Some(SavedCursor {
                row: shift_row(saved.row, main_removed),
                col: clamp_col(saved.col),
                ..saved
            });
        }
        if let Some(saved) = self.saved_cursor_alt.take() {
            self.saved_cursor_alt = Some(SavedCursor {
                row: shift_row(saved.row, alt_removed),
                col: clamp_col(saved.col),
                ..saved
            });
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

        // タブストップ(タスク#61)は列方向の状態なので行のリサイズとは独立に扱う:
        // 既存列(0..min(old_cols, new_cols))のHTS/TBCによる変更はそのまま保持し、
        // 新しく増えた列(new_cols > old_colsの場合)だけ既定パターンで埋める
        // (alacrittyの`TabStops::resize`と同じ方針)。縮む場合は単に切り詰める——
        // 失われた列のタブストップ状態を覚えておく意味は無い。
        self.tab_stops.resize(new_cols, false);
        for c in old_cols..new_cols {
            self.tab_stops[c] = c % 8 == 0;
        }

        self.cols = new_cols;
        self.rows = new_rows;
        // Sixel(タスク#42): 実際にサイズが変わった場合のみ到達する(直前の早期returnで
        // 同一サイズは弾かれている)ため、行の入れ替え・切り詰めで既存の画像配置の
        // row/colが指す内容が変わり得る——`clear_images`docコメントの理由で単純に消す。
        self.clear_images();
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
            if new_cols < old_cols {
                // 列数を縮小した場合のみ、単純な`Vec::resize`による末尾切り捨てが
                // 全角文字ペアの右半分(placeholder)だけを切り落とし得る。孤立した
                // プレースホルダ無し幅2本体セルを残さないよう、他の経路と同じ
                // ロジックで後始末する(タスク#85)。
                //
                // 列数が変わらない(行数のみ変更)・列数を拡大する場合は無条件に
                // 呼んではいけない——`cols == 1`端末が意図的に保持しているplaceholder
                // 無しの幅2本体セル(タスク#56の既存挙動)まで、列を全く縮めていない
                // resizeのついでに空白化してしまう回帰をCodexレビューで指摘された。
                sanitize_wide_cells(&mut row);
            }
            new_cells.extend(row);
        }
        new_cells
    }

    fn switch_to_alt(&mut self, save_cursor: bool) {
        // alt画面への切替は画面内容を丸ごと差し替える——全画面dirty(#93)。
        self.full_damage_pending = true;
        if save_cursor {
            self.saved_cursor_main = Some(SavedCursor {
                row: self.cursor_row,
                col: self.cursor_col,
                attrs: self.cur_attrs,
                g0: self.g0_charset,
                g1: self.g1_charset,
                gl_is_g1: self.gl_is_g1,
            });
        }
        let theme = self.theme;
        self.main_cells = self.cells().clone();
        let blank = blank_cell_for_theme(&theme);
        self.alt_cells = vec![blank; self.cols * self.rows];
        self.alt_active = true;
        if save_cursor {
            self.cursor_row = 0;
            self.cursor_col = 0;
            self.cur_attrs = TermAttrs::default_for(&theme);
            // カーソル位置・SGR属性と同様、alt画面への切替は文字セット状態も
            // フレッシュな既定(G0=ASCII、GL=G0)に戻す(タスク#41)。main画面復帰時
            // には下の`switch_to_main`が`saved_cursor_main`からこの状態を復元する。
            self.g0_charset = Charset::Ascii;
            self.g1_charset = Charset::Ascii;
            self.gl_is_g1 = false;
        }
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        // OSC 8(タスク#40): アクティブなハイパーリンクはalt画面の裏表切り替えを
        // またいで持ち越さない(`SavedCursor`の保存対象にも含めない——xterm等の
        // 実装がリンクの下線状態を画面切替で引き継がないのに倣う)。
        self.active_link_id = None;
        // Sixel(タスク#42): 画像はmain/alt画面ごとに別々に管理しない(スコープ外、
        // [Terminal::clear_images]docコメント参照)——切替のたびに単純に消去する。
        self.clear_images();
    }

    fn switch_to_main(&mut self, restore_cursor: bool) {
        if !self.alt_active { return; }
        // main画面への復帰も画面内容を丸ごと差し替える——全画面dirty(#93)。
        self.full_damage_pending = true;
        self.alt_active = false;
        if restore_cursor {
            if let Some(saved) = self.saved_cursor_main.take() {
                self.cursor_row = saved.row;
                self.cursor_col = saved.col;
                self.cur_attrs = saved.attrs;
                self.g0_charset = saved.g0;
                self.g1_charset = saved.g1;
                self.gl_is_g1 = saved.gl_is_g1;
            }
        }
        self.active_link_id = None;
        self.clear_images();
    }

    /// DECSC(`ESC 7`)およびCSI `s`(ANSI.SYS方言、DECLRMM未実装のためintermediate無しの
    /// `CSI s`は常にこちらとして扱ってよい——`fn csi_dispatch`呼び出し元のコメント参照)。
    /// 現在アクティブな画面(`alt_active`)に応じて`saved_cursor_main`/`saved_cursor_alt`の
    /// どちらか一方だけを更新する(タスク#57)。`switch_to_alt`(`?1047`/`?1049`)と同じ
    /// スロットを共有する設計の理由は[Terminal]の`saved_cursor_main`フィールド
    /// docコメント参照。
    fn save_cursor_decsc(&mut self) {
        let saved = Some(SavedCursor {
            row: self.cursor_row,
            col: self.cursor_col,
            attrs: self.cur_attrs,
            g0: self.g0_charset,
            g1: self.g1_charset,
            gl_is_g1: self.gl_is_g1,
        });
        if self.alt_active {
            self.saved_cursor_alt = saved;
        } else {
            self.saved_cursor_main = saved;
        }
    }

    /// DECRC(`ESC 8`)およびCSI `u`。対応する`save_cursor_decsc`が一度も呼ばれていない
    /// (スロットが`None`)場合、VT100系の挙動が実装依存で割れる仕様のため、この実装では
    /// 安全側に倒して何もしない(カーソルを勝手に原点等へ移動させない)。
    fn restore_cursor_decrc(&mut self) {
        let saved = if self.alt_active { self.saved_cursor_alt } else { self.saved_cursor_main };
        if let Some(saved) = saved {
            // 保存後にresizeで画面が縮んでいる可能性を考慮し、現在のcols/rowsへ
            // クランプする。`resize_preserving_state`は`saved_cursor_*`自体を
            // resize時に追従更新するが(念のため多重に安全側でもクランプする)、
            // colはprintの遅延折り返し状態(`== cols`)を許容する必要があるため
            // `cols`ちょうどまでは許容し、それを超える場合のみ切り詰める。
            self.cursor_row = saved.row.min(self.rows.saturating_sub(1));
            self.cursor_col = saved.col.min(self.cols);
            self.cur_attrs = saved.attrs;
            self.g0_charset = saved.g0;
            self.g1_charset = saved.g1;
            self.gl_is_g1 = saved.gl_is_g1;
        }
    }

    /// SU(`CSI Ps S`)。scroll region内を`n`行分上方向へシフトし、下端を空行で
    /// 埋める。[scroll_down_region](SD)と同じ理由で、シフト対象が0行になる
    /// (`n == region_size`)場合はシフトループ自体をスキップする — `bot - n`を
    /// `top == 0`の状態で直接計算すると`usize`アンダーフローでpanicするため
    /// (タスク#69: `Ps`がフルスクロール領域幅以上のときに実際に踏んでいた既存
    /// バグの修正。空行埋めの範囲も`bot - n + 1`ではなく`bot + 1 - n`の順序で
    /// 計算し同じ理由のアンダーフローを避ける)。
    fn scroll_up_region(&mut self, n: usize) {
        // スクロールは行座標をずらすので行差分では追随できない——全画面dirty(#93)。
        self.full_damage_pending = true;
        // Sixel(タスク#42): スクロールは画像が乗っていたグリッド位置の内容を
        // 別の内容へ置き換えるため、既存の画像配置は無条件に消す(`clear_images`
        // docコメント参照)。
        self.clear_images();
        let top = self.scroll_top;
        let bot = self.scroll_bottom;
        let region_size = bot - top + 1;
        let n = n.min(region_size);
        let cols = self.cols;

        if top == 0 && !self.alt_active {
            for i in 0..n {
                let row_start = i * cols;
                let row = self.main_cells[row_start..row_start + cols].to_vec();
                self.pending_scrollback.push(row);
            }
            // OSC 133(タスク#13): 実際にscrollbackへ押し出された行数だけ
            // `total_scrolled_lines`を進める(`Terminal::total_scrolled_lines`
            // フィールドdocコメント参照)。
            self.total_scrolled_lines += n as u64;
        }

        if n < region_size {
            for row in top..=(bot - n) {
                for col in 0..cols {
                    let src = self.cells_mut()[(row + n) * cols + col].clone();
                    self.cells_mut()[row * cols + col] = src;
                }
            }
        }
        let blank = self.blank();
        for row in (bot + 1 - n)..=bot {
            for col in 0..cols {
                self.cells_mut()[row * cols + col] = blank.clone();
            }
        }
    }

    /// SD(`CSI Ps T`)。scroll region([scroll_top, scroll_bottom])の内容を`n`行分
    /// 下方向へシフトし、上端(`scroll_top`側)を空行で埋める。[scroll_up_region]
    /// (SU、`CSI Ps S`)の対になる操作 — 構造は上下反転しただけで同じ:
    /// カーソル位置は変更せず、region外の行には触れない。SUと異なり、下端から
    /// 押し出された行はどこにも保存せず単に破棄する(scrollbackは「上に消えた行の
    /// 履歴」であり、下方向スクロールで失われる行はその対象ではない — xtermも
    /// SDでscrollbackを変更しない)。
    ///
    /// [insert_lines]/[delete_lines]と同じ理由で、シフト対象が0行になる
    /// (`n == region_size`)場合はシフトループ自体をスキップする — `bot - n`を
    /// `top == 0`の状態で直接計算すると`usize`アンダーフローでpanicするため。
    fn scroll_down_region(&mut self, n: usize) {
        self.full_damage_pending = true; // #93: 行座標がずれるので全画面dirty。
        self.clear_images(); // Sixel(タスク#42): scroll_up_regionと同じ理由。
        let top = self.scroll_top;
        let bot = self.scroll_bottom;
        let region_size = bot - top + 1;
        let n = n.min(region_size);
        let cols = self.cols;

        if n < region_size {
            for row in (top..=(bot - n)).rev() {
                for col in 0..cols {
                    let src = self.cells_mut()[row * cols + col].clone();
                    self.cells_mut()[(row + n) * cols + col] = src;
                }
            }
        }
        let blank = self.blank();
        for row in top..(top + n) {
            for col in 0..cols {
                self.cells_mut()[row * cols + col] = blank.clone();
            }
        }
    }

    /// IL(`CSI Ps L`)。カーソル行に`n`個の空行を挿入し、カーソル行〜scroll_bottomの
    /// 内容を下方向へ押し出す(scroll_bottomを超えて溢れた行は破棄)。
    ///
    /// - カーソルが現在のscroll region([scroll_top, scroll_bottom])の外にある場合は
    ///   no-op(xterm/VT102 同様、IL/DLはscroll region内でのみ効く)。
    /// - `n`をregionサイズ(`scroll_bottom - cursor_row + 1`)にクランプすることで、
    ///   [scroll_up_region]に存在する「n == region幅」時の`usize`アンダーフローを
    ///   同じ形では踏まない(縮めた領域を経由せず、シフト対象が0行の時はシフト
    ///   ループ自体をスキップする)。
    /// - カーソル位置(行・列とも)は変更しない。挿入で押し出された行は
    ///   [pending_scrollback] に一切積まない(Fableレビュー: `scroll_up_region`は
    ///   `top==0 && !alt`の場合のみscrollbackへpushするため、IL/DLを安直に
    ///   `scroll_up_region`経由で実装するとカーソルが0行目にある時の押し出し行が
    ///   誤って履歴に混入するバグを生む——このメソッドは常にscrollbackへ触れない)。
    fn insert_lines(&mut self, n: usize) {
        let top = self.cursor_row;
        let bot = self.scroll_bottom;
        if top < self.scroll_top || top > bot { return; }
        self.full_damage_pending = true; // #93: 行座標がずれるので全画面dirty。
        self.clear_images(); // Sixel(タスク#42): scroll_up_regionと同じ理由。
        let region_size = bot - top + 1;
        let n = n.min(region_size);
        let cols = self.cols;
        if n < region_size {
            // 下から上へ(行番号の大きい方から)コピーすることで、書き込み先
            // (row + n)がまだ読んでいない元データを上書きしないようにする。
            for row in (top..=(bot - n)).rev() {
                for col in 0..cols {
                    let src = self.cells_mut()[row * cols + col].clone();
                    self.cells_mut()[(row + n) * cols + col] = src;
                }
            }
        }
        let blank = self.blank();
        for row in top..(top + n) {
            for col in 0..cols {
                self.cells_mut()[row * cols + col] = blank.clone();
            }
        }
    }

    /// DL(`CSI Ps M`)。カーソル行から`n`行を削除し、それより下(〜scroll_bottom)の
    /// 内容を上方向へ詰める。押し出された分(下端)は空行で埋める。
    ///
    /// [insert_lines] と対になる実装 — 制約・不変条件([pending_scrollback]に
    /// 一切触れない、カーソル位置不変、scroll region外ではno-op)は同じ。
    /// アンダーフロー回避のため、空行で埋める開始行を`bot - n + 1`ではなく
    /// `top + (region_size - n)`として計算する(`n == region_size`の時
    /// `bot - n + 1`は`usize`の直接減算だと桁あふれし得るが、こちらは
    /// `region_size - n >= 0`が`n`のクランプにより保証されているため安全)。
    fn delete_lines(&mut self, n: usize) {
        let top = self.cursor_row;
        let bot = self.scroll_bottom;
        if top < self.scroll_top || top > bot { return; }
        self.full_damage_pending = true; // #93: 行座標がずれるので全画面dirty。
        self.clear_images(); // Sixel(タスク#42): scroll_up_regionと同じ理由。
        let region_size = bot - top + 1;
        let n = n.min(region_size);
        let cols = self.cols;
        if n < region_size {
            for row in top..=(bot - n) {
                for col in 0..cols {
                    let src = self.cells_mut()[(row + n) * cols + col].clone();
                    self.cells_mut()[row * cols + col] = src;
                }
            }
        }
        let blank = self.blank();
        let blank_start = top + (region_size - n);
        for row in blank_start..=bot {
            for col in 0..cols {
                self.cells_mut()[row * cols + col] = blank.clone();
            }
        }
    }

    /// [insert_chars]/[delete_chars] が行内で全角(wide)文字の片割れを分断してしまった
    /// 場合の後始末(タスク#47)。「本体セル(ch の表示幅==2)の右隣が
    /// `is_wide_placeholder` である」「プレースホルダセルの左隣が本体セルである」の
    /// 対応関係が崩れた片割れを、それぞれ通常の空白セルへ変換する——挿入/削除で
    /// 本体とプレースホルダの間に別セルが割り込んだ、または片方だけが行の反対側へ
    /// シフトされて対応が消えた状態を放置すると、以後の描画で幅の合わない孤立した
    /// 全角文字グリフや、本体を持たない浮いたプレースホルダが残る。
    ///
    /// 実体は[sanitize_wide_cells]に委譲する——リサイズによる列数縮小時の右端クリップ
    /// (`resize_grid`)・DECAWM off時の右端書き込み(`print_mapped`)も同じ不変量崩れを
    /// 起こし得るため、それらとロジックを共有する(タスク#85)。
    fn sanitize_wide_row(&mut self, row: usize) {
        let cols = self.cols;
        let row_base = row * cols;
        sanitize_wide_cells(&mut self.cells_mut()[row_base..row_base + cols]);
    }

    /// ICH(`CSI Ps @`)。カーソル位置に`n`個の空白セルを挿入し、カーソル位置〜行末の
    /// 内容を右へ押し出す(行末を超えて溢れたセルは破棄)。操作は現在行に閉じており、
    /// scroll region や他の行には一切影響しない。カーソル位置は変更しない
    /// (xterm/VT102 仕様)。
    ///
    /// `cursor_col`が折り返し待ち(`== cols`)の場合は[erase_cells]等の他のCSIハンドラ
    /// と同様、見えている最終列(`cols - 1`)にクランプしてから計算する。
    fn insert_chars(&mut self, n: usize) {
        if self.cursor_row >= self.rows { return; }
        let row = self.cursor_row;
        let cols = self.cols;
        let col = self.cursor_col.min(cols.saturating_sub(1));
        let region_size = cols - col;
        let n = n.min(region_size);
        if n == 0 { return; }
        let row_base = row * cols;
        if n < region_size {
            // insert_lines と同じ理由(書き込み先がまだ読んでいない元データを上書き
            // しないようにするため)で、右から左(列番号の大きい方から)コピーする。
            for c in (col..=(cols - 1 - n)).rev() {
                let src = self.cells_mut()[row_base + c].clone();
                self.cells_mut()[row_base + c + n] = src;
            }
        }
        let blank = self.blank();
        for c in col..(col + n) {
            self.cells_mut()[row_base + c] = blank.clone();
        }
        self.sanitize_wide_row(row);
    }

    /// DCH(`CSI Ps P`)。カーソル位置から`n`個のセルを削除し、それより右の内容を
    /// 左へ詰める。押し出された分(行末)は現在のSGR属性の空白で埋める。
    ///
    /// [insert_chars] と対になる実装 — 制約([sanitize_wide_row]による片割れ処理、
    /// 現在行に閉じる、カーソル位置不変)は同じ。アンダーフロー回避のため、空白で
    /// 埋める開始列を`delete_lines`と同様`col + (region_size - n)`として計算する。
    fn delete_chars(&mut self, n: usize) {
        if self.cursor_row >= self.rows { return; }
        let row = self.cursor_row;
        let cols = self.cols;
        let col = self.cursor_col.min(cols.saturating_sub(1));
        let region_size = cols - col;
        let n = n.min(region_size);
        if n == 0 { return; }
        let row_base = row * cols;
        if n < region_size {
            for c in col..=(cols - 1 - n) {
                let src = self.cells_mut()[row_base + c + n].clone();
                self.cells_mut()[row_base + c] = src;
            }
        }
        let blank = self.blank();
        let blank_start = col + (region_size - n);
        for c in blank_start..cols {
            self.cells_mut()[row_base + c] = blank.clone();
        }
        self.sanitize_wide_row(row);
    }

    /// ECH(`CSI Ps X`)。カーソル位置から`n`個のセルを、シフトを伴わずその場で
    /// 現在のSGR属性の空白に置き換える(ICH/DCHと異なり右側の内容は動かない)。
    fn erase_chars(&mut self, n: usize) {
        if self.cursor_row >= self.rows { return; }
        let row = self.cursor_row;
        let cols = self.cols;
        let col = self.cursor_col.min(cols.saturating_sub(1));
        let n = n.min(cols - col);
        if n == 0 { return; }
        let row_base = row * cols;
        let blank = self.blank();
        for c in col..(col + n) {
            self.cells_mut()[row_base + c] = blank.clone();
        }
        self.sanitize_wide_row(row);
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
            // `ESC[m`(パラメータ無し)は`ESC[0m`と同義 — 色だけでなく
            // bold/dim/italic/underline/blink/reverse/invisible/strikethrough も
            // 全てリセットする(SGR 0 と同じ扱いにしないと空psを送るリモートだけ
            // 装飾が残留するバグになる)。
            self.cur_attrs = TermAttrs::default_for(&theme);
            return;
        }
        let mut i = 0;
        while i < ps.len() {
            match ps[i] {
                0  => { self.cur_attrs = TermAttrs::default_for(&theme); }
                1  => { self.cur_attrs.bold = true; }
                2  => { self.cur_attrs.dim = true; }
                3  => { self.cur_attrs.italic = true; }
                4  => { self.cur_attrs.underline = true; }
                5  => { self.cur_attrs.blink = true; }
                7  => { self.cur_attrs.reverse = true; }
                8  => { self.cur_attrs.invisible = true; }
                9  => { self.cur_attrs.strikethrough = true; }
                // 22 は bold(1) と dim(2) の両方を同時にリセットする(SGR仕様通り;
                // 個別に取り消すコードは存在しない)。
                22 => { self.cur_attrs.bold = false; self.cur_attrs.dim = false; }
                23 => { self.cur_attrs.italic = false; }
                24 => { self.cur_attrs.underline = false; }
                25 => { self.cur_attrs.blink = false; }
                27 => { self.cur_attrs.reverse = false; }
                28 => { self.cur_attrs.invisible = false; }
                29 => { self.cur_attrs.strikethrough = false; }
                30..=37 => { self.cur_attrs.fg = theme.ansi16[(ps[i] - 30) as usize]; }
                38 => {
                    if let Some((color, advance)) = parse_extended_color(&theme, ps, i) {
                        self.cur_attrs.fg = color;
                        i += advance;
                    }
                }
                39 => { self.cur_attrs.fg = theme.default_fg; }
                40..=47 => { self.cur_attrs.bg = theme.ansi16[(ps[i] - 40) as usize]; }
                48 => {
                    if let Some((color, advance)) = parse_extended_color(&theme, ps, i) {
                        self.cur_attrs.bg = color;
                        i += advance;
                    }
                }
                49  => { self.cur_attrs.bg = theme.default_bg; }
                90..=97  => { self.cur_attrs.fg = theme.ansi16[8 + (ps[i] - 90) as usize]; }
                100..=107 => { self.cur_attrs.bg = theme.ansi16[8 + (ps[i] - 100) as usize]; }
                _ => {}
            }
            i += 1;
        }
    }

    /// DECSCUSR(`CSI Ps SP q`)のパラメータ表: 0/1=blinking block(既定)、2=steady
    /// block、3=blinking underline、4=steady underline、5=blinking bar、
    /// 6=steady bar。未知のパラメータ(xterm仕様上は無いが、将来拡張分)は無視する。
    fn set_cursor_shape_from_decscusr(&mut self, ps: u16) {
        let (shape, blink) = match ps {
            0 | 1 => (CursorShape::Block, true),
            2 => (CursorShape::Block, false),
            3 => (CursorShape::Underline, true),
            4 => (CursorShape::Underline, false),
            5 => (CursorShape::Bar, true),
            6 => (CursorShape::Bar, false),
            _ => return,
        };
        self.cursor_shape = shape;
        self.cursor_blink = blink;
    }

    /// 座標付きの生ポインタイベント([PointerEvent])を、現在の
    /// [MouseReportingMode]/SGRモード(`?1006`)に従ってターミナルへ送るべき
    /// バイト列にエンコードする(タスク#36)。実体は[encode_pointer_event_bytes]
    /// (この`Terminal`インスタンスの状態を持たない自由関数)へ委譲する——
    /// 同じエンコードロジックを、UI層(#50/#51)がRust側セッション状態を経由せず
    /// 直接呼べる`#[uniffi::export]`関数(`lib.rs::terminal_pointer_event_bytes`)
    /// からも再利用するため(タスク#51)。
    pub(crate) fn encode_pointer_event(&self, event: PointerEvent) -> Option<Vec<u8>> {
        encode_pointer_event_bytes(event, self.cols, self.rows, self.mouse_reporting_mode, self.sgr_mouse_mode, self.urxvt_mouse_mode)
    }

    /// フォーカスレポーティング(`?1004`、タスク#60)が有効な場合のみ、OS由来のフォーカス
    /// 変化イベント(`focused`)をxterm互換のシーケンスへエンコードする。無効時は`None`を
    /// 返す——呼び出し元([crate::session_state::SessionState::notify_focus_change])は
    /// これを「何も送らない」の合図として扱う。vim/tmuxの`FocusGained`/`FocusLost`
    /// autocmdはこのシーケンスの有無だけを見て座標や修飾キーを持たないため、
    /// [encode_pointer_event]と違いエンコードに分岐は無い。
    pub(crate) fn encode_focus_event(&self, focused: bool) -> Option<Vec<u8>> {
        if !self.focus_reporting_mode {
            return None;
        }
        Some(if focused { b"\x1b[I".to_vec() } else { b"\x1b[O".to_vec() })
    }

    /// OSC 10(`is_fg == true`)/OSC 11(`is_fg == false`)の set/query 共通処理(タスク#58)。
    /// `spec == "?"`はquery——現在のtheme既定色を`rgb:RRRR/GGGG/BBBB`形式で
    /// `pending_terminal_responses`に積む(応答経路はDA/DSRと同じ、タスク#38)。
    /// それ以外はsetとして解釈を試み、パースできた場合のみ`self.theme`を更新する
    /// (`set_theme()`のdoc commentの通り、既に解決済みのセルは遡って再着色されない
    /// ——このOSCによるsetも同じ制約を継承する)。パースできない`spec`は無視する
    /// (実端末も未知のcolor specは無視して応答もしない)。
    fn handle_osc_default_color(&mut self, is_fg: bool, spec: &[u8], bell_terminated: bool) {
        if spec == b"?" {
            let color = if is_fg { self.theme.default_fg } else { self.theme.default_bg };
            let r = ((color >> 16) & 0xFF) as u8;
            let g = ((color >> 8) & 0xFF) as u8;
            let b = (color & 0xFF) as u8;
            let terminator: &[u8] = if bell_terminated { b"\x07" } else { b"\x1b\\" };
            let osc_num: &[u8] = if is_fg { b"10" } else { b"11" };
            let mut resp = Vec::with_capacity(32);
            resp.extend_from_slice(b"\x1b]");
            resp.extend_from_slice(osc_num);
            resp.extend_from_slice(format!(";rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}", r, r, g, g, b, b).as_bytes());
            resp.extend_from_slice(terminator);
            self.pending_terminal_responses.push(resp);
            return;
        }
        if let Some(argb) = parse_osc_color_spec(spec) {
            if is_fg {
                // `cur_attrs.fg`はSGR実行時点で`theme.default_fg`から具体値へ解決済み
                // (`default_for`/SGR `39`)なので、その値が「今の」既定色と一致している
                // 間はまだ明示的な色指定を受けていない=論理的に"default"を指しているとみなし、
                // 新しい既定色へ追従させる(codexレビュー指摘: これをしないと、OSC 10/11
                // set直後SGRリセットを挟まず印字した文字が旧既定色のまま描かれてしまう)。
                // 既に別の色をSGRで明示指定済みのcur_attrsには影響しない。
                if self.cur_attrs.fg == self.theme.default_fg {
                    self.cur_attrs.fg = argb;
                }
                self.theme.default_fg = argb;
            } else {
                if self.cur_attrs.bg == self.theme.default_bg {
                    self.cur_attrs.bg = argb;
                }
                self.theme.default_bg = argb;
            }
        }
    }

    /// OSC 8(`ESC]8;params;URIST`、タスク#40)。vteはOSCペイロードを`;`ごとに
    /// `params`へ分割して渡すため、`params[0]`が`b"8"`、`params[1]`が`id=...`等の
    /// パラメータ列(未使用——複数行にまたがる同一リンクの同定には現状使わない、
    /// スコープ外)、`params[2]`以降がURI本体になる。URI自体が(エスケープされない)
    /// `;`を含む実装は稀にあるため、`params[2]`単独ではなく`params[2..]`を`;`で
    /// 再結合してURIとして扱う(Fableレビュー2次指摘)。
    ///
    /// URIが空(`ESC]8;;ST`、またはURI相当のparamsが無い)ならアクティブリンクを
    /// 閉じる。空でなければ`intern_link`でid化してアクティブリンクにする——以降の
    /// `print()`が書き込む全セルへこのidが付与される。
    ///
    /// vteの既定ビルド(`no_std`フィーチャ、`Cargo.toml`の`vte = "0.13"`はデフォルト
    /// フィーチャ込み)はOSCペイロードのraw byteバッファが`MAX_OSC_RAW`(1024バイト)の
    /// 固定長`ArrayVec`で、バッファが埋まると以降のバイト(`;`区切り自体を含む)は
    /// 静かに捨てられる。1024バイトを超える長いURLは`osc_dispatch`に丸ごとは届かず
    /// 途中で切り詰められたバイト列になる(`test_osc8_extremely_long_url_is_truncated_by_vte_osc_buffer`
    /// で明示)。
    fn handle_osc8_hyperlink(&mut self, params: &[&[u8]]) {
        let uri: Option<String> = if params.len() >= 3 {
            let mut buf = Vec::new();
            for (i, part) in params[2..].iter().enumerate() {
                if i > 0 { buf.push(b';'); }
                buf.extend_from_slice(part);
            }
            String::from_utf8(buf).ok()
        } else {
            None
        };
        match uri {
            Some(s) if !s.is_empty() => {
                // 上限到達後の新規URL(タスク#70)、bidi format controls/制御文字を
                // 含むURL(タスク#77)は`intern_link`が`None`を返す——その場合は
                // リンク無し扱いにフォールバックする。
                self.active_link_id = self.intern_link(s);
            }
            _ => {
                self.active_link_id = None;
            }
        }
    }

    /// URL文字列をintern表へ登録し、そのid(`link_table`のindex)を返す。同じURLは
    /// 既存のidを再利用する(`link_ids`による重複排除)。表が`MAX_LINK_TABLE`件に
    /// 達した後の未登録URLはインターンせず`None`を返す(タスク#70) ——
    /// 呼び出し元はこれをリンク無しへフォールバックさせる。既存id(=既存セルの
    /// `link_id`参照)には影響しない。
    ///
    /// bidi format controls/C0・C1制御文字を含むURLもインターンせず`None`を返す
    /// (タスク#77)。Android/iOSのOSC8確認ダイアログは`link_table`の文字列をそのまま
    /// 表示するため、これらを許すとbidi override等で表示文字列を偽装できてしまう
    /// (`contains_disallowed_hyperlink_chars`参照)。
    fn intern_link(&mut self, uri: String) -> Option<u32> {
        if contains_disallowed_hyperlink_chars(&uri) {
            return None;
        }
        if let Some(&id) = self.link_ids.get(&uri) {
            return Some(id);
        }
        if self.link_table.len() >= MAX_LINK_TABLE {
            return None;
        }
        let id = self.link_table.len() as u32;
        self.link_table.push(uri.clone());
        self.link_ids.insert(uri, id);
        Some(id)
    }

    // ── OSC 133 セマンティックプロンプト(タスク#13) ──────────

    /// OSC 133;A/B/C/Dの1マークを処理する。`sub`はA/B/C/Dのバイト、`extra`は
    /// `D`の場合に付く可能性のあるexit code(`params[2]`、他のフォーマットの
    /// 追加パラメータ——例えば`B;aid=1`の`aid=1`——ならパース失敗として無視する)。
    /// A/B/C以外の未知の`sub`(将来のOSC 133拡張等)は無視する。
    fn handle_osc133_prompt_mark(&mut self, sub: &[u8], extra: Option<&[u8]>) {
        let row = self.total_scrolled_lines + self.cursor_row as u64;
        let kind = match sub {
            b"A" => PromptMarkKind::PromptStart,
            b"B" => PromptMarkKind::CommandStart,
            b"C" => PromptMarkKind::CommandExecuted,
            b"D" => {
                let exit_code = extra
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .and_then(|s| s.trim().parse::<i32>().ok());
                PromptMarkKind::CommandFinished { exit_code }
            }
            _ => return,
        };

        // 「今コマンド入力中か」(タップでのカーソル移動、`cursor_move_bytes_for_click`
        // が参照する)。Bで開始し、それ以外の全マークで終了する——次にAが来て
        // 新しいプロンプトサイクルが始まった場合も含む。
        self.input_line_active = matches!(kind, PromptMarkKind::CommandStart);

        // 「直前コマンドの出力だけをコピー」(タスク#13)向けのキャプチャ制御。
        match kind {
            PromptMarkKind::CommandExecuted => {
                self.capturing_command_output = true;
                self.current_output_line.clear();
                self.current_output_lines.clear();
            }
            PromptMarkKind::CommandFinished { .. } => {
                if self.capturing_command_output {
                    if !self.current_output_line.is_empty() {
                        self.current_output_lines.push(std::mem::take(&mut self.current_output_line));
                    }
                    self.last_command_output = Some(std::mem::take(&mut self.current_output_lines));
                }
                self.capturing_command_output = false;
            }
            _ => {}
        }

        self.prompt_marks.push_back(PromptMark { kind, row });
        if self.prompt_marks.len() > MAX_PROMPT_MARKS {
            self.prompt_marks.pop_front();
        }
    }

    /// [prompt_marks]から「ジャンプ対象」となる行番号の列を、古い→新しい順で返す。
    /// 各プロンプトサイクルにつき1件——`PromptStart`(A)を優先し、そのサイクルで
    /// Aが来ていない場合のみ`CommandStart`(B)をフォールバックとして使う(シェルに
    /// よってはAを送らずBのみのものがある)。`CommandExecuted`/`CommandFinished`が
    /// 来ると次のサイクルとみなし、次のA/Bが改めてアンカーになる。
    fn prompt_anchor_rows(&self) -> Vec<u64> {
        let mut anchors = Vec::with_capacity(self.prompt_marks.len());
        let mut cycle_anchored = false;
        for mark in &self.prompt_marks {
            match mark.kind {
                PromptMarkKind::PromptStart => {
                    anchors.push(mark.row);
                    cycle_anchored = true;
                }
                PromptMarkKind::CommandStart => {
                    if !cycle_anchored {
                        anchors.push(mark.row);
                        cycle_anchored = true;
                    }
                }
                PromptMarkKind::CommandExecuted | PromptMarkKind::CommandFinished { .. } => {
                    cycle_anchored = false;
                }
            }
        }
        anchors
    }

    /// 絶対行番号`abs_row`が、呼び出し時点で実際に表示可能(ライブ画面上、または
    /// まだ`scrollback_len`件のscrollback内に残っている)かどうか。SCROLLBACK_LIMIT
    /// によるトリミングで既に失われた行はfalseを返す——「前/次のプロンプトへ
    /// ジャンプ」がそのような行を飛ばして次の候補を探せるようにするための判定。
    fn is_prompt_row_reachable(&self, abs_row: u64, scrollback_len: u32) -> bool {
        if abs_row >= self.total_scrolled_lines {
            // ライブ画面上。リサイズ直後等で万一画面外を指していたら不可視。
            return abs_row - self.total_scrolled_lines < self.rows as u64;
        }
        let offset = self.total_scrolled_lines - 1 - abs_row;
        offset < scrollback_len as u64
    }

    /// 到達可能であることが確認済みの絶対行番号`abs_row`を、呼び出し元
    /// (`session.rs`/Kotlin側)がそのまま使える[crate::PromptJumpTarget]へ変換する。
    /// `scroll_offset`は既存の検索ジャンプ(タスク#37、[crate::ScrollbackSearchMatch]の
    /// `row`と[SessionCore::scrollback_cells]の`offset`)と同じ規約——scrollback側なら
    /// そのままoffsetとして使える値。
    fn resolve_prompt_row_target(&self, abs_row: u64) -> crate::PromptJumpTarget {
        if abs_row >= self.total_scrolled_lines {
            crate::PromptJumpTarget { scroll_offset: 0, is_live: true }
        } else {
            let offset = (self.total_scrolled_lines - 1 - abs_row).min(u32::MAX as u64) as u32;
            crate::PromptJumpTarget { scroll_offset: offset, is_live: false }
        }
    }

    /// 「前/次のプロンプトへジャンプ」(タスク#13)。`from_scroll_offset`/
    /// `from_showing_scrollback`は呼び出し時点でKotlin側が表示している位置
    /// (既存の検索ジャンプ・タスク#79の`scrollOffset`/`showingScrollback`と同じ規約
    /// ——`.claude/rules/rust-ssot.md`が「UI表示だけに閉じた状態」として許容する
    /// スクロール位置そのものを渡す)。`scrollback_len`は呼び出し時点の
    /// `SessionCore::scrollback_len()`(トリミングで実際に残っている行数)。
    ///
    /// - ライブ画面表示中(`from_showing_scrollback == false`)の基準点は現在の
    ///   カーソル行の絶対行番号(`total_scrolled_lines + cursor_row`)——ライブ画面上に
    ///   複数のプロンプトが同時に見えている場合(小さい端末・短いコマンド列)でも、
    ///   現在位置より上にある直近のプロンプトへ正しくジャンプできる。
    /// - `want_previous`が偽(「次」)で、scrollback内に候補が無い場合は、ライブ画面
    ///   ([PromptJumpTarget::is_live] = true)へ「戻る」ジャンプ先を返す(ライブ表示中に
    ///   「次」を呼んだ場合は既に最新なので`None`)。
    pub(crate) fn prompt_jump_target(
        &self,
        want_previous: bool,
        from_scroll_offset: u32,
        from_showing_scrollback: bool,
        scrollback_len: u32,
    ) -> Option<crate::PromptJumpTarget> {
        let reference = if from_showing_scrollback {
            self.total_scrolled_lines.saturating_sub(1).saturating_sub(from_scroll_offset as u64)
        } else {
            self.total_scrolled_lines + self.cursor_row as u64
        };
        let anchors = self.prompt_anchor_rows();
        if want_previous {
            anchors.into_iter()
                .filter(|&r| r < reference && self.is_prompt_row_reachable(r, scrollback_len))
                .max()
                .map(|r| self.resolve_prompt_row_target(r))
        } else {
            let next = anchors.into_iter()
                .filter(|&r| r > reference && self.is_prompt_row_reachable(r, scrollback_len))
                .min();
            match next {
                Some(r) => Some(self.resolve_prompt_row_target(r)),
                None if from_showing_scrollback => {
                    Some(crate::PromptJumpTarget { scroll_offset: 0, is_live: true })
                }
                None => None,
            }
        }
    }

    /// タップされたセル(`row`/`col`、0-indexed画面座標)が現在アクティブな入力行
    /// (OSC 133;Bは来たが;Cがまだの区間、かつタップ行が現在のカーソル行と一致する
    /// ——`input_line_active`フィールドdocコメント参照)上であれば、そこへカーソルを
    /// 移動する矢印キー相当のバイト列を返す(Ghostty`cl=line`相当、タスク#13)。
    /// 対象外(未アクティブ・別の行・移動量ゼロ)なら`None`——呼び出し元は
    /// 何も送らない。
    ///
    /// 実際にリモートのシェル(readline等)のカーソル位置を動かすには、ターミナル側の
    /// 表示カーソルを直接動かすのではなく、物理キーボードで矢印キーを`delta`回
    /// 押したのと同じバイト列を送る必要がある——readlineはパラメータ付きCSI
    /// (`ESC[5C`等)を「5回分の移動」として解釈しないため([crate::terminal_arrow_bytes]
    /// と同じエンコード関数を使い、修飾キー無しの単純な矢印キー入力を`delta`回分
    /// 生成する)。
    pub(crate) fn cursor_move_bytes_for_click(&self, row: u32, col: u32) -> Option<Vec<u8>> {
        if !self.input_line_active {
            return None;
        }
        if row as usize != self.cursor_row {
            return None;
        }
        let target_col = (col as usize).min(self.cols.saturating_sub(1));
        let delta = target_col as i64 - self.cursor_col as i64;
        if delta == 0 {
            return None;
        }
        let letter = if delta > 0 { b'C' } else { b'D' };
        let modifiers = crate::TerminalKeyModifiers::default();
        let mut bytes = Vec::new();
        for _ in 0..delta.unsigned_abs() {
            bytes.extend_from_slice(&crate::terminal_arrow_bytes(letter, self.application_cursor_mode, modifiers));
        }
        Some(bytes)
    }

    /// 直前に完了したコマンド(OSC 133;C〜Dの区間)の出力をテキストとして返す
    /// (タスク#13「直前コマンドの出力だけをコピー」)。まだ完了したコマンドが
    /// 無ければ`None`。複数行は`\n`で結合する。
    pub(crate) fn last_command_output_text(&self) -> Option<String> {
        self.last_command_output.as_ref().map(|lines| lines.join("\n"))
    }
}

/// OSC8のURLとして許さない文字が含まれているかを判定する(タスク#77)。
///
/// Android/iOSのOSC8確認ダイアログ(`isOpenableHyperlinkScheme`によるスキーム
/// 制限とは別の層)は`link_table`の文字列をそのままユーザーに提示する。この
/// 文字列にUnicode双方向書式制御文字(bidi format controls)が混じっていると、
/// 実際のリンク先とは異なる文字列に見えるよう表示順序を偽装できてしまう
/// (例: `\u{202E}`で末尾の拡張子を先頭にひっくり返して見せる、いわゆる
/// "RTLO spoofing")。
///
/// 「Unicode Cf/Ccカテゴリを丸ごと拒否」は正当なIRI(パーセントエンコード
/// されていない非ASCII文字を含むURL等)まで広く巻き込みすぎるため
/// (codexレビュー済み)、対象は次の2種類のみに絞る:
/// - bidi format controls: `U+202A..=U+202E`(LRE/RLE/PDF/LRO/RLO)、
///   `U+2066..=U+2069`(LRI/RLI/FSI/PDI)
/// - C0/C1制御文字: `U+0000..=U+001F`、`U+007F`(DEL)、`U+0080..=U+009F`
///   (vteのOSC文字列パーサはC0の大半を`osc_dispatch`に渡す前に静かに捨てる
///   ため実際には主にC1側・DEL側が到達経路になるが、将来の実装変更に
///   備えて両方を対象にする)
fn contains_disallowed_hyperlink_chars(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c,
            '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{0000}'..='\u{001F}'
                | '\u{007F}'
                | '\u{0080}'..='\u{009F}'
        )
    })
}

/// OSC 10/11などが使う`Pt`のcolor spec(`rgb:R.../G.../B...`または`#RRGGBB`系)を
/// ARGB(`0xFFRRGGBB`)へパースする(タスク#58)。xtermの実装同様、各成分は1〜4桁の
/// 16進数(`rgb:`形式)または、3つの等長16進成分(`#`形式、3/6/9/12桁)を許す。
/// 桁数がその成分の表現できる最大値未満のスケールで表現されている場合は
/// 8bitへ丸めてスケールする(例: `rgb:f/0/0`は`0xFFFF0000`相当の赤)。
fn parse_osc_color_spec(spec: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(spec).ok()?;
    let scale = |hex: &str| -> Option<u8> {
        if hex.is_empty() || hex.len() > 4 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(hex, 16).ok()?;
        let max = (1u32 << (hex.len() as u32 * 4)) - 1;
        Some(((v * 255 + max / 2) / max) as u8)
    };
    if let Some(rest) = s.strip_prefix("rgb:") {
        let mut parts = rest.split('/');
        let (r, g, b) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        let (r, g, b) = (scale(r)?, scale(g)?, scale(b)?);
        return Some(0xFF000000 | (r as u32) << 16 | (g as u32) << 8 | b as u32);
    }
    if let Some(rest) = s.strip_prefix('#') {
        let n = rest.len();
        if n == 0 || n % 3 != 0 || n > 12 || !rest.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let per = n / 3;
        let comp = |idx: usize| -> Option<u8> { scale(&rest[idx * per..(idx + 1) * per]) };
        let (r, g, b) = (comp(0)?, comp(1)?, comp(2)?);
        return Some(0xFF000000 | (r as u32) << 16 | (g as u32) << 8 | b as u32);
    }
    None
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

impl Terminal {
    /// 文字集合(charset)写像を適用**した後**の文字をそのままセルへ書き込む。
    /// `print()`本体はここへ委譲する。REP(`CSI Ps b`、タスク#48)は
    /// `last_graphic_cell`に写像後の文字を保存しているため、こちらを直接
    /// 呼ぶことで「保存時と異なる文字集合がその後選択されていた場合に
    /// 再写像されて別の文字が繰り返されてしまう」バグ(タスク#84)を避ける。
    fn print_mapped(&mut self, c: char) {
        use unicode_width::UnicodeWidthChar;
        let width = c.width().unwrap_or(1);

        if width == 0 {
            // 幅0の結合文字(combining character、例: U+0301 COMBINING ACUTE ACCENT)。
            // 独立したセルとして書き込んでカーソルを進めてしまうと、以後の文字が
            // 全て1桁ずつ右へずれてしまう。直前に印字した文字のセルへグラフェムとして
            // 追加し、カーソルは進めない(Fableレビュー: タスク#39)。
            //
            // `cursor_col` の有効範囲は 0..=cols(== cols は「次の print() で折り返す」
            // wrap-pending状態、388行目付近のコメント参照)。wrap-pending中に結合文字が
            // 来た場合も改行させず、`cursor_col - 1 == cols - 1`(現在行の最終セル)へ
            // 付加するのが正しい — 折り返しを先に実行すると結合文字が次行の先頭に
            // 単独で置かれてしまう(Fableレビュー: タスク#39)。
            if self.cursor_col == 0 {
                // 行頭で、結合させる直前の文字が無い(RIS直後・行クリア直後等)。
                // グラフェムクラスタリング(ZWJ絵文字等)は対象外(Fableレビューでスコープ外
                // と明記済み) — 単純に無視する。
                return;
            }
            if self.cursor_row >= self.rows {
                return;
            }
            let attach_row = self.cursor_row;
            let mut attach_col = self.cursor_col - 1;
            // 全角(wide)文字のプレースホルダセル(2セル目)に結合文字が来た場合は、
            // プレースホルダ自体ではなくその前の全角文字本体セルへ付加する。
            if attach_col > 0 && self.cell_mut(attach_row, attach_col).is_wide_placeholder {
                attach_col -= 1;
            }
            let cell = self.cell_mut(attach_row, attach_col);
            if cell.ch.len() + c.len_utf8() > MAX_COMBINING_BYTES_PER_CELL {
                // 上限超過分は黙って捨てる(タスク#78) — 大量の結合文字を送りつけて
                // 1セルの文字列を際限なく成長させるリモートに対するメモリガード。
                return;
            }
            let mut combined = String::with_capacity(cell.ch.len() + c.len_utf8());
            combined.push_str(&cell.ch);
            combined.push(c);
            cell.ch = smol_str::SmolStr::new(combined);
            return;
        }

        // 折り返しが必要かどうかを「書く前」に判定する。通常の折り返し待ち
        // (`cursor_col >= cols`)に加え、全角(width==2)文字が現在行に1列しか
        // 残っていない場合(`cursor_col == cols - 1`)も対象に含める——xtermは
        // 全角文字を半分だけ現在行に置いたりしない。丸ごと次行へ送る
        // (Fableレビュー: タスク#56、以前は書いた後に判定していたため
        // placeholder側だけ欠落し文字が半分に切れていた)。
        // `self.cols > 1` を追加で要求する: `cols == 1` の端末では全角文字は
        // 折り返した先でも絶対に収まらないため、この条件が無いと行頭
        // (`cursor_col == 0`)であっても毎回強制的に改行してしまい、書かれる
        // はずだった行を1行無駄にしてしまう(Codexレビュー指摘)。
        let needs_wrap = self.cursor_col >= self.cols
            || (width == 2 && self.cols > 1 && self.cursor_col + 1 >= self.cols);

        if needs_wrap {
            if self.autowrap_mode {
                self.cursor_col = 0;
                self.newline();
            } else {
                // DECAWM off(`CSI ?7l`): 次行へ折り返さず、右端の最終列
                // (`cols - 1`)を上書きし続ける(xterm仕様、タスク#56)。
                self.cursor_col = self.cols.saturating_sub(1);
            }
        }

        if self.cursor_row < self.rows {
            let attrs = self.cur_attrs;
            // OSC 8(タスク#40): ハイパーリンク状態はSGR属性(`cur_attrs`)とは別に
            // `active_link_id`として持っており、実際に書くセルへここで付与する。
            let link_id = self.active_link_id;
            // REP(`CSI Ps b`、タスク#48)が繰り返す対象として、文字・属性・その時点の
            // アクティブリンクの組を凍結して記録する。実際にセルへ書き込む直前
            // (このif内)でのみ更新することで、画面外(このブロックに入らない場合)に
            // 対する`print()`呼び出しでは更新しない——「実際に画面へ書かれた最後の
            // graphic文字」という定義を保つ。
            self.last_graphic_cell = Some((c, attrs, link_id));
            let mut cell = attrs.to_cell(smol_str::SmolStr::new(c.encode_utf8(&mut [0u8; 4])));
            cell.link_id = link_id;
            *self.cell_mut(self.cursor_row, self.cursor_col) = cell;
            let has_room_for_placeholder = width == 2 && self.cursor_col + 1 < self.cols;
            let advance = if has_room_for_placeholder {
                // wide文字の2セル目(placeholder)も現在の属性(reverse等も含め)を
                // 正しく引き継ぐ — 以前は bold だけ無条件で false になっていた。
                let mut placeholder = attrs.to_cell(smol_str::SmolStr::new_inline(" "));
                placeholder.is_wide_placeholder = true;
                placeholder.link_id = link_id;
                *self.cell_mut(self.cursor_row, self.cursor_col + 1) = placeholder;
                2
            } else {
                1
            };
            if !self.autowrap_mode && width == 2 && !has_room_for_placeholder {
                // DECAWM off時、全角文字を最終列(`cols - 1`)へ書いたためplaceholder
                // を置く列が無かったケース(`needs_wrap`の`else`枝、上のコメント参照)。
                // このまま放置すると「表示幅2だがplaceholderを伴わない孤立した本体
                // セル」が残ってしまう(タスク#85)。ICH/DCH/ECH(`sanitize_wide_row`)
                // と同じ規約で、片割れを持てなかった本体を空白セルへ変換する。
                //
                // `cols == 1` の端末(全角文字が絶対に収まらない、タスク#56)は対象外
                // ——`!self.autowrap_mode`条件により、DECAWM on時のこのケース
                // (test_wide_char_in_one_column_terminal_does_not_waste_a_blank_row_first)
                // では発火しない。
                self.sanitize_wide_row(self.cursor_row);
            }
            if self.autowrap_mode {
                self.cursor_col += advance;
            } else {
                // DECAWM offの間は折り返し待ち状態(`cursor_col == cols`)自体に
                // 入らせない——常に見えている最終列にクランプし、次のprint()も
                // 同じ列を上書きする。
                self.cursor_col = (self.cursor_col + advance).min(self.cols.saturating_sub(1));
            }
        }
    }
}

impl Perform for Terminal {
    fn print(&mut self, c: char) {
        // SI/SO(`execute`)・`ESC ( `/`ESC ) `(`esc_dispatch`)で選択された文字集合が
        // DEC Special Graphicsの場合、ASCII範囲の文字をUnicode罫線/記号文字へ書き換えて
        // からセルへ書き込む(タスク#41)。結合文字判定(`print_mapped`内)より前に行う
        // 必要がある — 写像先の罫線文字はいずれも幅1(結合文字になり得ない)。
        let active_charset = if self.gl_is_g1 { self.g1_charset } else { self.g0_charset };
        let c = if active_charset == Charset::DecSpecialGraphics {
            dec_special_graphics(c)
        } else {
            c
        };
        // OSC 133(タスク#13)「直前コマンドの出力だけをコピー」向けのテキスト
        // キャプチャ。ここは`print_mapped`のグリッド書き込み(結合文字・折り返し等)
        // より前でよい——vteが呼ぶ`print()`はデコード済みの1文字ずつなので、
        // ここでの単純な文字列連結がそのまま結合文字を含む正しいUnicodeテキストになる
        // (`capturing_command_output`フィールドdocコメントのスコープ外事項も参照)。
        if self.capturing_command_output {
            self.current_output_line.push(c);
        }
        self.print_mapped(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            // `saturating_add`(`wrapping_add`ではない): u64::MAXで頭打ちにする。
            // `wrapping_add`だとu64::MAX→0の周回でbell_generationが後退し、
            // 「前回より値が進んだか」で検知する呼び出し側の単調増加前提
            // (フィールドdoc参照)を壊してしまう(Codexレビュー: タスク#24)。
            0x07 => { self.bell_generation = self.bell_generation.saturating_add(1); }
            0x0D => { self.cursor_col = 0; }
            0x0A | 0x0B | 0x0C => {
                // OSC 133(タスク#13)出力キャプチャ: 明示的なLF/VT/FFのみを論理行の
                // 区切りとして扱う(`print_mapped`の自動折り返しも内部で`newline()`を
                // 呼ぶが、あれは表示上の折り返しであり論理行の区切りではないため、
                // ここ[execute、実際のC0制御バイト経由の呼び出し]でのみ確定させる)。
                if self.capturing_command_output {
                    self.current_output_lines.push(std::mem::take(&mut self.current_output_line));
                }
                self.newline();
            }
            0x08 => { if self.cursor_col > 0 { self.cursor_col -= 1; } }
            // SO(Shift Out)/SI(Shift In、タスク#41): GL(印字に使われる文字集合)を
            // G1/G0へ切り替える。実際の写像先(ASCIIかDEC Special Graphicsか)は
            // `g0_charset`/`g1_charset`(`esc_dispatch`の`ESC ( `/`ESC ) `で設定)を
            // `print()`が参照する。
            0x0E => { self.gl_is_g1 = true; }  // SO
            0x0F => { self.gl_is_g1 = false; } // SI
            // HT(タブ、タスク#61): 現在位置より右にある最も近いタブストップへ進む
            // (HTS/`ESC H`・TBC/`CSI g`で変更可能な[Terminal::tab_stops]を参照。
            // 未設定なら既定の8列おき)。
            0x09 => { self.cursor_col = self.next_tab_stop(self.cursor_col); }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _ignore: bool, action: char) {
        let is_dec = intermediates.first() == Some(&b'?');
        let ps: Vec<u16> = params.iter().map(|sub| sub[0]).collect();
        let p0 = ps.get(0).copied().unwrap_or(0);
        let p1 = ps.get(1).copied().unwrap_or(0);

        // Kitty keyboard protocol(タスク#54、`CSI > flags u`push/`CSI < Pn u`pop/
        // `CSI = flags ; mode u`set/`CSI ? u`query)。DECSCUSR/DA/DSRと同様、
        // action(`u`)だけでなくintermediatesを明示的に見て`is_dec`分岐より前に
        // 横取りする必要がある——理由は2つ:
        // 1. `CSI ? u`は`intermediates == [b'?']`なので、これより後にある`is_dec`
        //    分岐(`?`のみを見る)に先に捕まると無条件`return`(1788行目付近)で
        //    無応答のまま握りつぶされる(DECモードクエリと誤認、Fableレビュー
        //    指摘と同種の中間バイト分岐漏れ)。
        // 2. intermediatesを見ない既存の`match action { 'u' => restore_cursor_decrc() }`
        //    (下の総合`match action`ブロック内)は、`>`/`<`/`=`付きの`u`もCSI u
        //    (ANSI.SYS方言のrestore cursor、タスク#57)として誤処理してしまう。
        //
        // フラグスタックはmain/alt画面ごとに独立して保持する(仕様: "The main and
        // alternate screens...must maintain their own, independent, keyboard mode
        // stacks"、`kitty_flags_stack`/`kitty_flags_stack_mut`が`alt_active`で
        // 参照先を切り替える)。
        if action == 'u' && intermediates == [b'>'] {
            // Push: 新しいエントリ(既定flags=0)をスタック最上段に積む。上限に
            // 達している場合は最も古いエントリ(index 0)を追い出す(仕様通り)。
            let flags = p0;
            let stack = self.kitty_flags_stack_mut();
            if stack.len() >= KITTY_KEYBOARD_STACK_MAX {
                stack.remove(0);
            }
            stack.push(flags);
            return;
        }
        if action == 'u' && intermediates == [b'<'] {
            // Pop: 既定1エントリ。要求数がスタックの深さを超えてもpanicせず、
            // 空になるだけ(仕様: 空になれば有効flagsは0=legacy modeへ戻る、
            // `kitty_keyboard_flags`のフォールバックがそのまま実現する)。
            let n = (p0.max(1) as usize).min(self.kitty_flags_stack().len());
            let stack = self.kitty_flags_stack_mut();
            let new_len = stack.len() - n;
            stack.truncate(new_len);
            return;
        }
        if action == 'u' && intermediates == [b'='] {
            // Set: 現在のトップエントリのflagsを`mode`(既定1)に応じて更新する。
            // 1=丸ごと置換、2=指定ビットをOR(セット)、3=指定ビットをAND NOT
            // (クリア)。仕様は「スタックが空の状態でSetされた場合」を明記して
            // いないため、pushと同様に新しいエントリを1つ作る(=現在flagsを
            // 更新するという利用者の意図に最も自然に合致する実装判断)。
            let flags = p0;
            let mode = if p1 == 0 { 1 } else { p1 };
            let stack = self.kitty_flags_stack_mut();
            let current = stack.last().copied().unwrap_or(0);
            let new_flags = match mode {
                2 => current | flags,
                3 => current & !flags,
                _ => flags, // mode 1(既定)およびその他未定義値
            };
            if let Some(top) = stack.last_mut() {
                *top = new_flags;
            } else {
                stack.push(new_flags);
            }
            return;
        }
        if action == 'u' && intermediates == [b'?'] {
            // Query: 現在有効なflags(スタック最上段、空なら0)を`CSI ? flags u`で
            // 返す。応答経路はDA/DSR(タスク#38)と同じ`pending_terminal_responses`。
            let flags = self.kitty_keyboard_flags();
            let resp = format!("\x1b[?{}u", flags);
            self.pending_terminal_responses.push(resp.into_bytes());
            return;
        }

        if is_dec {
            // 全DECモード(`?1000`等)を`p0`(先頭パラメータ)だけでなく`ps`全体を
            // ループして処理する(タスク#68)。実アプリ(vim/tmux等)は`CSI ?1049;25h`
            // (alt screen切替+カーソル表示、DECTCEMは`h`=表示/`l`=非表示)や
            // `CSI ?1000;1006h`(マウストラッキング+
            // SGR拡張)のように複数モードを1シーケンスにまとめて送ることが珍しくなく、
            // `p0`しか見ないと後続のパラメータが無視される(元々マウス関連のみ
            // `for &p in &ps`ループで対応済みだったが、他のモードは`match (action, p0)`
            // で先頭だけしか見ておらずcodexレビュー指摘の残課題だった——1つのループへ統合)。
            for &p in &ps {
                match (action, p) {
                    ('h', 1000) => { self.mouse_reporting_mode = MouseReportingMode::Normal; }
                    ('h', 1002) => { self.mouse_reporting_mode = MouseReportingMode::ButtonEvent; }
                    ('h', 1003) => { self.mouse_reporting_mode = MouseReportingMode::AnyEvent; }
                    // `?1000`/`?1002`/`?1003`は実xtermと同様に単一の内部モード変数を
                    // 共有する——後からsetしたものが有効になり、いずれかをreset(`l`)
                    // すると番号に関わらず無効(`Off`)に戻る(どのモード番号でreset
                    // 要求されたかは区別しない、xterm実装に合わせた挙動)。
                    ('l', 1000) | ('l', 1002) | ('l', 1003) => { self.mouse_reporting_mode = MouseReportingMode::Off; }
                    // SGR拡張マウスレポーティング(`?1006`)。マウスモード自体
                    // (`?1000`/`?1002`/`?1003`)とは独立に有効/無効を切り替えられる
                    // (xterm互換: SGRだけ先にonにしておいて後からトラッキングモードを
                    // 選ぶ、という順序も許容する必要があるため)。
                    ('h', 1006) => { self.sgr_mouse_mode = true; }
                    ('l', 1006) => { self.sgr_mouse_mode = false; }
                    ('h', 1007) => { self.alternate_scroll = true; }
                    ('l', 1007) => { self.alternate_scroll = false; }
                    ('h', 1015) => { self.urxvt_mouse_mode = true; }
                    ('l', 1015) => { self.urxvt_mouse_mode = false; }
                    ('h', 47) | ('h', 1047) => { self.switch_to_alt(false); }
                    ('h', 1049) => { self.switch_to_alt(true); }
                    ('l', 47) | ('l', 1047) => { self.switch_to_main(false); }
                    ('l', 1049) => { self.switch_to_main(true); }
                    ('h', 25) => { self.cursor_visible = true; }
                    ('l', 25) => { self.cursor_visible = false; }
                    // DECSET/DECRST ?12(`CSI ?12h`/`CSI ?12l`): カーソル点滅on/off単体。
                    // `CursorShape`(DECSCUSR、`CSI Ps SP q`)とは独立したフィールドを
                    // 更新する——DECSCUSRのパラメータでも`cursor_blink`は変わるが、
                    // こちらは形状を変えずに点滅の有無だけを切り替える(タスク#55)。
                    ('h', 12) => { self.cursor_blink = true; }
                    ('l', 12) => { self.cursor_blink = false; }
                    ('h', 1) => { self.application_cursor_mode = true; }
                    ('l', 1) => { self.application_cursor_mode = false; }
                    // DECAWM(`CSI ?7h`/`CSI ?7l`): 自動折り返しon/off(タスク#56)。
                    ('h', 7) => { self.autowrap_mode = true; }
                    ('l', 7) => { self.autowrap_mode = false; }
                    // DECOM(`CSI ?6h`/`CSI ?6l`、origin mode、タスク#59)。実端末(xterm含む)
                    // に倣い、on/offどちらの切り替えでもカーソルをhome位置へ移動する
                    // (on: scroll_top行、off: 0行目。列は常に0)。
                    ('h', 6) => {
                        self.origin_mode = true;
                        self.cursor_row = self.scroll_top;
                        self.cursor_col = 0;
                    }
                    ('l', 6) => {
                        self.origin_mode = false;
                        self.cursor_row = 0;
                        self.cursor_col = 0;
                    }
                    ('h', 2004) => { self.bracketed_paste_mode = true; }
                    ('l', 2004) => { self.bracketed_paste_mode = false; }
                    // DEC Synchronized Output(`CSI ?2026h`/`CSI ?2026l`)。tmux/neovim等が
                    // フルスクリーン再描画を1フレームとして送るために使う。実際の画面
                    // 更新push抑制はここでは行わない([synchronized_output_active]
                    // フィールドのdocコメント参照)——`Terminal`はフラグを保持するだけ。
                    ('h', 2026) => { self.synchronized_output_active = true; }
                    ('l', 2026) => { self.synchronized_output_active = false; }
                    // フォーカスレポーティング(`CSI ?1004h`/`l`、タスク#60)。有効な間だけ
                    // [encode_focus_event]がOS由来のフォーカス変化を`CSI I`/`CSI O`へ
                    // エンコードして送るようになる。
                    ('h', 1004) => { self.focus_reporting_mode = true; }
                    ('l', 1004) => { self.focus_reporting_mode = false; }
                    _ => {}
                }
            }
            return;
        }

        // DECSCUSR(`CSI Ps SP q`): 中間バイトが SP(0x20)単体の場合のみ扱う。
        // 中間バイト無しの `CSI Ps q`(DECLL、別機能・未実装)と区別するため、
        // action(`q`)だけでなく intermediates を明示的に確認する——ここを見落とすと
        // DECLL を誤ってカーソル形状変更として処理してしまう(Fableレビュー指摘)。
        if action == 'q' && intermediates == [b' '] {
            self.set_cursor_shape_from_decscusr(p0);
            return;
        }

        // Primary DA(`CSI c`/`CSI 0 c`)と Secondary DA(`CSI > c`)は同じ action('c')だが、
        // vte は `>` を intermediates に入れて渡すため、DECSCUSR と同様ここで
        // intermediates を見て明示的に分岐してから return する(タスク#38、Fableレビュー
        // 指摘: `is_dec`の`?`判定と混同しないこと)。応答は新しいtransport経路を作らず、
        // 既存の `SideEffect::SendStdin` 経路(`SessionState::apply()`が
        // `take_pending_terminal_responses()`を変換する)にそのまま乗せる。
        // `Ps`(p0)は仕様上0のみが有効な識別要求で、vte自身のANSIハンドラも
        // `next_param_or(0) == 0` を条件にしている(Codexレビュー指摘)ため、それ以外の
        // `Ps`(例: `CSI 1c`)には応答しない。
        if action == 'c' && intermediates.is_empty() && p0 == 0 {
            // Primary DA: VT100 with AVO を名乗る、広く使われる最小応答に`;4`
            // (Sixelグラフィックス対応、タスク#42)を追加する。多くのアプリは
            // DA1応答に"4"が含まれるかだけを見てSixelを送るかどうか判断するため、
            // 既存の分類("VT100 with AVO")自体は変えずに属性だけ追加する
            // (Fable 2次レビュー: #38をこのタスクのblockedByにした理由そのもの——
            // これを広告しない限り多くのアプリはそもそもSixelを送ってこない)。
            self.pending_terminal_responses.push(b"\x1b[?1;2;4c".to_vec());
            return;
        }
        if action == 'c' && intermediates == [b'>'] && p0 == 0 {
            // Secondary DA: `CSI > Pp ; Pv ; Pc c`(端末種別;ファームウェア版;cartridge)。
            self.pending_terminal_responses.push(b"\x1b[>0;100;0c".to_vec());
            return;
        }
        // DSR(`CSI 5n`: device status, `CSI 6n`: CPR/cursor position report)。
        // fish のプロンプト位置検出や neovim の DA1/DA2 起動時タイムアウトが CPR 未応答に
        // 実際に依存しているため(タスク#38、Fable 2次レビューでP1へ昇格)、両方に応答する。
        if action == 'n' && intermediates.is_empty() {
            match p0 {
                5 => { self.pending_terminal_responses.push(b"\x1b[0n".to_vec()); }
                6 => {
                    // `print()`は右端に書いた直後、実際に折り返すのは次のprintable文字を
                    // 受けた時まで遅延させるため(delayed wrap)、その間`cursor_col`は
                    // `cols`(範囲外)になり得る。CPRは可視上のカーソル位置(最終列)を
                    // 報告すべきなので`cols - 1`にクランプする(Codexレビュー指摘)。
                    let visible_col = self.cursor_col.min(self.cols.saturating_sub(1));
                    // origin modeが有効な間、CPRが報告する行はCUP/HVPと同じ座標系
                    // (scroll_top基準の相対値)になる(タスク#59、実端末の挙動)。
                    let (floor, _) = self.origin_row_bounds();
                    let reported_row = self.cursor_row.saturating_sub(floor);
                    let resp = format!("\x1b[{};{}R", reported_row + 1, visible_col + 1);
                    self.pending_terminal_responses.push(resp.into_bytes());
                }
                _ => {}
            }
            return;
        }

        match action {
            // CUU/CUD/CNL/CPL(`A`/`B`/`E`/`F`): origin modeが有効な間は画面全体ではなく
            // scroll regionの上下端([scroll_top, scroll_bottom])が可動範囲になる
            // (タスク#59)。`origin_row_bounds()`はoffの場合`(0, rows-1)`を返すので
            // 既存(origin mode無し)の挙動はそのまま保たれる。
            'A' => {
                let n = p0.max(1) as usize;
                let (floor, _) = self.origin_row_bounds();
                self.cursor_row = self.cursor_row.saturating_sub(n).max(floor);
            }
            'B' => {
                let n = p0.max(1) as usize;
                let (_, ceil) = self.origin_row_bounds();
                self.cursor_row = (self.cursor_row + n).min(ceil);
            }
            'C' => { let n = p0.max(1) as usize; self.cursor_col = (self.cursor_col + n).min(self.cols - 1); }
            'D' => { let n = p0.max(1) as usize; self.cursor_col = self.cursor_col.saturating_sub(n); }
            'E' => {
                let n = p0.max(1) as usize;
                let (_, ceil) = self.origin_row_bounds();
                self.cursor_row = (self.cursor_row + n).min(ceil);
                self.cursor_col = 0;
            }
            'F' => {
                let n = p0.max(1) as usize;
                let (floor, _) = self.origin_row_bounds();
                self.cursor_row = self.cursor_row.saturating_sub(n).max(floor);
                self.cursor_col = 0;
            }
            // CHA(`CSI Ps G`)/HPA(`CSI Ps `` `、タスク#65)。どちらも「列を`Ps`
            // (既定1、1-indexed)へ絶対移動する」で完全に同じ挙動(xterm含む実端末も
            // 区別しない)なので同じ腕で処理する。`TERM=xterm-256color`(ssh_handler.rs)
            // が広告するterminfoに`hpa`があり、ncurses/readlineが実際にHPAを発行し
            // 得るがこれまで未対応だった(Fableレビュー指摘)。
            'G' | '`' => { self.cursor_col = (p0.max(1) as usize - 1).min(self.cols - 1); }
            // CHT(`CSI Ps I`、タスク#65)。カーソルを右方向へ`Ps`個(既定1)先の
            // タブストップまで移動する。`next_tab_stop`はHTS/`ESC H`・TBC/`CSI g`
            // (タスク#61)で変更された[Terminal::tab_stops]を参照するため、可変
            // タブストップにも対応する。行末(`cols - 1`)に達したらそれ以上進まない
            // (`next_tab_stop`自体がそこでクランプする)。
            //
            // `execute`のHTと全く同じ「`next_tab_stop`で計算してからクランプ」の
            // 順序にする——事前に`cursor_col >= cols - 1`で早期breakするガードを
            // 入れると、直前の`print()`による折り返し待ち状態(`cursor_col == cols`、
            // まだ改行していない)を「既に行末にいる」と誤認してカーソルを一切
            // 動かさず、`cursor_col == cols`という画面外の値のまま放置してしまう
            // (codexレビュー指摘)。HTと同じ順序なら、折り返し待ち状態からでも
            // 最終列(`cols - 1`)へ正しく正規化される。
            'I' => {
                let n = p0.max(1) as usize;
                for _ in 0..n {
                    let next = self.next_tab_stop(self.cursor_col);
                    if next == self.cursor_col {
                        // 既に最終列(`next_tab_stop`のフォールバック)に達している。
                        break;
                    }
                    self.cursor_col = next;
                }
            }
            // CBT(`CSI Ps Z`、タスク#65)。CHT('I')と対称: カーソルを左方向へ
            // `Ps`個(既定1)前のタブストップまで移動する。`prev_tab_stop`が
            // 可変タブストップ(タスク#61)を参照する点もCHTと同じ。列0に達したら
            // それ以上戻らない。
            'Z' => {
                let n = p0.max(1) as usize;
                for _ in 0..n {
                    if self.cursor_col == 0 { break; }
                    self.cursor_col = self.prev_tab_stop(self.cursor_col);
                }
            }
            // CUP/HVP(`H`/`f`): origin modeが有効な間、行番号はscroll region上端
            // (`scroll_top`)を基準とした相対値になり、可動範囲も region内に
            // クランプされる(タスク#59)。列は左右マージン未実装のため既存通り
            // 画面全体基準のまま。
            'H' | 'f' => {
                let (floor, ceil) = self.origin_row_bounds();
                self.cursor_row = (floor + p0.max(1) as usize - 1).clamp(floor, ceil);
                self.cursor_col = (p1.max(1) as usize - 1).min(self.cols - 1);
            }
            'J' => {
                // `print()`の遅延折り返し(delayed wrap)中は`cursor_col`が`cols`
                // (範囲外)になり得る。生の`cursor_col`をそのまま使うと、EL/ED問わず
                // 「現在行の右端」を指すはずのインデックスが次行の先頭に1セルはみ出す
                // off-by-oneになる——CPR(`CSI 6n`)と同じく可視上の最終列(`cols - 1`)
                // にクランプしてから計算する(Fableレビュー: タスク#56)。
                let col = self.cursor_col.min(self.cols.saturating_sub(1));
                match p0 {
                    0 => { let s = self.cursor_row * self.cols + col; self.erase_cells(s, self.cols * self.rows); }
                    1 => { let e = self.cursor_row * self.cols + col + 1; self.erase_cells(0, e); }
                    2 | 3 => {
                        self.erase_cells(0, self.cols * self.rows);
                        self.cursor_row = 0; self.cursor_col = 0;
                        // Sixel(タスク#42、codexレビュー指摘): `CSI 2J`/`CSI 3J`は画面
                        // 全体をテキストとして消去する操作であり、`clear(1)`実行時に
                        // 頻繁に送られる。ここで画像配置を消さないと、テキストは
                        // 消えたのに古いSixel画像だけ画面に残り続ける(部分消去の
                        // ED0/ED1・ELはスコープ外のままだが、全画面消去は十分に
                        // 一般的なケースのため対応する、`ImagePlacement`のdocコメント
                        // 参照)。
                        self.clear_images();
                    }
                    _ => {}
                }
            }
            'K' => {
                let row = self.cursor_row;
                // 上の'J'と同じ理由でクランプする。これにより EL0(`CSI 0K`)が
                // 遅延折り返し中(右端に文字を書いた直後)に現在行を消せない
                // (`s == e`でno-opになる)バグ、および EL1(`CSI 1K`)が次行先頭
                // 1セルまで誤って消してしまうバグの両方を修正する(タスク#56)。
                let col = self.cursor_col.min(self.cols.saturating_sub(1));
                match p0 {
                    0 => { let s = row * self.cols + col; let e = (row + 1) * self.cols; self.erase_cells(s, e); }
                    1 => { let s = row * self.cols; let e = row * self.cols + col + 1; self.erase_cells(s, e); }
                    2 => { let s = row * self.cols; let e = (row + 1) * self.cols; self.erase_cells(s, e); }
                    _ => {}
                }
            }
            'L' => { self.insert_lines(p0.max(1) as usize); }
            'M' => { self.delete_lines(p0.max(1) as usize); }
            // ICH/DCH/ECH(タスク#47): 文字単位の挿入・削除・消去。IL/DL(行単位、'L'/'M')
            // とは異なり現在行に閉じる(scroll region非依存)。
            '@' => { self.insert_chars(p0.max(1) as usize); }
            'P' => { self.delete_chars(p0.max(1) as usize); }
            'X' => { self.erase_chars(p0.max(1) as usize); }
            'S' => { self.scroll_up_region(p0.max(1) as usize); }
            // SD(`CSI Ps T`、タスク#49)。SU('S')の対。ただしxtermでは`CSI T`は
            // パラメータ数によって別機能に化ける多重定義シーケンスなので、ここで
            // 明示的にガードする(Fableレビュー指摘):
            // - パラメータ5個(`CSI Ps;Ps;Ps;Ps;Ps T`)は highlight mouse tracking 開始
            //   (未実装・no-opのままにする — 誤ってSDとして解釈すると画面が壊れる)。
            // - `CSI > Ps;Ps T`(intermediateに`>`)はタイトルモードリセット(未実装)で、
            //   SDとは無関係。intermediates非空の`CSI T`は一律SDとして扱わない。
            // SDとして処理してよいのは「パラメータ0〜1個、かつintermediate無し」の
            // 場合のみ。
            'T' if intermediates.is_empty() && params.len() <= 1 => {
                self.scroll_down_region(p0.max(1) as usize);
            }
            // REP(`CSI Ps b`、タスク#48): 直前に画面へ書かれたgraphic文字を、その
            // 文字が書かれた時点のSGR属性のまま`Ps`回繰り返す(既定1回)。
            // `last_graphic_cell`が`None`(画面先頭・RIS直後など直前文字が存在しない
            // 場合)はno-op。`self.print()`を再呼び出しして実現する(ICH/DCH等と異なり
            // 専用の書き込みロジックを持たない)——折り返し・全角文字・DECAWM off等の
            // 挙動を`print()`本体と完全に一致させ二重実装によるズレを防ぐため。
            // `print_mapped()`は常に`self.cur_attrs`(現在値)を参照するため、記録済みの
            // 属性で描画させるにはループの間だけ`cur_attrs`を差し替え、終わったら元に戻す
            // (REP自体はカーソル位置のSGR状態を変更しない副作用のない操作であるべき)。
            // `last_graphic_cell`は写像**後**の文字を保存している(2237行目付近参照)ため、
            // ここでは`print()`ではなく`print_mapped()`を直接呼ぶ——`print()`だと現在の
            // charsetで再度写像されてしまい、保存後にcharsetが切り替わっていた場合
            // (例: ASCIIで'q'を印字→`ESC(0`でDEC Special Graphicsへ切替→REP)に、
            // 記録した文字とは別の文字(この例では'q'ではなく'─')が繰り返されてしまう
            // (タスク#84)。
            'b' => {
                if let Some((c, attrs, link_id)) = self.last_graphic_cell {
                    let n = p0.max(1) as usize;
                    let restore_attrs = self.cur_attrs;
                    let restore_link_id = self.active_link_id;
                    self.cur_attrs = attrs;
                    self.active_link_id = link_id;
                    for _ in 0..n {
                        self.print_mapped(c);
                    }
                    self.cur_attrs = restore_attrs;
                    self.active_link_id = restore_link_id;
                }
            }
            // VPA(`CSI Ps d`): CUP/HVPと同様、origin modeが有効な間は行番号が
            // scroll region上端基準になる(タスク#59)。
            'd' => {
                let (floor, ceil) = self.origin_row_bounds();
                self.cursor_row = (floor + p0.max(1) as usize - 1).clamp(floor, ceil);
            }
            // TBC(`CSI Ps g`、タスク#61)。`Ps`省略/0: 現在のカーソル列のタブストップを
            // 解除する。`Ps`3: 全タブストップを解除する(vim/tmux等がリサイズ後や
            // 端末初期化時に送る、より一般的なケース)。他の`Ps`値(1/2/4/5、行単位の
            // タブ機構等)はこの実装が対応していない機能に対する要求なのでno-op。
            'g' => {
                match p0 {
                    0 => {
                        let col = self.cursor_col.min(self.cols.saturating_sub(1));
                        self.tab_stops[col] = false;
                    }
                    3 => {
                        for stop in self.tab_stops.iter_mut() { *stop = false; }
                    }
                    _ => {}
                }
            }
            'm' => { self.handle_sgr(&ps); }
            'r' => {
                // タスク#64: パラメータ省略(`CSI r`、p0==p1==0)は「画面全体を
                // scroll regionにリセット」であって、xtermも含め実端末はこれを
                // `CSI 1;<rows>r`と等価に扱う。だが`p0.max(1)`/`p1.max(1)`で
                // どちらも1にフォールバックしていたため、省略時top=bot=0となり
                // `top < bot`が偽になって何もしない(vim/less終了直後などscroll
                // regionが元に戻らない表示破壊バグ)になっていた。省略された
                // パラメータだけをデフォルト値(top→画面最上行、bot→画面最下行)
                // に補うことで、`CSI Ntr`(bot省略)のような片側省略も含め
                // 仕様通りの挙動にする。
                let top = if p0 == 0 { 0 } else { (p0 as usize - 1).min(self.rows - 1) };
                let bot = if p1 == 0 { self.rows - 1 } else { (p1 as usize - 1).min(self.rows - 1) };
                if top < bot {
                    self.scroll_top = top;
                    self.scroll_bottom = bot;
                    // DECSTBM(タスク#59、codexレビュー指摘): 実端末(xterm含む)は
                    // scroll region変更のたびカーソルをhome位置(origin mode on:
                    // 新しい scroll_top行、off: 画面0行目。いずれも列0)へ移動する。
                    // これを怠ると、DECOM on中に狭いregionを設定した直後、カーソルが
                    // 新region外に取り残された状態になり得る(CUU/CUDのregion内
                    // clamp・CPRのregion相対報告という他のDECOM挙動の前提が崩れる)。
                    let (home_row, _) = self.origin_row_bounds();
                    self.cursor_row = home_row;
                    self.cursor_col = 0;
                }
            }
            // CSI s / CSI u(ANSI.SYS方言のsave/restore cursor、タスク#57)。
            // `CSI s`は本来DECSLRM(左右マージン設定、DECLRMM `?69h`有効時のみ)と
            // 曖昧だが、DECLRMM/左右マージン自体がこのコードベースに未実装
            // (scroll_top/scroll_bottomの上下marginのみ対応)なので、常にDECSCと
            // 同義の save cursor として扱ってよい。
            's' => { self.save_cursor_decsc(); }
            'u' => { self.restore_cursor_decrc(); }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        match (params.get(0), params.get(1)) {
            (Some(&b"0"), Some(title)) | (Some(&b"2"), Some(title)) => {
                if let Ok(s) = std::str::from_utf8(title) {
                    self.title = Some(s.to_string());
                }
            }
            // OSC 10/11(`ESC]10;<spec>ST`/`ESC]11;<spec>ST`): default
            // foreground/background色のset・query(タスク#58)。vim/neovimは
            // 起動時に`ESC]11;?`をqueryして背景の明暗を判定し
            // `background`オプション(termguicolors連携)を自動設定するため、
            // 特にqueryへの応答実装が実利が大きい(Fableレビュー指摘)。
            // `Pt == "?"`はquery(host→remoteへ現在値を返す)、それ以外は
            // set(このセッションのtheme既定色を更新する)。応答は新しい
            // transport経路を作らず、DA/DSR(タスク#38)と同じ
            // `pending_terminal_responses`→`SideEffect::SendStdin`経路に乗せる。
            (Some(&b"10"), Some(&spec)) => {
                self.handle_osc_default_color(true, spec, bell_terminated);
            }
            (Some(&b"11"), Some(&spec)) => {
                self.handle_osc_default_color(false, spec, bell_terminated);
            }
            // OSC 8(`ESC]8;params;URIST`): ハイパーリンク(タスク#40)。詳細は
            // `handle_osc8_hyperlink`のdocコメント参照。`params.get(1)`が`None`
            // (`ESC]8ST`のようなパラメータ自体が無い形)も含めて丸ごと委譲する
            // (`handle_osc8_hyperlink`側で`params.len()`を見て安全にno-op/close
            // として扱う)。
            (Some(&b"8"), _) => {
                self.handle_osc8_hyperlink(params);
            }
            // OSC 133(`ESC]133;<Ps>[;...]ST`、タスク#13): セマンティックプロンプト
            // (A=プロンプト開始/B=コマンド入力開始/C=コマンド実行開始/D=コマンド終了)。
            // 詳細は`handle_osc133_prompt_mark`参照。
            (Some(&b"133"), Some(&sub)) => {
                self.handle_osc133_prompt_mark(sub, params.get(2).copied());
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

    /// DCS(`ESC P ... <final>`)開始。タスク#42: 最終バイトが`q`のものだけSixelとして
    /// デコード対象にする(`Pa;Pb;Ph`は`sixel.rs`のモジュールdoc「実装範囲」参照 —
    /// 使わない)。それ以外の最終バイト(未対応のDCSサブプロトコル)は`sixel_decoder`を
    /// `None`のままにし、後続の`put`/`unhook`が単に無視するようにする(従来通り
    /// 黙って破棄)。
    fn hook(&mut self, _params: &vte::Params, _ints: &[u8], _ignore: bool, c: char) {
        if c == 'q' {
            self.sixel_decoder = Some(SixelDecoder::new());
        } else {
            self.sixel_decoder = None;
        }
    }
    fn put(&mut self, byte: u8) {
        if let Some(dec) = self.sixel_decoder.as_mut() {
            dec.feed(byte);
        }
    }
    fn unhook(&mut self) {
        if let Some(dec) = self.sixel_decoder.take() {
            if let Some(img) = dec.finish() {
                self.place_sixel_image(img);
            }
        }
    }
    fn esc_dispatch(&mut self, ints: &[u8], _ignore: bool, byte: u8) {
        match byte {
            // RI(Reverse Index、`ESC M`)。intermediateが空の場合のみ扱う——`ESC ( M`/
            // `ESC ) M`(G0/G1文字セット指定の最終バイトとして`M`が来るケース)を
            // このガード無しに先に捕まえてしまい、G0/G1フォールバック(下記)より前に
            // 画面をスクロールしてしまう誤動作があったため(タスク#75、fable/codex指摘)。
            b'M' if ints.is_empty() => {
                if self.cursor_row == self.scroll_top {
                    // RIによる領域内シフトも行座標がずれる構造的変更——#93と同じ理由で
                    // 全画面dirty(セルフレビューで検出: sibling関数のscroll_up_region等は
                    // 全て立てていたのにRIだけ漏れていた)。
                    self.full_damage_pending = true;
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
            // RIS(Reset to Initial State、`ESC c`)。intermediateが空の場合のみ扱う
            // ——`ESC ( c`(G0文字セット指定の最終バイトとして`c`が来るケース)を
            // このガード無しに先に捕まえてしまい、G0/G1フォールバック(下記)より前に
            // 全画面リセットしてしまう誤動作があったため(タスク#75、fable/codex指摘)。
            b'c' if ints.is_empty() => { self.reset_all(); }
            // DECSC/DECRC(タスク#57)。`b'7'`/`b'8'`はASCII '7'/'8'(0x37/0x38)。
            // intermediateが空の場合のみDECSC/DECRCとして扱う——`ESC # 8`(DECALN、
            // screen alignment test、未実装につきno-op)がintermediate `#`付きの
            // 別シーケンスとして同じ最終バイト`8`を使うため、`ints`を無視すると
            // DECALNまで誤ってDECRCとして処理してしまう(codexレビュー指摘)。
            b'7' if ints.is_empty() => { self.save_cursor_decsc(); }
            b'8' if ints.is_empty() => { self.restore_cursor_decrc(); }
            // HTS(`ESC H`、タスク#61): 現在のカーソル列にタブストップを設定する。
            // `print()`の遅延折り返し中(`cursor_col == cols`)は他のカーソル位置参照
            // 箇所(CPR/EL/ED等)と同様、可視上の最終列(`cols - 1`)にクランプしてから
            // 設定する。intermediateが空の場合のみ扱う(`ESC 7`/`ESC 8`と同じ理由で、
            // 中間バイト付きの別シーケンスと最終バイトが衝突しないことを明示する)。
            b'H' if ints.is_empty() => {
                let col = self.cursor_col.min(self.cols.saturating_sub(1));
                self.tab_stops[col] = true;
            }
            // G0/G1文字セット指定(`ESC ( <final>`/`ESC ) <final>`、タスク#41)。
            // `byte`は最終バイト(vteが`ints`と`byte`を分離して渡す——中間バイト
            // `(`/`)`自体は`byte`には現れない)。`0`(DEC Special Graphics)だけ
            // マッピングテーブルを持ち、それ以外の最終バイト(`B`=US ASCII、
            // UK `A`等の他の国別セット)は全てASCIIとして扱う([Charset]の
            // docコメント参照——codexレビュー指摘: 以前は`B`以外の未知の最終
            // バイトを無視しており、DEC Special Graphics指定中に`ESC ( A`等が
            // 来てもASCIIへ戻せなかった)。
            b'0' if ints == [b'('] => { self.g0_charset = Charset::DecSpecialGraphics; }
            _ if ints == [b'('] => { self.g0_charset = Charset::Ascii; }
            b'0' if ints == [b')'] => { self.g1_charset = Charset::DecSpecialGraphics; }
            _ if ints == [b')'] => { self.g1_charset = Charset::Ascii; }
            // DECKPAM/DECKPNM(`ESC =`/`ESC >`、タスク#43)。intermediateが空の場合のみ
            // 扱う(`ESC 7`/`ESC 8`/`ESC H`と同じ理由——中間バイト付きの別シーケンスと
            // 最終バイトが衝突しないことを明示する)。
            b'=' if ints.is_empty() => { self.application_keypad_mode = true; }
            b'>' if ints.is_empty() => { self.application_keypad_mode = false; }
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
    fn test_su_clamp_count_beyond_region_size_without_panic() {
        // タスク#69: `n`がフルスクロール領域幅(ここでは5)以上のとき、
        // `scroll_up_region`が`bot - n`で`usize`アンダーフローしてpanicしていた
        // 既存バグの回帰テスト(`test_sd_clamp_count_beyond_region_size_without_panic`
        // と対称)。
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        feed(&mut t, b"\x1b[100S");
        for row in 0..5 {
            assert_eq!(cell(&t, row, 0), " ", "row {row} should be blank after over-sized SU");
        }
    }

    #[test]
    fn test_su_full_region_scroll_pushes_all_rows_to_pending_scrollback() {
        // `n == region_size`(regionが画面全体、top==0)のとき、押し出された全行が
        // pending_scrollbackへ積まれ、かつpanicしないことを確認する。
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        feed(&mut t, b"\x1b[5S");
        let pending = t.take_scrollback();
        assert_eq!(pending.len(), 5);
        for row in 0..5 {
            assert_eq!(cell(&t, row, 0), " ", "row {row} should be blank after full-region SU");
        }
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
    fn test_sgr_underline_italic_strikethrough_blink_dim_invisible() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[4;3;9;5;2;8mA");
        let c = &t.screen_cells()[0];
        assert!(c.underline, "SGR 4 should set underline");
        assert!(c.italic, "SGR 3 should set italic");
        assert!(c.strikethrough, "SGR 9 should set strikethrough");
        assert!(c.blink, "SGR 5 should set blink");
        assert!(c.dim, "SGR 2 should set dim");
        assert!(c.invisible, "SGR 8 should set invisible");
    }

    #[test]
    fn test_sgr_individual_attribute_resets() {
        let mut t = Terminal::new(80, 24, Theme::default());
        // 全部立てた上で、それぞれの reset コードだけを送り個別に消せることを確認する。
        feed(&mut t, b"\x1b[4;3;9;5;2;8m\x1b[24;23;29;25;22;28mA");
        let c = &t.screen_cells()[0];
        assert!(!c.underline, "SGR 24 should reset underline");
        assert!(!c.italic, "SGR 23 should reset italic");
        assert!(!c.strikethrough, "SGR 29 should reset strikethrough");
        assert!(!c.blink, "SGR 25 should reset blink");
        assert!(!c.dim, "SGR 22 should reset dim");
        assert!(!c.invisible, "SGR 28 should reset invisible");
    }

    #[test]
    fn test_sgr_22_resets_both_bold_and_dim() {
        // SGR 22 は「bold/dim いずれかを解除する」共通のリセットコードであり、
        // bold(1)/dim(2) どちらが立っていても両方消す(SGR仕様通り、個別コードは無い)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[1;2m\x1b[22mA");
        let c = &t.screen_cells()[0];
        assert!(!c.bold, "SGR 22 should reset bold");
        assert!(!c.dim, "SGR 22 should reset dim");
    }

    #[test]
    fn test_sgr_reverse_swaps_effective_colors_at_write_time() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[31;44;7mA"); // fg=red, bg=blue, reverse
        let c = &t.screen_cells()[0];
        // 実効色は書込み時に fg/bg が入れ替わって解決されている
        // (このコードベースの「SGRパース時にARGBへ解決する」既存方針に合わせる)。
        assert_eq!(c.fg, Theme::default().ansi16[4], "reverse: effective fg should be the logical bg (blue)");
        assert_eq!(c.bg, Theme::default().ansi16[1], "reverse: effective bg should be the logical fg (red)");
    }

    #[test]
    fn test_sgr_27_reverse_reset_restores_original_colors() {
        let mut t = Terminal::new(80, 24, Theme::default());
        // reverse を解除(SGR 27)した後に書いた文字は元の論理色(fg=red,bg=blue)のまま。
        feed(&mut t, b"\x1b[31;44;7m\x1b[27mA");
        let c = &t.screen_cells()[0];
        assert_eq!(c.fg, Theme::default().ansi16[1]);
        assert_eq!(c.bg, Theme::default().ansi16[4]);
    }

    #[test]
    fn test_sgr_0_and_empty_ps_clear_all_new_attributes() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[1;2;3;4;5;7;8;9m\x1b[0mA");
        let c = &t.screen_cells()[0];
        assert!(!c.bold && !c.dim && !c.italic && !c.underline && !c.blink && !c.invisible && !c.strikethrough);
        assert_eq!(c.fg, Theme::default().default_fg);
        assert_eq!(c.bg, Theme::default().default_bg);

        // 空パラメータ(`ESC[m`)も SGR 0 と同義であるべき(Fableレビュー指摘)。
        let mut t2 = Terminal::new(80, 24, Theme::default());
        feed(&mut t2, b"\x1b[1;2;3;4;5;7;8;9m\x1b[mB");
        let c2 = &t2.screen_cells()[0];
        assert!(!c2.bold && !c2.dim && !c2.italic && !c2.underline && !c2.blink && !c2.invisible && !c2.strikethrough);
        assert_eq!(c2.fg, Theme::default().default_fg);
        assert_eq!(c2.bg, Theme::default().default_bg);
    }

    #[test]
    fn test_blank_and_wide_char_placeholder_inherit_current_attributes() {
        let mut t = Terminal::new(80, 24, Theme::default());
        // wide文字(全角)の2セル目(placeholder)も現在のSGR属性を引き継ぐこと。
        feed(&mut t, b"\x1b[1;4m\xE3\x81\x82"); // bold+underline, "あ"(全角、UTF-8: E3 81 82)
        assert_eq!(t.screen_cells()[1].ch.as_str(), " ");
        assert!(t.screen_cells()[1].bold, "wide-char placeholder should inherit bold");
        assert!(t.screen_cells()[1].underline, "wide-char placeholder should inherit underline");

        // erase(`blank()`経由)で作られる空白セルも現在のSGR属性を引き継ぐこと。
        feed(&mut t, b"\x1b[2J");
        let c = &t.screen_cells()[0];
        assert!(c.bold, "erased blank cells should inherit bold");
        assert!(c.underline, "erased blank cells should inherit underline");
    }

    #[test]
    fn test_combining_char_appends_to_previous_cell_without_advancing_cursor() {
        // 幅0の結合文字(U+0301 COMBINING ACUTE ACCENT)は、独立したセルとして
        // カーソル位置に置かれるのではなく、直前のセルへグラフェムとして付加され、
        // カーソルは進まない(Fableレビュー: タスク#39)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, "e\u{0301}".as_bytes());
        assert_eq!(cell(&t, 0, 0), "e\u{0301}");
        assert_eq!(t.cursor_col(), 1, "combining char must not advance the cursor");
        // 続けて通常文字を書くと、結合文字の直後の(進んでいない)セルに書かれる。
        feed(&mut t, b"f");
        assert_eq!(cell(&t, 0, 1), "f");
        assert_eq!(t.cursor_col(), 2);
    }

    #[test]
    fn test_combining_char_after_wide_char_attaches_to_wide_char_main_cell() {
        // 全角文字の直後に結合文字が来た場合、2セル目のプレースホルダではなく
        // 全角文字自身の本体セル(1セル目)へ付加するのが正解(Fableレビュー: タスク#39)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, "\u{3042}\u{0301}".as_bytes()); // "あ" + COMBINING ACUTE ACCENT
        assert_eq!(cell(&t, 0, 0), "\u{3042}\u{0301}", "combining char should attach to the wide char's main cell");
        assert_eq!(cell(&t, 0, 1), " ", "wide char placeholder cell must stay untouched");
        assert_eq!(t.cursor_col(), 2, "cursor must stay right after the wide char (unchanged by the combining char)");
    }

    #[test]
    fn test_combining_char_at_wrap_pending_attaches_to_last_column_without_wrapping() {
        // wrap-pending状態(cursor_col == cols、次のprintで折り返す)で結合文字が来た場合、
        // 折り返しを実行せず現在行の最終セル(cols-1)へ付加する
        // (Fableレビュー: タスク#39 — 折り返しを先にやると次行の先頭に単独で置かれてしまう)。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"0123456789"); // ちょうど10文字でcols(10)埋まり、wrap-pending状態になる
        assert_eq!(t.cursor_col(), 10);
        assert_eq!(t.cursor_row(), 0);

        feed(&mut t, "\u{0301}".as_bytes());
        assert_eq!(cell(&t, 0, 9), "9\u{0301}", "combining char at wrap-pending should attach to the last column of the current row");
        assert_eq!(t.cursor_row(), 0, "combining char at wrap-pending must not trigger a wrap");
        assert_eq!(t.cursor_col(), 10, "wrap-pending state itself must be left untouched by the combining char");

        // 直後に通常文字を書くと、初めてそこで折り返しが発生する(既存のwrap挙動に影響しない)。
        feed(&mut t, b"X");
        assert_eq!(t.cursor_row(), 1);
        assert_eq!(cell(&t, 1, 0), "X");
    }

    #[test]
    fn test_combining_char_at_wrap_pending_after_wide_char_attaches_to_wide_main_cell() {
        // 全角文字がちょうど最終2セル(cols-2, cols-1)を占めてwrap-pending状態になった
        // 直後に結合文字が来た場合、全角文字のプレースホルダ(cols-1)にではなく
        // 本体セル(cols-2)へ付加する(codexレビュー: タスク#39、wrap-pendingと全角直後の
        // 2条件が重なるケースの回帰防止)。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"01234567"); // col0..7を埋める(8セル)
        feed(&mut t, "\u{3042}".as_bytes()); // "あ"(全角)がcol8-9を占め、cursor_col==10(wrap-pending)になる
        assert_eq!(t.cursor_col(), 10);
        assert_eq!(t.cursor_row(), 0);

        feed(&mut t, "\u{0301}".as_bytes());
        assert_eq!(cell(&t, 0, 8), "\u{3042}\u{0301}", "combining char should attach to the wide char's main cell, not its placeholder");
        assert_eq!(cell(&t, 0, 9), " ", "wide char placeholder cell must stay untouched");
        assert_eq!(t.cursor_row(), 0, "must not trigger a wrap");
        assert_eq!(t.cursor_col(), 10, "wrap-pending state must be left untouched");
    }

    #[test]
    fn test_combining_char_at_start_of_line_with_no_prior_char_is_ignored() {
        // 行頭で付加対象の文字が存在しない場合(RIS直後・クリア直後等)は無視する。
        // グラフェムクラスタリング(ZWJ絵文字等)は対象外(Fableレビューでスコープ外と明記)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, "\u{0301}".as_bytes());
        assert_eq!(cell(&t, 0, 0), " ", "no base char to attach to: cell must remain blank");
        assert_eq!(t.cursor_col(), 0, "no cell was written, cursor must not move");
    }

    #[test]
    fn test_combining_char_bytes_are_capped_per_cell() {
        // `a` + U+0301(2バイト)を大量に送っても、1セルの`ch`は
        // `MAX_COMBINING_BYTES_PER_CELL`を超えて際限なく成長しない(タスク#78)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"a");
        // 十分大きいN(1000個)を送り、上限超過分が黙って捨てられることを確認する。
        for _ in 0..1000 {
            feed(&mut t, "\u{0301}".as_bytes());
        }
        let ch = &t.screen_cells()[0].ch;
        assert!(
            ch.len() <= MAX_COMBINING_BYTES_PER_CELL,
            "combining chars must be capped at MAX_COMBINING_BYTES_PER_CELL bytes, got {} bytes",
            ch.len()
        );
        // "a"(1バイト)+ U+0301(2バイト)ずつ、上限(128バイト)を超えない最大個数
        // (63個 = 126バイト)まで実際に追加され、それ以降は捨てられたことを厳密に検証する。
        assert_eq!(ch.len(), 1 + 63 * 2, "expected exactly 63 combining chars to fit before the cap kicks in");
        assert!(ch.starts_with('a'), "base char must be preserved");
        // カーソルは結合文字によって一切進まない(上限超過後も既存の仕様通り)。
        assert_eq!(t.cursor_col(), 1);
    }

    // ── REP(`CSI Ps b`、直前文字繰り返し、タスク#48) ─────────────

    #[test]
    fn test_rep_repeats_last_printed_char_with_explicit_count() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"A\x1b[3b"); // "A" の後にREPで3回追加繰り返し
        assert_eq!(cell(&t, 0, 0), "A");
        assert_eq!(cell(&t, 0, 1), "A");
        assert_eq!(cell(&t, 0, 2), "A");
        assert_eq!(cell(&t, 0, 3), "A");
        assert_eq!(cell(&t, 0, 4), " ", "only 1(original) + 3(REP) = 4 cells should be filled");
        assert_eq!(t.cursor_col(), 4);
    }

    #[test]
    fn test_rep_default_count_is_one_when_param_omitted() {
        // `CSI b`(パラメータ省略)は`CSI 1b`と同義(他のCSIパラメータの既定値と同じ扱い)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"A\x1b[b");
        assert_eq!(cell(&t, 0, 0), "A");
        assert_eq!(cell(&t, 0, 1), "A");
        assert_eq!(cell(&t, 0, 2), " ");
        assert_eq!(t.cursor_col(), 2);
    }

    #[test]
    fn test_rep_is_noop_when_no_prior_graphic_char() {
        // 画面先頭など「直前に一度もgraphic文字が書かれていない」状態でのREPはno-op
        // (タスク#48の要求事項: 「直前文字が無い」状態の扱いを決めてテストする)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[5b");
        assert_eq!(cell(&t, 0, 0), " ");
        assert_eq!(t.cursor_col(), 0, "REP with no prior char must not move the cursor or write anything");
    }

    #[test]
    fn test_rep_is_noop_immediately_after_ris_even_if_something_was_printed_before() {
        // RIS(`ESC c`)は「直前のgraphic文字」の記憶自体もリセットする(画面全体を
        // 消去する以上、繰り返す対象も存在しないと扱うのが自然、タスク#48)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"A\x1bc\x1b[3b");
        assert_eq!(cell(&t, 0, 0), " ", "RIS must clear the screen and REP must stay a no-op afterward");
    }

    #[test]
    fn test_rep_survives_intervening_newline_and_writes_at_new_cursor_position() {
        // 改行等の制御機能を挟んでも「直前のgraphic文字」の記憶はクリアしない
        // (xterm/VTE系実装の一般的挙動に合わせる、タスク#48)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"A\r\n\x1b[2b");
        assert_eq!(cell(&t, 0, 0), "A");
        assert_eq!(cell(&t, 1, 0), "A", "REP after a newline should still repeat the last printed char");
        assert_eq!(cell(&t, 1, 1), "A");
        assert_eq!(t.cursor_row(), 1);
        assert_eq!(t.cursor_col(), 2);
    }

    #[test]
    fn test_rep_uses_the_attrs_the_char_was_originally_drawn_with_not_current_attrs() {
        // REPは「直前に描画した文字・属性」を繰り返す(タスク#48要件)——文字を最初に
        // 書いた時点のSGR属性を凍結して使い、その後SGRが変わっていても(REPを実行する
        // 時点で偶然有効な)現在の属性には影響されない。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[31mA\x1b[0m\x1b[2b"); // 赤字で"A"、その後SGRリセットしてからREP
        let theme = Theme::default();
        assert_eq!(t.screen_cells()[0].fg, theme.ansi16[1], "the originally-printed 'A' keeps its red color");
        assert_eq!(t.screen_cells()[1].fg, theme.ansi16[1], "REP-written copies must reuse the attrs frozen at print time (red), not the now-reset current attrs");
        assert_eq!(t.screen_cells()[2].fg, theme.ansi16[1]);
        assert_eq!(cell(&t, 0, 1), "A");
        assert_eq!(cell(&t, 0, 2), "A");

        // REP自身はカーソル位置の現在SGR状態を変更しない(副作用のない操作) —
        // REP直後に書いた通常文字は、REP前にリセットされていた属性(赤ではない)を使う。
        feed(&mut t, b"B");
        assert_eq!(t.screen_cells()[3].fg, theme.default_fg, "SGR state after REP must be whatever was current before REP ran, unaffected by the frozen repeat attrs");
        assert_eq!(cell(&t, 0, 3), "B");
    }

    #[test]
    fn test_rep_after_combining_char_repeats_the_base_char_not_the_combining_mark() {
        // 幅0の結合文字は`last_graphic_cell`を更新しない(`print()`の幅0分岐は別経路)。
        // "e" + COMBINING ACUTE ACCENT の後のREPは、結合済みの「é」ではなく
        // 素の"e"を繰り返す(タスク#48)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, "e\u{0301}".as_bytes());
        feed(&mut t, b"\x1b[b");
        assert_eq!(cell(&t, 0, 0), "e\u{0301}", "original combined cell is untouched");
        assert_eq!(cell(&t, 0, 1), "e", "REP repeats the plain base char, not the combined grapheme");
    }

    #[test]
    fn test_rep_repeats_wide_char_occupying_two_cells_per_repetition() {
        // 全角文字のREPは、通常の`print_mapped()`(タスク#84で`print()`から分離)と
        // 同じ折り返し・プレースホルダロジックを再利用するため、1回の繰り返しごとに
        // 2セルずつ消費する(タスク#48)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, "\u{3042}".as_bytes()); // "あ"(全角) が col0-1 を占める
        feed(&mut t, b"\x1b[2b"); // さらに2回繰り返す -> col2-3, col4-5
        assert_eq!(cell(&t, 0, 0), "\u{3042}");
        assert_eq!(cell(&t, 0, 1), " ");
        assert_eq!(cell(&t, 0, 2), "\u{3042}");
        assert_eq!(cell(&t, 0, 3), " ");
        assert_eq!(cell(&t, 0, 4), "\u{3042}");
        assert_eq!(cell(&t, 0, 5), " ");
        assert_eq!(t.cursor_col(), 6);
    }

    #[test]
    fn test_rep_wraps_to_next_line_when_repeating_past_right_edge() {
        // REPは`print_mapped()`(タスク#84で`print()`から分離)をそのまま再利用する
        // ので、右端に到達すれば通常の折り返し(autowrap on)がそのまま働く(タスク#48)。
        let mut t = Terminal::new(5, 3, Theme::default());
        feed(&mut t, b"ABCD"); // col0-3を埋める(col4が残り1マス)
        feed(&mut t, b"\x1b[3b"); // "D"をさらに3回 -> col4(行0)、col0-1(行1、折り返し後)
        assert_eq!(cell(&t, 0, 4), "D");
        assert_eq!(t.cursor_row(), 1);
        assert_eq!(cell(&t, 1, 0), "D");
        assert_eq!(cell(&t, 1, 1), "D");
        assert_eq!(t.cursor_col(), 2);
    }

    #[test]
    fn test_rep_repeats_the_char_originally_printed_even_after_charset_switch() {
        // タスク#84: `last_graphic_cell`は写像**後**の文字を保存する。REPが
        // `print()`(charsetを現在値で再適用する経路)を再利用すると、記録後に
        // charsetが切り替わっていた場合に別の文字が再写像されて繰り返されてしまう。
        // 再現: ASCII下で'q'を印字 → `ESC(0`でG0をDEC Special Graphicsへ切替
        // (この文字集合では'q'は罫線文字'─'へ写像される) → REP。
        // 正しい挙動は、charset切り替え前に実際に画面へ書かれた'q'を繰り返すこと
        // であり、切り替え後のcharsetで'q'を再写像した'─'ではない。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"q\x1b(0\x1b[b");
        assert_eq!(cell(&t, 0, 0), "q", "original 'q' printed under ASCII charset is untouched");
        assert_eq!(
            cell(&t, 0, 1),
            "q",
            "REP must repeat the char as it was actually drawn ('q'), not re-map it through the charset now active"
        );
    }

    #[test]
    fn test_alt_screen_roundtrip_preserves_sgr_attributes() {
        // vim起動→終了のような alt screen 往復で、SGR属性(新規属性含む)が
        // main側のカーソル状態として保存/復元されることを確認する
        // (Fableレビュー: saved_cursor タプルへの属性追加漏れの回帰防止)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[1;4;3;9;5;7mmain"); // bold+underline+italic+strike+blink+reverse
        feed(&mut t, b"\x1b[?1049h"); // alt へ(カーソル保存、alt側は属性リセットされる)
        assert!(!t.screen_cells()[0].bold, "alt screen should start with reset attributes");
        feed(&mut t, b"\x1b[?1049l"); // main へ復帰(保存した属性が復元される)
        feed(&mut t, b"X"); // 復元された属性で1文字書く
        let c = &t.screen_cells()[4]; // "main" の後ろ
        assert!(c.bold, "restored cursor attrs after alt roundtrip should keep bold");
        assert!(c.underline, "restored cursor attrs after alt roundtrip should keep underline");
        assert!(c.italic, "restored cursor attrs after alt roundtrip should keep italic");
        assert!(c.strikethrough, "restored cursor attrs after alt roundtrip should keep strikethrough");
        assert!(c.blink, "restored cursor attrs after alt roundtrip should keep blink");
        // reverse で fg=default_bg / bg=default_fg に実効色解決されているはず
        assert_eq!(c.fg, Theme::default().default_bg);
        assert_eq!(c.bg, Theme::default().default_fg);
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

    // ── OSC 8 ハイパーリンク(タスク#40) ────────────────────

    #[test]
    fn test_osc8_open_link_attaches_link_id_to_printed_cells() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]8;;https://example.com\x07hi");
        assert_eq!(t.screen_cells()[0].link_id, Some(0));
        assert_eq!(t.screen_cells()[1].link_id, Some(0));
        assert_eq!(t.link_table(), &["https://example.com".to_string()]);
    }

    #[test]
    fn test_osc8_text_before_link_and_after_close_has_no_link_id() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"a");
        feed(&mut t, b"\x1b]8;;https://example.com\x07b");
        feed(&mut t, b"\x1b]8;;\x07c"); // 空URIで閉じる
        assert_eq!(t.screen_cells()[0].link_id, None, "リンク開始前のセルはリンク無し");
        assert_eq!(t.screen_cells()[1].link_id, Some(0), "リンク中のセルはリンクid付き");
        assert_eq!(t.screen_cells()[2].link_id, None, "リンクを閉じた後のセルはリンク無し");
    }

    #[test]
    fn test_osc8_same_url_is_interned_to_the_same_id() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]8;;https://example.com\x07a\x1b]8;;\x07");
        feed(&mut t, b"\x1b]8;;https://example.com\x07b\x1b]8;;\x07");
        assert_eq!(t.screen_cells()[0].link_id, t.screen_cells()[1].link_id);
        assert_eq!(t.link_table().len(), 1, "同一URLは重複してinternされない");
    }

    #[test]
    fn test_osc8_different_urls_get_different_ids() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]8;;https://a.example\x07a\x1b]8;;\x07");
        feed(&mut t, b"\x1b]8;;https://b.example\x07b\x1b]8;;\x07");
        assert_ne!(t.screen_cells()[0].link_id, t.screen_cells()[1].link_id);
        assert_eq!(t.link_table(), &["https://a.example".to_string(), "https://b.example".to_string()]);
    }

    #[test]
    fn test_osc8_id_param_is_rejoined_with_semicolon_from_uri_only() {
        // vteはOSCペイロードを`;`ごとにparamsへ分割して渡す——URIはparams[2]単独では
        // なくparams[2..]を`;`で再結合しなければならない(Fableレビュー2次指摘)。
        // このテストではURI自体に`;`を含むケースを再現する。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]8;;https://example.com/?a=1;b=2\x07x");
        assert_eq!(t.link_table(), &["https://example.com/?a=1;b=2".to_string()]);
    }

    #[test]
    fn test_osc8_id_param_itself_is_not_carried_into_the_url() {
        // `id=`パラメータ(params[1])は行折り返しをまたぐ同一リンク同定用だが、この
        // 実装ではURL自体のみをintern対象にする——id=部分がURLへ混入しないことを
        // 確認する。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]8;id=42;https://example.com\x07x");
        assert_eq!(t.link_table(), &["https://example.com".to_string()]);
    }

    #[test]
    fn test_osc8_malformed_sequence_without_params_is_a_noop() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]8\x07x"); // "8"だけでid/URIパートが無い
        assert_eq!(t.screen_cells()[0].link_id, None);
        assert!(t.link_table().is_empty());
    }

    #[test]
    fn test_osc8_active_link_cleared_by_ris() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]8;;https://example.com\x07");
        feed(&mut t, b"\x1bc"); // RIS(`ESC c`、フルリセット)
        feed(&mut t, b"x");
        assert_eq!(t.screen_cells()[0].link_id, None, "RIS後はアクティブリンクがクリアされている");
    }

    #[test]
    fn test_osc8_link_table_survives_ris_so_scrollback_ids_stay_valid() {
        // RISはアクティブリンク状態だけをクリアし、intern表自体はクリアしない
        // ——既にscrollbackへ流れた過去セルのlink_idがこの表のindexを指し続けて
        // いるため、RIS後にindexを再利用すると過去セルが別URLを指す破損になる。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]8;;https://example.com\x07x\x1b]8;;\x07");
        assert_eq!(t.link_table(), &["https://example.com".to_string()]);
        feed(&mut t, b"\x1bc"); // RIS
        assert_eq!(t.link_table(), &["https://example.com".to_string()], "link_table自体はRISでクリアされない");
    }

    #[test]
    fn test_osc8_active_link_cleared_on_switch_to_alt_screen() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]8;;https://example.com\x07");
        feed(&mut t, b"\x1b[?1049h"); // alt画面へ切替
        feed(&mut t, b"x");
        assert_eq!(t.screen_cells()[0].link_id, None, "alt画面切替でアクティブリンクはクリアされる");
    }

    #[test]
    fn test_osc8_active_link_cleared_on_switch_back_to_main_screen() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1049h"); // alt画面へ切替
        feed(&mut t, b"\x1b]8;;https://example.com\x07");
        feed(&mut t, b"\x1b[?1049l"); // mainへ復帰
        feed(&mut t, b"x");
        assert_eq!(t.screen_cells()[0].link_id, None, "main画面復帰でもアクティブリンクはクリアされる");
    }

    #[test]
    fn test_osc8_rep_replays_the_link_id_active_at_original_print_time() {
        // REP(`CSI Ps b`、タスク#48)は「文字が実際に書かれた時点の見た目」を
        // 繰り返す設計(SGR属性と同じ扱い)。ここではその時点でアクティブだった
        // リンクを再現し、REP実行時点(既にリンクを閉じた後)のリンク状態には
        // 化けないことを確認する。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]8;;https://example.com\x07a\x1b]8;;\x07");
        feed(&mut t, b"\x1b[3b"); // REP: 直前文字'a'を3回繰り返す
        for i in 0..4 {
            assert_eq!(t.screen_cells()[i].link_id, Some(0), "col {i}: REPで複製されたセルもリンクidを引き継ぐ");
        }
    }

    #[test]
    fn test_osc8_extremely_long_url_is_truncated_by_vte_osc_buffer() {
        // vteの既定ビルド(`no_std`フィーチャ有効、`vte = "0.13"`はデフォルト
        // フィーチャ込みでこれを含む)はOSC生バイトバッファが`MAX_OSC_RAW`
        // (1024バイト)固定長の`ArrayVec`で、溢れた分は静かに捨てられる。
        // 1024バイトを大幅に超えるURLは`osc_dispatch`に丸ごとは届かず、
        // 途中で切り詰められたバイト列として渡ってくる——ここではその挙動を
        // 明示する(切り詰められても`osc_dispatch`自体は呼ばれ、パニックしない
        // ことも合わせて確認する)。
        let mut t = Terminal::new(80, 24, Theme::default());
        let long_suffix = "x".repeat(2000);
        let long_url = format!("https://example.com/{long_suffix}");
        assert!(long_url.len() > 1024, "テストの前提として2000バイト超のURLを使う");
        let mut seq = Vec::new();
        seq.extend_from_slice(b"\x1b]8;;");
        seq.extend_from_slice(long_url.as_bytes());
        seq.extend_from_slice(b"\x07x");
        feed(&mut t, &seq);
        // パニックせず、かつ実際にintern表へ入ったURL(もしあれば)は元のURLより
        // 短く切り詰められている。
        if let Some(interned) = t.link_table().first() {
            assert!(interned.len() < long_url.len(), "1024バイトのOSCバッファで切り詰められているはず");
            assert!(long_url.starts_with(interned.as_str()), "切り詰め後も先頭は元のURLのprefixのまま");
        }
    }

    #[test]
    fn test_osc8_link_table_is_capped_and_falls_back_to_no_link_past_the_cap() {
        // タスク#70: リモートが相異なるOSC8 URLを大量に流しても`link_table`は
        // 無制限には増えない。上限到達後の新規URLはインターンされず、
        // アクティブリンクは「リンク無し」にフォールバックする。
        let mut t = Terminal::new(80, 24, Theme::default());
        // 上限ちょうどまでは各URLが順番にインターンされる。
        for i in 0..MAX_LINK_TABLE {
            feed(&mut t, format!("\x1b]8;;https://example.com/{i}\x07").as_bytes());
        }
        assert_eq!(t.link_table().len(), MAX_LINK_TABLE, "上限件数まではすべてインターンされる");

        // 上限を超えて新規URLを大量に流しても表はこれ以上増えない。
        for i in 0..1000 {
            feed(&mut t, format!("\x1b]8;;https://overflow.example/{i}\x07").as_bytes());
        }
        assert_eq!(
            t.link_table().len(),
            MAX_LINK_TABLE,
            "上限到達後は新規URLをインターンせず表のサイズは頭打ちになる"
        );

        // 上限到達後の新規URLで書いたセルはリンク無し(`link_id: None`)にフォール
        // バックする——既存idの再利用や誤った既存リンクへの化けは起きない。
        feed(&mut t, b"x");
        assert_eq!(
            t.screen_cells()[0].link_id, None,
            "上限超過後の新規URLはリンク無しにフォールバックする"
        );

        // 上限到達後でも既にインターン済みのURLは引き続き同じidを再利用できる
        // (重複排除ロジック自体は上限到達後も生きている)。
        feed(&mut t, b"\x1b]8;;https://example.com/0\x07y");
        assert_eq!(
            t.screen_cells()[1].link_id,
            Some(0),
            "上限到達後も既存URLの再利用(重複排除)は引き続き機能する"
        );
    }

    #[test]
    fn test_osc8_url_with_bidi_rlo_is_rejected_and_falls_back_to_no_link() {
        // タスク#77: U+202E(RIGHT-TO-LEFT OVERRIDE)を含むURLは、Android/iOSの
        // OSC8確認ダイアログに表示される文字列を偽装できてしまう(RTLO spoofing、
        // 例: 拡張子を先頭にひっくり返して見せる)ため、intern表に登録せずリンク
        // 無しへフォールバックしなければならない。
        let mut t = Terminal::new(80, 24, Theme::default());
        let malicious = "https://evil.example/\u{202e}gpj.exe";
        feed(&mut t, format!("\x1b]8;;{malicious}\x07x").as_bytes());
        assert_eq!(
            t.screen_cells()[0].link_id, None,
            "bidi override文字を含むURLはリンク無しにフォールバックする"
        );
        assert!(
            t.link_table().is_empty(),
            "bidi override文字を含むURLはintern表に登録されない"
        );
    }

    #[test]
    fn test_osc8_url_with_bidi_isolate_is_rejected() {
        // タスク#77: LRE/RLE/PDF/LRO/RLOだけでなく、bidi isolate(U+2066..=U+2069)
        // も同じ理由で拒否対象。
        let mut t = Terminal::new(80, 24, Theme::default());
        let malicious = "https://evil.example/\u{2066}x\u{2069}";
        feed(&mut t, format!("\x1b]8;;{malicious}\x07x").as_bytes());
        assert_eq!(t.screen_cells()[0].link_id, None);
        assert!(t.link_table().is_empty());
    }

    #[test]
    fn test_osc8_url_with_c1_control_char_is_rejected() {
        // タスク#77: C1制御文字(例: U+0085 NEL)はUTF-8では2バイトのマルチバイト
        // シーケンスとしてvteのOSC文字列パーサをそのまま通過する(C0と違い静かに
        // 捨てられない)ため、Rust側でのフィルタが必要な実際の到達経路になる。
        let mut t = Terminal::new(80, 24, Theme::default());
        let malicious = "https://example.com/\u{0085}evil";
        feed(&mut t, format!("\x1b]8;;{malicious}\x07x").as_bytes());
        assert_eq!(t.screen_cells()[0].link_id, None);
        assert!(t.link_table().is_empty());
    }

    #[test]
    fn test_osc8_url_with_del_char_is_rejected() {
        // タスク#77: DEL(0x7F)はvteのOSC文字列パーサが無視するC0範囲
        // (0x00-0x06/0x08-0x17/0x19/0x1C-0x1F)に含まれず素通りするため、
        // 実際に`osc_dispatch`まで到達しうる制御文字。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]8;;https://example.com/\x7fevil\x07x");
        assert_eq!(t.screen_cells()[0].link_id, None);
        assert!(t.link_table().is_empty());
    }

    #[test]
    fn test_osc8_url_without_disallowed_chars_is_unaffected_by_task77_filter() {
        // タスク#77のフィルタが正当なURLまで巻き込んでいないことの回帰確認。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]8;;https://example.com/normal-path?x=1\x07x");
        assert_eq!(t.screen_cells()[0].link_id, Some(0));
        assert_eq!(t.link_table(), &["https://example.com/normal-path?x=1".to_string()]);
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

    // ── DA/DSR/CPR応答(タスク#38) ────────────────────────

    #[test]
    fn test_primary_da_queues_response() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[c"); // Primary DA
        assert_eq!(t.take_pending_terminal_responses(), vec![b"\x1b[?1;2;4c".to_vec()]);
        // Consumed once.
        assert!(t.take_pending_terminal_responses().is_empty());
    }

    #[test]
    fn test_primary_da_with_explicit_zero_param_queues_response() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[0c"); // Primary DA、明示的に Ps=0
        assert_eq!(t.take_pending_terminal_responses(), vec![b"\x1b[?1;2;4c".to_vec()]);
    }

    #[test]
    fn test_secondary_da_queues_response_distinct_from_primary() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[>c"); // Secondary DA(vteは`>`をintermediatesに入れる)
        let resp = t.take_pending_terminal_responses();
        assert_eq!(resp, vec![b"\x1b[>0;100;0c".to_vec()]);
        assert_ne!(resp, vec![b"\x1b[?1;2;4c".to_vec()], "Primary DAと取り違えていないこと");
    }

    #[test]
    fn test_dsr_5n_queues_ok_response() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[5n"); // DSR: device status report
        assert_eq!(t.take_pending_terminal_responses(), vec![b"\x1b[0n".to_vec()]);
    }

    #[test]
    fn test_dsr_6n_cpr_reports_current_cursor_position_1indexed() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[6;11H"); // カーソルを row=6, col=11 (1-indexed) へ移動
        feed(&mut t, b"\x1b[6n"); // DSR: cursor position report (CPR)
        assert_eq!(t.take_pending_terminal_responses(), vec![b"\x1b[6;11R".to_vec()]);
    }

    #[test]
    fn test_dsr_unhandled_ps_queues_nothing() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[9n"); // 未対応のDSR種別
        assert!(t.take_pending_terminal_responses().is_empty());
    }

    #[test]
    fn test_dsr_6n_cpr_clamps_to_last_column_during_delayed_wrap() {
        // 右端に書いた直後は`print()`が実際の折り返しを次のprintable文字まで遅延させる
        // ("delayed wrap")ため、この間`cursor_col`は`cols`(範囲外)になり得る。CPRは
        // 可視上の位置(最終列 = cols)を報告すべきで、`cols + 1`のような範囲外の列を
        // 返してはいけない(Codexレビュー指摘)。
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"0123456789"); // ちょうど10文字で右端に到達、delayed wrap状態に入る
        feed(&mut t, b"\x1b[6n");
        assert_eq!(t.take_pending_terminal_responses(), vec![b"\x1b[1;10R".to_vec()]);
    }

    #[test]
    fn test_primary_da_with_nonzero_ps_is_ignored() {
        // Primary DA(識別要求)は`Ps`が省略時解釈込みで0の場合のみ有効(vte自身のANSI
        // ハンドラも`next_param_or(0) == 0`を条件にしている、Codexレビュー指摘)。
        // `CSI 1c`のような非0の`Ps`には応答しない。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[1c");
        assert!(t.take_pending_terminal_responses().is_empty());
    }

    #[test]
    fn test_secondary_da_with_nonzero_ps_is_ignored() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[>1c");
        assert!(t.take_pending_terminal_responses().is_empty());
    }

    #[test]
    fn test_reset_clears_pending_terminal_responses() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[6n"); // CPR要求をpendingにする
        feed(&mut t, b"\x1bc"); // RIS (full reset)
        assert!(t.take_pending_terminal_responses().is_empty());
    }

    // ── Kitty keyboard protocol(タスク#54) ────────────────────────

    #[test]
    fn test_kitty_query_default_flags_is_zero_legacy_mode() {
        let mut t = Terminal::new(80, 24, Theme::default());
        assert_eq!(t.kitty_keyboard_flags(), 0);
        feed(&mut t, b"\x1b[?u"); // CSI ? u: query
        assert_eq!(t.take_pending_terminal_responses(), vec![b"\x1b[?0u".to_vec()]);
    }

    #[test]
    fn test_kitty_push_sets_flags_and_query_reports_them() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[>5u"); // push flags=5 (disambiguate + report alternate keys)
        assert_eq!(t.kitty_keyboard_flags(), 5);
        feed(&mut t, b"\x1b[?u");
        assert_eq!(t.take_pending_terminal_responses(), vec![b"\x1b[?5u".to_vec()]);
    }

    #[test]
    fn test_kitty_push_without_param_defaults_to_zero() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[>1u");
        feed(&mut t, b"\x1b[>u"); // 省略時0をpush
        assert_eq!(t.kitty_keyboard_flags(), 0);
    }

    #[test]
    fn test_kitty_pop_default_pops_one_entry_restoring_previous() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[>1u");
        feed(&mut t, b"\x1b[>31u");
        assert_eq!(t.kitty_keyboard_flags(), 31);
        feed(&mut t, b"\x1b[<u"); // CSI < u: 既定1エントリpop
        assert_eq!(t.kitty_keyboard_flags(), 1, "popした後は1つ前のflagsに戻る");
    }

    #[test]
    fn test_kitty_pop_explicit_count_pops_multiple() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[>1u");
        feed(&mut t, b"\x1b[>2u");
        feed(&mut t, b"\x1b[>3u");
        feed(&mut t, b"\x1b[<2u"); // 2エントリpop
        assert_eq!(t.kitty_keyboard_flags(), 1);
    }

    #[test]
    fn test_kitty_pop_more_than_stack_depth_empties_without_panic() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[>5u");
        feed(&mut t, b"\x1b[<100u"); // スタックの深さ(1)を大きく超えるpop要求
        assert_eq!(t.kitty_keyboard_flags(), 0, "空になった後は0(legacy mode)に戻る");
        // 続けてpopしてもpanicしない。
        feed(&mut t, b"\x1b[<u");
        assert_eq!(t.kitty_keyboard_flags(), 0);
    }

    #[test]
    fn test_kitty_set_mode1_default_replaces_flags() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[>31u");
        feed(&mut t, b"\x1b[=1u"); // mode省略(既定1) → 丸ごと1へ置換
        assert_eq!(t.kitty_keyboard_flags(), 1);
    }

    #[test]
    fn test_kitty_set_mode2_ors_bits_into_current_flags() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[>1u"); // flags=1
        feed(&mut t, b"\x1b[=4;2u"); // mode2: 4をOR
        assert_eq!(t.kitty_keyboard_flags(), 5);
    }

    #[test]
    fn test_kitty_set_mode3_clears_bits_from_current_flags() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[>31u"); // flags=31(全ビット)
        feed(&mut t, b"\x1b[=5;3u"); // mode3: bit0とbit2をクリア
        assert_eq!(t.kitty_keyboard_flags(), 26); // 31 & !5 == 26
    }

    #[test]
    fn test_kitty_set_on_empty_stack_creates_entry() {
        let mut t = Terminal::new(80, 24, Theme::default());
        assert_eq!(t.kitty_keyboard_flags(), 0);
        feed(&mut t, b"\x1b[=3u"); // pushせずいきなりset
        assert_eq!(t.kitty_keyboard_flags(), 3);
    }

    #[test]
    fn test_kitty_push_beyond_stack_limit_evicts_oldest_entry() {
        let mut t = Terminal::new(80, 24, Theme::default());
        // KITTY_KEYBOARD_STACK_MAX(8)を超えてpushする。
        for i in 1..=9u16 {
            feed(&mut t, format!("\x1b[>{i}u").as_bytes());
        }
        assert_eq!(t.kitty_keyboard_flags(), 9, "最新のpushが有効");
        // 8回popしても最初にpushした`1`は既に追い出されているので残らない
        // (2番目にpushした`2`が最も古い生存エントリになっているはず)。
        for _ in 0..7 {
            feed(&mut t, b"\x1b[<u");
        }
        assert_eq!(t.kitty_keyboard_flags(), 2);
    }

    #[test]
    fn test_kitty_flags_stack_independent_between_main_and_alt_screen() {
        // 仕様: "The main and alternate screens...must maintain their own,
        // independent, keyboard mode stacks"。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[>5u"); // main画面でpush
        assert_eq!(t.kitty_keyboard_flags(), 5);
        feed(&mut t, b"\x1b[?1049h"); // alt画面へ切替
        assert_eq!(t.kitty_keyboard_flags(), 0, "alt画面は独立した空スタックを持つ");
        feed(&mut t, b"\x1b[>9u"); // alt画面でpush
        assert_eq!(t.kitty_keyboard_flags(), 9);
        feed(&mut t, b"\x1b[?1049l"); // main画面へ復帰
        assert_eq!(t.kitty_keyboard_flags(), 5, "main画面のスタックはalt切替をまたいで保持される");
    }

    #[test]
    fn test_kitty_bare_csi_u_without_intermediates_still_restores_cursor() {
        // 既存のCSI u(ANSI.SYS方言、タスク#57)はintermediates無しの`u`のみを
        // 対象とする。Kitty分岐(`>`/`<`/`=`/`?`付き)を追加してもこの既存挙動を
        // 壊してはいけない。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[10;5H"); // カーソルを移動
        feed(&mut t, b"\x1b[s"); // CSI s: save cursor
        feed(&mut t, b"\x1b[1;1H"); // 別の位置へ移動
        feed(&mut t, b"\x1b[u"); // CSI u(intermediates無し): restore cursor
        assert_eq!(t.cursor_row(), 9);
        assert_eq!(t.cursor_col(), 4);
        // Kitty側の状態には一切影響しない。
        assert_eq!(t.kitty_keyboard_flags(), 0);
        assert!(t.take_pending_terminal_responses().is_empty());
    }

    #[test]
    fn test_reset_clears_kitty_keyboard_flags_stack_on_both_screens() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[>5u");
        feed(&mut t, b"\x1b[?1049h"); // alt画面へ
        feed(&mut t, b"\x1b[>9u");
        feed(&mut t, b"\x1bc"); // RIS(alt画面上でもfull resetはmain画面に戻す)
        assert_eq!(t.kitty_keyboard_flags(), 0);
        // main画面側も空になっていることを確認する(alt→main往復では検証できない
        // ため、いったんmainでpushしてから確かめる)。
        feed(&mut t, b"\x1b[>7u");
        assert_eq!(t.kitty_keyboard_flags(), 7, "RIS後は素のmain画面スタックとして機能する");
    }

    // ── OSC 10/11 default fg/bg のset/query(タスク#58) ────────────────────────

    #[test]
    fn test_osc10_query_reports_default_fg_bell_terminated() {
        let mut t = Terminal::new(80, 24, Theme::default()); // default_fg = 0xFFCCCCCC
        feed(&mut t, b"\x1b]10;?\x07"); // BEL終端でquery
        assert_eq!(t.take_pending_terminal_responses(), vec![b"\x1b]10;rgb:cccc/cccc/cccc\x07".to_vec()]);
    }

    #[test]
    fn test_osc11_query_reports_default_bg_st_terminated() {
        let mut t = Terminal::new(80, 24, Theme::default()); // default_bg = 0xFF000000
        feed(&mut t, b"\x1b]11;?\x1b\\"); // ST(ESC \\)終端でquery — 応答も同じ終端子を使うべき
        assert_eq!(t.take_pending_terminal_responses(), vec![b"\x1b]11;rgb:0000/0000/0000\x1b\\".to_vec()]);
    }

    #[test]
    fn test_osc10_query_reports_custom_session_theme() {
        let mut theme = Theme::default();
        theme.default_fg = 0xFF112233;
        let mut t = Terminal::new(80, 24, theme);
        feed(&mut t, b"\x1b]10;?\x07");
        assert_eq!(t.take_pending_terminal_responses(), vec![b"\x1b]10;rgb:1111/2222/3333\x07".to_vec()]);
    }

    #[test]
    fn test_osc10_set_rgb_form_updates_theme_for_subsequent_query() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]10;rgb:ff/00/00\x07"); // fgを赤に設定(2桁hex成分)
        feed(&mut t, b"\x1b]10;?\x07");
        assert_eq!(t.take_pending_terminal_responses(), vec![b"\x1b]10;rgb:ffff/0000/0000\x07".to_vec()]);
    }

    #[test]
    fn test_osc11_set_hash_form_updates_theme_for_subsequent_query() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]11;#112233\x07"); // `#RRGGBB`形式
        feed(&mut t, b"\x1b]11;?\x07");
        assert_eq!(t.take_pending_terminal_responses(), vec![b"\x1b]11;rgb:1111/2222/3333\x07".to_vec()]);
    }

    #[test]
    fn test_osc10_set_invalid_spec_is_ignored() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]10;not-a-color\x07");
        feed(&mut t, b"\x1b]10;?\x07");
        // 既定値のまま変わっていないこと
        assert_eq!(t.take_pending_terminal_responses(), vec![b"\x1b]10;rgb:cccc/cccc/cccc\x07".to_vec()]);
    }

    #[test]
    fn test_osc10_set_does_not_queue_a_response() {
        // set(query以外)は何も送り返さない——実端末もsetそのものには応答しない。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]10;rgb:ff/00/00\x07");
        assert!(t.take_pending_terminal_responses().is_empty());
    }

    #[test]
    fn test_osc10_set_immediately_affects_text_printed_without_intervening_sgr() {
        // codexレビュー指摘: cur_attrs.fgはSGR実行時点で既定色から具体値へ解決済みのため、
        // OSC 10 set直後にSGRリセットを挟まず印字すると、`self.theme`だけ更新しても
        // 旧既定色のまま描かれてしまう。setがまだ明示色指定を受けていないcur_attrsを
        // 新しい既定色へ追従させることを確認する。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]10;rgb:ff/00/00\x07"); // fgを赤に設定
        feed(&mut t, b"x"); // SGRリセットを挟まず印字
        assert_eq!(t.screen_cells()[0].fg, 0xFFFF0000);
    }

    #[test]
    fn test_osc10_set_does_not_override_explicitly_colored_sgr() {
        // 既にSGRで明示的に色指定済みのcur_attrsは、OSC 10 setで既定色が変わっても
        // 追従してはいけない(まだ"default"を指していない)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[32m"); // fgを緑(SGR 32)に明示指定
        feed(&mut t, b"\x1b]10;rgb:ff/00/00\x07"); // 既定fgを赤に変更
        feed(&mut t, b"x");
        assert_eq!(t.screen_cells()[0].fg, Theme::default().ansi16[2], "明示指定した緑のまま変わらないこと");
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

    // ── DECAWM(?7h/?7l)・wrap関連バグ修正(タスク#56) ─────────────

    #[test]
    fn test_decawm_default_is_on() {
        let t = Terminal::new(80, 24, Theme::default());
        assert!(t.autowrap_mode(), "DECAWM should default to on (xterm既定)");
    }

    #[test]
    fn test_decawm_7l_disables_autowrap_and_7h_reenables() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?7l");
        assert!(!t.autowrap_mode());
        feed(&mut t, b"\x1b[?7h");
        assert!(t.autowrap_mode());
    }

    #[test]
    fn test_decawm_off_overwrites_last_column_instead_of_wrapping() {
        // DECAWM off の間、右端到達後の印字は次行へ折り返さず、右端の最終列
        // (cols-1)を上書きし続ける(xterm仕様)。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"\x1b[?7l");
        feed(&mut t, b"0123456789X"); // 10文字でちょうど右端、11文字目(X)は折り返さず上書き
        assert_eq!(t.cursor_row(), 0, "must not wrap to next row when DECAWM is off");
        assert_eq!(t.cursor_col(), 9, "cursor must stay clamped to the last column");
        assert_eq!(cell(&t, 0, 9), "X", "last column should be overwritten, not wrapped");
        assert_eq!(cell(&t, 0, 0), "0", "earlier columns must be untouched");
    }

    #[test]
    fn test_decawm_on_still_wraps_normally() {
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"0123456789X");
        assert_eq!(t.cursor_row(), 1, "DECAWM on (既定) は通常通り折り返す");
        assert_eq!(t.cursor_col(), 1);
        assert_eq!(cell(&t, 1, 0), "X");
    }

    #[test]
    fn test_decawm_reset_by_ris() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?7l");
        assert!(!t.autowrap_mode());
        feed(&mut t, b"\x1bc"); // RIS
        assert!(t.autowrap_mode(), "RISで既定(on)に戻る");
    }

    #[test]
    fn test_decawm_off_wide_char_at_last_column_does_not_leave_orphaned_wide_body() {
        // タスク#85: DECAWM off の状態で右端(最終列)に全角文字を書くと、
        // placeholderを置く列が無いため「表示幅2だがplaceholderを伴わない孤立した
        // 本体セル」が残ってしまっていた。sanitize_wide_row と同じ規約で、
        // 片割れを持てなかった本体は空白へ変換されるべき。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"\x1b[?7l"); // DECAWM off
        feed(&mut t, b"012345678"); // 9文字書いて残り1列(col=9、最終列)
        assert_eq!(t.cursor_col(), 9);
        feed(&mut t, "\u{3042}".as_bytes()); // "あ"(全角)を最終列へ
        assert_eq!(t.cursor_row(), 0, "DECAWM off なので折り返さない");
        assert_eq!(t.cursor_col(), 9, "カーソルは最終列にクランプされたまま");
        assert_eq!(
            cell(&t, 0, 9),
            " ",
            "placeholderを持てない全角本体は孤立させず空白へ変換する"
        );
        assert!(
            !t.screen_cells()[9].is_wide_placeholder,
            "空白化されたセル自体はplaceholderでもない"
        );
        assert_eq!(cell(&t, 0, 0), "0", "それ以前の列は無傷");
    }

    #[test]
    fn test_wide_char_that_does_not_fit_wraps_whole_char_to_next_row() {
        // 全角文字が最終列1つしか残っていない場合、半分だけ現在行に置くのではなく
        // 丸ごと次行へ折り返す(xterm仕様、以前は本体セルだけ書かれ半分に切れていた)。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"012345678"); // 9文字書いて残り1列(col=9)
        assert_eq!(t.cursor_col(), 9);
        feed(&mut t, "\u{3042}".as_bytes()); // "あ"(全角)
        assert_eq!(cell(&t, 0, 9), " ", "last column of row 0 must stay blank, not half-written");
        assert_eq!(t.cursor_row(), 1, "wide char must wrap entirely to the next row");
        assert_eq!(cell(&t, 1, 0), "\u{3042}");
        assert_eq!(cell(&t, 1, 1), " ", "wide char placeholder on the new row");
        assert_eq!(t.cursor_col(), 2);
    }

    #[test]
    fn test_wide_char_in_one_column_terminal_does_not_waste_a_blank_row_first() {
        // cols==1 の端末では全角文字は折り返した先でも絶対に収まらない。この場合
        // 「1列しか残っていないから折り返す」判定を無条件に適用すると、行頭
        // (cursor_col==0)であっても毎回強制的に改行し、最初の行を1行無駄にして
        // しまう(Codexレビュー指摘: タスク#56)。cols>1という前提を付けて防ぐ。
        let mut t = Terminal::new(1, 3, Theme::default());
        feed(&mut t, "\u{3042}".as_bytes()); // "あ"(全角)
        assert_eq!(t.cursor_row(), 0, "must not waste row 0 by pre-wrapping when nothing fits anyway");
        assert_eq!(cell(&t, 0, 0), "\u{3042}");
    }

    #[test]
    fn test_el0_clears_current_row_even_at_wrap_pending_column() {
        // EL0(現在位置から行末まで消去)は、右端まで書いた直後の遅延折り返し
        // (delayed wrap, cursor_col == cols)状態でも現在行の最終列を消せる
        // べき(以前はs==eになりno-opだった)。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"0123456789"); // ちょうど10文字、wrap-pending(cursor_col==10)
        assert_eq!(t.cursor_col(), 10);
        feed(&mut t, b"\x1b[0K");
        assert_eq!(cell(&t, 0, 9), " ", "EL0 must clear the last column even at wrap-pending");
        assert_eq!(cell(&t, 0, 0), "0", "earlier columns on the row must be untouched");
    }

    #[test]
    fn test_el1_at_wrap_pending_does_not_spill_into_next_row() {
        // EL1(行頭からカーソルまで消去)は、wrap-pending状態(cursor_col==cols)では
        // 可視上の最終列(cols-1)までを消すべきで、次行の先頭1セルまではみ出して
        // 消してはいけない(以前のoff-by-oneバグ)。
        //
        // row2に先にセンチネル('Z')を書き込んでから、cursor_row=1・cursor_col=10
        // (wrap-pending)という状況だけをカーソル上移動('A'はcursor_colを変えない)
        // で再現する — こうしないと「row2はもともと空白」なので、off-by-oneで
        // row2 col0 が誤って消されても空白のままでテストが検出できない
        // (Codexレビュー指摘: 修正前の実装でも偶然パスしてしまっていた)。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"\x1b[3;1HZZZZZZZZZZ"); // row2(0-indexed)をZで埋め、row2でwrap-pendingに
        assert_eq!(t.cursor_row(), 2);
        assert_eq!(t.cursor_col(), 10);
        feed(&mut t, b"\x1b[1A"); // row1へ移動。'A'はcursor_colを変えないのでwrap-pendingのまま
        assert_eq!(t.cursor_row(), 1);
        assert_eq!(t.cursor_col(), 10);
        feed(&mut t, b"\x1b[1K");
        for i in 0..10 {
            assert_eq!(cell(&t, 1, i), " ", "row1 col{i} should be cleared by EL1");
        }
        // row2(次行)のセンチネルは一切触れられていないこと。
        assert_eq!(cell(&t, 2, 0), "Z", "EL1 must not spill into the next row's first cell");
    }

    #[test]
    fn test_ed1_at_wrap_pending_does_not_spill_into_next_row() {
        // ED1(画面先頭からカーソルまで消去)も同じoff-by-oneが起きうる: wrap-pending
        // 状態で次の行の先頭1セルまで誤って消してしまってはいけない。上のEL1テストと
        // 同じ理由でrow2にセンチネルを先に書いてから検証する。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"\x1b[3;1HZZZZZZZZZZ"); // row2をZで埋め、row2でwrap-pendingに
        feed(&mut t, b"\x1b[1A"); // row1へ移動、cursor_colはwrap-pending(10)のまま
        assert_eq!(t.cursor_row(), 1);
        assert_eq!(t.cursor_col(), 10);
        feed(&mut t, b"\x1b[1J");
        for i in 0..10 {
            assert_eq!(cell(&t, 0, i), " ", "row0 col{i} should be cleared by ED1");
            assert_eq!(cell(&t, 1, i), " ", "row1 col{i} should be cleared by ED1");
        }
        assert_eq!(cell(&t, 2, 0), "Z", "ED1 must not spill into the next row's first cell");
    }

    #[test]
    fn test_ed0_at_wrap_pending_clears_the_last_column_of_current_row() {
        // ED0(カーソルから画面末尾まで消去)も同じ理由でwrap-pending中は
        // 現在行の最終列から(その前の列を飛ばさず)消去を開始すべき。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"0123456789"); // wrap-pending
        feed(&mut t, b"\x1b[0J");
        assert_eq!(cell(&t, 0, 9), " ", "ED0 at wrap-pending must clear the last column too");
        assert_eq!(cell(&t, 0, 0), "0", "earlier columns untouched");
    }

    // ── DECOM(`CSI ?6h`/`CSI ?6l`、origin mode、タスク#59) ─────────────

    #[test]
    fn test_decom_default_is_off() {
        let t = Terminal::new(80, 24, Theme::default());
        assert!(!t.origin_mode(), "DECOM should default to off (xterm既定)");
    }

    #[test]
    fn test_decom_6h_6l_toggles_and_homes_cursor() {
        let mut t = Terminal::new(10, 10, Theme::default());
        feed(&mut t, b"\x1b[3;7r"); // scroll region = 行2..6(0-indexed)
        feed(&mut t, b"\x1b[5;5H"); // 適当な位置へ移動しておく
        feed(&mut t, b"\x1b[?6h");
        assert!(t.origin_mode());
        // onへの切り替え自体がカーソルをscroll region上端(行2)・列0へhomeする。
        assert_eq!((t.cursor_row(), t.cursor_col()), (2, 0));

        feed(&mut t, b"\x1b[5;5H"); // origin mode有効中に再度動かしておく(offへの遷移確認用)
        feed(&mut t, b"\x1b[?6l");
        assert!(!t.origin_mode());
        // offへの切り替えは画面全体の原点(行0)・列0へhomeする。
        assert_eq!((t.cursor_row(), t.cursor_col()), (0, 0));
    }

    #[test]
    fn test_decom_cup_is_relative_to_scroll_region_and_clamped() {
        let mut t = Terminal::new(10, 10, Theme::default());
        feed(&mut t, b"\x1b[3;7r"); // scroll region = 行2..6(0-indexed)
        feed(&mut t, b"\x1b[?6h");
        // CUP(1;1) は origin mode下ではscroll region左上、つまり画面座標(2,0)。
        feed(&mut t, b"\x1b[1;1H");
        assert_eq!((t.cursor_row(), t.cursor_col()), (2, 0));
        // region高さ(5行)を超える行を指定してもregion下端(行6)にクランプされる。
        feed(&mut t, b"\x1b[99;1H");
        assert_eq!(t.cursor_row(), 6, "must clamp to scroll_bottom, not screen bottom");
    }

    #[test]
    fn test_decom_off_cup_is_absolute_screen_coordinates() {
        let mut t = Terminal::new(10, 10, Theme::default());
        feed(&mut t, b"\x1b[3;7r"); // scroll region = 行2..6(0-indexed)
        // origin mode off(既定)のままなら、scroll regionが設定されていてもCUPは
        // 画面全体の絶対座標のまま。
        feed(&mut t, b"\x1b[1;1H");
        assert_eq!((t.cursor_row(), t.cursor_col()), (0, 0));
    }

    #[test]
    fn test_decom_vpa_is_relative_to_scroll_region() {
        let mut t = Terminal::new(10, 10, Theme::default());
        feed(&mut t, b"\x1b[3;7r"); // scroll region = 行2..6(0-indexed)
        feed(&mut t, b"\x1b[?6h");
        feed(&mut t, b"\x1b[3d"); // VPA(3) → region上端(行2)から2行分オフセット = 行4
        assert_eq!(t.cursor_row(), 4);
    }

    #[test]
    fn test_decom_cuu_cud_confined_to_scroll_region() {
        let mut t = Terminal::new(10, 10, Theme::default());
        feed(&mut t, b"\x1b[3;7r"); // scroll region = 行2..6(0-indexed)
        feed(&mut t, b"\x1b[?6h"); // カーソルは行2へhome
        feed(&mut t, b"\x1b[99A"); // 大きくCUUしてもregion上端(行2)より上へは出ない
        assert_eq!(t.cursor_row(), 2);
        feed(&mut t, b"\x1b[99B"); // 大きくCUDしてもregion下端(行6)より下へは出ない
        assert_eq!(t.cursor_row(), 6);
    }

    #[test]
    fn test_decom_off_cuu_cud_still_confined_to_full_screen() {
        // origin mode offの間は、scroll regionが設定されていてもCUU/CUDは
        // 画面全体(0..rows-1)が可動範囲のまま(既存の挙動を壊さない)。
        let mut t = Terminal::new(10, 10, Theme::default());
        feed(&mut t, b"\x1b[3;7r"); // scroll region = 行2..6(0-indexed)
        feed(&mut t, b"\x1b[1;1H");
        feed(&mut t, b"\x1b[99A");
        assert_eq!(t.cursor_row(), 0);
        feed(&mut t, b"\x1b[99B");
        assert_eq!(t.cursor_row(), 9, "画面最下行(9)まで動けるべき");
    }

    #[test]
    fn test_decom_cpr_reports_scroll_region_relative_row() {
        let mut t = Terminal::new(10, 10, Theme::default());
        feed(&mut t, b"\x1b[3;7r"); // scroll region = 行2..6(0-indexed)
        feed(&mut t, b"\x1b[?6h"); // カーソルは行2(region上端)へhome
        t.take_pending_terminal_responses();
        feed(&mut t, b"\x1b[6n");
        let resp = t.take_pending_terminal_responses();
        assert_eq!(
            resp,
            vec![b"\x1b[1;1R".to_vec()],
            "origin mode下ではCPRの行番号もregion上端基準の相対値になる"
        );
    }

    #[test]
    fn test_decstbm_homes_cursor_to_screen_origin_when_decom_off() {
        // codexレビュー指摘: DECSTBM(`CSI r`)はscroll region変更時、実端末同様
        // カーソルをhome位置へ移動する。DECOM offの間のhomeは画面左上(0,0)。
        let mut t = Terminal::new(10, 10, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4"); // カーソルを行4付近へ動かしておく
        feed(&mut t, b"\x1b[3;7r"); // scroll region = 行2..6(0-indexed)
        assert_eq!((t.cursor_row(), t.cursor_col()), (0, 0));
    }

    #[test]
    fn test_decstbm_homes_cursor_to_scroll_top_when_decom_on() {
        // 同上、DECOM onの間のhomeは新しいscroll_top行(列は常に0)。
        let mut t = Terminal::new(10, 10, Theme::default());
        feed(&mut t, b"\x1b[?6h"); // origin mode on
        feed(&mut t, b"\x1b[8;9H"); // カーソルを適当な位置へ動かしておく
        feed(&mut t, b"\x1b[3;7r"); // scroll region = 行2..6(0-indexed)
        assert_eq!(
            (t.cursor_row(), t.cursor_col()),
            (2, 0),
            "origin mode onの間、DECSTBM後のhomeは新しいscroll_top(行2)"
        );
    }

    #[test]
    fn test_decstbm_no_params_resets_to_full_screen_and_homes_cursor() {
        // タスク#64(Fableレビュー指摘): パラメータ省略の`CSI r`(p0==p1==0)は
        // 画面全体(top=0, bot=rows-1)へscroll regionをリセットしなければ
        // ならない。旧実装は`p0.max(1)`/`p1.max(1)`で両方1にフォールバック
        // していたため top=bot=0 になり `top < bot` が偽になって何もしない
        // (region維持・カーソルもhomeしない)バグになっていた
        // (vim/less終了直後にスクロール異常が残る原因)。
        let mut t = Terminal::new(10, 6, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");
        feed(&mut t, b"\x1b[3;5r"); // scroll region = 行2..4(0-indexed)に絞る
        feed(&mut t, b"\x1b[4;4H"); // regionの内側の適当な位置へカーソルを動かす
        feed(&mut t, b"\x1b[r"); // パラメータ省略のDECSTBM
        assert_eq!(
            (t.cursor_row(), t.cursor_col()),
            (0, 0),
            "パラメータ省略のCSI rも画面全体へのリセットとしてカーソルをhomeへ移動しなければならない"
        );
        // regionが画面全体(0..rows-1)へ戻ったことを、SDが画面全域に効くかで確認する。
        feed(&mut t, b"\x1b[1T"); // 1行下スクロール。regionが画面全体なら行0が空行になる
        assert_eq!(cell(&t, 0, 0), " ", "regionが画面全体に戻っていればSDで行0が空行になる");
        assert_eq!(cell(&t, 1, 0), "r", "旧行0がregion内(画面全体)で1行下へ押し出される");
        assert_eq!(cell(&t, 5, 0), "r", "旧行4がregion内(画面全体)で1行下へ押し出される");
    }

    #[test]
    fn test_decstbm_omitted_bottom_defaults_to_last_row() {
        // タスク#64: `CSI Ps r`(下端パラメータのみ省略、p1==0)はxterm等の実端末
        // 同様、下端を画面最下行として扱う(`p0.max(1)`のロジックのままだと
        // bot=0になり top<bot が常に偽になってしまう)。
        let mut t = Terminal::new(10, 6, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");
        feed(&mut t, b"\x1b[3r"); // 上端のみ行2(0-indexed)指定、下端は省略
        feed(&mut t, b"\x1b[1T"); // regionを1行下へスクロール
        assert_eq!(cell(&t, 0, 0), "r", "region上端より上の行0はSDの影響を受けない");
        assert_eq!(cell(&t, 1, 0), "r", "scroll_top未満の行1(region外)はSDの影響を受けない");
        assert_eq!(cell(&t, 2, 0), " ", "region上端(scroll_top=2)は空行で埋まる");
        assert_eq!(cell(&t, 5, 3), "4", "下端省略時は画面最下行までがregionになる");
    }

    #[test]
    fn test_decom_reset_by_ris() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?6h");
        assert!(t.origin_mode());
        feed(&mut t, b"\x1bc"); // RIS
        assert!(!t.origin_mode(), "RISで既定(off)に戻る");
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
    fn test_resize_preserving_state_shrinking_cols_sanitizes_clipped_wide_pair() {
        // タスク#85: 列数を縮小すると `resize_grid` の単純な `Vec::resize` による
        // 末尾切り捨てが、行末にあった全角(wide)文字ペアの右半分(placeholder)
        // だけを切り落とし、本体(表示幅2)がplaceholder無しで孤立して残ることが
        // あった。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, "1234\u{3042}".as_bytes()); // 列4-5に全角"あ"(本体+placeholder)
        assert_eq!(cell(&t, 0, 4), "\u{3042}");
        assert!(t.screen_cells()[5].is_wide_placeholder);
        // 新しい幅を5列にし、ちょうど全角ペアの本体だけが残る(placeholderの列が
        // 切り落とされる)ようにする。
        t.resize_preserving_state(5, 3);
        assert_eq!(t.screen_cells().len(), 5 * 3);
        assert_eq!(
            cell(&t, 0, 4),
            " ",
            "placeholderを失った全角本体は孤立させず空白へ変換する"
        );
        assert_eq!(cell(&t, 0, 0), "1", "それ以前の列は無傷");
    }

    #[test]
    fn test_resize_preserving_state_rows_only_resize_does_not_blank_one_column_wide_body() {
        // タスク#85修正のcodexレビュー指摘: `resize_grid`の全角ペア後始末を無条件に
        // 呼ぶと、列数を全く縮めていない(行数のみ変更する)resizeでも毎回発火し、
        // `cols == 1`端末が意図的に保持しているplaceholder無しの幅2本体セル
        // (タスク#56)まで空白化してしまう回帰があった。列を縮小しない限り
        // sanitizeは発火してはならない。
        let mut t = Terminal::new(1, 3, Theme::default());
        feed(&mut t, "\u{3042}".as_bytes()); // "あ"(全角) — cols==1なのでplaceholder無し
        assert_eq!(cell(&t, 0, 0), "\u{3042}");
        t.resize_preserving_state(1, 5); // 列数は変えず行数だけ増やす
        assert_eq!(
            cell(&t, 0, 0),
            "\u{3042}",
            "列数を縮めていないresizeで既存の全角本体を空白化してはいけない"
        );
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
    fn test_dectcem_hides_and_shows_cursor() {
        let mut t = Terminal::new(80, 24, Theme::default());
        assert!(t.cursor_visible(), "既定はカーソル表示");
        feed(&mut t, b"\x1b[?25l"); // DECTCEM: カーソル非表示
        assert!(!t.cursor_visible());
        feed(&mut t, b"\x1b[?25h"); // DECTCEM: カーソル表示
        assert!(t.cursor_visible());
    }

    #[test]
    fn test_deckpam_deckpnm_toggle_application_keypad_mode() {
        // タスク#43: DECKPAM(`ESC =`)/DECKPNM(`ESC >`)。
        let mut t = Terminal::new(80, 24, Theme::default());
        assert!(!t.application_keypad_mode(), "既定はnumeric keypad mode");
        feed(&mut t, b"\x1b="); // DECKPAM
        assert!(t.application_keypad_mode());
        feed(&mut t, b"\x1b>"); // DECKPNM
        assert!(!t.application_keypad_mode());
    }

    #[test]
    fn test_ris_resets_application_keypad_mode() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b=");
        assert!(t.application_keypad_mode());
        feed(&mut t, b"\x1bc"); // RIS
        assert!(!t.application_keypad_mode());
    }

    #[test]
    fn test_synchronized_output_mode_2026_toggle() {
        let mut t = Terminal::new(80, 24, Theme::default());
        assert!(!t.synchronized_output_active(), "既定はoff");
        feed(&mut t, b"\x1b[?2026h");
        assert!(t.synchronized_output_active());
        feed(&mut t, b"\x1b[?2026l");
        assert!(!t.synchronized_output_active());
    }

    #[test]
    fn test_ris_resets_synchronized_output_mode() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?2026h");
        assert!(t.synchronized_output_active());
        feed(&mut t, b"\x1bc"); // RIS
        assert!(!t.synchronized_output_active());
    }

    #[test]
    fn test_force_end_synchronized_output_clears_active_flag() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?2026h");
        assert!(t.synchronized_output_active());
        t.force_end_synchronized_output();
        assert!(!t.synchronized_output_active());
    }

    #[test]
    fn test_dectcem_reset_by_ris() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?25l");
        assert!(!t.cursor_visible());
        feed(&mut t, b"\x1bc"); // RIS (full reset)
        assert!(t.cursor_visible(), "RISで既定の表示状態に戻る");
    }

    #[test]
    fn test_decscusr_default_shape() {
        let t = Terminal::new(80, 24, Theme::default());
        assert_eq!(t.cursor_shape(), CursorShape::Block, "既定はblock");
        assert!(t.cursor_blink(), "既定は点滅");
    }

    #[test]
    fn test_decscusr_all_params() {
        let cases: &[(u16, CursorShape, bool)] = &[
            (0, CursorShape::Block, true),
            (1, CursorShape::Block, true),
            (2, CursorShape::Block, false),
            (3, CursorShape::Underline, true),
            (4, CursorShape::Underline, false),
            (5, CursorShape::Bar, true),
            (6, CursorShape::Bar, false),
        ];
        for &(ps, shape, blink) in cases {
            let mut t = Terminal::new(80, 24, Theme::default());
            feed(&mut t, format!("\x1b[{} q", ps).as_bytes());
            assert_eq!(t.cursor_shape(), shape, "Ps={ps}");
            assert_eq!(t.cursor_blink(), blink, "Ps={ps}");
        }
    }

    #[test]
    fn test_decscusr_unknown_param_ignored() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[5 q"); // bar, blinking
        assert_eq!(t.cursor_shape(), CursorShape::Bar);
        feed(&mut t, b"\x1b[99 q"); // 未知のパラメータ: 直前の状態を維持
        assert_eq!(t.cursor_shape(), CursorShape::Bar, "未知パラメータは無視される");
        assert!(t.cursor_blink());
    }

    /// Fableレビュー(タスク#32・2次)で指摘された罠: 中間バイト無しの `CSI Ps q`
    /// (DECLL、未実装)を DECSCUSR として誤処理してはいけない。csi_dispatch は
    /// intermediates == [b' '] の場合のみ DECSCUSR として扱うことを保証する。
    #[test]
    fn test_csi_q_without_intermediate_is_not_decscusr() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[5 q"); // まず bar/blinking にしておく
        assert_eq!(t.cursor_shape(), CursorShape::Bar);
        feed(&mut t, b"\x1b[2q"); // 中間バイト無し = DECLL(未実装、no-op)であるべき
        assert_eq!(t.cursor_shape(), CursorShape::Bar, "DECLLはカーソル形状を変えてはいけない");
        assert!(t.cursor_blink());
    }

    #[test]
    fn test_decscusr_reset_by_ris() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[4 q"); // steady underline
        assert_eq!(t.cursor_shape(), CursorShape::Underline);
        assert!(!t.cursor_blink());
        feed(&mut t, b"\x1bc"); // RIS (full reset)
        assert_eq!(t.cursor_shape(), CursorShape::Block, "RISで既定のblockに戻る");
        assert!(t.cursor_blink(), "RISで既定の点滅状態に戻る");
    }

    #[test]
    fn test_decset_12_toggles_cursor_blink_independent_of_shape() {
        let mut t = Terminal::new(80, 24, Theme::default());
        assert!(t.cursor_blink(), "既定は点滅");
        feed(&mut t, b"\x1b[?12l"); // 点滅off
        assert!(!t.cursor_blink());
        assert_eq!(t.cursor_shape(), CursorShape::Block, "?12は形状を変えない");
        feed(&mut t, b"\x1b[?12h"); // 点滅on
        assert!(t.cursor_blink());
        assert_eq!(t.cursor_shape(), CursorShape::Block, "?12は形状を変えない");

        // DECSCUSRで形状+点滅を設定した後でも、?12単体で点滅状態だけ上書きできる。
        feed(&mut t, b"\x1b[3 q"); // Ps=3: blinking underline
        assert_eq!(t.cursor_shape(), CursorShape::Underline);
        assert!(t.cursor_blink());
        feed(&mut t, b"\x1b[?12l"); // 点滅off。形状(Underline)は維持される。
        assert!(!t.cursor_blink());
        assert_eq!(t.cursor_shape(), CursorShape::Underline, "?12はDECSCUSRの形状を変えない");
    }

    #[test]
    fn test_decset_12_reset_by_ris() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?12l");
        assert!(!t.cursor_blink());
        feed(&mut t, b"\x1bc"); // RIS
        assert!(t.cursor_blink(), "RISで既定の点滅状態に戻る");
    }

    #[test]
    fn test_bell_increments_generation() {
        let mut t = Terminal::new(80, 24, Theme::default());
        assert_eq!(t.bell_generation(), 0, "既定は0");
        feed(&mut t, b"\x07");
        assert_eq!(t.bell_generation(), 1);
    }

    #[test]
    fn test_bell_multiple_increments_each_time() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x07\x07\x07");
        assert_eq!(t.bell_generation(), 3, "BELを受信するたびに単調増加する");
    }

    #[test]
    fn test_bell_osc_terminator_does_not_count() {
        // vte は OSC のターミネータとして使われた BEL(`ESC]0;title BEL`)を
        // ターミネータとして消費し、`execute()`には渡さない仕様。よって
        // タイトル設定に伴う BEL では鳴ってはいけない(Fableレビュー: タスク#24)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]0;My Title\x07");
        assert_eq!(t.title(), Some("My Title"));
        assert_eq!(t.bell_generation(), 0, "OSC終端のBELではbell_generationは進まない");
    }

    #[test]
    fn test_bell_not_reset_by_ris() {
        // reset_all(RIS)はpending clipboard等を律儀にリセットするが、
        // bell_generationは単調増加を維持する必要があるため意図的にリセットしない
        // (Fableレビュー: タスク#24)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x07\x07");
        assert_eq!(t.bell_generation(), 2);
        feed(&mut t, b"\x1bc"); // RIS (full reset)
        assert_eq!(t.bell_generation(), 2, "RISでbell_generationはリセットされない");
        feed(&mut t, b"\x07");
        assert_eq!(t.bell_generation(), 3, "RIS後もカウントは継続する");
    }

    #[test]
    fn test_resize_preserving_state_preserves_cursor_visibility() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?25l"); // カーソル非表示にしてからリサイズ
        t.resize_preserving_state(40, 12);
        assert!(!t.cursor_visible(), "リサイズでカーソル非表示状態が消えてはいけない");
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

    // ── IL/DL(`CSI Ps L`/`CSI Ps M`、タスク#35) ─────────────

    #[test]
    fn test_il_inserts_blank_lines_and_shifts_rest_down() {
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        feed(&mut t, b"\x1b[2;1H\x1b[2L"); // カーソルを行1(0-indexed)へ、2行挿入
        assert_eq!(cell(&t, 0, 0), "r", "row0はIL対象外(カーソルより上)なので不変");
        assert_eq!(cell(&t, 1, 0), " ", "挿入された空行");
        assert_eq!(cell(&t, 2, 0), " ", "挿入された空行");
        assert_eq!(cell(&t, 3, 0), "r", "旧row1が2行下へ押し出される");
        assert_eq!(cell(&t, 3, 3), "1", "旧row1(\"row1\")の内容がそのまま");
        assert_eq!(cell(&t, 4, 3), "2", "旧row2(\"row2\")の内容がそのまま");
        // 旧row3・旧row4はscroll_bottomを超えて溢れ、破棄される。
    }

    #[test]
    fn test_dl_deletes_lines_and_shifts_rest_up() {
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        feed(&mut t, b"\x1b[2;1H\x1b[2M"); // カーソルを行1(0-indexed)へ、2行削除
        assert_eq!(cell(&t, 0, 0), "r", "row0はDL対象外(カーソルより上)なので不変");
        assert_eq!(cell(&t, 1, 3), "3", "旧row3が2行上へ詰められる");
        assert_eq!(cell(&t, 2, 3), "4", "旧row4が2行上へ詰められる");
        assert_eq!(cell(&t, 3, 0), " ", "下端は空行で埋められる");
        assert_eq!(cell(&t, 4, 0), " ", "下端は空行で埋められる");
    }

    #[test]
    fn test_il_default_count_is_one() {
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1");
        feed(&mut t, b"\x1b[1;1H\x1b[L"); // Ps省略 == CSI 1L
        assert_eq!(cell(&t, 0, 0), " ", "空行が1行だけ挿入される");
        assert_eq!(cell(&t, 1, 0), "r", "旧row0が1行だけ下へ押し出される");
    }

    #[test]
    fn test_dl_default_count_is_one() {
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1");
        feed(&mut t, b"\x1b[1;1H\x1b[M"); // Ps省略 == CSI 1M
        assert_eq!(cell(&t, 0, 3), "1", "旧row1が1行だけ上へ詰められる");
    }

    #[test]
    fn test_il_dl_noop_when_cursor_outside_scroll_region() {
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        feed(&mut t, b"\x1b[2;4r"); // scroll region = 行1..3(0-indexed)
        // カーソルをregion上端より上(行0)に置いてIL/DLを試みる → no-op。
        feed(&mut t, b"\x1b[1;1H\x1b[2L");
        assert_eq!(cell(&t, 0, 0), "r", "region外のIL: 行0は不変のまま");
        assert_eq!(cell(&t, 1, 0), "r", "region外のIL: 行1も不変のまま");
        feed(&mut t, b"\x1b[1;1H\x1b[2M");
        assert_eq!(cell(&t, 0, 0), "r", "region外のDL: 行0は不変のまま");
        assert_eq!(cell(&t, 1, 0), "r", "region外のDL: 行1も不変のまま");
        // カーソルをregion下端より下(行4)に置いても同様にno-op。
        feed(&mut t, b"\x1b[5;1H\x1b[1L");
        assert_eq!(cell(&t, 4, 0), "r", "region外のIL: 行4は不変のまま");
    }

    #[test]
    fn test_il_dl_never_touch_pending_scrollback() {
        // Fableレビュー(2次): scroll_up_regionは`top==0 && !alt`の場合、押し出された行を
        // pending_scrollbackへpushする。IL/DLをこれ経由で安直に実装すると、カーソルが
        // 0行目にある状態でのDL/ILが削除/押し出しされた行を誤ってscrollback履歴へ
        // 混入させてしまう。IL/DLはこのバグを踏んでいないことをここで固定する
        // (カーソルが画面最上行にある――scroll_up_regionなら確実にscrollbackへ積む
        // 条件――状態で、DL/ILどちらも試す)。
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        assert!(t.take_scrollback().is_empty(), "折り返しなしの5行埋めではまだ何もスクロールアウトしていない");

        feed(&mut t, b"\x1b[1;1H\x1b[1M"); // カーソルは行0、DLで行0を削除
        assert!(
            t.take_scrollback().is_empty(),
            "DLで押し出された行はpending_scrollbackへ積んではならない"
        );

        feed(&mut t, b"\x1b[1;1H\x1b[1L"); // カーソルは行0、ILで空行を挿入(下端の行が溢れて破棄される)
        assert!(
            t.take_scrollback().is_empty(),
            "ILで画面外に溢れた行もpending_scrollbackへ積んではならない"
        );
    }

    #[test]
    fn test_il_dl_do_not_move_cursor() {
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2");
        feed(&mut t, b"\x1b[2;3H"); // 行1・列2(0-indexed)
        assert_eq!((t.cursor_row(), t.cursor_col()), (1, 2));
        feed(&mut t, b"\x1b[2L");
        assert_eq!((t.cursor_row(), t.cursor_col()), (1, 2), "ILはカーソル位置を変えない");
        feed(&mut t, b"\x1b[2M");
        assert_eq!((t.cursor_row(), t.cursor_col()), (1, 2), "DLはカーソル位置を変えない");
    }

    #[test]
    fn test_il_dl_clamp_count_beyond_region_size_without_panic() {
        // n がregionサイズを超える(=画面全体を押し出す/削除する)場合、
        // usizeアンダーフローでpanicせず、regionサイズにクランプして全行が
        // 空行になることを確認する。
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        feed(&mut t, b"\x1b[1;1H\x1b[100L");
        for row in 0..5 {
            assert_eq!(cell(&t, row, 0), " ", "row {row} should be blank after over-sized IL");
        }

        let mut t2 = Terminal::new(10, 5, Theme::default());
        feed(&mut t2, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        feed(&mut t2, b"\x1b[1;1H\x1b[100M");
        for row in 0..5 {
            assert_eq!(cell(&t2, row, 0), " ", "row {row} should be blank after over-sized DL");
        }
    }

    #[test]
    fn test_il_dl_blank_uses_current_sgr_bg() {
        // blank() は現在のSGR属性(色等)を引き継ぐ仕様(erase系と同じ) — IL/DLの
        // 空白セルもそれに倣うことを固定する(IL: 挿入された空行、DL: 下端の
        // 埋め合わせ行の両方をチェックする——codexレビュー指摘)。
        let red_bg = Theme::default().ansi16[1];

        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1");
        feed(&mut t, b"\x1b[41m"); // 赤背景
        feed(&mut t, b"\x1b[1;1H\x1b[1L");
        assert_eq!(t.screen_cells()[0].bg, red_bg, "ILで挿入された空行は現在のSGR背景色を引き継ぐ");

        let mut t2 = Terminal::new(10, 5, Theme::default());
        feed(&mut t2, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        feed(&mut t2, b"\x1b[41m"); // 赤背景
        feed(&mut t2, b"\x1b[1;1H\x1b[1M"); // 行0削除 → 下端(行4)が空行で埋まる
        assert_eq!(cell(&t2, 4, 0), " ");
        assert_eq!(t2.screen_cells()[4 * 10].bg, red_bg, "DLで下端に埋められた空行も現在のSGR背景色を引き継ぐ");
    }

    #[test]
    fn test_il_dl_confined_to_cursor_row_through_scroll_bottom_within_region() {
        // タスク要件: 「scroll regionと現在行の制約」——非全画面scroll region内で
        // IL/DLがcursor_row..scroll_bottomの範囲だけを動かし、scroll_top未満・
        // scroll_bottom超過(regionの外側)の行には一切触れないことを固定する
        // (codexレビュー指摘: no-opケースだけでなく、region内部での「効く」
        // 範囲そのものも固定すべき)。
        let mut t = Terminal::new(10, 6, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");
        feed(&mut t, b"\x1b[3;5r"); // scroll region = 行2..4(0-indexed)
        feed(&mut t, b"\x1b[4;1H\x1b[1M"); // カーソルは行3(region内)、DLで1行削除
        assert_eq!(cell(&t, 0, 0), "r", "region上端より上の行0はDLの影響を受けない");
        assert_eq!(cell(&t, 1, 0), "r", "scroll_top未満の行1(region外)はDLの影響を受けない");
        assert_eq!(cell(&t, 2, 0), "r", "region内だがカーソル行(3)より上の行2は不変");
        assert_eq!(cell(&t, 3, 3), "4", "旧row4がカーソル行(3)へ詰められる");
        assert_eq!(cell(&t, 4, 0), " ", "region下端(scroll_bottom=4)が空行で埋まる");
        assert_eq!(cell(&t, 5, 0), "r", "scroll_bottomを超えた行5(region外)はDLの影響を受けない");
    }

    #[test]
    fn test_sd_scrolls_content_down_and_blanks_top() {
        // SU(`CSI S`)の対 — SD(`CSI T`)はscroll region全体を下へn行シフトし、
        // 上端をn行分の空行で埋める。
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        feed(&mut t, b"\x1b[2T"); // 2行分下へスクロール
        assert_eq!(cell(&t, 0, 0), " ", "上端は空行で埋まる");
        assert_eq!(cell(&t, 1, 0), " ", "上端は空行で埋まる");
        assert_eq!(cell(&t, 2, 3), "0", "旧row0が2行下へ押し出される");
        assert_eq!(cell(&t, 3, 3), "1", "旧row1が2行下へ押し出される");
        assert_eq!(cell(&t, 4, 3), "2", "旧row2が2行下へ押し出される");
        // 旧row3・旧row4はscroll_bottomを超えて溢れ、破棄される。
    }

    #[test]
    fn test_sd_default_count_is_one() {
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1");
        feed(&mut t, b"\x1b[1;1H\x1b[T"); // Ps省略 == CSI 1T
        assert_eq!(cell(&t, 0, 0), " ", "空行が1行だけ挿入される");
        assert_eq!(cell(&t, 1, 0), "r", "旧row0が1行だけ下へ押し出される");
    }

    #[test]
    fn test_sd_confined_to_scroll_region() {
        let mut t = Terminal::new(10, 6, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");
        feed(&mut t, b"\x1b[3;5r"); // scroll region = 行2..4(0-indexed)
        feed(&mut t, b"\x1b[1T"); // regionを1行下へスクロール
        assert_eq!(cell(&t, 0, 0), "r", "region上端より上の行0はSDの影響を受けない");
        assert_eq!(cell(&t, 1, 0), "r", "scroll_top未満の行1(region外)はSDの影響を受けない");
        assert_eq!(cell(&t, 2, 0), " ", "region上端(scroll_top=2)は空行で埋まる");
        assert_eq!(cell(&t, 3, 3), "2", "旧row2がregion内で1行下へ押し出される");
        assert_eq!(cell(&t, 4, 3), "3", "旧row3がregion内で1行下へ押し出される");
        assert_eq!(cell(&t, 5, 0), "r", "scroll_bottomを超えた行5(region外)はSDの影響を受けない");
    }

    #[test]
    fn test_sd_clamp_count_beyond_region_size_without_panic() {
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        feed(&mut t, b"\x1b[100T");
        for row in 0..5 {
            assert_eq!(cell(&t, row, 0), " ", "row {row} should be blank after over-sized SD");
        }
    }

    #[test]
    fn test_sd_does_not_move_cursor() {
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2");
        feed(&mut t, b"\x1b[2;3H"); // 行1・列2(0-indexed)
        assert_eq!((t.cursor_row(), t.cursor_col()), (1, 2));
        feed(&mut t, b"\x1b[2T");
        assert_eq!((t.cursor_row(), t.cursor_col()), (1, 2), "SDはカーソル位置を変えない");
    }

    #[test]
    fn test_sd_never_touches_pending_scrollback() {
        // SDで下端から押し出されて消える行は、SUの押し出し行(scrollback行)とは
        // 意味が異なるため、pending_scrollbackへは一切積まれない。
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        assert!(t.take_scrollback().is_empty());
        feed(&mut t, b"\x1b[2T");
        assert!(
            t.take_scrollback().is_empty(),
            "SDで下端から押し出されて消える行はpending_scrollbackへ積んではならない"
        );
    }

    #[test]
    fn test_csi_t_multi_param_is_not_treated_as_sd() {
        // Fableレビュー(2次)指摘: xtermでは5パラメータの`CSI T`はhighlight mouse
        // tracking開始という別機能。誤ってSDとして解釈すると画面が壊れるため、
        // パラメータが2個以上ある`CSI T`はno-opのままであることを固定する。
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        feed(&mut t, b"\x1b[1;2;3;4;5T");
        assert_eq!(cell(&t, 0, 0), "r", "5パラメータのCSI TはSDとして扱われない(no-op)");
        assert_eq!(cell(&t, 0, 3), "0", "画面内容は変化しない");
    }

    #[test]
    fn test_csi_gt_t_is_not_treated_as_sd() {
        // `CSI > Ps;Ps T`(intermediateに`>`)はタイトルモードリセットで、SDとは無関係。
        // intermediates非空の`CSI T`は一律SDとして扱わない。
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        feed(&mut t, b"\x1b[>1T");
        assert_eq!(cell(&t, 0, 0), "r", "intermediate付きCSI TはSDとして扱われない(no-op)");
    }

    #[test]
    fn test_ich_inserts_blanks_and_shifts_right_within_row() {
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"abcdefg");
        feed(&mut t, b"\x1b[1;3H\x1b[2@"); // カーソルを行0・列2(0-indexed)へ、2セル挿入
        assert_eq!(cell(&t, 0, 0), "a", "カーソルより左は不変");
        assert_eq!(cell(&t, 0, 1), "b", "カーソルより左は不変");
        assert_eq!(cell(&t, 0, 2), " ", "挿入された空白");
        assert_eq!(cell(&t, 0, 3), " ", "挿入された空白");
        assert_eq!(cell(&t, 0, 4), "c", "旧列2以降が2列右へ押し出される");
        assert_eq!(cell(&t, 0, 8), "g", "行末近くまで押し出される");
        // 元々列7,8,9は空白だったので、押し出されて溢れた分は破棄されるだけで見た目には
        // 表れない(行の幅は10のまま)。
        assert_eq!((t.cursor_row(), t.cursor_col()), (0, 2), "ICHはカーソル位置を変えない");
    }

    #[test]
    fn test_dch_deletes_and_shifts_left_within_row() {
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"abcdefg");
        feed(&mut t, b"\x1b[1;3H\x1b[2P"); // カーソルを行0・列2(0-indexed)へ、2セル削除
        assert_eq!(cell(&t, 0, 0), "a", "カーソルより左は不変");
        assert_eq!(cell(&t, 0, 1), "b", "カーソルより左は不変");
        assert_eq!(cell(&t, 0, 2), "e", "旧列4('e')が2列左へ詰められる");
        assert_eq!(cell(&t, 0, 3), "f", "旧列5('f')が2列左へ詰められる");
        assert_eq!(cell(&t, 0, 4), "g", "旧列6('g')が2列左へ詰められる");
        assert_eq!(cell(&t, 0, 5), " ", "行末は空白で埋められる");
        assert_eq!((t.cursor_row(), t.cursor_col()), (0, 2), "DCHはカーソル位置を変えない");
    }

    #[test]
    fn test_ech_erases_in_place_without_shifting() {
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"abcdefg");
        feed(&mut t, b"\x1b[1;3H\x1b[2X"); // カーソルを行0・列2(0-indexed)へ、2セル消去
        assert_eq!(cell(&t, 0, 0), "a", "カーソルより左は不変");
        assert_eq!(cell(&t, 0, 1), "b", "カーソルより左は不変");
        assert_eq!(cell(&t, 0, 2), " ", "消去された");
        assert_eq!(cell(&t, 0, 3), " ", "消去された");
        assert_eq!(cell(&t, 0, 4), "e", "ECHはシフトしない — 消去範囲より右はそのまま");
        assert_eq!(cell(&t, 0, 5), "f", "ECHはシフトしない — 消去範囲より右はそのまま");
        assert_eq!((t.cursor_row(), t.cursor_col()), (0, 2), "ECHはカーソル位置を変えない");
    }

    #[test]
    fn test_ich_dch_ech_default_count_is_one() {
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"abc");
        feed(&mut t, b"\x1b[1;2H\x1b[@"); // Ps省略 == CSI 1@、列1(0-indexed)へ挿入
        assert_eq!(cell(&t, 0, 0), "a");
        assert_eq!(cell(&t, 0, 1), " ", "1セルだけ挿入される");
        assert_eq!(cell(&t, 0, 2), "b", "旧列1('b')が1列だけ右へ押し出される");

        let mut t2 = Terminal::new(10, 3, Theme::default());
        feed(&mut t2, b"abc");
        feed(&mut t2, b"\x1b[1;2H\x1b[P"); // Ps省略 == CSI 1P
        assert_eq!(cell(&t2, 0, 1), "c", "旧列2('c')が1列だけ左へ詰められる");

        let mut t3 = Terminal::new(10, 3, Theme::default());
        feed(&mut t3, b"abc");
        feed(&mut t3, b"\x1b[1;2H\x1b[X"); // Ps省略 == CSI 1X
        assert_eq!(cell(&t3, 0, 1), " ", "1セルだけ消去される");
        assert_eq!(cell(&t3, 0, 2), "c", "ECHはシフトしないので列2はそのまま");
    }

    #[test]
    fn test_ich_dch_ech_confined_to_current_row() {
        // Fableレビュー観点: 「行内完結の確認」——scroll regionや他の行に一切影響しない。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2");
        feed(&mut t, b"\x1b[2;2H\x1b[3@"); // 行1・列1(0-indexed)へ、3セル挿入
        assert_eq!(cell(&t, 0, 0), "r", "行0はICHの影響を受けない");
        assert_eq!(cell(&t, 2, 0), "r", "行2はICHの影響を受けない");
        assert_eq!(cell(&t, 1, 0), "r", "行1のカーソルより左は不変");

        let mut t2 = Terminal::new(10, 3, Theme::default());
        feed(&mut t2, b"row0\r\nrow1\r\nrow2");
        feed(&mut t2, b"\x1b[2;2H\x1b[3P");
        assert_eq!(cell(&t2, 0, 0), "r", "行0はDCHの影響を受けない");
        assert_eq!(cell(&t2, 2, 0), "r", "行2はDCHの影響を受けない");

        let mut t3 = Terminal::new(10, 3, Theme::default());
        feed(&mut t3, b"row0\r\nrow1\r\nrow2");
        feed(&mut t3, b"\x1b[2;2H\x1b[3X");
        assert_eq!(cell(&t3, 0, 0), "r", "行0はECHの影響を受けない");
        assert_eq!(cell(&t3, 2, 0), "r", "行2はECHの影響を受けない");
    }

    #[test]
    fn test_ich_dch_ech_clamp_count_beyond_row_width_without_panic() {
        // n が行の残り幅を超える場合、usizeアンダーフローでpanicせず、残り幅に
        // クランプして行末まで埋まる/詰まることを確認する(IL/DLの同種テストに倣う)。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"abcdefghij");
        feed(&mut t, b"\x1b[1;3H\x1b[100@");
        for col in 2..10 {
            assert_eq!(cell(&t, 0, col), " ", "col {col} should be blank after over-sized ICH");
        }

        let mut t2 = Terminal::new(10, 3, Theme::default());
        feed(&mut t2, b"abcdefghij");
        feed(&mut t2, b"\x1b[1;3H\x1b[100P");
        for col in 2..10 {
            assert_eq!(cell(&t2, 0, col), " ", "col {col} should be blank after over-sized DCH");
        }

        let mut t3 = Terminal::new(10, 3, Theme::default());
        feed(&mut t3, b"abcdefghij");
        feed(&mut t3, b"\x1b[1;3H\x1b[100X");
        for col in 2..10 {
            assert_eq!(cell(&t3, 0, col), " ", "col {col} should be blank after over-sized ECH");
        }
    }

    #[test]
    fn test_ich_dch_ech_blank_uses_current_sgr_bg() {
        let red_bg = Theme::default().ansi16[1];

        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"abc");
        feed(&mut t, b"\x1b[41m"); // 赤背景
        feed(&mut t, b"\x1b[1;1H\x1b[1@");
        assert_eq!(t.screen_cells()[0].bg, red_bg, "ICHで挿入された空白は現在のSGR背景色を引き継ぐ");

        let mut t2 = Terminal::new(10, 3, Theme::default());
        feed(&mut t2, b"abcdefghij");
        feed(&mut t2, b"\x1b[41m");
        feed(&mut t2, b"\x1b[1;1H\x1b[1P"); // 先頭1セル削除 → 行末が空白で埋まる
        assert_eq!(t2.screen_cells()[9].bg, red_bg, "DCHで行末に埋められた空白も現在のSGR背景色を引き継ぐ");

        let mut t3 = Terminal::new(10, 3, Theme::default());
        feed(&mut t3, b"abc");
        feed(&mut t3, b"\x1b[41m");
        feed(&mut t3, b"\x1b[1;1H\x1b[1X");
        assert_eq!(t3.screen_cells()[0].bg, red_bg, "ECHで消去された空白も現在のSGR背景色を引き継ぐ");
    }

    #[test]
    fn test_ich_splits_wide_char_pair_into_blanks() {
        // Fableレビュー観点: 「全角文字の片割れが分断される場合の扱い」。
        // 行 "ab全cd"(全角文字は列2・3の2セルを占有)の、プレースホルダ(列3)へ
        // カーソルを置いて1セル挿入すると、本体(列2)とプレースホルダ(移動後は列4)の
        // 対応が崩れる — 両方とも孤立せず、通常の空白セルへ変換されることを確認する。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, "ab全cd".as_bytes());
        assert_eq!(cell(&t, 0, 2), "全", "前提: 全角文字は列2に本体を持つ");
        assert!(t.screen_cells()[3].is_wide_placeholder, "前提: 列3はプレースホルダ");

        feed(&mut t, b"\x1b[1;4H\x1b[1@"); // 列3(0-indexed、プレースホルダ)へ、1セル挿入
        assert_eq!(cell(&t, 0, 0), "a");
        assert_eq!(cell(&t, 0, 1), "b");
        assert_eq!(cell(&t, 0, 2), " ", "片割れを失った全角本体は空白へ変換される");
        assert_eq!(cell(&t, 0, 3), " ", "挿入された空白");
        assert_eq!(cell(&t, 0, 4), " ", "片割れを失ったプレースホルダも通常の空白になる");
        assert!(!t.screen_cells()[4].is_wide_placeholder, "孤立したプレースホルダフラグは解除される");
        assert_eq!(cell(&t, 0, 5), "c", "旧列4('c')が1列右へ押し出される");
        assert_eq!(cell(&t, 0, 6), "d", "旧列5('d')が1列右へ押し出される");
    }

    #[test]
    fn test_dch_splits_wide_char_pair_into_blanks() {
        // [test_ich_splits_wide_char_pair_into_blanks] と対になるDCH版。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, "ab全cd".as_bytes());
        feed(&mut t, b"\x1b[1;4H\x1b[1P"); // 列3(0-indexed、プレースホルダ)を1セル削除
        assert_eq!(cell(&t, 0, 0), "a");
        assert_eq!(cell(&t, 0, 1), "b");
        assert_eq!(cell(&t, 0, 2), " ", "片割れを失った全角本体は空白へ変換される");
        assert_eq!(cell(&t, 0, 3), "c", "旧列4('c')が1列左へ詰められる");
        assert_eq!(cell(&t, 0, 4), "d", "旧列5('d')が1列左へ詰められる");
    }

    #[test]
    fn test_ech_splits_wide_char_pair_into_blanks() {
        // [test_ich_splits_wide_char_pair_into_blanks] と同じ観点をECH(シフト無し)でも
        // 固定する。ECHは片割れの片方だけ(本体のみ、またはプレースホルダのみ)を
        // 消去範囲に含めるケースがあり得る点がICH/DCHと異なる(codexレビュー: 非
        // blockingのテスト補強候補として指摘)。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, "ab全cd".as_bytes());
        // 本体(列2)のみを消去 → プレースホルダ(列3)が孤立する。
        feed(&mut t, b"\x1b[1;3H\x1b[1X"); // 列2(0-indexed)を1セル消去
        assert_eq!(cell(&t, 0, 2), " ", "消去された本体");
        assert_eq!(cell(&t, 0, 3), " ", "孤立したプレースホルダは通常の空白になる");
        assert!(!t.screen_cells()[3].is_wide_placeholder, "孤立したプレースホルダフラグは解除される");
        assert_eq!(cell(&t, 0, 4), "c", "ECHはシフトしないので列4は不変");

        let mut t2 = Terminal::new(10, 3, Theme::default());
        feed(&mut t2, "ab全cd".as_bytes());
        // プレースホルダ(列3)のみを消去 → 本体(列2)が孤立する。
        feed(&mut t2, b"\x1b[1;4H\x1b[1X"); // 列3(0-indexed)を1セル消去
        assert_eq!(cell(&t2, 0, 2), " ", "片割れを失った本体は空白へ変換される");
        assert_eq!(cell(&t2, 0, 3), " ", "消去されたプレースホルダ");
        assert_eq!(cell(&t2, 0, 4), "c", "ECHはシフトしないので列4は不変");
    }

    #[test]
    fn test_ich_dch_ech_unaffected_by_scroll_region() {
        // タスク要件: 「行内完結の確認」——IL/DLと異なり、ICH/DCH/ECHはscroll region
        // (`CSI r`)の制約を一切受けない(xterm/VT102仕様上、行編集はscroll regionの
        // 外側のカーソル行でも常に効く)。scroll regionをわざと狭く設定した状態で、
        // regionの外にあるカーソル行に対しても正常に動作することを固定する。
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
        feed(&mut t, b"\x1b[2;4r"); // scroll region = 行1..3(0-indexed)、行0・4はregion外
        feed(&mut t, b"\x1b[1;3H\x1b[2@"); // 行0(region外)・列2で2セル挿入
        assert_eq!(cell(&t, 0, 2), " ", "region外の行0でもICHは効く");
        assert_eq!(cell(&t, 0, 3), " ", "region外の行0でもICHは効く");
        assert_eq!(cell(&t, 0, 4), "w", "旧列2('w')が2列右へ押し出される");

        feed(&mut t, b"\x1b[5;3H\x1b[1P"); // 行4(region外)・列2で1セル削除
        assert_eq!(cell(&t, 4, 2), "4", "旧列3('4')が1列左へ詰められる — region外の行4でもDCHは効く");
    }

    // ── HPA(CSI Ps `)/CHT(CSI Ps I)/CBT(CSI Ps Z)(タスク#65) ─────────

    #[test]
    fn test_hpa_moves_cursor_to_absolute_column_same_as_cha() {
        let mut t = Terminal::new(20, 3, Theme::default());
        feed(&mut t, b"\x1b[2;5H"); // row=1, col=4(0-indexed)へ移動しておく
        feed(&mut t, b"\x1b[10`"); // HPA: 列10(1-indexed)へ絶対移動
        assert_eq!((t.cursor_row(), t.cursor_col()), (1, 9), "HPAは行を変えず列だけ絶対移動する(CHA/'G'と同一挙動)");
    }

    #[test]
    fn test_hpa_default_param_is_column_one() {
        let mut t = Terminal::new(20, 3, Theme::default());
        feed(&mut t, b"\x1b[1;5H\x1b[`"); // Ps省略 == CSI 1`
        assert_eq!(t.cursor_col(), 0, "Ps省略時は既定値1(列0、0-indexed)");
    }

    #[test]
    fn test_hpa_clamps_to_last_column() {
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"\x1b[100`"); // 画面幅(10)を超える列指定
        assert_eq!(t.cursor_col(), 9, "画面幅を超える場合は最終列にクランプされる");
    }

    #[test]
    fn test_cht_advances_to_next_fixed_tab_stop() {
        let mut t = Terminal::new(40, 3, Theme::default());
        feed(&mut t, b"\x1b[1;3H"); // 列2(0-indexed)へ移動
        feed(&mut t, b"\x1b[I"); // CHT、Ps省略==1
        assert_eq!(t.cursor_col(), 8, "HT(固定8桁ストップ)と同じ次のタブストップへ進む");
    }

    #[test]
    fn test_cht_with_count_advances_multiple_tab_stops() {
        let mut t = Terminal::new(40, 3, Theme::default());
        feed(&mut t, b"\x1b[1;3H"); // 列2(0-indexed)へ移動
        feed(&mut t, b"\x1b[3I"); // 3個先のタブストップ(8, 16, 24)
        assert_eq!(t.cursor_col(), 24);
    }

    #[test]
    fn test_cht_normalizes_delayed_wrap_pending_cursor_to_last_column() {
        // codexレビュー指摘: `print()`で行末まで書いた直後は折り返し待ち状態
        // (`cursor_col == cols`、まだ改行はしていない)になる。CHTがこの状態を
        // 「既に行末」と誤認して何もしないと`cursor_col`が画面外(`cols`)の
        // ままになってしまう——HT(0x09)と同じ「計算してからクランプ」の順序で
        // 最終列(`cols - 1`)へ正規化されることを固定する。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"0123456789"); // 画面幅ぴったり書いて折り返し待ち状態にする
        assert_eq!(t.cursor_col(), 10, "前提: 折り返し待ち状態(cols)になっている");
        feed(&mut t, b"\x1b[I");
        assert_eq!(t.cursor_col(), 9, "折り返し待ち状態からでも最終列へ正規化される");
    }

    #[test]
    fn test_cht_clamps_at_last_column_without_overshoot() {
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"\x1b[1;3H"); // 列2(0-indexed)へ移動、次のタブストップ(8)は画面内
        feed(&mut t, b"\x1b[5I"); // 画面幅(10)を大きく超える回数を要求
        assert_eq!(t.cursor_col(), 9, "行末を超えて進まず最終列にクランプされる");
    }

    #[test]
    fn test_cbt_moves_back_to_previous_fixed_tab_stop() {
        let mut t = Terminal::new(40, 3, Theme::default());
        feed(&mut t, b"\x1b[1;20H"); // 列19(0-indexed)へ移動
        feed(&mut t, b"\x1b[Z"); // CBT、Ps省略==1
        assert_eq!(t.cursor_col(), 16, "直前のタブストップ(16)へ戻る");
    }

    #[test]
    fn test_cbt_exactly_on_tab_stop_moves_to_previous_one() {
        let mut t = Terminal::new(40, 3, Theme::default());
        feed(&mut t, b"\x1b[1;17H"); // 列16(0-indexed、タブストップ上)へ移動
        feed(&mut t, b"\x1b[Z");
        assert_eq!(t.cursor_col(), 8, "ちょうどタブストップ上にいる場合はその1つ前へ戻る(その場に留まらない)");
    }

    #[test]
    fn test_cbt_with_count_moves_back_multiple_tab_stops() {
        let mut t = Terminal::new(40, 3, Theme::default());
        feed(&mut t, b"\x1b[1;20H"); // 列19(0-indexed)へ移動
        feed(&mut t, b"\x1b[2Z"); // 2個前のタブストップ(16, 8)
        assert_eq!(t.cursor_col(), 8);
    }

    #[test]
    fn test_cbt_clamps_at_column_zero_without_underflow() {
        let mut t = Terminal::new(40, 3, Theme::default());
        feed(&mut t, b"\x1b[1;5H"); // 列4(0-indexed)へ移動
        feed(&mut t, b"\x1b[10Z"); // 大きく超える回数を要求
        assert_eq!(t.cursor_col(), 0, "列0を下回らずクランプされる(panicもしない)");
    }

    // ── HTS(`ESC H`)/TBC(`CSI g`、可変タブストップ、タスク#61) ─────────

    #[test]
    fn test_hts_adds_custom_tab_stop_that_ht_stops_at() {
        let mut t = Terminal::new(40, 3, Theme::default());
        feed(&mut t, b"\x1b[1;6H"); // 列5(0-indexed)へ移動
        feed(&mut t, b"\x1bH"); // HTS: 列5にタブストップを追加
        feed(&mut t, b"\x1b[1;1H"); // 行頭へ戻る
        feed(&mut t, b"\t"); // HT
        assert_eq!(t.cursor_col(), 5, "既定の8桁ストップより先に、HTSで追加したストップで止まる");
    }

    #[test]
    fn test_tbc_ps0_clears_only_tab_stop_at_cursor_column() {
        let mut t = Terminal::new(40, 3, Theme::default());
        feed(&mut t, b"\x1b[1;9H"); // 列8(0-indexed、既定のタブストップ上)へ移動
        feed(&mut t, b"\x1b[g"); // TBC Ps省略==0: この列のストップだけ解除
        feed(&mut t, b"\x1b[1;1H");
        feed(&mut t, b"\t"); // HT: 列8はもう止まらず次の16へ
        assert_eq!(t.cursor_col(), 16, "TBC(Ps=0)で解除した列は飛び越し、次のストップへ進む");
    }

    #[test]
    fn test_tbc_ps3_clears_all_tab_stops_so_ht_goes_to_last_column() {
        let mut t = Terminal::new(40, 3, Theme::default());
        feed(&mut t, b"\x1b[3g"); // TBC Ps=3: 全ストップ解除
        feed(&mut t, b"\t"); // HT: ストップが無いので最終列へ
        assert_eq!(t.cursor_col(), 39, "全ストップ解除後のHTは最終列まで進む");
    }

    #[test]
    fn test_cht_cbt_use_custom_tab_stops() {
        let mut t = Terminal::new(40, 3, Theme::default());
        feed(&mut t, b"\x1b[1;11H"); // 列10(0-indexed)へ移動
        feed(&mut t, b"\x1bH"); // HTS: 列10にカスタムストップ
        feed(&mut t, b"\x1b[1;1H");
        feed(&mut t, b"\x1b[I"); // CHT: 既定の8ではなく、まず既定の8に止まる
        assert_eq!(t.cursor_col(), 8, "既定の8桁ストップは維持されたまま");
        feed(&mut t, b"\x1b[I"); // 次は列10のカスタムストップへ
        assert_eq!(t.cursor_col(), 10, "HTSで追加したカスタムストップにもCHTが止まる");
        feed(&mut t, b"\x1b[Z"); // CBT: カスタムストップ(10)から既定の8へ戻る
        assert_eq!(t.cursor_col(), 8);
    }

    #[test]
    fn test_tab_stops_survive_resize_within_old_width_but_new_columns_get_default_pattern() {
        let mut t = Terminal::new(20, 3, Theme::default());
        feed(&mut t, b"\x1b[1;9H"); // 列8(既定のストップ上)へ移動
        feed(&mut t, b"\x1b[g"); // TBC Ps=0: 列8のストップを解除
        t.resize_preserving_state(40, 3); // 幅を広げる(既存タブストップ状態は保持されるべき)
        feed(&mut t, b"\x1b[1;1H");
        feed(&mut t, b"\t"); // HT: 列8は解除済みなので飛び越して次の16へ
        assert_eq!(t.cursor_col(), 16, "リサイズ前に解除した列8のストップは保持されたまま(復活しない)");
        // 新しく増えた列(20..40)は既定の8桁パターンで初期化されている。
        feed(&mut t, b"\x1b[1;25H"); // 列24(0-indexed、新しい領域内の既定ストップ)へ移動
        feed(&mut t, b"\t");
        assert_eq!(t.cursor_col(), 32, "新しく増えた列は既定の8桁パターンで初期化される");
    }

    #[test]
    fn test_ris_restores_default_tab_stops() {
        let mut t = Terminal::new(40, 3, Theme::default());
        feed(&mut t, b"\x1b[3g"); // 全ストップ解除
        feed(&mut t, b"\x1bc"); // RIS
        feed(&mut t, b"\t"); // HT: 既定の8桁パターンに戻っているはず
        assert_eq!(t.cursor_col(), 8, "RISでタブストップは既定(8桁おき)に戻る");
    }

    // ── DEC private modeの複数パラメータ(Pm)対応(タスク#68) ─────────────

    #[test]
    fn test_dec_private_mode_multi_param_applies_all_of_them() {
        // `CSI ?25l`でカーソルを隠した状態から、`CSI ?1049;25h`(alt screenへ切替
        // + カーソル再表示、DECTCEMは`h`=表示/`l`=非表示)を1シーケンスで送る。
        // 修正前は先頭パラメータ(1049)しか見ておらず、25(カーソル再表示)が
        // 無視されていた。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"main");
        feed(&mut t, b"\x1b[?25l");
        assert!(!t.cursor_visible());
        feed(&mut t, b"\x1b[?1049;25h");
        assert_eq!(cell(&t, 0, 0), " ", "1049によりalt screen(空白)へ切り替わっていること");
        assert!(t.cursor_visible(), "25も同一シーケンス内で処理されカーソルが再表示されること");
    }

    #[test]
    fn test_dec_private_mode_multi_param_reset_applies_all_of_them() {
        // 逆方向(`CSI ?1049;25l`): alt screenからmainへ戻る + カーソル非表示、を
        // 1シーケンスで送る。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"main");
        feed(&mut t, b"\x1b[?1049h"); // alt画面へ(この時点ではcursor_visibleはtrueのまま)
        feed(&mut t, b"\x1b[?1049;25l");
        assert_eq!(cell(&t, 0, 0), "m", "1049によりmain screen(内容復元)へ戻っていること");
        assert!(!t.cursor_visible(), "25も同一シーケンス内で処理されカーソルが隠れていること");
    }

    #[test]
    fn test_dec_private_mode_multi_param_mixes_mouse_and_other_modes() {
        // マウス関連(既存対応済み)と他のモードが同一シーケンスに混在するケースも
        // 全パラメータが処理されることを確認する。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"\x1b[?1000;1006;7l"); // マウスOFF(既定と同じだが明示) + DECAWM off
        assert!(!t.autowrap_mode());
        feed(&mut t, b"\x1b[?1000;1006;7h");
        assert_eq!(t.mouse_reporting_mode(), MouseReportingMode::Normal);
        assert!(t.sgr_mouse_mode());
        assert!(t.autowrap_mode());
    }

    // ── DECSC/DECRC(`ESC 7`/`ESC 8`)・CSI s/u(タスク#57) ─────────────

    #[test]
    fn test_esc_7_8_saves_and_restores_cursor_position_and_sgr() {
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b[3;5H"); // row=2, col=4 (0-indexed)
        feed(&mut t, b"\x1b[1m"); // bold on
        feed(&mut t, b"\x1b7"); // DECSC: 位置(2,4)・bold=trueを保存
        feed(&mut t, b"\x1b[1;1H"); // カーソル移動
        feed(&mut t, b"\x1b[0m"); // bold off
        feed(&mut t, b"\x1b8"); // DECRC: 復元
        assert_eq!(t.cursor_row(), 2);
        assert_eq!(t.cursor_col(), 4);
        feed(&mut t, b"x");
        assert!(cell_bold(&t, 2, 4), "DECRC後、保存時のbold属性で描画される");
    }

    #[test]
    fn test_csi_s_u_ansi_sys_saves_and_restores_cursor() {
        // CSI s / CSI u は ESC 7 / ESC 8 と同義(ANSI.SYS方言、DECLRMM未実装なので曖昧さ無し)。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b[3;5H\x1b[s"); // 位置(2,4)を保存
        feed(&mut t, b"\x1b[1;1H"); // カーソル移動
        feed(&mut t, b"\x1b[u"); // 復元
        assert_eq!(t.cursor_row(), 2);
        assert_eq!(t.cursor_col(), 4);
    }

    #[test]
    fn test_decrc_without_prior_decsc_is_noop() {
        // 事前の保存が無い状態でのDECRCは、カーソルを勝手に移動させない(安全側no-op)。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b[3;5H");
        feed(&mut t, b"\x1b8"); // 保存なしのDECRC
        assert_eq!(t.cursor_row(), 2);
        assert_eq!(t.cursor_col(), 4);
    }

    #[test]
    fn test_decsc_alt_screen_slot_independent_from_main() {
        // alt画面上の明示ESC7/ESC8は、main画面のDECSCスロットと独立している
        // ([Terminal]の`saved_cursor_main`フィールドdocコメント参照)。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b[2;3H"); // main: row=1, col=2
        feed(&mut t, b"\x1b7"); // main側スロットに保存
        feed(&mut t, b"\x1b[?1049h"); // alt画面へ(暗黙的にmain側スロットを上書き保存)
        feed(&mut t, b"\x1b[4;4H"); // alt: row=3, col=3
        feed(&mut t, b"\x1b7"); // alt側スロットに保存
        feed(&mut t, b"\x1b[1;1H"); // alt画面上でカーソル移動
        feed(&mut t, b"\x1b8"); // alt側スロットから復元
        assert_eq!(t.cursor_row(), 3);
        assert_eq!(t.cursor_col(), 3);
        feed(&mut t, b"\x1b[?1049l"); // main画面へ復帰(?1049hが上書き保存した位置に復元)
        assert_eq!(t.cursor_row(), 1);
        assert_eq!(t.cursor_col(), 2);
    }

    #[test]
    fn test_ris_clears_saved_decsc_cursor() {
        // RIS(`ESC c`)は保存済みDECSCスロットもクリアする——リセット後のDECRCが
        // リセット前の古い位置へ復元してしまう回帰を防ぐ。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b[3;5H\x1b7"); // 位置(2,4)を保存
        feed(&mut t, b"\x1bc"); // RIS
        feed(&mut t, b"\x1b8"); // 保存済みスロットは無いはず → no-op
        assert_eq!(t.cursor_row(), 0);
        assert_eq!(t.cursor_col(), 0);
    }

    #[test]
    fn test_esc_hash_8_decaln_is_not_mistaken_for_decrc() {
        // codexレビュー指摘: `ESC # 8`(DECALN、intermediate `#`付き)は最終バイトが
        // DECRC(`ESC 8`)と同じ`8`だが別シーケンス。DECALN自体は未実装(no-op)なので、
        // 保存済みカーソルへ誤って復元(DECRCとして処理)されないことを固定する。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b[3;5H\x1b7"); // 位置(2,4)を保存
        feed(&mut t, b"\x1b[1;1H"); // カーソル移動
        feed(&mut t, b"\x1b#8"); // DECALN(未実装・no-op) — DECRCとして誤処理しないこと
        assert_eq!(t.cursor_row(), 0, "DECALNはDECRCとして扱われず、カーソルは動かない");
        assert_eq!(t.cursor_col(), 0);
    }

    #[test]
    fn test_naked_decrc_after_1049_exit_without_new_decsc_is_noop() {
        // `switch_to_main`(`?1047l`/`?1049l`)は復元後に保存スロットを消費(take)する
        // 既存挙動(タスク#57の変更対象外)。これにより、alt画面を抜けた後に新たな
        // `ESC 7`無しで単独の`ESC 8`が来ても、古い1049復元位置へ再度ジャンプしたり
        // せず安全にno-opになることを固定する(codexレビュー: `restore_cursor_decrc`が
        // 非消費であることとの整合性に対する指摘への対応)。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b[2;3H"); // main: row=1, col=2
        feed(&mut t, b"\x1b[?1049h"); // alt画面へ(main側スロットへ暗黙保存)
        feed(&mut t, b"\x1b[?1049l"); // main画面へ復帰(スロットを消費して復元)
        assert_eq!(t.cursor_row(), 1);
        assert_eq!(t.cursor_col(), 2);
        feed(&mut t, b"\x1b[5;5H"); // 別の位置へ移動
        feed(&mut t, b"\x1b8"); // 新たなDECSC無しの単独DECRC → no-op
        assert_eq!(t.cursor_row(), 4, "スロットは1049l復帰時に消費済みなのでno-op");
        assert_eq!(t.cursor_col(), 4);
    }

    #[test]
    fn test_csi_s_two_param_form_still_saves_cursor_position_not_margins() {
        // `CSI Pl;Pr s`はDECLRMM(左右マージンモード)有効時はDECSLRM(マージン設定)に
        // 化けるが、このコードベースはDECLRMM/左右マージン自体を実装していないため、
        // パラメータの有無・個数によらず常にDECSCと同義のsave cursorとして扱う
        // (`csi_dispatch`のコメント参照、codexレビュー指摘への対応として明示的に固定)。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b[3;5H\x1b[7;12s"); // 位置(2,4)、パラメータ2個付きのCSI s
        feed(&mut t, b"\x1b[1;1H");
        feed(&mut t, b"\x1b[u");
        assert_eq!(t.cursor_row(), 2, "パラメータはマージン設定ではなく無視され、保存時のカーソル位置が使われる");
        assert_eq!(t.cursor_col(), 4);
    }

    fn cell_bold(t: &Terminal, row: usize, col: usize) -> bool {
        t.screen_cells()[row * t.cols() + col].bold
    }

    // ── G0/G1文字セット・DEC Special Graphics(タスク#41) ─────────

    #[test]
    fn test_dec_special_graphics_maps_line_drawing_chars() {
        // `ESC ( 0`でG0をDEC Special Graphicsに指定すると、以降のASCII 'q'/'x'/'l'等が
        // 罫線文字(non-UTF-8ロケールのncurses/mc等が出力する典型パターン)に写像される。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b(0lqqqk\r\nx   x\r\nmqqqj\x1b(B");
        assert_eq!(cell(&t, 0, 0), "┌");
        assert_eq!(cell(&t, 0, 1), "─");
        assert_eq!(cell(&t, 0, 4), "┐");
        assert_eq!(cell(&t, 1, 0), "│");
        assert_eq!(cell(&t, 1, 4), "│");
        assert_eq!(cell(&t, 2, 0), "└");
        assert_eq!(cell(&t, 2, 4), "┘");
    }

    #[test]
    fn test_esc_paren_b_reverts_to_ascii() {
        // `ESC ( B`(US ASCII)へ戻すと、以降は通常のASCIIとして印字される。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b(0q\x1b(Bq");
        assert_eq!(cell(&t, 0, 0), "─", "DEC Special Graphics指定中の'q'は罫線に写像される");
        assert_eq!(cell(&t, 0, 1), "q", "ASCIIへ戻した後の'q'はそのまま");
    }

    #[test]
    fn test_si_so_switches_between_g0_and_g1() {
        // SO(0x0E)でG1を、SI(0x0F)でG0をGLへ呼び出す。G1だけをDEC Special Graphicsに
        // 指定し、G0はASCIIのまま残しておくことでSI/SOの切替自体を検証する。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b)0"); // G1 = DEC Special Graphics(G0はASCIIのまま)
        feed(&mut t, b"q"); // GL=G0(既定) → 素のASCII 'q'
        feed(&mut t, b"\x0e"); // SO: GL=G1
        feed(&mut t, b"q"); // → 罫線に写像
        feed(&mut t, b"\x0f"); // SI: GL=G0
        feed(&mut t, b"q"); // → 再びASCII
        assert_eq!(cell(&t, 0, 0), "q");
        assert_eq!(cell(&t, 0, 1), "─");
        assert_eq!(cell(&t, 0, 2), "q");
    }

    #[test]
    fn test_charset_unknown_final_byte_falls_back_to_ascii() {
        // 未対応の最終バイト(例: UK国別セット`A`)はASCIIとして扱う([Charset]の
        // docコメント参照——`0`(DEC Special Graphics)以外はグラフィック写像を
        // 持たないASCII相当の文字集合という設計方針をそのまま反映する。
        // codexレビュー: 以前は無視して直前の指定を保持していたが、コメントの
        // 意図(「区別せずASCIIとして扱う」)と食い違っていたため修正)。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b(0"); // G0 = DEC Special Graphics
        feed(&mut t, b"\x1b(A"); // 未対応の最終バイト → ASCIIへフォールバック
        feed(&mut t, b"q");
        assert_eq!(cell(&t, 0, 0), "q", "未対応の指定はASCII相当として扱われる");
    }

    #[test]
    fn test_esc_paren_c_is_g0_charset_fallback_not_ris() {
        // タスク#75回帰テスト: `ESC ( c`は中間バイト`(`付きのG0文字セット指定
        // (未対応の最終バイト`c` → ASCIIフォールバック)であり、intermediate無しの
        // `ESC c`(RIS)とは全く別のシーケンス。`b'c'`アームにガードが無いと
        // `ints`を無視して`reset_all()`(全画面消去+カーソルホーム等)が誤発動する。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"hello");
        feed(&mut t, b"\x1b(c"); // G0文字セット指定(未対応の最終バイト) であり RIS ではない
        assert_eq!(cell(&t, 0, 0), "h", "ESC ( c で画面がリセットされてはいけない");
        assert_eq!(t.cursor_col(), 5, "ESC ( c でカーソルがホームへ戻ってはいけない");
    }

    #[test]
    fn test_esc_c_without_intermediate_is_still_ris() {
        // 上のガード追加が`ESC c`(intermediate無し、本来のRIS)自体を壊していない
        // ことの確認。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"hello");
        feed(&mut t, b"\x1bc"); // RIS
        assert_eq!(cell(&t, 0, 0), " ", "ESC c(RIS)は画面をリセットする");
        assert_eq!(t.cursor_col(), 0, "ESC c(RIS)はカーソルをホームへ戻す");
    }

    #[test]
    fn test_esc_paren_m_is_g0_charset_fallback_not_ri() {
        // タスク#75回帰テスト: `ESC ( M`は中間バイト付きのG0文字セット指定
        // (未対応の最終バイト`M` → ASCIIフォールバック)であり、intermediate
        // 無しの`ESC M`(RI/Reverse Index、カーソル上移動またはスクロール)とは別
        // シーケンス。`b'M'`アームにガードが無いと`ints`を無視してRI相当の画面
        // 移動が誤発動する(codexレビュー指摘: `ESC ) M`と1テストに同居させると
        // 旧実装で`ESC ( M`側の失敗で先に落ちて`ESC ) M`側の再現性が検証されない
        // ため、独立したテストに分離)。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"hello\r\n");
        assert_eq!(t.cursor_row(), 1);
        feed(&mut t, b"\x1b(M"); // G0文字セット指定(未対応の最終バイト)であり RI ではない
        assert_eq!(t.cursor_row(), 1, "ESC ( M でカーソルが上に移動してはいけない");
        assert_eq!(cell(&t, 0, 0), "h", "ESC ( M で画面がスクロールしてはいけない");
    }

    #[test]
    fn test_esc_close_paren_m_is_g1_charset_fallback_not_ri() {
        // タスク#75回帰テスト: `ESC ) M`は中間バイト付きのG1文字セット指定
        // (未対応の最終バイト`M` → ASCIIフォールバック)であり、`ESC M`(RI)とは
        // 別シーケンス。上の`ESC ( M`テストと同じ理由で独立したテストとして
        // 再現条件を検証する(codexレビュー指摘)。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"hello\r\n");
        assert_eq!(t.cursor_row(), 1);
        feed(&mut t, b"\x1b)M"); // G1文字セット指定(未対応の最終バイト)であり RI ではない
        assert_eq!(t.cursor_row(), 1, "ESC ) M でカーソルが上に移動してはいけない");
        assert_eq!(cell(&t, 0, 0), "h", "ESC ) M で画面がスクロールしてはいけない");
    }

    #[test]
    fn test_esc_m_without_intermediate_is_still_reverse_index() {
        // 上のガード追加が`ESC M`(intermediate無し、本来のRI)自体を壊していない
        // ことの確認。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"hello\r\n");
        assert_eq!(t.cursor_row(), 1);
        feed(&mut t, b"\x1bM"); // RI: カーソルが1行上へ
        assert_eq!(t.cursor_row(), 0, "ESC M(RI)はカーソルを上に移動する");
    }

    #[test]
    fn test_dec_special_graphics_underscore_maps_to_blank() {
        // VT100 User Guide Table 3-9: DEC Special Graphics上の`_`(0x5f)はblank
        // (空白)に写像される(codexレビュー指摘: 当初0x60〜0x7eのみ対応しており
        // 0x5fが漏れていた)。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b(0_");
        assert_eq!(cell(&t, 0, 0), " ");
    }

    #[test]
    fn test_alt_screen_switch_saves_and_restores_charset_state() {
        // `?1049h`/`?1049l`(alt画面切替)もDECSC/DECRCと同じスロットを共有するため、
        // 文字セット状態も保存/復元対象になる(タスク#41、`switch_to_alt`/
        // `switch_to_main`のコメント参照。codexレビュー: alt画面切替経路の
        // charsetカバレッジが薄いという指摘への対応)。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b(0"); // main: G0 = DEC Special Graphics
        feed(&mut t, b"\x1b[?1049h"); // alt画面へ(main側の文字セット状態を暗黙保存)
        // alt画面に入った直後はフレッシュな既定(G0=ASCII)にリセットされている。
        feed(&mut t, b"q");
        assert_eq!(cell(&t, 0, 0), "q", "alt画面入場直後はG0=ASCIIにリセットされる");
        feed(&mut t, b"\x1b(0"); // alt側でG0をDEC Special Graphicsに変更
        feed(&mut t, b"\r"); // 行頭へ戻す
        feed(&mut t, b"q");
        assert_eq!(cell(&t, 0, 0), "─", "alt画面上でも独立してDEC Special Graphicsを指定できる");
        feed(&mut t, b"\x1b[?1049l"); // main画面へ復帰(保存されたG0=DEC Special Graphicsが復元される)
        feed(&mut t, b"q");
        assert_eq!(cell(&t, 0, 0), "─", "main画面復帰後は\\x1b(0直後に保存したDEC Special Graphics指定が復元される");
    }

    #[test]
    fn test_decsc_decrc_save_restore_charset_state() {
        // DECSC(`ESC 7`)/DECRC(`ESC 8`)は仕様上カーソル位置・SGR属性に加え文字セット
        // 状態も保存/復元対象(タスク#41、[Terminal]の`saved_cursor_main`docコメント参照)。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b(0"); // G0 = DEC Special Graphics
        feed(&mut t, b"\x1b7"); // 保存(G0=DEC Special Graphicsを含む)
        feed(&mut t, b"\x1b(B"); // G0をASCIIへ変更
        feed(&mut t, b"q");
        assert_eq!(cell(&t, 0, 0), "q", "ESC 7後にASCIIへ変更した直後はASCIIのまま");
        feed(&mut t, b"\x1b8"); // 復元 → カーソルも(0,0)へ戻り、G0はDEC Special Graphicsへ戻る
        feed(&mut t, b"q");
        assert_eq!(cell(&t, 0, 0), "─", "ESC 8でDEC Special Graphics指定が復元される");
    }

    #[test]
    fn test_ris_resets_charset_state() {
        // RIS(`ESC c`)はG0/G1指定・GL選択をすべて既定(G0=G1=ASCII、GL=G0)へ戻す。
        let mut t = Terminal::new(20, 5, Theme::default());
        feed(&mut t, b"\x1b(0\x0e"); // G0=DEC Special Graphics、GL=G1(SO)
        feed(&mut t, b"\x1bc"); // RIS
        feed(&mut t, b"q");
        assert_eq!(cell(&t, 0, 0), "q", "RIS後はASCII/G0既定に戻っている");
    }

    // ── マウスレポーティング(タスク#36)────────────────────

    fn no_mods() -> TerminalKeyModifiers { TerminalKeyModifiers::default() }

    #[test]
    fn test_mouse_mode_default_off() {
        let t = Terminal::new(80, 24, Theme::default());
        assert_eq!(t.mouse_reporting_mode(), MouseReportingMode::Off);
        assert!(!t.sgr_mouse_mode());
    }

    #[test]
    fn test_mouse_mode_decset_1000_1002_1003() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h");
        assert_eq!(t.mouse_reporting_mode(), MouseReportingMode::Normal);
        feed(&mut t, b"\x1b[?1002h");
        assert_eq!(t.mouse_reporting_mode(), MouseReportingMode::ButtonEvent);
        feed(&mut t, b"\x1b[?1003h");
        assert_eq!(t.mouse_reporting_mode(), MouseReportingMode::AnyEvent);
    }

    #[test]
    fn test_mouse_mode_decrst_any_number_turns_off() {
        // xterm互換: `?1000`/`?1002`/`?1003`は同一の内部モードを共有するため、
        // どの番号でreset(`l`)しても番号に関わらずOffに戻る。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1003h");
        assert_eq!(t.mouse_reporting_mode(), MouseReportingMode::AnyEvent);
        feed(&mut t, b"\x1b[?1000l");
        assert_eq!(t.mouse_reporting_mode(), MouseReportingMode::Off);
    }

    #[test]
    fn test_mouse_mode_sgr_1006_independent_toggle() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1006h");
        assert!(t.sgr_mouse_mode());
        // SGRを先にonにしても、マウストラッキング自体は別モードのまま(Off)。
        assert_eq!(t.mouse_reporting_mode(), MouseReportingMode::Off);
        feed(&mut t, b"\x1b[?1006l");
        assert!(!t.sgr_mouse_mode());
    }

    #[test]
    fn test_mouse_mode_reset_by_ris() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1003h\x1b[?1006h");
        assert_eq!(t.mouse_reporting_mode(), MouseReportingMode::AnyEvent);
        assert!(t.sgr_mouse_mode());
        feed(&mut t, b"\x1bc"); // RIS
        assert_eq!(t.mouse_reporting_mode(), MouseReportingMode::Off, "RISで既定のOffに戻る");
        assert!(!t.sgr_mouse_mode(), "RISで既定のfalseに戻る");
    }

    #[test]
    fn test_mouse_mode_preserved_across_resize() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1002h\x1b[?1006h");
        t.resize_preserving_state(40, 12);
        assert_eq!(t.mouse_reporting_mode(), MouseReportingMode::ButtonEvent);
        assert!(t.sgr_mouse_mode());
    }

    #[test]
    fn test_mouse_mode_decset_combined_pm_sets_tracking_and_sgr_together() {
        // vim/tmux等はトラッキングモードとSGR拡張を`CSI ?1000;1006h`のように
        // 1シーケンスにまとめて送ることが珍しくない(codexレビュー指摘: 先頭
        // パラメータしか見ないと後続の1006が無視され、SGRを要求したのにlegacy
        // X10形式のまま返してしまうバグになる)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000;1006h");
        assert_eq!(t.mouse_reporting_mode(), MouseReportingMode::Normal);
        assert!(t.sgr_mouse_mode());
        feed(&mut t, b"\x1b[?1000;1006l");
        assert_eq!(t.mouse_reporting_mode(), MouseReportingMode::Off);
        assert!(!t.sgr_mouse_mode());
    }

    #[test]
    fn test_encode_pointer_event_off_mode_reports_nothing() {
        let t = Terminal::new(80, 24, Theme::default());
        let event = PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::Left), modifiers: no_mods(),
        };
        assert_eq!(t.encode_pointer_event(event), None);
    }

    #[test]
    fn test_encode_pointer_event_sgr_press_and_release() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h\x1b[?1006h");
        let press = t.encode_pointer_event(PointerEvent {
            row: 4, col: 9, kind: MouseEventKind::Press,
            button: Some(MouseButton::Left), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(press, b"\x1b[<0;10;5M");
        let release = t.encode_pointer_event(PointerEvent {
            row: 4, col: 9, kind: MouseEventKind::Release,
            button: Some(MouseButton::Left), modifiers: no_mods(),
        }).unwrap();
        // releaseは同じボタン番号のまま、終端が小文字'm'になる(SGRはreleaseでも
        // どのボタンが離されたか表現できる)。
        assert_eq!(release, b"\x1b[<0;10;5m");
    }

    #[test]
    fn test_encode_pointer_event_sgr_with_modifiers() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h\x1b[?1006h");
        let mods = TerminalKeyModifiers { shift: true, ctrl: true, ..Default::default() };
        let press = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::Right), modifiers: mods,
        }).unwrap();
        // Right=2, Shift(4)+Ctrl(16)=20 → Cb=22。
        assert_eq!(press, b"\x1b[<22;1;1M");
    }

    #[test]
    fn test_encode_pointer_event_sgr_wheel() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h\x1b[?1006h");
        let up = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::WheelUp), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(up, b"\x1b[<64;1;1M");
        let down = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::WheelDown), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(down, b"\x1b[<65;1;1M");
    }

    #[test]
    fn test_encode_pointer_event_legacy_x10_press_and_release() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h"); // 1006無し(レガシー)
        let press = t.encode_pointer_event(PointerEvent {
            row: 4, col: 9, kind: MouseEventKind::Press,
            button: Some(MouseButton::Left), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(press, vec![0x1B, b'[', b'M', 32, 32 + 10, 32 + 5]);
        // レガシー形式のreleaseは仕様上どのボタンだったか表現できず常に3(no button)。
        let release = t.encode_pointer_event(PointerEvent {
            row: 4, col: 9, kind: MouseEventKind::Release,
            button: Some(MouseButton::Left), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(release, vec![0x1B, b'[', b'M', 32 + 3, 32 + 10, 32 + 5]);
    }

    #[test]
    fn test_encode_pointer_event_legacy_x10_clamps_coordinates_at_223() {
        // 1バイトにしかエンコードできないレガシー形式は、座標を223で頭打ちに
        // クランプする(Fableレビュー指摘: 割り切って未実装にせず実装する判断)。
        let mut t = Terminal::new(300, 300, Theme::default());
        feed(&mut t, b"\x1b[?1000h");
        let press = t.encode_pointer_event(PointerEvent {
            row: 299, col: 299, kind: MouseEventKind::Press,
            button: Some(MouseButton::Left), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(press, vec![0x1B, b'[', b'M', 32, 32 + 223, 32 + 223]);
    }

    #[test]
    fn test_encode_pointer_event_clamps_out_of_bounds_coordinates_to_terminal_size() {
        // 呼び出し元が誤って画面外の座標(例: リサイズ直後の古い座標)を渡してきても、
        // この端末の実サイズへクランプしてから送る(codexレビュー指摘: SGRが
        // 無クランプだと、80列の端末でも列1001のような存在しない座標を報告できて
        // しまっていた)。80x24の端末なので有効な最終セルは(23, 79)(0-based)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h\x1b[?1006h");
        let sgr = t.encode_pointer_event(PointerEvent {
            row: 1000, col: 1000, kind: MouseEventKind::Press,
            button: Some(MouseButton::Left), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(sgr, b"\x1b[<0;80;24M", "SGRも端末の最終列/行にクランプされる");

        feed(&mut t, b"\x1b[?1006l"); // legacy形式に切り替え
        let legacy = t.encode_pointer_event(PointerEvent {
            row: 1000, col: 1000, kind: MouseEventKind::Press,
            button: Some(MouseButton::Left), modifiers: no_mods(),
        }).unwrap();
        // 80/24とも223未満なので、プロトコル上限ではなく端末サイズでクランプされる。
        assert_eq!(legacy, vec![0x1B, b'[', b'M', 32, 32 + 80, 32 + 24]);
    }

    #[test]
    fn test_encode_pointer_event_motion_suppressed_in_normal_mode() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h"); // Normal: press/releaseのみ
        let motion = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Motion,
            button: Some(MouseButton::Left), modifiers: no_mods(),
        });
        assert_eq!(motion, None, "Normalモードではドラッグ移動も報告しない");
    }

    #[test]
    fn test_encode_pointer_event_drag_reported_in_button_event_mode() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1002h\x1b[?1006h"); // ButtonEvent
        let drag = t.encode_pointer_event(PointerEvent {
            row: 1, col: 1, kind: MouseEventKind::Motion,
            button: Some(MouseButton::Left), modifiers: no_mods(),
        }).unwrap();
        // motionビット(32)がCbに加算される: 0(Left) + 32 = 32。
        assert_eq!(drag, b"\x1b[<32;2;2M");
        // ボタン無しの単純な移動はButtonEventモードでは報告しない。
        let hover = t.encode_pointer_event(PointerEvent {
            row: 1, col: 1, kind: MouseEventKind::Motion,
            button: None, modifiers: no_mods(),
        });
        assert_eq!(hover, None, "ButtonEventモードはボタン無しの移動を報告しない");
    }

    #[test]
    fn test_encode_pointer_event_hover_reported_in_any_event_mode() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1003h\x1b[?1006h"); // AnyEvent
        let hover = t.encode_pointer_event(PointerEvent {
            row: 2, col: 2, kind: MouseEventKind::Motion,
            button: None, modifiers: no_mods(),
        }).unwrap();
        // ボタン無し移動のbaseは3("no button")+ motionビット(32) = 35。
        assert_eq!(hover, b"\x1b[<35;3;3M");
    }

    #[test]
    fn test_encode_pointer_event_wheel_reported_even_in_normal_mode() {
        // ホイールは移動ではなくPress扱いなので、Normal(?1000)でも報告される。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h\x1b[?1006h");
        let up = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::WheelUp), modifiers: no_mods(),
        });
        assert!(up.is_some());
    }

    #[test]
    fn test_encode_pointer_event_urxvt_press_and_release() {
        // URXVT (?1015) uses `CSI Cb ; Cx ; Cy M` (no `<` prefix, unlike SGR).
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h\x1b[?1015h");
        let press = t.encode_pointer_event(PointerEvent {
            row: 4, col: 9, kind: MouseEventKind::Press,
            button: Some(MouseButton::Left), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(press, b"\x1b[0;10;5M");
        let release = t.encode_pointer_event(PointerEvent {
            row: 4, col: 9, kind: MouseEventKind::Release,
            button: Some(MouseButton::Left), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(release, b"\x1b[0;10;5m");
    }

    #[test]
    fn test_encode_pointer_event_urxvt_with_modifiers() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h\x1b[?1015h");
        let mods = TerminalKeyModifiers { shift: true, ctrl: true, ..Default::default() };
        let press = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::Right), modifiers: mods,
        }).unwrap();
        // Right=2, Shift(4)+Ctrl(16)=20 → Cb=22
        assert_eq!(press, b"\x1b[22;1;1M");
    }

    #[test]
    fn test_encode_pointer_event_urxvt_wheel() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h\x1b[?1015h");
        let up = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::WheelUp), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(up, b"\x1b[64;1;1M");
        let down = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::WheelDown), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(down, b"\x1b[65;1;1M");
    }

    #[test]
    fn test_encode_pointer_event_urxvt_drag_motion() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1002h\x1b[?1015h");
        let drag = t.encode_pointer_event(PointerEvent {
            row: 1, col: 1, kind: MouseEventKind::Motion,
            button: Some(MouseButton::Left), modifiers: no_mods(),
        }).unwrap();
        // motionビット(32)がCbに加算: 0(Left) + 32 = 32
        assert_eq!(drag, b"\x1b[32;2;2M");
    }

    #[test]
    fn test_encode_pointer_event_sgr_takes_precedence_over_urxvt() {
        // When both ?1006 (SGR) and ?1015 (URXVT) are enabled, SGR wins.
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h\x1b[?1006h\x1b[?1015h");
        let press = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::Left), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(press, b"\x1b[<0;1;1M", "SGR has `<` prefix, URXVT does not");
    }

    #[test]
    fn test_encode_pointer_event_horizontal_wheel_sgr() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h\x1b[?1006h");
        let left = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::WheelLeft), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(left, b"\x1b[<66;1;1M");
        let right = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::WheelRight), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(right, b"\x1b[<67;1;1M");
    }

    #[test]
    fn test_encode_pointer_event_horizontal_wheel_legacy() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h"); // legacy X10
        let left = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::WheelLeft), modifiers: no_mods(),
        }).unwrap();
        // WheelLeft=66, +32 = 98
        assert_eq!(left, vec![0x1B, b'[', b'M', 32 + 66, 32 + 1, 32 + 1]);
        let right = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::WheelRight), modifiers: no_mods(),
        }).unwrap();
        // WheelRight=67, +32 = 99
        assert_eq!(right, vec![0x1B, b'[', b'M', 32 + 67, 32 + 1, 32 + 1]);
    }

    #[test]
    fn test_encode_pointer_event_middle_button_sgr() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h\x1b[?1006h");
        let press = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::Middle), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(press, b"\x1b[<1;1;1M");
    }

    #[test]
    fn test_encode_pointer_event_right_button_legacy() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h");
        let press = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::Right), modifiers: no_mods(),
        }).unwrap();
        // Right=2, +32 = 34
        assert_eq!(press, vec![0x1B, b'[', b'M', 32 + 2, 32 + 1, 32 + 1]);
    }

    #[test]
    fn test_encode_pointer_event_urxvt_horizontal_wheel() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1000h\x1b[?1015h");
        let left = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::WheelLeft), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(left, b"\x1b[66;1;1M");
        let right = t.encode_pointer_event(PointerEvent {
            row: 0, col: 0, kind: MouseEventKind::Press,
            button: Some(MouseButton::WheelRight), modifiers: no_mods(),
        }).unwrap();
        assert_eq!(right, b"\x1b[67;1;1M");
    }

    // ── フォーカスレポーティング(`?1004`、タスク#60) ─────────

    #[test]
    fn test_cursor_forward_cuf() {
        let mut t = Terminal::new(80, 24, Theme::default());
        // Place cursor at col 5, then move forward 3
        t.cursor_col = 5;
        feed(&mut t, b"\x1b[3C");
        assert_eq!(t.cursor_col, 8);
        assert_eq!(t.cursor_row, 0);
    }

    #[test]
    fn test_cursor_forward_cuf_clamps_at_last_column() {
        let mut t = Terminal::new(80, 24, Theme::default());
        t.cursor_col = 78;
        feed(&mut t, b"\x1b[5C");
        assert_eq!(t.cursor_col, 79); // cols-1
    }

    #[test]
    fn test_cursor_backward_cub() {
        let mut t = Terminal::new(80, 24, Theme::default());
        t.cursor_col = 10;
        feed(&mut t, b"\x1b[4D");
        assert_eq!(t.cursor_col, 6);
    }

    #[test]
    fn test_cursor_backward_cub_clamps_at_zero() {
        let mut t = Terminal::new(80, 24, Theme::default());
        t.cursor_col = 2;
        feed(&mut t, b"\x1b[10D");
        assert_eq!(t.cursor_col, 0);
    }

    #[test]
    fn test_cursor_next_line_cnl() {
        let mut t = Terminal::new(80, 24, Theme::default());
        t.cursor_row = 3;
        t.cursor_col = 10;
        feed(&mut t, b"\x1b[2E");
        assert_eq!(t.cursor_row, 5);
        assert_eq!(t.cursor_col, 0, "CNL moves to column 0 after moving down");
    }

    #[test]
    fn test_cursor_previous_line_cpl() {
        let mut t = Terminal::new(80, 24, Theme::default());
        t.cursor_row = 5;
        t.cursor_col = 10;
        feed(&mut t, b"\x1b[2F");
        assert_eq!(t.cursor_row, 3);
        assert_eq!(t.cursor_col, 0, "CPL moves to column 0 after moving up");
    }

    #[test]
    fn test_cursor_key_mode_decckm() {
        let mut t = Terminal::new(80, 24, Theme::default());
        assert!(!t.application_cursor_mode());
        feed(&mut t, b"\x1b[?1h");
        assert!(t.application_cursor_mode());
        feed(&mut t, b"\x1b[?1l");
        assert!(!t.application_cursor_mode());
    }

    #[test]
    fn test_su_with_scroll_region() {
        let mut t = Terminal::new(80, 24, Theme::default());
        // Fill rows 0-5 with identifiable content
        for row in 0..6 {
            let ch = (b'A' + row as u8) as char;
            t.print(ch);
            t.cursor_col = 0;
            t.newline();
        }
        // Set scroll region to rows 1-4 (1-indexed: 2;5)
        feed(&mut t, b"\x1b[2;5r");
        // Move cursor into the region and scroll up 1
        t.cursor_row = 2;
        feed(&mut t, b"\x1b[1S");
        // Row 0 (outside region) should be unchanged ('A')
        assert_eq!(t.screen_cells()[0 * 80].ch.as_str(), "A");
        // Row 1 (top of region) should now have 'C' (scrolled from row 2)
        assert_eq!(t.screen_cells()[1 * 80].ch.as_str(), "C");
        // Row 4 (bottom of region) should be blank
        assert_eq!(t.screen_cells()[4 * 80].ch.as_str(), " ");
    }

    #[test]
    fn test_reverse_index_ri() {
        let mut t = Terminal::new(80, 24, Theme::default());
        t.cursor_row = 5;
        feed(&mut t, b"\x1bM"); // Reverse Index
        assert_eq!(t.cursor_row, 4);
    }

    #[test]
    fn test_reverse_index_ri_at_top_scrolls_down() {
        let mut t = Terminal::new(80, 24, Theme::default());
        t.print('X');
        t.cursor_row = 0;
        t.cursor_col = 0;
        feed(&mut t, b"\x1bM"); // RI at top of screen
        // Row 0 should have been scrolled down, cursor stays at 0
        assert_eq!(t.cursor_row, 0);
        // Content 'X' that was at row 0 should now be at row 1
        assert_eq!(t.screen_cells()[1 * 80].ch.as_str(), "X");
    }

    // ── フォーカスレポーティング(`?1004`、タスク#60) ─────────

    #[test]
    fn test_focus_reporting_mode_default_off() {
        let t = Terminal::new(80, 24, Theme::default());
        assert!(!t.focus_reporting_mode());
    }

    #[test]
    fn test_focus_reporting_mode_decset_decrst_1004() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1004h");
        assert!(t.focus_reporting_mode());
        feed(&mut t, b"\x1b[?1004l");
        assert!(!t.focus_reporting_mode());
    }

    #[test]
    fn test_focus_reporting_mode_reset_by_ris() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1004h");
        assert!(t.focus_reporting_mode());
        feed(&mut t, b"\x1bc"); // RIS
        assert!(!t.focus_reporting_mode(), "RISで既定のoffに戻る");
    }

    #[test]
    fn test_focus_reporting_mode_preserved_across_resize() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1004h");
        t.resize_preserving_state(40, 12);
        assert!(t.focus_reporting_mode());
    }

    #[test]
    fn test_encode_focus_event_off_mode_reports_nothing() {
        let t = Terminal::new(80, 24, Theme::default());
        assert_eq!(t.encode_focus_event(true), None);
        assert_eq!(t.encode_focus_event(false), None);
    }

    #[test]
    fn test_encode_focus_event_gained_and_lost() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1004h");
        assert_eq!(t.encode_focus_event(true), Some(b"\x1b[I".to_vec()));
        assert_eq!(t.encode_focus_event(false), Some(b"\x1b[O".to_vec()));
    }

    // ── Sixel(`DCS Pa;Pb;Ph q ... ST`、タスク#42) ─────────────

    #[test]
    fn test_sixel_places_image_at_cursor_and_advances_cursor_below_it() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[3;5H"); // カーソルを(row=2,col=4)へ(1-indexed入力)
        // 色0を赤で定義し、1ピクセルだけ描くsixel(1x1画像)。
        feed(&mut t, b"\x1bPq#0;2;100;0;0@\x1b\\");
        let images = t.images();
        assert_eq!(images.len(), 1);
        let img = &images[0];
        assert_eq!(img.row, 2);
        assert_eq!(img.col, 4);
        assert_eq!(img.width_px, 1);
        assert_eq!(img.height_px, 1);
        assert_eq!(img.rgba, vec![255, 0, 0, 255]);
        assert_eq!(img.rows_span, 1);
        assert_eq!(img.cols_span, 1);
        // 画像の下・左端へカーソルが移動する(実装ノート参照)。
        assert_eq!(t.cursor_row(), 3);
        assert_eq!(t.cursor_col(), 0);
    }

    #[test]
    fn test_sixel_image_gets_monotonically_increasing_ids() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1bPq#0;2;100;0;0@\x1b\\");
        feed(&mut t, b"\x1bPq#0;2;100;0;0@\x1b\\");
        let ids: Vec<u64> = t.images().iter().map(|i| i.id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids[1] > ids[0]);
    }

    #[test]
    fn test_sixel_image_span_clamped_to_remaining_screen_width() {
        // 名目セルサイズ(10px/col)換算で画面幅を大きく超える画像でも、
        // `cols_span`は画面の残り列数を超えない(スコープ外: 自動スクロールしない、
        // 画面右端でクリップする)。
        let mut t = Terminal::new(5, 24, Theme::default());
        feed(&mut t, b"\x1bPq#0;2;100;0;0!500@\x1b\\"); // 幅500pxの1バンド画像
        let images = t.images();
        assert_eq!(images.len(), 1);
        let img = &images[0];
        assert_eq!(img.width_px, 500);
        assert_eq!(img.cols_span, 5, "画面の残り列数(5)にクランプされること");
    }

    // ── Kitty graphics(APC、タスク#53) ─────────────
    // ApcInterceptorはsession_state.rs層にあるため、ここではdispatch_kitty_apcに
    // 抽出済みAPCペイロードを直接渡す(kitty_graphics.rsのKittyGraphics単体テストと
    // 同じ流儀)。

    #[test]
    fn test_kitty_delete_all_removes_placement_without_client_id() {
        // Opusレビュー指摘: `i=`(client image id)を送らない画像(chafa等が実際に
        // そうする)が`d=a`(全削除)で消せなかった不具合の回帰テスト。
        use base64::Engine;
        let mut t = Terminal::new(80, 24, Theme::default());
        let payload = format!(
            "Gf=32,s=1,v=1,a=T;{}",
            base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 4])
        );
        t.dispatch_kitty_apc(payload.as_bytes());
        assert_eq!(t.images().len(), 1, "i=無しでも画像は配置される");
        t.dispatch_kitty_apc(b"Ga=d,d=A");
        assert!(t.images().is_empty(), "i=無しの画像もd=aで削除されること");
    }

    #[test]
    fn test_kitty_delete_all_does_not_remove_sixel_images() {
        use base64::Engine;
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1bPq#0;2;100;0;0@\x1b\\"); // sixel画像を1枚配置
        let payload = format!(
            "Gf=32,s=1,v=1,a=T;{}",
            base64::engine::general_purpose::STANDARD.encode([9u8, 9, 9, 9])
        );
        t.dispatch_kitty_apc(payload.as_bytes());
        assert_eq!(t.images().len(), 2);
        t.dispatch_kitty_apc(b"Ga=d,d=A");
        assert_eq!(t.images().len(), 1, "Kittyのd=aはsixel画像を巻き込まない");
    }

    #[test]
    fn test_kitty_delete_by_id_removes_only_matching_placement() {
        use base64::Engine;
        let mut t = Terminal::new(80, 24, Theme::default());
        let img_a = format!(
            "Gf=32,s=1,v=1,a=T,i=1;{}",
            base64::engine::general_purpose::STANDARD.encode([1u8, 1, 1, 1])
        );
        let img_b = format!(
            "Gf=32,s=1,v=1,a=T,i=2;{}",
            base64::engine::general_purpose::STANDARD.encode([2u8, 2, 2, 2])
        );
        t.dispatch_kitty_apc(img_a.as_bytes());
        t.dispatch_kitty_apc(img_b.as_bytes());
        assert_eq!(t.images().len(), 2);
        t.dispatch_kitty_apc(b"Ga=d,d=I,i=1");
        let images = t.images();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].rgba, vec![2, 2, 2, 2]);
    }

    #[test]
    fn test_reset_clears_sixel_images() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1bPq#0;2;100;0;0@\x1b\\");
        assert_eq!(t.images().len(), 1);
        feed(&mut t, b"\x1bc"); // RIS
        assert!(t.images().is_empty());
    }

    #[test]
    fn test_scroll_clears_sixel_images() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1bPq#0;2;100;0;0@\x1b\\");
        assert_eq!(t.images().len(), 1);
        // 画面全体を埋めて自然にスクロールさせる(24行分の改行)。
        for _ in 0..24 {
            feed(&mut t, b"\r\n");
        }
        assert!(t.images().is_empty(), "スクロールで既存の画像配置は消去される");
    }

    #[test]
    fn test_alt_screen_switch_clears_sixel_images() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1bPq#0;2;100;0;0@\x1b\\");
        assert_eq!(t.images().len(), 1);
        feed(&mut t, b"\x1b[?1049h"); // alt screenへ
        assert!(t.images().is_empty());
    }

    #[test]
    fn test_resize_clears_sixel_images() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1bPq#0;2;100;0;0@\x1b\\");
        assert_eq!(t.images().len(), 1);
        t.resize_preserving_state(40, 12);
        assert!(t.images().is_empty());
    }

    #[test]
    fn test_full_screen_erase_clears_sixel_images() {
        // codexレビュー指摘: `CSI 2J`/`CSI 3J`(全画面消去、`clear(1)`が送る)は
        // テキストと一緒に画像配置も消えるべき。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1bPq#0;2;100;0;0@\x1b\\");
        assert_eq!(t.images().len(), 1);
        feed(&mut t, b"\x1b[2J");
        assert!(t.images().is_empty());
    }

    #[test]
    fn test_partial_erase_does_not_clear_sixel_images() {
        // ED0/ED1・EL等の部分消去はスコープ外(`ImagePlacement`のdocコメント参照)
        // であることを明示的に固定する回帰テスト。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1bPq#0;2;100;0;0@\x1b\\");
        assert_eq!(t.images().len(), 1);
        feed(&mut t, b"\x1b[0J"); // ED0
        feed(&mut t, b"\x1b[K");  // EL0
        assert_eq!(t.images().len(), 1, "部分消去では画像は消えない(スコープ外)");
    }

    #[test]
    fn test_non_sixel_dcs_is_ignored() {
        // 最終バイトが`q`以外のDCS(未対応のサブプロトコル)は無視され、画像は
        // 作られない(黙って破棄する従来通りの挙動)。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1bP1$tsomething\x1b\\");
        assert!(t.images().is_empty());
    }

    #[test]
    fn test_primary_da_advertises_sixel_support() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[c");
        let resp = t.take_pending_terminal_responses();
        assert_eq!(resp, vec![b"\x1b[?1;2;4c".to_vec()], "DA1応答に';4'(Sixel対応)が含まれること");
    }

    #[test]
    fn test_sixel_total_rgba_bytes_capped_even_when_under_max_live_images() {
        // タスク#90再現条件: 約2000x2000px(sixel::MAX_SIXEL_AREA相当、RGBA8888で
        // 16MiB)の画像を`MAX_LIVE_IMAGES`(32)回連続で置くと、個数キャップだけ
        // では理論上16MiB×32=512MiBまで増え得る。`place_sixel_image`を直接
        // 呼び(実際のsixelバイト列エンコードは不要——寿命管理ロジックの検証が
        // 目的)、合計バイト数が`MAX_TOTAL_IMAGE_RGBA_BYTES`(64MiB)を上回らない
        // ことを確認する。
        let mut t = Terminal::new(80, 24, Theme::default());
        let width = 2000usize;
        let height = 2000usize;
        assert_eq!(width * height, 4_000_000, "テストの前提: 2000x2000pxはsixel::MAX_SIXEL_AREA(4_000_000)と一致する");
        for _ in 0..32 {
            t.place_sixel_image(SixelImage {
                width,
                height,
                rgba: vec![0u8; width * height * 4],
            });
        }
        let total: usize = t.images().iter().map(|img| img.rgba.len()).sum();
        assert!(
            total <= MAX_TOTAL_IMAGE_RGBA_BYTES,
            "合計RGBAバイト数がMAX_TOTAL_IMAGE_RGBA_BYTES({MAX_TOTAL_IMAGE_RGBA_BYTES})を超えている: {total}"
        );
        assert!(t.images().len() < 32, "総バイト数キャップにより個数キャップ(32)未満まで捨てられているはず");
    }

    #[test]
    fn test_sixel_total_rgba_bytes_evicts_oldest_first() {
        // 総バイト数キャップに達したら最も古い画像から捨てる(`MAX_LIVE_IMAGES`
        // 超過時の1枚捨てと同じ「古いものを優先して捨てる」ポリシー)ことを、
        // idの単調増加で確認する。
        let mut t = Terminal::new(80, 24, Theme::default());
        let width = 2000usize;
        let height = 2000usize;
        for _ in 0..8 {
            t.place_sixel_image(SixelImage { width, height, rgba: vec![0u8; width * height * 4] });
        }
        let ids: Vec<u64> = t.images().iter().map(|img| img.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "残っている画像は古い順(id昇順)のまま並んでいるはず");
        // 最初に置いた画像(id最小)は総バイト数キャップにより既に捨てられている。
        assert!(!ids.contains(&0), "最も古い画像(id=0)は総バイト数キャップで捨てられているはず");
    }

    #[test]
    fn test_sixel_small_images_not_evicted_by_byte_cap_within_max_live_images() {
        // 小さい画像であれば総バイト数キャップに達しないため、
        // `MAX_LIVE_IMAGES`(個数キャップ)の挙動がそのまま保たれることを確認する
        // (総バイト数キャップの追加が既存の個数キャップの挙動を壊していないこと)。
        let mut t = Terminal::new(80, 24, Theme::default());
        for _ in 0..40 {
            t.place_sixel_image(SixelImage { width: 1, height: 1, rgba: vec![0, 0, 0, 255] });
        }
        assert_eq!(t.images().len(), MAX_LIVE_IMAGES, "小さい画像では従来通りMAX_LIVE_IMAGES(32)枚に収まること");
    }

    // ── OSC 133 セマンティックプロンプト(タスク#13) ──────────

    #[test]
    fn test_osc133_abcd_sequence_does_not_affect_screen_content() {
        // OSC 133自体は不可視のマーカーであり、画面内容(セル)には一切影響しない。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n");
        feed(&mut t, b"\x1b]133;C\x07file.txt\r\n");
        feed(&mut t, b"\x1b]133;D;0\x07");
        assert_eq!(cell(&t, 0, 0), "$");
        assert_eq!(cell(&t, 0, 2), "l");
        assert_eq!(cell(&t, 1, 0), "f");
    }

    #[test]
    fn test_osc133_jump_to_previous_prompt_finds_prompt_pushed_into_scrollback() {
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"\x1b]133;A\x07$ cmd1\r\n");
        // 十分な行を流してこのプロンプトをscrollbackへ押し出す(rows=3の端末で
        // これだけ流せば必ずスクロールする)。
        feed(&mut t, b"a\r\nb\r\nc\r\nd\r\n");
        assert!(t.total_scrolled_lines > 0);

        // ライブ画面表示中(showing_scrollback=false)から「前」へジャンプすると、
        // scrollbackへ押し出されたこのプロンプトが見つかる。
        let target = t.prompt_jump_target(true, 0, false, t.total_scrolled_lines as u32);
        let target = target.expect("scrollbackへ押し出されたプロンプトが見つかるはず");
        assert!(!target.is_live);
    }

    #[test]
    fn test_osc133_jump_previous_among_multiple_live_prompts_finds_nearest_before_cursor() {
        // 端末が十分大きく(rows=10)、2つのプロンプトが両方ともスクロールせずライブ画面上に
        // 同時に見えているケース。カーソル位置(現在位置)より前にある直近のプロンプトが
        // 見つかることを確認する(単に「ライブ画面最上行より上か」だけでは、この
        // ケースを正しく扱えない——docコメント参照)。
        let mut t = Terminal::new(10, 10, Theme::default());
        feed(&mut t, b"\x1b]133;A\x07$ cmd1\r\n"); // mark row=0
        feed(&mut t, b"\x1b]133;A\x07$ cmd2\r\n"); // mark row=1、カーソルはこの後row=2
        assert_eq!(t.total_scrolled_lines, 0, "rows=10なのでこのテストでは一切スクロールしない前提");

        let prev = t.prompt_jump_target(true, 0, false, 0)
            .expect("カーソル位置より前の直近プロンプト(cmd2)が見つかるはず");
        assert!(prev.is_live);
        assert_eq!(prev.scroll_offset, 0, "is_live=trueの場合scroll_offsetは意味を持たない(0固定)");
    }

    #[test]
    fn test_osc133_jump_next_falls_back_to_live_when_no_later_prompt_exists() {
        // scrollback表示中(showing_scrollback=true)に「次」を呼んだが、それより後の
        // プロンプトマークが1つも無い場合は、ライブ画面へ「戻る」ジャンプ先を返す。
        let t = Terminal::new(10, 5, Theme::default());
        let next = t.prompt_jump_target(false, 0, true, 0)
            .expect("ライブへ戻るフォールバックが返るはず");
        assert!(next.is_live);
    }

    #[test]
    fn test_osc133_jump_next_returns_none_when_already_live_and_no_later_prompt() {
        // ライブ画面表示中(showing_scrollback=false)に「次」を呼んだ場合、フォール
        // バックは行わない(既にライブより後は無いため)。
        let t = Terminal::new(10, 5, Theme::default());
        assert!(t.prompt_jump_target(false, 0, false, 0).is_none());
    }

    #[test]
    fn test_osc133_jump_previous_returns_none_when_no_earlier_prompt() {
        // カーソルがまだそのプロンプト自身の行にいる(=マーク直後、何も出力していない)
        // 場合、「前」は見つからない——前の絶対行番号(reference)自体がこのマークの行と
        // 同じで、`want_previous`の`r < reference`(厳密未満)を満たさないため。
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"\x1b]133;A\x07");
        assert!(t.prompt_jump_target(true, 0, false, 0).is_none());
    }

    #[test]
    fn test_osc133_jump_next_returns_none_when_already_live_with_no_more_prompts() {
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"\x1b]133;A\x07$ only\r\n");
        assert!(t.prompt_jump_target(false, 0, false, 0).is_none());
    }

    #[test]
    fn test_osc133_jump_falls_back_to_command_start_when_prompt_start_absent() {
        // 一部のシェルはAを送らずBのみを送る——その場合Bをアンカーとして使う。
        let mut t = Terminal::new(10, 5, Theme::default());
        feed(&mut t, b"\x1b]133;B\x07$ cmd1\r\n");
        feed(&mut t, b"\x1b]133;B\x07$ cmd2\r\n");
        let prev = t.prompt_jump_target(true, 0, false, 0);
        assert!(prev.is_some(), "Aが無くてもBをアンカーとしてジャンプできるはず");
    }

    #[test]
    fn test_osc133_jump_unreachable_evicted_row_is_skipped() {
        // scrollback_lenを実際より小さく(=evicted扱いに)すると、scrollbackへ既に
        // 押し出された唯一の候補がジャンプ対象から外れる。
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"\x1b]133;A\x07$ cmd1\r\n");
        feed(&mut t, b"a\r\nb\r\nc\r\nd\r\n"); // mark1をscrollbackへ押し出す
        assert!(t.total_scrolled_lines > 0, "rows=3の端末でこれだけ流せば必ずスクロールする");

        // scrollback_len=0は「1つ目のプロンプト行は既にscrollbackから追い出された」
        // ことを意味する。
        assert!(t.prompt_jump_target(true, 0, false, 0).is_none());
        // 十分なscrollback_lenを渡せば、同じ絶対行番号のマークが見つかる。
        let target = t.prompt_jump_target(true, 0, false, t.total_scrolled_lines as u32);
        assert!(target.is_some(), "scrollback_lenが十分ならreachableと判定されるはず");
    }

    #[test]
    fn test_osc133_click_moves_cursor_forward_on_active_input_line() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]133;B\x07ab");
        // カーソルは(0,2)。同じ行の列5をタップ -> 3回分right矢印。
        let bytes = t.cursor_move_bytes_for_click(0, 5).expect("入力行上のタップは有効なはず");
        assert_eq!(bytes, b"\x1b[C\x1b[C\x1b[C".to_vec());
    }

    #[test]
    fn test_osc133_click_moves_cursor_backward_on_active_input_line() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]133;B\x07abcde");
        // カーソルは(0,5)。列1をタップ -> 4回分left矢印。
        let bytes = t.cursor_move_bytes_for_click(0, 1).expect("入力行上のタップは有効なはず");
        assert_eq!(bytes, b"\x1b[D\x1b[D\x1b[D\x1b[D".to_vec());
    }

    #[test]
    fn test_osc133_click_uses_ss3_form_when_application_cursor_mode_active() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b[?1h"); // DECCKM on
        feed(&mut t, b"\x1b]133;B\x07ab");
        let bytes = t.cursor_move_bytes_for_click(0, 3).expect("入力行上のタップは有効なはず");
        assert_eq!(bytes, b"\x1bOC".to_vec());
    }

    #[test]
    fn test_osc133_click_is_noop_before_command_start() {
        // OSC 133;Bがまだ来ていない(=入力行がアクティブでない)状態のタップは無視する。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"ab");
        assert!(t.cursor_move_bytes_for_click(0, 0).is_none());
    }

    #[test]
    fn test_osc133_click_is_noop_after_command_executed() {
        // ;Cが来た後(出力フェーズ)はもう入力行ではないので無視する。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]133;B\x07ab\x1b]133;C\x07");
        assert!(t.cursor_move_bytes_for_click(0, 0).is_none());
    }

    #[test]
    fn test_osc133_click_is_noop_on_a_different_row() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]133;B\x07ab");
        assert!(t.cursor_move_bytes_for_click(1, 0).is_none(), "カーソル行以外のタップは無視するはず");
    }

    #[test]
    fn test_osc133_click_is_noop_when_tapped_column_equals_cursor_column() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]133;B\x07ab");
        assert!(t.cursor_move_bytes_for_click(0, 2).is_none(), "移動量ゼロは送信しない");
    }

    #[test]
    fn test_osc133_copy_last_command_output_single_line() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]133;C\x07file.txt\r\n");
        feed(&mut t, b"\x1b]133;D;0\x07");
        assert_eq!(t.last_command_output_text(), Some("file.txt".to_string()));
    }

    #[test]
    fn test_osc133_copy_last_command_output_multi_line_joins_with_newline() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]133;C\x07line1\r\nline2\r\n");
        feed(&mut t, b"\x1b]133;D\x07");
        assert_eq!(t.last_command_output_text(), Some("line1\nline2".to_string()));
    }

    #[test]
    fn test_osc133_copy_last_command_output_none_before_any_command_finished() {
        let t = Terminal::new(80, 24, Theme::default());
        assert_eq!(t.last_command_output_text(), None);
    }

    #[test]
    fn test_osc133_copy_last_command_output_is_overwritten_by_newer_command() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]133;C\x07old\r\n");
        feed(&mut t, b"\x1b]133;D\x07");
        feed(&mut t, b"\x1b]133;C\x07new\r\n");
        feed(&mut t, b"\x1b]133;D\x07");
        assert_eq!(t.last_command_output_text(), Some("new".to_string()), "直前の1件のみを保持する");
    }

    #[test]
    fn test_osc133_copy_last_command_output_ignores_bytes_outside_c_to_d_window() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]133;A\x07prompt-not-output\r\n");
        feed(&mut t, b"\x1b]133;C\x07actual-output\r\n");
        feed(&mut t, b"\x1b]133;D\x07next-prompt-not-output\r\n");
        assert_eq!(t.last_command_output_text(), Some("actual-output".to_string()));
    }

    #[test]
    fn test_osc133_d_exit_code_parses_when_present_and_numeric() {
        // exit codeそのものは`last_command_output_text`からは見えないが、パース失敗
        // (数値以外)でもpanicしない/後続の処理を壊さないことを確認する。
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]133;C\x07out\r\n");
        feed(&mut t, b"\x1b]133;D;127\x07");
        assert_eq!(t.last_command_output_text(), Some("out".to_string()));
    }

    #[test]
    fn test_osc133_d_with_non_numeric_extra_param_does_not_panic() {
        let mut t = Terminal::new(80, 24, Theme::default());
        feed(&mut t, b"\x1b]133;C\x07out\r\n");
        feed(&mut t, b"\x1b]133;D;aid=1\x07");
        assert_eq!(t.last_command_output_text(), Some("out".to_string()));
    }

    #[test]
    fn test_osc133_reset_all_clears_transient_state_but_keeps_history_addressable() {
        let mut t = Terminal::new(10, 3, Theme::default());
        feed(&mut t, b"\x1b]133;A\x07$ cmd1\r\n");
        feed(&mut t, b"a\r\nb\r\nc\r\nd\r\n"); // cmd1をscrollbackへ押し出す
        feed(&mut t, b"\x1b]133;B\x07partial-input");
        feed(&mut t, b"\x1bc"); // RIS
        // RIS後は「今入力中」ではなくなる(タップは無効化)。
        assert!(t.cursor_move_bytes_for_click(0, 0).is_none());
        // だが過去のプロンプト(scrollbackへ押し出し済み)への絶対行番号ベースの
        // ジャンプ判断は壊れない(`total_scrolled_lines`/`prompt_marks`はRISで
        // クリアしないため)。
        let target = t.prompt_jump_target(true, 0, false, t.total_scrolled_lines as u32);
        assert!(target.is_some());
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
