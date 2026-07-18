package tools.isekai.terminal

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import uniffi.isekai_terminal_core.ScrollbackSearchMatch

/**
 * タスク#66: [searchHighlightMatch]のピュアな判断ロジックを検証する。
 *
 * codexレビューで指摘された回帰(`row == 0u`の除外漏れにより、ライブ画面表示中
 * [scrollOffset == 0]にscrollback最新行[row == 0u]のマッチが誤ってハイライトされてしまう
 * バグ)を再発防止するため、iOS版`TerminalView.swift`の
 * `currentSearchMatch.flatMap { $0.row == 0 ? nil : $0 }`と対称のケースを含む。
 */
class TerminalScreenSearchHighlightTest {

    @Test
    fun `returns null when there is no current match`() {
        assertNull(searchHighlightMatch(null, scrollOffset = 3))
    }

    @Test
    fun `returns the match when scrollOffset equals its row`() {
        val match = ScrollbackSearchMatch(row = 3u, col = 1u, len = 2u)
        assertEquals(match, searchHighlightMatch(match, scrollOffset = 3))
    }

    @Test
    fun `returns null when scrollOffset does not equal its row`() {
        val match = ScrollbackSearchMatch(row = 3u, col = 1u, len = 2u)
        assertNull(searchHighlightMatch(match, scrollOffset = 1))
    }

    @Test
    fun `returns null for row zero even when scrollOffset is zero`() {
        // codexレビュー指摘の回帰ケース: row == 0u(scrollback最新行)は、
        // scrollOffset == 0 が「ライブ画面表示」を兼ねる既存規約と衝突するため、
        // scrollOffsetが偶然0のとき(=ライブ画面表示中)でも誤ってハイライトしてはいけない。
        val match = ScrollbackSearchMatch(row = 0u, col = 1u, len = 2u)
        assertNull(searchHighlightMatch(match, scrollOffset = 0))
    }

    @Test
    fun `returns null for row zero regardless of scrollOffset`() {
        // row == 0u はこの仕組み経由では原理的に到達不能な値なので、scrollOffsetが
        // どの値であっても常にnullを返す(誤ってジャンプできたように見せない)。
        val match = ScrollbackSearchMatch(row = 0u, col = 1u, len = 2u)
        assertNull(searchHighlightMatch(match, scrollOffset = 5))
    }
}
