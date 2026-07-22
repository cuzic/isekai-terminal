package tools.isekai.terminal.ui

import org.junit.Assert.assertEquals
import org.junit.Test
import uniffi.isekai_terminal_core.CellData
import uniffi.isekai_terminal_core.CursorShape
import uniffi.isekai_terminal_core.MouseReportingMode
import uniffi.isekai_terminal_core.ScreenUpdate

class TerminalSelectionTest {

    // 実際の rust-core は行末の空きセルを ch=" " で埋める（terminal.rs 参照）。
    // text の各文字を1セルに割り当て、残りは空白セルで cols まで埋める。
    private fun rowCells(text: String, cols: Int): List<CellData> {
        val chars = text.toList()
        return (0 until cols).map { i ->
            val ch = chars.getOrNull(i)?.toString() ?: " "
            CellData(
                ch = ch, fg = 0xFFFFFFFFu, bg = 0xFF000000u, bold = false,
                dim = false, italic = false, underline = false,
                strikethrough = false, blink = false, invisible = false,
                linkId = null,
            )
        }
    }

    private fun screenUpdate(rows: List<String>, cols: Int): ScreenUpdate {
        val cells = rows.flatMap { rowCells(it, cols) }
        return ScreenUpdate(
            updateSeq = 0u,
            cols = cols.toUInt(),
            rows = rows.size.toUInt(),
            cells = cells,
            cursorRow = 0u,
            cursorCol = 0u,
            title = null,
            applicationCursorMode = false,
            applicationKeypadMode = false,
            bracketedPasteMode = false,
            mouseReportingMode = MouseReportingMode.OFF,
            sgrMouseMode = false,
            alternateScroll = false,
            urxvtMouseMode = false,
            cursorVisible = true,
            bellGeneration = 0uL,
            cursorShape = CursorShape.BLOCK,
            cursorBlink = true,
            linkTable = emptyList(),
            images = emptyList(),
            kittyKeyboardFlags = 0u,
            dirtyRows = null,
        )
    }

    // ── reconstructSelectionText ────────────────────────────────────

    @Test
    fun `single row selection reconstructs that line`() {
        val update = screenUpdate(listOf("hello", "world"), cols = 10)
        val sel = SelectionRange(CellPos(0, 0), CellPos(0, 0))
        assertEquals("hello", reconstructSelectionText(update, sel))
    }

    @Test
    fun `multi line selection joins rows with newline`() {
        val update = screenUpdate(listOf("foo", "bar", "baz"), cols = 10)
        val sel = SelectionRange(CellPos(0, 0), CellPos(2, 0))
        assertEquals("foo\nbar\nbaz", reconstructSelectionText(update, sel))
    }

    @Test
    fun `selection is linewise so column of anchor and head is ignored`() {
        // MVP は行単位選択: anchor/head の col は行範囲の決定に使われない
        val update = screenUpdate(listOf("foo", "bar", "baz"), cols = 10)
        val sel = SelectionRange(CellPos(0, 9), CellPos(2, 0))
        assertEquals("foo\nbar\nbaz", reconstructSelectionText(update, sel))
    }

    @Test
    fun `reversed anchor and head still selects rows in order`() {
        val update = screenUpdate(listOf("foo", "bar", "baz"), cols = 10)
        // head が anchor より上の行（下から上へドラッグしたケース）
        val sel = SelectionRange(CellPos(2, 0), CellPos(0, 0))
        assertEquals("foo\nbar\nbaz", reconstructSelectionText(update, sel))
    }

    @Test
    fun `trailing whitespace on each line is trimmed`() {
        val update = screenUpdate(listOf("hi", "there"), cols = 10)
        val sel = SelectionRange(CellPos(0, 0), CellPos(1, 0))
        val result = reconstructSelectionText(update, sel)
        assertEquals("hi\nthere", result)
        assertEquals(false, result.contains(" \n"))
        assertEquals(false, result.endsWith(" "))
    }

    @Test
    fun `fullwidth characters are preserved in reconstructed text`() {
        val update = screenUpdate(listOf("あいう"), cols = 10)
        val sel = SelectionRange(CellPos(0, 0), CellPos(0, 0))
        val result = reconstructSelectionText(update, sel)
        assertEquals(true, result.contains("あ"))
        assertEquals(true, result.contains("い"))
        assertEquals(true, result.contains("う"))
        // 行末の空白セルはtrimされる
        assertEquals(false, result.endsWith(" "))
    }

    @Test
    fun `empty cells list returns empty string instead of crashing`() {
        val update = ScreenUpdate(
            updateSeq = 0u,
            cols = 80u, rows = 24u, cells = emptyList(),
            cursorRow = 0u, cursorCol = 0u, title = null,
            applicationCursorMode = false, applicationKeypadMode = false, bracketedPasteMode = false,
            mouseReportingMode = MouseReportingMode.OFF, sgrMouseMode = false,
            alternateScroll = false,
            urxvtMouseMode = false,
            cursorVisible = true, bellGeneration = 0uL,
            cursorShape = CursorShape.BLOCK, cursorBlink = true, linkTable = emptyList(),
            images = emptyList(), kittyKeyboardFlags = 0u,
            dirtyRows = null,
        )
        val sel = SelectionRange(CellPos(0, 0), CellPos(1, 0))
        assertEquals("", reconstructSelectionText(update, sel))
    }

    @Test
    fun `zero cols or rows returns empty string`() {
        val update = ScreenUpdate(
            updateSeq = 0u,
            cols = 0u, rows = 0u, cells = emptyList(),
            cursorRow = 0u, cursorCol = 0u, title = null,
            applicationCursorMode = false, applicationKeypadMode = false, bracketedPasteMode = false,
            mouseReportingMode = MouseReportingMode.OFF, sgrMouseMode = false,
            alternateScroll = false,
            urxvtMouseMode = false,
            cursorVisible = true, bellGeneration = 0uL,
            cursorShape = CursorShape.BLOCK, cursorBlink = true, linkTable = emptyList(),
            images = emptyList(), kittyKeyboardFlags = 0u,
            dirtyRows = null,
        )
        val sel = SelectionRange(CellPos(0, 0), CellPos(0, 0))
        assertEquals("", reconstructSelectionText(update, sel))
    }

    @Test
    fun `selection rows are clamped to viewport bounds`() {
        val update = screenUpdate(listOf("only-row"), cols = 10)
        // head が画面外の行を指していても rows-1 にクランプされる
        val sel = SelectionRange(CellPos(0, 0), CellPos(99, 0))
        assertEquals("only-row", reconstructSelectionText(update, sel))
    }

    @Test
    fun `all-blank line reconstructs to empty string for that line`() {
        val update = screenUpdate(listOf("foo", "", "bar"), cols = 10)
        val sel = SelectionRange(CellPos(0, 0), CellPos(2, 0))
        assertEquals("foo\n\nbar", reconstructSelectionText(update, sel))
    }

    // ── offsetToCellPos ──────────────────────────────────────────────

    @Test
    fun `offsetToCellPos maps pixel coordinates to cell grid`() {
        assertEquals(CellPos(0, 0), offsetToCellPos(0f, 0f, cellWidth = 10f, cellHeight = 20f, cols = 80, rows = 24))
        assertEquals(CellPos(1, 2), offsetToCellPos(25f, 21f, cellWidth = 10f, cellHeight = 20f, cols = 80, rows = 24))
    }

    @Test
    fun `offsetToCellPos clamps to viewport bounds`() {
        assertEquals(CellPos(23, 79), offsetToCellPos(99999f, 99999f, cellWidth = 10f, cellHeight = 20f, cols = 80, rows = 24))
        assertEquals(CellPos(0, 0), offsetToCellPos(-50f, -50f, cellWidth = 10f, cellHeight = 20f, cols = 80, rows = 24))
    }

    @Test
    fun `offsetToCellPos returns origin when grid is degenerate`() {
        assertEquals(CellPos(0, 0), offsetToCellPos(10f, 10f, cellWidth = 10f, cellHeight = 20f, cols = 0, rows = 0))
        assertEquals(CellPos(0, 0), offsetToCellPos(10f, 10f, cellWidth = 0f, cellHeight = 0f, cols = 80, rows = 24))
    }

    // ── SelectionRange.startRow / endRow ────────────────────────────

    @Test
    fun `SelectionRange normalizes start and end row regardless of drag direction`() {
        val downward = SelectionRange(CellPos(1, 0), CellPos(5, 0))
        assertEquals(1, downward.startRow)
        assertEquals(5, downward.endRow)

        val upward = SelectionRange(CellPos(5, 0), CellPos(1, 0))
        assertEquals(1, upward.startRow)
        assertEquals(5, upward.endRow)
    }
}
