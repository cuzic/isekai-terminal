package tools.isekai.terminal.ui

import org.junit.Assert.assertEquals
import org.junit.Test
import uniffi.isekai_terminal_core.CellData

/**
 * [computeBgRuns] は背景描画のバッチ化(セルごとの `drawRect` を連続区間ごとの
 * `drawRect` にまとめる)のためのピュア関数。実際のレンダリング結果と等価であることを
 * 「区間の和集合がセルごとの塗りつぶし対象と一致する」形で検証する。
 */
class SshTerminalCanvasTest {

    private val defaultBg = 0xFF000000.toInt()

    private fun cell(bg: Int) = CellData(ch = " ", fg = 0xFFFFFFFFu, bg = bg.toUInt(), bold = false)

    @Test
    fun `all default background produces no runs`() {
        val cells = List(10) { cell(defaultBg) }
        val runs = computeBgRuns(cells, rowStart = 0, cols = 10, themeBgArgb = defaultBg)
        assertEquals(emptyList<BgRun>(), runs)
    }

    @Test
    fun `single non-default cell produces a single-width run`() {
        val red = 0xFFFF0000.toInt()
        val cells = List(5) { i -> cell(if (i == 2) red else defaultBg) }
        val runs = computeBgRuns(cells, rowStart = 0, cols = 5, themeBgArgb = defaultBg)
        assertEquals(listOf(BgRun(2, 3, red)), runs)
    }

    @Test
    fun `contiguous same-color cells merge into one run`() {
        val red = 0xFFFF0000.toInt()
        val cells = listOf(defaultBg, red, red, red, defaultBg).map { cell(it) }
        val runs = computeBgRuns(cells, rowStart = 0, cols = 5, themeBgArgb = defaultBg)
        assertEquals(listOf(BgRun(1, 4, red)), runs)
    }

    @Test
    fun `adjacent cells with different non-default colors form separate runs`() {
        val red = 0xFFFF0000.toInt()
        val blue = 0xFF0000FF.toInt()
        val cells = listOf(red, red, blue).map { cell(it) }
        val runs = computeBgRuns(cells, rowStart = 0, cols = 3, themeBgArgb = defaultBg)
        assertEquals(listOf(BgRun(0, 2, red), BgRun(2, 3, blue)), runs)
    }

    @Test
    fun `run touching the end of the row is closed correctly`() {
        val red = 0xFFFF0000.toInt()
        val cells = listOf(defaultBg, defaultBg, red, red).map { cell(it) }
        val runs = computeBgRuns(cells, rowStart = 0, cols = 4, themeBgArgb = defaultBg)
        assertEquals(listOf(BgRun(2, 4, red)), runs)
    }

    @Test
    fun `rowStart offset is respected so other rows are ignored`() {
        val red = 0xFFFF0000.toInt()
        val blue = 0xFF0000FF.toInt()
        // 2行分(cols=3)。1行目は全て赤、2行目は全て青。
        val cells = List(3) { cell(red) } + List(3) { cell(blue) }
        val runsRow0 = computeBgRuns(cells, rowStart = 0, cols = 3, themeBgArgb = defaultBg)
        val runsRow1 = computeBgRuns(cells, rowStart = 3, cols = 3, themeBgArgb = defaultBg)
        assertEquals(listOf(BgRun(0, 3, red)), runsRow0)
        assertEquals(listOf(BgRun(0, 3, blue)), runsRow1)
    }
}
