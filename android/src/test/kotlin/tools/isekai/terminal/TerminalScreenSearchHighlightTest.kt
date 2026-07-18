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
 * `searchHighlight`計算と対称のケースを含む。
 *
 * タスク#79: 当初の実装は`row == 0u`を常に除外していたため、検索結果に`row == 0u`の
 * マッチが出てもジャンプ・ハイライトのどちらも到達できない(「検索結果に出るのに
 * 到達できない」)UX上のバグがあった。[showingScrollback](`TerminalScreen.kt`の
 * `jumpToCurrentSearchMatch`がscrollback最新行へジャンプする際に真にする、
 * `scrollOffset == 0`とは独立したフラグ)を導入し、実際にscrollback最新行を表示中
 * ([showingScrollback] == true)であればrow=0のマッチもハイライトできるようにした。
 */
class TerminalScreenSearchHighlightTest {

    @Test
    fun `returns null when there is no current match`() {
        assertNull(searchHighlightMatch(null, scrollOffset = 3, showingScrollback = false))
    }

    @Test
    fun `returns the match when scrollOffset equals its row`() {
        val match = ScrollbackSearchMatch(row = 3u, col = 1u, len = 2u)
        assertEquals(match, searchHighlightMatch(match, scrollOffset = 3, showingScrollback = false))
    }

    @Test
    fun `returns null when scrollOffset does not equal its row`() {
        val match = ScrollbackSearchMatch(row = 3u, col = 1u, len = 2u)
        assertNull(searchHighlightMatch(match, scrollOffset = 1, showingScrollback = false))
    }

    @Test
    fun `returns null for row zero when scrollOffset is zero and not showing scrollback`() {
        // ライブ画面表示中(scrollOffset == 0 かつ showingScrollback == false)は、
        // row == 0u(scrollback最新行)のマッチを誤ってハイライトしてはいけない
        // (codexレビュー指摘の回帰ケース)。
        val match = ScrollbackSearchMatch(row = 0u, col = 1u, len = 2u)
        assertNull(searchHighlightMatch(match, scrollOffset = 0, showingScrollback = false))
    }

    @Test
    fun `returns the match for row zero when scrollOffset is zero and showing scrollback`() {
        // タスク#79の回帰確認: `jumpToCurrentSearchMatch`がscrollback最新行(row=0)へ
        // ジャンプした後(showingScrollback = true)は、scrollOffset == 0のままでも
        // そのマッチを正しくハイライトできなければならない(以前は常にnullを返していた)。
        val match = ScrollbackSearchMatch(row = 0u, col = 1u, len = 2u)
        assertEquals(match, searchHighlightMatch(match, scrollOffset = 0, showingScrollback = true))
    }

    @Test
    fun `returns null for row zero when scrollOffset is nonzero even if showing scrollback`() {
        // showingScrollbackが真でも、実際に表示されているoffsetとrowが一致しなければ
        // ハイライトしない(ジャンプ直後でscrollOffsetがまだ追従していない場合の
        // 誤描画を避ける)。
        val match = ScrollbackSearchMatch(row = 0u, col = 1u, len = 2u)
        assertNull(searchHighlightMatch(match, scrollOffset = 5, showingScrollback = true))
    }
}
