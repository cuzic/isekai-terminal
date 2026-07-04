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

/// アプリ全体の既定テーマ設定(プロセス全体で共有されるグローバル状態)。
///
/// Kotlin 側の `SharedPreferences("tssh_ui")` に保存される、アプリ全体のデフォルト値
/// （`set_terminal_theme` 参照）。Phase 12 より前はこれが全セッション共通の唯一の
/// テーマだったが、現在は各セッション(タブ)が `Terminal.theme`
/// （`SessionCore::current_theme` 経由）に自分のテーマのスナップショットを持ち、
/// - 新規タブ作成時（`SessionCore::start`）にこの既定値をスナップショットする
/// - プロファイル固有のテーマ・タブごとの上書きがあれば、その後
///   `SessionOrchestrator::set_session_theme` で明示的に上書きする
/// という形で「Global default → Profile default → Tab/session override」の
/// 3段階を実現している（Kotlin 側 `TerminalTabsViewModel` 参照）。
static THEME: LazyLock<RwLock<Theme>> = LazyLock::new(|| RwLock::new(Theme::default()));

/// アプリ全体の既定テーマのスナップショットを取得する。
/// 呼び出し以降に新規作成されるセッションの初期テーマにのみ影響し、既存セッションの
/// テーマを変更したい場合は `SessionOrchestrator::set_session_theme` を使うこと。
pub(crate) fn current() -> Theme {
    *THEME.read()
}

/// アプリ全体の既定テーマを差し替える。以降に新規作成される（＝プロファイル/タブ固有の
/// 上書きを持たない）セッションの初期テーマに反映される。既存セッションには影響しない
/// （Kotlin 側が非上書きのタブへ個別に `set_session_theme` を呼んで伝播させる設計）。
pub(crate) fn set(theme: Theme) {
    *THEME.write() = theme;
}

/// UniFFI境界で渡ってくる生の値(`ansi16`は16色に満たない場合が既定値で埋められる)から
/// [Theme] を組み立てる。グローバル設定(`set_terminal_theme`)・per-session設定
/// (`SessionOrchestrator::set_session_theme`)の両方で共有するロジック。
pub(crate) fn from_raw(ansi16: Vec<u32>, default_fg: u32, default_bg: u32) -> Theme {
    let mut table = Theme::default().ansi16;
    for (slot, v) in table.iter_mut().zip(ansi16.into_iter()) {
        *slot = v;
    }
    Theme { ansi16: table, default_fg, default_bg }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_raw_with_empty_ansi16_keeps_all_defaults() {
        let theme = from_raw(Vec::new(), 0xFF111111, 0xFF222222);
        assert_eq!(theme.ansi16, Theme::default().ansi16);
        assert_eq!(theme.default_fg, 0xFF111111);
        assert_eq!(theme.default_bg, 0xFF222222);
    }

    #[test]
    fn from_raw_with_fewer_than_16_overwrites_only_leading_slots() {
        let theme = from_raw(vec![0xFFAAAAAA, 0xFFBBBBBB], 0, 0);
        let default = Theme::default().ansi16;
        assert_eq!(theme.ansi16[0], 0xFFAAAAAA);
        assert_eq!(theme.ansi16[1], 0xFFBBBBBB);
        // 残り14スロットは既定値のまま
        assert_eq!(&theme.ansi16[2..], &default[2..]);
    }

    #[test]
    fn from_raw_with_exactly_16_overwrites_every_slot() {
        let custom: Vec<u32> = (0..16).map(|i| 0xFF000000 + i).collect();
        let theme = from_raw(custom.clone(), 0, 0);
        assert_eq!(theme.ansi16.to_vec(), custom);
    }

    #[test]
    fn from_raw_with_more_than_16_ignores_extra_entries() {
        let mut custom: Vec<u32> = (0..16).map(|i| 0xFF000000 + i).collect();
        custom.push(0xFFFFFFFF); // 17番目、はみ出す分は無視されるはず
        let theme = from_raw(custom.clone(), 0, 0);
        assert_eq!(theme.ansi16.to_vec(), &custom[..16]);
    }

    // `current()`/`set()`は本当にプロセス全体で共有される`static`を読み書きするため、
    // ここではテストしない — `cargo test`はデフォルトでテストを並列実行するので、
    // この2つを直接叩くテストは他の(将来`session.rs`に追加されるかもしれない)
    // `theme::current()`に依存するテストと競合し、フレーキーの原因になりうる。
}
