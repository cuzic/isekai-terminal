//! Sixel(DCS `Pa;Pb;Ph q <data> ST`)デコーダ(タスク#42)。
//!
//! `vte`はDCSシーケンスを`hook`/`put`/`unhook`で正しく配送する(#53のKitty graphics
//! と違いAPC未配送問題は無い、Fable 2次レビュー確認済み)。このモジュールは
//! `put()`で1バイトずつ届く生のsixelデータ本体を、`Terminal`から独立して
//! デコードすることに専念する(パーサー状態機械そのものにテストしやすさを持たせる
//! ため`terminal.rs`とは別ファイルに分離)。実際の画面上への配置・寿命管理は
//! `terminal.rs::Terminal::place_sixel_image`が担う(rust-ssot: 判断ロジックは
//! すべてRust側に置き、Android/iOSは`ImagePlacement`が指す矩形へ`rgba`を
//! そのまま描画するだけでよい)。
//!
//! ## 実装範囲(スコープ外の明記)
//! - ラスタ属性(`"Pan;Pad;Ph;Pv`)は構文として消費するのみで、画像サイズの
//!   事前確保には使わない(実データから動的に幅・高さを確定させるため不要)。
//! - 背景色選択(Pb)は区別しない — 未設定ピクセルは常に透過(alpha=0)として扱う
//!   (Pb=1の「透過」相当の挙動に統一。Pb=0の「デバイス既定背景色で塗る」は
//!   非対応)。
//! - DEC HLSカラー(`#Pc;1;H;L;S`)のHue原点はDEC実機とCSS HSLとで厳密には
//!   異なる(DECは0°=青起点)が、標準HSL→RGB変換で近似する(多くの現代
//!   ターミナル実装が採る簡略化)。
//! - 巨大な/悪意ある画像サイズによるメモリ枯渇を防ぐため、`MAX_SIXEL_DIM`
//!   (1軸あたり)・`MAX_SIXEL_AREA`(バウンディングボックス概算)で打ち切る
//!   (打ち切られた場合、その画像は破棄され画面には何も配置されない —
//!   中途半端な壊れた画像を表示するより安全側に倒す)。

use std::collections::HashMap;

/// 1軸(幅/高さ)あたりの上限(ピクセル)。これを超えたら即座にデコードを諦める。
/// Kitty graphics(`kitty_graphics.rs`、#53)も同じ上限を共有する。
pub(crate) const MAX_SIXEL_DIM: usize = 4096;
/// バウンディングボックス面積(幅×高さ)の概算上限。ジャグ配列のため正確な
/// ピクセル数ではないが、モバイル端末で許容できるメモリ量(4M pixel * 4bytes
/// = 16MiB)へ確実に収めるための粗いガード。Kitty graphicsも共有する。
pub(crate) const MAX_SIXEL_AREA: usize = 4_000_000;

pub(crate) struct SixelImage {
    pub(crate) width: usize,
    pub(crate) height: usize,
    /// RGBA8888、row-major、左上原点。`width * height * 4`バイト。
    pub(crate) rgba: Vec<u8>,
}

/// パーサーの内部状態。数値パラメータをまたぐバイト列(`#Pc;Pu;Px;Py;Pz`等)を
/// `put()`の1バイトずつの呼び出しをまたいで組み立てるための状態機械。
enum Mode {
    Normal,
    /// `!`の後、繰り返し回数の10進数字を読み取り中。次に来る非数字バイトが
    /// 「繰り返すsixel文字」そのもの。
    Repeat { count: u32 },
    /// `#`の後、色レジスタ番号(Pc)の10進数字を読み取り中。
    ColorNum { num: u32 },
    /// `#Pc;`の後、`Pu;Px;Py;Pz`をカンマ区切りで読み取り中。
    ColorSpec { pc: u32, params: Vec<u32>, cur: u32, has_digit: bool },
    /// `"`の後、`Pan;Pad;Ph;Pv`を読み取り中(値自体は使わない、構文の消費のみ)。
    Raster { cur: u32 },
}

pub(crate) struct SixelDecoder {
    palette: HashMap<u16, (u8, u8, u8)>,
    current_color: u16,
    x: usize,
    /// 現在のsixelバンド(6ピクセル高)の先頭y座標。
    y: usize,
    /// ジャグ配列(各行が独立に伸びる)。`rows[y][x]`はARGB
    /// (`0xAARRGGBB`、alpha=0=未設定/透過)。
    rows: Vec<Vec<u32>>,
    mode: Mode,
    /// サイズ上限超過等で以降のバイトを無視して良い状態になったら立てる。
    aborted: bool,
}

fn pct_to_u8(p: u32) -> u8 {
    let p = p.min(100);
    ((p * 255 + 50) / 100) as u8
}

/// DEC HLS(`Pu=1`、H:0-360, L/S:0-100)を標準HSLとして近似的にRGBへ変換する
/// (モジュールdocの「実装範囲」参照)。
fn hls_to_rgb(h: u32, l: u32, s: u32) -> (u8, u8, u8) {
    let h = (h % 360) as f32;
    let l = (l.min(100) as f32) / 100.0;
    let s = (s.min(100) as f32) / 100.0;
    if s <= 0.0 {
        let v = (l * 255.0).round().clamp(0.0, 255.0) as u8;
        return (v, v, v);
    }
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let conv = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (conv(r1), conv(g1), conv(b1))
}

/// VT340由来の既定16色sixelパレット(色番号0〜15)。値は広く使われている
/// libsixel/xterm実装の既定テーブルに準拠(百分率をRGBへ変換)。
fn default_palette() -> HashMap<u16, (u8, u8, u8)> {
    let pct = |r: u32, g: u32, b: u32| (pct_to_u8(r), pct_to_u8(g), pct_to_u8(b));
    HashMap::from([
        (0, pct(0, 0, 0)),
        (1, pct(20, 20, 80)),
        (2, pct(80, 13, 13)),
        (3, pct(20, 80, 20)),
        (4, pct(80, 20, 80)),
        (5, pct(20, 80, 80)),
        (6, pct(80, 80, 20)),
        (7, pct(53, 53, 53)),
        (8, pct(26, 26, 26)),
        (9, pct(33, 33, 60)),
        (10, pct(60, 26, 26)),
        (11, pct(33, 60, 33)),
        (12, pct(60, 33, 60)),
        (13, pct(33, 60, 60)),
        (14, pct(60, 60, 33)),
        (15, pct(80, 80, 80)),
    ])
}

impl SixelDecoder {
    pub(crate) fn new() -> Self {
        SixelDecoder {
            palette: default_palette(),
            current_color: 0,
            x: 0,
            y: 0,
            rows: Vec::new(),
            mode: Mode::Normal,
            aborted: false,
        }
    }

    pub(crate) fn feed(&mut self, byte: u8) {
        if self.aborted {
            return;
        }
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        self.mode = match mode {
            Mode::Normal => self.step_normal(byte),
            Mode::Repeat { count } => self.step_repeat(byte, count),
            Mode::ColorNum { num } => self.step_color_num(byte, num),
            Mode::ColorSpec { pc, params, cur, has_digit } => {
                self.step_color_spec(byte, pc, params, cur, has_digit)
            }
            Mode::Raster { cur } => self.step_raster(byte, cur),
        };
    }

    fn step_normal(&mut self, byte: u8) -> Mode {
        match byte {
            b'#' => Mode::ColorNum { num: 0 },
            b'!' => Mode::Repeat { count: 0 },
            b'"' => Mode::Raster { cur: 0 },
            b'-' => {
                self.y += 6;
                self.x = 0;
                Mode::Normal
            }
            b'$' => {
                self.x = 0;
                Mode::Normal
            }
            0x3f..=0x7e => {
                self.draw_sixel_char(byte);
                Mode::Normal
            }
            _ => Mode::Normal,
        }
    }

    fn step_repeat(&mut self, byte: u8, count: u32) -> Mode {
        if byte.is_ascii_digit() {
            return Mode::Repeat { count: count.saturating_mul(10).saturating_add((byte - b'0') as u32) };
        }
        let times = if count == 0 { 1 } else { count }.min(MAX_SIXEL_DIM as u32);
        if (0x3f..=0x7e).contains(&byte) {
            for _ in 0..times {
                self.draw_sixel_char(byte);
                if self.aborted {
                    break;
                }
            }
            Mode::Normal
        } else {
            // 仕様上ありえない並び(`!`直後がsixel文字でない)。無視して通常状態へ戻り、
            // このバイト自体は再度Normalとして解釈する。
            self.step_normal(byte)
        }
    }

    fn step_color_num(&mut self, byte: u8, num: u32) -> Mode {
        if byte.is_ascii_digit() {
            return Mode::ColorNum { num: num.saturating_mul(10).saturating_add((byte - b'0') as u32) };
        }
        if byte == b';' {
            return Mode::ColorSpec { pc: num, params: Vec::new(), cur: 0, has_digit: false };
        }
        self.current_color = num as u16;
        self.step_normal(byte)
    }

    fn step_color_spec(&mut self, byte: u8, pc: u32, mut params: Vec<u32>, cur: u32, has_digit: bool) -> Mode {
        if byte.is_ascii_digit() {
            return Mode::ColorSpec {
                pc,
                params,
                cur: cur.saturating_mul(10).saturating_add((byte - b'0') as u32),
                has_digit: true,
            };
        }
        if byte == b';' {
            params.push(if has_digit { cur } else { 0 });
            return Mode::ColorSpec { pc, params, cur: 0, has_digit: false };
        }
        params.push(if has_digit { cur } else { 0 });
        self.define_and_select_color(pc as u16, &params);
        self.step_normal(byte)
    }

    fn step_raster(&mut self, byte: u8, cur: u32) -> Mode {
        if byte.is_ascii_digit() {
            return Mode::Raster { cur: cur.saturating_mul(10).saturating_add((byte - b'0') as u32) };
        }
        if byte == b';' {
            return Mode::Raster { cur: 0 };
        }
        // Pan/Pad/Ph/Pvの値は使わない(モジュールdoc参照)。構文だけ消費してNormalへ戻る。
        self.step_normal(byte)
    }

    fn define_and_select_color(&mut self, pc: u16, params: &[u32]) {
        self.current_color = pc;
        if params.len() < 4 {
            return;
        }
        let rgb = match params[0] {
            2 => (pct_to_u8(params[1]), pct_to_u8(params[2]), pct_to_u8(params[3])),
            1 => hls_to_rgb(params[1], params[2], params[3]),
            _ => return,
        };
        self.palette.insert(pc, rgb);
    }

    fn current_argb(&self) -> u32 {
        let (r, g, b) = self.palette.get(&self.current_color).copied().unwrap_or((255, 255, 255));
        0xFF000000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
    }

    fn draw_sixel_char(&mut self, byte: u8) {
        let bits = byte - 0x3f;
        let argb = self.current_argb();
        for i in 0..6u32 {
            if bits & (1 << i) != 0 {
                self.set_pixel(self.x, self.y + i as usize, argb);
                if self.aborted {
                    return;
                }
            }
        }
        self.x += 1;
        if self.x >= MAX_SIXEL_DIM {
            self.aborted = true;
        }
    }

    fn set_pixel(&mut self, x: usize, y: usize, argb: u32) {
        if x >= MAX_SIXEL_DIM || y >= MAX_SIXEL_DIM {
            self.aborted = true;
            return;
        }
        if (x + 1).saturating_mul(y + 1) > MAX_SIXEL_AREA {
            self.aborted = true;
            return;
        }
        while self.rows.len() <= y {
            self.rows.push(Vec::new());
        }
        let row = &mut self.rows[y];
        if row.len() <= x {
            row.resize(x + 1, 0);
        }
        row[x] = argb;
    }

    /// デコードを終了し、確定したビットマップを返す。サイズ超過で中断された場合・
    /// 1ピクセルも描画されなかった場合は`None`(呼び出し元は何も配置しない)。
    pub(crate) fn finish(self) -> Option<SixelImage> {
        if self.aborted {
            return None;
        }
        let height = self.rows.len();
        let width = self.rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if width == 0 || height == 0 {
            return None;
        }
        let mut rgba = vec![0u8; width * height * 4];
        for (y, row) in self.rows.iter().enumerate() {
            for x in 0..width {
                let argb = row.get(x).copied().unwrap_or(0);
                let idx = (y * width + x) * 4;
                rgba[idx] = ((argb >> 16) & 0xff) as u8;
                rgba[idx + 1] = ((argb >> 8) & 0xff) as u8;
                rgba[idx + 2] = (argb & 0xff) as u8;
                rgba[idx + 3] = ((argb >> 24) & 0xff) as u8;
            }
        }
        Some(SixelImage { width, height, rgba })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_str(dec: &mut SixelDecoder, s: &str) {
        for b in s.bytes() {
            dec.feed(b);
        }
    }

    #[test]
    fn all_zero_bits_sixel_char_produces_no_image() {
        let mut dec = SixelDecoder::new();
        // '?'(0x3f)はbits=0(全ビット未設定)なので、どのピクセルも実際にはセット
        // されない。1バイト分xは進むが、set_pixelが一度も呼ばれずrowsが空のまま
        // なので画像自体が確定しない。
        feed_str(&mut dec, "#1?");
        assert!(dec.finish().is_none());
    }

    #[test]
    fn sets_pixel_from_bitmask() {
        let mut dec = SixelDecoder::new();
        // '@' = 0x3f + 1 = bit0のみ→ y=0の1ピクセルが立つ。
        feed_str(&mut dec, "#0;2;100;0;0@");
        let img = dec.finish().expect("image");
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        assert_eq!(&img.rgba, &[255, 0, 0, 255]);
    }

    #[test]
    fn repeat_expands_run_horizontally() {
        let mut dec = SixelDecoder::new();
        feed_str(&mut dec, "#0;2;0;100;0!3@");
        let img = dec.finish().expect("image");
        assert_eq!(img.width, 3);
        assert_eq!(img.height, 1);
        for i in 0..3 {
            let idx = i * 4;
            assert_eq!(&img.rgba[idx..idx + 4], &[0, 255, 0, 255]);
        }
    }

    #[test]
    fn newline_moves_to_next_band() {
        let mut dec = SixelDecoder::new();
        // 1行目に1ピクセル、`-`で次の6ピクセルバンドへ、2行目(y=6)にもう1ピクセル。
        feed_str(&mut dec, "#0;2;100;0;0@-@");
        let img = dec.finish().expect("image");
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 7);
        let idx0 = 0 * 4;
        let idx6 = 6 * 4;
        assert_eq!(&img.rgba[idx0..idx0 + 4], &[255, 0, 0, 255]);
        assert_eq!(&img.rgba[idx6..idx6 + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn carriage_return_resets_column_without_new_band() {
        let mut dec = SixelDecoder::new();
        feed_str(&mut dec, "#0;2;100;0;0@$@");
        let img = dec.finish().expect("image");
        // '$'はxを0へ戻すだけなので2文字目は同じ列(x=0)に上書きされ、幅1のまま。
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
    }

    #[test]
    fn undefined_color_register_falls_back_to_white() {
        let mut dec = SixelDecoder::new();
        feed_str(&mut dec, "#99@");
        let img = dec.finish().expect("image");
        assert_eq!(&img.rgba, &[255, 255, 255, 255]);
    }

    #[test]
    fn raster_attributes_are_consumed_without_affecting_pixels() {
        let mut dec = SixelDecoder::new();
        feed_str(&mut dec, "\"1;1;10;10#0;2;100;0;0@");
        let img = dec.finish().expect("image");
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        assert_eq!(&img.rgba, &[255, 0, 0, 255]);
    }

    #[test]
    fn empty_input_produces_no_image() {
        let dec = SixelDecoder::new();
        assert!(dec.finish().is_none());
    }

    #[test]
    fn oversized_image_is_aborted() {
        let mut dec = SixelDecoder::new();
        // x を MAX_SIXEL_DIM 超まで一気に進める(repeatで多数の空文字'?'相当は
        // ビット無しなのでxが進まないため、代わりに'@'を大量repeatする)。
        feed_str(&mut dec, "!5000@");
        assert!(dec.finish().is_none());
    }
}
