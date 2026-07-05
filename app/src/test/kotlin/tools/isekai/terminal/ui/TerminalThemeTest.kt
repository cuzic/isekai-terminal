package tools.isekai.terminal.ui

import android.app.Application
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * 配色テーマのプリセット選択・永続化（案C）のテスト。
 *
 * Rust 側への実際の反映（`uniffi.isekai_terminal_core.setTerminalTheme`）はホスト JVM 上で
 * ネイティブライブラリを解決できないため、ここでは native 呼び出しに一切触れず、
 * 1) プリセット名 → テーマの解決ロジック、2) SharedPreferences への永続化・復元、
 * の2点のみを検証する（実際の Rust 反映は `cargo test -p isekai-terminal-core --lib` 側でカバー済み）。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class TerminalThemeTest {

    @Test fun `byName resolves each known preset`() {
        assertEquals(TerminalThemes.DEFAULT_DARK, TerminalThemes.byName("Default Dark"))
        assertEquals(TerminalThemes.SOLARIZED_DARK, TerminalThemes.byName("Solarized Dark"))
        assertEquals(TerminalThemes.DRACULA, TerminalThemes.byName("Dracula"))
        assertEquals(TerminalThemes.NORD, TerminalThemes.byName("Nord"))
    }

    @Test fun `byName falls back to default dark for unknown name`() {
        assertEquals(TerminalThemes.DEFAULT_DARK, TerminalThemes.byName("nonexistent-theme"))
    }

    @Test fun `byName falls back to default dark for null`() {
        assertEquals(TerminalThemes.DEFAULT_DARK, TerminalThemes.byName(null))
    }

    @Test fun `every preset has exactly 16 ansi colors`() {
        TerminalThemes.ALL.forEach { theme ->
            assertEquals("theme '${theme.name}' must have 16 ansi colors", 16, theme.ansi16.size)
        }
    }

    @Test fun `presets are distinct from each other`() {
        val names = TerminalThemes.ALL.map { it.name }
        assertEquals(names.distinct(), names)
        // 見た目上も別テーマであることの簡易チェック（背景色が全部同じでは意味がない）
        val backgrounds = TerminalThemes.ALL.map { it.backgroundArgb() }
        assertEquals(backgrounds.distinct().size, backgrounds.size)
    }

    @Test fun `ansi16Argb converts to opaque ARGB uints matching the Color values`() {
        val theme = TerminalThemes.DEFAULT_DARK
        val argb = theme.ansi16Argb()
        assertEquals(16, argb.size)
        // index 1 は赤 (0xFFAA0000)
        assertEquals(0xFFAA0000u, argb[1])
        assertNotEquals(argb[0], argb[1])
    }

    @Test fun `selecting a theme persists to SharedPreferences and survives restart`() {
        val ctx = ApplicationProvider.getApplicationContext<Application>()
        val prefs = ctx.getSharedPreferences("isekai_terminal_ui", Application.MODE_PRIVATE)

        // 選択（ProfileListScreen の onSelect と同じ書き込み方）
        prefs.edit().putString(TerminalThemes.PREF_KEY, TerminalThemes.DRACULA.name).apply()

        // 「次回起動時の復元」は永続化されたプリセット名を読み直して解決するだけなので、
        // 新しい SharedPreferences ハンドルで読み直しても同じ結果になることを確認する
        val restoredPrefs = ctx.getSharedPreferences("isekai_terminal_ui", Application.MODE_PRIVATE)
        val restored = TerminalThemes.byName(restoredPrefs.getString(TerminalThemes.PREF_KEY, null))
        assertEquals(TerminalThemes.DRACULA, restored)
    }

    @Test fun `no persisted value yet resolves to default dark on first launch`() {
        val ctx = ApplicationProvider.getApplicationContext<Application>()
        val prefs = ctx.getSharedPreferences("isekai_terminal_ui_fresh_${System.nanoTime()}", Application.MODE_PRIVATE)
        val restored = TerminalThemes.byName(prefs.getString(TerminalThemes.PREF_KEY, null))
        assertEquals(TerminalThemes.DEFAULT_DARK, restored)
    }
}
