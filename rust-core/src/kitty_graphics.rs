//! Kitty graphics protocol(APC `ESC _ G <key>=<value>(,…) ; <base64> ESC \`、#53)
//! デコーダ。`sixel.rs`の兄弟モジュール——同じ`ImagePlacement`データフローに乗せ、
//! 実際の画面配置・寿命管理は`terminal.rs`側が担う(rust-ssot: 判断ロジックは
//! すべてRust側に集約し、Android/iOSは`ImagePlacement`が指す矩形へ`rgba`をそのまま
//! 描画するだけでよい)。
//!
//! ## なぜAPCを自前で切り出すのか
//! `vte` 0.13は`ESC _ … ST`(APC文字列)の中身を状態機械テーブル上すべて`Ignore`に
//! 落とし、`Perform`にAPC配送用フックを持たない(`sixel.rs`が使うDCSの
//! `hook`/`put`/`unhook`とは対照的)。そのためKitty graphicsのAPC本体はvteに渡す前に
//! [ApcInterceptor]がバイトストリームから抜き取り、完成したペイロードだけを
//! `Terminal::dispatch_kitty_apc`へ渡す(それ以外のバイトはバイト等価でvteへ素通し
//! するため、DCS/sixelやCSI等の既存シーケンス処理には一切干渉しない)。
//!
//! ## 既知の限界(不正/非整形入力時のみ)
//! `ESC _`を無条件に横取りするため、**整形式でないバイト列**では既存シーケンスと干渉し
//! うる(実在のKitty/正常なサーバーが生成しないケースなので対象外・コードでは対処しない):
//! - 直前の制御文字列(OSC等)がSTで閉じられないまま`ESC _`で中断された場合、その未終端
//!   文字列のvte側状態は取り残される(例: `ESC ] 8;;URL ESC _ …`)。
//! - APC直前に**偶数個**の余分なESCがあると、素通し後にvte側のエスケープ終端解釈がずれる
//!   (例: `ESC ESC _ <apc> ST Z` → vteは`ESC Z`=DECIDと解釈しうる)。
//!
//! ## 実装範囲(MVP、スコープ外を明記)
//! 対応する:
//! - 転送形式 `f=32`(RGBA)・`f=24`(RGB)の生ピクセル(依存追加不要)。
//! - 転送形式 `f=100`(PNG)。実ツール(`chafa --format=kitty`・`kitty +kitten icat`)は
//!   既定でPNGを送るため、生RGBAのみでは実用的な互換性が得られない。純Rustの`png`
//!   crate(C依存なし、Androidクロスコンパイル可)を足す価値ありと判断した。
//! - `m=1`/`m=0`によるチャンク分割転送(大きな画像を複数APCに分けて送る実ツールの挙動)。
//! - `a=T`(転送して即表示)。カーソル位置へ配置する(sixelと同じ配置意味論)。
//! - `a=d`(削除): `d=a`/`d=A`/裸の`d`(=全Kitty画像削除)と、`d=i`/`d=I`+`i=<id>`
//!   (client image id指定削除)のみ。
//!
//! スコープ外(`terminal.rs`のsixel節と同じく明示):
//! - `a=t`(転送のみ・非表示): デコードはするが表示せず、後から`a=p`で置くための画像
//!   ストアは持たない(下記のとおり`a=p`自体が対象外)ため実質no-op。
//! - `a=p`(既存画像の再配置)・`a=f`/`a=a`(アニメーション/複数フレーム)。
//! - Unicodeプレースホルダ/仮想配置(`U=1`、U+10EEEE)——テキストと一緒に画像を
//!   スクロールさせる新方式。sixelも本コアではスクロール時に画像を消す挙動なので、
//!   Kittyも同様にスクロールで消える(`ImagePlacement`のdocコメント参照、挙動を揃える)。
//! - 8bit C1形式のAPC導入子(`0x9f`)は認識しない——7bitの`ESC _`のみを起点とする。
//!   `0x9f`はUTF-8の継続バイト範囲(0x80-0xBF)に重なり、vteによるUTF-8デコード前の
//!   生バイト段でこれをAPC開始と見なすと日本語等のマルチバイト文字を破壊しうるため
//!   (UTF-8前提のSSHセッションでC1導入子が正当に来ることは実質無い)。なお終端側の
//!   C1 ST(`0x9c`)は受理する——APC本体はbase64(ASCII)なので途中に生の`0x9c`が正当に
//!   現れることはなく曖昧さが無いため、導入子とは扱いを分ける。
//! - 共有メモリ/ファイル転送(`t=s`/`t=f`/`t=t`)。既定の直接転送(`t=d`、APC内base64)のみ。
//! - `d=`の位置指定・z-index範囲指定などの削除バリアント。
//! - z-index合成は「後勝ち」のみ(多層合成なし)。
//! - 巨大/悪意ある画像によるメモリ枯渇は、sixelと同じ`MAX_SIXEL_DIM`/`MAX_SIXEL_AREA`
//!   で打ち切る(超過時はその画像を破棄し画面には何も置かない)。

use base64::Engine;
use crate::sixel::{MAX_SIXEL_AREA, MAX_SIXEL_DIM};

/// 1つのAPCペイロード(base64展開後)の上限バイト数。単一APCにチャンク分割せず
/// 巨大なPNG/生RGBAを流し込まれてもメモリが際限なく増えないための粗いガード。
/// これを超えたチャンクは以降のバイトを捨てる(結果としてデコードは失敗し画像は破棄)。
const MAX_APC_PAYLOAD: usize = 32 * 1024 * 1024;

/// [ApcInterceptor::feed]の1バイト処理結果。
pub(crate) enum ApcStep {
    /// このバイトをそのままvteパーサーへ渡す。
    Pass(u8),
    /// 2バイトを順にvteへ渡す(APCかと思って保留していたESCと、今回のバイト)。
    PassTwo(u8, u8),
    /// インターセプタが消費した(vteへは何も渡さない)。
    Consume,
    /// APC文字列が1つ完成した(`ESC _`とST終端を除いた中身)。
    Apc(Vec<u8>),
}

enum ApcState {
    Ground,
    /// Ground中にESCを見て保留中(次が`_`ならAPC開始)。
    EscSeen,
    /// APC本体を収集中。
    Collecting,
    /// APC本体収集中にESCを見た(次が`\`ならST=終端)。
    CollectingEsc,
}

/// バイトストリームから`ESC _ … ST`(APC文字列)だけを抜き取る前段。vteはAPCを
/// 配送しないため、Kitty graphicsのAPC本体はここで切り出す(モジュールdoc参照)。
pub(crate) struct ApcInterceptor {
    state: ApcState,
    buf: Vec<u8>,
    truncated: bool,
}

impl ApcInterceptor {
    pub(crate) fn new() -> Self {
        ApcInterceptor { state: ApcState::Ground, buf: Vec::new(), truncated: false }
    }

    pub(crate) fn feed(&mut self, byte: u8) -> ApcStep {
        match self.state {
            ApcState::Ground => {
                // 起点は7bitの`ESC`(0x1b)のみ。8bit C1のAPC導入子(0x9f)は**意図的に**
                // 認識しない——`on_stdout`はUTF-8デコード前の生バイトを流すため、0x9fは
                // UTF-8継続バイト範囲(0x80-0xBF)と重なる(例: '生' = E7 94 9F の末尾が0x9F)。
                // ここで0x9fをAPC開始と見なすとCJK/絵文字の末尾バイトを飲み込んでテキストを
                // 破壊する。8bit形式は対象外(モジュールdoc参照)——実端末は7bit ESC_/ESC\を使う。
                if byte == 0x1b {
                    self.state = ApcState::EscSeen;
                    ApcStep::Consume
                } else {
                    ApcStep::Pass(byte)
                }
            }
            ApcState::EscSeen => match byte {
                0x5f => {
                    // `ESC _` = APC開始。保留していたESCは破棄し本体収集へ。
                    self.state = ApcState::Collecting;
                    self.buf.clear();
                    self.truncated = false;
                    ApcStep::Consume
                }
                0x1b => {
                    // ESC ESC: 直前のESCを素通しし、新しいESCを保留して判定を継続。
                    ApcStep::Pass(0x1b)
                }
                _ => {
                    // APCではなかった。保留ESCと今回のバイトを順にvteへ渡す。
                    self.state = ApcState::Ground;
                    ApcStep::PassTwo(0x1b, byte)
                }
            },
            ApcState::Collecting => match byte {
                0x1b => {
                    self.state = ApcState::CollectingEsc;
                    ApcStep::Consume
                }
                0x9c => {
                    // C1 ST(単バイト終端)。ここで0x9cを終端として受理できるのは、APC本体が
                    // base64(ASCII)で生の0x9cが正当に現れないため——導入子側(Ground)で
                    // 0x9f/0x9cを特別扱いしないのとは非対称(Groundの生バイトはUTF-8で
                    // 0x80-0xBFが多用されるため、Ground参照)。
                    self.state = ApcState::Ground;
                    if self.truncated {
                        self.buf.clear();
                    }
                    ApcStep::Apc(std::mem::take(&mut self.buf))
                }
                _ => {
                    if self.buf.len() < MAX_APC_PAYLOAD {
                        self.buf.push(byte);
                    } else {
                        self.truncated = true;
                    }
                    ApcStep::Consume
                }
            },
            ApcState::CollectingEsc => {
                if byte == 0x5c {
                    // ST = `ESC \`。APC完成。
                    self.state = ApcState::Ground;
                    if self.truncated {
                        // 上限超過で途中破棄済み——空扱いで返し、dispatch側でno-opになる。
                        self.buf.clear();
                    }
                    ApcStep::Apc(std::mem::take(&mut self.buf))
                } else {
                    // ECMA-48: 文字列中のESCは文字列を中断し新しいエスケープシーケンスを
                    // 開始する。base64本体にESCは現れないのでこのAPCは不正/中断扱いとして
                    // 破棄し、直前のESCを「Groundで見たESC」として捉え直して、このバイトを
                    // その状態で再評価する(握りつぶさず後続シーケンスの先頭を保つ。
                    // `ESC _`ならそのまま新しいAPC開始になる)。
                    self.buf.clear();
                    self.truncated = false;
                    self.state = ApcState::EscSeen;
                    self.feed(byte)
                }
            }
        }
    }
}

/// 完成したAPCペイロードをdispatchした結果、Terminalに実行させたい操作。
pub(crate) enum KittyCommand {
    /// デコード済み画像をカーソル位置へ配置する(`a=T`)。
    Place(KittyImage),
    /// 全Kitty画像を削除する(`d=a`/`d=A`/裸の`d`)。
    DeleteAll,
    /// client image id指定でKitty画像を削除する(`d=i`/`d=I`+`i=<id>`)。
    DeleteId(u64),
    /// 何もしない(転送のみ/チャンク蓄積中/未対応/パース失敗)。
    None,
}

/// デコード完了したKitty画像。`terminal.rs`が`ImagePlacement`へ変換して配置する。
pub(crate) struct KittyImage {
    /// client側が付けた画像id(`i=`)。削除(`d=I,i=<id>`)で参照するため保持する。
    pub(crate) kitty_id: Option<u64>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    /// RGBA8888、row-major、左上原点。`width * height * 4`バイト。
    pub(crate) rgba: Vec<u8>,
}

/// 転送中(`m=1`のチャンク分割)の画像を組み立てるための状態。Kittyの直接転送は
/// 一度に1件しか進行しないため単一スロットで足りる。
struct Pending {
    format: u32,
    width: u32,
    height: u32,
    kitty_id: Option<u64>,
    display: bool,
    payload_b64: Vec<u8>,
}

pub(crate) struct KittyGraphics {
    pending: Option<Pending>,
}

impl KittyGraphics {
    pub(crate) fn new() -> Self {
        KittyGraphics { pending: None }
    }

    /// APCペイロード1件(`ESC _`・STを除いた中身)を処理する。先頭が`G`でなければ
    /// Kitty graphics以外のAPCとして無視する。
    pub(crate) fn dispatch(&mut self, payload: &[u8]) -> KittyCommand {
        // チャンク継続中(pending有り)の場合、後続チャンクはcontrolに`m`しか持たない
        // ことが多いので、先頭`G`判定より前に継続として処理する。
        if self.pending.is_some() {
            let (control, data) = split_payload(payload);
            let keys = parse_control(control);
            return self.continue_transmit(&keys, data);
        }

        if payload.first() != Some(&b'G') {
            return KittyCommand::None;
        }
        let (control, data) = split_payload(payload);
        let keys = parse_control(control);

        match action(&keys) {
            b'T' | b't' => self.start_transmit(&keys, data),
            b'd' => delete_command(&keys),
            // `q`(query)・`p`(placement)・`f`/`a`(animation)等は未対応(モジュールdoc参照)。
            _ => KittyCommand::None,
        }
    }

    fn start_transmit(&mut self, keys: &[(u8, Vec<u8>)], data: &[u8]) -> KittyCommand {
        let format = num(keys, b'f').unwrap_or(32) as u32;
        // 直接転送(`t=d`)以外は未対応。
        if let Some(t) = chr(keys, b't') {
            if t != b'd' {
                return KittyCommand::None;
            }
        }
        let mut pending = Pending {
            format,
            width: num(keys, b's').unwrap_or(0) as u32,
            height: num(keys, b'v').unwrap_or(0) as u32,
            kitty_id: num(keys, b'i'),
            display: action(keys) == b'T',
            payload_b64: Vec::new(),
        };
        pending.payload_b64.extend_from_slice(data);
        let more = num(keys, b'm').unwrap_or(0) == 1;
        if more {
            self.pending = Some(pending);
            KittyCommand::None
        } else {
            self.finalize(pending)
        }
    }

    fn continue_transmit(&mut self, keys: &[(u8, Vec<u8>)], data: &[u8]) -> KittyCommand {
        let mut pending = self.pending.take().expect("pending checked by caller");
        if pending.payload_b64.len() + data.len() <= MAX_APC_PAYLOAD {
            pending.payload_b64.extend_from_slice(data);
        }
        let more = num(keys, b'm').unwrap_or(0) == 1;
        if more {
            self.pending = Some(pending);
            KittyCommand::None
        } else {
            self.finalize(pending)
        }
    }

    fn finalize(&mut self, pending: Pending) -> KittyCommand {
        if !pending.display {
            // `a=t`(転送のみ)は表示しない。画像ストアを持たないため実質no-op。
            return KittyCommand::None;
        }
        let bytes = match base64::engine::general_purpose::STANDARD.decode(&pending.payload_b64) {
            Ok(b) => b,
            Err(_) => return KittyCommand::None,
        };
        let decoded = match pending.format {
            32 => rgba_from_raw(&bytes, pending.width, pending.height, 4),
            24 => rgba_from_raw(&bytes, pending.width, pending.height, 3),
            100 => png_to_rgba(&bytes),
            _ => None,
        };
        match decoded {
            Some((width, height, rgba)) => KittyCommand::Place(KittyImage {
                kitty_id: pending.kitty_id,
                width,
                height,
                rgba,
            }),
            None => KittyCommand::None,
        }
    }
}

fn action(keys: &[(u8, Vec<u8>)]) -> u8 {
    chr(keys, b'a').unwrap_or(b't')
}

fn delete_command(keys: &[(u8, Vec<u8>)]) -> KittyCommand {
    // 大文字/小文字はデータ解放の有無だけの違い——画面上の配置削除としては同じに扱う。
    match chr(keys, b'd').unwrap_or(b'a') {
        b'a' | b'A' => KittyCommand::DeleteAll,
        b'i' | b'I' => match num(keys, b'i') {
            Some(id) => KittyCommand::DeleteId(id),
            None => KittyCommand::None,
        },
        _ => KittyCommand::None,
    }
}

/// ペイロードを`control`(先頭の`G`を含む key=value 群)と`base64本体`に`;`で分割する。
fn split_payload(payload: &[u8]) -> (&[u8], &[u8]) {
    match payload.iter().position(|&b| b == b';') {
        Some(i) => (&payload[..i], &payload[i + 1..]),
        None => (payload, &[]),
    }
}

/// `G` に続く `k=v,k=v,…` をパースする。`k`は1文字。先頭の`G`はスキップする。
fn parse_control(control: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let control = control.strip_prefix(b"G").unwrap_or(control);
    let mut out = Vec::new();
    for part in control.split(|&b| b == b',') {
        if part.is_empty() {
            continue;
        }
        if let Some(eq) = part.iter().position(|&b| b == b'=') {
            let key = part[0];
            out.push((key, part[eq + 1..].to_vec()));
        }
    }
    out
}

fn value<'a>(keys: &'a [(u8, Vec<u8>)], k: u8) -> Option<&'a [u8]> {
    keys.iter().find(|(key, _)| *key == k).map(|(_, v)| v.as_slice())
}

fn num(keys: &[(u8, Vec<u8>)], k: u8) -> Option<u64> {
    let v = value(keys, k)?;
    std::str::from_utf8(v).ok()?.parse::<u64>().ok()
}

fn chr(keys: &[(u8, Vec<u8>)], k: u8) -> Option<u8> {
    value(keys, k)?.first().copied()
}

/// 生ピクセル(1ピクセルあたり`bpp`バイト、RGBまたはRGBA)をRGBA8888へ整える。
/// 幅・高さ・バッファ長が一致しない/上限超過なら`None`(画像を破棄)。
fn rgba_from_raw(bytes: &[u8], width: u32, height: u32, bpp: usize) -> Option<(usize, usize, Vec<u8>)> {
    let width = width as usize;
    let height = height as usize;
    if !dims_ok(width, height) {
        return None;
    }
    if bytes.len() != width.checked_mul(height)?.checked_mul(bpp)? {
        return None;
    }
    if bpp == 4 {
        return Some((width, height, bytes.to_vec()));
    }
    // RGB → RGBA(alpha=255)。
    let mut rgba = vec![0u8; width * height * 4];
    for (i, px) in bytes.chunks_exact(3).enumerate() {
        let o = i * 4;
        rgba[o] = px[0];
        rgba[o + 1] = px[1];
        rgba[o + 2] = px[2];
        rgba[o + 3] = 255;
    }
    Some((width, height, rgba))
}

/// PNG(`f=100`)をRGBA8888へデコードする。パレット/グレースケール/16bitは
/// `EXPAND | STRIP_16`で8bit RGB(A)へ正規化してから整える。
fn png_to_rgba(bytes: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let info = reader.info();
    let width = info.width as usize;
    let height = info.height as usize;
    if !dims_ok(width, height) {
        return None;
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).ok()?;
    buf.truncate(frame.buffer_size());
    let channels = match frame.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        // EXPANDでIndexedは消えるはずだが、念のため未対応扱い。
        png::ColorType::Indexed => return None,
    };
    if buf.len() != width.checked_mul(height)?.checked_mul(channels)? {
        return None;
    }
    let mut rgba = vec![0u8; width * height * 4];
    for (i, px) in buf.chunks_exact(channels).enumerate() {
        let o = i * 4;
        match channels {
            4 => rgba[o..o + 4].copy_from_slice(px),
            3 => {
                rgba[o..o + 3].copy_from_slice(px);
                rgba[o + 3] = 255;
            }
            2 => {
                rgba[o] = px[0];
                rgba[o + 1] = px[0];
                rgba[o + 2] = px[0];
                rgba[o + 3] = px[1];
            }
            _ => {
                rgba[o] = px[0];
                rgba[o + 1] = px[0];
                rgba[o + 2] = px[0];
                rgba[o + 3] = 255;
            }
        }
    }
    Some((width, height, rgba))
}

fn dims_ok(width: usize, height: usize) -> bool {
    width != 0
        && height != 0
        && width <= MAX_SIXEL_DIM
        && height <= MAX_SIXEL_DIM
        && width.saturating_mul(height) <= MAX_SIXEL_AREA
}

#[cfg(test)]
mod tests {
    use super::*;

    /// バイト列をインターセプタに通し、抜き取れたAPCペイロード一覧と、vteへ素通し
    /// されたバイト列を返す。
    fn run_interceptor(input: &[u8]) -> (Vec<Vec<u8>>, Vec<u8>) {
        let mut ic = ApcInterceptor::new();
        let mut apcs = Vec::new();
        let mut passed = Vec::new();
        for &b in input {
            match ic.feed(b) {
                ApcStep::Pass(x) => passed.push(x),
                ApcStep::PassTwo(a, c) => {
                    passed.push(a);
                    passed.push(c);
                }
                ApcStep::Consume => {}
                ApcStep::Apc(p) => apcs.push(p),
            }
        }
        (apcs, passed)
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn interceptor_extracts_apc_and_passes_rest_through() {
        // "ab" + APC("Gf=32") + "cd"、APCは `ESC \` で終端。
        let (apcs, passed) = run_interceptor(b"ab\x1b_Gf=32\x1b\\cd");
        assert_eq!(apcs, vec![b"Gf=32".to_vec()]);
        assert_eq!(passed, b"abcd".to_vec());
    }

    #[test]
    fn interceptor_passes_non_apc_escape_through_untouched() {
        // `ESC [ 0 m`(SGR reset)はvteへ2バイト目以降含めそのまま渡る。
        let (apcs, passed) = run_interceptor(b"x\x1b[0my");
        assert!(apcs.is_empty());
        assert_eq!(passed, b"x\x1b[0my".to_vec());
    }

    #[test]
    fn interceptor_does_not_disturb_dcs_st_terminator() {
        // DCS `ESC P q ... ESC \`(sixel相当)はAPCではないので全バイトvteへ渡る。
        let (apcs, passed) = run_interceptor(b"\x1bPq#0@\x1b\\");
        assert!(apcs.is_empty());
        assert_eq!(passed, b"\x1bPq#0@\x1b\\".to_vec());
    }

    #[test]
    fn interceptor_accepts_c1_st_terminator() {
        let (apcs, _) = run_interceptor(b"\x1b_Gi=1\x9c");
        assert_eq!(apcs, vec![b"Gi=1".to_vec()]);
    }

    #[test]
    fn aborted_apc_reprocesses_following_escape_sequence() {
        // APC本体の途中で `ESC [` (ST=`ESC \`ではない)が来た場合: 不完全APCは破棄し、
        // `ESC [ 0 m` はvteへ素通しされる(先頭ESCを含め1バイトも落とさない)。
        let (apcs, passed) = run_interceptor(b"\x1b_Gf=32\x1b[0m");
        assert!(apcs.is_empty(), "不完全なAPCは破棄される");
        assert_eq!(passed, b"\x1b[0m".to_vec(), "中断後のCSIは完全に素通しされる");
    }

    #[test]
    fn aborted_apc_can_start_a_new_apc() {
        // `... ESC _`(本体中のESC直後が`_`)は現APC中断+新APC開始。
        let (apcs, passed) = run_interceptor(b"\x1b_Gold\x1b_Gi=9\x1b\\z");
        assert_eq!(apcs, vec![b"Gi=9".to_vec()], "2つ目のAPCだけが完成扱い");
        assert_eq!(passed, b"z".to_vec());
    }

    #[test]
    fn decode_raw_rgba_single_pixel() {
        let mut kg = KittyGraphics::new();
        let payload = format!("Gf=32,s=1,v=1,a=T;{}", b64(&[10, 20, 30, 40]));
        match kg.dispatch(payload.as_bytes()) {
            KittyCommand::Place(img) => {
                assert_eq!(img.width, 1);
                assert_eq!(img.height, 1);
                assert_eq!(img.rgba, vec![10, 20, 30, 40]);
            }
            _ => panic!("expected Place"),
        }
    }

    #[test]
    fn decode_raw_rgb_expands_alpha() {
        let mut kg = KittyGraphics::new();
        let payload = format!("Gf=24,s=1,v=1,a=T;{}", b64(&[1, 2, 3]));
        match kg.dispatch(payload.as_bytes()) {
            KittyCommand::Place(img) => assert_eq!(img.rgba, vec![1, 2, 3, 255]),
            _ => panic!("expected Place"),
        }
    }

    #[test]
    fn chunked_transmission_reassembles() {
        let mut kg = KittyGraphics::new();
        // 2x1 RGBA画像を2チャンクに分割(base64テキストとして連結される)。
        let full = b64(&[1, 1, 1, 1, 2, 2, 2, 2]);
        let (a, b) = full.split_at(4);
        // 1チャンク目: 制御情報 + m=1。
        let first = format!("Gf=32,s=2,v=1,a=T,m=1;{}", a);
        assert!(matches!(kg.dispatch(first.as_bytes()), KittyCommand::None));
        // 2チャンク目: m=0 で確定。
        let second = format!("Gm=0;{}", b);
        match kg.dispatch(second.as_bytes()) {
            KittyCommand::Place(img) => {
                assert_eq!((img.width, img.height), (2, 1));
                assert_eq!(img.rgba, vec![1, 1, 1, 1, 2, 2, 2, 2]);
            }
            _ => panic!("expected Place after final chunk"),
        }
    }

    #[test]
    fn transmit_only_does_not_display() {
        let mut kg = KittyGraphics::new();
        let payload = format!("Gf=32,s=1,v=1,a=t;{}", b64(&[0, 0, 0, 255]));
        assert!(matches!(kg.dispatch(payload.as_bytes()), KittyCommand::None));
    }

    #[test]
    fn delete_all_variants() {
        let mut kg = KittyGraphics::new();
        assert!(matches!(kg.dispatch(b"Ga=d,d=A"), KittyCommand::DeleteAll));
        assert!(matches!(kg.dispatch(b"Ga=d,d=a"), KittyCommand::DeleteAll));
        assert!(matches!(kg.dispatch(b"Ga=d"), KittyCommand::DeleteAll));
    }

    #[test]
    fn delete_by_id() {
        let mut kg = KittyGraphics::new();
        match kg.dispatch(b"Ga=d,d=I,i=7") {
            KittyCommand::DeleteId(7) => {}
            _ => panic!("expected DeleteId(7)"),
        }
    }

    #[test]
    fn non_kitty_apc_ignored() {
        let mut kg = KittyGraphics::new();
        assert!(matches!(kg.dispatch(b"Xsomething"), KittyCommand::None));
    }

    #[test]
    fn wrong_size_raw_buffer_dropped() {
        let mut kg = KittyGraphics::new();
        // s=2,v=2 は 16バイト必要だが4バイトしか渡さない → 破棄。
        let payload = format!("Gf=32,s=2,v=2,a=T;{}", b64(&[1, 2, 3, 4]));
        assert!(matches!(kg.dispatch(payload.as_bytes()), KittyCommand::None));
    }

    #[test]
    fn oversized_dims_dropped() {
        let mut kg = KittyGraphics::new();
        let payload = format!("Gf=32,s=99999,v=99999,a=T;{}", b64(&[0, 0, 0, 0]));
        assert!(matches!(kg.dispatch(payload.as_bytes()), KittyCommand::None));
    }

    #[test]
    fn decode_png_rgba() {
        // 2x1のPNG(赤・緑)を生成してf=100で送る。
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, 2, 1);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer
                .write_image_data(&[255, 0, 0, 255, 0, 255, 0, 255])
                .unwrap();
        }
        let mut kg = KittyGraphics::new();
        let payload = format!("Gf=100,a=T;{}", b64(&png_bytes));
        match kg.dispatch(payload.as_bytes()) {
            KittyCommand::Place(img) => {
                assert_eq!((img.width, img.height), (2, 1));
                assert_eq!(img.rgba, vec![255, 0, 0, 255, 0, 255, 0, 255]);
            }
            _ => panic!("expected Place from PNG"),
        }
    }
}
