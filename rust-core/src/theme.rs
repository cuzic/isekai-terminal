use std::sync::LazyLock;
use parking_lot::RwLock;

/// SGR 解釈に使う配色テーブル（案C: パレットは Rust 側に残しつつ差し替え可能にする）。
///
/// `ansi16` は SGR 30-37/90-97（fg）・40-47/100-107（bg）が参照する 16 色を
/// `[normal(0..8), bright(8..16)]` の順に並べたもの。`ansi256_to_argb` の
/// 16 色部分（0..=15）もこのテーブルをそのまま使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Theme {
    pub(crate) ansi16: [u32; 16],
    pub(crate) default_fg: u32,
    pub(crate) default_bg: u32,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            ansi16: [
                // normal
                0xFF000000, 0xFFAA0000, 0xFF00AA00, 0xFFAAAA00,
                0xFF0000AA, 0xFFAA00AA, 0xFF00AAAA, 0xFFAAAAAA,
                // bright
                0xFF555555, 0xFFFF5555, 0xFF55FF55, 0xFFFFFF55,
                0xFF5555FF, 0xFFFF55FF, 0xFF55FFFF, 0xFFFFFFFF,
            ],
            default_fg: 0xFFCCCCCC,
            default_bg: 0xFF000000,
        }
    }
}

/// プロセス全体で共有されるグローバルなテーマ設定。
///
/// テーマはプロファイル毎ではなく Kotlin 側の `SharedPreferences("tssh_ui")` に保存される
/// グローバル設定であり（`set_terminal_theme` 参照）、どのセッション（`Terminal`）から見ても
/// 同じテーブルを参照する必要があるため、`Terminal` インスタンスにテーブルを持たせるのではなく
/// プロセス全体の `static` として保持する。既存コードの並行制御パターン
/// （`parking_lot::Mutex` を使った `Arc<Mutex<_>>` 共有）に合わせ、読み取りが多く書き込みが
/// 稀なこのテーブルには `parking_lot::RwLock` を採用した。
static THEME: LazyLock<RwLock<Theme>> = LazyLock::new(|| RwLock::new(Theme::default()));

/// 現在のテーマのスナップショットを取得する。
/// 呼び出し以降にパースされる SGR の色解決にのみ影響し、既に scrollback に
/// 積まれた行は遡って再着色されない（既知の制約）。
pub(crate) fn current() -> Theme {
    *THEME.read()
}

/// テーマを差し替える。以降に `Terminal` が解決する SGR の色に反映される。
pub(crate) fn set(theme: Theme) {
    *THEME.write() = theme;
}
