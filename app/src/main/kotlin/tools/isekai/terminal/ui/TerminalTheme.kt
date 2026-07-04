package tools.isekai.terminal.ui

import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toArgb

/**
 * ターミナル配色テーマ（プリセット選択・永続化 案C）。
 *
 * SGR 解釈テーブル自体は Rust 側（`rust-core/src/theme.rs`）がグローバル状態として保持し、
 * `uniffi.tssh_core.setTerminalTheme` 経由で差し替える。このクラスは Kotlin 側での
 * プリセット定義・永続化キー・Rust に渡す ARGB への変換のみを担う。
 *
 * 呼び出し以降にパースされる SGR にのみ反映され、既に scrollback に積まれた行は
 * 遡って再着色されない（既知の制約。`set_terminal_theme` のドキュメント参照）。
 */
data class TerminalTheme(
    val name: String,
    val foreground: Color,
    val background: Color,
    val cursor: Color,
    val ansi16: List<Color>,
) {
    init {
        require(ansi16.size == 16) { "ansi16 must have exactly 16 entries, got ${ansi16.size}" }
    }

    fun foregroundArgb(): UInt = foreground.toArgbUInt()
    fun backgroundArgb(): UInt = background.toArgbUInt()
    fun ansi16Argb(): List<UInt> = ansi16.map { it.toArgbUInt() }
}

private fun Color.toArgbUInt(): UInt = toArgb().toUInt()

/** [uniffi.tssh_core.setTerminalTheme]/[tools.isekai.terminal.session.TerminalSession.setTheme]
 *  など、`(ansi16Argb, foregroundArgb, backgroundArgb)` の3引数を取るsetterへ、[TerminalTheme]の
 *  ARGB変換を共通の形で渡すためのヘルパー(呼び出し側の配線都合であり、テーマデータ自体の
 *  責務ではないためTerminalThemeのメンバーではなく独立した拡張関数にしている)。 */
fun TerminalTheme.applyTo(setter: (ansi16: List<UInt>, foreground: UInt, background: UInt) -> Unit) =
    setter(ansi16Argb(), foregroundArgb(), backgroundArgb())

object TerminalThemes {
    /** `SharedPreferences("tssh_ui")` に保存するテーマ名のキー */
    const val PREF_KEY = "terminal_theme"

    // 既定ダーク: rust-core/src/theme.rs の Theme::default() と同じ値を維持する
    // （既存の VTE ユニットテスト・見た目の後方互換のため必ず一致させること）。
    val DEFAULT_DARK = TerminalTheme(
        name = "Default Dark",
        foreground = Color(0xFFCCCCCC),
        background = Color(0xFF000000),
        cursor = Color(0xFFFFFFFF),
        ansi16 = listOf(
            Color(0xFF000000), Color(0xFFAA0000), Color(0xFF00AA00), Color(0xFFAAAA00),
            Color(0xFF0000AA), Color(0xFFAA00AA), Color(0xFF00AAAA), Color(0xFFAAAAAA),
            Color(0xFF555555), Color(0xFFFF5555), Color(0xFF55FF55), Color(0xFFFFFF55),
            Color(0xFF5555FF), Color(0xFFFF55FF), Color(0xFF55FFFF), Color(0xFFFFFFFF),
        ),
    )

    // Solarized Dark 公式パレット出典: https://ethanschoonover.com/solarized/
    val SOLARIZED_DARK = TerminalTheme(
        name = "Solarized Dark",
        foreground = Color(0xFF839496), // base0
        background = Color(0xFF002B36), // base03
        cursor = Color(0xFF93A1A1), // base1
        ansi16 = listOf(
            Color(0xFF073642), // base02        (black)
            Color(0xFFDC322F), // red
            Color(0xFF859900), // green
            Color(0xFFB58900), // yellow
            Color(0xFF268BD2), // blue
            Color(0xFFD33682), // magenta
            Color(0xFF2AA198), // cyan
            Color(0xFFEEE8D5), // base2         (white)
            Color(0xFF002B36), // base03        (bright black)
            Color(0xFFCB4B16), // orange        (bright red)
            Color(0xFF586E75), // base01        (bright green)
            Color(0xFF657B83), // base00        (bright yellow)
            Color(0xFF839496), // base0         (bright blue)
            Color(0xFF6C71C4), // violet        (bright magenta)
            Color(0xFF93A1A1), // base1         (bright cyan)
            Color(0xFFFDF6E3), // base3         (bright white)
        ),
    )

    // Dracula 公式ターミナル配色 出典: https://draculatheme.com/contribute (Terminal palette)
    val DRACULA = TerminalTheme(
        name = "Dracula",
        foreground = Color(0xFFF8F8F2),
        background = Color(0xFF282A36),
        cursor = Color(0xFFF8F8F2),
        ansi16 = listOf(
            Color(0xFF21222C), Color(0xFFFF5555), Color(0xFF50FA7B), Color(0xFFF1FA8C),
            Color(0xFFBD93F9), Color(0xFFFF79C6), Color(0xFF8BE9FD), Color(0xFFF8F8F2),
            Color(0xFF6272A4), Color(0xFFFF6E6E), Color(0xFF69FF94), Color(0xFFFFFFA5),
            Color(0xFFD6ACFF), Color(0xFFFF92DF), Color(0xFFA4FFFF), Color(0xFFFFFFFF),
        ),
    )

    // Nord 公式パレット 出典: https://www.nordtheme.com/docs/colors-and-palettes
    val NORD = TerminalTheme(
        name = "Nord",
        foreground = Color(0xFFD8DEE9), // nord4
        background = Color(0xFF2E3440), // nord0
        cursor = Color(0xFFD8DEE9), // nord4
        ansi16 = listOf(
            Color(0xFF3B4252), // nord1  black
            Color(0xFFBF616A), // nord11 red
            Color(0xFFA3BE8C), // nord14 green
            Color(0xFFEBCB8B), // nord13 yellow
            Color(0xFF81A1C1), // nord9  blue
            Color(0xFFB48EAD), // nord15 magenta
            Color(0xFF88C0D0), // nord8  cyan
            Color(0xFFE5E9F0), // nord5  white
            Color(0xFF4C566A), // nord3  bright black
            Color(0xFFBF616A), // nord11 bright red
            Color(0xFFA3BE8C), // nord14 bright green
            Color(0xFFEBCB8B), // nord13 bright yellow
            Color(0xFF81A1C1), // nord9  bright blue
            Color(0xFFB48EAD), // nord15 bright magenta
            Color(0xFF8FBCBB), // nord7  bright cyan
            Color(0xFFECEFF4), // nord6  bright white
        ),
    )

    val ALL: List<TerminalTheme> = listOf(DEFAULT_DARK, SOLARIZED_DARK, DRACULA, NORD)

    /** プリセット名からテーマを解決する。未知の名前・null の場合は既定ダークにフォールバックする。 */
    fun byName(name: String?): TerminalTheme = ALL.find { it.name == name } ?: DEFAULT_DARK
}
