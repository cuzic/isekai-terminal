package tools.isekai.terminal.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import uniffi.isekai_terminal_core.CellData
import uniffi.isekai_terminal_core.CursorShape
import uniffi.isekai_terminal_core.MouseReportingMode
import uniffi.isekai_terminal_core.ScreenUpdate

class TerminalHyperlinkTest {

    private fun cell(ch: String, linkId: UInt?) = CellData(
        ch = ch, fg = 0xFFFFFFFFu, bg = 0xFF000000u, bold = false,
        dim = false, italic = false, underline = false,
        strikethrough = false, blink = false, invisible = false,
        linkId = linkId,
    )

    /** 1行 * [cols]列の[ScreenUpdate]を組み立てる。cellsは呼び出し側が個別に用意する。 */
    private fun screenUpdate(cells: List<CellData>, cols: Int, rows: Int, linkTable: List<String>) = ScreenUpdate(
        cols = cols.toUInt(),
        rows = rows.toUInt(),
        cells = cells,
        cursorRow = 0u,
        cursorCol = 0u,
        title = null,
        applicationCursorMode = false,
        applicationKeypadMode = false,
        bracketedPasteMode = false,
        mouseReportingMode = MouseReportingMode.OFF,
        sgrMouseMode = false,
        cursorVisible = true,
        bellGeneration = 0uL,
        cursorShape = CursorShape.BLOCK,
        cursorBlink = true,
        linkTable = linkTable,
        images = emptyList(),
        kittyKeyboardFlags = 0u,
    )

    // ── linkUrlAtCell ────────────────────────────────────────────────

    @Test
    fun `cell with linkId resolves to the URL in linkTable`() {
        val cells = listOf(cell("h", 0u), cell("i", 0u), cell(" ", null))
        val update = screenUpdate(cells, cols = 3, rows = 1, linkTable = listOf("https://example.com"))
        assertEquals("https://example.com", linkUrlAtCell(update, row = 0, col = 0))
        assertEquals("https://example.com", linkUrlAtCell(update, row = 0, col = 1))
    }

    @Test
    fun `cell without linkId returns null`() {
        val cells = listOf(cell("h", 0u), cell(" ", null))
        val update = screenUpdate(cells, cols = 2, rows = 1, linkTable = listOf("https://example.com"))
        assertNull(linkUrlAtCell(update, row = 0, col = 1))
    }

    @Test
    fun `out of bounds row or col returns null instead of crashing`() {
        val cells = listOf(cell("h", 0u))
        val update = screenUpdate(cells, cols = 1, rows = 1, linkTable = listOf("https://example.com"))
        assertNull(linkUrlAtCell(update, row = -1, col = 0))
        assertNull(linkUrlAtCell(update, row = 0, col = 5))
        assertNull(linkUrlAtCell(update, row = 5, col = 0))
    }

    @Test
    fun `zero cols or rows returns null`() {
        val update = screenUpdate(emptyList(), cols = 0, rows = 0, linkTable = emptyList())
        assertNull(linkUrlAtCell(update, row = 0, col = 0))
    }

    @Test
    fun `linkId pointing past linkTable bounds returns null defensively`() {
        // 本来Rust側は常に有効なindexしかセルへ書かないはずだが、呼び出し側の
        // 防御としてクラッシュせずnullを返すことを確認する。
        val cells = listOf(cell("h", 99u))
        val update = screenUpdate(cells, cols = 1, rows = 1, linkTable = listOf("https://example.com"))
        assertNull(linkUrlAtCell(update, row = 0, col = 0))
    }

    // ── isOpenableHyperlinkScheme ────────────────────────────────────

    @Test
    fun `http and https schemes are openable`() {
        assertEquals(true, isOpenableHyperlinkScheme("http://example.com"))
        assertEquals(true, isOpenableHyperlinkScheme("https://example.com/path?x=1"))
        // スキームは大文字小文字を区別しない(RFC 3986)
        assertEquals(true, isOpenableHyperlinkScheme("HTTPS://EXAMPLE.COM"))
    }

    @Test
    fun `dangerous schemes are rejected`() {
        assertEquals(false, isOpenableHyperlinkScheme("intent://example.com#Intent;scheme=https;end"))
        assertEquals(false, isOpenableHyperlinkScheme("file:///etc/passwd"))
        assertEquals(false, isOpenableHyperlinkScheme("javascript:alert(1)"))
        assertEquals(false, isOpenableHyperlinkScheme("content://com.example/data"))
    }

    @Test
    fun `strings without a valid scheme are rejected`() {
        assertEquals(false, isOpenableHyperlinkScheme(""))
        assertEquals(false, isOpenableHyperlinkScheme("example.com"))
        assertEquals(false, isOpenableHyperlinkScheme("not a url at all"))
    }
}
