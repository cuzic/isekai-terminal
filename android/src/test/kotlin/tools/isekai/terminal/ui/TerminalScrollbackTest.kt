package tools.isekai.terminal.ui

import org.junit.Assert.assertEquals
import org.junit.Test
import uniffi.isekai_terminal_core.CellData
import uniffi.isekai_terminal_core.CursorShape
import uniffi.isekai_terminal_core.MouseReportingMode
import uniffi.isekai_terminal_core.ScreenUpdate

/**
 * タスク#46: [synthesizeDisplayUpdate]の合成ロジックを検証する。
 * iOS版`TerminalScrollbackTests.swift`と対称。
 */
class TerminalScrollbackTest {

    private fun cell(ch: Char) = CellData(
        ch = ch.toString(), fg = 0xFFFFFFFFu, bg = 0xFF000000u, bold = false,
        dim = false, italic = false, underline = false,
        strikethrough = false, blink = false, invisible = false, linkId = null,
    )

    private fun update(
        rows: List<String>,
        cols: Int,
        cursorRow: UInt = 0u,
        cursorCol: UInt = 0u,
        cursorVisible: Boolean = true,
        bellGeneration: ULong = 0uL,
    ): ScreenUpdate {
        val cells = rows.flatMap { line ->
            (0 until cols).map { i -> cell(line.getOrNull(i) ?: ' ') }
        }
        return ScreenUpdate(
            cols = cols.toUInt(),
            rows = rows.size.toUInt(),
            cells = cells,
            cursorRow = cursorRow,
            cursorCol = cursorCol,
            title = "session",
            applicationCursorMode = true,
            bracketedPasteMode = true,
            mouseReportingMode = MouseReportingMode.OFF,
            sgrMouseMode = false,
            cursorVisible = cursorVisible,
            bellGeneration = bellGeneration,
            cursorShape = CursorShape.BLOCK,
            cursorBlink = true,
            linkTable = emptyList(),
            images = emptyList(),
            kittyKeyboardFlags = 0u,
        )
    }

    @Test
    fun `returns live update when scroll offset is zero`() {
        val live = update(rows = listOf("live line"), cols = 20, cursorCol = 3u)

        val result = synthesizeDisplayUpdate(live, scrollOffset = 0, scrollbackCells = null)

        assertEquals(live, result)
    }

    @Test
    fun `synthesizes scrollback update when offset is positive`() {
        val live = update(rows = listOf("live line"), cols = 20, cursorCol = 3u)
        val scrollbackCells = List(20) { cell('x') } // live.cols(20) * live.rows(1)

        val result = synthesizeDisplayUpdate(live, scrollOffset = 5, scrollbackCells = scrollbackCells)

        assertEquals(scrollbackCells, result.cells)
        assertEquals(live.cols, result.cols)
        assertEquals(live.rows, result.rows)
        assertEquals(live.title, result.title)
        assertEquals(live.applicationCursorMode, result.applicationCursorMode)
        assertEquals(live.bracketedPasteMode, result.bracketedPasteMode)
        assertEquals(live.cursorVisible, result.cursorVisible)
        assertEquals(live.bellGeneration, result.bellGeneration)
    }

    @Test
    fun `preserves bell generation when showing scrollback`() {
        // bellGenerationはBEL通知用のRust側SSOTカウンタ(タスク#24)。スクロールバック表示に
        // 切り替えても直近のライブ値を落としてはいけない(iOS版`synthesizeDisplayUpdate`と対称)。
        val live = update(rows = listOf("live line"), cols = 20, bellGeneration = 7uL)
        val scrollbackCells = List(20) { cell('x') }

        val result = synthesizeDisplayUpdate(live, scrollOffset = 5, scrollbackCells = scrollbackCells)

        assertEquals(7uL, result.bellGeneration)
    }

    @Test
    fun `preserves cursor visibility false when showing scrollback`() {
        val live = update(rows = listOf("live line"), cols = 20, cursorVisible = false)
        val scrollbackCells = List(20) { cell('x') }

        val result = synthesizeDisplayUpdate(live, scrollOffset = 5, scrollbackCells = scrollbackCells)

        assertEquals(false, result.cursorVisible)
    }

    @Test
    fun `hides cursor off screen when showing scrollback`() {
        val live = update(rows = listOf("live line"), cols = 20, cursorCol = 3u)
        val scrollbackCells = List(20) { cell('x') }

        val result = synthesizeDisplayUpdate(live, scrollOffset = 5, scrollbackCells = scrollbackCells)

        assertEquals(live.rows, result.cursorRow)
        assertEquals(0u, result.cursorCol)
    }

    @Test
    fun `falls back to live when scrollback cell count mismatches`() {
        val live = update(rows = listOf("live line"), cols = 20)
        val wrongSizeCells = listOf(cell('x'))

        val result = synthesizeDisplayUpdate(live, scrollOffset = 5, scrollbackCells = wrongSizeCells)

        assertEquals(live, result)
    }

    @Test
    fun `falls back to live when scrollback cells is null`() {
        val live = update(rows = listOf("live line"), cols = 20)

        val result = synthesizeDisplayUpdate(live, scrollOffset = 5, scrollbackCells = null)

        assertEquals(live, result)
    }

    @Test
    fun `falls back to live when cell count was sized for a viewport cols different from live cols`() {
        // Codexレビュー(タスク#46): リサイズ中の過渡状態では、呼び出し側がCompose層で独自計算した
        // ビューポート由来のcols(例:新しい幅)と、live.cols(直近のScreenUpdateブロードキャストが
        // まだ反映していない旧い幅)が食い違いうる。scrollbackCellsがlive.colsではなく別のcols幅で
        // 詰められていた場合、必ずライブへフォールバックし、cols/rowsとcellsの件数が食い違った
        // ScreenUpdateを返してはいけない(呼び出し側`SshTerminalCanvas`のインデックス計算が
        // ずれてIndexOutOfBoundsException等を起こしうるため)。
        val live = update(rows = listOf("live line"), cols = 20)
        val cellsSizedForDifferentCols = List(30) { cell('x') } // live.cols(20) * live.rows(1) と不一致

        val result = synthesizeDisplayUpdate(live, scrollOffset = 5, scrollbackCells = cellsSizedForDifferentCols)

        assertEquals(live, result)
    }
}
